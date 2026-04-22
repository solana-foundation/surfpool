use std::{
    cmp::max,
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    str::FromStr,
    time::SystemTime,
};

use agave_feature_set::FeatureSet;
use base64::{Engine, prelude::BASE64_STANDARD};
use chrono::Utc;
use convert_case::Casing;
use crossbeam_channel::{Receiver, Sender, unbounded};
use litesvm::types::{
    FailedTransactionMetadata, SimulatedTransactionInfo, TransactionMetadata, TransactionResult,
};
use solana_account::{Account, AccountSharedData, ReadableAccount};
use solana_account_decoder::{
    UiAccount, UiAccountData, UiAccountEncoding, UiDataSliceConfig, encode_ui_account,
    parse_account_data::{AccountAdditionalDataV3, ParsedAccount, SplTokenAdditionalDataV2},
};
use solana_client::{
    rpc_client::SerializableTransaction,
    rpc_config::{RpcAccountInfoConfig, RpcBlockConfig, RpcTransactionLogsFilter},
    rpc_filter::RpcFilterType,
    rpc_response::{RpcKeyedAccount, RpcLogsResponse, RpcPerfSample},
};
use solana_clock::{Clock, Slot};
use solana_commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_epoch_info::EpochInfo;
use solana_epoch_schedule::EpochSchedule;
use solana_genesis_config::GenesisConfig;
use solana_hash::Hash;
use solana_inflation::Inflation;
use solana_loader_v3_interface::state::UpgradeableLoaderState;
use solana_message::{
    Message, VersionedMessage, inline_nonce::is_advance_nonce_instruction_data, v0::LoadedAddresses,
};
use solana_program_option::COption;
use solana_pubkey::Pubkey;
use solana_rpc_client_api::response::SlotInfo;
use solana_sdk_ids::{bpf_loader, system_program};
use solana_signature::Signature;
use solana_system_interface::instruction as system_instruction;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;
use solana_transaction_status::{TransactionDetails, TransactionStatusMeta, UiConfirmedBlock};
use spl_token_2022_interface::extension::{
    BaseStateWithExtensions, StateWithExtensions, interest_bearing_mint::InterestBearingConfig,
    scaled_ui_amount::ScaledUiAmountConfig,
};
use surfpool_types::{
    AccountChange, AccountProfileState, AccountSnapshot, DEFAULT_PROFILING_MAP_CAPACITY,
    DEFAULT_SLOT_TIME_MS, ExportSnapshotConfig, ExportSnapshotScope, FifoMap, Idl,
    OverrideInstance, ProfileResult, RpcProfileDepth, RpcProfileResultConfig,
    RunbookExecutionStatusReport, SimnetEvent, SvmFeatureConfig, TransactionConfirmationStatus,
    TransactionStatusEvent, UiAccountChange, UiAccountProfileState, UiProfileResult, VersionedIdl,
    types::{
        ComputeUnitsEstimationResult, KeyedProfileResult, UiKeyedProfileResult, UuidOrSignature,
    },
};
use txtx_addon_kit::{
    indexmap::IndexMap,
    types::types::{AddonJsonConverter, Value},
};
use txtx_addon_network_svm::codec::idl::borsh_encode_value_to_idl_type;
use txtx_addon_network_svm_types::idl::{
    parse_bytes_to_value_with_expected_idl_type_def_ty,
    parse_bytes_to_value_with_expected_idl_type_def_ty_with_leftover_bytes,
};
use uuid::Uuid;

use super::{
    AccountSubscriptionData, BlockHeader, BlockIdentifier, FINALIZATION_SLOT_THRESHOLD,
    GetAccountResult, GeyserBlockMetadata, GeyserEntryInfo, GeyserEvent, GeyserSlotStatus,
    ProgramSubscriptionData, SignatureSubscriptionData, SignatureSubscriptionType,
    remote::SurfnetRemoteClient,
};
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    rpc::utils::convert_transaction_metadata_from_canonical,
    scenarios::TemplateRegistry,
    storage::{OverlayStorage, Storage, new_kv_store, new_kv_store_with_default},
    surfnet::{
        LogsSubscriptionData, locker::is_supported_token_program, surfnet_lite_svm::SurfnetLiteSvm,
    },
    types::{
        GeyserAccountUpdate, MintAccount, OfflineAccountConfig, SerializableAccountAdditionalData,
        SurfnetTransactionStatus, SyntheticBlockhash, TokenAccount, TransactionWithStatusMeta,
    },
};

lazy_static::lazy_static! {
    /// Interval (in slots) at which to perform garbage collection on the lite SVM cache.
    /// About 1 hour at standard 400ms slot time.
    /// Configurable via SURFPOOL_GARBAGE_COLLECTION_INTERVAL_SLOTS env var.
    pub static ref GARBAGE_COLLECTION_INTERVAL_SLOTS: u64 = {
        std::env::var("SURFPOOL_GARBAGE_COLLECTION_INTERVAL_SLOTS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9_000)
    };

    /// Interval (in slots) at which to checkpoint the latest slot to storage.
    /// About 1 minute at standard 400ms slot time (60000ms / 400ms = 150 slots).
    /// Configurable via SURFPOOL_CHECKPOINT_INTERVAL_SLOTS env var.
    pub static ref CHECKPOINT_INTERVAL_SLOTS: u64 = {
        std::env::var("SURFPOOL_CHECKPOINT_INTERVAL_SLOTS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(150)
    };
}

/// Helper function to apply an override to a decoded account value using dot notation
pub fn apply_override_to_decoded_account(
    decoded_value: &mut Value,
    path: &str,
    value: &serde_json::Value,
) -> SurfpoolResult<()> {
    let parts: Vec<&str> = path.split('.').collect();

    if parts.is_empty() {
        return Err(SurfpoolError::internal("Empty path provided for override"));
    }

    // Navigate to the parent of the target field
    let mut current = decoded_value;
    for part in &parts[..parts.len() - 1] {
        match current {
            Value::Object(map) => {
                current = map.get_mut(&part.to_string()).ok_or_else(|| {
                    SurfpoolError::internal(format!(
                        "Path segment '{}' not found in decoded account",
                        part
                    ))
                })?;
            }
            _ => {
                return Err(SurfpoolError::internal(format!(
                    "Cannot navigate through field '{}' - not an object",
                    part
                )));
            }
        }
    }

    // Set the final field
    let final_key = parts[parts.len() - 1];
    match current {
        Value::Object(map) => {
            // Convert serde_json::Value to txtx Value
            let txtx_value = json_to_txtx_value(value)?;
            map.insert(final_key.to_string(), txtx_value);
            Ok(())
        }
        _ => Err(SurfpoolError::internal(format!(
            "Cannot set field '{}' - parent is not an object",
            final_key
        ))),
    }
}

/// Helper function to convert serde_json::Value to txtx Value
fn json_to_txtx_value(json: &serde_json::Value) -> SurfpoolResult<Value> {
    match json {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(b) => Ok(Value::Bool(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i as i128))
            } else if let Some(u) = n.as_u64() {
                Ok(Value::Integer(u as i128))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Float(f))
            } else {
                Err(SurfpoolError::internal(format!(
                    "Unable to convert number: {}",
                    n
                )))
            }
        }
        serde_json::Value::String(s) => Ok(Value::String(s.clone())),
        serde_json::Value::Array(arr) => {
            let txtx_arr: Result<Vec<Value>, _> = arr.iter().map(json_to_txtx_value).collect();
            Ok(Value::Array(Box::new(txtx_arr?)))
        }
        serde_json::Value::Object(obj) => {
            let mut txtx_obj = IndexMap::new();
            for (k, v) in obj.iter() {
                txtx_obj.insert(k.clone(), json_to_txtx_value(v)?);
            }
            Ok(Value::Object(txtx_obj))
        }
    }
}

pub type AccountOwner = Pubkey;

#[allow(deprecated)]
use solana_sysvar::recent_blockhashes::MAX_ENTRIES;

#[allow(deprecated)]
pub const MAX_RECENT_BLOCKHASHES_STANDARD: usize = MAX_ENTRIES;

pub fn get_txtx_value_json_converters() -> Vec<AddonJsonConverter<'static>> {
    vec![
        Box::new(move |value: &txtx_addon_kit::types::types::Value| {
            txtx_addon_network_svm_types::SvmValue::to_json(value)
        }) as AddonJsonConverter<'static>,
    ]
}

const DEFAULT_LOG_BYTES_LIMIT: Option<usize> = Some(10_000);

#[derive(Debug, Clone)]
pub struct SurfnetSvmConfig {
    pub surfnet_id: String,
    pub feature_config: SvmFeatureConfig,
    pub slot_time: u64,
    pub instruction_profiling_enabled: bool,
    pub max_profiles: usize,
    pub log_bytes_limit: Option<usize>,
}

impl Default for SurfnetSvmConfig {
    fn default() -> Self {
        Self {
            surfnet_id: "default".to_string(),
            feature_config: SvmFeatureConfig::default(),
            slot_time: DEFAULT_SLOT_TIME_MS,
            instruction_profiling_enabled: true,
            max_profiles: DEFAULT_PROFILING_MAP_CAPACITY,
            log_bytes_limit: DEFAULT_LOG_BYTES_LIMIT,
        }
    }
}

/// `SurfnetSvm` provides a lightweight Solana Virtual Machine (SVM) for testing and simulation.
///
/// It supports a local in-memory blockchain state,
/// remote RPC connections, transaction processing, and account management.
///
/// It also exposes channels to listen for simulation events (`SimnetEvent`) and Geyser plugin events (`GeyserEvent`).
#[derive(Clone)]
pub struct SurfnetSvm {
    pub inner: SurfnetLiteSvm,
    pub remote_rpc_url: Option<String>,
    pub chain_tip: BlockIdentifier,
    pub blocks: Box<dyn Storage<u64, BlockHeader>>,
    pub transactions: Box<dyn Storage<String, SurfnetTransactionStatus>>,
    pub transactions_queued_for_confirmation: VecDeque<(
        VersionedTransaction,
        Sender<TransactionStatusEvent>,
        Option<TransactionError>,
    )>,
    pub transactions_queued_for_finalization: VecDeque<(
        Slot,
        VersionedTransaction,
        Sender<TransactionStatusEvent>,
        Option<TransactionError>,
    )>,
    pub perf_samples: VecDeque<RpcPerfSample>,
    pub transactions_processed: u64,
    pub latest_epoch_info: EpochInfo,
    pub simnet_events_tx: Sender<SimnetEvent>,
    pub geyser_events_tx: Sender<GeyserEvent>,
    pub signature_subscriptions: HashMap<Signature, Vec<SignatureSubscriptionData>>,
    pub account_subscriptions: AccountSubscriptionData,
    pub program_subscriptions: ProgramSubscriptionData,
    pub slot_subscriptions: Vec<Sender<SlotInfo>>,
    pub profile_tag_map: Box<dyn Storage<String, Vec<UuidOrSignature>>>,
    pub simulated_transaction_profiles: Box<dyn Storage<String, KeyedProfileResult>>,
    pub executed_transaction_profiles: Box<dyn Storage<String, KeyedProfileResult>>,
    pub logs_subscriptions: Vec<LogsSubscriptionData>,
    pub snapshot_subscriptions: Vec<super::SnapshotSubscriptionData>,
    pub updated_at: u64,
    pub slot_time: u64,
    pub start_time: SystemTime,
    pub accounts_by_owner: Box<dyn Storage<String, Vec<String>>>,
    pub account_associated_data: Box<dyn Storage<String, SerializableAccountAdditionalData>>,
    pub token_accounts: Box<dyn Storage<String, TokenAccount>>,
    pub token_mints: Box<dyn Storage<String, MintAccount>>,
    pub token_accounts_by_owner: Box<dyn Storage<String, Vec<String>>>,
    pub token_accounts_by_delegate: Box<dyn Storage<String, Vec<String>>>,
    pub token_accounts_by_mint: Box<dyn Storage<String, Vec<String>>>,
    pub total_supply: u64,
    pub circulating_supply: u64,
    pub non_circulating_supply: u64,
    pub non_circulating_accounts: Vec<String>,
    pub genesis_config: GenesisConfig,
    pub inflation: Inflation,
    /// A global monotonically increasing atomic number, which can be used to tell the order of the account update.
    /// For example, when an account is updated in the same slot multiple times,
    /// the update with higher write_version should supersede the one with lower write_version.
    pub write_version: u64,
    pub registered_idls: Box<dyn Storage<String, Vec<VersionedIdl>>>,
    pub feature_set: FeatureSet,
    pub instruction_profiling_enabled: bool,
    pub max_profiles: usize,
    pub runbook_executions: Vec<RunbookExecutionStatusReport>,
    pub account_update_slots: HashMap<Pubkey, Slot>,
    pub streamed_accounts: Box<dyn Storage<String, bool>>,
    pub recent_blockhashes: VecDeque<(SyntheticBlockhash, i64)>,
    pub scheduled_overrides: Box<dyn Storage<u64, Vec<OverrideInstance>>>,
    /// Tracks accounts that should not be downloaded from the remote RPC.
    /// This includes accounts explicitly closed locally and accounts marked offline via cheatcodes.
    /// The key is the account pubkey as a string. If `include_owned_accounts` is true,
    /// accounts owned by this pubkey are also marked offline and excluded from remote download.
    pub offline_accounts: Box<dyn Storage<String, OfflineAccountConfig>>,
    /// The slot at which this surfnet instance started (may be non-zero when connected to remote).
    /// Used as the lower bound for block reconstruction.
    pub genesis_slot: Slot,
    /// The `updated_at` timestamp when this surfnet started at `genesis_slot`.
    /// Used to reconstruct block_time: genesis_updated_at + ((slot - genesis_slot) * slot_time)
    pub genesis_updated_at: u64,
    /// Storage for persisting the latest slot checkpoint.
    /// Used for recovery on restart with sparse block storage.
    pub slot_checkpoint: Box<dyn Storage<String, u64>>,
    /// Tracks the slot at which we last persisted the checkpoint.
    pub last_checkpoint_slot: u64,
}

/// Add `pubkey_str` to the pubkey-list at `key`, creating the entry when absent
/// and deduplicating on insert. The shared-pubkey indexes (`accounts_by_owner`,
/// `token_accounts_by_owner`, `token_accounts_by_mint`,
/// `token_accounts_by_delegate`) map one pubkey-string to the list of account
/// pubkey-strings that share the indexed trait; the `contains` guard prevents
/// double-registration when `update_account_registries` is called on an
/// unchanged account.
fn add_pubkey_to_index(
    index: &mut Box<dyn Storage<String, Vec<String>>>,
    key: String,
    pubkey_str: &str,
) -> SurfpoolResult<()> {
    let mut accounts = index.get(&key).ok().flatten().unwrap_or_default();
    if !accounts.iter().any(|pk| pk == pubkey_str) {
        accounts.push(pubkey_str.to_string());
        index.store(key, accounts)?;
    }
    Ok(())
}

/// Remove `pubkey_str` from the pubkey-list at `key`. When the list becomes
/// empty the entry is taken rather than stored, so downstream `keys()`
/// iterations don't surface empty buckets.
fn remove_pubkey_from_index(
    index: &mut Box<dyn Storage<String, Vec<String>>>,
    key: &str,
    pubkey_str: &str,
) -> SurfpoolResult<()> {
    let key_owned = key.to_string();
    if let Some(mut accounts) = index.get(&key_owned).ok().flatten() {
        accounts.retain(|pk| pk != pubkey_str);
        if accounts.is_empty() {
            index.take(&key_owned)?;
        } else {
            index.store(key_owned, accounts)?;
        }
    }
    Ok(())
}

impl SurfnetSvm {
    pub fn default() -> (Self, Receiver<SimnetEvent>, Receiver<GeyserEvent>) {
        Self::new(SurfnetSvmConfig::default()).unwrap()
    }

    pub fn new(
        config: SurfnetSvmConfig,
    ) -> SurfpoolResult<(Self, Receiver<SimnetEvent>, Receiver<GeyserEvent>)> {
        Self::build(None, config)
    }

    pub fn new_with_db(
        database_url: Option<&str>,
        config: SurfnetSvmConfig,
    ) -> SurfpoolResult<(Self, Receiver<SimnetEvent>, Receiver<GeyserEvent>)> {
        Self::build(database_url, config)
    }

    /// Explicitly shutdown the SVM, performing cleanup like WAL checkpoint for SQLite.
    /// This should be called before the application exits to ensure data is persisted.
    pub fn shutdown(&self) {
        self.inner.shutdown();
        self.blocks.shutdown();
        self.transactions.shutdown();
        self.token_accounts.shutdown();
        self.token_mints.shutdown();
        self.accounts_by_owner.shutdown();
        self.token_accounts_by_owner.shutdown();
        self.token_accounts_by_delegate.shutdown();
        self.token_accounts_by_mint.shutdown();
        self.streamed_accounts.shutdown();
        self.scheduled_overrides.shutdown();
        self.registered_idls.shutdown();
        self.profile_tag_map.shutdown();
        self.simulated_transaction_profiles.shutdown();
        self.executed_transaction_profiles.shutdown();
        self.account_associated_data.shutdown();
    }

    /// Creates a clone of the SVM with overlay storage wrappers for all database-backed fields.
    /// This allows profiling transactions without affecting the underlying database.
    /// All storage writes are buffered in memory and discarded when the clone is dropped.
    pub fn clone_for_profiling(&self) -> Self {
        let (dummy_simnet_tx, _) = crossbeam_channel::bounded(1);
        let (dummy_geyser_tx, _) = crossbeam_channel::bounded(1);

        Self {
            inner: self.inner.clone_for_profiling(),
            remote_rpc_url: self.remote_rpc_url.clone(),
            chain_tip: self.chain_tip.clone(),

            // Wrap all storage fields with OverlayStorage
            blocks: OverlayStorage::wrap(self.blocks.clone_box()),
            transactions: OverlayStorage::wrap(self.transactions.clone_box()),
            profile_tag_map: OverlayStorage::wrap(self.profile_tag_map.clone_box()),
            simulated_transaction_profiles: OverlayStorage::wrap(
                self.simulated_transaction_profiles.clone_box(),
            ),
            executed_transaction_profiles: OverlayStorage::wrap(
                self.executed_transaction_profiles.clone_box(),
            ),
            accounts_by_owner: OverlayStorage::wrap(self.accounts_by_owner.clone_box()),
            account_associated_data: OverlayStorage::wrap(self.account_associated_data.clone_box()),
            token_accounts: OverlayStorage::wrap(self.token_accounts.clone_box()),
            token_mints: OverlayStorage::wrap(self.token_mints.clone_box()),
            token_accounts_by_owner: OverlayStorage::wrap(self.token_accounts_by_owner.clone_box()),
            token_accounts_by_delegate: OverlayStorage::wrap(
                self.token_accounts_by_delegate.clone_box(),
            ),
            token_accounts_by_mint: OverlayStorage::wrap(self.token_accounts_by_mint.clone_box()),
            registered_idls: OverlayStorage::wrap(self.registered_idls.clone_box()),
            streamed_accounts: OverlayStorage::wrap(self.streamed_accounts.clone_box()),
            scheduled_overrides: OverlayStorage::wrap(self.scheduled_overrides.clone_box()),

            // Clone non-storage fields normally
            transactions_queued_for_confirmation: self.transactions_queued_for_confirmation.clone(),
            transactions_queued_for_finalization: self.transactions_queued_for_finalization.clone(),
            perf_samples: self.perf_samples.clone(),
            transactions_processed: self.transactions_processed,
            latest_epoch_info: self.latest_epoch_info.clone(),

            // Use dummy channels to prevent event propagation during profiling
            simnet_events_tx: dummy_simnet_tx,
            geyser_events_tx: dummy_geyser_tx,

            signature_subscriptions: self.signature_subscriptions.clone(),
            account_subscriptions: self.account_subscriptions.clone(),
            program_subscriptions: self.program_subscriptions.clone(),
            // Don't clone subscriptions - profiling clone shouldn't send notifications
            slot_subscriptions: Vec::new(),
            logs_subscriptions: Vec::new(),
            snapshot_subscriptions: Vec::new(),

            updated_at: self.updated_at,
            slot_time: self.slot_time,
            start_time: self.start_time,

            total_supply: self.total_supply,
            circulating_supply: self.circulating_supply,
            non_circulating_supply: self.non_circulating_supply,
            non_circulating_accounts: self.non_circulating_accounts.clone(),
            genesis_config: self.genesis_config.clone(),
            inflation: self.inflation,
            write_version: self.write_version,
            feature_set: self.feature_set.clone(),
            instruction_profiling_enabled: self.instruction_profiling_enabled,
            max_profiles: self.max_profiles,
            runbook_executions: self.runbook_executions.clone(),
            account_update_slots: self.account_update_slots.clone(),
            recent_blockhashes: self.recent_blockhashes.clone(),
            offline_accounts: OverlayStorage::wrap(self.offline_accounts.clone_box()),
            genesis_slot: self.genesis_slot,
            genesis_updated_at: self.genesis_updated_at,
            slot_checkpoint: OverlayStorage::wrap(self.slot_checkpoint.clone_box()),
            last_checkpoint_slot: self.last_checkpoint_slot,
        }
    }

