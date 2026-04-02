#[cfg(feature = "prometheus")]
use std::time::SystemTime;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    fmt,
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex},
};

use blake3::Hash;
use chrono::{DateTime, Local};
use crossbeam_channel::Sender;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Visitor};
use serde_with::{BytesOrString, serde_as};
use solana_account::Account;
use solana_account_decoder_client_types::{ParsedAccount, UiAccount, UiAccountEncoding};
use solana_clock::{Clock, Epoch, Slot};
use solana_epoch_info::EpochInfo;
use solana_message::inner_instruction::InnerInstructionsList;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_context::TransactionReturnData;
use solana_transaction_error::TransactionError;
use txtx_addon_kit::indexmap::IndexMap;
use uuid::Uuid;

use crate::DEFAULT_MAINNET_RPC_URL;

pub const DEFAULT_RPC_PORT: u16 = 8899;
pub const DEFAULT_WS_PORT: u16 = 8900;
pub const DEFAULT_STUDIO_PORT: u16 = 8488;
pub const CHANGE_TO_DEFAULT_STUDIO_PORT_ONCE_SUPERVISOR_MERGED: u16 = 18488;
pub const DEFAULT_NETWORK_HOST: &str = "127.0.0.1";
pub const DEFAULT_SLOT_TIME_MS: u64 = 400;
pub type Idl = anchor_lang_idl::types::Idl;
pub const DEFAULT_PROFILING_MAP_CAPACITY: usize = 200;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransactionMetadata {
    pub signature: Signature,
    pub logs: Vec<String>,
    pub inner_instructions: InnerInstructionsList,
    pub compute_units_consumed: u64,
    pub return_data: TransactionReturnData,
    pub fee: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransactionConfirmationStatus {
    Processed,
    Confirmed,
    Finalized,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum BlockProductionMode {
    #[default]
    Clock,
    Transaction,
    Manual,
}

impl fmt::Display for BlockProductionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockProductionMode::Clock => write!(f, "clock"),
            BlockProductionMode::Transaction => write!(f, "transaction"),
            BlockProductionMode::Manual => write!(f, "manual"),
        }
    }
}

impl FromStr for BlockProductionMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "clock" => Ok(BlockProductionMode::Clock),
            "transaction" => Ok(BlockProductionMode::Transaction),
            "manual" => Ok(BlockProductionMode::Manual),
            _ => Err(format!(
                "Invalid block production mode: {}. Valid values are: clock, transaction, manual",
                s
            )),
        }
    }
}

#[derive(Debug)]
pub enum SubgraphEvent {
    EndpointReady,
    InfoLog(DateTime<Local>, String),
    ErrorLog(DateTime<Local>, String),
    WarnLog(DateTime<Local>, String),
    DebugLog(DateTime<Local>, String),
    Shutdown,
}

impl SubgraphEvent {
    pub fn info<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::InfoLog(Local::now(), msg.into())
    }

    pub fn warn<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::WarnLog(Local::now(), msg.into())
    }

    pub fn error<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::ErrorLog(Local::now(), msg.into())
    }

    pub fn debug<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::DebugLog(Local::now(), msg.into())
    }
}

/// Result structure for compute units estimation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ComputeUnitsEstimationResult {
    pub success: bool,
    pub compute_units_consumed: u64,
    pub log_messages: Option<Vec<String>>,
    pub error_message: Option<String>,
}

/// The struct for storing the profiling results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyedProfileResult {
    pub slot: u64,
    pub key: UuidOrSignature,
    pub instruction_profiles: Option<Vec<ProfileResult>>,
    pub transaction_profile: ProfileResult,
    #[serde(with = "pubkey_account_map")]
    pub readonly_account_states: HashMap<Pubkey, Account>,
}