    pub(crate) fn default_epoch_schedule() -> EpochSchedule {
        EpochSchedule::without_warmup()
    }

    pub(crate) fn default_epoch_info(epoch_schedule: &EpochSchedule) -> EpochInfo {
        EpochInfo {
            epoch: 0,
            slot_index: 0,
            slots_in_epoch: epoch_schedule.slots_per_epoch,
            absolute_slot: FINALIZATION_SLOT_THRESHOLD,
            block_height: FINALIZATION_SLOT_THRESHOLD,
            transaction_count: None,
        }
    }

    fn register_builtin_template_idls(&mut self) {
        let registry = TemplateRegistry::new();
        for (_, template) in registry.templates.into_iter() {
            let _ = self.register_idl(template.idl, None);
        }
    }

    /// Creates a new instance of `SurfnetSvm`.
    ///
    /// Returns a tuple containing the SVM instance, a receiver for simulation events, and a receiver for Geyser plugin events.
    fn build(
        database_url: Option<&str>,
        config: SurfnetSvmConfig,
    ) -> SurfpoolResult<(Self, Receiver<SimnetEvent>, Receiver<GeyserEvent>)> {
        let (simnet_events_tx, simnet_events_rx) = crossbeam_channel::bounded(1024);
        let (geyser_events_tx, geyser_events_rx) = crossbeam_channel::bounded(1024);
        let surfnet_id = config.surfnet_id;

        let inner = SurfnetLiteSvm::new(database_url, &surfnet_id)?;

        let native_mint_account = inner
            .get_account(&spl_token_interface::native_mint::ID)?
            .unwrap();

        let native_mint_associated_data = {
            let mint = StateWithExtensions::<spl_token_2022_interface::state::Mint>::unpack(
                &native_mint_account.data,
            )
            .unwrap();
            let unix_timestamp = inner.get_sysvar::<Clock>().unix_timestamp;
            let interest_bearing_config = mint
                .get_extension::<InterestBearingConfig>()
                .map(|x| (*x, unix_timestamp))
                .ok();
            let scaled_ui_amount_config = mint
                .get_extension::<ScaledUiAmountConfig>()
                .map(|x| (*x, unix_timestamp))
                .ok();
            AccountAdditionalDataV3 {
                spl_token_additional_data: Some(SplTokenAdditionalDataV2 {
                    decimals: mint.base.decimals,
                    interest_bearing_config,
                    scaled_ui_amount_config,
                }),
            }
        };
        let parsed_mint_account = MintAccount::unpack(&native_mint_account.data).unwrap();

        // Load native mint into owned account and token mint indexes
        let mut accounts_by_owner_db: Box<dyn Storage<String, Vec<String>>> =
            new_kv_store(&database_url, "accounts_by_owner", &surfnet_id)?;
        accounts_by_owner_db.store(
            native_mint_account.owner.to_string(),
            vec![spl_token_interface::native_mint::ID.to_string()],
        )?;
        let blocks_db = new_kv_store(&database_url, "blocks", &surfnet_id)?;
        let transactions_db = new_kv_store(&database_url, "transactions", &surfnet_id)?;
        let token_accounts_db = new_kv_store(&database_url, "token_accounts", &surfnet_id)?;
        let mut token_mints_db: Box<dyn Storage<String, MintAccount>> =
            new_kv_store(&database_url, "token_mints", &surfnet_id)?;
        let mut account_associated_data_db: Box<
            dyn Storage<String, SerializableAccountAdditionalData>,
        > = new_kv_store(&database_url, "account_associated_data", &surfnet_id)?;
        // Store initial account associated data (native mint)
        account_associated_data_db.store(
            spl_token_interface::native_mint::ID.to_string(),
            native_mint_associated_data.into(),
        )?;
        token_mints_db.store(
            spl_token_interface::native_mint::ID.to_string(),
            parsed_mint_account,
        )?;
        let token_accounts_by_owner_db: Box<dyn Storage<String, Vec<String>>> =
            new_kv_store(&database_url, "token_accounts_by_owner", &surfnet_id)?;
        let token_accounts_by_delegate_db: Box<dyn Storage<String, Vec<String>>> =
            new_kv_store(&database_url, "token_accounts_by_delegate", &surfnet_id)?;
        let token_accounts_by_mint_db: Box<dyn Storage<String, Vec<String>>> =
            new_kv_store(&database_url, "token_accounts_by_mint", &surfnet_id)?;
        let streamed_accounts_db: Box<dyn Storage<String, bool>> =
            new_kv_store(&database_url, "streamed_accounts", &surfnet_id)?;
        let scheduled_overrides_db: Box<dyn Storage<u64, Vec<OverrideInstance>>> =
            new_kv_store(&database_url, "scheduled_overrides", &surfnet_id)?;
        let offline_accounts_db: Box<dyn Storage<String, OfflineAccountConfig>> =
            new_kv_store(&database_url, "offline_accounts", &surfnet_id)?;
        let registered_idls_db: Box<dyn Storage<String, Vec<VersionedIdl>>> =
            new_kv_store(&database_url, "registered_idls", &surfnet_id)?;
        let profile_tag_map_db: Box<dyn Storage<String, Vec<UuidOrSignature>>> =
            new_kv_store(&database_url, "profile_tag_map", &surfnet_id)?;
        let simulated_transaction_profiles_db: Box<dyn Storage<String, KeyedProfileResult>> =
            new_kv_store(&database_url, "simulated_transaction_profiles", &surfnet_id)?;
        let executed_transaction_profiles_db: Box<dyn Storage<String, KeyedProfileResult>> = {
            // Ensure max_profiles is at least 1 to avoid creating a zero-capacity FifoMap
            let max_profiles = max(1, config.max_profiles);
            new_kv_store_with_default(
                &database_url,
                "executed_transaction_profiles",
                &surfnet_id,
                // Use FifoMap for executed_transaction_profiles to maintain FIFO eviction behavior
                // (when no on-disk DB is provided)
                move || Box::new(FifoMap::<String, KeyedProfileResult>::new(max_profiles)),
            )?
        };
        let slot_checkpoint_db: Box<dyn Storage<String, u64>> =
            new_kv_store(&database_url, "slot_checkpoint", &surfnet_id)?;

        // Recover chain state: prefer slot checkpoint, fall back to max block in DB
        let checkpoint_slot = slot_checkpoint_db.get(&"latest_slot".to_string())?;
        let max_block_slot = blocks_db
            .into_iter()
            .unwrap()
            .max_by_key(|(slot, _): &(u64, BlockHeader)| *slot);

        let chain_tip = match (checkpoint_slot, max_block_slot) {
            // Prefer checkpoint if it's higher than the max stored block
            (Some(checkpoint), Some((block_slot, block))) => {
                if checkpoint > block_slot {
                    // Use checkpoint slot with synthetic blockhash
                    BlockIdentifier {
                        index: checkpoint,
                        hash: SyntheticBlockhash::new(checkpoint).to_string(),
                    }
                } else {
                    // Use the stored block
                    BlockIdentifier {
                        index: block.block_height,
                        hash: block.hash,
                    }
                }
            }
            (Some(checkpoint), None) => BlockIdentifier {
                index: checkpoint,
                hash: SyntheticBlockhash::new(checkpoint).to_string(),
            },
            (None, Some((_, block))) => BlockIdentifier {
                index: block.block_height,
                hash: block.hash,
            },
            (None, None) => BlockIdentifier::zero(),
        };

        // Initialize transactions_processed from database count for persistent storage
        let transactions_processed = transactions_db.count()?;
        let epoch_schedule = Self::default_epoch_schedule();
        let epoch_info = Self::default_epoch_info(&epoch_schedule);
        let updated_at = Utc::now().timestamp_millis() as u64;

        let mut svm = Self {
            inner,
            remote_rpc_url: None,
            chain_tip,
            blocks: blocks_db,
            transactions: transactions_db,
            perf_samples: VecDeque::new(),
            transactions_processed,
            simnet_events_tx,
            geyser_events_tx,
            latest_epoch_info: epoch_info.clone(),
            transactions_queued_for_confirmation: VecDeque::new(),
            transactions_queued_for_finalization: VecDeque::new(),
            signature_subscriptions: HashMap::new(),
            account_subscriptions: HashMap::new(),
            program_subscriptions: HashMap::new(),
            slot_subscriptions: Vec::new(),
            profile_tag_map: profile_tag_map_db,
            simulated_transaction_profiles: simulated_transaction_profiles_db,
            executed_transaction_profiles: executed_transaction_profiles_db,
            logs_subscriptions: Vec::new(),
            snapshot_subscriptions: Vec::new(),
            updated_at,
            slot_time: config.slot_time,
            start_time: SystemTime::now(),
            accounts_by_owner: accounts_by_owner_db,
            account_associated_data: account_associated_data_db,
            token_accounts: token_accounts_db,
            token_mints: token_mints_db,
            token_accounts_by_owner: token_accounts_by_owner_db,
            token_accounts_by_delegate: token_accounts_by_delegate_db,
            token_accounts_by_mint: token_accounts_by_mint_db,
            total_supply: 0,
            circulating_supply: 0,
            non_circulating_supply: 0,
            non_circulating_accounts: Vec::new(),
            genesis_config: GenesisConfig::default(),
            inflation: Inflation::default(),
            write_version: 0,
            registered_idls: registered_idls_db,
            feature_set: FeatureSet::default(),
            instruction_profiling_enabled: config.instruction_profiling_enabled,
            max_profiles: config.max_profiles,
            runbook_executions: Vec::new(),
            account_update_slots: HashMap::new(),
            streamed_accounts: streamed_accounts_db,
            recent_blockhashes: VecDeque::new(),
            scheduled_overrides: scheduled_overrides_db,
            offline_accounts: offline_accounts_db,
            genesis_slot: epoch_info.absolute_slot,
            genesis_updated_at: updated_at,
            slot_checkpoint: slot_checkpoint_db,
            last_checkpoint_slot: 0,
        };

        if config.feature_config != SvmFeatureConfig::default() {
            svm.apply_feature_config(&config.feature_config);
        }
        svm.inner.set_log_bytes_limit(config.log_bytes_limit);
        svm.chain_tip = svm.new_blockhash();
        svm.register_builtin_template_idls();
        svm.inner.set_sysvar(&epoch_schedule);
        svm.reconstruct_sysvars();

        Ok((svm, simnet_events_rx, geyser_events_rx))
    }

    /// Applies the SVM feature configuration to the internal feature set.
    ///
    /// This method enables or disables specific SVM features based on the provided configuration.
    /// Features explicitly listed in `enable` will be activated, and features in `disable` will be deactivated.
    ///
    /// # Arguments
    /// * `config` - The feature configuration specifying which features to enable/disable.
    pub fn apply_feature_config(&mut self, config: &SvmFeatureConfig) {
        let mut starting_set = FeatureSet::all_enabled();
        // Apply explicit enables
        for pubkey in &config.enable {
            debug!("Activating feature {}", pubkey);
            starting_set.activate(pubkey, 0);
        }

        // Apply explicit disables
        for pubkey in &config.disable {
            debug!("Deactivating feature {}", pubkey);
            starting_set.deactivate(pubkey);
        }
        self.feature_set = starting_set;
        // Rebuild inner VM with updated feature set
        self.inner.apply_feature_config(self.feature_set.clone());
    }

    pub fn increment_write_version(&mut self) -> u64 {
        self.write_version += 1;
        self.write_version
    }

    /// Initializes the SVM with the provided epoch info and epoch schedule.
    ///
    /// This is reserved for remote-derived startup data that is not known until the runloop
    /// is ready to connect to a remote RPC.
    ///
    /// # Arguments
    /// * `epoch_info` - The epoch information to initialize with.
    pub fn initialize(&mut self, epoch_info: EpochInfo, epoch_schedule: EpochSchedule) {
        self.chain_tip = self.new_blockhash();
        self.latest_epoch_info = epoch_info.clone();
        // Set genesis_slot to the current slot when initializing (syncing with remote)
        // This marks the starting point for this surfnet instance
        self.genesis_slot = epoch_info.absolute_slot;
        self.updated_at = Utc::now().timestamp_millis() as u64;
        // Update genesis_updated_at to match the new genesis_slot
        self.genesis_updated_at = self.updated_at;

        self.inner.set_sysvar(&epoch_schedule);

        // Reconstruct all sysvars (RecentBlockhashes, SlotHashes, Clock)
        self.reconstruct_sysvars();
    }

    pub fn set_profile_instructions(&mut self, do_profile_instructions: bool) {
        self.instruction_profiling_enabled = do_profile_instructions;
    }

    /// Airdrops a specified amount of lamports to a single public key.
    ///
    /// # Arguments
    /// * `pubkey` - The recipient public key.
    /// * `lamports` - The amount of lamports to airdrop.
    ///
    /// # Returns
    /// A `TransactionResult` indicating success or failure.
    #[allow(clippy::result_large_err)]
    pub fn airdrop(&mut self, pubkey: &Pubkey, lamports: u64) -> SurfpoolResult<TransactionResult> {
        // Capture pre-airdrop balances for the airdrop account, recipient, and system program.
        let airdrop_pubkey = self.inner.airdrop_pubkey();

        let airdrop_account_before = self
            .get_account(&airdrop_pubkey)?
            .unwrap_or_else(|| Account::default());
        let recipient_account_before = self
            .get_account(pubkey)?
            .unwrap_or_else(|| Account::default());
        let system_account_before = self
            .get_account(&system_program::id())?
            .unwrap_or_else(|| Account::default());

        let res = self.inner.airdrop(pubkey, lamports);
        let (status_tx, _rx) = unbounded();
        if let Ok(ref tx_result) = res {
            let slot = self.latest_epoch_info.absolute_slot;
            // Capture post-airdrop balances
            let airdrop_account_after = self
                .get_account(&airdrop_pubkey)?
                .unwrap_or_else(|| Account::default());
            let recipient_account_after = self
                .get_account(pubkey)?
                .unwrap_or_else(|| Account::default());
            let system_account_after = self
                .get_account(&system_program::id())?
                .unwrap_or_else(|| Account::default());

            // Construct a synthetic transaction that mirrors the underlying airdrop.
            let tx = VersionedTransaction {
                signatures: vec![tx_result.signature],
                message: VersionedMessage::Legacy(Message::new(
                    &[system_instruction::transfer(
                        &airdrop_pubkey,
                        pubkey,
                        lamports,
                    )],
                    Some(&airdrop_pubkey),
                )),
            };

            self.transactions.store(
                tx.get_signature().to_string(),
                SurfnetTransactionStatus::processed(
                    TransactionWithStatusMeta {
                        slot,
                        transaction: tx.clone(),
                        meta: TransactionStatusMeta {
                            status: Ok(()),
                            fee: 5000,
                            pre_balances: vec![
                                airdrop_account_before.lamports,
                                recipient_account_before.lamports,
                                system_account_before.lamports,
                            ],
                            post_balances: vec![
                                airdrop_account_after.lamports,
                                recipient_account_after.lamports,
                                system_account_after.lamports,
                            ],
                            inner_instructions: Some(vec![]),
                            log_messages: Some(tx_result.logs.clone()),
                            pre_token_balances: Some(vec![]),
                            post_token_balances: Some(vec![]),
                            rewards: Some(vec![]),
                            loaded_addresses: LoadedAddresses::default(),
                            return_data: Some(tx_result.return_data.clone()),
                            compute_units_consumed: Some(tx_result.compute_units_consumed),
                            cost_units: None,
                        },
                    },
                    HashSet::from([*pubkey]),
                ),
            )?;
            self.notify_signature_subscribers(
                SignatureSubscriptionType::processed(),
                tx.get_signature(),
                slot,
                None,
            );
            self.notify_logs_subscribers(
                tx.get_signature(),
                None,
                tx_result.logs.clone(),
                CommitmentLevel::Processed,
            );
            self.transactions_queued_for_confirmation
                .push_back((tx, status_tx.clone(), None));
            let account = self.get_account(pubkey)?.unwrap();
            self.set_account(pubkey, account)?;
        }
        Ok(res)
    }

    /// Airdrops a specified amount of lamports to a list of public keys.
    ///
    /// # Arguments
    /// * `lamports` - The amount of lamports to airdrop.
    /// * `addresses` - Slice of recipient public keys.
    pub fn airdrop_pubkeys(&mut self, lamports: u64, addresses: &[Pubkey]) {
        for recipient in addresses {
            match self.airdrop(recipient, lamports) {
                Ok(_) => {
                    let _ = self.simnet_events_tx.send(SimnetEvent::info(format!(
                        "Genesis airdrop successful {}: {}",
                        recipient, lamports
                    )));
                }
                Err(e) => {
                    let _ = self.simnet_events_tx.send(SimnetEvent::error(format!(
                        "Genesis airdrop failed {}: {}",
                        recipient, e
                    )));
                }
            };
        }
    }

    /// Returns the latest known absolute slot from the local epoch info.
    pub const fn get_latest_absolute_slot(&self) -> Slot {
        self.latest_epoch_info.absolute_slot
    }

    /// Returns the latest blockhash known by the SVM.
    pub fn latest_blockhash(&self) -> solana_hash::Hash {
        Hash::from_str(&self.chain_tip.hash).expect("Invalid blockhash")
    }

    /// Returns the latest epoch info known by the `SurfnetSvm`.
    pub fn latest_epoch_info(&self) -> EpochInfo {
        self.latest_epoch_info.clone()
    }

    /// Calculates the block time for a given slot based on genesis timestamp.
    /// Returns the time in milliseconds since genesis.
    pub fn calculate_block_time_for_slot(&self, slot: Slot) -> u64 {
        // Calculate time relative to genesis_slot (when this surfnet started)
        let slots_since_genesis = slot.saturating_sub(self.genesis_slot);
        self.genesis_updated_at + (slots_since_genesis * self.slot_time)
    }

    /// Checks if a slot is within the valid range for sparse block storage.
    /// A slot is valid if it's between genesis_slot (inclusive) and latest_slot (inclusive).
    ///
    /// # Arguments
    /// * `slot` - The slot number to check.
    ///
    /// # Returns
    /// `true` if the slot is within the valid range, `false` otherwise.
    pub fn is_slot_in_valid_range(&self, slot: Slot) -> bool {
        let latest_slot = self.get_latest_absolute_slot();
        slot >= self.genesis_slot && slot <= latest_slot
    }