impl KeyedProfileResult {
    pub fn new(
        slot: u64,
        key: UuidOrSignature,
        instruction_profiles: Option<Vec<ProfileResult>>,
        transaction_profile: ProfileResult,
        readonly_account_states: HashMap<Pubkey, Account>,
    ) -> Self {
        Self {
            slot,
            key,
            instruction_profiles,
            transaction_profile,
            readonly_account_states,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileResult {
    #[serde(with = "pubkey_option_account_map")]
    pub pre_execution_capture: ExecutionCapture,
    #[serde(with = "pubkey_option_account_map")]
    pub post_execution_capture: ExecutionCapture,
    pub compute_units_consumed: u64,
    pub log_messages: Option<Vec<String>>,
    pub error_message: Option<String>,
}

pub type ExecutionCapture = BTreeMap<Pubkey, Option<Account>>;

impl ProfileResult {
    pub fn new(
        pre_execution_capture: ExecutionCapture,
        post_execution_capture: ExecutionCapture,
        compute_units_consumed: u64,
        log_messages: Option<Vec<String>>,
        error_message: Option<String>,
    ) -> Self {
        Self {
            pre_execution_capture,
            post_execution_capture,
            compute_units_consumed,
            log_messages,
            error_message,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AccountProfileState {
    Readonly,
    Writable(AccountChange),
}

impl AccountProfileState {
    pub fn new(
        pubkey: Pubkey,
        pre_account: Option<Account>,
        post_account: Option<Account>,
        readonly_accounts: &[Pubkey],
    ) -> Self {
        if readonly_accounts.contains(&pubkey) {
            return AccountProfileState::Readonly;
        }

        match (pre_account, post_account) {
            (None, Some(post_account)) => {
                AccountProfileState::Writable(AccountChange::Create(post_account))
            }
            (Some(pre_account), None) => {
                AccountProfileState::Writable(AccountChange::Delete(pre_account))
            }
            (Some(pre_account), Some(post_account)) if pre_account == post_account => {
                AccountProfileState::Writable(AccountChange::Unchanged(Some(pre_account)))
            }
            (Some(pre_account), Some(post_account)) => {
                AccountProfileState::Writable(AccountChange::Update(pre_account, post_account))
            }
            (None, None) => AccountProfileState::Writable(AccountChange::Unchanged(None)),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AccountChange {
    Create(Account),
    Update(Account, Account),
    Delete(Account),
    Unchanged(Option<Account>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcProfileResultConfig {
    pub encoding: Option<UiAccountEncoding>,
    pub depth: Option<RpcProfileDepth>,
}

impl Default for RpcProfileResultConfig {
    fn default() -> Self {
        Self {
            encoding: Some(UiAccountEncoding::JsonParsed),
            depth: Some(RpcProfileDepth::default()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpcProfileDepth {
    Transaction,
    #[default]
    Instruction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UiKeyedProfileResult {
    pub slot: u64,
    pub key: UuidOrSignature,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction_profiles: Option<Vec<UiProfileResult>>,
    pub transaction_profile: UiProfileResult,
    #[serde(with = "profile_state_map")]
    pub readonly_account_states: IndexMap<Pubkey, UiAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UiProfileResult {
    #[serde(with = "profile_state_map")]
    pub account_states: IndexMap<Pubkey, UiAccountProfileState>,
    pub compute_units_consumed: u64,
    pub log_messages: Option<Vec<String>>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", tag = "type", content = "accountChange")]
#[allow(clippy::large_enum_variant)]
pub enum UiAccountProfileState {
    Readonly,
    Writable(UiAccountChange),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", tag = "type", content = "data")]
pub enum UiAccountChange {
    Create(UiAccount),
    Update(UiAccount, UiAccount),
    Delete(UiAccount),
    /// The account didn't change. If [Some], this is the initial state. If [None], the account didn't exist before/after execution.
    Unchanged(Option<UiAccount>),
}

/// P starts with 300 lamports
/// Ix 1 Transfers 100 lamports to P
/// Ix 2 Transfers 100 lamports to P
///
/// Profile result 1 is from executing just Ix 1
/// AccountProfileState::Writable(P, AccountChange::Update( UiAccount { lamports: 300, ...}, UiAccount { lamports: 400, ... }))
///
/// Profile result 2 is from executing Ix 1 and Ix 2
/// AccountProfileState::Writable(P, AccountChange::Update( UiAccount { lamports: 400, ...}, UiAccount { lamports: 500, ... }))
pub mod profile_state_map {
    use super::*;

    pub fn serialize<S, T>(map: &IndexMap<Pubkey, T>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        let str_map: IndexMap<String, &T> = map.iter().map(|(k, v)| (k.to_string(), v)).collect();
        str_map.serialize(serializer)
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<IndexMap<Pubkey, T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        let str_map: IndexMap<String, T> = IndexMap::deserialize(deserializer)?;
        str_map
            .into_iter()
            .map(|(k, v)| {
                Pubkey::from_str(&k)
                    .map(|pk| (pk, v))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

/// Serialization module for HashMap<Pubkey, Account>
pub mod pubkey_account_map {
    use super::*;

    pub fn serialize<S>(map: &HashMap<Pubkey, Account>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let str_map: HashMap<String, &Account> =
            map.iter().map(|(k, v)| (k.to_string(), v)).collect();
        str_map.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<HashMap<Pubkey, Account>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let str_map: HashMap<String, Account> = HashMap::deserialize(deserializer)?;
        str_map
            .into_iter()
            .map(|(k, v)| {
                Pubkey::from_str(&k)
                    .map(|pk| (pk, v))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

/// Serialization module for BTreeMap<Pubkey, Option<Account>>
pub mod pubkey_option_account_map {
    use super::*;

    pub fn serialize<S>(
        map: &BTreeMap<Pubkey, Option<Account>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let str_map: BTreeMap<String, &Option<Account>> =
            map.iter().map(|(k, v)| (k.to_string(), v)).collect();
        str_map.serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<BTreeMap<Pubkey, Option<Account>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let str_map: BTreeMap<String, Option<Account>> = BTreeMap::deserialize(deserializer)?;
        str_map
            .into_iter()
            .map(|(k, v)| {
                Pubkey::from_str(&k)
                    .map(|pk| (pk, v))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

#[derive(Debug)]
pub enum SimnetEvent {
    /// Surfnet is ready, with the initial count of processed transactions from storage
    Ready(u64),
    Connected(String),
    Aborted(String),
    Shutdown,
    SystemClockUpdated(Clock),
    ClockUpdate(ClockCommand),
    EpochInfoUpdate(EpochInfo),
    BlockHashExpired,
    InfoLog(DateTime<Local>, String),
    ErrorLog(DateTime<Local>, String),
    WarnLog(DateTime<Local>, String),
    DebugLog(DateTime<Local>, String),
    PluginLoaded(String),
    TransactionReceived(DateTime<Local>, VersionedTransaction),
    TransactionProcessed(
        DateTime<Local>,
        TransactionMetadata,
        Option<TransactionError>,
    ),
    AccountUpdate(DateTime<Local>, Pubkey),
    TaggedProfile {
        result: KeyedProfileResult,
        tag: String,
        timestamp: DateTime<Local>,
    },
    RunbookStarted(String),
    RunbookCompleted(String, Option<Vec<String>>),
}

impl SimnetEvent {
    pub fn info<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::InfoLog(Local::now(), msg.into())
    }

    pub fn warn<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::WarnLog(Local::now(), msg.into())
    }

    pub fn error<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::ErrorLog(Local::now(), msg.into())
    }

    pub fn debug<S>(msg: S) -> Self
    where
        S: Into<String>,
    {
        Self::DebugLog(Local::now(), msg.into())
    }

    pub fn transaction_processed(meta: TransactionMetadata, err: Option<TransactionError>) -> Self {
        Self::TransactionProcessed(Local::now(), meta, err)
    }

    pub fn transaction_received(tx: VersionedTransaction) -> Self {
        Self::TransactionReceived(Local::now(), tx)
    }

    pub fn account_update(pubkey: Pubkey) -> Self {
        Self::AccountUpdate(Local::now(), pubkey)
    }

    pub fn tagged_profile(result: KeyedProfileResult, tag: String) -> Self {
        Self::TaggedProfile {
            result,
            tag,
            timestamp: Local::now(),
        }
    }

    pub fn account_update_msg(&self) -> String {
        match self {
            SimnetEvent::AccountUpdate(_, pubkey) => {
                format!("Account {} updated.", pubkey)
            }
            _ => unreachable!("This function should only be called for AccountUpdate events"),
        }
    }

    pub fn epoch_info_update_msg(&self) -> String {
        match self {
            SimnetEvent::EpochInfoUpdate(epoch_info) => {
                format!(
                    "Datasource connection successful. Epoch {} / Slot index {} / Slot {}.",
                    epoch_info.epoch, epoch_info.slot_index, epoch_info.absolute_slot
                )
            }
            _ => unreachable!("This function should only be called for EpochInfoUpdate events"),
        }
    }

    pub fn plugin_loaded_msg(&self) -> String {
        match self {
            SimnetEvent::PluginLoaded(plugin_name) => {
                format!("Plugin {} successfully loaded.", plugin_name)
            }
            _ => unreachable!("This function should only be called for PluginLoaded events"),
        }
    }

    pub fn clock_update_msg(&self) -> String {
        match self {
            SimnetEvent::SystemClockUpdated(clock) => {
                format!("Clock ticking (epoch {}, slot {})", clock.epoch, clock.slot)
            }
            _ => {
                unreachable!("This function should only be called for SystemClockUpdated events")
            }
        }
    }
}

#[derive(Debug)]
pub enum TransactionStatusEvent {
    Success(TransactionConfirmationStatus),
    SimulationFailure((TransactionError, TransactionMetadata)),
    ExecutionFailure((TransactionError, TransactionMetadata)),
    VerificationFailure(String),
}

#[derive(Debug)]
pub enum SimnetCommand {
    SlotForward(Option<Hash>),
    SlotBackward(Option<Hash>),
    CommandClock(Option<(Hash, String)>, ClockCommand),
    UpdateInternalClock(Option<(Hash, String)>, Clock),
    UpdateInternalClockWithConfirmation(Option<(Hash, String)>, Clock, Sender<EpochInfo>),
    UpdateBlockProductionMode(BlockProductionMode),
    ProcessTransaction(
        Option<(Hash, String)>,
        VersionedTransaction,
        Sender<TransactionStatusEvent>,
        bool,
        Option<bool>,
    ),
    Terminate(Option<(Hash, String)>),
    StartRunbookExecution(String),
    CompleteRunbookExecution(String, Option<Vec<String>>),
    FetchRemoteAccounts(Vec<Pubkey>, String),
    AirdropProcessed,
    SendBundle((String, Vec<String>)),
}

#[derive(Debug)]
pub enum ClockCommand {
    Pause,
    /// Pause with confirmation - sends epoch info back when actually paused
    PauseWithConfirmation(Sender<EpochInfo>),
    Resume,
    Toggle,
    UpdateSlotInterval(u64),
}

pub enum ClockEvent {
    Tick,
    ExpireBlockHash,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SanitizedConfig {
    pub rpc_url: String,
    pub ws_url: String,
    pub rpc_datasource_url: Option<String>,
    pub studio_url: String,
    pub graphql_query_route_url: String,
    pub version: String,
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SurfpoolConfig {
    pub simnets: Vec<SimnetConfig>,
    pub rpc: RpcConfig,
    pub subgraph: SubgraphConfig,
    pub studio: StudioConfig,
    pub plugin_config_path: Vec<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimnetConfig {
    pub offline_mode: bool,
    pub remote_rpc_url: Option<String>,
    pub slot_time: u64,
    pub block_production_mode: BlockProductionMode,
    pub airdrop_addresses: Vec<Pubkey>,
    pub airdrop_token_amount: u64,
    pub expiry: Option<u64>,
    pub instruction_profiling_enabled: bool,
    pub max_profiles: usize,
    pub log_bytes_limit: Option<usize>,
    pub skip_signature_verification: bool,
    /// Unique identifier for this surfnet instance. Used to isolate database storage
    /// when multiple surfnets share the same database. Defaults to "default".
    pub surfnet_id: String,
    /// Snapshot accounts to preload at startup.
    /// Keys are pubkey strings, values can be None to fetch from remote RPC.
    pub snapshot: BTreeMap<String, Option<AccountSnapshot>>,
}

impl Default for SimnetConfig {
    fn default() -> Self {
        Self {
            offline_mode: false,
            remote_rpc_url: Some(DEFAULT_MAINNET_RPC_URL.to_string()),
            slot_time: DEFAULT_SLOT_TIME_MS, // Default to 400ms to match CLI default
            block_production_mode: BlockProductionMode::Clock,
            airdrop_addresses: vec![],
            airdrop_token_amount: 0,
            expiry: None,
            instruction_profiling_enabled: true,
            max_profiles: DEFAULT_PROFILING_MAP_CAPACITY,
            log_bytes_limit: Some(10_000),
            skip_signature_verification: false,
            surfnet_id: "default".to_string(),
            snapshot: BTreeMap::new(),
        }
    }
}

impl SimnetConfig {
    /// Returns a sanitized version of the datasource URL safe for display.
    /// Only returns scheme and host (e.g., "https://example.com") to prevent
    /// leaking API keys in paths or query parameters.
    pub fn get_sanitized_datasource_url(&self) -> Option<String> {
        let raw = self.remote_rpc_url.as_ref()?;

        if let Ok(url) = url::Url::parse(raw) {
            let scheme = url.scheme();
            let host = url.host_str()?;
            Some(format!("{}://{}", scheme, host))
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SubgraphConfig {}

pub const DEFAULT_GOSSIP_PORT: u16 = 8001;
pub const DEFAULT_TPU_PORT: u16 = 8003;
pub const DEFAULT_TPU_QUIC_PORT: u16 = 8004;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcConfig {
    pub bind_host: String,
    pub bind_port: u16,
    pub ws_port: u16,
    pub gossip_port: u16,
    pub tpu_port: u16,
    pub tpu_quic_port: u16,
}

impl RpcConfig {
    pub fn get_rpc_base_url(&self) -> String {
        format!("{}:{}", self.bind_host, self.bind_port)
    }
    pub fn get_ws_base_url(&self) -> String {
        format!("{}:{}", self.bind_host, self.ws_port)
    }
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            bind_host: DEFAULT_NETWORK_HOST.to_string(),
            bind_port: DEFAULT_RPC_PORT,
            ws_port: DEFAULT_WS_PORT,
            gossip_port: DEFAULT_GOSSIP_PORT,
            tpu_port: DEFAULT_TPU_PORT,
            tpu_quic_port: DEFAULT_TPU_QUIC_PORT,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StudioConfig {
    pub bind_host: String,
    pub bind_port: u16,
}

impl StudioConfig {
    pub fn get_studio_base_url(&self) -> String {
        format!("{}:{}", self.bind_host, self.bind_port)
    }
}

impl Default for StudioConfig {
    fn default() -> Self {
        Self {
            bind_host: DEFAULT_NETWORK_HOST.to_string(),
            bind_port: CHANGE_TO_DEFAULT_STUDIO_PORT_ONCE_SUPERVISOR_MERGED,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub struct CreateSurfnetRequest {
    pub domain: String,
    pub block_production_mode: BlockProductionMode,
    pub datasource_rpc_url: String,
    pub settings: Option<CloudSurfnetSettings>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "snake_case", default)]
pub struct CloudSurfnetSettings {
    pub database_url: Option<String>,
    pub profiling_disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gating: Option<CloudSurfnetRpcGating>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "snake_case", default)]
pub struct CloudSurfnetRpcGating {
    pub private_methods_secret_token: Option<String>,
    pub private_methods: Vec<String>,
    pub public_methods: Vec<String>,
    pub disabled_methods: Vec<String>,
}

impl CloudSurfnetRpcGating {
    pub fn restricted() -> CloudSurfnetRpcGating {
        CloudSurfnetRpcGating {
            private_methods: vec![],
            private_methods_secret_token: None,
            public_methods: vec![],
            disabled_methods: vec![
                "surfnet_cloneProgramAccount".into(),
                "surfnet_profileTransaction".into(),
                "surfnet_getProfileResultsByTag".into(),
                "surfnet_setSupply".into(),
                "surfnet_setProgramAuthority".into(),
                "surfnet_getTransactionProfile".into(),
                "surfnet_registerIdl".into(),
                "surfnet_getActiveIdl".into(),
                "surfnet_getLocalSignatures".into(),
                "surfnet_timeTravel".into(),
                "surfnet_pauseClock".into(),
                "surfnet_resumeClock".into(),
                "surfnet_resetAccount".into(),
                "surfnet_resetNetwork".into(),
                "surfnet_exportSnapshot".into(),
                "surfnet_offlineAccount".into(),
                "surfnet_streamAccount".into(),
                "surfnet_streamAccounts".into(),
                "surfnet_getStreamedAccounts".into(),
            ],
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CreateNetworkRequest {
    pub workspace_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub datasource_rpc_url: String,
    pub block_production_mode: BlockProductionMode,
    pub profiling_enabled: Option<bool>,
}

impl CreateNetworkRequest {
    pub fn new(
        workspace_id: Uuid,
        name: String,
        description: Option<String>,
        datasource_rpc_url: String,
        block_production_mode: BlockProductionMode,
        profiling_enabled: bool,
    ) -> Self {
        Self {
            workspace_id,
            name,
            description,
            datasource_rpc_url,
            block_production_mode,
            profiling_enabled: Some(profiling_enabled),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct CreateNetworkResponse {
    pub rpc_url: String,
}

#[derive(Serialize, Deserialize)]
pub struct DeleteNetworkRequest {
    pub workspace_id: Uuid,
    pub network_id: Uuid,
}

impl DeleteNetworkRequest {
    pub fn new(workspace_id: Uuid, network_id: Uuid) -> Self {
        Self {
            workspace_id,
            network_id,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct DeleteNetworkResponse;

#[serde_as]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUpdate {
    /// providing this value sets the lamports in the account
    pub lamports: Option<u64>,
    /// providing this value sets the data held in this account
    #[serde_as(as = "Option<BytesOrString>")]
    pub data: Option<Vec<u8>>,
    ///  providing this value sets the program that owns this account. If executable, the program that loads this account.
    pub owner: Option<String>,
    /// providing this value sets whether this account's data contains a loaded program (and is now read-only)
    pub executable: Option<bool>,
    /// providing this value sets the epoch at which this account will next owe rent
    pub rent_epoch: Option<Epoch>,
}

#[derive(Debug, Clone)]
pub enum SetSomeAccount {
    Account(String),
    NoAccount,
}

impl<'de> Deserialize<'de> for SetSomeAccount {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SetSomeAccountVisitor;

        impl<'de> Visitor<'de> for SetSomeAccountVisitor {
            type Value = SetSomeAccount;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a Pubkey String or the String 'null'")
            }

            fn visit_some<D_>(self, deserializer: D_) -> std::result::Result<Self::Value, D_::Error>
            where
                D_: Deserializer<'de>,
            {
                Deserialize::deserialize(deserializer).map(|v: String| match v.as_str() {
                    "null" => SetSomeAccount::NoAccount,
                    _ => SetSomeAccount::Account(v.to_string()),
                })
            }
        }

        deserializer.deserialize_option(SetSomeAccountVisitor)
    }
}

impl Serialize for SetSomeAccount {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            SetSomeAccount::Account(val) => serializer.serialize_str(val),
            SetSomeAccount::NoAccount => serializer.serialize_str("null"),
        }
    }
}

#[serde_as]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenAccountUpdate {
    /// providing this value sets the amount of the token in the account data
    pub amount: Option<u64>,
    /// providing this value sets the delegate of the token account
    pub delegate: Option<SetSomeAccount>,
    /// providing this value sets the state of the token account
    pub state: Option<String>,
    /// providing this value sets the amount authorized to the delegate
    pub delegated_amount: Option<u64>,
    /// providing this value sets the close authority of the token account
    pub close_authority: Option<SetSomeAccount>,
}

// token supply update for set supply method in SVM tricks
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SupplyUpdate {
    pub total: Option<u64>,
    pub circulating: Option<u64>,
    pub non_circulating: Option<u64>,
    pub non_circulating_accounts: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Copy)]
pub enum UuidOrSignature {
    Uuid(Uuid),
    Signature(Signature),
}

impl std::fmt::Display for UuidOrSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UuidOrSignature::Uuid(uuid) => write!(f, "{}", uuid),
            UuidOrSignature::Signature(signature) => write!(f, "{}", signature),
        }
    }
}

impl<'de> Deserialize<'de> for UuidOrSignature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;

        if let Ok(uuid) = Uuid::parse_str(&s) {
            return Ok(UuidOrSignature::Uuid(uuid));
        }

        if let Ok(signature) = s.parse::<Signature>() {
            return Ok(UuidOrSignature::Signature(signature));
        }

        Err(serde::de::Error::custom(
            "expected a Uuid or a valid Solana Signature",
        ))
    }
}

impl Serialize for UuidOrSignature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            UuidOrSignature::Uuid(uuid) => serializer.serialize_str(&uuid.to_string()),
            UuidOrSignature::Signature(signature) => {
                serializer.serialize_str(&signature.to_string())
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum DataIndexingCommand {
    ProcessCollection(Uuid),
    ProcessCollectionEntriesPack(Uuid, Vec<u8>),
}

// Define a wrapper struct
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionedIdl(pub Slot, pub Idl);

// Implement ordering based on Slot
impl PartialEq for VersionedIdl {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for VersionedIdl {}

impl PartialOrd for VersionedIdl {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VersionedIdl {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

#[derive(Debug, Clone)]
pub struct FifoMap<K, V> {
    // IndexMap is a map that preserves the insertion order of the keys. (It will be used for the FIFO eviction)
    map: IndexMap<K, V>,
}

impl<K: std::hash::Hash + Eq, V> Default for FifoMap<K, V> {
    fn default() -> Self {
        Self::new(DEFAULT_PROFILING_MAP_CAPACITY)
    }
}
impl<K: std::hash::Hash + Eq, V> FifoMap<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            map: IndexMap::with_capacity(capacity),
        }
    }

    pub fn capacity(&self) -> usize {
        self.map.capacity()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Insert a key/value. If `K` is new and we're full, evict the oldest (FIFO)
    /// Returns a tuple of (old_value, evicted_key):
    /// - old_value: The previous value if this was an update to an existing key
    /// - evicted_key: The key that was evicted if the map was at capacity
    pub fn insert(&mut self, key: K, value: V) -> (Option<V>, Option<K>) {
        if self.map.contains_key(&key) {
            // Update doesn't change insertion order in IndexMap
            return (self.map.insert(key, value), None);
        }
        let evicted_key = if self.map.len() == self.map.capacity() {
            // Evict oldest (index 0). O(n) due shifting the rest of the map
            // We could use a hashmap + vecdeque to get O(1) here, but then we'd have to handle removing from both maps, storing the index, and managing the eviction.
            // This is a good compromise between performance and simplicity. And thinking about memory usage, this is probably the best way to go.
            self.map.shift_remove_index(0).map(|(k, _)| k)
        } else {
            None
        };
        self.map.insert(key, value);
        (None, evicted_key)
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.map.get_mut(key)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Removes a key from the map, returning the value if present.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.map.shift_remove(key)
    }

    // This is a wrapper around the IndexMap::iter() method, but it preserves the insertion order of the keys.
    // It's used to iterate over the profiling map in the order of the keys being inserted.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&K, &V)> {
        self.map.iter()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSnapshot {
    pub lamports: u64,
    pub owner: String,
    pub executable: bool,
    pub rent_epoch: u64,
    /// Base64 encoded data
    pub data: String,
    /// Parsed account data if available
    pub parsed_data: Option<ParsedAccount>,
}

impl AccountSnapshot {
    pub fn new(
        lamports: u64,
        owner: String,
        executable: bool,
        rent_epoch: u64,
        data: String,
        parsed_data: Option<ParsedAccount>,
    ) -> Self {
        Self {
            lamports,
            owner,
            executable,
            rent_epoch,
            data,
            parsed_data,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSnapshotConfig {
    pub include_parsed_accounts: Option<bool>,
    pub filter: Option<ExportSnapshotFilter>,
    pub scope: ExportSnapshotScope,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExportSnapshotScope {
    #[default]
    Network,
    PreTransaction(String),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSnapshotFilter {
    pub include_program_accounts: Option<bool>,
    pub include_accounts: Option<Vec<String>>,
    pub exclude_accounts: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetAccountConfig {
    pub include_owned_accounts: Option<bool>,
}

impl Default for ResetAccountConfig {
    fn default() -> Self {
        Self {
            include_owned_accounts: Some(false),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamAccountConfig {
    pub include_owned_accounts: Option<bool>,
}

impl Default for StreamAccountConfig {
    fn default() -> Self {
        Self {
            include_owned_accounts: Some(false),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamAccountsEntry {
    pub pubkey: String,
    #[serde(default)]
    pub include_owned_accounts: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfflineAccountConfig {
    pub include_owned_accounts: Option<bool>,
}

impl Default for OfflineAccountConfig {
    fn default() -> Self {
        Self {
            include_owned_accounts: Some(false),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamedAccountInfo {
    pub pubkey: String,
    pub include_owned_accounts: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetSurfnetInfoResponse {
    runbook_executions: Vec<RunbookExecutionStatusReport>,
}
impl GetSurfnetInfoResponse {
    pub fn new(runbook_executions: Vec<RunbookExecutionStatusReport>) -> Self {
        Self { runbook_executions }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetStreamedAccountsResponse {
    accounts: Vec<StreamedAccountInfo>,
}
impl GetStreamedAccountsResponse {
    pub fn from_iter<I>(streamed_accounts: I) -> Self
    where
        I: IntoIterator<Item = (String, bool)>,
    {
        let accounts = streamed_accounts
            .into_iter()
            .map(|(pubkey, include_owned_accounts)| StreamedAccountInfo {
                pubkey,
                include_owned_accounts,
            })
            .collect();
        Self { accounts }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunbookExecutionStatusReport {
    pub started_at: u32,
    pub completed_at: Option<u32>,
    pub runbook_id: String,
    pub errors: Option<Vec<String>>,
}
impl RunbookExecutionStatusReport {
    pub fn new(runbook_id: String) -> Self {
        Self {
            started_at: Local::now().timestamp() as u32,
            completed_at: None,
            runbook_id,
            errors: None,
        }
    }
    pub fn mark_completed(&mut self, error: Option<Vec<String>>) {
        self.completed_at = Some(Local::now().timestamp() as u32);
        self.errors = error;
    }
}
/// WebSocket subscription counts
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct WsSubscriptions {
    pub signatures: usize,
    pub accounts: usize,
    pub slots: usize,
    pub logs: usize,
}

/// Surfpool node status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfpoolStatus {
    pub slot: u64,
    pub epoch: u64,
    pub slot_index: u64,
    pub transactions_count: u64,
    pub transactions_processed: u64,
    pub uptime_ms: u64,
    pub ws_subscriptions: WsSubscriptions,
}

#[cfg(feature = "prometheus")]
fn default_prometheus_addr() -> String {
    "0.0.0.0:9000".to_string()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheatcodeConfig {
    pub lockout: bool, // if true, allows disabling even the `surfnet_enableCheatcodes`/`surfnetdisableCheatcodes` methods
    pub filter: CheatcodeFilter,
}

#[derive(Serialize, Deserialize, Default)]
pub struct CheatcodeControlConfig {
    pub lockout: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CheatcodeFilter {
    All(String),
    List(Vec<String>), // disables cheatcodes in a named list
}

impl CheatcodeConfig {
    pub fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(CheatcodeConfig {
            lockout: false,
            filter: CheatcodeFilter::List(vec![]),
        }))
    }

    pub fn lockout(&mut self) {
        self.lockout = true;
    }

    pub fn disable_all(&mut self, lockout: bool, available_cheatcodes: Vec<String>) {
        if lockout {
            self.lockout = true;
        }
        self.filter = Self::filter_all_list(lockout, available_cheatcodes);
    }

    pub fn disable_cheatcode(&mut self, cheatcode: &String) -> Result<(), String> {
        if !self.lockout
            && (cheatcode.eq("surfnet_enableCheatcode") || cheatcode.eq("surfnet_disableCheatcode"))
        {
            return Err("Cannot disable surfnet_disableCheatcode or surfnet_enableCheatcode while lockout is false".to_string());
        }

        if let CheatcodeFilter::List(list) = &mut self.filter {
            if !list.contains(cheatcode) {
                list.push(cheatcode.to_string());
                Ok(())
            } else {
                Err("Cheatcode already disabled".to_string())
            }
        } else {
            Err("All cheatcodes disabled".to_string())
        }
    }
    pub fn enable_cheatcode(&mut self, cheatcode: &str) -> Result<(), String> {
        if let CheatcodeFilter::List(list) = &mut self.filter {
            if let Some(pos) = list.iter().position(|c| c == cheatcode) {
                list.remove(pos);
                Ok(())
            } else {
                Err("Cheatcode isn't disabled".to_string())
            }
        } else {
            Err("All cheatcodes are disabled".to_string())
        }
    }

    pub fn is_cheatcode_disabled(&self, cheatcode: &String) -> bool {
        match &self.filter {
            CheatcodeFilter::List(list) => list.contains(cheatcode),
            CheatcodeFilter::All(_) => true,
        }
    }

    pub fn filter_all_list(lockout: bool, available_cheatcodes: Vec<String>) -> CheatcodeFilter {
        // when lockout == true, it's important to disable surfnet_disableCheatcode as well
        // since calling surfnet_disableCheatcode with lockout == false will override the current config, which is a bug
        if lockout {
            CheatcodeFilter::All("all".to_string())
        } else {
            // remove `surfnet_disableCheatcode` and `surfnet_enableCheatcode` from the list of available cheatcodes
            let filter = available_cheatcodes
                .into_iter()
                .filter(|c| (c.ne("surfnet_disableCheatcode") && c.ne("surfnet_enableCheatcode")))
                .collect();
            CheatcodeFilter::List(filter)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use solana_account_decoder_client_types::{ParsedAccount, UiAccountData};

    use super::*;

    #[test]
    fn test_disable_cheatcode_with_lockout_allows_protected_methods() {
        // This test catches the bug where lockout was not propagated to
        // CheatcodeConfig before calling disable_cheatcode(), causing
        // "Cannot disable surfnet_disableCheatcode or surfnet_enableCheatcode
        // while lockout is false" even when the request included lockout: true.
        let config = CheatcodeConfig::new();
        let mut config = config.lock().unwrap();

        // Simulate the RPC layer propagating lockout before processing the list
        config.lockout();

        // These should succeed because lockout is set
        assert!(
            config
                .disable_cheatcode(&"surfnet_setAccount".to_string())
                .is_ok()
        );
        assert!(
            config
                .disable_cheatcode(&"surfnet_enableCheatcode".to_string())
                .is_ok()
        );
        assert!(
            config
                .disable_cheatcode(&"surfnet_disableCheatcode".to_string())
                .is_ok()
        );
    }

    #[test]
    fn test_disable_cheatcode_without_lockout_rejects_protected_methods() {
        let config = CheatcodeConfig::new();
        let mut config = config.lock().unwrap();

        // Without lockout, disabling protected methods should fail
        assert!(
            config
                .disable_cheatcode(&"surfnet_enableCheatcode".to_string())
                .is_err()
        );
        assert!(
            config
                .disable_cheatcode(&"surfnet_disableCheatcode".to_string())
                .is_err()
        );

        // But regular cheatcodes should still work
        assert!(
            config
                .disable_cheatcode(&"surfnet_setAccount".to_string())
                .is_ok()
        );
    }

    #[test]
    fn test_disable_all_with_lockout_persists_lockout_flag() {
        // This test catches the bug where disable_all() did not set
        // self.lockout = true, so subsequent operations would not see lockout.
        let config = CheatcodeConfig::new();
        let mut config = config.lock().unwrap();

        let available = vec![
            "surfnet_setAccount".to_string(),
            "surfnet_enableCheatcode".to_string(),
            "surfnet_disableCheatcode".to_string(),
        ];

        config.disable_all(true, available);
        assert!(config.lockout);
    }

    #[test]
    fn test_disable_all_without_lockout_does_not_set_lockout() {
        let config = CheatcodeConfig::new();
        let mut config = config.lock().unwrap();

        let available = vec![
            "surfnet_setAccount".to_string(),
            "surfnet_enableCheatcode".to_string(),
            "surfnet_disableCheatcode".to_string(),
        ];

        config.disable_all(false, available);
        assert!(!config.lockout);
    }

    #[test]
    fn print_ui_keyed_profile_result() {
        let pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let readonly_account_state = UiAccount {
            lamports: 100,
            data: UiAccountData::Binary(
                "ABCDEFG".into(),
                solana_account_decoder_client_types::UiAccountEncoding::Base64,
            ),
            owner: owner.to_string(),
            executable: false,
            rent_epoch: 0,
            space: Some(100),
        };

        let account_1 = UiAccount {
            lamports: 100,
            data: UiAccountData::Json(ParsedAccount {
                program: "custom-program".into(),
                parsed: json!({
                    "field1": "value1",
                    "field2": "value2"
                }),
                space: 50,
            }),
            owner: owner.to_string(),
            executable: false,
            rent_epoch: 0,
            space: Some(100),
        };

        let account_2 = UiAccount {
            lamports: 100,
            data: UiAccountData::Json(ParsedAccount {
                program: "custom-program".into(),
                parsed: json!({
                    "field1": "updated-value1",
                    "field2": "updated-value2"
                }),
                space: 50,
            }),
            owner: owner.to_string(),
            executable: false,
            rent_epoch: 0,
            space: Some(100),
        };
        let profile_result = UiKeyedProfileResult {
            slot: 123,
            key: UuidOrSignature::Uuid(Uuid::new_v4()),
            instruction_profiles: Some(vec![
                UiProfileResult {
                    account_states: IndexMap::from_iter([
                        (
                            pubkey,
                            UiAccountProfileState::Writable(UiAccountChange::Create(
                                account_1.clone(),
                            )),
                        ),
                        (owner, UiAccountProfileState::Readonly),
                    ]),
                    compute_units_consumed: 100,
                    log_messages: Some(vec![
                        "Log message: Creating Account".to_string(),
                        "Log message: Account created".to_string(),
                    ]),
                    error_message: None,
                },
                UiProfileResult {
                    account_states: IndexMap::from_iter([
                        (
                            pubkey,
                            UiAccountProfileState::Writable(UiAccountChange::Update(
                                account_1,
                                account_2.clone(),
                            )),
                        ),
                        (owner, UiAccountProfileState::Readonly),
                    ]),
                    compute_units_consumed: 100,
                    log_messages: Some(vec![
                        "Log message: Updating Account".to_string(),
                        "Log message: Account updated".to_string(),
                    ]),
                    error_message: None,
                },
                UiProfileResult {
                    account_states: IndexMap::from_iter([
                        (
                            pubkey,
                            UiAccountProfileState::Writable(UiAccountChange::Delete(account_2)),
                        ),
                        (owner, UiAccountProfileState::Readonly),
                    ]),
                    compute_units_consumed: 100,
                    log_messages: Some(vec![
                        "Log message: Deleting Account".to_string(),
                        "Log message: Account deleted".to_string(),
                    ]),
                    error_message: None,
                },
            ]),
            transaction_profile: UiProfileResult {
                account_states: IndexMap::from_iter([
                    (
                        pubkey,
                        UiAccountProfileState::Writable(UiAccountChange::Unchanged(None)),
                    ),
                    (owner, UiAccountProfileState::Readonly),
                ]),
                compute_units_consumed: 300,
                log_messages: Some(vec![
                    "Log message: Creating Account".to_string(),
                    "Log message: Account created".to_string(),
                    "Log message: Updating Account".to_string(),
                    "Log message: Account updated".to_string(),
                    "Log message: Deleting Account".to_string(),
                    "Log message: Account deleted".to_string(),
                ]),
                error_message: None,
            },
            readonly_account_states: IndexMap::from_iter([(owner, readonly_account_state)]),
        };
        println!("{}", serde_json::to_string_pretty(&profile_result).unwrap());
    }

    #[test]
    fn test_profiling_map_capacity() {
        let profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        assert_eq!(profiling_map.capacity(), 10);
    }

    #[test]
    fn test_profiling_map_len() {
        let profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        assert!(profiling_map.len() == 0);
    }

    #[test]
    fn test_profiling_map_is_empty() {
        let profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        assert_eq!(profiling_map.is_empty(), true);
    }

    #[test]
    fn test_profiling_map_insert() {
        let mut profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        let key = Signature::default();
        let value = KeyedProfileResult::new(
            1,
            UuidOrSignature::Signature(key),
            None,
            ProfileResult::new(BTreeMap::new(), BTreeMap::new(), 0, None, None),
            HashMap::new(),
        );
        profiling_map.insert(key, value.clone());
        assert_eq!(profiling_map.len(), 1);
    }

    #[test]
    fn test_profiling_map_get() {
        let mut profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        let key = Signature::default();
        let value = KeyedProfileResult::new(
            1,
            UuidOrSignature::Signature(key),
            None,
            ProfileResult::new(BTreeMap::new(), BTreeMap::new(), 0, None, None),
            HashMap::new(),
        );
        profiling_map.insert(key, value.clone());

        assert_eq!(profiling_map.get(&key), Some(&value));
    }

    #[test]
    fn test_profiling_map_get_mut() {
        let mut profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        let key = Signature::default();
        let mut value = KeyedProfileResult::new(
            1,
            UuidOrSignature::Signature(key),
            None,
            ProfileResult::new(BTreeMap::new(), BTreeMap::new(), 0, None, None),
            HashMap::new(),
        );
        profiling_map.insert(key, value.clone());
        assert_eq!(profiling_map.get_mut(&key), Some(&mut value));
    }

    #[test]
    fn test_profiling_map_contains_key() {
        let mut profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        let key = Signature::default();
        let value = KeyedProfileResult::new(
            1,
            UuidOrSignature::Signature(key),
            None,
            ProfileResult::new(BTreeMap::new(), BTreeMap::new(), 0, None, None),
            HashMap::new(),
        );
        profiling_map.insert(key, value.clone());

        assert_eq!(profiling_map.contains_key(&key), true);
    }

    #[test]
    fn test_profiling_map_iter() {
        let mut profiling_map = FifoMap::<Signature, KeyedProfileResult>::new(10);
        let key = Signature::default();
        let value = KeyedProfileResult::new(
            1,
            UuidOrSignature::Signature(key),
            None,
            ProfileResult::new(BTreeMap::new(), BTreeMap::new(), 0, None, None),
            HashMap::new(),
        );
        profiling_map.insert(key, value.clone());

        assert_eq!(profiling_map.iter().count(), 1);
    }

    #[test]
    fn test_profiling_map_evicts_oldest_on_overflow() {
        let mut profiling_map = FifoMap::<String, u32>::new(10);
        profiling_map.insert("a".to_string(), 1);
        profiling_map.insert("b".to_string(), 2);
        profiling_map.insert("c".to_string(), 3);
        profiling_map.insert("d".to_string(), 4);
        profiling_map.insert("e".to_string(), 5);
        profiling_map.insert("f".to_string(), 6);
        profiling_map.insert("g".to_string(), 7);
        profiling_map.insert("h".to_string(), 8);
        profiling_map.insert("i".to_string(), 9);
        profiling_map.insert("j".to_string(), 10);

        println!("Profiling map: {:?}", profiling_map);
        println!("Profile Map capacity: {:?}", profiling_map.capacity());
        println!("Profile Map len: {:?}", profiling_map.len());

        assert_eq!(profiling_map.len(), 10);

        // Now insert one more, which should evict the oldest
        profiling_map.insert("k".to_string(), 11);
        assert_eq!(profiling_map.len(), 10);
        assert_eq!(profiling_map.get(&"a".to_string()), None);
        assert_eq!(profiling_map.get(&"k".to_string()), Some(&11));
    }

    #[test]
    fn test_profiling_map_update_do_not_reorder() {
        let mut profiling_map = FifoMap::<&str, u32>::new(4);
        profiling_map.insert("a", 1);
        profiling_map.insert("b", 2);
        profiling_map.insert("c", 3);
        profiling_map.insert("d", 4);

        //update b, should not reorder (order remains a:1,b:2,c:3,d:4)
        println!("Profiling map: {:?}", profiling_map);
        println!("Profile Map key b holds: {:?}", profiling_map.get(&"b"));
        profiling_map.insert("b", 4);
        println!("Profile Map key b holds: {:?}", profiling_map.get(&"b"));

        //overflow with a new key, should evict the oldest (a)
        profiling_map.insert("e", 5);
        assert_eq!(profiling_map.len(), 4);
        assert_eq!(profiling_map.get(&"a"), None);
        assert_eq!(profiling_map.get(&"b"), Some(&4));
        assert_eq!(profiling_map.get(&"e"), Some(&5));

        let get: Vec<_> = profiling_map.iter().map(|(k, v)| (*k, *v)).collect();
        println!("Profiling map: {:?}", get);
        assert_eq!(get, vec![("b", 4), ("c", 3), ("d", 4), ("e", 5)]);
    }

    #[test]
    fn test_export_snapshot_scope_serialization() {
        // Test Network variant
        let network_config = ExportSnapshotConfig {
            include_parsed_accounts: None,
            filter: None,
            scope: ExportSnapshotScope::Network,
        };
        let network_json = serde_json::to_value(&network_config).unwrap();
        println!(
            "Network config: {}",
            serde_json::to_string_pretty(&network_json).unwrap()
        );
        assert_eq!(network_json["scope"], json!("network"));

        // Test PreTransaction variant
        let pre_tx_config = ExportSnapshotConfig {
            include_parsed_accounts: None,
            filter: None,
            scope: ExportSnapshotScope::PreTransaction("5signature123".to_string()),
        };
        let pre_tx_json = serde_json::to_value(&pre_tx_config).unwrap();
        println!(
            "PreTransaction config: {}",
            serde_json::to_string_pretty(&pre_tx_json).unwrap()
        );
        assert_eq!(
            pre_tx_json["scope"],
            json!({"preTransaction": "5signature123"})
        );

        // Test deserialization
        let deserialized_network: ExportSnapshotConfig =
            serde_json::from_value(network_json).unwrap();
        assert_eq!(deserialized_network.scope, ExportSnapshotScope::Network);

        let deserialized_pre_tx: ExportSnapshotConfig =
            serde_json::from_value(pre_tx_json).unwrap();
        assert_eq!(
            deserialized_pre_tx.scope,
            ExportSnapshotScope::PreTransaction("5signature123".to_string())
        );
    }

    #[test]
    fn test_sanitize_datasource_url_strips_path_and_query() {
        // API key in path should be stripped
        let config = SimnetConfig {
            remote_rpc_url: Some(
                "https://example.rpc-provider.com/v2/abc123def456ghi789".to_string(),
            ),
            ..Default::default()
        };
        let sanitized = config.get_sanitized_datasource_url().unwrap();
        assert_eq!(sanitized, "https://example.rpc-provider.com");
        assert!(!sanitized.contains("abc123"));
    }

    #[test]
    fn test_sanitize_datasource_url_strips_query_params() {
        let config = SimnetConfig {
            remote_rpc_url: Some(
                "https://mainnet.helius-rpc.com/?api-key=secret-key-12345".to_string(),
            ),
            ..Default::default()
        };
        let sanitized = config.get_sanitized_datasource_url().unwrap();
        assert_eq!(sanitized, "https://mainnet.helius-rpc.com");
        assert!(!sanitized.contains("secret-key"));
    }

    #[test]
    fn test_sanitize_datasource_url_public_rpc() {
        let config = SimnetConfig {
            remote_rpc_url: Some("https://api.mainnet-beta.solana.com".to_string()),
            ..Default::default()
        };
        let sanitized = config.get_sanitized_datasource_url().unwrap();
        assert_eq!(sanitized, "https://api.mainnet-beta.solana.com");
    }

    #[test]
    fn test_sanitize_datasource_url_none() {
        let config = SimnetConfig {
            remote_rpc_url: None,
            ..Default::default()
        };
        assert!(config.get_sanitized_datasource_url().is_none());
    }

    #[test]
    fn test_sanitize_datasource_url_invalid() {
        let config = SimnetConfig {
            remote_rpc_url: Some("not-a-valid-url".to_string()),
            ..Default::default()
        };
        assert!(config.get_sanitized_datasource_url().is_none());
    }
}