    /// Gets a block from storage, or reconstructs an empty block if the slot is within
    /// the valid range (sparse block storage).
    ///
    /// # Arguments
    /// * `slot` - The slot number to retrieve.
    ///
    /// # Returns
    /// * `Ok(Some(BlockHeader))` - If the block exists or can be reconstructed
    /// * `Ok(None)` - If the slot is outside the valid range
    /// * `Err(_)` - If there was an error accessing storage
    pub fn get_block_or_reconstruct(&self, slot: Slot) -> SurfpoolResult<Option<BlockHeader>> {
        match self.blocks.get(&slot)? {
            Some(block) => Ok(Some(block)),
            None => {
                if self.is_slot_in_valid_range(slot) {
                    Ok(Some(self.reconstruct_empty_block(slot)))
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Reconstructs an empty block header for a slot that wasn't stored.
    /// This is used for sparse block storage where empty blocks are not persisted.
    pub fn reconstruct_empty_block(&self, slot: Slot) -> BlockHeader {
        let block_height = slot;
        BlockHeader {
            hash: SyntheticBlockhash::new(block_height).to_string(),
            previous_blockhash: SyntheticBlockhash::new(block_height.saturating_sub(1)).to_string(),
            parent_slot: slot.saturating_sub(1),
            block_time: (self.calculate_block_time_for_slot(slot) / 1_000) as i64,
            block_height,
            signatures: vec![],
        }
    }

    /// Reconstructs RecentBlockhashes, SlotHashes, and Clock sysvars deterministically
    /// from the current slot. Called on startup and after garbage collection to ensure
    /// consistent sysvar state without requiring database persistence.
    ///
    /// Note: SyntheticBlockhash uses chain_tip.index (relative index), while SlotHashes
    /// and Clock use absolute slots (chain_tip.index + genesis_slot).
    #[allow(deprecated)]
    pub fn reconstruct_sysvars(&mut self) {
        use solana_slot_hashes::SlotHashes;
        use solana_sysvar::recent_blockhashes::{IterItem, RecentBlockhashes};

        let current_index = self.chain_tip.index;
        let current_absolute_slot = self.get_latest_absolute_slot();

        // Calculate range for blockhashes - use relative indices for SyntheticBlockhash
        let start_index = current_index.saturating_sub(MAX_RECENT_BLOCKHASHES_STANDARD as u64 - 1);

        // Generate all synthetic blockhashes using relative indices (chain_tip.index style)
        // This matches how new_blockhash() generates hashes
        let synthetic_hashes: Vec<_> = (start_index..=current_index)
            .rev()
            .map(SyntheticBlockhash::new)
            .collect();

        // 1. Reconstruct RecentBlockhashes (last 150 blockhashes)
        let recent_blockhashes_vec: Vec<_> = synthetic_hashes
            .iter()
            .enumerate()
            .map(|(index, hash)| IterItem(index as u64, hash.hash(), 0))
            .collect();
        let recent_blockhashes = RecentBlockhashes::from_iter(recent_blockhashes_vec);
        self.inner.set_sysvar(&recent_blockhashes);

        // 2. Reconstruct SlotHashes - maps absolute slots to blockhashes
        let start_absolute_slot = start_index + self.genesis_slot;
        let slot_hashes_vec: Vec<_> = (start_absolute_slot..=current_absolute_slot)
            .rev()
            .zip(synthetic_hashes.iter())
            .map(|(slot, hash)| (slot, *hash.hash()))
            .collect();
        let slot_hashes = SlotHashes::new(&slot_hashes_vec);
        self.inner.set_sysvar(&slot_hashes);

        // 3. Reconstruct Clock using absolute slot
        let unix_timestamp = self.calculate_block_time_for_slot(current_absolute_slot) / 1_000;
        let clock = Clock {
            slot: current_absolute_slot,
            epoch: self.latest_epoch_info.epoch,
            unix_timestamp: unix_timestamp as i64,
            epoch_start_timestamp: 0,
            leader_schedule_epoch: 0,
        };
        self.inner.set_sysvar(&clock);
    }

    /// Generates and sets a new blockhash, updating the RecentBlockhashes sysvar.
    ///
    /// # Returns
    /// A new `BlockIdentifier` for the updated blockhash.
    #[allow(deprecated)]
    fn new_blockhash(&mut self) -> BlockIdentifier {
        use solana_slot_hashes::SlotHashes;
        use solana_sysvar::recent_blockhashes::{IterItem, RecentBlockhashes};
        // Backup the current block hashes
        let recent_blockhashes_backup = self.inner.get_sysvar::<RecentBlockhashes>();
        let num_blockhashes_expected = recent_blockhashes_backup
            .len()
            .min(MAX_RECENT_BLOCKHASHES_STANDARD);
        // Invalidate the current block hash.
        // LiteSVM bug / feature: calling this method empties `sysvar::<RecentBlockhashes>()`
        self.inner.expire_blockhash();
        // Rebuild recent blockhashes
        let mut recent_blockhashes = Vec::with_capacity(num_blockhashes_expected);
        let recent_blockhashes_overriden = self.inner.get_sysvar::<RecentBlockhashes>();
        let latest_entry = recent_blockhashes_overriden
            .first()
            .expect("Latest blockhash not found");

        let new_synthetic_blockhash = SyntheticBlockhash::new(self.chain_tip.index);
        let new_synthetic_blockhash_str = new_synthetic_blockhash.to_string();

        recent_blockhashes.push(IterItem(
            0,
            new_synthetic_blockhash.hash(),
            latest_entry.fee_calculator.lamports_per_signature,
        ));

        // Append the previous blockhashes, ignoring the first one
        for (index, entry) in recent_blockhashes_backup.iter().enumerate() {
            if recent_blockhashes.len() >= MAX_RECENT_BLOCKHASHES_STANDARD {
                break;
            }
            recent_blockhashes.push(IterItem(
                (index + 1) as u64,
                &entry.blockhash,
                entry.fee_calculator.lamports_per_signature,
            ));
        }

        self.inner
            .set_sysvar(&RecentBlockhashes::from_iter(recent_blockhashes));

        let mut slot_hashes = self.inner.get_sysvar::<SlotHashes>();
        slot_hashes.add(
            self.get_latest_absolute_slot() + 1,
            *new_synthetic_blockhash.hash(),
        );
        self.inner.set_sysvar(&SlotHashes::new(&slot_hashes));

        BlockIdentifier::new(
            self.chain_tip.index + 1,
            new_synthetic_blockhash_str.as_str(),
        )
    }

    /// Checks if the provided blockhash is recent (present in the RecentBlockhashes sysvar).
    ///
    /// # Arguments
    /// * `recent_blockhash` - The blockhash to check.
    ///
    /// # Returns
    /// `true` if the blockhash is recent, `false` otherwise.
    pub fn check_blockhash_is_recent(&self, recent_blockhash: &Hash) -> bool {
        #[allow(deprecated)]
        self.inner
            .get_sysvar::<solana_sysvar::recent_blockhashes::RecentBlockhashes>()
            .iter()
            .any(|entry| entry.blockhash == *recent_blockhash)
    }

    /// Validates the blockhash of a transaction, considering nonce accounts if present.
    /// If the transaction uses a nonce account, the blockhash is validated against the nonce account's stored blockhash.
    /// Otherwise, it is validated against the RecentBlockhashes sysvar.
    ///
    /// # Arguments
    /// * `tx` - The transaction to validate.
    ///
    /// # Returns
    /// `true` if the transaction blockhash is valid, `false` otherwise.
    pub fn validate_transaction_blockhash(&self, tx: &VersionedTransaction) -> bool {
        let recent_blockhash = tx.message.recent_blockhash();

        let some_nonce_account_index = tx
            .message
            .instructions()
            .get(solana_nonce::NONCED_TX_MARKER_IX_INDEX as usize)
            .filter(|instruction| {
                matches!(
                    tx.message.static_account_keys().get(instruction.program_id_index as usize),
                    Some(program_id) if system_program::check_id(program_id)
                ) && is_advance_nonce_instruction_data(&instruction.data)
            })
            .map(|instruction| {
                // nonce account is the first account in the instruction
                instruction.accounts.get(0)
            });

        debug!(
            "Validating tx blockhash: {}; is nonce tx?: {}",
            recent_blockhash,
            some_nonce_account_index.is_some()
        );

        if let Some(nonce_account_index) = some_nonce_account_index {
            trace!(
                "Nonce tx detected. Nonce account index: {:?}",
                nonce_account_index
            );
            let Some(nonce_account_index) = nonce_account_index else {
                return false;
            };

            let Some(nonce_account_pubkey) = tx
                .message
                .static_account_keys()
                .get(*nonce_account_index as usize)
            else {
                return false;
            };

            trace!("Nonce account pubkey: {:?}", nonce_account_pubkey,);

            // Here we're swallowing errors in the storage - if we fail to fetch the account because of a storage error,
            // we're just considering the blockhash to be invalid.
            let Ok(Some(nonce_account)) = self.get_account(nonce_account_pubkey) else {
                return false;
            };
            trace!("Nonce account: {:?}", nonce_account);

            let Some(nonce_data) =
                bincode::deserialize::<solana_nonce::versions::Versions>(&nonce_account.data).ok()
            else {
                return false;
            };
            trace!("Nonce account data: {:?}", nonce_data);

            let nonce_state = nonce_data.state();
            let initialized_state = match nonce_state {
                solana_nonce::state::State::Uninitialized => return false,
                solana_nonce::state::State::Initialized(data) => data,
            };
            return initialized_state.blockhash() == *recent_blockhash;
        } else {
            self.check_blockhash_is_recent(recent_blockhash)
        }
    }

    /// Verifies the signature of a transaction and validates that it hasn't already been processed.
    /// ### Note
    /// LiteSVM also can do this for our transactions, but we disable it.
    /// If sigverify is enabled at the LiteSVM level, the transaction simulations are always verified as well.
    /// So, if the user is trying to skip signature verification for a simulation, we'd need to unset and set this value,
    /// requiring a mutable reference to the SVM, which we don't have/want in the simulation path.
    /// Additionally, having this function internally lets us do this check before we start fetching accounts from mainnet.
    pub fn sigverify(&self, tx: &VersionedTransaction) -> Result<(), FailedTransactionMetadata> {
        let signature = tx.signatures[0];

        if tx.verify_with_results().iter().any(|valid| !*valid) {
            return Err(FailedTransactionMetadata {
                err: TransactionError::SignatureFailure,
                meta: TransactionMetadata::default(),
            });
        }

        if matches!(
            self.transactions.get(&signature.to_string()),
            Ok(Some(SurfnetTransactionStatus::Processed(_)))
        ) {
            return Err(FailedTransactionMetadata {
                err: TransactionError::AlreadyProcessed,
                meta: TransactionMetadata::default(),
            });
        }
        Ok(())
    }

    /// Sets an account in the local SVM state and notifies listeners.
    ///
    /// # Arguments
    /// * `pubkey` - The public key of the account.
    /// * `account` - The [Account] to insert.
    ///
    /// # Returns
    /// `Ok(())` on success, or an error if the operation fails.
    pub fn set_account(&mut self, pubkey: &Pubkey, account: Account) -> SurfpoolResult<()> {
        self.inner
            .set_account(*pubkey, account.clone())
            .map_err(|e| SurfpoolError::set_account(*pubkey, e))?;

        self.account_update_slots
            .insert(*pubkey, self.get_latest_absolute_slot());

        // Update the account registries and indexes
        self.update_account_registries(pubkey, &account)?;

        // Notify account subscribers
        self.notify_account_subscribers(pubkey, &account);

        // Notify program subscribers
        self.notify_program_subscribers(pubkey, &account);

        let _ = self
            .simnet_events_tx
            .send(SimnetEvent::account_update(*pubkey));
        Ok(())
    }

    pub fn update_account_registries(
        &mut self,
        pubkey: &Pubkey,
        account: &Account,
    ) -> SurfpoolResult<()> {
        let is_deleted_account = account == &Account::default();

        // Mirror the SVM state into the backing database. The inner SVM is
        // already up to date by the time this function runs; the database is
        // the side-effect target.
        if is_deleted_account {
            self.inner.delete_account_in_db(pubkey)?;
        } else {
            self.inner
                .set_account_in_db(*pubkey, account.clone().into())?;
        }

        if is_deleted_account {
            // Record the account as offline so the surfnet does not re-fetch
            // it from the upstream RPC, then drop any stale index entries
            // that pointed at its prior on-chain state.
            self.offline_accounts.store(
                pubkey.to_string(),
                OfflineAccountConfig {
                    include_owned_accounts: false,
                },
            )?;
            if let Some(old_account) = self.get_account(pubkey)? {
                self.remove_from_indexes(pubkey, &old_account)?;
            }
            return Ok(());
        }

        // Drop any stale owner/mint/delegate entries for the prior version of
        // the account before indexing the new one; otherwise a change of
        // owner would leave the old owner's bucket pointing at `pubkey`.
        if let Some(old_account) = self.get_account(pubkey)? {
            self.remove_from_indexes(pubkey, &old_account)?;
        }

        let pubkey_str = pubkey.to_string();
        add_pubkey_to_index(
            &mut self.accounts_by_owner,
            account.owner.to_string(),
            &pubkey_str,
        )?;

        if is_supported_token_program(&account.owner) {
            self.index_token_account_variant(pubkey, &pubkey_str, account)?;
            self.index_mint_account_variant(pubkey, account)?;
            self.index_token_2022_mint_extensions(pubkey, account)?;
        }
        Ok(())
    }

    /// If `account.data` decodes as a token account, add it to the
    /// owner/mint/delegate indexes and cache the unpacked `TokenAccount`.
    /// A decode failure is treated as "not a token account" (not an error);
    /// the enclosing call only dispatches here when the owner program is
    /// already known to be a supported token program.
    fn index_token_account_variant(
        &mut self,
        pubkey: &Pubkey,
        pubkey_str: &str,
        account: &Account,
    ) -> SurfpoolResult<()> {
        let Ok(token_account) = TokenAccount::unpack(&account.data) else {
            return Ok(());
        };
        add_pubkey_to_index(
            &mut self.token_accounts_by_owner,
            token_account.owner().to_string(),
            pubkey_str,
        )?;
        add_pubkey_to_index(
            &mut self.token_accounts_by_mint,
            token_account.mint().to_string(),
            pubkey_str,
        )?;
        if let COption::Some(delegate) = token_account.delegate() {
            add_pubkey_to_index(
                &mut self.token_accounts_by_delegate,
                delegate.to_string(),
                pubkey_str,
            )?;
        }
        self.token_accounts
            .store(pubkey.to_string(), token_account)?;
        Ok(())
    }

    /// If `account.data` decodes as a mint, cache the unpacked `MintAccount`.
    fn index_mint_account_variant(
        &mut self,
        pubkey: &Pubkey,
        account: &Account,
    ) -> SurfpoolResult<()> {
        let Ok(mint_account) = MintAccount::unpack(&account.data) else {
            return Ok(());
        };
        self.token_mints.store(pubkey.to_string(), mint_account)?;
        Ok(())
    }

    /// If `account.data` decodes as a Token-2022 mint with extensions,
    /// snapshot the decimals and rate-limited extension state
    /// (`InterestBearingConfig`, `ScaledUiAmountConfig`) into
    /// `account_associated_data` so the RPC layer can serve UI-amount
    /// conversions without re-parsing the raw account on every request.
    fn index_token_2022_mint_extensions(
        &mut self,
        pubkey: &Pubkey,
        account: &Account,
    ) -> SurfpoolResult<()> {
        let Ok(mint) =
            StateWithExtensions::<spl_token_2022_interface::state::Mint>::unpack(&account.data)
        else {
            return Ok(());
        };
        let unix_timestamp = self.inner.get_sysvar::<Clock>().unix_timestamp;
        let interest_bearing_config = mint
            .get_extension::<InterestBearingConfig>()
            .map(|x| (*x, unix_timestamp))
            .ok();
        let scaled_ui_amount_config = mint
            .get_extension::<ScaledUiAmountConfig>()
            .map(|x| (*x, unix_timestamp))
            .ok();
        let additional_data: SerializableAccountAdditionalData = AccountAdditionalDataV3 {
            spl_token_additional_data: Some(SplTokenAdditionalDataV2 {
                decimals: mint.base.decimals,
                interest_bearing_config,
                scaled_ui_amount_config,
            }),
        }
        .into();
        self.account_associated_data
            .store(pubkey.to_string(), additional_data)?;
        Ok(())
    }

    fn remove_from_indexes(
        &mut self,
        pubkey: &Pubkey,
        old_account: &Account,
    ) -> SurfpoolResult<()> {
        let pubkey_str = pubkey.to_string();
        remove_pubkey_from_index(
            &mut self.accounts_by_owner,
            &old_account.owner.to_string(),
            &pubkey_str,
        )?;

        if is_supported_token_program(&old_account.owner)
            && let Some(old_token_account) = self.token_accounts.take(&pubkey_str)?
        {
            remove_pubkey_from_index(
                &mut self.token_accounts_by_owner,
                &old_token_account.owner().to_string(),
                &pubkey_str,
            )?;
            remove_pubkey_from_index(
                &mut self.token_accounts_by_mint,
                &old_token_account.mint().to_string(),
                &pubkey_str,
            )?;
            if let COption::Some(delegate) = old_token_account.delegate() {
                remove_pubkey_from_index(
                    &mut self.token_accounts_by_delegate,
                    &delegate.to_string(),
                    &pubkey_str,
                )?;
            }
        }
        Ok(())
    }

    pub fn reset_network(
        &mut self,
        epoch_info: EpochInfo,
        epoch_schedule: EpochSchedule,
    ) -> SurfpoolResult<()> {
        self.inner.reset(self.feature_set.clone())?;

        let native_mint_account = self
            .inner
            .get_account(&spl_token_interface::native_mint::ID)?
            .unwrap();

        let native_mint_associated_data = {
            let mint = StateWithExtensions::<spl_token_2022_interface::state::Mint>::unpack(
                &native_mint_account.data,
            )
            .unwrap();
            let unix_timestamp = self.inner.get_sysvar::<Clock>().unix_timestamp;
            let interest_bearing_config = mint
                .get_extension::<InterestBearingConfig>()
                .map(|x| (*x, unix_timestamp))
                .ok();
            let scaled_ui_amount_config = mint
                .get_extension::<ScaledUiAmountConfig>()
                .map(|x| (*x, unix_timestamp))
                .ok();
            AccountAdditionalDataV3 {
                spl_token_additional_data: Some(SplTokenAdditionalDataV2 {
                    decimals: mint.base.decimals,
                    interest_bearing_config,
                    scaled_ui_amount_config,
                }),
            }
        };

        let parsed_mint_account = MintAccount::unpack(&native_mint_account.data).unwrap();

        self.blocks.clear()?;
        self.transactions.clear()?;
        self.transactions_queued_for_confirmation.clear();
        self.transactions_queued_for_finalization.clear();
        self.perf_samples.clear();
        self.transactions_processed = 0;
        self.profile_tag_map.clear()?;
        self.simulated_transaction_profiles.clear()?;
        self.executed_transaction_profiles.clear()?;
        self.accounts_by_owner.clear()?;
        self.accounts_by_owner.store(
            native_mint_account.owner.to_string(),
            vec![spl_token_interface::native_mint::ID.to_string()],
        )?;
        self.account_associated_data.clear()?;
        self.account_associated_data.store(
            spl_token_interface::native_mint::ID.to_string(),
            native_mint_associated_data.into(),
        )?;
        self.token_accounts.clear()?;
        self.token_mints.clear()?;
        self.token_mints.store(
            spl_token_interface::native_mint::ID.to_string(),
            parsed_mint_account,
        )?;
        self.token_accounts_by_owner.clear()?;
        self.token_accounts_by_delegate.clear()?;
        self.token_accounts_by_mint.clear()?;
        self.non_circulating_accounts.clear();
        self.registered_idls.clear()?;
        self.register_builtin_template_idls();
        self.runbook_executions.clear();
        self.streamed_accounts.clear()?;
        self.scheduled_overrides.clear()?;

        let current_time = chrono::Utc::now().timestamp_millis() as u64;
        self.updated_at = current_time;
        self.genesis_updated_at = current_time;
        self.latest_epoch_info = epoch_info.clone();
        // Set genesis_slot to the current slot when resetting (similar to initialize)
        self.genesis_slot = epoch_info.absolute_slot;
        let chain_tip_hash = SyntheticBlockhash::new(epoch_info.block_height).to_string();
        self.chain_tip = BlockIdentifier::new(epoch_info.block_height, chain_tip_hash.as_str());
        self.inner.set_sysvar(&epoch_schedule);
        // Rebuild sysvars so getLatestBlockhash / sendTransaction stay aligned after reset.
        self.reconstruct_sysvars();
        // Reset checkpoint state to avoid recovering stale chain tips after a reset.
        self.slot_checkpoint.clear()?;
        self.last_checkpoint_slot = self.genesis_slot;
        self.recent_blockhashes.clear();

        Ok(())
    }

    pub fn reset_account(
        &mut self,
        pubkey: &Pubkey,
        include_owned_accounts: bool,
    ) -> SurfpoolResult<()> {
        let Some(account) = self.get_account(pubkey)? else {
            return Ok(());
        };

        if account.executable {
            // Handle upgradeable program - also reset the program data account
            if account.owner == solana_sdk_ids::bpf_loader_upgradeable::id() {
                let program_data_pubkey =
                    solana_loader_v3_interface::get_program_data_address(pubkey);

                // Reset the program data account first
                self.purge_account_from_cache(&account, &program_data_pubkey)?;
            }
        }
        if include_owned_accounts {
            let owned_accounts = self.get_account_owned_by(pubkey)?;
            for (owned_pubkey, _) in owned_accounts {
                // Avoid infinite recursion by not cascading further
                self.purge_account_from_cache(&account, &owned_pubkey)?;
            }
        }
        // Reset the account itself
        self.purge_account_from_cache(&account, pubkey)?;
        Ok(())
    }

    fn purge_account_from_cache(
        &mut self,
        account: &Account,
        pubkey: &Pubkey,
    ) -> SurfpoolResult<()> {
        self.remove_from_indexes(pubkey, account)?;

        self.inner.delete_account(pubkey)?;

        Ok(())
    }

    /// Sends a transaction to the system for execution.
    ///
    /// This function attempts to send a transaction to the blockchain. It first increments the `transactions_processed` counter.
    /// Then it sends the transaction to the system and updates its status. If the transaction is successfully processed, it is
    /// cached locally, and a "transaction processed" event is sent. If the transaction fails, the error is recorded and an event
    /// is sent indicating the failure.
    ///
    /// # Arguments
    /// * `tx` - The transaction to send.
    /// * `cu_analysis_enabled` - Whether compute unit analysis is enabled.
    ///
    /// # Returns
    /// `Ok(res)` if processed successfully, or `Err(tx_failure)` if failed.
    #[allow(clippy::result_large_err)]
    pub fn send_transaction(
        &mut self,
        tx: VersionedTransaction,
        cu_analysis_enabled: bool,
        sigverify: bool,
    ) -> TransactionResult {
        if sigverify {
            self.sigverify(&tx)?;
        }

        if cu_analysis_enabled {
            let estimation_result = self.estimate_compute_units(&tx);
            let _ = self.simnet_events_tx.try_send(SimnetEvent::info(format!(
                "CU Estimation for tx: {} | Consumed: {} | Success: {} | Logs: {:?} | Error: {:?}",
                tx.signatures
                    .first()
                    .map_or_else(|| "N/A".to_string(), |s| s.to_string()),
                estimation_result.compute_units_consumed,
                estimation_result.success,
                estimation_result.log_messages,
                estimation_result.error_message
            )));
        }
        self.transactions_processed += 1;

        if !self.validate_transaction_blockhash(&tx) {
            let meta = TransactionMetadata::default();
            let err = solana_transaction_error::TransactionError::BlockhashNotFound;

            let transaction_meta = convert_transaction_metadata_from_canonical(&meta);

            let _ = self
                .simnet_events_tx
                .try_send(SimnetEvent::transaction_processed(
                    transaction_meta,
                    Some(err.clone()),
                ));
            return Err(FailedTransactionMetadata { err, meta });
        }

        match self.inner.send_transaction(tx.clone()) {
            Ok(res) => Ok(res),
            Err(tx_failure) => {
                let transaction_meta =
                    convert_transaction_metadata_from_canonical(&tx_failure.meta);

                let _ = self
                    .simnet_events_tx
                    .try_send(SimnetEvent::transaction_processed(
                        transaction_meta,
                        Some(tx_failure.err.clone()),
                    ));
                Err(tx_failure)
            }
        }
    }

    /// Estimates the compute units that a transaction will consume by simulating it.
    ///
    /// Does not commit any state changes to the SVM.
    ///
    /// # Arguments
    /// * `transaction` - The transaction to simulate.
    ///
    /// # Returns
    /// A `ComputeUnitsEstimationResult` with simulation details.
    pub fn estimate_compute_units(
        &self,
        transaction: &VersionedTransaction,
    ) -> ComputeUnitsEstimationResult {
        if !self.validate_transaction_blockhash(transaction) {
            return ComputeUnitsEstimationResult {
                success: false,
                compute_units_consumed: 0,
                log_messages: None,
                error_message: Some(
                    solana_transaction_error::TransactionError::BlockhashNotFound.to_string(),
                ),
            };
        }

        match self.inner.simulate_transaction(transaction.clone()) {
            Ok(sim_info) => ComputeUnitsEstimationResult {
                success: true,
                compute_units_consumed: sim_info.meta.compute_units_consumed,
                log_messages: Some(sim_info.meta.logs),
                error_message: None,
            },
            Err(failed_meta) => ComputeUnitsEstimationResult {
                success: false,
                compute_units_consumed: failed_meta.meta.compute_units_consumed,
                log_messages: Some(failed_meta.meta.logs),
                error_message: Some(failed_meta.err.to_string()),
            },
        }
    }

    /// Simulates a transaction and returns detailed simulation info or failure metadata.
    ///
    /// # Arguments
    /// * `tx` - The transaction to simulate.
    ///
    /// # Returns
    /// `Ok(SimulatedTransactionInfo)` if successful, or `Err(FailedTransactionMetadata)` if failed.
    #[allow(clippy::result_large_err)]
    pub fn simulate_transaction(
        &self,
        tx: VersionedTransaction,
        sigverify: bool,
    ) -> Result<SimulatedTransactionInfo, FailedTransactionMetadata> {
        if sigverify {
            self.sigverify(&tx)?;
        }

        if !self.validate_transaction_blockhash(&tx) {
            let meta = TransactionMetadata::default();
            let err = TransactionError::BlockhashNotFound;

            return Err(FailedTransactionMetadata { err, meta });
        }
        self.inner.simulate_transaction(tx)
    }

    /// Confirms transactions queued for confirmation, updates epoch/slot, and sends events.
    ///
    /// # Returns
    /// `Ok(Vec<Signature>)` with confirmed signatures, or `Err(SurfpoolError)` on error.
    fn confirm_transactions(&mut self) -> Result<(Vec<Signature>, HashSet<Pubkey>), SurfpoolError> {
        let mut confirmed_transactions = vec![];
        let slot = self.latest_epoch_info.slot_index;
        let current_slot = self.latest_epoch_info.absolute_slot;

        let mut all_mutated_account_keys = HashSet::new();

        while let Some((tx, status_tx, error)) =
            self.transactions_queued_for_confirmation.pop_front()
        {
            let _ = status_tx.try_send(TransactionStatusEvent::Success(
                TransactionConfirmationStatus::Confirmed,
            ));
            let signature = tx.signatures[0];
            let finalized_at = self.latest_epoch_info.absolute_slot + FINALIZATION_SLOT_THRESHOLD;
            self.transactions_queued_for_finalization.push_back((
                finalized_at,
                tx,
                status_tx,
                error.clone(),
            ));

            self.notify_signature_subscribers(
                SignatureSubscriptionType::confirmed(),
                &signature,
                slot,
                error,
            );

            let Some(SurfnetTransactionStatus::Processed(tx_data)) =
                self.transactions.get(&signature.to_string()).ok().flatten()
            else {
                continue;
            };
            let (tx_with_status_meta, mutated_account_keys) = tx_data.as_ref();
            all_mutated_account_keys.extend(mutated_account_keys);

            for pubkey in mutated_account_keys {
                self.account_update_slots.insert(*pubkey, current_slot);
            }

            self.notify_logs_subscribers(
                &signature,
                None,
                tx_with_status_meta
                    .meta
                    .log_messages
                    .clone()
                    .unwrap_or(vec![]),
                CommitmentLevel::Confirmed,
            );
            confirmed_transactions.push(signature);
        }

        Ok((confirmed_transactions, all_mutated_account_keys))
    }

    /// Finalizes transactions queued for finalization, sending finalized events as needed.
    ///
    /// # Returns
    /// `Ok(())` on success, or `Err(SurfpoolError)` on error.
    fn finalize_transactions(&mut self) -> Result<(), SurfpoolError> {
        let current_slot = self.latest_epoch_info.absolute_slot;
        let mut requeue = VecDeque::new();
        while let Some((finalized_at, tx, status_tx, error)) =
            self.transactions_queued_for_finalization.pop_front()
        {
            if current_slot >= finalized_at {
                let _ = status_tx.try_send(TransactionStatusEvent::Success(
                    TransactionConfirmationStatus::Finalized,
                ));
                let signature = &tx.signatures[0];
                self.notify_signature_subscribers(
                    SignatureSubscriptionType::finalized(),
                    signature,
                    self.latest_epoch_info.absolute_slot,
                    error,
                );
                let Some(SurfnetTransactionStatus::Processed(tx_data)) =
                    self.transactions.get(&signature.to_string()).ok().flatten()
                else {
                    continue;
                };
                let (tx_with_status_meta, _) = tx_data.as_ref();
                let logs = tx_with_status_meta
                    .meta
                    .log_messages
                    .clone()
                    .unwrap_or(vec![]);
                self.notify_logs_subscribers(signature, None, logs, CommitmentLevel::Finalized);
            } else {
                requeue.push_back((finalized_at, tx, status_tx, error));
            }
        }
        // Requeue any transactions that are not yet finalized
        self.transactions_queued_for_finalization
            .append(&mut requeue);

        Ok(())
    }

    /// Writes account updates to the SVM state based on the provided account update result.
    ///
    /// # Arguments
    /// * `account_update` - The account update result to process.
    pub fn write_account_update(&mut self, account_update: GetAccountResult) {
        let init_programdata_account = |program_account: &Account| {
            if !program_account.executable {
                return None;
            }
            if !program_account
                .owner
                .eq(&solana_sdk_ids::bpf_loader_upgradeable::id())
            {
                return None;
            }
            let Ok(UpgradeableLoaderState::Program {
                programdata_address,
            }) = bincode::deserialize::<UpgradeableLoaderState>(&program_account.data)
            else {
                return None;
            };

            let programdata_state = UpgradeableLoaderState::ProgramData {
                upgrade_authority_address: Some(system_program::id()),
                slot: self.get_latest_absolute_slot(),
            };
            let mut data = bincode::serialize(&programdata_state).unwrap();

            data.extend_from_slice(crate::surfnet::noop_program::NOOP_PROGRAM_ELF);
            let lamports = self.inner.minimum_balance_for_rent_exemption(data.len());
            Some((
                programdata_address,
                Account {
                    lamports,
                    data,
                    owner: solana_sdk_ids::bpf_loader_upgradeable::id(),
                    executable: false,
                    rent_epoch: 0,
                },
            ))
        };
        match account_update {
            GetAccountResult::FoundAccount(pubkey, account, do_update_account) => {
                if do_update_account {
                    if let Some((programdata_address, programdata_account)) =
                        init_programdata_account(&account)
                    {
                        match self.get_account(&programdata_address) {
                            Ok(None) => {
                                if let Err(e) =
                                    self.set_account(&programdata_address, programdata_account)
                                {
                                    let _ = self
                                        .simnet_events_tx
                                        .send(SimnetEvent::error(e.to_string()));
                                }
                            }
                            Ok(Some(_)) => {}
                            Err(e) => {
                                let _ = self
                                    .simnet_events_tx
                                    .send(SimnetEvent::error(e.to_string()));
                            }
                        }
                    }
                    if let Err(e) = self.set_account(&pubkey, account.clone()) {
                        let _ = self
                            .simnet_events_tx
                            .send(SimnetEvent::error(e.to_string()));
                    }
                }
            }
            GetAccountResult::FoundProgramAccount((pubkey, account), (_, None)) => {
                if let Some((programdata_address, programdata_account)) =
                    init_programdata_account(&account)
                {
                    match self.get_account(&programdata_address) {
                        Ok(None) => {
                            if let Err(e) =
                                self.set_account(&programdata_address, programdata_account)
                            {
                                let _ = self
                                    .simnet_events_tx
                                    .send(SimnetEvent::error(e.to_string()));
                            }
                        }
                        Ok(Some(_)) => {}
                        Err(e) => {
                            let _ = self
                                .simnet_events_tx
                                .send(SimnetEvent::error(e.to_string()));
                        }
                    }
                }
                if let Err(e) = self.set_account(&pubkey, account.clone()) {
                    let _ = self
                        .simnet_events_tx
                        .send(SimnetEvent::error(e.to_string()));
                }
            }
            GetAccountResult::FoundTokenAccount((pubkey, account), (_, None)) => {
                if let Err(e) = self.set_account(&pubkey, account.clone()) {
                    let _ = self
                        .simnet_events_tx
                        .send(SimnetEvent::error(e.to_string()));
                }
            }
            GetAccountResult::FoundProgramAccount(
                (pubkey, account),
                (coupled_pubkey, Some(coupled_account)),
            )
            | GetAccountResult::FoundTokenAccount(
                (pubkey, account),
                (coupled_pubkey, Some(coupled_account)),
            ) => {
                // The data account _must_ be set first, as the program account depends on it.
                if let Err(e) = self.set_account(&coupled_pubkey, coupled_account.clone()) {
                    let _ = self
                        .simnet_events_tx
                        .send(SimnetEvent::error(e.to_string()));
                }
                if let Err(e) = self.set_account(&pubkey, account.clone()) {
                    let _ = self
                        .simnet_events_tx
                        .send(SimnetEvent::error(e.to_string()));
                }
            }
            GetAccountResult::None(_) => {}
        }
    }

    pub fn confirm_current_block(&mut self) -> SurfpoolResult<()> {
        let slot = self.get_latest_absolute_slot();
        let previous_chain_tip = self.chain_tip.clone();
        if slot % *GARBAGE_COLLECTION_INTERVAL_SLOTS == 0 {
            debug!("Clearing liteSVM cache at slot {}", slot);
            self.inner.garbage_collect(self.feature_set.clone());
        }
        self.chain_tip = self.new_blockhash();
        // Confirm processed transactions
        let (confirmed_signatures, all_mutated_account_keys) = self.confirm_transactions()?;
        let write_version = self.increment_write_version();

        // Notify Geyser plugin of account updates
        for pubkey in all_mutated_account_keys {
            let Some(account) = self.inner.get_account(&pubkey)? else {
                continue;
            };
            self.geyser_events_tx
                .send(GeyserEvent::UpdateAccount(
                    GeyserAccountUpdate::block_update(pubkey, account, slot, write_version),
                ))
                .ok();
        }

        let num_transactions = confirmed_signatures.len() as u64;
        self.updated_at += self.slot_time;

        // Only store blocks that have transactions (sparse block storage)
        // Empty blocks can be reconstructed on-the-fly from their slot number
        if !confirmed_signatures.is_empty() {
            self.blocks.store(
                slot,
                BlockHeader {
                    hash: self.chain_tip.hash.clone(),
                    previous_blockhash: previous_chain_tip.hash.clone(),
                    block_time: self.updated_at as i64 / 1_000,
                    block_height: self.chain_tip.index,
                    parent_slot: slot,
                    signatures: confirmed_signatures,
                },
            )?;
        }

        // Checkpoint the latest slot periodically (~every 150 slots / 1 minute at standard slot time)
        // This allows recovery after restart without storing every empty block
        if slot.saturating_sub(self.last_checkpoint_slot) >= *CHECKPOINT_INTERVAL_SLOTS {
            self.slot_checkpoint
                .store("latest_slot".to_string(), slot)?;
            self.last_checkpoint_slot = slot;
        }

        if self.perf_samples.len() > 30 {
            self.perf_samples.pop_back();
        }
        self.perf_samples.push_front(RpcPerfSample {
            slot,
            num_slots: 1,
            sample_period_secs: 1,
            num_transactions,
            num_non_vote_transactions: Some(num_transactions),
        });

        self.latest_epoch_info.slot_index += 1;
        self.latest_epoch_info.block_height = self.chain_tip.index;
        self.latest_epoch_info.absolute_slot += 1;
        if self.latest_epoch_info.slot_index > self.latest_epoch_info.slots_in_epoch {
            self.latest_epoch_info.slot_index = 0;
            self.latest_epoch_info.epoch += 1;
        }
        let total_transactions = self.latest_epoch_info.transaction_count.unwrap_or(0);
        self.latest_epoch_info.transaction_count = Some(total_transactions + num_transactions);

        let parent_slot = self.latest_epoch_info.absolute_slot.saturating_sub(1);
        let new_slot = self.latest_epoch_info.absolute_slot;
        let root = new_slot.saturating_sub(FINALIZATION_SLOT_THRESHOLD);
        self.notify_slot_subscribers(new_slot, parent_slot, root);

        // Notify geyser plugins of slot status (Confirmed)
        self.geyser_events_tx
            .send(GeyserEvent::UpdateSlotStatus {
                slot: new_slot,
                parent: Some(parent_slot),
                status: GeyserSlotStatus::Confirmed,
            })
            .ok();

        // Notify geyser plugins of block metadata
        let block_metadata = GeyserBlockMetadata {
            slot: new_slot,
            blockhash: self.chain_tip.hash.clone(),
            parent_slot,
            parent_blockhash: previous_chain_tip.hash.clone(),
            block_time: Some(self.updated_at as i64 / 1_000),
            block_height: Some(self.chain_tip.index),
            executed_transaction_count: num_transactions,
            entry_count: 1, // Surfpool produces 1 entry per block
        };
        self.geyser_events_tx
            .send(GeyserEvent::NotifyBlockMetadata(block_metadata))
            .ok();

        // Notify geyser plugins of entry (Surfpool emits 1 entry per block)
        let entry_hash = solana_hash::Hash::from_str(&self.chain_tip.hash)
            .map(|h| h.to_bytes().to_vec())
            .unwrap_or_else(|_| vec![0u8; 32]);
        let entry_info = GeyserEntryInfo {
            slot: new_slot,
            index: 0, // Single entry per block
            num_hashes: 1,
            hash: entry_hash,
            executed_transaction_count: num_transactions,
            starting_transaction_index: 0,
        };
        self.geyser_events_tx
            .send(GeyserEvent::NotifyEntry(entry_info))
            .ok();

        let clock: Clock = Clock {
            slot: self.latest_epoch_info.absolute_slot,
            epoch: self.latest_epoch_info.epoch,
            unix_timestamp: self.updated_at as i64 / 1_000,
            epoch_start_timestamp: 0, // todo
            leader_schedule_epoch: 0, // todo
        };

        let _ = self
            .simnet_events_tx
            .send(SimnetEvent::SystemClockUpdated(clock.clone()));
        self.inner.set_sysvar(&clock);

        self.finalize_transactions()?;

        // Notify geyser plugins of newly rooted (finalized) slot
        // Only emit if root is a valid slot (greater than genesis)
        if root >= self.genesis_slot {
            self.geyser_events_tx
                .send(GeyserEvent::UpdateSlotStatus {
                    slot: root,
                    parent: root.checked_sub(1),
                    status: GeyserSlotStatus::Rooted,
                })
                .ok();
        }

        // Evict the accounts marked as streamed from cache to enforce them to be fetched again
        let accounts_to_reset: Vec<_> = self.streamed_accounts.into_iter()?.collect();
        for (pubkey_str, include_owned_accounts) in accounts_to_reset {
            let pubkey = Pubkey::from_str(&pubkey_str)
                .map_err(|e| SurfpoolError::invalid_pubkey(&pubkey_str, e.to_string()))?;
            self.reset_account(&pubkey, include_owned_accounts)?;
        }

        Ok(())
    }

    /// Materializes scheduled overrides for the current slot
    ///
    /// This function:
    /// 1. Dequeues overrides scheduled for the current slot
    /// 2. Resolves account addresses (Pubkey or PDA)
    /// 3. Optionally fetches fresh account data from remote if `fetch_before_use` is enabled
    /// 4. Applies the overrides to the account data
    /// 5. Updates the SVM state
    pub async fn materialize_overrides(
        &mut self,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    ) -> SurfpoolResult<()> {
        let current_slot = self.latest_epoch_info.absolute_slot;

        // Remove and get overrides for this slot
        let Some(overrides) = self.scheduled_overrides.take(&current_slot)? else {
            // No overrides for this slot
            return Ok(());
        };

        debug!(
            "Materializing {} override(s) for slot {}",
            overrides.len(),
            current_slot
        );

        for override_instance in overrides {
            if !override_instance.enabled {
                debug!("Skipping disabled override: {}", override_instance.id);
                continue;
            }

            // Resolve account address
            let account_pubkey = match &override_instance.account {
                surfpool_types::AccountAddress::Pubkey(pubkey_str) => {
                    match Pubkey::from_str(pubkey_str) {
                        Ok(pubkey) => pubkey,
                        Err(e) => {
                            warn!(
                                "Failed to parse pubkey '{}' for override {}: {}",
                                pubkey_str, override_instance.id, e
                            );
                            continue;
                        }
                    }
                }
                surfpool_types::AccountAddress::Pda {
                    program_id: _,
                    seeds: _,
                } => unimplemented!(),
            };

            debug!(
                "Processing override {} for account {} (label: {:?})",
                override_instance.id, account_pubkey, override_instance.label
            );

            // Fetch fresh account data from remote if requested
            if override_instance.fetch_before_use {
                if let Some((client, _)) = remote_ctx {
                    debug!(
                        "Fetching fresh account data for {} from remote",
                        account_pubkey
                    );

                    match client
                        .get_account(&account_pubkey, CommitmentConfig::confirmed())
                        .await
                    {
                        Ok(GetAccountResult::FoundAccount(_pubkey, remote_account, _)) => {
                            debug!(
                                "Fetched account {} from remote: {} lamports, {} bytes",
                                account_pubkey,
                                remote_account.lamports(),
                                remote_account.data().len()
                            );

                            // Set the fresh account data in the SVM
                            if let Err(e) = self.inner.set_account(account_pubkey, remote_account) {
                                warn!(
                                    "Failed to set account {} from remote: {}",
                                    account_pubkey, e
                                );
                            }
                        }
                        Ok(GetAccountResult::None(_)) => {
                            debug!("Account {} not found on remote", account_pubkey);
                        }
                        Ok(_) => {
                            debug!("Account {} fetched (other variant)", account_pubkey);
                        }
                        Err(e) => {
                            warn!(
                                "Failed to fetch account {} from remote: {}",
                                account_pubkey, e
                            );
                        }
                    }
                } else {
                    debug!(
                        "fetch_before_use enabled but no remote client available for override {}",
                        override_instance.id
                    );
                }
            }

            // Apply the override values to the account data
            if !override_instance.values.is_empty() {
                debug!(
                    "Override {} applying {} field modification(s) to account {}",
                    override_instance.id,
                    override_instance.values.len(),
                    account_pubkey
                );

                // Get the account from the SVM
                let Some(account) = self.inner.get_account(&account_pubkey)? else {
                    warn!(
                        "Account {} not found in SVM for override {}, skipping modifications",
                        account_pubkey, override_instance.id
                    );
                    continue;
                };

                // Get the account owner (program ID)
                let owner_program_id = account.owner();

                // Look up the IDL for the owner program
                let idl_versions = match self.registered_idls.get(&owner_program_id.to_string()) {
                    Ok(Some(versions)) => versions,
                    Ok(None) => {
                        warn!(
                            "No IDL registered for program {} (owner of account {}), skipping override {}",
                            owner_program_id, account_pubkey, override_instance.id
                        );
                        continue;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to get IDL for program {}: {}, skipping override {}",
                            owner_program_id, e, override_instance.id
                        );
                        continue;
                    }
                };

                // Get the latest IDL version (first in the sorted Vec)
                let Some(versioned_idl) = idl_versions.first() else {
                    warn!(
                        "IDL versions empty for program {}, skipping override {}",
                        owner_program_id, override_instance.id
                    );
                    continue;
                };

                let idl = &versioned_idl.1;

                // Get account data
                let account_data = account.data();

                // Use get_forged_account_data to apply the overrides
                let new_account_data = match self.get_forged_account_data(
                    &account_pubkey,
                    account_data,
                    idl,
                    &override_instance.values,
                ) {
                    Ok(data) => data,
                    Err(e) => {
                        warn!(
                            "Failed to forge account data for {} (override {}): {}",
                            account_pubkey, override_instance.id, e
                        );
                        continue;
                    }
                };

                // Create a new account with modified data
                let modified_account = Account {
                    lamports: account.lamports(),
                    data: new_account_data,
                    owner: *account.owner(),
                    executable: account.executable(),
                    rent_epoch: account.rent_epoch(),
                };

                // Update the account in the SVM
                if let Err(e) = self.inner.set_account(account_pubkey, modified_account) {
                    warn!(
                        "Failed to set modified account {} in SVM: {}",
                        account_pubkey, e
                    );
                } else {
                    debug!(
                        "Successfully applied {} override(s) to account {} (override {})",
                        override_instance.values.len(),
                        account_pubkey,
                        override_instance.id
                    );
                }
            }
        }

        Ok(())
    }

    /// Forges account data by applying overrides to existing account data
    ///
    /// This function:
    /// 1. Validates account data size (must be at least 8 bytes for discriminator)
    /// 2. Splits discriminator and serialized data
    /// 3. Finds the account type in the IDL using the discriminator
    /// 4. Deserializes the account data
    /// 5. Applies field overrides using dot notation
    /// 6. Re-serializes the modified data
    /// 7. Reconstructs the account data with the original discriminator
    ///
    /// # Arguments
    /// * `account_pubkey` - The account address (for error messages)
    /// * `account_data` - The original account data bytes
    /// * `idl` - The IDL for the account's program
    /// * `overrides` - Map of field paths to new values
    ///
    /// # Returns
    /// The forged account data as bytes, or an error
    pub fn get_forged_account_data(
        &self,
        account_pubkey: &Pubkey,
        account_data: &[u8],
        idl: &Idl,
        overrides: &HashMap<String, serde_json::Value>,
    ) -> SurfpoolResult<Vec<u8>> {
        // Validate account data size
        if account_data.len() < 8 {
            return Err(SurfpoolError::invalid_account_data(
                account_pubkey,
                "Account data too small to be an Anchor account (need at least 8 bytes for discriminator)",
                Some("Data length too small"),
            ));
        }

        // Split discriminator and data
        let discriminator = &account_data[..8];
        let serialized_data = &account_data[8..];

        // Find the account type using the discriminator
        let account_def = idl
            .accounts
            .iter()
            .find(|acc| acc.discriminator.eq(discriminator))
            .ok_or_else(|| {
                SurfpoolError::internal(format!(
                    "Account with discriminator '{:?}' not found in IDL",
                    discriminator
                ))
            })?;

        // Find the corresponding type definition
        let account_type = idl
            .types
            .iter()
            .find(|t| t.name == account_def.name)
            .ok_or_else(|| {
                SurfpoolError::internal(format!(
                    "Type definition for account '{}' not found in IDL",
                    account_def.name
                ))
            })?;

        // Set up generics for parsing
        let empty_vec = vec![];
        let idl_type_def_generics = idl
            .types
            .iter()
            .find(|t| t.name == account_type.name)
            .map(|t| &t.generics);

        // Deserialize the account data using proper Borsh deserialization
        // Use the version that returns leftover bytes to preserve any trailing padding
        let (mut parsed_value, leftover_bytes) =
            parse_bytes_to_value_with_expected_idl_type_def_ty_with_leftover_bytes(
                serialized_data,
                &account_type.ty,
                &idl.types,
                &vec![],
                idl_type_def_generics.unwrap_or(&empty_vec),
            )
            .map_err(|e| {
                SurfpoolError::deserialize_error(
                    "account data",
                    format!("Failed to deserialize account data using Borsh: {}", e),
                )
            })?;

        // Apply overrides to the decoded value
        for (path, value) in overrides {
            apply_override_to_decoded_account(&mut parsed_value, path, value)?;
        }

        // Construct an IdlType::Defined that references the account type
        // This is needed because borsh_encode_value_to_idl_type expects IdlType, not IdlTypeDefTy
        use anchor_lang_idl::types::{IdlGenericArg, IdlType};
        let defined_type = IdlType::Defined {
            name: account_type.name.clone(),
            generics: account_type
                .generics
                .iter()
                .map(|_| IdlGenericArg::Type {
                    ty: IdlType::String,
                })
                .collect(),
        };

        // Re-encode the value using Borsh
        let re_encoded_data =
            borsh_encode_value_to_idl_type(&parsed_value, &defined_type, &idl.types, None)
                .map_err(|e| {
                    SurfpoolError::internal(format!(
                        "Failed to re-encode account data using Borsh: {}",
                        e
                    ))
                })?;

        // Reconstruct the account data with discriminator and preserve any trailing bytes
        let mut new_account_data =
            Vec::with_capacity(8 + re_encoded_data.len() + leftover_bytes.len());
        new_account_data.extend_from_slice(discriminator);
        new_account_data.extend_from_slice(&re_encoded_data);
        new_account_data.extend_from_slice(leftover_bytes);

        Ok(new_account_data)
    }

    /// Subscribes for updates on a transaction signature for a given subscription type.
    ///
    /// # Arguments
    /// * `signature` - The transaction signature to subscribe to.
    /// * `subscription_type` - The type of subscription (confirmed/finalized).
    ///
    /// # Returns
    /// A receiver for slot and transaction error updates.
    pub fn subscribe_for_signature_updates(
        &mut self,
        signature: &Signature,
        subscription_type: SignatureSubscriptionType,
    ) -> Receiver<(Slot, Option<TransactionError>)> {
        let (tx, rx) = unbounded();
        self.signature_subscriptions
            .entry(*signature)
            .or_default()
            .push((subscription_type, tx));
        rx
    }

    pub fn subscribe_for_account_updates(
        &mut self,
        account_pubkey: &Pubkey,
        encoding: Option<UiAccountEncoding>,
    ) -> Receiver<UiAccount> {
        let (tx, rx) = unbounded();
        self.account_subscriptions
            .entry(*account_pubkey)
            .or_default()
            .push((encoding, tx));
        rx
    }

    pub fn subscribe_for_program_updates(
        &mut self,
        program_id: &Pubkey,
        encoding: Option<UiAccountEncoding>,
        filters: Option<Vec<RpcFilterType>>,
    ) -> Receiver<RpcKeyedAccount> {
        let (tx, rx) = unbounded();
        self.program_subscriptions
            .entry(*program_id)
            .or_default()
            .push((encoding, filters, tx));
        rx
    }

    /// Notifies signature subscribers of a status update, sending slot and error info.
    ///
    /// # Arguments
    /// * `status` - The subscription type (confirmed/finalized).
    /// * `signature` - The transaction signature.
    /// * `slot` - The slot number.
    /// * `err` - Optional transaction error.
    pub fn notify_signature_subscribers(
        &mut self,
        status: SignatureSubscriptionType,
        signature: &Signature,
        slot: Slot,
        err: Option<TransactionError>,
    ) {
        let mut remaining = vec![];
        if let Some(subscriptions) = self.signature_subscriptions.remove(signature) {
            for (subscription_type, tx) in subscriptions {
                if status.eq(&subscription_type) {
                    if tx.send((slot, err.clone())).is_err() {
                        // The receiver has been dropped, so we can skip notifying
                        continue;
                    }
                } else {
                    remaining.push((subscription_type, tx));
                }
            }
            if !remaining.is_empty() {
                self.signature_subscriptions.insert(*signature, remaining);
            }
        }
    }

    pub fn notify_account_subscribers(
        &mut self,
        account_updated_pubkey: &Pubkey,
        account: &Account,
    ) {
        let mut remaining = vec![];
        if let Some(subscriptions) = self.account_subscriptions.remove(account_updated_pubkey) {
            for (encoding, tx) in subscriptions {
                let config = RpcAccountInfoConfig {
                    encoding,
                    ..Default::default()
                };
                let account = self
                    .account_to_rpc_keyed_account(account_updated_pubkey, account, &config, None)
                    .account;
                if tx.send(account).is_err() {
                    // The receiver has been dropped, so we can skip notifying
                    continue;
                } else {
                    remaining.push((encoding, tx));
                }
            }
            if !remaining.is_empty() {
                self.account_subscriptions
                    .insert(*account_updated_pubkey, remaining);
            }
        }
    }

    pub fn notify_program_subscribers(&mut self, account_pubkey: &Pubkey, account: &Account) {
        let program_id = account.owner;
        let mut remaining = vec![];
        if let Some(subscriptions) = self.program_subscriptions.remove(&program_id) {
            for (encoding, filters, tx) in subscriptions {
                // Apply filters if present
                if let Some(ref active_filters) = filters {
                    match super::locker::apply_rpc_filters(&account.data, active_filters) {
                        Ok(true) => {} // Account matches all filters
                        Ok(false) => {
                            // Filtered out - keep subscription active but don't notify
                            remaining.push((encoding, filters, tx));
                            continue;
                        }
                        Err(_) => {
                            // Error applying filter - keep subscription, skip notification
                            remaining.push((encoding, filters, tx));
                            continue;
                        }
                    }
                }

                let config = RpcAccountInfoConfig {
                    encoding,
                    ..Default::default()
                };
                let keyed_account =
                    self.account_to_rpc_keyed_account(account_pubkey, account, &config, None);
                if tx.send(keyed_account).is_err() {
                    // The receiver has been dropped, so we can skip notifying
                    continue;
                } else {
                    remaining.push((encoding, filters, tx));
                }
            }
            if !remaining.is_empty() {
                self.program_subscriptions.insert(program_id, remaining);
            }
        }
    }

    /// Retrieves a confirmed block at the given slot, including transactions and metadata.
    ///
    /// # Arguments
    /// * `slot` - The slot number to retrieve the block for.
    /// * `config` - The configuration for the block retrieval.
    ///
    /// # Returns
    /// `Some(UiConfirmedBlock)` if found, or `None` if not present.
    pub fn get_block_at_slot(
        &self,
        slot: Slot,
        config: &RpcBlockConfig,
    ) -> SurfpoolResult<Option<UiConfirmedBlock>> {
        // Try to get stored block, or reconstruct empty block if within valid range
        let Some(block) = self.get_block_or_reconstruct(slot)? else {
            return Ok(None);
        };

        let show_rewards = config.rewards.unwrap_or(true);
        let transaction_details = config
            .transaction_details
            .unwrap_or(TransactionDetails::Full);

        let transactions = match transaction_details {
            TransactionDetails::Full => Some(
                block
                    .signatures
                    .iter()
                    .filter_map(|sig| self.transactions.get(&sig.to_string()).ok().flatten())
                    .map(|tx_with_meta| {
                        let (meta, _) = tx_with_meta.expect_processed();
                        meta.encode(
                            config.encoding.unwrap_or(
                                solana_transaction_status::UiTransactionEncoding::JsonParsed,
                            ),
                            config.max_supported_transaction_version,
                            show_rewards,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(SurfpoolError::from)?,
            ),
            TransactionDetails::Signatures => None,
            TransactionDetails::None => None,
            TransactionDetails::Accounts => Some(
                block
                    .signatures
                    .iter()
                    .filter_map(|sig| self.transactions.get(&sig.to_string()).ok().flatten())
                    .map(|tx_with_meta| {
                        let (meta, _) = tx_with_meta.expect_processed();
                        meta.to_json_accounts(
                            config.max_supported_transaction_version,
                            show_rewards,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(SurfpoolError::from)?,
            ),
        };

        let signatures = match transaction_details {
            TransactionDetails::Signatures => {
                Some(block.signatures.iter().map(|t| t.to_string()).collect())
            }
            TransactionDetails::Full | TransactionDetails::Accounts | TransactionDetails::None => {
                None
            }
        };

        let block = UiConfirmedBlock {
            previous_blockhash: block.previous_blockhash.clone(),
            blockhash: block.hash.clone(),
            parent_slot: block.parent_slot,
            transactions,
            signatures,
            rewards: if show_rewards { Some(vec![]) } else { None },
            num_reward_partitions: None,
            block_time: Some(block.block_time / 1000),
            block_height: Some(block.block_height),
        };
        Ok(Some(block))
    }

    /// Returns the blockhash for a given slot, if available.
    pub fn blockhash_for_slot(&self, slot: Slot) -> Option<Hash> {
        self.blocks
            .get(&slot)
            .unwrap()
            .and_then(|header| header.hash.parse().ok())
    }

    /// Gets all accounts owned by a specific program ID from the account registry.
    ///
    /// # Arguments
    ///
    /// * `program_id` - The program ID to search for owned accounts.
    ///
    /// # Returns
    ///
    /// * A vector of (account_pubkey, account) tuples for all accounts owned by the program.
    pub fn get_account_owned_by(
        &self,
        program_id: &Pubkey,
    ) -> SurfpoolResult<Vec<(Pubkey, Account)>> {
        let account_pubkeys = self
            .accounts_by_owner
            .get(&program_id.to_string())
            .ok()
            .flatten()
            .unwrap_or_default();

        account_pubkeys
            .iter()
            .filter_map(|pk_str| {
                let pk = Pubkey::from_str(pk_str).ok()?;
                self.get_account(&pk)
                    .map(|res| res.map(|account| (pk, account.clone())))
                    .transpose()
            })
            .collect::<Result<Vec<_>, SurfpoolError>>()
    }

    fn get_additional_data(
        &self,
        pubkey: &Pubkey,
        token_mint: Option<Pubkey>,
    ) -> Option<AccountAdditionalDataV3> {
        let token_mint = if let Some(mint) = token_mint {
            Some(mint)
        } else {
            self.token_accounts
                .get(&pubkey.to_string())
                .ok()
                .flatten()
                .map(|ta| ta.mint())
        };

        token_mint.and_then(|mint| {
            self.account_associated_data
                .get(&mint.to_string())
                .ok()
                .flatten()
                .and_then(|data| data.try_into().ok())
        })
    }

    pub fn account_to_rpc_keyed_account<T: ReadableAccount>(
        &self,
        pubkey: &Pubkey,
        account: &T,
        config: &RpcAccountInfoConfig,
        token_mint: Option<Pubkey>,
    ) -> RpcKeyedAccount {
        let additional_data = self.get_additional_data(pubkey, token_mint);

        RpcKeyedAccount {
            pubkey: pubkey.to_string(),
            account: self.encode_ui_account(
                pubkey,
                account,
                config.encoding.unwrap_or(UiAccountEncoding::Base64),
                additional_data,
                config.data_slice,
            ),
        }
    }

    /// Gets all token accounts that have delegated authority to a specific delegate.
    ///
    /// # Arguments
    ///
    /// * `delegate` - The delegate pubkey to search for token accounts that have granted authority.
    ///
    /// # Returns
    ///
    /// * A vector of (account_pubkey, token_account) tuples for all token accounts delegated to the specified delegate.
    pub fn get_token_accounts_by_delegate(&self, delegate: &Pubkey) -> Vec<(Pubkey, TokenAccount)> {
        if let Some(account_pubkeys) = self
            .token_accounts_by_delegate
            .get(&delegate.to_string())
            .ok()
            .flatten()
        {
            account_pubkeys
                .iter()
                .filter_map(|pk_str| {
                    let pk = Pubkey::from_str(pk_str).ok()?;
                    self.token_accounts
                        .get(pk_str)
                        .ok()
                        .flatten()
                        .map(|ta| (pk, ta))
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Gets all token accounts owned by a specific owner.
    ///
    /// # Arguments
    ///
    /// * `owner` - The owner pubkey to search for token accounts.
    ///
    /// # Returns
    ///
    /// * A vector of (account_pubkey, token_account) tuples for all token accounts owned by the specified owner.
    pub fn get_parsed_token_accounts_by_owner(
        &self,
        owner: &Pubkey,
    ) -> Vec<(Pubkey, TokenAccount)> {
        if let Some(account_pubkeys) = self
            .token_accounts_by_owner
            .get(&owner.to_string())
            .ok()
            .flatten()
        {
            account_pubkeys
                .iter()
                .filter_map(|pk_str| {
                    let pk = Pubkey::from_str(pk_str).ok()?;
                    self.token_accounts
                        .get(pk_str)
                        .ok()
                        .flatten()
                        .map(|ta| (pk, ta))
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn get_token_accounts_by_owner(
        &self,
        owner: &Pubkey,
    ) -> SurfpoolResult<Vec<(Pubkey, Account)>> {
        let account_pubkeys = self
            .token_accounts_by_owner
            .get(&owner.to_string())
            .ok()
            .flatten()
            .unwrap_or_default();

        account_pubkeys
            .iter()
            .filter_map(|pk_str| {
                let pk = Pubkey::from_str(pk_str).ok()?;
                self.get_account(&pk)
                    .map(|res| res.map(|account| (pk, account.clone())))
                    .transpose()
            })
            .collect::<Result<Vec<_>, SurfpoolError>>()
    }

    /// Gets all token accounts for a specific mint (token type).
    ///
    /// # Arguments
    ///
    /// * `mint` - The mint pubkey to search for token accounts.
    ///
    /// # Returns
    ///
    /// * A vector of (account_pubkey, token_account) tuples for all token accounts of the specified mint.
    pub fn get_token_accounts_by_mint(&self, mint: &Pubkey) -> Vec<(Pubkey, TokenAccount)> {
        if let Some(account_pubkeys) = self
            .token_accounts_by_mint
            .get(&mint.to_string())
            .ok()
            .flatten()
        {
            account_pubkeys
                .iter()
                .filter_map(|pk_str| {
                    let pk = Pubkey::from_str(pk_str).ok()?;
                    self.token_accounts
                        .get(pk_str)
                        .ok()
                        .flatten()
                        .map(|ta| (pk, ta))
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn subscribe_for_slot_updates(&mut self) -> Receiver<SlotInfo> {
        let (tx, rx) = unbounded();
        self.slot_subscriptions.push(tx);
        rx
    }

    pub fn notify_slot_subscribers(&mut self, slot: Slot, parent: Slot, root: Slot) {
        self.slot_subscriptions
            .retain(|tx| tx.send(SlotInfo { slot, parent, root }).is_ok());
    }

    pub fn write_simulated_profile_result(
        &mut self,
        uuid: Uuid,
        tag: Option<String>,
        profile_result: KeyedProfileResult,
    ) -> SurfpoolResult<()> {
        self.simulated_transaction_profiles
            .store(uuid.to_string(), profile_result)?;

        let tag = tag.unwrap_or_else(|| uuid.to_string());
        let mut tags = self
            .profile_tag_map
            .get(&tag)
            .ok()
            .flatten()
            .unwrap_or_default();
        tags.push(UuidOrSignature::Uuid(uuid));
        self.profile_tag_map.store(tag, tags)?;
        Ok(())
    }

    pub fn write_executed_profile_result(
        &mut self,
        signature: Signature,
        profile_result: KeyedProfileResult,
    ) -> SurfpoolResult<()> {
        self.executed_transaction_profiles
            .store(signature.to_string(), profile_result)?;
        let tag = signature.to_string();
        let mut tags = self
            .profile_tag_map
            .get(&tag)
            .ok()
            .flatten()
            .unwrap_or_default();
        tags.push(UuidOrSignature::Signature(signature));
        self.profile_tag_map.store(tag, tags)?;
        Ok(())
    }

    pub fn subscribe_for_logs_updates(
        &mut self,
        commitment_level: &CommitmentLevel,
        filter: &RpcTransactionLogsFilter,
    ) -> Receiver<(Slot, RpcLogsResponse)> {
        let (tx, rx) = unbounded();
        self.logs_subscriptions
            .push((*commitment_level, filter.clone(), tx));
        rx
    }

    pub fn notify_logs_subscribers(
        &mut self,
        signature: &Signature,
        err: Option<TransactionError>,
        logs: Vec<String>,
        commitment_level: CommitmentLevel,
    ) {
        for (expected_level, filter, tx) in self.logs_subscriptions.iter() {
            if !expected_level.eq(&commitment_level) {
                continue; // Skip if commitment level is not expected
            }

            let should_notify = match filter {
                RpcTransactionLogsFilter::All | RpcTransactionLogsFilter::AllWithVotes => true,

                RpcTransactionLogsFilter::Mentions(mentioned_accounts) => {
                    // Get the tx accounts including loaded addresses
                    let transaction_accounts =
                        if let Some(SurfnetTransactionStatus::Processed(tx_data)) =
                            self.transactions.get(&signature.to_string()).ok().flatten()
                        {
                            let (tx_meta, _) = tx_data.as_ref();
                            let mut accounts = match &tx_meta.transaction.message {
                                VersionedMessage::Legacy(msg) => msg.account_keys.clone(),
                                VersionedMessage::V0(msg) => msg.account_keys.clone(),
                            };

                            accounts.extend(&tx_meta.meta.loaded_addresses.writable);
                            accounts.extend(&tx_meta.meta.loaded_addresses.readonly);
                            Some(accounts)
                        } else {
                            None
                        };

                    let Some(accounts) = transaction_accounts else {
                        continue;
                    };

                    mentioned_accounts.iter().any(|filtered_acc| {
                        if let Ok(filtered_pubkey) = Pubkey::from_str(&filtered_acc) {
                            accounts.contains(&filtered_pubkey)
                        } else {
                            false
                        }
                    })
                }
            };

            if should_notify {
                let message = RpcLogsResponse {
                    signature: signature.to_string(),
                    err: err.clone().map(|e| e.into()),
                    logs: logs.clone(),
                };
                let _ = tx.send((self.get_latest_absolute_slot(), message));
            }
        }
    }

    /// Registers a snapshot subscription and returns a sender and receiver for notifications.
    /// The actual import logic should be handled by the caller (SurfnetSvmLocker).
    pub fn register_snapshot_subscription(
        &mut self,
    ) -> (
        Sender<super::SnapshotImportNotification>,
        Receiver<super::SnapshotImportNotification>,
    ) {
        let (tx, rx) = unbounded();
        self.snapshot_subscriptions.push(tx.clone());
        (tx, rx)
    }

    pub async fn fetch_snapshot_from_url(
        snapshot_url: &str,
    ) -> Result<
        std::collections::BTreeMap<String, Option<surfpool_types::AccountSnapshot>>,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let response = reqwest::get(snapshot_url).await?;
        let text = response.text().await?;

        // Parse the JSON snapshot data
        let snapshot: std::collections::BTreeMap<String, Option<surfpool_types::AccountSnapshot>> =
            serde_json::from_str(&text)?;

        Ok(snapshot)
    }

    pub fn register_idl(&mut self, idl: Idl, slot: Option<Slot>) -> SurfpoolResult<()> {
        let slot = slot.unwrap_or(self.latest_epoch_info.absolute_slot);
        let program_id = Pubkey::from_str_const(&idl.address);
        let program_id_str = program_id.to_string();
        let mut idl_versions = self
            .registered_idls
            .get(&program_id_str)
            .ok()
            .flatten()
            .unwrap_or_default();
        idl_versions.push(VersionedIdl(slot, idl));
        // Sort by slot descending so the latest IDL is first
        idl_versions.sort_by(|a, b| b.0.cmp(&a.0));
        self.registered_idls.store(program_id_str, idl_versions)?;
        Ok(())
    }

    fn encode_ui_account_profile_state(
        &self,
        pubkey: &Pubkey,
        account_profile_state: AccountProfileState,
        encoding: &UiAccountEncoding,
    ) -> UiAccountProfileState {
        let additional_data = self.get_additional_data(pubkey, None);

        match account_profile_state {
            AccountProfileState::Readonly => UiAccountProfileState::Readonly,
            AccountProfileState::Writable(account_change) => {
                let change = match account_change {
                    AccountChange::Create(account) => UiAccountChange::Create(
                        self.encode_ui_account(pubkey, &account, *encoding, additional_data, None),
                    ),
                    AccountChange::Update(account_before, account_after) => {
                        UiAccountChange::Update(
                            self.encode_ui_account(
                                pubkey,
                                &account_before,
                                *encoding,
                                additional_data,
                                None,
                            ),
                            self.encode_ui_account(
                                pubkey,
                                &account_after,
                                *encoding,
                                additional_data,
                                None,
                            ),
                        )
                    }
                    AccountChange::Delete(account) => UiAccountChange::Delete(
                        self.encode_ui_account(pubkey, &account, *encoding, additional_data, None),
                    ),
                    AccountChange::Unchanged(account) => {
                        UiAccountChange::Unchanged(account.map(|account| {
                            self.encode_ui_account(
                                pubkey,
                                &account,
                                *encoding,
                                additional_data,
                                None,
                            )
                        }))
                    }
                };
                UiAccountProfileState::Writable(change)
            }
        }
    }

    fn encode_ui_profile_result(
        &self,
        profile_result: ProfileResult,
        readonly_accounts: &[Pubkey],
        encoding: &UiAccountEncoding,
    ) -> UiProfileResult {
        let ProfileResult {
            pre_execution_capture,
            post_execution_capture,
            compute_units_consumed,
            log_messages,
            error_message,
        } = profile_result;

        let account_states = pre_execution_capture
            .into_iter()
            .zip(post_execution_capture)
            .map(|((pubkey, pre_account), (_, post_account))| {
                // if pubkey != post {
                //     panic!(
                //         "Pre-execution pubkey {} does not match post-execution pubkey {}",
                //         pubkey, post
                //     );
                // }
                let state =
                    AccountProfileState::new(pubkey, pre_account, post_account, readonly_accounts);
                (
                    pubkey,
                    self.encode_ui_account_profile_state(&pubkey, state, encoding),
                )
            })
            .collect::<IndexMap<Pubkey, UiAccountProfileState>>();

        UiProfileResult {
            account_states,
            compute_units_consumed,
            log_messages,
            error_message,
        }
    }

    pub fn encode_ui_keyed_profile_result(
        &self,
        keyed_profile_result: KeyedProfileResult,
        config: &RpcProfileResultConfig,
    ) -> UiKeyedProfileResult {
        let KeyedProfileResult {
            slot,
            key,
            instruction_profiles,
            transaction_profile,
            readonly_account_states,
        } = keyed_profile_result;

        let encoding = config.encoding.unwrap_or(UiAccountEncoding::JsonParsed);

        let readonly_accounts = readonly_account_states.keys().cloned().collect::<Vec<_>>();

        let default = RpcProfileDepth::default();
        let instruction_profiles = match *config.depth.as_ref().unwrap_or(&default) {
            RpcProfileDepth::Transaction => None,
            RpcProfileDepth::Instruction => instruction_profiles.map(|instruction_profiles| {
                instruction_profiles
                    .into_iter()
                    .map(|p| self.encode_ui_profile_result(p, &readonly_accounts, &encoding))
                    .collect()
            }),
        };

        let transaction_profile =
            self.encode_ui_profile_result(transaction_profile, &readonly_accounts, &encoding);

        let readonly_account_states = readonly_account_states
            .into_iter()
            .map(|(pubkey, account)| {
                let account = self.encode_ui_account(&pubkey, &account, encoding, None, None);
                (pubkey, account)
            })
            .collect();

        UiKeyedProfileResult {
            slot,
            key,
            instruction_profiles,
            transaction_profile,
            readonly_account_states,
        }
    }

    pub fn encode_ui_account<T: ReadableAccount>(
        &self,
        pubkey: &Pubkey,
        account: &T,
        encoding: UiAccountEncoding,
        additional_data: Option<AccountAdditionalDataV3>,
        data_slice_config: Option<UiDataSliceConfig>,
    ) -> UiAccount {
        let owner_program_id = account.owner();
        let data = account.data();

        if encoding == UiAccountEncoding::JsonParsed {
            if let Ok(Some(registered_idls)) =
                self.registered_idls.get(&owner_program_id.to_string())
            {
                let filter_slot = self.latest_epoch_info.absolute_slot;
                // IDLs are stored sorted by slot descending (most recent first)
                let ordered_available_idls = registered_idls
                    .iter()
                    // only get IDLs that are active (their slot is before the latest slot)
                    .filter_map(|VersionedIdl(slot, idl)| {
                        if *slot <= filter_slot {
                            Some(idl)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();

                // if we have none in this loop, it means the only IDLs registered for this pubkey are for a
                // future slot, for some reason. if we have some, we'll try each one in this loop, starting
                // with the most recent one, to see if the account data can be parsed to the IDL type
                for idl in &ordered_available_idls {
                    // If we have a valid IDL, use it to parse the account data
                    let discriminator = &data[..8];
                    if let Some(matching_account) = idl
                        .accounts
                        .iter()
                        .find(|a| a.discriminator.eq(&discriminator))
                    {
                        // If we found a matching account, we can look up the type to parse the account
                        if let Some(account_type) =
                            idl.types.iter().find(|t| t.name == matching_account.name)
                        {
                            let empty_vec = vec![];
                            let idl_type_def_generics = idl
                                .types
                                .iter()
                                .find(|t| t.name == account_type.name)
                                .map(|t| &t.generics);

                            // If we found a matching account type, we can use it to parse the account data
                            let rest = data[8..].as_ref();
                            if let Ok(parsed_value) =
                                parse_bytes_to_value_with_expected_idl_type_def_ty(
                                    rest,
                                    &account_type.ty,
                                    &idl.types,
                                    &vec![],
                                    idl_type_def_generics.unwrap_or(&empty_vec),
                                )
                            {
                                return UiAccount {
                                    lamports: account.lamports(),
                                    data: UiAccountData::Json(ParsedAccount {
                                        program: idl
                                            .metadata
                                            .name
                                            .to_string()
                                            .to_case(convert_case::Case::Kebab),
                                        parsed: parsed_value
                                            .to_json(Some(&get_txtx_value_json_converters())),
                                        space: data.len() as u64,
                                    }),
                                    owner: owner_program_id.to_string(),
                                    executable: account.executable(),
                                    rent_epoch: account.rent_epoch(),
                                    space: Some(data.len() as u64),
                                };
                            }
                        }
                    }
                }
            }
        }

        // Fall back to the default encoding
        encode_ui_account(
            pubkey,
            account,
            encoding,
            additional_data,
            data_slice_config,
        )
    }

    pub fn get_account(&self, pubkey: &Pubkey) -> SurfpoolResult<Option<Account>> {
        self.inner.get_account(pubkey)
    }

    pub fn get_all_accounts(&self) -> SurfpoolResult<Vec<(Pubkey, AccountSharedData)>> {
        self.inner.get_all_accounts()
    }

    pub fn get_transaction(
        &self,
        signature: &Signature,
    ) -> SurfpoolResult<Option<SurfnetTransactionStatus>> {
        Ok(self.transactions.get(&signature.to_string())?)
    }

    pub fn start_runbook_execution(&mut self, runbook_id: String) {
        self.runbook_executions
            .push(RunbookExecutionStatusReport::new(runbook_id));
    }

    pub fn complete_runbook_execution(&mut self, runbook_id: &str, error: Option<Vec<String>>) {
        if let Some(execution) = self
            .runbook_executions
            .iter_mut()
            .find(|e| e.runbook_id.eq(runbook_id) && e.completed_at.is_none())
        {
            execution.mark_completed(error);
        }
    }

    /// Export all accounts to a JSON file suitable for test fixtures
    ///
    /// # Arguments
    /// * `encoding` - The encoding to use for account data (Base64, JsonParsed, etc.)
    ///
    /// # Returns
    /// A BTreeMap of pubkey -> AccountFixture that can be serialized to JSON.
    pub fn export_snapshot(
        &self,
        config: ExportSnapshotConfig,
    ) -> SurfpoolResult<BTreeMap<String, AccountSnapshot>> {
        let mut fixtures = BTreeMap::new();
        let encoding = if config.include_parsed_accounts.unwrap_or_default() {
            UiAccountEncoding::JsonParsed
        } else {
            UiAccountEncoding::Base64
        };
        let filter = config.filter.unwrap_or_default();
        let include_program_accounts = filter.include_program_accounts.unwrap_or(false);
        let include_accounts = filter.include_accounts.unwrap_or_default();
        let exclude_accounts = filter.exclude_accounts.unwrap_or_default();

        fn is_program_account(pubkey: &Pubkey) -> bool {
            pubkey == &bpf_loader::id()
                || pubkey == &solana_sdk_ids::bpf_loader_deprecated::id()
                || pubkey == &solana_sdk_ids::bpf_loader_upgradeable::id()
        }

        // Helper function to process an account and add it to fixtures
        let mut process_account = |pubkey: &Pubkey, account: &Account| {
            let is_include_account = include_accounts.iter().any(|k| k.eq(&pubkey.to_string()));
            let is_exclude_account = exclude_accounts.iter().any(|k| k.eq(&pubkey.to_string()));
            let is_program_account = is_program_account(&account.owner);
            if is_exclude_account
                || ((is_program_account && !include_program_accounts) && !is_include_account)
            {
                return;
            }

            // For token accounts, we need to provide the mint additional data
            let additional_data: Option<AccountAdditionalDataV3> = if account.owner
                == spl_token_interface::id()
                || account.owner == spl_token_2022_interface::id()
            {
                if let Ok(token_account) = TokenAccount::unpack(&account.data) {
                    self.account_associated_data
                        .get(&token_account.mint().to_string())
                        .ok()
                        .flatten()
                        .and_then(|data| data.try_into().ok())
                } else {
                    self.account_associated_data
                        .get(&pubkey.to_string())
                        .ok()
                        .flatten()
                        .and_then(|data| data.try_into().ok())
                }
            } else {
                self.account_associated_data
                    .get(&pubkey.to_string())
                    .ok()
                    .flatten()
                    .and_then(|data| data.try_into().ok())
            };

            let ui_account =
                self.encode_ui_account(pubkey, account, encoding, additional_data, None);

            let (base64, parsed_data) = match ui_account.data {
                UiAccountData::Json(parsed_account) => {
                    (BASE64_STANDARD.encode(account.data()), Some(parsed_account))
                }
                UiAccountData::Binary(base64, _) => (base64, None),
                UiAccountData::LegacyBinary(_) => unreachable!(),
            };

            let account_snapshot = AccountSnapshot::new(
                account.lamports,
                account.owner.to_string(),
                account.executable,
                account.rent_epoch,
                base64,
                parsed_data,
            );

            fixtures.insert(pubkey.to_string(), account_snapshot);
        };

        match &config.scope {
            ExportSnapshotScope::Network => {
                // Export all network accounts (current behavior)
                for (pubkey, account_shared_data) in self.get_all_accounts()? {
                    let account = Account::from(account_shared_data.clone());
                    process_account(&pubkey, &account);
                }
            }
            ExportSnapshotScope::PreTransaction(signature_str) => {
                // Export accounts from a specific transaction's pre-execution state
                if let Ok(signature) = Signature::from_str(signature_str) {
                    if let Ok(Some(profile)) = self
                        .executed_transaction_profiles
                        .get(&signature.to_string())
                    {
                        // Collect accounts from pre-execution capture only
                        // This gives us the account state BEFORE the transaction executed
                        for (pubkey, account_opt) in
                            &profile.transaction_profile.pre_execution_capture
                        {
                            if let Some(account) = account_opt {
                                process_account(pubkey, account);
                            }
                        }

                        // Also collect readonly account states (these don't change)
                        for (pubkey, account) in &profile.readonly_account_states {
                            process_account(pubkey, account);
                        }
                    }
                }
            }
        }

        Ok(fixtures)
    }

    /// Registers a scenario for execution by scheduling its overrides
    ///
    /// The `slot` parameter is the base slot from which relative override slot heights are calculated.
    /// If not provided, uses the current slot.
    pub fn register_scenario(
        &mut self,
        scenario: surfpool_types::Scenario,
        slot: Option<Slot>,
    ) -> SurfpoolResult<()> {
        // Use provided slot or current slot as the base for relative slot heights
        let base_slot = slot.unwrap_or(self.latest_epoch_info.absolute_slot);

        info!(
            "Registering scenario: {} ({}) with {} overrides at base slot {}",
            scenario.name,
            scenario.id,
            scenario.overrides.len(),
            base_slot
        );

        // Schedule overrides by adding base slot to their scenario-relative slots
        for override_instance in scenario.overrides {
            let scenario_relative_slot = override_instance.scenario_relative_slot;
            let absolute_slot = base_slot + scenario_relative_slot;

            debug!(
                "Scheduling override at absolute slot {} (base {} + relative {})",
                absolute_slot, base_slot, scenario_relative_slot
            );

            let mut slot_overrides = self
                .scheduled_overrides
                .get(&absolute_slot)
                .ok()
                .flatten()
                .unwrap_or_default();
            slot_overrides.push(override_instance);
            self.scheduled_overrides
                .store(absolute_slot, slot_overrides)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use agave_feature_set::{
        blake3_syscall_enabled, curve25519_syscall_enabled, disable_fees_sysvar,
        enable_extend_program_checked, enable_loader_v4, enable_sbpf_v1_deployment_and_execution,
        enable_sbpf_v2_deployment_and_execution, enable_sbpf_v3_deployment_and_execution,
        formalize_loaded_transaction_data_size, move_precompile_verification_to_svm,
        raise_cpi_nesting_limit_to_8,
    };
    use base64::{Engine, engine::general_purpose};
    use borsh::BorshSerialize;
    // use test_log::test; // uncomment to get logs from litesvm
    use solana_account::Account;
    use solana_loader_v3_interface::get_program_data_address;
    use solana_program_pack::Pack;
    use spl_token_interface::state::{Account as TokenAccount, AccountState};
    use test_case::test_case;

    use super::*;
    use crate::storage::tests::TestType;

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_synthetic_blockhash_generation(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Test with different chain tip indices
        let test_cases = vec![0, 1, 42, 255, 1000, 0x12345678];

        for index in test_cases {
            svm.chain_tip = BlockIdentifier::new(index, "test_hash");

            // Generate the synthetic blockhash
            let new_blockhash = svm.new_blockhash();

            // Verify the blockhash string contains our expected pattern
            let blockhash_str = new_blockhash.hash.clone();
            println!("Index {} -> Blockhash: {}", index, blockhash_str);

            // The blockhash should be a valid base58 string
            assert!(!blockhash_str.is_empty());
            assert!(blockhash_str.len() > 20); // Base58 encoded 32 bytes should be around 44 chars

            // Verify it's deterministic - same index should produce same blockhash
            svm.chain_tip = BlockIdentifier::new(index, "test_hash");
            let new_blockhash2 = svm.new_blockhash();
            assert_eq!(new_blockhash.hash, new_blockhash2.hash);
        }
    }

    #[test]
    fn test_synthetic_blockhash_base58_encoding() {
        // Test the base58 encoding logic directly
        let test_index = 42u64;
        let index_hex = format!("{:08x}", test_index)
            .replace('0', "x")
            .replace('O', "x");

        let target_length = 43;
        let padding_needed = target_length - SyntheticBlockhash::PREFIX.len() - index_hex.len();
        let padding = "x".repeat(padding_needed.max(0));
        let target_string = format!("{}{}{}", SyntheticBlockhash::PREFIX, padding, index_hex);

        println!("Target string: {}", target_string);

        // Verify the string is valid base58
        let decoded_bytes = bs58::decode(&target_string).into_vec();
        assert!(decoded_bytes.is_ok(), "String should be valid base58");

        let bytes = decoded_bytes.unwrap();
        assert!(bytes.len() <= 32, "Decoded bytes should fit in 32 bytes");

        // Test that we can create a hash from these bytes
        let mut blockhash_bytes = [0u8; 32];
        blockhash_bytes[..bytes.len().min(32)].copy_from_slice(&bytes[..bytes.len().min(32)]);
        let hash = Hash::new_from_array(blockhash_bytes);

        // Verify the hash can be converted back to string
        let hash_str = hash.to_string();
        assert!(!hash_str.is_empty());
        println!("Generated hash: {}", hash_str);
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_blockhash_consistency_across_calls(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Set a specific chain tip
        svm.chain_tip = BlockIdentifier::new(123, "initial_hash");

        // Generate multiple blockhashes and verify they're consistent
        let mut previous_hash: Option<BlockIdentifier> = None;
        for i in 0..5 {
            let new_blockhash = svm.new_blockhash();
            println!(
                "Call {}: index={}, hash={}",
                i, new_blockhash.index, new_blockhash.hash
            );

            if let Some(prev) = previous_hash {
                // Each call should increment the index
                assert_eq!(new_blockhash.index, prev.index + 1);
                // But the hash should be different (since index changed)
                assert_ne!(new_blockhash.hash, prev.hash);
            } else {
                // First call should increment from the initial chain tip
                assert_eq!(new_blockhash.index, svm.chain_tip.index + 1);
            }

            previous_hash = Some(new_blockhash.clone());
            // Update the chain tip for the next iteration
            svm.chain_tip = new_blockhash;
        }
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_token_account_indexing(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        let owner = Pubkey::new_unique();
        let delegate = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let token_account_pubkey = Pubkey::new_unique();

        // create a token account with delegate
        let mut token_account_data = [0u8; TokenAccount::LEN];
        let token_account = TokenAccount {
            mint,
            owner,
            amount: 1000,
            delegate: COption::Some(delegate),
            state: AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 500,
            close_authority: COption::None,
        };
        token_account.pack_into_slice(&mut token_account_data);

        let account = Account {
            lamports: 1000000,
            data: token_account_data.to_vec(),
            owner: spl_token_interface::id(),
            executable: false,
            rent_epoch: 0,
        };

        svm.set_account(&token_account_pubkey, account).unwrap();

        // test all indexes were created correctly
        assert_eq!(svm.token_accounts.keys().unwrap().len(), 1);

        // test owner index
        let owner_accounts = svm.get_parsed_token_accounts_by_owner(&owner);
        assert_eq!(owner_accounts.len(), 1);
        assert_eq!(owner_accounts[0].0, token_account_pubkey);

        // test delegate index
        let delegate_accounts = svm.get_token_accounts_by_delegate(&delegate);
        assert_eq!(delegate_accounts.len(), 1);
        assert_eq!(delegate_accounts[0].0, token_account_pubkey);

        // test mint index
        let mint_accounts = svm.get_token_accounts_by_mint(&mint);
        assert_eq!(mint_accounts.len(), 1);
        assert_eq!(mint_accounts[0].0, token_account_pubkey);
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_account_update_removes_old_indexes(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        let owner = Pubkey::new_unique();
        let old_delegate = Pubkey::new_unique();
        let new_delegate = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let token_account_pubkey = Pubkey::new_unique();

        //  reate initial token account with old delegate
        let mut token_account_data = [0u8; TokenAccount::LEN];
        let token_account = TokenAccount {
            mint,
            owner,
            amount: 1000,
            delegate: COption::Some(old_delegate),
            state: AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 500,
            close_authority: COption::None,
        };
        token_account.pack_into_slice(&mut token_account_data);

        let account = Account {
            lamports: 1000000,
            data: token_account_data.to_vec(),
            owner: spl_token_interface::id(),
            executable: false,
            rent_epoch: 0,
        };

        // insert initial account
        svm.set_account(&token_account_pubkey, account).unwrap();

        // verify old delegate has the account
        assert_eq!(svm.get_token_accounts_by_delegate(&old_delegate).len(), 1);
        assert_eq!(svm.get_token_accounts_by_delegate(&new_delegate).len(), 0);

        // update with new delegate
        let updated_token_account = TokenAccount {
            mint,
            owner,
            amount: 1000,
            delegate: COption::Some(new_delegate),
            state: AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 500,
            close_authority: COption::None,
        };
        updated_token_account.pack_into_slice(&mut token_account_data);

        let updated_account = Account {
            lamports: 1000000,
            data: token_account_data.to_vec(),
            owner: spl_token_interface::id(),
            executable: false,
            rent_epoch: 0,
        };

        // update the account
        svm.set_account(&token_account_pubkey, updated_account)
            .unwrap();

        // verify indexes were updated correctly
        assert_eq!(svm.get_token_accounts_by_delegate(&old_delegate).len(), 0);
        assert_eq!(svm.get_token_accounts_by_delegate(&new_delegate).len(), 1);
        assert_eq!(svm.get_parsed_token_accounts_by_owner(&owner).len(), 1);
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_non_token_accounts_not_indexed(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        let system_account_pubkey = Pubkey::new_unique();
        let account = Account {
            lamports: 1000000,
            data: vec![],
            owner: solana_system_interface::program::id(), // system program, not token program
            executable: false,
            rent_epoch: 0,
        };

        svm.set_account(&system_account_pubkey, account).unwrap();

        // should be in general registry but not token indexes
        assert_eq!(svm.token_accounts.keys().unwrap().len(), 0);
        assert_eq!(svm.token_accounts_by_owner.keys().unwrap().len(), 0);
        assert_eq!(svm.token_accounts_by_delegate.keys().unwrap().len(), 0);
        assert_eq!(svm.token_accounts_by_mint.keys().unwrap().len(), 0);
    }

    fn expect_account_update_event(
        events_rx: &Receiver<SimnetEvent>,
        svm: &SurfnetSvm,
        pubkey: &Pubkey,
        expected_account: &Account,
    ) -> bool {
        match events_rx.recv() {
            Ok(event) => match event {
                SimnetEvent::AccountUpdate(_, account_pubkey) => {
                    assert_eq!(pubkey, &account_pubkey);
                    assert_eq!(
                        svm.get_account(&pubkey).unwrap().as_ref(),
                        Some(expected_account)
                    );
                    true
                }
                event => {
                    println!("unexpected simnet event: {:?}", event);
                    false
                }
            },
            Err(_) => false,
        }
    }

    fn _expect_error_event(events_rx: &Receiver<SimnetEvent>, expected_error: &str) -> bool {
        match events_rx.recv() {
            Ok(event) => match event {
                SimnetEvent::ErrorLog(_, err) => {
                    assert_eq!(err, expected_error);

                    true
                }
                event => {
                    println!("unexpected simnet event: {:?}", event);
                    false
                }
            },
            Err(_) => false,
        }
    }

    fn create_program_accounts() -> (Pubkey, Account, Pubkey, Account) {
        let program_pubkey = Pubkey::new_unique();
        let program_data_address = get_program_data_address(&program_pubkey);
        let program_account = Account {
            lamports: 1000000000000,
            data: bincode::serialize(
                &solana_loader_v3_interface::state::UpgradeableLoaderState::Program {
                    programdata_address: program_data_address,
                },
            )
            .unwrap(),
            owner: solana_sdk_ids::bpf_loader_upgradeable::ID,
            executable: true,
            rent_epoch: 10000000000000,
        };

        let mut bin = include_bytes!("../tests/assets/metaplex_program.bin").to_vec();
        let mut data = bincode::serialize(
            &solana_loader_v3_interface::state::UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address: Some(Pubkey::new_unique()),
            },
        )
        .unwrap();
        data.append(&mut bin); // push our binary after the state data
        let program_data_account = Account {
            lamports: 10000000000000,
            data,
            owner: solana_sdk_ids::bpf_loader_upgradeable::ID,
            executable: false,
            rent_epoch: 10000000000000,
        };
        (
            program_pubkey,
            program_account,
            program_data_address,
            program_data_account,
        )
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_inserting_account_updates(test_type: TestType) {
        let (mut svm, events_rx, _geyser_rx) = test_type.initialize_svm();

        let pubkey = Pubkey::new_unique();
        let account = Account {
            lamports: 1000,
            data: vec![1, 2, 3],
            owner: Pubkey::new_unique(),
            executable: false,
            rent_epoch: 0,
        };

        // GetAccountResult::None should be a noop when writing account updates
        {
            let index_before = svm.get_all_accounts().unwrap();
            let empty_update = GetAccountResult::None(pubkey);
            svm.write_account_update(empty_update);
            assert_eq!(svm.get_all_accounts().unwrap(), index_before);
        }

        // GetAccountResult::FoundAccount with `DoUpdateSvm` flag to false should be a noop
        {
            let index_before = svm.get_all_accounts().unwrap();
            let found_update = GetAccountResult::FoundAccount(pubkey, account.clone(), false);
            svm.write_account_update(found_update);
            assert_eq!(svm.get_all_accounts().unwrap(), index_before);
        }

        // GetAccountResult::FoundAccount with `DoUpdateSvm` flag to true should update the account
        {
            let index_before = svm.get_all_accounts().unwrap();
            let found_update = GetAccountResult::FoundAccount(pubkey, account.clone(), true);
            svm.write_account_update(found_update);
            assert_eq!(
                svm.get_all_accounts().unwrap().len(),
                index_before.len() + 1
            );
            if !expect_account_update_event(&events_rx, &svm, &pubkey, &account) {
                panic!(
                    "Expected account update event not received after GetAccountResult::FoundAccount update"
                );
            }
        }

        // GetAccountResult::FoundProgramAccount with no program account inserts a default programdata account
        {
            let (program_address, program_account, program_data_address, _) =
                create_program_accounts();

            let mut data = bincode::serialize(
                &solana_loader_v3_interface::state::UpgradeableLoaderState::ProgramData {
                    slot: svm.get_latest_absolute_slot(),
                    upgrade_authority_address: Some(system_program::id()),
                },
            )
            .unwrap();

            let mut bin = crate::surfnet::noop_program::NOOP_PROGRAM_ELF.to_vec();
            data.append(&mut bin); // push our binary after the state data
            let lamports = svm.inner.minimum_balance_for_rent_exemption(data.len());
            let default_program_data_account = Account {
                lamports,
                data,
                owner: solana_sdk_ids::bpf_loader_upgradeable::ID,
                executable: false,
                rent_epoch: 0,
            };

            let index_before = svm.get_all_accounts().unwrap();
            let found_program_account_update = GetAccountResult::FoundProgramAccount(
                (program_address, program_account.clone()),
                (program_data_address, None),
            );
            svm.write_account_update(found_program_account_update);

            if !expect_account_update_event(
                &events_rx,
                &svm,
                &program_data_address,
                &default_program_data_account,
            ) {
                panic!(
                    "Expected account update event not received after inserting default program data account"
                );
            }

            if !expect_account_update_event(&events_rx, &svm, &program_address, &program_account) {
                panic!(
                    "Expected account update event not received after GetAccountResult::FoundProgramAccount update for program pubkey"
                );
            }
            assert_eq!(
                svm.get_all_accounts().unwrap().len(),
                index_before.len() + 2
            );
        }

        // GetAccountResult::FoundProgramAccount with program account + program data account inserts two accounts
        {
            let (program_address, program_account, program_data_address, program_data_account) =
                create_program_accounts();

            let index_before = svm.get_all_accounts().unwrap();
            let found_program_account_update = GetAccountResult::FoundProgramAccount(
                (program_address, program_account.clone()),
                (program_data_address, Some(program_data_account.clone())),
            );
            svm.write_account_update(found_program_account_update);
            assert_eq!(
                svm.get_all_accounts().unwrap().len(),
                index_before.len() + 2
            );
            if !expect_account_update_event(
                &events_rx,
                &svm,
                &program_data_address,
                &program_data_account,
            ) {
                panic!(
                    "Expected account update event not received after GetAccountResult::FoundProgramAccount update for program data pubkey"
                );
            }

            if !expect_account_update_event(&events_rx, &svm, &program_address, &program_account) {
                panic!(
                    "Expected account update event not received after GetAccountResult::FoundProgramAccount update for program pubkey"
                );
            }
        }

        // If we insert the program data account ahead of time, then have a GetAccountResult::FoundProgramAccount with just the program data account,
        // we should get one insert
        {
            let (program_address, program_account, program_data_address, program_data_account) =
                create_program_accounts();

            let index_before = svm.get_all_accounts().unwrap();
            let found_update = GetAccountResult::FoundAccount(
                program_data_address,
                program_data_account.clone(),
                true,
            );
            svm.write_account_update(found_update);
            assert_eq!(
                svm.get_all_accounts().unwrap().len(),
                index_before.len() + 1
            );
            if !expect_account_update_event(
                &events_rx,
                &svm,
                &program_data_address,
                &program_data_account,
            ) {
                panic!(
                    "Expected account update event not received after GetAccountResult::FoundAccount update"
                );
            }

            let index_before = svm.get_all_accounts().unwrap();
            let program_account_found_update = GetAccountResult::FoundProgramAccount(
                (program_address, program_account.clone()),
                (program_data_address, None),
            );
            svm.write_account_update(program_account_found_update);
            assert_eq!(
                svm.get_all_accounts().unwrap().len(),
                index_before.len() + 1
            );
            if !expect_account_update_event(&events_rx, &svm, &program_address, &program_account) {
                panic!(
                    "Expected account update event not received after GetAccountResult::FoundAccount update"
                );
            }
        }
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_encode_ui_account(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        let idl_v1: Idl =
            serde_json::from_slice(&include_bytes!("../tests/assets/idl_v1.json").to_vec())
                .unwrap();

        svm.register_idl(idl_v1.clone(), Some(0)).unwrap();

        let account_pubkey = Pubkey::new_unique();

        #[derive(borsh::BorshSerialize)]
        pub struct CustomAccount {
            pub my_custom_data: u64,
            pub another_field: String,
            pub bool: bool,
            pub pubkey: Pubkey,
        }

        // Account data not matching IDL schema should use default encoding
        {
            let account_data = vec![0; 100];
            let base64_data = general_purpose::STANDARD.encode(&account_data);
            let expected_data = UiAccountData::Binary(base64_data, UiAccountEncoding::Base64);
            let account = Account {
                lamports: 1000,
                data: account_data,
                owner: idl_v1.address.parse().unwrap(),
                executable: false,
                rent_epoch: 0,
            };

            let ui_account = svm.encode_ui_account(
                &account_pubkey,
                &account,
                UiAccountEncoding::JsonParsed,
                None,
                None,
            );
            let expected_account = UiAccount {
                lamports: 1000,
                data: expected_data,
                owner: idl_v1.address.clone(),
                executable: false,
                rent_epoch: 0,
                space: Some(account.data.len() as u64),
            };
            assert_eq!(ui_account, expected_account);
        }

        // valid account data matching IDL schema should be parsed
        {
            let mut account_data = idl_v1.accounts[0].discriminator.clone();
            let pubkey = Pubkey::new_unique();
            CustomAccount {
                my_custom_data: 42,
                another_field: "test".to_string(),
                bool: true,
                pubkey,
            }
            .serialize(&mut account_data)
            .unwrap();

            let account = Account {
                lamports: 1000,
                data: account_data,
                owner: idl_v1.address.parse().unwrap(),
                executable: false,
                rent_epoch: 0,
            };

            let ui_account = svm.encode_ui_account(
                &account_pubkey,
                &account,
                UiAccountEncoding::JsonParsed,
                None,
                None,
            );
            let expected_account = UiAccount {
                lamports: 1000,
                data: UiAccountData::Json(ParsedAccount {
                    program: format!("{}", idl_v1.metadata.name).to_case(convert_case::Case::Kebab),
                    parsed: serde_json::json!({
                        "my_custom_data": 42,
                        "another_field": "test",
                        "bool": true,
                        "pubkey": pubkey.to_string(),
                    }),
                    space: account.data.len() as u64,
                }),
                owner: idl_v1.address.clone(),
                executable: false,
                rent_epoch: 0,
                space: Some(account.data.len() as u64),
            };
            assert_eq!(ui_account, expected_account);
        }

        let idl_v2: Idl =
            serde_json::from_slice(&include_bytes!("../tests/assets/idl_v2.json").to_vec())
                .unwrap();

        svm.register_idl(idl_v2.clone(), Some(100)).unwrap();

        // even though we have a new IDL that is more recent, we should be able to match with the old IDL
        {
            let mut account_data = idl_v1.accounts[0].discriminator.clone();
            let pubkey = Pubkey::new_unique();
            CustomAccount {
                my_custom_data: 42,
                another_field: "test".to_string(),
                bool: true,
                pubkey,
            }
            .serialize(&mut account_data)
            .unwrap();

            let account = Account {
                lamports: 1000,
                data: account_data,
                owner: idl_v1.address.parse().unwrap(),
                executable: false,
                rent_epoch: 0,
            };

            let ui_account = svm.encode_ui_account(
                &account_pubkey,
                &account,
                UiAccountEncoding::JsonParsed,
                None,
                None,
            );
            let expected_account = UiAccount {
                lamports: 1000,
                data: UiAccountData::Json(ParsedAccount {
                    program: format!("{}", idl_v1.metadata.name).to_case(convert_case::Case::Kebab),
                    parsed: serde_json::json!({
                        "my_custom_data": 42,
                        "another_field": "test",
                        "bool": true,
                        "pubkey": pubkey.to_string(),
                    }),
                    space: account.data.len() as u64,
                }),
                owner: idl_v1.address.clone(),
                executable: false,
                rent_epoch: 0,
                space: Some(account.data.len() as u64),
            };
            assert_eq!(ui_account, expected_account);
        }

        // valid account data matching IDL v2 schema should be parsed, if svm slot reaches IDL registration slot
        {
            // use the v2 shape of the custom account
            #[derive(borsh::BorshSerialize)]
            pub struct CustomAccount {
                pub my_custom_data: u64,
                pub another_field: String,
                pub pubkey: Pubkey,
            }
            let mut account_data = idl_v1.accounts[0].discriminator.clone();
            let pubkey = Pubkey::new_unique();
            CustomAccount {
                my_custom_data: 42,
                another_field: "test".to_string(),
                pubkey,
            }
            .serialize(&mut account_data)
            .unwrap();

            let account = Account {
                lamports: 1000,
                data: account_data.clone(),
                owner: idl_v1.address.parse().unwrap(),
                executable: false,
                rent_epoch: 0,
            };

            let ui_account = svm.encode_ui_account(
                &account_pubkey,
                &account,
                UiAccountEncoding::JsonParsed,
                None,
                None,
            );
            let base64_data = general_purpose::STANDARD.encode(&account_data);
            let expected_data = UiAccountData::Binary(base64_data, UiAccountEncoding::Base64);
            let expected_account = UiAccount {
                lamports: 1000,
                data: expected_data,
                owner: idl_v1.address.clone(),
                executable: false,
                rent_epoch: 0,
                space: Some(account.data.len() as u64),
            };
            assert_eq!(ui_account, expected_account);

            svm.latest_epoch_info.absolute_slot = 100; // simulate reaching the slot where IDL v2 was registered

            let ui_account = svm.encode_ui_account(
                &account_pubkey,
                &account,
                UiAccountEncoding::JsonParsed,
                None,
                None,
            );
            let expected_account = UiAccount {
                lamports: 1000,
                data: UiAccountData::Json(ParsedAccount {
                    program: format!("{}", idl_v1.metadata.name).to_case(convert_case::Case::Kebab),
                    parsed: serde_json::json!({
                        "my_custom_data": 42,
                        "another_field": "test",
                        "pubkey": pubkey.to_string(),
                    }),
                    space: account.data.len() as u64,
                }),
                owner: idl_v1.address.clone(),
                executable: false,
                rent_epoch: 0,
                space: Some(account.data.len() as u64),
            };
            assert_eq!(ui_account, expected_account);
        }
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_profiling_map_capacity_default(test_type: TestType) {
        let (svm, _events_rx, _geyser_rx) = test_type.initialize_svm();
        assert_eq!(svm.max_profiles, DEFAULT_PROFILING_MAP_CAPACITY);
    }

    #[test]
    fn test_default_uses_no_db_storage() {
        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        assert!(svm.inner.db.is_none());
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_new_with_db_uses_sqlite_storage() {
        let (svm, _events_rx, _geyser_rx) =
            SurfnetSvm::new_with_db(Some(":memory:"), SurfnetSvmConfig::default()).unwrap();
        assert!(svm.inner.db.is_some());
    }

    #[test]
    fn test_constructor_applies_startup_config() {
        let config = SurfnetSvmConfig {
            surfnet_id: "constructor-test".to_string(),
            feature_config: SvmFeatureConfig::new().disable(disable_fees_sysvar::id()),
            slot_time: 123,
            instruction_profiling_enabled: false,
            max_profiles: 17,
            log_bytes_limit: None,
        };
        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::new(config).unwrap();

        assert_eq!(svm.slot_time, 123);
        assert!(!svm.instruction_profiling_enabled);
        assert_eq!(svm.max_profiles, 17);
        assert_eq!(
            svm.latest_epoch_info.absolute_slot,
            FINALIZATION_SLOT_THRESHOLD
        );
        assert_eq!(svm.genesis_slot, FINALIZATION_SLOT_THRESHOLD);
        assert!(!svm.feature_set.is_active(&disable_fees_sysvar::id()));

        let epoch_schedule = svm.inner.get_sysvar::<EpochSchedule>();
        assert!(!epoch_schedule.warmup);

        let registry = TemplateRegistry::new();
        for (_, template) in registry.templates {
            let program_id = template.idl.address.clone();
            assert!(svm.registered_idls.get(&program_id).unwrap().is_some());
        }
    }

    #[test]
    fn test_initialize_only_updates_remote_state() {
        let config = SurfnetSvmConfig {
            surfnet_id: "remote-init-test".to_string(),
            feature_config: SvmFeatureConfig::new().disable(disable_fees_sysvar::id()),
            slot_time: 321,
            instruction_profiling_enabled: false,
            max_profiles: 23,
            log_bytes_limit: None,
        };
        let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::new(config).unwrap();
        let epoch_info = EpochInfo {
            epoch: 7,
            slot_index: 4,
            slots_in_epoch: crate::surfnet::SLOTS_PER_EPOCH,
            absolute_slot: 777,
            block_height: 777,
            transaction_count: None,
        };

        svm.initialize(epoch_info.clone(), EpochSchedule::without_warmup());

        assert_eq!(svm.slot_time, 321);
        assert!(!svm.instruction_profiling_enabled);
        assert_eq!(svm.max_profiles, 23);
        assert!(!svm.feature_set.is_active(&disable_fees_sysvar::id()));
        assert_eq!(svm.latest_epoch_info, epoch_info);
        assert_eq!(svm.genesis_slot, 777);
    }

    // Feature configuration tests

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_apply_feature_config_empty(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();
        let config = SvmFeatureConfig::new();

        // Should not panic with empty config
        svm.apply_feature_config(&config);
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_apply_feature_config_enable_feature(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Disable a feature first
        let feature_id = enable_loader_v4::id();
        svm.feature_set.deactivate(&feature_id);
        assert!(!svm.feature_set.is_active(&feature_id));

        // Now enable it via config
        let config = SvmFeatureConfig::new().enable(enable_loader_v4::id());
        svm.apply_feature_config(&config);

        assert!(svm.feature_set.is_active(&feature_id));
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_apply_feature_config_disable_feature(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Feature should be inactive by default (all_disabled)
        let feature_id = disable_fees_sysvar::id();
        assert!(!svm.feature_set.is_active(&feature_id));

        // Now applying feature config defaults to all enabled
        svm.apply_feature_config(&SvmFeatureConfig::new());
        assert!(svm.feature_set.is_active(&feature_id));

        // But we can explicitly disable via config
        let config = SvmFeatureConfig::new().disable(disable_fees_sysvar::id());
        svm.apply_feature_config(&config);
        assert!(!svm.feature_set.is_active(&feature_id));
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_apply_feature_config_mainnet_defaults(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();
        let config = SvmFeatureConfig::default_mainnet_features();

        svm.apply_feature_config(&config);

        // Features disabled on mainnet should now be inactive
        assert!(!svm.feature_set.is_active(&enable_loader_v4::id()));
        assert!(
            !svm.feature_set
                .is_active(&enable_extend_program_checked::id())
        );
        assert!(!svm.feature_set.is_active(&blake3_syscall_enabled::id()));
        assert!(
            !svm.feature_set
                .is_active(&enable_sbpf_v1_deployment_and_execution::id())
        );
        assert!(
            !svm.feature_set
                .is_active(&formalize_loaded_transaction_data_size::id())
        );
        assert!(
            !svm.feature_set
                .is_active(&move_precompile_verification_to_svm::id())
        );

        // Features active on mainnet should still be active
        assert!(svm.feature_set.is_active(&disable_fees_sysvar::id()));
        assert!(svm.feature_set.is_active(&curve25519_syscall_enabled::id()));
        assert!(
            svm.feature_set
                .is_active(&enable_sbpf_v2_deployment_and_execution::id())
        );
        assert!(
            svm.feature_set
                .is_active(&enable_sbpf_v3_deployment_and_execution::id())
        );
        assert!(
            svm.feature_set
                .is_active(&raise_cpi_nesting_limit_to_8::id())
        );
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_apply_feature_config_mainnet_with_override(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Start with mainnet defaults, but enable loader v4
        let config = SvmFeatureConfig::default_mainnet_features().enable(enable_loader_v4::id());

        svm.apply_feature_config(&config);

        // Loader v4 should be enabled despite mainnet defaults
        assert!(svm.feature_set.is_active(&enable_loader_v4::id()));

        // Other mainnet-disabled features should still be disabled
        assert!(!svm.feature_set.is_active(&blake3_syscall_enabled::id()));
        assert!(
            !svm.feature_set
                .is_active(&enable_extend_program_checked::id())
        );
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_apply_feature_config_multiple_changes(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        let config = SvmFeatureConfig::new()
            .enable(enable_loader_v4::id())
            .enable(enable_sbpf_v2_deployment_and_execution::id())
            .disable(disable_fees_sysvar::id())
            .disable(blake3_syscall_enabled::id());

        svm.apply_feature_config(&config);

        assert!(svm.feature_set.is_active(&enable_loader_v4::id()));
        assert!(
            svm.feature_set
                .is_active(&enable_sbpf_v2_deployment_and_execution::id())
        );
        assert!(!svm.feature_set.is_active(&disable_fees_sysvar::id()));
        assert!(!svm.feature_set.is_active(&blake3_syscall_enabled::id()));
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_apply_feature_config_preserves_native_mint(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Native mint should exist before
        assert!(
            svm.inner
                .get_account(&spl_token_interface::native_mint::ID)
                .unwrap()
                .is_some()
        );

        let config = SvmFeatureConfig::new().disable(disable_fees_sysvar::id());
        svm.apply_feature_config(&config);

        // Native mint should still exist after (re-added in apply_feature_config)
        assert!(
            svm.inner
                .get_account(&spl_token_interface::native_mint::ID)
                .unwrap()
                .is_some()
        );
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_apply_feature_config_idempotent(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        let config = SvmFeatureConfig::new()
            .enable(enable_loader_v4::id())
            .disable(disable_fees_sysvar::id());

        // Apply twice
        svm.apply_feature_config(&config);
        svm.apply_feature_config(&config);

        // State should be the same
        assert!(svm.feature_set.is_active(&enable_loader_v4::id()));
        assert!(!svm.feature_set.is_active(&disable_fees_sysvar::id()));
    }

    // Garbage collection tests

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_garbage_collected_account_tracking(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        let owner = Pubkey::new_unique();
        let account_pubkey = Pubkey::new_unique();

        let account = Account {
            lamports: 1000000,
            data: vec![1, 2, 3, 4, 5],
            owner,
            executable: false,
            rent_epoch: 0,
        };

        svm.set_account(&account_pubkey, account.clone()).unwrap();

        assert!(svm.get_account(&account_pubkey).unwrap().is_some());
        assert!(
            !svm.offline_accounts
                .contains_key(&account_pubkey.to_string())
                .unwrap()
        );
        assert_eq!(svm.get_account_owned_by(&owner).unwrap().len(), 1);

        let empty_account = Account::default();
        svm.update_account_registries(&account_pubkey, &empty_account)
            .unwrap();

        assert!(
            svm.offline_accounts
                .contains_key(&account_pubkey.to_string())
                .unwrap()
        );

        assert_eq!(svm.get_account_owned_by(&owner).unwrap().len(), 0);

        let owned_accounts = svm.get_account_owned_by(&owner).unwrap();
        assert!(!owned_accounts.iter().any(|(pk, _)| *pk == account_pubkey));
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_garbage_collected_token_account_cleanup(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        let token_owner = Pubkey::new_unique();
        let delegate = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let token_account_pubkey = Pubkey::new_unique();

        let mut token_account_data = [0u8; TokenAccount::LEN];
        let token_account = TokenAccount {
            mint,
            owner: token_owner,
            amount: 1000,
            delegate: COption::Some(delegate),
            state: AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 500,
            close_authority: COption::None,
        };
        token_account.pack_into_slice(&mut token_account_data);

        let account = Account {
            lamports: 2000000,
            data: token_account_data.to_vec(),
            owner: spl_token_interface::id(),
            executable: false,
            rent_epoch: 0,
        };

        svm.set_account(&token_account_pubkey, account).unwrap();

        assert_eq!(
            svm.get_token_accounts_by_owner(&token_owner).unwrap().len(),
            1
        );
        assert_eq!(svm.get_token_accounts_by_delegate(&delegate).len(), 1);
        assert!(
            !svm.offline_accounts
                .contains_key(&token_account_pubkey.to_string())
                .unwrap()
        );

        let empty_account = Account::default();
        svm.update_account_registries(&token_account_pubkey, &empty_account)
            .unwrap();

        assert!(
            svm.offline_accounts
                .contains_key(&token_account_pubkey.to_string())
                .unwrap()
        );

        assert_eq!(
            svm.get_token_accounts_by_owner(&token_owner).unwrap().len(),
            0
        );
        assert_eq!(svm.get_token_accounts_by_delegate(&delegate).len(), 0);
        assert!(
            svm.token_accounts
                .get(&token_account_pubkey.to_string())
                .unwrap()
                .is_none()
        );
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_is_slot_in_valid_range(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Set up: genesis_slot = 100, latest absolute slot = 110
        svm.genesis_slot = 100;
        svm.latest_epoch_info.absolute_slot = 110;

        // Test slots within valid range
        assert!(
            svm.is_slot_in_valid_range(100),
            "genesis_slot should be valid"
        );
        assert!(
            svm.is_slot_in_valid_range(105),
            "middle slot should be valid"
        );
        assert!(
            svm.is_slot_in_valid_range(110),
            "latest slot should be valid"
        );

        // Test slots outside valid range
        assert!(
            !svm.is_slot_in_valid_range(99),
            "slot before genesis should be invalid"
        );
        assert!(
            !svm.is_slot_in_valid_range(111),
            "slot after latest should be invalid"
        );
        assert!(
            !svm.is_slot_in_valid_range(0),
            "slot 0 should be invalid when genesis > 0"
        );
        assert!(
            !svm.is_slot_in_valid_range(1000),
            "far future slot should be invalid"
        );
    }

    #[test]
    fn test_is_slot_in_valid_range_genesis_zero() {
        let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();

        // Set up: genesis_slot = 0, latest absolute slot = 50
        svm.genesis_slot = 0;
        svm.latest_epoch_info.absolute_slot = 50;

        // Test boundary conditions with genesis at 0
        assert!(
            svm.is_slot_in_valid_range(0),
            "slot 0 should be valid when genesis = 0"
        );
        assert!(
            svm.is_slot_in_valid_range(25),
            "middle slot should be valid"
        );
        assert!(
            svm.is_slot_in_valid_range(50),
            "latest slot should be valid"
        );
        assert!(
            !svm.is_slot_in_valid_range(51),
            "slot after latest should be invalid"
        );
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_get_block_or_reconstruct_stored_block(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Set up: genesis_slot = 0, latest absolute slot = 100
        svm.genesis_slot = 0;
        svm.latest_epoch_info.absolute_slot = 100;

        // Store a block with transactions
        let stored_block = BlockHeader {
            hash: "stored_block_hash".to_string(),
            previous_blockhash: "prev_hash".to_string(),
            parent_slot: 49,
            block_time: 1234567890,
            block_height: 50,
            signatures: vec![Signature::new_unique()],
        };
        svm.blocks.store(50, stored_block.clone()).unwrap();

        // Retrieve the stored block
        let result = svm.get_block_or_reconstruct(50).unwrap();
        assert!(result.is_some(), "should return stored block");
        let block = result.unwrap();
        assert_eq!(block.hash, "stored_block_hash");
        assert_eq!(block.signatures.len(), 1);
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_get_block_or_reconstruct_empty_block(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Set up: genesis_slot = 0, latest absolute slot = 100
        svm.genesis_slot = 0;
        svm.latest_epoch_info.absolute_slot = 100;
        svm.genesis_updated_at = 1000000; // 1 second in ms
        svm.slot_time = 400; // 400ms per slot

        // Request a slot that wasn't stored (no block stored at slot 50)
        let result = svm.get_block_or_reconstruct(50).unwrap();
        assert!(
            result.is_some(),
            "should reconstruct empty block for valid slot"
        );

        let block = result.unwrap();
        // Verify it's a reconstructed empty block
        assert!(
            block.signatures.is_empty(),
            "reconstructed block should have no signatures"
        );
        assert_eq!(block.block_height, 50);
        assert_eq!(block.parent_slot, 49);

        // Verify the block time is calculated correctly
        // genesis_updated_at (1000000ms) + (50 slots * 400ms) = 1020000ms = 1020 seconds
        assert_eq!(block.block_time, 1020);
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_get_block_or_reconstruct_out_of_range(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Set up: genesis_slot = 100, latest absolute slot = 110
        svm.genesis_slot = 100;
        svm.latest_epoch_info.absolute_slot = 110;

        // Request slot before genesis
        let result = svm.get_block_or_reconstruct(50).unwrap();
        assert!(
            result.is_none(),
            "should return None for slot before genesis"
        );

        // Request slot after latest
        let result = svm.get_block_or_reconstruct(200).unwrap();
        assert!(result.is_none(), "should return None for slot after latest");
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    #[allow(deprecated)]
    fn test_reconstruct_sysvars_recent_blockhashes(test_type: TestType) {
        use solana_sysvar::recent_blockhashes::RecentBlockhashes;

        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Set up: chain_tip.index = 10, genesis_slot = 0
        svm.chain_tip = BlockIdentifier::new(10, "test_hash");
        svm.genesis_slot = 0;
        svm.latest_epoch_info.absolute_slot = 10;

        svm.reconstruct_sysvars();

        // Verify RecentBlockhashes sysvar
        let recent_blockhashes = svm.inner.get_sysvar::<RecentBlockhashes>();

        // Should have 11 entries (indices 0 through 10)
        assert_eq!(recent_blockhashes.len(), 11);

        // First entry should be the hash for chain_tip.index (10)
        let expected_hash = SyntheticBlockhash::new(10);
        assert_eq!(
            recent_blockhashes.first().unwrap().blockhash,
            *expected_hash.hash(),
            "First blockhash should match SyntheticBlockhash for chain_tip.index"
        );

        // Last entry should be the hash for index 0
        let expected_last_hash = SyntheticBlockhash::new(0);
        assert_eq!(
            recent_blockhashes.last().unwrap().blockhash,
            *expected_last_hash.hash(),
            "Last blockhash should match SyntheticBlockhash for index 0"
        );
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    #[allow(deprecated)]
    fn test_reconstruct_sysvars_slot_hashes(test_type: TestType) {
        use solana_slot_hashes::SlotHashes;

        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Set up: chain_tip.index = 5, genesis_slot = 100 (absolute slot = 105)
        svm.chain_tip = BlockIdentifier::new(5, "test_hash");
        svm.genesis_slot = 100;
        svm.latest_epoch_info.absolute_slot = 105;

        svm.reconstruct_sysvars();

        // Verify SlotHashes sysvar
        let slot_hashes = svm.inner.get_sysvar::<SlotHashes>();

        // Should have 6 entries (indices 0 through 5, mapped to slots 100 through 105)
        assert_eq!(slot_hashes.len(), 6);

        // Check that slot 105 maps to hash for index 5
        let expected_hash_105 = SyntheticBlockhash::new(5);
        let hash_for_105 = slot_hashes.get(&105);
        assert!(hash_for_105.is_some(), "SlotHashes should contain slot 105");
        assert_eq!(
            hash_for_105.unwrap(),
            expected_hash_105.hash(),
            "Hash for slot 105 should match SyntheticBlockhash for index 5"
        );

        // Check that slot 100 maps to hash for index 0
        let expected_hash_100 = SyntheticBlockhash::new(0);
        let hash_for_100 = slot_hashes.get(&100);
        assert!(hash_for_100.is_some(), "SlotHashes should contain slot 100");
        assert_eq!(
            hash_for_100.unwrap(),
            expected_hash_100.hash(),
            "Hash for slot 100 should match SyntheticBlockhash for index 0"
        );
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    fn test_reconstruct_sysvars_clock(test_type: TestType) {
        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Set up: chain_tip.index = 50, genesis_slot = 1000 (absolute slot = 1050)
        svm.chain_tip = BlockIdentifier::new(50, "test_hash");
        svm.genesis_slot = 1000;
        svm.latest_epoch_info.absolute_slot = 1050;
        svm.latest_epoch_info.epoch = 5;
        svm.genesis_updated_at = 2_000_000; // 2 seconds in ms
        svm.slot_time = 400; // 400ms per slot

        svm.reconstruct_sysvars();

        // Verify Clock sysvar
        let clock = svm.inner.get_sysvar::<Clock>();

        assert_eq!(clock.slot, 1050, "Clock slot should be absolute slot");
        assert_eq!(clock.epoch, 5, "Clock epoch should match latest_epoch_info");

        // Expected timestamp: genesis_updated_at + (50 slots * 400ms) = 2_000_000 + 20_000 = 2_020_000ms = 2020 seconds
        assert_eq!(
            clock.unix_timestamp, 2020,
            "Clock unix_timestamp should be calculated correctly"
        );
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    #[allow(deprecated)]
    fn test_reconstruct_sysvars_max_blockhashes(test_type: TestType) {
        use solana_sysvar::recent_blockhashes::RecentBlockhashes;

        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Set up: chain_tip.index = 200 (more than MAX_RECENT_BLOCKHASHES_STANDARD = 150)
        svm.chain_tip = BlockIdentifier::new(200, "test_hash");
        svm.genesis_slot = 0;
        svm.latest_epoch_info.absolute_slot = 200;

        svm.reconstruct_sysvars();

        // Verify RecentBlockhashes sysvar is capped at MAX_RECENT_BLOCKHASHES_STANDARD
        let recent_blockhashes = svm.inner.get_sysvar::<RecentBlockhashes>();

        assert_eq!(
            recent_blockhashes.len(),
            MAX_RECENT_BLOCKHASHES_STANDARD,
            "RecentBlockhashes should be capped at MAX_RECENT_BLOCKHASHES_STANDARD"
        );

        // First entry should still be for chain_tip.index (200)
        let expected_hash = SyntheticBlockhash::new(200);
        assert_eq!(
            recent_blockhashes.first().unwrap().blockhash,
            *expected_hash.hash(),
            "First blockhash should match SyntheticBlockhash for chain_tip.index"
        );

        // Last entry should be for index 51 (200 - 149)
        let expected_last_hash = SyntheticBlockhash::new(51);
        assert_eq!(
            recent_blockhashes.last().unwrap().blockhash,
            *expected_last_hash.hash(),
            "Last blockhash should match SyntheticBlockhash for start_index"
        );
    }

    #[test_case(TestType::sqlite(); "with on-disk sqlite db")]
    #[test_case(TestType::in_memory(); "with in-memory sqlite db")]
    #[test_case(TestType::no_db(); "with no db")]
    #[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
    #[allow(deprecated)]
    fn test_reconstruct_sysvars_deterministic(test_type: TestType) {
        use solana_slot_hashes::SlotHashes;
        use solana_sysvar::recent_blockhashes::RecentBlockhashes;

        let (mut svm, _events_rx, _geyser_rx) = test_type.initialize_svm();

        // Set up initial state
        svm.chain_tip = BlockIdentifier::new(25, "test_hash");
        svm.genesis_slot = 50;
        svm.latest_epoch_info.absolute_slot = 75;
        svm.latest_epoch_info.epoch = 2;
        svm.genesis_updated_at = 1_000_000;
        svm.slot_time = 400;

        // First reconstruction
        svm.reconstruct_sysvars();
        let blockhashes_1 = svm.inner.get_sysvar::<RecentBlockhashes>();
        let slot_hashes_1 = svm.inner.get_sysvar::<SlotHashes>();
        let clock_1 = svm.inner.get_sysvar::<Clock>();

        // Second reconstruction with same state
        svm.reconstruct_sysvars();
        let blockhashes_2 = svm.inner.get_sysvar::<RecentBlockhashes>();
        let slot_hashes_2 = svm.inner.get_sysvar::<SlotHashes>();
        let clock_2 = svm.inner.get_sysvar::<Clock>();

        // Verify determinism - results should be identical
        assert_eq!(blockhashes_1.len(), blockhashes_2.len());
        for (b1, b2) in blockhashes_1.iter().zip(blockhashes_2.iter()) {
            assert_eq!(
                b1.blockhash, b2.blockhash,
                "RecentBlockhashes should be deterministic"
            );
        }

        assert_eq!(slot_hashes_1.len(), slot_hashes_2.len());
        assert_eq!(clock_1.slot, clock_2.slot);
        assert_eq!(clock_1.epoch, clock_2.epoch);
        assert_eq!(clock_1.unix_timestamp, clock_2.unix_timestamp);
    }
}
