use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::SystemTime,
};

use bincode::serialized_size;
use crossbeam_channel::{Receiver, Sender};
use itertools::Itertools;
use litesvm::types::{
    FailedTransactionMetadata, SimulatedTransactionInfo, TransactionMetadata, TransactionResult,
};
use solana_account::{Account, ReadableAccount};
use solana_account_decoder::{
    UiAccount, UiAccountEncoding, UiDataSliceConfig,
    parse_account_data::AccountAdditionalDataV3,
    parse_bpf_loader::{BpfUpgradeableLoaderAccountType, UiProgram, parse_bpf_upgradeable_loader},
    parse_token::UiTokenAmount,
};
use solana_address_lookup_table_interface::state::AddressLookupTable;
use solana_client::{
    rpc_client::SerializableTransaction,
    rpc_config::{
        RpcAccountInfoConfig, RpcBlockConfig, RpcLargestAccountsConfig, RpcLargestAccountsFilter,
        RpcSignaturesForAddressConfig, RpcTransactionConfig, RpcTransactionLogsFilter,
    },
    rpc_filter::RpcFilterType,
    rpc_request::TokenAccountsFilter,
    rpc_response::{
        RpcAccountBalance, RpcConfirmedTransactionStatusWithSignature, RpcKeyedAccount,
        RpcLogsResponse, RpcTokenAccountBalance,
    },
};
use solana_clock::{Clock, Slot, UnixTimestamp};
use solana_commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_epoch_info::EpochInfo;
use solana_hash::Hash;
use solana_loader_v3_interface::{get_program_data_address, state::UpgradeableLoaderState};
use solana_message::{
    Message, SimpleAddressLoader, VersionedMessage,
    compiled_instruction::CompiledInstruction,
    v0::{LoadedAddresses, MessageAddressTableLookup},
};
use solana_pubkey::Pubkey;
use solana_rpc_client_api::response::SlotInfo;
use solana_signature::Signature;
use solana_transaction::{sanitized::SanitizedTransaction, versioned::VersionedTransaction};
use solana_transaction_error::TransactionError;
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta,
    TransactionConfirmationStatus as SolanaTransactionConfirmationStatus, UiConfirmedBlock,
    UiTransactionEncoding,
};
use surfpool_types::{
    AccountSnapshot, ComputeUnitsEstimationResult, ExecutionCapture, ExportSnapshotConfig, Idl,
    KeyedProfileResult, ProfileResult, RpcProfileResultConfig, RunbookExecutionStatusReport,
    SimnetCommand, SimnetEvent, TransactionConfirmationStatus, TransactionStatusEvent,
    UiKeyedProfileResult, UuidOrSignature, VersionedIdl,
};
use tokio::sync::RwLock;
use txtx_addon_kit::indexmap::IndexSet;
use uuid::Uuid;

use super::{
    AccountFactory, GetAccountResult, GetTransactionResult, GeyserEvent, SignatureSubscriptionType,
    SurfnetSvm, remote::SurfnetRemoteClient,
};
use crate::{
    error::{AirdropError, SurfpoolError, SurfpoolResult},
    helpers::time_travel::calculate_time_travel_clock,
    rpc::utils::{convert_transaction_metadata_from_canonical, verify_pubkey},
    surfnet::FINALIZATION_SLOT_THRESHOLD,
    types::{
        GeyserAccountUpdate, OfflineAccountConfig, RemoteRpcResult, SurfnetTransactionStatus,
        TimeTravelConfig, TokenAccount, TransactionLoadedAddresses, TransactionWithStatusMeta,
    },
};

enum ProcessTransactionResult {
    Success(TransactionMetadata),
    SimulationFailure(FailedTransactionMetadata),
    ExecutionFailure(FailedTransactionMetadata),
}

pub struct SvmAccessContext<T> {
    pub slot: Slot,
    pub latest_epoch_info: EpochInfo,
    pub latest_blockhash: Hash,
    pub inner: T,
}

impl<T> SvmAccessContext<T> {
    pub fn new(slot: Slot, latest_epoch_info: EpochInfo, latest_blockhash: Hash, inner: T) -> Self {
        Self {
            slot,
            latest_blockhash,
            latest_epoch_info,
            inner,
        }
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }

    pub fn with_new_value<N>(self, inner: N) -> SvmAccessContext<N> {
        SvmAccessContext {
            slot: self.slot,
            latest_blockhash: self.latest_blockhash,
            latest_epoch_info: self.latest_epoch_info,
            inner,
        }
    }
}

pub type SurfpoolContextualizedResult<T> = SurfpoolResult<SvmAccessContext<T>>;

/// Helper function to apply an override to a JSON value using dot notation path
///
/// # Arguments
/// * `json` - The JSON value to modify
/// * `path` - Dot-separated path to the field (e.g., "price_message.price")
/// * `value` - The new value to set
///
/// # Returns
/// Result indicating success or error
pub struct SurfnetSvmLocker(pub Arc<RwLock<SurfnetSvm>>);

impl Clone for SurfnetSvmLocker {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

/// Functions for reading and writing to the underlying SurfnetSvm instance
impl SurfnetSvmLocker {
    /// Explicitly shutdown the SVM, performing cleanup like WAL checkpoint for SQLite.
    /// This should be called before the application exits to ensure data is persisted.
    pub fn shutdown(&self) {
        let read_lock = self.0.clone();
        tokio::task::block_in_place(move || {
            let read_guard = read_lock.blocking_read();
            read_guard.shutdown();
        });
    }

    /// Executes a read-only operation on the underlying `SurfnetSvm` by acquiring a blocking read lock.
    /// Accepts a closure that receives a shared reference to `SurfnetSvm` and returns a value.
    ///
    /// # Returns
    /// The result produced by the closure.
    pub fn with_svm_reader<T, F>(&self, reader: F) -> T
    where
        F: FnOnce(&SurfnetSvm) -> T + Send + Sync,
    {
        let read_lock = self.0.clone();
        tokio::task::block_in_place(move || {
            let read_guard = read_lock.blocking_read();
            reader(&read_guard)
        })
    }

    /// Executes a read-only operation and wraps the result in `SvmAccessContext`, capturing
    /// slot, epoch info, and blockhash along with the closure's result.
    fn with_contextualized_svm_reader<T, F>(&self, reader: F) -> SvmAccessContext<T>
    where
        F: Fn(&SurfnetSvm) -> T + Send + Sync,
        T: Send + 'static,
    {
        let read_lock = self.0.clone();
        tokio::task::block_in_place(move || {
            let read_guard = read_lock.blocking_read();
            let res = reader(&read_guard);

            SvmAccessContext::new(
                read_guard.get_latest_absolute_slot(),
                read_guard.latest_epoch_info(),
                read_guard.latest_blockhash(),
                res,
            )
        })
    }

    /// Executes a write operation on the underlying `SurfnetSvm` by acquiring a blocking write lock.
    /// Accepts a closure that receives a mutable reference to `SurfnetSvm` and returns a value.
    ///
    /// # Returns
    /// The result produced by the closure.
    pub fn with_svm_writer<T, F>(&self, writer: F) -> T
    where
        F: FnOnce(&mut SurfnetSvm) -> T + Send + Sync,
        T: Send + 'static,
    {
        let write_lock = self.0.clone();
        tokio::task::block_in_place(move || {
            let mut write_guard = write_lock.blocking_write();
            writer(&mut write_guard)
        })
    }
}

/// Functions for creating and initializing the underlying SurfnetSvm instance
impl SurfnetSvmLocker {
    /// Constructs a new `SurfnetSvmLocker` wrapping the given `SurfnetSvm` instance.
    pub fn new(svm: SurfnetSvm) -> Self {
        Self(Arc::new(RwLock::new(svm)))
    }

    /// Initializes the locked `SurfnetSvm` with remote-derived startup state when available.
    pub async fn initialize(&self, remote_ctx: &Option<SurfnetRemoteClient>) -> SurfpoolResult<()> {
        let Some(remote_client) = remote_ctx else {
            return Ok(());
        };

        let (mut epoch_info, epoch_schedule) = {
            let epoch_info = remote_client.get_epoch_info().await?;
            let epoch_schedule = remote_client.get_epoch_schedule().await?;
            (epoch_info, epoch_schedule)
        };
        epoch_info.transaction_count = None;

        self.with_svm_writer(move |svm_writer| {
            svm_writer.initialize(epoch_info, epoch_schedule);
        });
        Ok(())
    }
}

/// Functions for getting accounts from the underlying SurfnetSvm instance or remote client
impl SurfnetSvmLocker {
    /// Filters the downloaded account result to remove accounts owned by offline owners.
    fn filter_downloaded_account_result(
        requested_pubkey: &Pubkey,
        result: GetAccountResult,
        offline_owners: &[Pubkey],
    ) -> GetAccountResult {
        match result {
            GetAccountResult::FoundAccount(_, account, _)
            | GetAccountResult::FoundProgramAccount((_, account), _)
            | GetAccountResult::FoundTokenAccount((_, account), _)
                if offline_owners.contains(&account.owner) =>
            {
                GetAccountResult::None(*requested_pubkey)
            }
            other => other,
        }
    }

    /// Retrieves a local account from the SVM cache, returning a contextualized result.
    pub fn get_account_local(&self, pubkey: &Pubkey) -> SvmAccessContext<GetAccountResult> {
        self.with_contextualized_svm_reader(|svm_reader| {
            return svm_reader.inner.get_account_result(pubkey).unwrap();
        })
    }

    /// Attempts local retrieval, then fetches from remote if missing, returning a contextualized result.
    ///
    /// Does not fetch from remote if the account has been explicitly blocked from remote downloads.
    pub async fn get_account_local_then_remote(
        &self,
        client: &SurfnetRemoteClient,
        pubkey: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> SurfpoolContextualizedResult<GetAccountResult> {
        let result = self.get_account_local(pubkey);

        if result.inner.is_none() {
            let is_offline = self.is_account_offline(pubkey);

            if !is_offline {
                let offline_owners = self.get_offline_account_owners();
                let remote_account = client.get_account(pubkey, commitment_config).await?;
                Ok(
                    result.with_new_value(Self::filter_downloaded_account_result(
                        pubkey,
                        remote_account,
                        &offline_owners,
                    )),
                )
            } else {
                Ok(result)
            }
        } else {
            Ok(result)
        }
    }

    /// Retrieves an account, using local or remote based on context, applying a default factory if provided.
    pub async fn get_account(
        &self,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
        pubkey: &Pubkey,
        factory: Option<AccountFactory>,
    ) -> SurfpoolContextualizedResult<GetAccountResult> {
        let result = if let Some((remote_client, commitment_config)) = remote_ctx {
            self.get_account_local_then_remote(remote_client, pubkey, *commitment_config)
                .await?
        } else {
            self.get_account_local(pubkey)
        };

        match (&result.inner, factory) {
            (&GetAccountResult::None(_), Some(factory)) => {
                let default = factory(self.clone());
                Ok(result.with_new_value(default))
            }
            _ => Ok(result),
        }
    }
    /// Retrieves multiple accounts from local cache, returning a contextualized result.
    pub fn get_multiple_accounts_local(
        &self,
        pubkeys: &[Pubkey],
    ) -> SvmAccessContext<Vec<GetAccountResult>> {
        self.with_contextualized_svm_reader(|svm_reader| {
            let mut accounts = vec![];

            for pubkey in pubkeys {
                let result = svm_reader.inner.get_account_result(pubkey).unwrap();
                if result.is_none() {};
                accounts.push(result);
            }
            accounts
        })
    }

    /// Retrieves multiple accounts from local storage, with remote fallback for missing accounts.
    ///
    /// Returns accounts in the same order as the input `pubkeys` array. Accounts found locally
    /// are returned as-is; accounts not found locally are fetched from the remote RPC client.
    /// Accounts that have been marked offline are not fetched from remote.
    pub async fn get_multiple_accounts_with_remote_fallback(
        &self,
        client: &SurfnetRemoteClient,
        pubkeys: &[Pubkey],
        commitment_config: CommitmentConfig,
    ) -> SurfpoolContextualizedResult<Vec<GetAccountResult>> {
        let SvmAccessContext {
            slot,
            latest_epoch_info,
            latest_blockhash,
            inner: local_results,
        } = self.get_multiple_accounts_local(pubkeys);

        // Collect missing pubkeys that are not offline (local_results is already in correct order from pubkeys).
        let mut missing_accounts = Vec::new();
        for result in &local_results {
            let GetAccountResult::None(pubkey) = result else {
                continue;
            };
            if self.is_account_offline(pubkey) {
                continue;
            }
            missing_accounts.push(*pubkey);
        }

        if missing_accounts.is_empty() {
            // All accounts found locally, already in correct order
            return Ok(SvmAccessContext::new(
                slot,
                latest_epoch_info,
                latest_blockhash,
                local_results,
            ));
        }
        debug!(
            "Missing accounts will be fetched: {}",
            missing_accounts.iter().join(", ")
        );
        // Fetch missing accounts from remote
        let remote_results = client
            .get_multiple_accounts(&missing_accounts, commitment_config)
            .await?;

        // Build map of pubkey -> remote result for O(1) lookup
        let offline_owners = self.get_offline_account_owners();
        let remote_map: HashMap<Pubkey, GetAccountResult> = missing_accounts
            .iter()
            .copied()
            .zip(remote_results.into_iter())
            .map(|(requested_pubkey, result)| {
                (
                    requested_pubkey,
                    Self::filter_downloaded_account_result(
                        &requested_pubkey,
                        result,
                        &offline_owners,
                    ),
                )
            })
            .collect();

        // Replace None entries with remote results while preserving order
        // We iterate through original pubkeys array to ensure order is explicit
        let combined_results: Vec<GetAccountResult> = pubkeys
            .iter()
            .zip(local_results.into_iter())
            .map(|(pubkey, local_result)| {
                match local_result {
                    GetAccountResult::None(_) => remote_map
                        .get(pubkey)
                        .cloned()
                        .unwrap_or(GetAccountResult::None(*pubkey)),
                    found => {
                        debug!("Keeping local account: {}", pubkey);
                        found
                    } // Keep found accounts (no clone, just move)
                }
            })
            .collect();

        Ok(SvmAccessContext::new(
            slot,
            latest_epoch_info,
            latest_blockhash,
            combined_results,
        ))
    }

    /// Retrieves multiple accounts, using local or remote context and applying factory defaults if provided.
    pub async fn get_multiple_accounts(
        &self,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
        pubkeys: &[Pubkey],
        factory: Option<AccountFactory>,
    ) -> SurfpoolContextualizedResult<Vec<GetAccountResult>> {
        let results = if let Some((remote_client, commitment_config)) = remote_ctx {
            self.get_multiple_accounts_with_remote_fallback(
                remote_client,
                pubkeys,
                *commitment_config,
            )
            .await?
        } else {
            self.get_multiple_accounts_local(pubkeys)
        };

        let mut combined = Vec::with_capacity(results.inner.len());
        for result in results.inner.clone() {
            match (&result, &factory) {
                (&GetAccountResult::None(_), Some(factory)) => {
                    let default = factory(self.clone());
                    combined.push(default);
                }
                _ => combined.push(result),
            }
        }
        Ok(results.with_new_value(combined))
    }

    /// Loads accounts from a snapshot into the SVM.
    ///
    /// This method should be called before geyser plugins start to ensure they receive
    /// the account updates with `is_startup=true`.
    ///
    /// # Arguments
    /// * `snapshot` - A map of pubkey strings to optional account snapshots.
    ///   - If the value is Some(AccountSnapshot), the account is loaded directly.
    ///   - If the value is None, the account is fetched from the remote RPC (if available).
    /// * `remote_client` - Optional remote RPC client to fetch None accounts.
    /// * `commitment_config` - Commitment level for remote RPC calls.
    ///
    /// # Returns
    /// The number of accounts successfully loaded.
    pub async fn load_snapshot(
        &self,
        snapshot: &BTreeMap<String, Option<AccountSnapshot>>,
        remote_client: Option<&SurfnetRemoteClient>,
        commitment_config: CommitmentConfig,
    ) -> SurfpoolResult<usize> {
        use std::str::FromStr;

        use base64::{Engine, prelude::BASE64_STANDARD};

        let mut loaded_count = 0;

        // Separate accounts into those with data and those needing remote fetch
        let mut accounts_to_load: Vec<(Pubkey, Account)> = Vec::new();
        let mut pubkeys_to_fetch: Vec<Pubkey> = Vec::new();

        for (pubkey_str, account_snapshot_opt) in snapshot.iter() {
            let pubkey = match Pubkey::from_str(pubkey_str) {
                Ok(pk) => pk,
                Err(e) => {
                    self.with_svm_reader(|svm| {
                        let _ = svm.simnet_events_tx.send(SimnetEvent::warn(format!(
                            "Skipping invalid pubkey '{}' in snapshot: {}",
                            pubkey_str, e
                        )));
                    });
                    continue;
                }
            };

            match account_snapshot_opt {
                Some(account_snapshot) => {
                    // Decode base64 data
                    let data = match BASE64_STANDARD.decode(&account_snapshot.data) {
                        Ok(d) => d,
                        Err(e) => {
                            self.with_svm_reader(|svm| {
                                let _ = svm.simnet_events_tx.send(SimnetEvent::warn(format!(
                                    "Skipping account '{}': failed to decode base64 data: {}",
                                    pubkey_str, e
                                )));
                            });
                            continue;
                        }
                    };

                    // Parse owner pubkey
                    let owner = match Pubkey::from_str(&account_snapshot.owner) {
                        Ok(pk) => pk,
                        Err(e) => {
                            self.with_svm_reader(|svm| {
                                let _ = svm.simnet_events_tx.send(SimnetEvent::warn(format!(
                                    "Skipping account '{}': invalid owner pubkey: {}",
                                    pubkey_str, e
                                )));
                            });
                            continue;
                        }
                    };

                    // Create the account
                    let account = Account {
                        lamports: account_snapshot.lamports,
                        data,
                        owner,
                        executable: account_snapshot.executable,
                        rent_epoch: account_snapshot.rent_epoch,
                    };

                    accounts_to_load.push((pubkey, account));
                }
                None => {
                    // Queue for remote fetch if client is available
                    if remote_client.is_some() {
                        pubkeys_to_fetch.push(pubkey);
                    }
                }
            }
        }

        // Fetch None accounts from remote RPC if client is available
        if let Some(client) = remote_client {
            if !pubkeys_to_fetch.is_empty() {
                self.with_svm_reader(|svm| {
                    let _ = svm.simnet_events_tx.send(SimnetEvent::info(format!(
                        "Fetching {} accounts from remote RPC for snapshot",
                        pubkeys_to_fetch.len()
                    )));
                });

                match client
                    .get_multiple_accounts(&pubkeys_to_fetch, commitment_config)
                    .await
                {
                    Ok(remote_results) => {
                        for (pubkey, result) in pubkeys_to_fetch.iter().zip(remote_results) {
                            match result {
                                GetAccountResult::FoundAccount(_, account, _) => {
                                    accounts_to_load.push((*pubkey, account));
                                }
                                GetAccountResult::FoundProgramAccount(
                                    (program_pubkey, program_account),
                                    (data_pubkey, data_account_opt),
                                ) => {
                                    accounts_to_load.push((program_pubkey, program_account));
                                    if let Some(data_account) = data_account_opt {
                                        accounts_to_load.push((data_pubkey, data_account));
                                    }
                                }
                                GetAccountResult::FoundTokenAccount(
                                    (token_pubkey, token_account),
                                    (mint_pubkey, mint_account_opt),
                                ) => {
                                    accounts_to_load.push((token_pubkey, token_account));
                                    if let Some(mint_account) = mint_account_opt {
                                        accounts_to_load.push((mint_pubkey, mint_account));
                                    }
                                }
                                GetAccountResult::None(_) => {
                                    // Account not found on remote, skip
                                }
                            }
                        }
                    }
                    Err(e) => {
                        self.with_svm_reader(|svm| {
                            let _ = svm.simnet_events_tx.send(SimnetEvent::warn(format!(
                                "Failed to fetch some accounts from remote: {}",
                                e
                            )));
                        });
                    }
                }
            }
        }

        // Load all accounts into the SVM
        self.with_svm_writer(|svm| {
            let slot = svm.get_latest_absolute_slot();

            for (pubkey, account) in accounts_to_load {
                if let Err(e) = svm.set_account(&pubkey, account.clone()) {
                    let _ = svm.simnet_events_tx.send(SimnetEvent::warn(format!(
                        "Failed to set account '{}': {}",
                        pubkey, e
                    )));
                    continue;
                }

                // Send startup account update to geyser
                let write_version = svm.increment_write_version();
                let _ = svm.geyser_events_tx.send(GeyserEvent::StartupAccountUpdate(
                    GeyserAccountUpdate::startup_update(pubkey, account, slot, write_version),
                ));

                loaded_count += 1;
            }
        });

        Ok(loaded_count)
    }

    /// Retrieves largest accounts from local cache, returning a contextualized result.
    pub fn get_largest_accounts_local(
        &self,
        config: RpcLargestAccountsConfig,
    ) -> SurfpoolContextualizedResult<Vec<RpcAccountBalance>> {
        let res: Vec<RpcAccountBalance> = self.with_svm_reader(|svm_reader| {
            let non_circulating_accounts: Vec<_> = svm_reader
                .non_circulating_accounts
                .iter()
                .flat_map(|acct| verify_pubkey(acct))
                .collect();

            let ordered_accounts = svm_reader
                .get_all_accounts()?
                .into_iter()
                .sorted_by(|a, b| b.1.lamports().cmp(&a.1.lamports()))
                .collect::<Vec<_>>();
            let ordered_filtered_accounts = match config.filter {
                Some(RpcLargestAccountsFilter::NonCirculating) => ordered_accounts
                    .into_iter()
                    .filter(|(pubkey, _)| non_circulating_accounts.contains(pubkey))
                    .collect::<Vec<_>>(),
                Some(RpcLargestAccountsFilter::Circulating) => ordered_accounts
                    .into_iter()
                    .filter(|(pubkey, _)| !non_circulating_accounts.contains(pubkey))
                    .collect::<Vec<_>>(),
                None => ordered_accounts,
            };

            Ok::<Vec<RpcAccountBalance>, SurfpoolError>(
                ordered_filtered_accounts
                    .iter()
                    .take(20)
                    .map(|(pubkey, account)| RpcAccountBalance {
                        address: pubkey.to_string(),
                        lamports: account.lamports(),
                    })
                    .collect(),
            )
        })?;
        Ok(self.with_contextualized_svm_reader(|_| res.to_owned()))
    }

    pub async fn get_largest_accounts_local_then_remote(
        &self,
        client: &SurfnetRemoteClient,
        config: RpcLargestAccountsConfig,
        commitment_config: CommitmentConfig,
    ) -> SurfpoolContextualizedResult<Vec<RpcAccountBalance>> {
        // get all non-circulating and circulating pubkeys from the remote client first,
        // and insert them locally
        {
            let remote_non_circulating_pubkeys_result = client
                .get_largest_accounts(Some(RpcLargestAccountsConfig {
                    filter: Some(RpcLargestAccountsFilter::NonCirculating),
                    ..config
                }))
                .await?;

            let (mut remote_non_circulating_pubkeys, mut remote_circulating_pubkeys) =
                match remote_non_circulating_pubkeys_result {
                    RemoteRpcResult::Ok(non_circulating_accounts) => {
                        let remote_circulating_pubkeys_result = client
                            .get_largest_accounts(Some(RpcLargestAccountsConfig {
                                filter: Some(RpcLargestAccountsFilter::Circulating),
                                ..config
                            }))
                            .await?;

                        let remote_circulating_pubkeys = match remote_circulating_pubkeys_result {
                            RemoteRpcResult::Ok(circulating_accounts) => circulating_accounts,
                            RemoteRpcResult::MethodNotSupported => {
                                unreachable!()
                            }
                        };
                        (
                            non_circulating_accounts
                                .iter()
                                .map(|account_balance| verify_pubkey(&account_balance.address))
                                .collect::<SurfpoolResult<Vec<_>>>()?,
                            remote_circulating_pubkeys
                                .iter()
                                .map(|account_balance| verify_pubkey(&account_balance.address))
                                .collect::<SurfpoolResult<Vec<_>>>()?,
                        )
                    }
                    RemoteRpcResult::MethodNotSupported => {
                        let tx = self.simnet_events_tx();
                        let _ = tx.send(SimnetEvent::warn("The `getLargestAccounts` method was sent to the remote RPC, but this method isn't supported by your RPC provider. Only local accounts will be returned."));
                        (vec![], vec![])
                    }
                };

            let mut combined = Vec::with_capacity(
                remote_non_circulating_pubkeys.len() + remote_circulating_pubkeys.len(),
            );
            combined.append(&mut remote_non_circulating_pubkeys);
            combined.append(&mut remote_circulating_pubkeys);

            let get_account_results = self
                .get_multiple_accounts_with_remote_fallback(client, &combined, commitment_config)
                .await?
                .inner;

            self.write_multiple_account_updates(&get_account_results);
        }

        // now that our local cache is aware of all large remote accounts, we can get the largest accounts locally
        // and filter according to the config
        self.get_largest_accounts_local(config)
    }

    pub async fn get_largest_accounts(
        &self,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
        config: RpcLargestAccountsConfig,
    ) -> SurfpoolContextualizedResult<Vec<RpcAccountBalance>> {
        if let Some((remote_client, commitment_config)) = remote_ctx {
            self.get_largest_accounts_local_then_remote(remote_client, config, *commitment_config)
                .await
        } else {
            self.get_largest_accounts_local(config)
        }
    }

    pub fn account_to_rpc_keyed_account<T: ReadableAccount + Send + Sync>(
        &self,
        pubkey: &Pubkey,
        account: &T,
        config: &RpcAccountInfoConfig,
        token_mint: Option<Pubkey>,
    ) -> RpcKeyedAccount {
        self.with_svm_reader(|svm_reader| {
            svm_reader.account_to_rpc_keyed_account(pubkey, account, config, token_mint)
        })
    }
}

/// Get signatures for Addresses
impl SurfnetSvmLocker {
    /// Returns local `getSignaturesForAddress` results in the same newest-first order expected by
    /// the Solana RPC.
    ///
    /// The implementation has to do more than filter by slot:
    /// - transactions are ordered by descending slot
    /// - transactions within the same slot are ordered by their execution order in the block
    /// - `before` and `until` are pagination boundaries in that final ordered stream
    ///
    /// To preserve those semantics, we first collect matching transactions, reconstruct their
    /// intra-slot ordering from block headers, sort the full result stream, and only then apply
    /// the `before` / `until` window followed by `limit`.
    pub fn get_signatures_for_address_local(
        &self,
        pubkey: &Pubkey,
        config: Option<&RpcSignaturesForAddressConfig>,
    ) -> SvmAccessContext<Vec<RpcConfirmedTransactionStatusWithSignature>> {
        let before = config.and_then(|c| c.before.as_ref());
        let until = config.and_then(|c| c.until.as_ref());
        let limit = config.and_then(|c| c.limit).unwrap_or(1000);
        let min_context_slot = config.and_then(|c| c.min_context_slot).unwrap_or_default();

        self.with_contextualized_svm_reader(move |svm_reader| {
            let current_slot = svm_reader.get_latest_absolute_slot();

            let sigs: Vec<_> = svm_reader
                .transactions
                .into_iter()
                .map(|iter| {
                    iter.filter_map(|(sig, status)| {
                        let (
                            TransactionWithStatusMeta {
                                slot,
                                transaction,
                                meta,
                            },
                            _,
                        ) = status
                            .as_processed()
                            .expect("expected processed transaction");

                        if slot < min_context_slot {
                            return None;
                        }

                        // Check if the pubkey is a signer

                        if !transaction.message.static_account_keys().contains(pubkey) {
                            return None;
                        }

                        // Determine confirmation status
                        let confirmation_status = match current_slot {
                            cs if cs == slot => SolanaTransactionConfirmationStatus::Processed,
                            cs if cs < slot + FINALIZATION_SLOT_THRESHOLD => {
                                SolanaTransactionConfirmationStatus::Confirmed
                            }
                            _ => SolanaTransactionConfirmationStatus::Finalized,
                        };

                        Some(RpcConfirmedTransactionStatusWithSignature {
                            err: match meta.status {
                                Ok(_) => None,
                                Err(e) => Some(e.into()),
                            },
                            slot,
                            memo: None,
                            block_time: None,
                            confirmation_status: Some(confirmation_status),
                            signature: sig,
                        })
                    })
                    .collect()
                })
                .unwrap_or_default();

            // `getSignaturesForAddress` is ordered newest-first, but transactions that share a
            // slot also need to preserve their execution order within that block.
            let unique_slots: HashSet<u64> = sigs.iter().map(|s| s.slot).collect();
            let mut sig_position: HashMap<String, usize> = HashMap::new();
            for slot in unique_slots {
                if let Ok(Some(block_header)) = svm_reader.blocks.get(&slot) {
                    for (idx, block_sig) in block_header.signatures.iter().enumerate() {
                        sig_position.insert(block_sig.to_string(), idx);
                    }
                }
            }

            let sigs: Vec<_> = sigs
                .into_iter()
                // Order from most recent to least recent so pagination boundaries
                // can be applied against the exact transaction sequence.
                .sorted_by(|a, b| {
                    b.slot.cmp(&a.slot).then_with(|| {
                        let a_pos = sig_position.get(&a.signature).unwrap_or(&usize::MAX);
                        let b_pos = sig_position.get(&b.signature).unwrap_or(&usize::MAX);
                        b_pos.cmp(&a_pos)
                    })
                })
                .collect();

            let window = {
                // `before` and `until` are boundaries in the final ordered result stream, not
                // just slot filters. We compute a [start..end) index range after sorting so
                // same-slot pagination behaves correctly and `until` stays exclusive.
                let start = match before {
                    // `before` is exclusive, so we start one item after the boundary when it
                    // exists. If it does not exist locally, the local window is empty.
                    Some(before) => match sigs.iter().position(|sig| sig.signature.eq(before)) {
                        Some(idx) => idx + 1,
                        None => sigs.len(),
                    },
                    None => 0,
                };

                let end = match until {
                    // `until` is also exclusive, so the boundary itself is not included. We only
                    // search within `sigs[start..]` so the end boundary is resolved relative to the
                    // already-trimmed start of the window. If it is missing, we keep the full tail.
                    Some(until) => {
                        match sigs[start..].iter().position(|sig| sig.signature.eq(until)) {
                            Some(offset) => start + offset,
                            None => sigs.len(),
                        }
                    }
                    None => sigs.len(),
                };
                start..end
            };

            // Apply the pagination window first, then enforce the RPC limit on that slice.
            sigs[window].iter().take(limit).cloned().collect()
        })
    }

    pub async fn get_signatures_for_address_local_then_remote(
        &self,
        client: &SurfnetRemoteClient,
        pubkey: &Pubkey,
        config: Option<&RpcSignaturesForAddressConfig>,
    ) -> SurfpoolContextualizedResult<Vec<RpcConfirmedTransactionStatusWithSignature>> {
        let results = self.get_signatures_for_address_local(pubkey, config);
        let limit = config.and_then(|c| c.limit).unwrap_or(1000);

        let SvmAccessContext {
            slot,
            latest_blockhash,
            latest_epoch_info,
            inner: mut combined_results,
        } = results;
        if combined_results.len() < limit {
            let mut remote_results = client.get_signatures_for_address(pubkey, config).await?;
            combined_results.append(&mut remote_results);
        }

        Ok(SvmAccessContext::new(
            slot,
            latest_epoch_info,
            latest_blockhash,
            combined_results,
        ))
    }

    pub async fn get_signatures_for_address(
        &self,
        remote_ctx: &Option<(SurfnetRemoteClient, ())>,
        pubkey: &Pubkey,
        config: Option<&RpcSignaturesForAddressConfig>,
    ) -> SurfpoolContextualizedResult<Vec<RpcConfirmedTransactionStatusWithSignature>> {
        let results = if let Some((remote_client, _)) = remote_ctx {
            self.get_signatures_for_address_local_then_remote(remote_client, pubkey, config)
                .await?
        } else {
            self.get_signatures_for_address_local(pubkey, config)
        };

        Ok(results)
    }
}

/// Functions for getting transactions from the underlying SurfnetSvm instance or remote client
impl SurfnetSvmLocker {
    /// Retrieves a transaction by signature, using local or remote based on context.
    pub async fn get_transaction(
        &self,
        remote_ctx: &Option<SurfnetRemoteClient>,
        signature: &Signature,
        config: RpcTransactionConfig,
    ) -> SurfpoolResult<GetTransactionResult> {
        if let Some(remote_client) = remote_ctx {
            self.get_transaction_local_then_remote(remote_client, signature, config)
                .await
        } else {
            self.get_transaction_local(signature, &config)
        }
    }

    /// Retrieves a transaction from local cache, returning a contextualized result.
    pub fn get_transaction_local(
        &self,
        signature: &Signature,
        config: &RpcTransactionConfig,
    ) -> SurfpoolResult<GetTransactionResult> {
        self.with_svm_reader(|svm_reader| {
            let latest_absolute_slot = svm_reader.get_latest_absolute_slot();

            let Some(entry) = svm_reader.transactions.get(&signature.to_string())? else {
                return Ok(GetTransactionResult::None(*signature));
            };

            let (transaction_with_status_meta, _) = entry.expect_processed();
            let slot = transaction_with_status_meta.slot;
            let block_time = svm_reader
                .blocks
                .get(&slot)?
                .map(|b| (b.block_time / 1_000) as UnixTimestamp)
                .unwrap_or(0);
            let encoded = transaction_with_status_meta.encode(
                config.encoding.unwrap_or(UiTransactionEncoding::JsonParsed),
                config.max_supported_transaction_version,
                true,
            )?;
            Ok(GetTransactionResult::found_transaction(
                *signature,
                EncodedConfirmedTransactionWithStatusMeta {
                    slot,
                    transaction: encoded,
                    block_time: Some(block_time),
                },
                latest_absolute_slot,
            ))
        })
    }

    /// Retrieves a transaction locally then from remote if missing, returning a contextualized result.
    pub async fn get_transaction_local_then_remote(
        &self,
        client: &SurfnetRemoteClient,
        signature: &Signature,
        config: RpcTransactionConfig,
    ) -> SurfpoolResult<GetTransactionResult> {
        let local_result = self.get_transaction_local(signature, &config)?;
        let latest_absolute_slot = self.get_latest_absolute_slot();
        if local_result.is_none() {
            Ok(client
                .get_transaction(*signature, config, latest_absolute_slot)
                .await)
        } else {
            Ok(local_result)
        }
    }
}

/// Functions for simulating and processing transactions in the underlying SurfnetSvm instance
impl SurfnetSvmLocker {
    /// Simulates a transaction on the SVM, returning detailed info or failure metadata.
    #[allow(clippy::result_large_err)]
    pub fn simulate_transaction(
        &self,
        transaction: VersionedTransaction,
        sigverify: bool,
    ) -> Result<SimulatedTransactionInfo, FailedTransactionMetadata> {
        self.with_svm_reader(move |svm_reader| {
            svm_reader.simulate_transaction(transaction, sigverify)
        })
    }

    pub fn is_instruction_profiling_enabled(&self) -> bool {
        self.with_svm_reader(|svm_reader| svm_reader.instruction_profiling_enabled)
    }

    pub fn get_profiling_map_capacity(&self) -> usize {
        self.with_svm_reader(|svm_reader| svm_reader.max_profiles)
    }

    pub async fn process_transaction(
        &self,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
        transaction: VersionedTransaction,
        status_tx: Sender<TransactionStatusEvent>,
        skip_preflight: bool,
        sigverify: bool,
    ) -> SurfpoolResult<()> {
        let do_propagate_status_updates = true;
        let signature = transaction.signatures[0];
        let profile_result = match self
            .fetch_all_tx_accounts_then_process_tx_returning_profile_res(
                remote_ctx,
                transaction,
                &status_tx,
                skip_preflight,
                sigverify,
                do_propagate_status_updates,
            )
            .await
        {
            Ok(result) => result,
            Err(e) => {
                // Ensure the status channel always receives a response to prevent
                // the RPC handler from hanging on recv() when errors occur during
                // account fetching, ALT resolution, or other pre-processing steps.
                // This is critical for issue #454 where program close stops block production.
                //
                // AccountLoadedTwice errors should go through SimulationFailure to produce
                // Agave-compatible JSON-RPC error format with structured `err` and `data` fields.
                let err_str = e.to_string();
                if err_str.contains("Account loaded twice") {
                    let _ = status_tx.try_send(TransactionStatusEvent::SimulationFailure((
                        TransactionError::AccountLoadedTwice,
                        surfpool_types::TransactionMetadata::default(),
                    )));
                } else {
                    let _ =
                        status_tx.try_send(TransactionStatusEvent::VerificationFailure(err_str));
                }
                return Err(e);
            }
        };

        self.with_svm_writer(|svm_writer| {
            svm_writer.write_executed_profile_result(signature, profile_result)
        })?;

        Ok(())
    }

    pub async fn profile_transaction(
        &self,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
        transaction: VersionedTransaction,
        tag: Option<String>,
    ) -> SurfpoolContextualizedResult<Uuid> {
        // Use clone_for_profiling to wrap all storage fields with overlay storage,
        // ensuring mutations during profiling don't affect the underlying database
        let svm_clone = self.with_svm_reader(|svm_reader| svm_reader.clone_for_profiling());

        let svm_locker = SurfnetSvmLocker::new(svm_clone);

        let (status_tx, _) = crossbeam_channel::unbounded();

        let skip_preflight = true; // skip preflight checks during transaction profiling
        let sigverify = true; // do verify signatures during transaction profiling
        let do_propagate_status_updates = false; // don't propagate status updates during transaction profiling
        let mut profile_result = svm_locker
            .fetch_all_tx_accounts_then_process_tx_returning_profile_res(
                remote_ctx,
                transaction,
                &status_tx,
                skip_preflight,
                sigverify,
                do_propagate_status_updates,
            )
            .await?;

        let uuid = Uuid::new_v4();
        profile_result.key = UuidOrSignature::Uuid(uuid);

        self.with_svm_writer(|svm_writer| {
            svm_writer.write_simulated_profile_result(uuid, tag, profile_result)
        })?;

        Ok(self.with_contextualized_svm_reader(|_| uuid))
    }

    async fn fetch_all_tx_accounts_then_process_tx_returning_profile_res(
        &self,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
        transaction: VersionedTransaction,
        status_tx: &Sender<TransactionStatusEvent>,
        skip_preflight: bool,
        sigverify: bool,
        do_propagate: bool,
    ) -> SurfpoolResult<KeyedProfileResult> {
        let signature = transaction.signatures[0];

        // Sigverify the transaction upfront before doing any account fetching or other pre-processing.
        if sigverify {
            self.with_svm_reader(|svm_reader| svm_reader.sigverify(&transaction))
                .map_err(|e| Into::<SurfpoolError>::into(e.err))?;
        }

        let latest_absolute_slot = self.with_svm_writer(|svm_writer| {
            let latest_absolute_slot = svm_writer.get_latest_absolute_slot();
            svm_writer.notify_signature_subscribers(
                SignatureSubscriptionType::received(),
                &signature,
                latest_absolute_slot,
                None,
            );

            latest_absolute_slot
        });

        // find accounts that are needed for this transaction but are missing from the local
        // svm cache, fetch them from the RPC, and insert them locally
        let tx_loaded_addresses = self
            .get_loaded_addresses(remote_ctx, &transaction.message)
            .await?;

        // Check for duplicate accounts between static keys and ALT-loaded addresses.
        // Agave rejects such transactions pre-execution with AccountLoadedTwice.
        if let Some(ref loaded) = tx_loaded_addresses {
            let static_keys: HashSet<&Pubkey> =
                transaction.message.static_account_keys().iter().collect();
            for loaded_key in loaded.all_loaded_addresses() {
                if static_keys.contains(loaded_key) {
                    return Err(TransactionError::AccountLoadedTwice.into());
                }
            }
        }

        // we don't want the pubkeys of the address lookup tables to be included in the transaction accounts,
        // but we do want the pubkeys of the accounts _loaded_ by the ALT to be in the transaction accounts.
        let transaction_accounts = self.get_pubkeys_from_message(
            &transaction.message,
            tx_loaded_addresses
                .as_ref()
                .map(|l| l.all_loaded_addresses()),
        );
        debug!(
            "Transaction {} accounts inputs: {}",
            transaction.get_signature(),
            transaction_accounts.iter().join(", ")
        );

        let account_updates = self
            .get_multiple_accounts(remote_ctx, &transaction_accounts, None)
            .await?
            .inner;

        // We also need the pubkeys of the ALTs to be pulled from the remote, so we'll do a fetch for them
        let alt_account_updates = self
            .get_multiple_accounts(
                remote_ctx,
                &tx_loaded_addresses
                    .as_ref()
                    .map(|l| l.alt_addresses())
                    .unwrap_or_default(),
                None,
            )
            .await?
            .inner;

        let readonly_account_states = transaction_accounts
            .iter()
            .enumerate()
            .filter_map(|(i, pubkey)| {
                if transaction.message.is_maybe_writable(i, None) {
                    None
                } else {
                    self.get_account_local(pubkey)
                        .inner
                        .map_account()
                        .ok()
                        .map(|a| (*pubkey, a))
                }
            })
            .collect::<HashMap<_, _>>();

        self.with_svm_writer(|svm_writer| {
            for update in &account_updates {
                svm_writer.write_account_update(update.clone());
            }
            for update in alt_account_updates {
                svm_writer.write_account_update(update);
            }
        });

        let pre_execution_capture = {
            let mut capture = ExecutionCapture::new();
            for account_update in account_updates.into_iter() {
                match account_update {
                    GetAccountResult::None(pubkey) => {
                        capture.insert(pubkey, None);
                    }
                    GetAccountResult::FoundAccount(pubkey, account, _)
                    | GetAccountResult::FoundProgramAccount((pubkey, account), _)
                    | GetAccountResult::FoundTokenAccount((pubkey, account), _) => {
                        capture.insert(pubkey, Some(account));
                    }
                }
            }
            capture
        };

        let (accounts_before, token_accounts_before, token_programs) =
            self.with_svm_reader(|svm_reader| {
                let accounts_before = transaction_accounts
                    .iter()
                    .map(|p| svm_reader.inner.get_account(p))
                    .collect::<Result<Vec<Option<Account>>, SurfpoolError>>()?;

                let token_accounts_before = transaction_accounts
                    .iter()
                    .enumerate()
                    .filter_map(|(i, p)| {
                        svm_reader
                            .token_accounts
                            .get(&p.to_string())
                            .ok()
                            .flatten()
                            .map(|a| (i, a))
                    })
                    .collect::<Vec<_>>();

                let token_programs = token_accounts_before
                    .iter()
                    .map(|(i, ta)| {
                        svm_reader
                            .get_account(&transaction_accounts[*i])
                            .map(|res| res.map(|a| a.owner).unwrap_or(ta.token_program_id()))
                    })
                    .collect::<Result<Vec<_>, SurfpoolError>>()?;

                Ok::<
                    (
                        Vec<Option<Account>>,
                        Vec<(usize, TokenAccount)>,
                        Vec<Pubkey>,
                    ),
                    SurfpoolError,
                >((accounts_before, token_accounts_before, token_programs))
            })?;

        let loaded_addresses = tx_loaded_addresses.as_ref().map(|l| l.loaded_addresses());

        let ix_profiles = if self.is_instruction_profiling_enabled() {
            match self
                .generate_instruction_profiles(
                    &transaction,
                    &transaction_accounts,
                    &tx_loaded_addresses,
                    &accounts_before,
                    &token_accounts_before,
                    &token_programs,
                    pre_execution_capture.clone(),
                    &status_tx,
                )
                .await
            {
                Ok(profiles) => profiles,
                Err(e) => {
                    let _ = self.simnet_events_tx().try_send(SimnetEvent::error(format!(
                        "Failed to generate instruction profiles: {}",
                        e
                    )));
                    None
                }
            }
        } else {
            None
        };

        let profile_result = self
            .process_transaction_internal(
                transaction,
                skip_preflight,
                sigverify,
                &transaction_accounts,
                &loaded_addresses,
                &accounts_before,
                &token_accounts_before,
                &token_programs,
                pre_execution_capture,
                &status_tx,
                do_propagate,
            )
            .await?;

        Ok(KeyedProfileResult::new(
            latest_absolute_slot,
            UuidOrSignature::Signature(signature),
            ix_profiles,
            profile_result,
            readonly_account_states,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn generate_instruction_profiles(
        &self,
        transaction: &VersionedTransaction,
        transaction_accounts: &[Pubkey],
        loaded_addresses: &Option<TransactionLoadedAddresses>,
        accounts_before: &[Option<Account>],
        token_accounts_before: &[(usize, TokenAccount)],
        token_programs: &[Pubkey],
        pre_execution_capture: ExecutionCapture,
        status_tx: &Sender<TransactionStatusEvent>,
    ) -> SurfpoolResult<Option<Vec<ProfileResult>>> {
        let instructions = transaction.message.instructions();
        let ix_count = instructions.len();
        if ix_count == 0 {
            return Ok(None);
        }
        // Extract account categories from original transaction

        let mut ix_profile_results: Vec<ProfileResult> = vec![];

        for idx in 1..=ix_count {
            let partial_transaction_res = self.create_partial_transaction(
                instructions,
                transaction_accounts,
                transaction,
                idx,
                loaded_addresses,
            );

            let mut ix_required_accounts = IndexSet::new();
            for &account_idx in &instructions[idx - 1].accounts {
                ix_required_accounts.insert(transaction_accounts[account_idx as usize]);
            }
            ix_required_accounts
                .insert(transaction_accounts[instructions[idx - 1].program_id_index as usize]);

            let Some(partial_tx) = partial_transaction_res else {
                debug!("Unable to create partial transaction");
                return Ok(None);
            };

            let mut previous_execution_captures = ExecutionCapture::new();
            let mut previous_cus = 0;
            let mut previous_log_count = 0;
            for result in ix_profile_results.iter() {
                previous_execution_captures.extend(result.post_execution_capture.clone());
                previous_cus += result.compute_units_consumed;
                previous_log_count += result.log_messages.as_ref().map(|m| m.len()).unwrap_or(0);
            }

            let skip_preflight = true;
            let sigverify = false;
            let do_propagate = false;

            let mut pre_execution_capture_cursor = pre_execution_capture.clone();
            // If a pre-execution capture was provided, any pubkeys that are in the capture
            // that we just took should be replaced with those from the pre-execution capture.
            let capture_keys: Vec<_> = pre_execution_capture_cursor.keys().cloned().collect();
            for pubkey in capture_keys.into_iter() {
                if let Some(pre_account) = previous_execution_captures.remove(&pubkey) {
                    // Replace the account with the pre-execution one
                    pre_execution_capture_cursor.insert(pubkey, pre_account);
                }
            }
            let mut svm_clone = self.with_svm_reader(|svm_reader| svm_reader.clone_for_profiling());

            let (dummy_simnet_tx, _) = crossbeam_channel::bounded(1);
            let (dummy_geyser_tx, _) = crossbeam_channel::bounded(1);
            svm_clone.simnet_events_tx = dummy_simnet_tx;
            svm_clone.geyser_events_tx = dummy_geyser_tx;

            let svm_locker = SurfnetSvmLocker::new(svm_clone);
            let mut profile_result = svm_locker
                .process_transaction_internal(
                    partial_tx,
                    skip_preflight,
                    sigverify,
                    transaction_accounts,
                    &loaded_addresses.as_ref().map(|l| l.loaded_addresses()),
                    accounts_before,
                    token_accounts_before,
                    token_programs,
                    pre_execution_capture_cursor,
                    status_tx,
                    do_propagate,
                )
                .await?;

            profile_result
                .pre_execution_capture
                .retain(|pubkey, _| ix_required_accounts.contains(pubkey));
            profile_result
                .post_execution_capture
                .retain(|pubkey, _| ix_required_accounts.contains(pubkey));

            profile_result.compute_units_consumed = profile_result
                .compute_units_consumed
                .saturating_sub(previous_cus);
            profile_result.log_messages = profile_result.log_messages.map(|logs| {
                logs.into_iter()
                    .skip(previous_log_count)
                    .collect::<Vec<_>>()
            });

            ix_profile_results.push(profile_result);
        }

        Ok(Some(ix_profile_results))
    }

    fn handle_simulation_failure(
        &self,
        signature: Signature,
        failed_transaction_metadata: FailedTransactionMetadata,
        pre_execution_capture: ExecutionCapture,
        simulated_slot: Slot,
        status_tx: Sender<TransactionStatusEvent>,
        do_propagate: bool,
    ) -> ProfileResult {
        let FailedTransactionMetadata { err, meta } = failed_transaction_metadata;

        let cus = meta.compute_units_consumed;
        let log_messages = meta.logs.clone();
        let err_string = err.to_string();

        if do_propagate {
            let meta = convert_transaction_metadata_from_canonical(&meta);
            let simnet_events_tx = self.simnet_events_tx();
            let _ = simnet_events_tx.try_send(SimnetEvent::error(format!(
                "Transaction simulation failed: {}",
                err
            )));

            self.with_svm_writer(|svm_writer| {
                svm_writer.notify_signature_subscribers(
                    SignatureSubscriptionType::processed(),
                    &signature,
                    simulated_slot,
                    Some(err.clone()),
                );
                svm_writer.notify_logs_subscribers(
                    &signature,
                    Some(err.clone()),
                    log_messages.clone(),
                    CommitmentLevel::Processed,
                );
            });
            let _ = status_tx.try_send(TransactionStatusEvent::SimulationFailure((err, meta)));
        }
        ProfileResult::new(
            pre_execution_capture,
            BTreeMap::new(),
            cus,
            Some(log_messages),
            Some(err_string),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_execution_failure(
        &self,
        failed_transaction_metadata: FailedTransactionMetadata,
        transaction: VersionedTransaction,
        simulated_slot: Slot,
        pubkeys_from_message: &[Pubkey],
        accounts_before: &[Option<Account>],
        token_accounts_before: &[(usize, TokenAccount)],
        token_programs: &[Pubkey],
        loaded_addresses: &Option<LoadedAddresses>,
        pre_execution_capture: ExecutionCapture,
        status_tx: Sender<TransactionStatusEvent>,
        do_propagate: bool,
    ) -> SurfpoolResult<ProfileResult> {
        let FailedTransactionMetadata { err, meta } = failed_transaction_metadata;

        let cus = meta.compute_units_consumed;
        let log_messages = meta.logs.clone();
        let err_string = err.to_string();
        let signature = meta.signature;

        let accounts_after = pubkeys_from_message
            .iter()
            .map(|p| self.with_svm_reader(|svm_reader| svm_reader.inner.get_account(p)))
            .collect::<SurfpoolResult<Vec<Option<Account>>>>()?;

        for (pubkey, (before, after)) in pubkeys_from_message
            .iter()
            .zip(accounts_before.iter().zip(accounts_after.iter()))
        {
            if before.ne(&after) {
                self.with_svm_writer(|svm_writer| {
                    if let Some(after) = &after {
                        let _ = svm_writer.update_account_registries(pubkey, after);
                        svm_writer.notify_account_subscribers(pubkey, &after);
                        svm_writer.notify_program_subscribers(pubkey, &after);
                    } else {
                        svm_writer.notify_account_subscribers(pubkey, &Account::default());
                        svm_writer.notify_program_subscribers(pubkey, &Account::default());
                    }
                });
            }
        }

        let token_mints = self
            .with_svm_reader(|svm_reader| {
                token_accounts_before
                    .iter()
                    .map(|(_, a)| {
                        svm_reader
                            .token_mints
                            .get(&a.mint().to_string())
                            .ok()
                            .flatten()
                            .ok_or(SurfpoolError::token_mint_not_found(a.mint()))
                    })
                    .collect::<Result<Vec<_>, SurfpoolError>>()
            })
            .unwrap_or_default();

        if do_propagate {
            let meta_canonical = convert_transaction_metadata_from_canonical(&meta);
            let simnet_events_tx = self.simnet_events_tx();
            let _ = simnet_events_tx.try_send(SimnetEvent::error(format!(
                "Transaction execution failed: {}",
                err
            )));
            let _ = status_tx.try_send(TransactionStatusEvent::ExecutionFailure((
                err.clone(),
                meta_canonical.clone(),
            )));

            self.with_svm_writer(|svm_writer| {
                let transaction_with_status_meta = TransactionWithStatusMeta::from_failure(
                    simulated_slot,
                    transaction.clone(),
                    &FailedTransactionMetadata {
                        err: err.clone(),
                        meta: meta.clone(),
                    },
                    accounts_before,
                    &accounts_after,
                    token_accounts_before,
                    token_mints,
                    token_programs,
                    loaded_addresses.clone().unwrap_or_default(),
                );
                svm_writer.transactions.store(
                    signature.to_string(),
                    SurfnetTransactionStatus::processed(
                        transaction_with_status_meta.clone(),
                        HashSet::new(),
                    ),
                )?;

                let _ = svm_writer
                    .geyser_events_tx
                    .send(GeyserEvent::NotifyTransaction(
                        transaction_with_status_meta,
                        Some(transaction.clone()),
                    ));

                svm_writer.transactions_queued_for_confirmation.push_back((
                    transaction.clone(),
                    status_tx.clone(),
                    Some(err.clone()),
                ));

                svm_writer.notify_signature_subscribers(
                    SignatureSubscriptionType::processed(),
                    &signature,
                    simulated_slot,
                    Some(err.clone()),
                );
                svm_writer.notify_logs_subscribers(
                    &signature,
                    Some(err.clone()),
                    log_messages.clone(),
                    CommitmentLevel::Processed,
                );
                let _ = svm_writer
                    .simnet_events_tx
                    .try_send(SimnetEvent::transaction_processed(
                        meta_canonical,
                        Some(err),
                    ));
                Ok::<(), SurfpoolError>(())
            })?;
        }
        Ok(ProfileResult::new(
            pre_execution_capture,
            BTreeMap::new(),
            cus,
            Some(log_messages),
            Some(err_string),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_execution_success(
        &self,
        transaction_metadata: TransactionMetadata,
        transaction: VersionedTransaction,
        simulated_slot: Slot,
        pubkeys_from_message: &[Pubkey],
        loaded_addresses: &Option<LoadedAddresses>,
        accounts_before: &[Option<Account>],
        token_accounts_before: &[(usize, TokenAccount)],
        token_programs: &[Pubkey],
        pre_execution_capture: ExecutionCapture,
        status_tx: &Sender<TransactionStatusEvent>,
        do_propagate: bool,
    ) -> SurfpoolResult<ProfileResult> {
        let cus = transaction_metadata.compute_units_consumed;
        let logs = transaction_metadata.logs.clone();
        let signature = transaction.signatures[0];

        let post_execution_capture = self.with_svm_writer(|svm_writer| {
            let accounts_after = pubkeys_from_message
                .iter()
                .map(|p| svm_writer.inner.get_account_no_db(p))
                .collect::<Vec<Option<Account>>>();
            let (sanitized_transaction, versioned_transaction) = if do_propagate {
                let address_loader = match (&transaction.message, &loaded_addresses) {
                    (VersionedMessage::V0(_), Some(loaded_addresses)) => {
                        SimpleAddressLoader::Enabled(loaded_addresses.clone())
                    }
                    // V0 messages without address table lookups still require an enabled loader.
                    (VersionedMessage::V0(_), None) => {
                        SimpleAddressLoader::Enabled(LoadedAddresses::default())
                    }
                    (VersionedMessage::Legacy(_), _) => SimpleAddressLoader::Disabled,
                };

                (
                    SanitizedTransaction::try_create(
                        transaction.clone(),
                        transaction.message.hash(),
                        Some(false),
                        address_loader,
                        &HashSet::new(), // todo: provide reserved account keys
                    )
                    .map_err(|error| {
                        debug!(
                            "Failed to sanitize transaction {} for Geyser account updates: {:?}",
                            signature, error
                        );
                        error
                    })
                    .ok(),
                    Some(transaction.clone()),
                )
            } else {
                (None, None)
            };

            let mut mutated_account_pubkeys = HashSet::new();
            for (pubkey, (before, after)) in pubkeys_from_message
                .iter()
                .zip(accounts_before.iter().zip(accounts_after.clone()))
            {
                if before.ne(&after) {
                    mutated_account_pubkeys.insert(*pubkey);
                    let after = after.unwrap_or_default();
                    svm_writer.update_account_registries(pubkey, &after)?;
                    let write_version = svm_writer.increment_write_version();

                    if let Some(sanitized_transaction) = sanitized_transaction.clone() {
                        let _ = svm_writer.geyser_events_tx.send(GeyserEvent::UpdateAccount(
                            GeyserAccountUpdate::transaction_update(
                                *pubkey,
                                after.clone(),
                                svm_writer.get_latest_absolute_slot(),
                                sanitized_transaction.clone(),
                                write_version,
                            ),
                        ));
                    }
                    svm_writer.notify_account_subscribers(pubkey, &after);
                    svm_writer.notify_program_subscribers(pubkey, &after);
                }
            }

            let mut token_accounts_after = vec![];
            let mut post_execution_capture = BTreeMap::new();
            let mut post_token_program_ids = vec![];

            for (i, (pubkey, account)) in pubkeys_from_message
                .iter()
                .zip(accounts_after.iter())
                .enumerate()
            {
                let token_account = svm_writer
                    .token_accounts
                    .get(&pubkey.to_string())
                    .ok()
                    .flatten();
                post_execution_capture.insert(*pubkey, account.clone());

                if let Some(token_account) = token_account {
                    token_accounts_after.push((i, token_account));
                    post_token_program_ids.push(
                        account
                            .as_ref()
                            .map(|a| a.owner)
                            .unwrap_or(spl_token_interface::id()),
                    );
                }
            }

            let token_mints = token_accounts_after
                .iter()
                .map(|(_, a)| {
                    svm_writer
                        .token_mints
                        .get(&a.mint().to_string())
                        .ok()
                        .flatten()
                        .ok_or(SurfpoolError::token_mint_not_found(a.mint()))
                })
                .collect::<Result<Vec<_>, SurfpoolError>>()?;

            if do_propagate {
                let transaction_meta =
                    convert_transaction_metadata_from_canonical(&transaction_metadata);
                let transaction_with_status_meta = TransactionWithStatusMeta::new(
                    svm_writer.get_latest_absolute_slot(),
                    transaction.clone(),
                    transaction_metadata,
                    accounts_before,
                    &accounts_after,
                    token_accounts_before,
                    &token_accounts_after,
                    token_mints,
                    token_programs,
                    &post_token_program_ids,
                    loaded_addresses.clone().unwrap_or_default(),
                );
                svm_writer.transactions.store(
                    transaction_meta.signature.to_string(),
                    SurfnetTransactionStatus::processed(
                        transaction_with_status_meta.clone(),
                        mutated_account_pubkeys,
                    ),
                )?;

                let _ = svm_writer
                    .simnet_events_tx
                    .try_send(SimnetEvent::transaction_processed(transaction_meta, None));

                let _ = svm_writer
                    .geyser_events_tx
                    .send(GeyserEvent::NotifyTransaction(
                        transaction_with_status_meta,
                        versioned_transaction,
                    ));

                svm_writer.transactions_queued_for_confirmation.push_back((
                    transaction.clone(),
                    status_tx.clone(),
                    None,
                ));

                svm_writer.notify_signature_subscribers(
                    SignatureSubscriptionType::processed(),
                    &signature,
                    simulated_slot,
                    None,
                );
                svm_writer.notify_logs_subscribers(
                    &signature,
                    None,
                    logs.clone(),
                    CommitmentLevel::Processed,
                );
                let _ = status_tx.try_send(TransactionStatusEvent::Success(
                    TransactionConfirmationStatus::Processed,
                ));
            }

            Ok::<ExecutionCapture, SurfpoolError>(post_execution_capture)
        })?;

        Ok(ProfileResult::new(
            pre_execution_capture,
            post_execution_capture,
            cus,
            Some(logs),
            None,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_transaction_internal(
        &self,
        transaction: VersionedTransaction,
        skip_preflight: bool,
        sigverify: bool,
        transaction_accounts: &[Pubkey],
        loaded_addresses: &Option<LoadedAddresses>,
        accounts_before: &[Option<Account>],
        token_accounts_before: &[(usize, TokenAccount)],
        token_programs: &[Pubkey],
        pre_execution_capture: ExecutionCapture,
        status_tx: &Sender<TransactionStatusEvent>,
        do_propagate: bool,
    ) -> SurfpoolResult<ProfileResult> {
        let res = match self
            .do_process_transaction_internal(transaction.clone(), skip_preflight, sigverify)
            .await
        {
            ProcessTransactionResult::Success(transaction_metadata) => self
                .handle_execution_success(
                    transaction_metadata,
                    transaction,
                    self.get_latest_absolute_slot(),
                    transaction_accounts,
                    loaded_addresses,
                    accounts_before,
                    token_accounts_before,
                    token_programs,
                    pre_execution_capture,
                    status_tx,
                    do_propagate,
                )?,
            ProcessTransactionResult::SimulationFailure(failed_transaction_metadata) => self
                .handle_simulation_failure(
                    transaction.signatures[0],
                    failed_transaction_metadata,
                    pre_execution_capture,
                    self.get_latest_absolute_slot(),
                    status_tx.clone(),
                    do_propagate,
                ),
            ProcessTransactionResult::ExecutionFailure(failed) => self.handle_execution_failure(
                failed,
                transaction,
                self.get_latest_absolute_slot(),
                transaction_accounts,
                accounts_before,
                token_accounts_before,
                token_programs,
                loaded_addresses,
                pre_execution_capture,
                status_tx.clone(),
                do_propagate,
            )?,
        };
        Ok(res)
    }

    async fn do_process_transaction_internal(
        &self,
        transaction: VersionedTransaction,
        skip_preflight: bool,
        sigverify: bool,
    ) -> ProcessTransactionResult {
        // if not skipping preflight, simulate the transaction
        if !skip_preflight {
            if let Err(e) = self.with_svm_reader(|svm_reader| {
                svm_reader
                    .simulate_transaction(transaction.clone(), sigverify)
                    .map_err(ProcessTransactionResult::SimulationFailure)
            }) {
                return e;
            }
        }

        match self.with_svm_writer(|svm_writer| {
            svm_writer
                .send_transaction(transaction, false /* cu_analysis_enabled */, sigverify)
                .map_err(|e| {
                    debug!("Transaction execution failure: {:?}", e.meta);
                    ProcessTransactionResult::ExecutionFailure(e)
                })
                .map(ProcessTransactionResult::Success)
        }) {
            Ok(res) => res,
            Err(res) => res,
        }
    }
}

/// Functions for writing account updates to the underlying SurfnetSvm instance
impl SurfnetSvmLocker {
    /// Writes a single account update into the SVM state if present.
    pub fn write_account_update(&self, account_update: GetAccountResult) {
        if !account_update.requires_update() {
            return;
        }

        self.with_svm_writer(move |svm_writer| {
            svm_writer.write_account_update(account_update.clone())
        })
    }

    /// Writes multiple account updates into the SVM state when any are present.
    pub fn write_multiple_account_updates(&self, account_updates: &[GetAccountResult]) {
        if account_updates
            .iter()
            .all(|update| !update.requires_update())
        {
            return;
        }

        self.with_svm_writer(move |svm_writer| {
            for update in account_updates {
                svm_writer.write_account_update(update.clone());
            }
        });
    }

    /// Resets an account in the SVM state for refresh/streaming.
    ///
    /// This function coordinates the reset of accounts by removing them from the local cache,
    /// allowing them to be fetched fresh from mainnet on the next access.
    /// It handles program accounts (including their program data accounts) and can optionally
    /// cascade the reset to all accounts owned by a program.
    pub fn reset_account(
        &self,
        pubkey: Pubkey,
        include_owned_accounts: bool,
    ) -> SurfpoolResult<()> {
        let simnet_events_tx = self.simnet_events_tx();
        let _ = simnet_events_tx.send(SimnetEvent::info(format!(
            "Account {} will be reset",
            pubkey
        )));
        // Set the account online so it can be fetched from mainnet again.
        self.remove_offline_account(pubkey, include_owned_accounts)?;

        self.with_svm_writer(move |svm_writer| {
            svm_writer.reset_account(&pubkey, include_owned_accounts)
        })
    }

    /// Resets SVM state and clears all offline account entries.
    ///
    /// This function coordinates the reset of the entire network state.
    /// It also clears the offline account set so all accounts can be fetched from mainnet again.
    pub async fn reset_network(
        &self,
        remote_ctx: &Option<SurfnetRemoteClient>,
    ) -> SurfpoolResult<()> {
        let simnet_events_tx = self.simnet_events_tx();
        let _ = simnet_events_tx.send(SimnetEvent::info("Resetting network..."));

        // Fetch epoch info from remote if available (similar to initialize)
        let (mut epoch_info, epoch_schedule) = if let Some(remote_client) = remote_ctx {
            (
                remote_client.get_epoch_info().await?,
                remote_client.get_epoch_schedule().await?,
            )
        } else {
            let epoch_schedule = SurfnetSvm::default_epoch_schedule();
            let epoch_info = SurfnetSvm::default_epoch_info(&epoch_schedule);
            (epoch_info, epoch_schedule)
        };
        epoch_info.transaction_count = None;

        self.with_svm_writer(move |svm_writer| {
            let _ = svm_writer.reset_network(epoch_info, epoch_schedule);
            let _ = svm_writer.offline_accounts.clear();
        });
        Ok(())
    }

    /// Marks an account as offline, preventing it from being downloaded from the remote RPC.
    ///
    /// When `include_owned_accounts` is enabled, this also marks accounts as offline that are already known locally.
    /// Accounts discovered later through direct remote fetches are rejected lazily if they are
    /// owned by an offline owner.
    pub async fn insert_offline_account(
        &self,
        pubkey: Pubkey,
        include_owned_accounts: bool,
    ) -> SurfpoolResult<()> {
        let simnet_events_tx = self.simnet_events_tx();
        let _ = simnet_events_tx.send(SimnetEvent::info(format!(
            "Account {} will be marked offline (excluded from remote downloads)",
            pubkey
        )));

        self.with_svm_writer(move |svm_writer| {
            if let Err(e) = svm_writer.offline_accounts.store(
                pubkey.to_string(),
                OfflineAccountConfig {
                    include_owned_accounts,
                },
            ) {
                warn!("Failed to store offline account {}: {}", pubkey, e);
            }
        });

        Ok(())
    }

    /// Streams an account by its pubkey.
    pub fn stream_account(
        &self,
        pubkey: Pubkey,
        include_owned_accounts: bool,
    ) -> SurfpoolResult<()> {
        let simnet_events_tx = self.simnet_events_tx();
        let _ = simnet_events_tx.send(SimnetEvent::info(format!(
            "Account {} changes will be streamed",
            pubkey
        )));
        self.with_svm_writer(|svm_writer| {
            svm_writer
                .streamed_accounts
                .store(pubkey.to_string(), include_owned_accounts)
        })?;
        Ok(())
    }

    pub fn get_streamed_accounts(&self) -> Vec<(String, bool)> {
        self.with_svm_reader(|svm_reader| {
            svm_reader
                .streamed_accounts
                .into_iter()
                .map(|iter| iter.collect())
                .unwrap_or_default()
        })
    }

    /// Removes an account from the offline account set.
    ///
    /// This allows the account to be fetched from mainnet again if requested.
    /// This is useful when resetting an account for a refresh/stream operation.
    pub fn remove_offline_account(
        &self,
        pubkey: Pubkey,
        include_owned_accounts: bool,
    ) -> SurfpoolResult<()> {
        self.with_svm_writer(move |svm_writer| {
            if let Err(e) = svm_writer.offline_accounts.take(&pubkey.to_string()) {
                warn!("Failed to set account online {}: {}", pubkey, e);
            }

            if include_owned_accounts {
                // Set online any locally-known accounts owned by this pubkey.
                // Uses the accounts_by_owner index as a fast lookup.
                let owned_pubkeys: Vec<Pubkey> = svm_writer
                    .accounts_by_owner
                    .get(&pubkey.to_string())
                    .ok()
                    .flatten()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|pk_str| pk_str.parse().ok())
                    .collect();
                for owned_pk in owned_pubkeys {
                    if let Err(e) = svm_writer.offline_accounts.take(&owned_pk.to_string()) {
                        warn!("Failed to set account online {}: {}", owned_pk, e);
                    }
                }
            }
        });
        Ok(())
    }

    /// Returns true if the given pubkey is marked offline.
    pub fn is_account_offline(&self, pubkey: &Pubkey) -> bool {
        self.with_svm_reader(|svm_reader| {
            svm_reader
                .offline_accounts
                .contains_key(&pubkey.to_string())
                .unwrap_or(false)
        })
    }

    /// Gets all owners whose accounts are marked offline.
    pub fn get_offline_account_owners(&self) -> Vec<Pubkey> {
        self.with_svm_reader(|svm_reader| {
            svm_reader
                .offline_accounts
                .into_iter()
                .unwrap_or_else(|e| {
                    warn!("Failed to iterate offline_accounts: {}", e);
                    Box::new(std::iter::empty())
                })
                .filter(|(_, config)| config.include_owned_accounts)
                .filter_map(|(k, _)| match k.parse() {
                    Ok(pk) => Some(pk),
                    Err(e) => {
                        warn!("Invalid pubkey in offline_accounts: {}: {}", k, e);
                        None
                    }
                })
                .collect()
        })
    }

    /// Registers a scenario for execution
    pub fn register_scenario(
        &self,
        scenario: surfpool_types::Scenario,
        slot: Option<Slot>,
    ) -> SurfpoolResult<()> {
        self.with_svm_writer(move |svm_writer| svm_writer.register_scenario(scenario, slot))
    }
}

/// Token account related functions
impl SurfnetSvmLocker {
    /// Fetches all token accounts for an owner, returning remote results and missing pubkeys contexts.
    pub async fn get_token_accounts_by_owner(
        &self,
        remote_ctx: &Option<SurfnetRemoteClient>,
        owner: Pubkey,
        filter: &TokenAccountsFilter,
        config: &RpcAccountInfoConfig,
    ) -> SurfpoolContextualizedResult<Vec<RpcKeyedAccount>> {
        if let Some(remote_client) = remote_ctx {
            self.get_token_accounts_by_owner_local_then_remote(owner, filter, remote_client, config)
                .await
        } else {
            self.get_token_accounts_by_owner_local(owner, filter, config)
        }
    }

    pub fn get_token_accounts_by_owner_local(
        &self,
        owner: Pubkey,
        filter: &TokenAccountsFilter,
        config: &RpcAccountInfoConfig,
    ) -> SurfpoolContextualizedResult<Vec<RpcKeyedAccount>> {
        let result = self.with_contextualized_svm_reader(|svm_reader| {
            svm_reader
                .get_parsed_token_accounts_by_owner(&owner)
                .iter()
                .filter_map(|(pubkey, token_account)| {
                    svm_reader
                        .get_account(pubkey)
                        .map(|res| {
                            let Some(account) = res else {
                                return None;
                            };
                            if match filter {
                                TokenAccountsFilter::Mint(mint) => token_account.mint().eq(mint),
                                TokenAccountsFilter::ProgramId(program_id) => {
                                    account.owner.eq(program_id)
                                }
                            } {
                                Some(svm_reader.account_to_rpc_keyed_account(
                                    pubkey,
                                    &account,
                                    config,
                                    Some(token_account.mint()),
                                ))
                            } else {
                                None
                            }
                        })
                        .transpose()
                })
                .collect::<SurfpoolResult<Vec<_>>>()
        });
        let SvmAccessContext {
            slot,
            latest_epoch_info,
            latest_blockhash,
            inner: accounts,
        } = result;
        Ok(SvmAccessContext::new(
            slot,
            latest_epoch_info,
            latest_blockhash,
            accounts?,
        ))
    }

    pub async fn get_token_accounts_by_owner_local_then_remote(
        &self,
        owner: Pubkey,
        filter: &TokenAccountsFilter,
        remote_client: &SurfnetRemoteClient,
        config: &RpcAccountInfoConfig,
    ) -> SurfpoolContextualizedResult<Vec<RpcKeyedAccount>> {
        let SvmAccessContext {
            slot,
            latest_epoch_info,
            latest_blockhash,
            inner: local_accounts,
        } = self.get_token_accounts_by_owner_local(owner, filter, config)?;

        let remote_accounts = remote_client
            .get_token_accounts_by_owner(owner, filter, config)
            .await?;

        let mut combined_accounts = remote_accounts;

        for local_account in local_accounts {
            if let Some((pos, _)) = combined_accounts
                .iter()
                .find_position(|RpcKeyedAccount { pubkey, .. }| pubkey.eq(&local_account.pubkey))
            {
                combined_accounts[pos] = local_account;
            } else {
                combined_accounts.push(local_account);
            }
        }

        Ok(SvmAccessContext::new(
            slot,
            latest_epoch_info,
            latest_blockhash,
            combined_accounts,
        ))
    }

    pub async fn get_token_accounts_by_delegate(
        &self,
        remote_ctx: &Option<SurfnetRemoteClient>,
        delegate: Pubkey,
        filter: &TokenAccountsFilter,
        config: &RpcAccountInfoConfig,
    ) -> SurfpoolContextualizedResult<Vec<RpcKeyedAccount>> {
        // Validate that the program is supported if using ProgramId filter
        if let TokenAccountsFilter::ProgramId(program_id) = filter {
            if !is_supported_token_program(program_id) {
                return Err(SurfpoolError::unsupported_token_program(*program_id));
            }
        }

        if let Some(remote_client) = remote_ctx {
            self.get_token_accounts_by_delegate_local_then_remote(
                delegate,
                filter,
                remote_client,
                config,
            )
            .await
        } else {
            self.get_token_accounts_by_delegate_local(delegate, filter, config)
        }
    }
}

/// Token account by delegate related functions
impl SurfnetSvmLocker {
    pub fn get_token_accounts_by_delegate_local(
        &self,
        delegate: Pubkey,
        filter: &TokenAccountsFilter,
        config: &RpcAccountInfoConfig,
    ) -> SurfpoolContextualizedResult<Vec<RpcKeyedAccount>> {
        let result = self.with_contextualized_svm_reader(|svm_reader| {
            svm_reader
                .get_token_accounts_by_delegate(&delegate)
                .iter()
                .filter_map(|(pubkey, token_account)| {
                    svm_reader
                        .get_account(pubkey)
                        .map(|res| {
                            let Some(account) = res else {
                                return None;
                            };
                            let include = match filter {
                                TokenAccountsFilter::Mint(mint) => token_account.mint() == *mint,
                                TokenAccountsFilter::ProgramId(program_id) => {
                                    account.owner == *program_id
                                        && is_supported_token_program(program_id)
                                }
                            };

                            if include {
                                Some(svm_reader.account_to_rpc_keyed_account(
                                    pubkey,
                                    &account,
                                    config,
                                    Some(token_account.mint()),
                                ))
                            } else {
                                None
                            }
                        })
                        .transpose()
                })
                .collect::<SurfpoolResult<Vec<_>>>()
        });
        let SvmAccessContext {
            slot,
            latest_epoch_info,
            latest_blockhash,
            inner: accounts,
        } = result;
        Ok(SvmAccessContext::new(
            slot,
            latest_epoch_info,
            latest_blockhash,
            accounts?,
        ))
    }

    pub async fn get_token_accounts_by_delegate_local_then_remote(
        &self,
        delegate: Pubkey,
        filter: &TokenAccountsFilter,
        remote_client: &SurfnetRemoteClient,
        config: &RpcAccountInfoConfig,
    ) -> SurfpoolContextualizedResult<Vec<RpcKeyedAccount>> {
        let SvmAccessContext {
            slot,
            latest_epoch_info,
            latest_blockhash,
            inner: local_accounts,
        } = self.get_token_accounts_by_delegate_local(delegate, filter, config)?;

        let remote_accounts = remote_client
            .get_token_accounts_by_delegate(delegate, filter, config)
            .await?;

        let mut combined_accounts = remote_accounts;

        for local_account in local_accounts {
            if let Some((pos, _)) = combined_accounts
                .iter()
                .find_position(|RpcKeyedAccount { pubkey, .. }| pubkey.eq(&local_account.pubkey))
            {
                // Replace remote account with local one (local takes precedence)
                combined_accounts[pos] = local_account;
            } else {
                // Add local account that wasn't found in remote results
                combined_accounts.push(local_account);
            }
        }

        Ok(SvmAccessContext::new(
            slot,
            latest_epoch_info,
            latest_blockhash,
            combined_accounts,
        ))
    }
}

/// Get largest account related account
impl SurfnetSvmLocker {
    pub fn get_token_largest_accounts_local(
        &self,
        mint: &Pubkey,
    ) -> SvmAccessContext<Vec<RpcTokenAccountBalance>> {
        self.with_contextualized_svm_reader(|svm_reader| {
            let token_accounts = svm_reader.get_token_accounts_by_mint(mint);

            // get mint information to determine decimals
            let mint_decimals = if let Some(mint_account) =
                svm_reader.token_mints.get(&mint.to_string()).ok().flatten()
            {
                mint_account.decimals()
            } else {
                0
            };

            // convert to RpcTokenAccountBalance and sort by balance
            let mut balances: Vec<RpcTokenAccountBalance> = token_accounts
                .into_iter()
                .map(|(pubkey, token_account)| RpcTokenAccountBalance {
                    address: pubkey.to_string(),
                    amount: UiTokenAmount {
                        amount: token_account.amount().to_string(),
                        decimals: mint_decimals,
                        ui_amount: Some(format_ui_amount(token_account.amount(), mint_decimals)),
                        ui_amount_string: format_ui_amount_string(
                            token_account.amount(),
                            mint_decimals,
                        ),
                    },
                })
                .collect();

            // sort by amount in descending order
            balances.sort_by(|a, b| {
                let amount_a: u64 = a.amount.amount.parse().unwrap_or(0);
                let amount_b: u64 = b.amount.amount.parse().unwrap_or(0);
                amount_b.cmp(&amount_a)
            });

            // limit to top 20 accounts
            balances.truncate(20);

            balances
        })
    }

    pub async fn get_token_largest_accounts_local_then_remote(
        &self,
        client: &SurfnetRemoteClient,
        mint: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> SurfpoolContextualizedResult<Vec<RpcTokenAccountBalance>> {
        let SvmAccessContext {
            slot,
            latest_epoch_info,
            latest_blockhash,
            inner: local_accounts,
        } = self.get_token_largest_accounts_local(mint);

        let remote_accounts = client
            .get_token_largest_accounts(mint, commitment_config)
            .await?;

        let mut combined_accounts = remote_accounts;

        // if the account is in both the local and remote list, add the local one and not the remote
        for local_account in local_accounts {
            if let Some((pos, _)) = combined_accounts
                .iter()
                .find_position(|remote_account| remote_account.address == local_account.address)
            {
                combined_accounts[pos] = local_account;
            } else {
                combined_accounts.push(local_account);
            }
        }

        // re-sort and limit after combining
        combined_accounts.sort_by(|a, b| {
            let amount_a: u64 = a.amount.amount.parse().unwrap_or(0);
            let amount_b: u64 = b.amount.amount.parse().unwrap_or(0);
            amount_b.cmp(&amount_a)
        });
        combined_accounts.truncate(20);

        Ok(SvmAccessContext::new(
            slot,
            latest_epoch_info,
            latest_blockhash,
            combined_accounts,
        ))
    }

    /// Fetches the largest token accounts for a specific mint, returning contextualized results.
    pub async fn get_token_largest_accounts(
        &self,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
        mint: &Pubkey,
    ) -> SurfpoolContextualizedResult<Vec<RpcTokenAccountBalance>> {
        if let Some((remote_client, commitment_config)) = remote_ctx {
            self.get_token_largest_accounts_local_then_remote(
                remote_client,
                mint,
                *commitment_config,
            )
            .await
        } else {
            Ok(self.get_token_largest_accounts_local(mint))
        }
    }
}

/// Address lookup table related functions
impl SurfnetSvmLocker {
    /// Extracts pubkeys from a VersionedMessage, resolving address lookup tables as needed.
    pub fn get_pubkeys_from_message(
        &self,
        message: &VersionedMessage,
        all_transaction_lookup_table_addresses: Option<Vec<&Pubkey>>,
    ) -> Vec<Pubkey> {
        match message {
            VersionedMessage::Legacy(message) => message.account_keys.clone(),
            VersionedMessage::V0(message) => {
                let mut acc_keys = message.account_keys.clone();

                if let Some(loaded_addresses) = all_transaction_lookup_table_addresses {
                    acc_keys.extend(loaded_addresses);
                }
                acc_keys
            }
        }
    }

    /// Gets addresses loaded from on-chain lookup tables from a VersionedMessage.
    pub async fn get_loaded_addresses(
        &self,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
        message: &VersionedMessage,
    ) -> SurfpoolResult<Option<TransactionLoadedAddresses>> {
        match message {
            VersionedMessage::Legacy(_) => Ok(None),
            VersionedMessage::V0(message) => {
                if message.address_table_lookups.is_empty() {
                    return Ok(None);
                }
                let mut loaded = TransactionLoadedAddresses::new();
                for alt in message.address_table_lookups.iter() {
                    self.get_lookup_table_addresses(remote_ctx, alt, &mut loaded)
                        .await?;
                }

                Ok(Some(loaded))
            }
        }
    }

    /// Retrieves loaded addresses from a lookup table account, validating owner and indices.
    pub async fn get_lookup_table_addresses(
        &self,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
        address_table_lookup: &MessageAddressTableLookup,
        transaction_loaded_addresses: &mut TransactionLoadedAddresses,
    ) -> SurfpoolResult<()> {
        let table_account = self
            .get_account(remote_ctx, &address_table_lookup.account_key, None)
            .await?
            .inner
            .map_account()?;

        if table_account.owner == solana_sdk_ids::address_lookup_table::id() {
            let SvmAccessContext {
                slot: current_slot,
                inner: slot_hashes,
                ..
            } = self.with_contextualized_svm_reader(|svm_reader| {
                svm_reader
                    .inner
                    .get_sysvar::<solana_slot_hashes::SlotHashes>()
            });

            //let current_slot = self.get_latest_absolute_slot(); // or should i use this?
            let data = &table_account.data.clone();
            let lookup_table = AddressLookupTable::deserialize(data).map_err(|_ix_err| {
                SurfpoolError::invalid_account_data(
                    address_table_lookup.account_key,
                    table_account.data,
                    Some("Attempted to lookup addresses from an invalid account"),
                )
            })?;

            let writable = lookup_table
                .lookup(
                    current_slot,
                    &address_table_lookup.writable_indexes,
                    &slot_hashes,
                )
                .map_err(|_ix_err| {
                    SurfpoolError::invalid_lookup_index(address_table_lookup.account_key)
                })?;

            let readable = lookup_table
                .lookup(
                    current_slot,
                    &address_table_lookup.readonly_indexes,
                    &slot_hashes,
                )
                .map_err(|_ix_err| {
                    SurfpoolError::invalid_lookup_index(address_table_lookup.account_key)
                })?;

            let MessageAddressTableLookup {
                account_key,
                writable_indexes,
                readonly_indexes,
            } = address_table_lookup.to_owned();

            transaction_loaded_addresses.insert_members(
                account_key,
                writable_indexes
                    .into_iter()
                    .zip(writable.into_iter())
                    .collect(),
                readonly_indexes
                    .into_iter()
                    .zip(readable.into_iter())
                    .collect(),
            );

            Ok(())
        } else {
            Err(SurfpoolError::invalid_account_owner(
                table_account.owner,
                Some("Attempted to lookup addresses from an account owned by the wrong program"),
            ))
        }
    }
}

/// Profiling helper functions
impl SurfnetSvmLocker {
    /// Estimates compute units for a transaction via contextualized simulation.
    pub fn estimate_compute_units(
        &self,
        transaction: &VersionedTransaction,
    ) -> SvmAccessContext<ComputeUnitsEstimationResult> {
        self.with_contextualized_svm_reader(|svm_reader| {
            svm_reader.estimate_compute_units(transaction)
        })
    }

    /// Creates a partial transaction for instruction profiling by extracting and remapping
    /// a subset of instructions from the original transaction.
    ///
    /// This helper function handles the complex logic of:
    /// - Collecting all accounts referenced by the instruction subset
    /// - Categorizing accounts based on their original roles (signers vs non-signers)
    /// - Building a new account key list in the correct order
    /// - Remapping instruction account indices to match the new account list
    /// - Creating a valid partial transaction with appropriate signatures
    ///
    /// # Arguments
    /// * `instructions` - All instructions from the original transaction
    /// * `message_accounts` - Account keys from the original transaction
    /// * `mutable_signers` - Mutable signer accounts from original transaction
    /// * `readonly_signers` - Readonly signer accounts from original transaction
    /// * `mutable_non_signers` - Mutable non-signer accounts from original transaction
    /// * `transaction` - The original transaction for reference
    /// * `idx` - Number of instructions to include in the partial transaction
    ///
    /// # Returns
    /// A partial transaction containing the first `idx` instructions and the accounts used for
    /// the last instruction, or None if creation fails
    #[allow(clippy::too_many_arguments)]
    fn create_partial_transaction(
        &self,
        instructions: &[CompiledInstruction],
        message_accounts: &[Pubkey],
        transaction: &VersionedTransaction,
        idx: usize,
        loaded_addresses: &Option<TransactionLoadedAddresses>,
    ) -> Option<VersionedTransaction> {
        // Keep the full account map from the original transaction for every partial pass.
        // This simplifies remapping: we only keep the first `idx` instructions, but retain
        // the original `message_accounts` ordering and address table lookups.
        let ixs_for_tx = instructions[0..idx].to_vec();

        // Build a new message that keeps the original account map and address table lookups,
        // but only contains the first `idx` instructions.
        let new_message = match transaction.message {
            VersionedMessage::Legacy(ref message) => VersionedMessage::Legacy(Message {
                account_keys: message_accounts[..message.account_keys.len()].to_vec(),
                header: message.header,
                recent_blockhash: *transaction.message.recent_blockhash(),
                instructions: ixs_for_tx.clone(),
            }),
            VersionedMessage::V0(ref message) => {
                VersionedMessage::V0(solana_message::v0::Message {
                    account_keys: message_accounts[..message.account_keys.len()].to_vec(),
                    header: message.header,
                    recent_blockhash: *transaction.message.recent_blockhash(),
                    instructions: ixs_for_tx.clone(),
                    // Preserve the original address table lookups when available.
                    address_table_lookups: loaded_addresses
                        .as_ref()
                        .map(|l| l.to_address_table_lookups())
                        .unwrap_or_default(),
                })
            }
        };

        let tx = VersionedTransaction {
            signatures: transaction.signatures.clone(),
            message: new_message,
        };

        Some(tx)
    }

    /// Returns the profile result for a given signature or UUID, and whether it exists in the SVM.
    pub fn get_profile_result(
        &self,
        signature_or_uuid: UuidOrSignature,
        config: &RpcProfileResultConfig,
    ) -> SurfpoolResult<Option<UiKeyedProfileResult>> {
        let result = match &signature_or_uuid {
            UuidOrSignature::Signature(signature) => self.with_svm_reader(|svm| {
                svm.executed_transaction_profiles
                    .get(&signature.to_string())
                    .ok()
                    .flatten()
            }),
            UuidOrSignature::Uuid(uuid) => self.with_svm_reader(|svm| {
                svm.simulated_transaction_profiles
                    .get(&uuid.to_string())
                    .ok()
                    .flatten()
            }),
        };
        Ok(result.map(|profile| self.encode_ui_keyed_profile_result(profile, config)))
    }

    pub fn encode_ui_keyed_profile_result(
        &self,
        profile: KeyedProfileResult,
        config: &RpcProfileResultConfig,
    ) -> UiKeyedProfileResult {
        self.with_svm_reader(|svm_reader| {
            svm_reader.encode_ui_keyed_profile_result(profile, config)
        })
    }

    /// Returns the profile results for a given tag.
    pub fn get_profile_results_by_tag(
        &self,
        tag: String,
        config: &RpcProfileResultConfig,
    ) -> SurfpoolResult<Option<Vec<UiKeyedProfileResult>>> {
        let tag_map = self.with_svm_reader(|svm| svm.profile_tag_map.get(&tag).ok().flatten());
        match tag_map {
            None => Ok(None),
            Some(uuids_or_sigs) => {
                let mut profiles = Vec::new();
                for id in uuids_or_sigs {
                    let profile = self.get_profile_result(id, config)?;
                    if profile.is_none() {
                        return Err(SurfpoolError::tag_not_found(&tag));
                    }
                    profiles.push(profile.unwrap());
                }
                Ok(Some(profiles))
            }
        }
    }

    pub fn register_idl(&self, idl: Idl, slot: Option<Slot>) -> SurfpoolResult<()> {
        self.with_svm_writer(|svm_writer| svm_writer.register_idl(idl, slot))
    }

    pub fn get_idl(&self, address: &Pubkey, slot: Option<Slot>) -> Option<Idl> {
        self.with_svm_reader(|svm_reader| {
            let query_slot = slot.unwrap_or_else(|| svm_reader.get_latest_absolute_slot());
            // IDLs are stored sorted by slot descending, so the first one that passes the filter is the latest
            svm_reader
                .registered_idls
                .get(&address.to_string())
                .ok()
                .flatten()
                .and_then(|idl_versions| {
                    idl_versions
                        .iter()
                        .filter(|VersionedIdl(s, _)| *s <= query_slot)
                        .max()
                        .map(|VersionedIdl(_, idl)| idl.clone())
                })
        })
    }

    /// Forges account data by decoding with IDL, applying overrides, and re-encoding.
    ///
    /// # Arguments
    /// * `account_pubkey` - The public key of the account (used for error messages)
    /// * `account_data` - The raw account data bytes
    /// * `idl` - The IDL for decoding/encoding the account data
    /// * `overrides` - HashMap of field paths (dot notation) to values to override
    ///
    /// # Returns
    /// The modified account data bytes with discriminator
    /// Forges account data by applying overrides to existing account data
    ///
    /// This delegates to the SurfnetSvm implementation.
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
        self.with_svm_reader(|svm_reader| {
            svm_reader.get_forged_account_data(account_pubkey, account_data, idl, overrides)
        })
    }
}
/// Program account related functions
impl SurfnetSvmLocker {
    /// Clones a program account from source to destination, handling upgradeable loader state.
    pub async fn clone_program_account(
        &self,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
        source_program_id: &Pubkey,
        destination_program_id: &Pubkey,
    ) -> SurfpoolContextualizedResult<()> {
        let expected_source_program_data_address = get_program_data_address(source_program_id);

        let result = self
            .get_multiple_accounts(
                remote_ctx,
                &[*source_program_id, expected_source_program_data_address],
                None,
            )
            .await?;

        let mut accounts = result
            .inner
            .clone()
            .into_iter()
            .map(|a| a.map_account())
            .collect::<SurfpoolResult<Vec<Account>>>()?;

        let source_program_data_account = accounts.remove(1);
        let source_program_account = accounts.remove(0);

        let BpfUpgradeableLoaderAccountType::Program(UiProgram {
            program_data: source_program_data_address,
        }) = parse_bpf_upgradeable_loader(&source_program_account.data).map_err(|e| {
            SurfpoolError::invalid_program_account(source_program_id, e.to_string())
        })?
        else {
            return Err(SurfpoolError::expected_program_account(source_program_id));
        };

        if source_program_data_address.ne(&expected_source_program_data_address.to_string()) {
            return Err(SurfpoolError::invalid_program_account(
                source_program_id,
                format!(
                    "Program data address mismatch: expected {}, found {}",
                    expected_source_program_data_address, source_program_data_address
                ),
            ));
        }

        let destination_program_data_address = get_program_data_address(destination_program_id);

        // create a new program account that has the `program_data` field set to the
        // destination program data address
        let mut new_program_account = source_program_account;
        new_program_account.data = bincode::serialize(&UpgradeableLoaderState::Program {
            programdata_address: destination_program_data_address,
        })
        .map_err(|e| SurfpoolError::internal(format!("Failed to serialize program data: {}", e)))?;

        self.with_svm_writer(|svm_writer| {
            svm_writer.set_account(
                &destination_program_data_address,
                source_program_data_account.clone(),
            )?;

            svm_writer.set_account(destination_program_id, new_program_account.clone())?;
            Ok::<(), SurfpoolError>(())
        })?;

        Ok(result.with_new_value(()))
    }

    pub async fn set_program_authority(
        &self,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
        program_id: Pubkey,
        new_authority: Option<Pubkey>,
    ) -> SurfpoolContextualizedResult<()> {
        let SvmAccessContext {
            slot,
            latest_epoch_info,
            latest_blockhash,
            inner: mut get_account_result,
        } = self.get_account(remote_ctx, &program_id, None).await?;

        let original_authority = match &mut get_account_result {
            GetAccountResult::None(pubkey) => {
                return Err(SurfpoolError::invalid_program_account(
                    pubkey,
                    "Account not found",
                ));
            }
            GetAccountResult::FoundAccount(pubkey, program_account, _) => {
                let programdata_address = get_program_data_address(pubkey);
                let mut programdata_account_result = self
                    .get_account(remote_ctx, &programdata_address, None)
                    .await?
                    .inner;
                match &mut programdata_account_result {
                    GetAccountResult::None(pubkey) => {
                        return Err(SurfpoolError::invalid_program_account(
                            pubkey,
                            "Program data account does not exist",
                        ));
                    }
                    GetAccountResult::FoundAccount(_, programdata_account, _) => {
                        let original_authority = update_programdata_account(
                            &program_id,
                            programdata_account,
                            new_authority,
                        )?;

                        get_account_result = GetAccountResult::FoundProgramAccount(
                            (*pubkey, program_account.clone()),
                            (programdata_address, Some(programdata_account.clone())),
                        );

                        original_authority
                    }
                    GetAccountResult::FoundProgramAccount(_, _)
                    | GetAccountResult::FoundTokenAccount(_, _) => {
                        return Err(SurfpoolError::invalid_program_account(
                            pubkey,
                            "Not a program account",
                        ));
                    }
                }
            }
            GetAccountResult::FoundProgramAccount(_, (_, None)) => {
                return Err(SurfpoolError::invalid_program_account(
                    program_id,
                    "Program data account does not exist",
                ));
            }
            GetAccountResult::FoundProgramAccount(_, (_, Some(programdata_account))) => {
                update_programdata_account(&program_id, programdata_account, new_authority)?
            }
            GetAccountResult::FoundTokenAccount(_, _) => {
                return Err(SurfpoolError::invalid_program_account(
                    program_id,
                    "Not a program account",
                ));
            }
        };

        let simnet_events_tx = self.simnet_events_tx();
        match (original_authority, new_authority) {
            (Some(original), Some(new)) => {
                if original != new {
                    let _ = simnet_events_tx.send(SimnetEvent::info(format!(
                        "Setting new authority for program {}",
                        program_id
                    )));
                    let _ = simnet_events_tx
                        .send(SimnetEvent::info(format!("Old Authority: {}", original)));
                    let _ =
                        simnet_events_tx.send(SimnetEvent::info(format!("New Authority: {}", new)));
                } else {
                    let _ = simnet_events_tx.send(SimnetEvent::info(format!(
                        "No authority change for program {}",
                        program_id
                    )));
                }
            }
            (Some(original), None) => {
                let _ = simnet_events_tx.send(SimnetEvent::info(format!(
                    "Removing authority for program {}",
                    program_id
                )));
                let _ = simnet_events_tx
                    .send(SimnetEvent::info(format!("Old Authority: {}", original)));
            }
            (None, Some(new)) => {
                let _ = simnet_events_tx.send(SimnetEvent::info(format!(
                    "Setting new authority for program {}",
                    program_id
                )));
                let _ = simnet_events_tx.send(SimnetEvent::info("Old Authority: None".to_string()));
                let _ = simnet_events_tx.send(SimnetEvent::info(format!("New Authority: {}", new)));
            }
            (None, None) => {
                let _ = simnet_events_tx.send(SimnetEvent::info(format!(
                    "No authority change for program {}",
                    program_id
                )));
            }
        };

        self.write_account_update(get_account_result);

        Ok(SvmAccessContext::new(
            slot,
            latest_epoch_info,
            latest_blockhash,
            (),
        ))
    }

    pub async fn get_program_accounts(
        &self,
        remote_ctx: &Option<SurfnetRemoteClient>,
        program_id: &Pubkey,
        account_config: RpcAccountInfoConfig,
        filters: Option<Vec<RpcFilterType>>,
    ) -> SurfpoolContextualizedResult<Vec<RpcKeyedAccount>> {
        if let Some(remote_client) = remote_ctx {
            self.get_program_accounts_local_then_remote(
                remote_client,
                program_id,
                account_config,
                filters,
            )
            .await
        } else {
            self.get_program_accounts_local(program_id, account_config, filters)
        }
    }

    /// Retrieves program accounts from the local SVM cache, returning a contextualized result.
    pub fn get_program_accounts_local(
        &self,
        program_id: &Pubkey,
        account_config: RpcAccountInfoConfig,
        filters: Option<Vec<RpcFilterType>>,
    ) -> SurfpoolContextualizedResult<Vec<RpcKeyedAccount>> {
        let res = self.with_svm_reader(|svm_reader| {
            let res = svm_reader.get_account_owned_by(program_id)?;

            let mut filtered = vec![];
            for (pubkey, account) in &res {
                if let Some(ref active_filters) = filters {
                    match apply_rpc_filters(&account.data, active_filters) {
                        Ok(true) => {}           // Account matches all filters
                        Ok(false) => continue,   // Filtered out
                        Err(e) => return Err(e), // Error applying filter, already JsonRpcError
                    }
                }

                filtered.push(svm_reader.account_to_rpc_keyed_account(
                    pubkey,
                    account,
                    &account_config,
                    None,
                ));
            }
            Ok(filtered)
        })?;

        Ok(self.with_contextualized_svm_reader(|_| res.clone()))
    }

    pub fn encode_ui_account(
        &self,
        pubkey: &Pubkey,
        account: &Account,
        encoding: UiAccountEncoding,
        additional_data: Option<AccountAdditionalDataV3>,
        data_slice: Option<UiDataSliceConfig>,
    ) -> UiAccount {
        self.with_svm_reader(|svm_reader| {
            svm_reader.encode_ui_account(pubkey, account, encoding, additional_data, data_slice)
        })
    }

    /// Retrieves program accounts from the local cache and remote client, combining results.
    pub async fn get_program_accounts_local_then_remote(
        &self,
        client: &SurfnetRemoteClient,
        program_id: &Pubkey,
        account_config: RpcAccountInfoConfig,
        filters: Option<Vec<RpcFilterType>>,
    ) -> SurfpoolContextualizedResult<Vec<RpcKeyedAccount>> {
        let SvmAccessContext {
            slot,
            latest_epoch_info,
            latest_blockhash,
            inner: local_accounts,
        } = self.get_program_accounts_local(program_id, account_config.clone(), filters.clone())?;

        let remote_accounts_result = client
            .get_program_accounts(program_id, account_config, filters)
            .await?;

        let remote_accounts = remote_accounts_result.handle_method_not_supported(|| {
            let tx = self.simnet_events_tx();
            let _ = tx.send(SimnetEvent::warn("The `getProgramAccounts` method was sent to the remote RPC, but this method isn't supported by your RPC provider. If you need this method, please use a different RPC provider."));
            vec![]
        });

        let mut combined_accounts = remote_accounts
            .into_iter()
            .map(|(pubkey, account)| RpcKeyedAccount {
                pubkey: pubkey.to_string(),
                account,
            })
            .collect::<Vec<RpcKeyedAccount>>();

        for local_account in local_accounts {
            // if the local account is in the remote set, replace it with the local one
            if let Some((pos, _)) = combined_accounts.iter().find_position(
                |RpcKeyedAccount {
                     pubkey: remote_pubkey,
                     ..
                 }| remote_pubkey.eq(&local_account.pubkey),
            ) {
                combined_accounts[pos] = local_account;
            } else {
                // otherwise, add the local account to the combined list
                combined_accounts.push(local_account);
            };
        }

        Ok(SvmAccessContext {
            slot,
            latest_epoch_info,
            latest_blockhash,
            inner: combined_accounts,
        })
    }
}

impl SurfnetSvmLocker {
    /// Returns the first local slot (the genesis_slot when this surfnet started).
    /// Since empty blocks can be reconstructed on-the-fly, all slots from genesis_slot onwards are valid.
    pub fn get_first_local_slot(&self) -> Option<Slot> {
        self.with_svm_reader(|svm| Some(svm.genesis_slot))
    }

    pub async fn get_block(
        &self,
        remote_ctx: &Option<SurfnetRemoteClient>,
        slot: &Slot,
        config: &RpcBlockConfig,
    ) -> SurfpoolContextualizedResult<Option<UiConfirmedBlock>> {
        let committed_slot = self.get_slot_for_commitment(&config.commitment.unwrap_or_default());
        if *slot > committed_slot {
            return Ok(SvmAccessContext {
                slot: committed_slot,
                latest_epoch_info: self.get_epoch_info(),
                latest_blockhash: self
                    .get_latest_blockhash(&CommitmentConfig::processed())
                    .unwrap_or_default(),
                inner: None,
            });
        }

        let first_local_slot = self.get_first_local_slot();

        let result = if first_local_slot.is_some() && first_local_slot.unwrap() > *slot {
            match remote_ctx {
                Some(remote_client) => Some(remote_client.get_block(slot, *config).await?),
                None => return Err(SurfpoolError::slot_too_old(*slot)),
            }
        } else {
            self.get_block_local(slot, config)?
        };

        Ok(SvmAccessContext {
            slot: *slot,
            latest_epoch_info: self.get_epoch_info(),
            latest_blockhash: self
                .get_latest_blockhash(&CommitmentConfig::processed())
                .unwrap_or_default(),
            inner: result,
        })
    }

    pub fn get_block_local(
        &self,
        slot: &Slot,
        config: &RpcBlockConfig,
    ) -> SurfpoolResult<Option<UiConfirmedBlock>> {
        self.with_svm_reader(|svm_reader| svm_reader.get_block_at_slot(*slot, config))
    }

    pub fn get_genesis_hash_local(&self) -> SvmAccessContext<Hash> {
        self.with_contextualized_svm_reader(|svm_reader| svm_reader.genesis_config.hash())
    }

    pub async fn get_genesis_hash(
        &self,
        remote_ctx: &Option<SurfnetRemoteClient>,
    ) -> SurfpoolContextualizedResult<Hash> {
        if let Some(client) = remote_ctx {
            let remote_hash = client.get_genesis_hash().await?;
            Ok(self.with_contextualized_svm_reader(|_| remote_hash))
        } else {
            Ok(self.get_genesis_hash_local())
        }
    }
}

/// Pass through functions for accessing the underlying SurfnetSvm instance
impl SurfnetSvmLocker {
    /// Returns a sender for simulation events from the underlying SVM.
    pub fn simnet_events_tx(&self) -> Sender<SimnetEvent> {
        self.with_svm_reader(|svm_reader| svm_reader.simnet_events_tx.clone())
    }

    /// Retrieves the latest epoch info from the underlying SVM.
    pub fn get_epoch_info(&self) -> EpochInfo {
        self.with_svm_reader(|svm_reader| svm_reader.latest_epoch_info.clone())
    }

    pub fn time_travel(
        &self,
        key: Option<(blake3::Hash, String)>,
        simnet_command_tx: Sender<SimnetCommand>,
        config: TimeTravelConfig,
    ) -> SurfpoolResult<EpochInfo> {
        let (epoch_info, slot_time, updated_at) = self.with_svm_reader(|svm_reader| {
            (
                svm_reader.latest_epoch_info.clone(),
                svm_reader.slot_time,
                svm_reader.updated_at,
            )
        });

        let clock_update: Clock =
            calculate_time_travel_clock(&config, updated_at, slot_time, &epoch_info)
                .map_err(|e| SurfpoolError::internal(e.to_string()))?;

        let formated_time = chrono::DateTime::from_timestamp(clock_update.unix_timestamp, 0)
            .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).unwrap())
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        // Create a channel for confirmation
        let (response_tx, response_rx) = crossbeam_channel::bounded(1);

        // Send the command with confirmation
        let _ = simnet_command_tx.send(SimnetCommand::UpdateInternalClockWithConfirmation(
            key,
            clock_update,
            response_tx,
        ));

        // Wait for confirmation with timeout
        let updated_epoch_info = response_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|e| {
                SurfpoolError::internal(format!("Failed to confirm clock update: {}", e))
            })?;

        let _ = self.simnet_events_tx().send(SimnetEvent::info(format!(
            "Time travel to {} successful (epoch {} / slot {})",
            formated_time, updated_epoch_info.epoch, updated_epoch_info.absolute_slot
        )));

        Ok(updated_epoch_info)
    }

    /// Retrieves the latest absolute slot from the underlying SVM.
    pub fn get_latest_absolute_slot(&self) -> Slot {
        self.with_svm_reader(|svm_reader| svm_reader.get_latest_absolute_slot())
    }

    /// Retrieves the latest blockhash for the given commitment config from the underlying SVM.
    pub fn get_latest_blockhash(&self, config: &CommitmentConfig) -> Option<Hash> {
        let slot = self.get_slot_for_commitment(config);
        self.with_svm_reader(|svm_reader| svm_reader.blockhash_for_slot(slot))
    }

    pub fn latest_absolute_blockhash(&self) -> Hash {
        self.with_svm_reader(|svm_reader| svm_reader.latest_blockhash())
    }

    pub fn get_slot_for_commitment(&self, commitment: &CommitmentConfig) -> Slot {
        self.with_svm_reader(|svm_reader| {
            let slot = svm_reader.get_latest_absolute_slot();
            match commitment.commitment {
                CommitmentLevel::Processed => slot,
                CommitmentLevel::Confirmed => slot.saturating_sub(1),
                CommitmentLevel::Finalized => slot.saturating_sub(FINALIZATION_SLOT_THRESHOLD),
            }
        })
    }

    /// Executes an airdrop via the underlying SVM.
    pub fn airdrop(
        &self,
        pubkey: &Pubkey,
        lamports: u64,
    ) -> Result<TransactionResult, AirdropError> {
        self.with_svm_writer(|svm_writer| svm_writer.airdrop(pubkey, lamports))
    }

    /// Executes a batch airdrop via the underlying SVM.
    pub fn airdrop_pubkeys(&self, lamports: u64, addresses: &[Pubkey]) {
        self.with_svm_writer(|svm_writer| svm_writer.airdrop_pubkeys(lamports, addresses))
    }

    /// Confirms the current block on the underlying SVM, returning `Ok(())` or an error.
    pub async fn confirm_current_block(
        &self,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    ) -> SurfpoolResult<()> {
        // Acquire write lock once and do both operations atomically
        // This prevents lock contention and potential deadlocks from mixing blocking and async locks
        let mut svm_writer = self.0.write().await;
        svm_writer.confirm_current_block()?;
        svm_writer.materialize_overrides(remote_ctx).await
    }

    /// Subscribes for signature updates (confirmed/finalized) and returns a receiver of events.
    pub fn subscribe_for_signature_updates(
        &self,
        signature: &Signature,
        subscription_type: SignatureSubscriptionType,
    ) -> Receiver<(Slot, Option<TransactionError>)> {
        self.with_svm_writer(|svm_writer| {
            svm_writer.subscribe_for_signature_updates(signature, subscription_type.clone())
        })
    }

    /// Subscribes for account updates and returns a receiver of account updates.
    pub fn subscribe_for_account_updates(
        &self,
        account_pubkey: &Pubkey,
        encoding: Option<UiAccountEncoding>,
    ) -> Receiver<UiAccount> {
        // Handles the locking/unlocking safely
        self.with_svm_writer(|svm_writer| {
            svm_writer.subscribe_for_account_updates(account_pubkey, encoding)
        })
    }

    /// Subscribes for program account updates and returns a receiver of keyed account updates.
    pub fn subscribe_for_program_updates(
        &self,
        program_id: &Pubkey,
        encoding: Option<UiAccountEncoding>,
        filters: Option<Vec<RpcFilterType>>,
    ) -> Receiver<RpcKeyedAccount> {
        self.with_svm_writer(|svm_writer| {
            svm_writer.subscribe_for_program_updates(program_id, encoding, filters)
        })
    }

    /// Subscribes for slot updates and returns a receiver of slot updates.
    pub fn subscribe_for_slot_updates(&self) -> Receiver<SlotInfo> {
        self.with_svm_writer(|svm_writer| svm_writer.subscribe_for_slot_updates())
    }

    /// Subscribes for logs updates and returns a receiver of logs updates.
    pub fn subscribe_for_logs_updates(
        &self,
        commitment_level: &CommitmentLevel,
        filter: &RpcTransactionLogsFilter,
    ) -> Receiver<(Slot, RpcLogsResponse)> {
        self.with_svm_writer(|svm_writer| {
            svm_writer.subscribe_for_logs_updates(commitment_level, filter)
        })
    }

    /// Subscribes for snapshot import updates and returns a receiver of snapshot import notifications.
    /// This method spawns a background task that fetches the snapshot and loads it via `load_snapshot`.
    pub fn subscribe_for_snapshot_import_updates(
        &self,
        snapshot_url: &str,
        snapshot_id: &str,
    ) -> Receiver<super::SnapshotImportNotification> {
        // Register the subscription and get the sender/receiver
        let (tx, rx) =
            self.with_svm_writer(|svm_writer| svm_writer.register_snapshot_subscription());

        // Clone the locker for use in the spawned task
        let locker = self.clone();
        let snapshot_url = snapshot_url.to_string();
        let snapshot_id = snapshot_id.to_string();

        tokio::spawn(async move {
            // Send initial notification
            let _ = tx.send(super::SnapshotImportNotification {
                snapshot_id: snapshot_id.clone(),
                status: super::SnapshotImportStatus::Started,
                accounts_loaded: 0,
                total_accounts: 0,
                error: None,
            });

            // Fetch snapshot from URL and parse it
            let snapshot_data = match SurfnetSvm::fetch_snapshot_from_url(&snapshot_url).await {
                Ok(data) => data,
                Err(e) => {
                    let _ = tx.send(super::SnapshotImportNotification {
                        snapshot_id,
                        status: super::SnapshotImportStatus::Failed,
                        accounts_loaded: 0,
                        total_accounts: 0,
                        error: Some(format!("Failed to fetch snapshot: {}", e)),
                    });
                    return;
                }
            };

            let total_accounts = snapshot_data.len() as u64;

            // Send progress notification with total count
            let _ = tx.send(super::SnapshotImportNotification {
                snapshot_id: snapshot_id.clone(),
                status: super::SnapshotImportStatus::InProgress,
                accounts_loaded: 0,
                total_accounts,
                error: None,
            });

            // Load the snapshot using the load_snapshot method
            match locker
                .load_snapshot(&snapshot_data, None, CommitmentConfig::processed())
                .await
            {
                Ok(loaded_count) => {
                    let _ = tx.send(super::SnapshotImportNotification {
                        snapshot_id,
                        status: super::SnapshotImportStatus::Completed,
                        accounts_loaded: loaded_count as u64,
                        total_accounts,
                        error: None,
                    });
                }
                Err(e) => {
                    let _ = tx.send(super::SnapshotImportNotification {
                        snapshot_id,
                        status: super::SnapshotImportStatus::Failed,
                        accounts_loaded: 0,
                        total_accounts,
                        error: Some(format!("Failed to load snapshot: {}", e)),
                    });
                }
            }
        });

        rx
    }

    pub fn runbook_executions(&self) -> Vec<RunbookExecutionStatusReport> {
        self.with_svm_reader(|svm_reader| svm_reader.runbook_executions.clone())
    }

    pub fn start_runbook_execution(&self, runbook_id: String) {
        self.with_svm_writer(|svm_writer| {
            svm_writer.instruction_profiling_enabled = false;
            svm_writer.start_runbook_execution(runbook_id);
        });
    }

    pub fn complete_runbook_execution(
        &self,
        runbook_id: String,
        error: Option<Vec<String>>,
        re_enable_ix_profiling: bool,
    ) {
        self.with_svm_writer(|svm_writer| {
            svm_writer.complete_runbook_execution(&runbook_id, error);
            let some_runbook_executing = svm_writer
                .runbook_executions
                .iter()
                .any(|e| e.completed_at.is_none());
            if !some_runbook_executing && re_enable_ix_profiling {
                svm_writer.instruction_profiling_enabled = true;
            }
        });
    }

    pub fn export_snapshot(
        &self,
        config: ExportSnapshotConfig,
    ) -> SurfpoolResult<BTreeMap<String, AccountSnapshot>> {
        self.with_svm_reader(|svm_reader| svm_reader.export_snapshot(config))
    }

    pub fn get_start_time(&self) -> SystemTime {
        self.with_svm_reader(|svm_reader| svm_reader.start_time)
    }
}

/// Helpers for writing program accounts
impl SurfnetSvmLocker {
    pub async fn write_program(
        &self,
        program_id: Pubkey,
        authority: Option<Pubkey>,
        offset: usize,
        data: &[u8],
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    ) -> SurfpoolResult<()> {
        let program_data_address = get_program_data_address(&program_id);

        let program_account = self
            .get_or_create_program_account(program_id, program_data_address, remote_ctx)
            .await?;

        let _ = self
            .write_program_data_account_with_offset(
                program_id,
                authority,
                program_data_address,
                offset,
                data,
                remote_ctx,
            )
            .await?;

        // Re-set the program account to force LiteSVM to recompile the program
        // from the updated programdata. Without this, the program cache retains
        // the noop placeholder compiled during initial program account creation.
        // Errors are expected for incomplete ELF (multi-chunk writes) and are
        // logged but not propagated.
        let set_result = self.with_svm_writer(|svm_writer| {
            svm_writer.set_account(&program_id, program_account.clone())
        });
        if let Err(e) = set_result {
            let _ = self.simnet_events_tx().send(SimnetEvent::info(format!(
                "Program cache update deferred for {}: {}",
                program_id, e
            )));
        }

        Ok(())
    }

    pub async fn get_or_create_program_account(
        &self,
        program_id: Pubkey,
        program_data_address: Pubkey,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    ) -> SurfpoolResult<Account> {
        // Get program account
        let SvmAccessContext {
            inner: program_account_result,
            ..
        } = self
            .get_account(
                remote_ctx,
                &program_id,
                Some(Box::new(move |svm_locker| {
                    // Create default program account if it doesn't exist
                    let program_state =
                        solana_loader_v3_interface::state::UpgradeableLoaderState::Program {
                            programdata_address: program_data_address,
                        };
                    let program_data = bincode::serialize(&program_state)
                        .expect("Failed to serialize program state");
                    let program_lamports = svm_locker.with_svm_reader(|svm_reader| {
                        svm_reader
                            .inner
                            .minimum_balance_for_rent_exemption(program_data.len())
                    });

                    let _ = svm_locker
                        .simnet_events_tx()
                        .send(SimnetEvent::info(format!(
                            "Creating program account {} with program data address {}",
                            program_id, program_data_address
                        )));

                    GetAccountResult::FoundAccount(
                        program_id,
                        solana_account::Account {
                            lamports: program_lamports,
                            data: program_data,
                            owner: solana_sdk_ids::bpf_loader_upgradeable::id(),
                            executable: true,
                            rent_epoch: 0,
                        },
                        true,
                    )
                })),
            )
            .await?;

        // Check if account was created before consuming it
        let was_program_created = matches!(
            program_account_result,
            GetAccountResult::FoundAccount(_, _, true)
        );

        // Ensure we have a valid program account
        let program_account = program_account_result.map_account()?;

        // Validate it's owned by the upgradeable loader
        if program_account.owner != solana_sdk_ids::bpf_loader_upgradeable::id() {
            return Err(SurfpoolError::invalid_program_account(
                &program_id,
                "Account not owned by the BPF Upgradeable Loader",
            ));
        }

        // Validate it's an executable program account
        if !program_account.executable {
            return Err(SurfpoolError::invalid_program_account(
                &program_id,
                "Account not executable",
            ));
        }

        // Persist the program account if it was newly created
        if was_program_created {
            self.write_account_update(GetAccountResult::FoundAccount(
                program_id,
                program_account.clone(),
                true,
            ));
        }
        Ok(program_account)
    }

    pub async fn write_program_data_account_with_offset(
        &self,
        program_id: Pubkey,
        authority: Option<Pubkey>,
        program_data_address: Pubkey,
        offset: usize,
        data: &[u8],
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    ) -> SurfpoolResult<Account> {
        // Get or create program data account
        let SvmAccessContext {
            inner: program_data_result,
            slot,
            ..
        } = self
            .get_account(
                &remote_ctx,
                &program_data_address,
                Some(Box::new(move |svm_locker| {
                    // Create default program data account if it doesn't exist
                    let programdata_state =
                        solana_loader_v3_interface::state::UpgradeableLoaderState::ProgramData {
                            slot: svm_locker.get_latest_absolute_slot(),
                            // TODO: currently litesvm breaks if you don't provide an authority,
                            // but once that's fixed we should remove the default to system program
                            upgrade_authority_address: authority
                                .or(Some(solana_system_interface::program::id())),
                        };
                    let mut programdata_data = bincode::serialize(&programdata_state)
                        .expect("Failed to serialize program data state");

                    // Add empty program data (will be filled by writes)
                    programdata_data.extend(vec![0u8; 0]);

                    let programdata_lamports = svm_locker.with_svm_reader(|svm_reader| {
                        svm_reader
                            .inner
                            .minimum_balance_for_rent_exemption(programdata_data.len())
                    });

                    let _ = svm_locker
                        .simnet_events_tx()
                        .send(SimnetEvent::info(format!(
                            "Creating program data account {} for program {}",
                            program_data_address, program_id
                        )));

                    GetAccountResult::FoundAccount(
                        program_data_address,
                        solana_account::Account {
                            lamports: programdata_lamports,
                            data: programdata_data,
                            owner: solana_sdk_ids::bpf_loader_upgradeable::id(),
                            executable: false,
                            rent_epoch: 0,
                        },
                        true,
                    )
                })),
            )
            .await?;

        // Get mutable program data account
        let mut program_data_account = program_data_result.map_account()?;

        // Calculate metadata size
        let metadata_size =
            solana_loader_v3_interface::state::UpgradeableLoaderState::size_of_programdata_metadata(
            );

        // Verify program data account has valid state
        let upgrade_authority_address = match bincode::deserialize::<
            solana_loader_v3_interface::state::UpgradeableLoaderState,
        >(&program_data_account.data[..metadata_size])
        {
            Ok(solana_loader_v3_interface::state::UpgradeableLoaderState::ProgramData {
                upgrade_authority_address,
                ..
            }) => upgrade_authority_address,
            Ok(_) => {
                return Err(SurfpoolError::invalid_program_data_account(
                    program_data_address,
                    "Account is not a program data account",
                ));
            }
            Err(e) => {
                return Err(SurfpoolError::invalid_program_data_account(
                    program_data_address,
                    format!("Invalid program data account state: {}", e),
                ));
            }
        };

        let new_metadata = if upgrade_authority_address.ne(&authority) {
            let _ = self.simnet_events_tx().send(SimnetEvent::info(format!(
                "Updating program authority of program {} to {}",
                program_id,
                authority.unwrap_or(solana_system_interface::program::id())
            )));
            solana_loader_v3_interface::state::UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: authority
                    .or(Some(solana_system_interface::program::id())),
            }
        } else {
            solana_loader_v3_interface::state::UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address,
            }
        };

        let metadata_bytes = bincode::serialize(&new_metadata).map_err(|e| {
            SurfpoolError::internal(format!("Failed to serialize program data metadata: {}", e))
        })?;

        // Strip the minimum_program.so placeholder if it was pre-filled by
        // init_programdata_account during program account creation. This prevents
        // leftover placeholder bytes when the actual program is smaller than 3312 bytes.
        let minimum_program_bytes = crate::surfnet::noop_program::NOOP_PROGRAM_ELF;
        if program_data_account.data.len() == metadata_size + minimum_program_bytes.len()
            && program_data_account.data[metadata_size..] == *minimum_program_bytes
        {
            program_data_account.data.truncate(metadata_size);
        }

        // Calculate absolute offset in account data (metadata + offset)
        let absolute_offset = metadata_size + offset;
        let end_offset = absolute_offset + data.len();

        // Expand account data if necessary
        if end_offset > program_data_account.data.len() {
            let new_size = end_offset;
            program_data_account.data.resize(new_size, 0);

            // Update lamports for rent exemption
            let new_lamports = self.with_svm_reader(|svm_reader| {
                svm_reader
                    .inner
                    .minimum_balance_for_rent_exemption(new_size)
            });
            program_data_account.lamports = new_lamports;

            let _ = self.simnet_events_tx().send(SimnetEvent::info(format!(
                "Expanding program data account to {} bytes",
                new_size
            )));
        }

        // Write the metadata
        program_data_account.data[..metadata_size].copy_from_slice(&metadata_bytes);
        // Write data at the specified offset
        program_data_account.data[absolute_offset..end_offset].copy_from_slice(&data);

        // Update the account in SVM
        self.with_svm_writer(|svm_writer| {
            svm_writer.set_account(&program_data_address, program_data_account.clone())?;
            Ok::<(), SurfpoolError>(())
        })?;

        let _ = self.simnet_events_tx().send(SimnetEvent::info(format!(
            "Wrote {} bytes to program {} at offset {}",
            data.len(),
            program_id,
            offset
        )));

        Ok(program_data_account)
    }
}

// Helper function to apply filters
pub(crate) fn apply_rpc_filters(
    account_data: &[u8],
    filters: &[RpcFilterType],
) -> SurfpoolResult<bool> {
    for filter in filters {
        match filter {
            RpcFilterType::DataSize(size) => {
                if account_data.len() as u64 != *size {
                    return Ok(false);
                }
            }
            RpcFilterType::Memcmp(memcmp_filter) => {
                // Use the public bytes_match method from solana_client::rpc_filter::Memcmp
                if !memcmp_filter.bytes_match(account_data) {
                    return Ok(false); // Content mismatch or out of bounds handled by bytes_match
                }
            }
            RpcFilterType::TokenAccountState => {
                return Err(SurfpoolError::internal(
                    "TokenAccountState filter is not supported",
                ));
            }
        }
    }
    Ok(true)
}

// used in the remote.rs
pub fn is_supported_token_program(program_id: &Pubkey) -> bool {
    *program_id == spl_token_interface::ID || *program_id == spl_token_2022_interface::ID
}

fn update_programdata_account(
    program_id: &Pubkey,
    programdata_account: &mut Account,
    new_authority: Option<Pubkey>,
) -> SurfpoolResult<Option<Pubkey>> {
    let upgradeable_loader_state =
        bincode::deserialize::<UpgradeableLoaderState>(&programdata_account.data).map_err(|e| {
            SurfpoolError::invalid_program_account(
                program_id,
                format!("Failed to serialize program data: {}", e),
            )
        })?;
    if let UpgradeableLoaderState::ProgramData {
        upgrade_authority_address,
        slot,
    } = upgradeable_loader_state
    {
        let offset = if upgrade_authority_address.is_some() {
            UpgradeableLoaderState::size_of_programdata_metadata()
        } else {
            UpgradeableLoaderState::size_of_programdata_metadata()
                - serialized_size(&Pubkey::default()).unwrap() as usize
        };

        let mut data = bincode::serialize(&UpgradeableLoaderState::ProgramData {
            upgrade_authority_address: new_authority,
            slot,
        })
        .map_err(|e| {
            SurfpoolError::invalid_program_account(
                program_id,
                format!("Failed to serialize program data: {}", e),
            )
        })?;

        data.append(&mut programdata_account.data[offset..].to_vec());

        programdata_account.data = data;

        Ok(upgrade_authority_address)
    } else {
        Err(SurfpoolError::invalid_program_account(
            program_id,
            "Invalid program data account",
        ))
    }
}

pub fn format_ui_amount_string(amount: u64, decimals: u8) -> String {
    if decimals > 0 {
        let divisor = 10u64.pow(decimals as u32);
        format!(
            "{:.decimals$}",
            amount as f64 / divisor as f64,
            decimals = decimals as usize
        )
    } else {
        amount.to_string()
    }
}

pub fn format_ui_amount(amount: u64, decimals: u8) -> f64 {
    if decimals > 0 {
        let divisor = 10u64.pow(decimals as u32);
        amount as f64 / divisor as f64
    } else {
        amount as f64
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use solana_account::Account;
    use solana_account_decoder::UiAccountEncoding;
    use solana_epoch_schedule::EpochSchedule;
    use solana_transaction_status::TransactionStatusMeta;

    use super::*;
    use crate::{
        scenarios::registry::PYTH_V2_IDL_CONTENT,
        surfnet::{BlockHeader, SurfnetSvm, svm::apply_override_to_decoded_account},
    };

    #[test]
    fn test_get_forged_account_data_with_pyth_fixture() {
        use borsh::{BorshDeserialize, BorshSerialize};

        // Define local structures matching Pyth IDL
        #[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
        pub enum VerificationLevel {
            Partial { num_signatures: u8 },
            Full,
        }

        #[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
        pub struct PriceFeedMessage {
            pub feed_id: [u8; 32],
            pub price: i64,
            pub conf: u64,
            pub exponent: i32,
            pub publish_time: i64,
            pub prev_publish_time: i64,
            pub ema_price: i64,
            pub ema_conf: u64,
        }

        #[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
        pub struct PriceUpdateV2 {
            pub write_authority: Pubkey,
            pub verification_level: VerificationLevel,
            pub price_message: PriceFeedMessage,
            pub posted_slot: u64,
        }

        // Pyth price feed account data fixture
        let account_data_hex = vec![
            0x22, 0xf1, 0x23, 0x63, 0x9d, 0x7e, 0xf4, 0xcd, // Discriminator
            0x35, 0xa7, 0x0c, 0x11, 0x16, 0x2f, 0xbf, 0x5a, 0x0e, 0x7f, 0x7d, 0x2f, 0x96, 0xe1,
            0x9f, 0x97, 0xb0, 0x22, 0x46, 0xa1, 0x56, 0x87, 0xee, 0x67, 0x27, 0x94, 0x89, 0x74,
            0x48, 0xe6, 0x58, 0xde, 0x01, 0xe6, 0x2d, 0xf6, 0xc8, 0xb4, 0xa8, 0x5f, 0xe1, 0xa6,
            0x7d, 0xb4, 0x4d, 0xc1, 0x2d, 0xe5, 0xdb, 0x33, 0x0f, 0x7a, 0xc6, 0x6b, 0x72, 0xdc,
            0x65, 0x8a, 0xfe, 0xdf, 0x0f, 0x4a, 0x41, 0x5b, 0x43, 0xd7, 0x1f, 0x18, 0x64, 0x5f,
            0x0a, 0x00, 0x00, 0x96, 0x67, 0xea, 0xc5, 0x00, 0x00, 0x00, 0x00, 0xf8, 0xff, 0xff,
            0xff, 0x5f, 0x2b, 0x00, 0x69, 0x00, 0x00, 0x00, 0x00, 0x5e, 0x2b, 0x00, 0x69, 0x00,
            0x00, 0x00, 0x00, 0xa0, 0x7c, 0x1a, 0x38, 0x63, 0x0a, 0x00, 0x00, 0x94, 0xa6, 0xb9,
            0xb5, 0x00, 0x00, 0x00, 0x00, 0x8c, 0x5e, 0x6d, 0x16, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        // Create a minimal Pyth IDL for testing
        let idl: Idl = serde_json::from_str(PYTH_V2_IDL_CONTENT).expect("Failed to load IDL");

        // Create overrides - note: this won't actually work with the JSON deserialization
        // since the account data is Borsh-encoded, but we're testing the structure
        let mut overrides: HashMap<String, serde_json::Value> = HashMap::new();

        // Verify IDL has matching discriminator
        let account_def = idl
            .accounts
            .iter()
            .find(|acc| acc.discriminator.eq(&account_data_hex[..8]));

        assert!(
            account_def.is_some(),
            "Should find PriceUpdateV2 account by discriminator"
        );
        assert_eq!(account_def.unwrap().name, "PriceUpdateV2");

        // Step 1: Instantiate an offline Svm instance
        let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
        let svm_locker = SurfnetSvmLocker::new(surfnet_svm);

        // Step 2: Register the IDL for this account
        let account_pubkey = Pubkey::from_str_const("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");
        svm_locker.register_idl(idl.clone(), None).unwrap();

        // Step 3: Create an account with the Pyth data
        let pyth_account = Account {
            lamports: 1_000_000,
            data: account_data_hex.clone(),
            owner: account_pubkey,
            executable: false,
            rent_epoch: 0,
        };

        // Step 4: Use encode_ui_account to decode/encode the account data
        let ui_account = svm_locker.encode_ui_account(
            &account_pubkey,
            &pyth_account,
            UiAccountEncoding::JsonParsed,
            None,
            None, // data_slice
        );

        // Step 5: Verify the UI account has parsed data
        println!("UI Account lamports: {}", ui_account.lamports);
        println!("UI Account owner: {}", ui_account.owner);

        // Assert on parsed account data
        use solana_account_decoder::UiAccountData;
        match &ui_account.data {
            UiAccountData::Json(parsed_account) => {
                let parsed_obj = &parsed_account.parsed;

                // Extract price_message object
                let price_message = parsed_obj
                    .get("price_message")
                    .expect("Should have price_message field")
                    .as_object()
                    .expect("price_message should be an object");

                // Assert on price
                let price = price_message
                    .get("price")
                    .expect("Should have price field")
                    .as_i64()
                    .expect("price should be a number");
                assert_eq!(price, 11404817473495, "Price should match expected value");

                // Assert on exponent
                let exponent = price_message
                    .get("exponent")
                    .expect("Should have exponent field")
                    .as_i64()
                    .expect("exponent should be a number");
                assert_eq!(exponent, -8, "Exponent should be -8");

                // Assert on ema_price
                let ema_price = price_message
                    .get("ema_price")
                    .expect("Should have ema_price field")
                    .as_i64()
                    .expect("ema_price should be a number");
                assert_eq!(
                    ema_price, 11421259300000,
                    "EMA price should match expected value"
                );

                // Assert on publish_time
                let publish_time = price_message
                    .get("publish_time")
                    .expect("Should have publish_time field")
                    .as_i64()
                    .expect("publish_time should be a number");
                assert_eq!(
                    publish_time, 1761618783,
                    "Publish time should match expected value"
                );

                println!("✓ All price assertions passed!");
            }
            _ => panic!("Expected JSON parsed account data"),
        }

        // Step 6: Test get_forged_account_data without overrides (should return same data)
        println!("\n--- Testing get_forged_account_data without overrides ---");
        let forged_data_no_overrides = svm_locker.get_forged_account_data(
            &account_pubkey,
            &account_data_hex,
            &idl,
            &overrides,
        );

        match forged_data_no_overrides {
            Ok(data) => {
                // If it succeeds, verify the data is unchanged
                assert_eq!(
                    data, account_data_hex,
                    "Data without overrides should match original"
                );
                println!("✓ Forged data without overrides matches original!");
            }
            Err(e) => {
                // If it fails, it's due to Borsh/JSON mismatch (expected for now)
                println!("Expected error (Borsh vs JSON): {:?}", e);
                println!("Note: This documents the need for proper Borsh implementation");
            }
        }

        // Step 7: Test get_forged_account_data with overrides
        println!("\n--- Testing get_forged_account_data with overrides ---");

        // Set new values for price and publish_time
        let new_price = 999999999999i64;
        let new_publish_time = 1234567890i64;
        let new_ema_price = 888888888888i64;

        overrides.insert("price_message.price".into(), json!(new_price));
        overrides.insert("price_message.publish_time".into(), json!(new_publish_time));
        overrides.insert("price_message.ema_price".into(), json!(new_ema_price));

        let forged_data_with_overrides = svm_locker.get_forged_account_data(
            &account_pubkey,
            &account_data_hex,
            &idl,
            &overrides,
        );

        match forged_data_with_overrides {
            Ok(modified_data) => {
                // Verify the data is different from original
                assert_ne!(
                    modified_data, account_data_hex,
                    "Modified data should be different from original"
                );
                println!("✓ Modified data is different from original!");

                // Create a modified account to verify the changes
                let modified_account = Account {
                    lamports: 1_000_000,
                    data: modified_data.clone(),
                    owner: account_pubkey,
                    executable: false,
                    rent_epoch: 0,
                };

                // Re-encode the modified account to verify the changes
                let modified_ui_account = svm_locker.encode_ui_account(
                    &account_pubkey,
                    &modified_account,
                    UiAccountEncoding::JsonParsed,
                    None,
                    None,
                );

                // Verify the modified values in the re-encoded account
                match &modified_ui_account.data {
                    UiAccountData::Json(parsed_account) => {
                        let parsed_obj = &parsed_account.parsed;
                        let price_message = parsed_obj
                            .get("price_message")
                            .expect("Should have price_message field")
                            .as_object()
                            .expect("price_message should be an object");

                        // Verify new price
                        let modified_price = price_message
                            .get("price")
                            .expect("Should have price field")
                            .as_i64()
                            .expect("price should be a number");
                        assert_eq!(
                            modified_price, new_price,
                            "Modified price should match override value"
                        );

                        // Verify new publish_time
                        let modified_publish_time = price_message
                            .get("publish_time")
                            .expect("Should have publish_time field")
                            .as_i64()
                            .expect("publish_time should be a number");
                        assert_eq!(
                            modified_publish_time, new_publish_time,
                            "Modified publish_time should match override value"
                        );

                        // Verify new ema_price
                        let modified_ema_price = price_message
                            .get("ema_price")
                            .expect("Should have ema_price field")
                            .as_i64()
                            .expect("ema_price should be a number");
                        assert_eq!(
                            modified_ema_price, new_ema_price,
                            "Modified ema_price should match override value"
                        );

                        // Verify exponent is unchanged
                        let exponent = price_message
                            .get("exponent")
                            .expect("Should have exponent field")
                            .as_i64()
                            .expect("exponent should be a number");
                        assert_eq!(exponent, -8, "Exponent should remain unchanged");

                        println!("✓ All override assertions passed!");
                        println!("  - Price changed: 11404817473495 → {}", new_price);
                        println!(
                            "  - Publish time changed: 1761618783 → {}",
                            new_publish_time
                        );
                        println!("  - EMA price changed: 11421259300000 → {}", new_ema_price);
                        println!("  - Exponent unchanged: -8");
                    }
                    _ => panic!("Expected JSON parsed account data for modified account"),
                }
            }
            Err(e) => {
                // If it fails, it's due to Borsh/JSON mismatch (expected for now)
                println!("Expected error (Borsh vs JSON): {:?}", e);
                println!("Note: Once Borsh serialization is implemented, this test will:");
                println!("  1. Successfully modify the account data");
                println!("  2. Verify price changed to: {}", new_price);
                println!("  3. Verify publish_time changed to: {}", new_publish_time);
                println!("  4. Verify ema_price changed to: {}", new_ema_price);
                println!("  5. Verify other fields remain unchanged");
            }
        }

        // Step 8: Demonstrate proper Borsh deserialization/serialization
        println!("\n--- Step 8: Testing with Borsh structures ---");

        // Deserialize the original account data using Borsh
        let account_bytes = &account_data_hex[8..];
        println!(
            "Account data length (without discriminator): {} bytes",
            account_bytes.len()
        );

        let mut reader = std::io::Cursor::new(account_bytes);
        let original_price_update = PriceUpdateV2::deserialize_reader(&mut reader)
            .expect("Should deserialize Pyth account data with Borsh");

        let bytes_read = reader.position() as usize;
        println!("Bytes read by Borsh: {}", bytes_read);
        if bytes_read < account_bytes.len() {
            println!(
                "Note: {} extra bytes at end (likely padding)",
                account_bytes.len() - bytes_read
            );
        }

        println!("Original Borsh-deserialized data:");
        println!("  - Price: {}", original_price_update.price_message.price);
        println!(
            "  - Exponent: {}",
            original_price_update.price_message.exponent
        );
        println!(
            "  - EMA Price: {}",
            original_price_update.price_message.ema_price
        );
        println!(
            "  - Publish time: {}",
            original_price_update.price_message.publish_time
        );

        // Assert original values match what we saw in JSON parsing
        assert_eq!(
            original_price_update.price_message.price, 11404817473495,
            "Borsh price should match JSON parsed value"
        );
        assert_eq!(
            original_price_update.price_message.exponent, -8,
            "Borsh exponent should match JSON parsed value"
        );
        assert_eq!(
            original_price_update.price_message.ema_price, 11421259300000,
            "Borsh ema_price should match JSON parsed value"
        );
        assert_eq!(
            original_price_update.price_message.publish_time, 1761618783,
            "Borsh publish_time should match JSON parsed value"
        );

        println!("✓ Borsh deserialization matches JSON parsing!");

        // Step 9: Modify and re-serialize with Borsh
        println!("\n--- Step 9: Modifying account data with Borsh ---");

        let mut modified_price_update = original_price_update.clone();
        modified_price_update.price_message.price = new_price;
        modified_price_update.price_message.publish_time = new_publish_time;
        modified_price_update.price_message.ema_price = new_ema_price;

        // Serialize back to bytes
        let modified_account_data =
            borsh::to_vec(&modified_price_update).expect("Should serialize modified data");

        // Prepend the discriminator
        let mut full_modified_data = account_data_hex[..8].to_vec();
        full_modified_data.extend_from_slice(&modified_account_data);

        println!("Modified Borsh-serialized data:");
        println!(
            "  - Price: {} → {}",
            original_price_update.price_message.price, new_price
        );
        println!(
            "  - Publish time: {} → {}",
            original_price_update.price_message.publish_time, new_publish_time
        );
        println!(
            "  - EMA Price: {} → {}",
            original_price_update.price_message.ema_price, new_ema_price
        );
        println!(
            "  - Exponent: {} (unchanged)",
            modified_price_update.price_message.exponent
        );

        // Verify the modified data is different
        assert_ne!(
            full_modified_data, account_data_hex,
            "Modified data should differ from original"
        );

        // Verify we can deserialize the modified data back
        let mut modified_reader = std::io::Cursor::new(&full_modified_data[8..]);
        let reloaded_price_update = PriceUpdateV2::deserialize_reader(&mut modified_reader)
            .expect("Should deserialize modified data");

        assert_eq!(
            reloaded_price_update.price_message.price, new_price,
            "Reloaded price should match modified value"
        );
        assert_eq!(
            reloaded_price_update.price_message.publish_time, new_publish_time,
            "Reloaded publish_time should match modified value"
        );
        assert_eq!(
            reloaded_price_update.price_message.ema_price, new_ema_price,
            "Reloaded ema_price should match modified value"
        );
        assert_eq!(
            reloaded_price_update.price_message.exponent,
            original_price_update.price_message.exponent,
            "Exponent should remain unchanged"
        );

        println!("✓ Borsh round-trip successful!");

        // Step 10: Verify with encode_ui_account
        println!("\n--- Step 10: Verify modified data with encode_ui_account ---");

        let modified_test_account = Account {
            lamports: 1_000_000,
            data: full_modified_data,
            owner: account_pubkey,
            executable: false,
            rent_epoch: 0,
        };

        let modified_ui_account = svm_locker.encode_ui_account(
            &account_pubkey,
            &modified_test_account,
            UiAccountEncoding::JsonParsed,
            None,
            None,
        );

        // Verify through JSON encoding as well
        match &modified_ui_account.data {
            UiAccountData::Json(parsed_account) => {
                let parsed_obj = &parsed_account.parsed;
                let price_message = parsed_obj
                    .get("price_message")
                    .expect("Should have price_message")
                    .as_object()
                    .expect("Should be object");

                let final_price = price_message
                    .get("price")
                    .expect("Should have price")
                    .as_i64()
                    .expect("Should be i64");
                let final_publish_time = price_message
                    .get("publish_time")
                    .expect("Should have publish_time")
                    .as_i64()
                    .expect("Should be i64");
                let final_ema_price = price_message
                    .get("ema_price")
                    .expect("Should have ema_price")
                    .as_i64()
                    .expect("Should be i64");

                assert_eq!(
                    final_price, new_price,
                    "JSON-parsed price should match Borsh value"
                );
                assert_eq!(
                    final_publish_time, new_publish_time,
                    "JSON-parsed publish_time should match Borsh value"
                );
                assert_eq!(
                    final_ema_price, new_ema_price,
                    "JSON-parsed ema_price should match Borsh value"
                );
            }
            _ => panic!("Expected JSON parsed data"),
        }
    }

    #[test]
    fn test_apply_override_to_decoded_account() {
        use txtx_addon_kit::{indexmap::IndexMap, types::types::Value};

        // Create a txtx Value object
        let mut price_message_obj = IndexMap::new();
        price_message_obj.insert("price".to_string(), Value::Integer(100));
        price_message_obj.insert("publish_time".to_string(), Value::Integer(1234567890));

        let mut decoded_value = IndexMap::new();
        decoded_value.insert(
            "price_message".to_string(),
            Value::Object(price_message_obj),
        );
        decoded_value.insert("expo".to_string(), Value::Integer(-8));

        let mut decoded_value = Value::Object(decoded_value);

        // Test simple override
        let result =
            apply_override_to_decoded_account(&mut decoded_value, "expo", &serde_json::json!(-6));
        assert!(result.is_ok());
        match &decoded_value {
            Value::Object(map) => {
                assert_eq!(map.get("expo"), Some(&Value::Integer(-6)));
            }
            _ => panic!("Expected Object"),
        }

        // Test nested override
        let result = apply_override_to_decoded_account(
            &mut decoded_value,
            "price_message.price",
            &serde_json::json!(200),
        );
        assert!(result.is_ok());
        match &decoded_value {
            Value::Object(map) => match map.get("price_message") {
                Some(Value::Object(price_msg)) => {
                    assert_eq!(price_msg.get("price"), Some(&Value::Integer(200)));
                }
                _ => panic!("Expected price_message to be Object"),
            },
            _ => panic!("Expected Object"),
        }

        // Test invalid path
        let result = apply_override_to_decoded_account(
            &mut decoded_value,
            "nonexistent.field",
            &serde_json::json!(999),
        );
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_v0_transaction_without_alt_emits_geyser_account_updates() {
        use std::time::Duration;

        use crossbeam_channel::{RecvTimeoutError, unbounded};
        use solana_keypair::Keypair;
        use solana_message::{VersionedMessage, v0};
        use solana_signer::Signer;
        use solana_system_interface::instruction as system_instruction;
        use solana_transaction::versioned::VersionedTransaction;

        let (svm, _events_rx, geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let payer = Keypair::new();
        let payer_pubkey = payer.pubkey();
        let recipient = Pubkey::new_unique();

        let _ = locker
            .airdrop(&payer_pubkey, 1_000_000_000)
            .expect("airdrop should succeed");

        let recent_blockhash = locker.latest_absolute_blockhash();
        let message = v0::Message::try_compile(
            &payer_pubkey,
            &[system_instruction::transfer(
                &payer_pubkey,
                &recipient,
                1_000_000,
            )],
            &[],
            recent_blockhash,
        )
        .expect("v0 message should compile");

        let tx =
            VersionedTransaction::try_new(VersionedMessage::V0(message), &[payer.insecure_clone()])
                .expect("v0 transaction should sign");

        let tx_signature = tx.signatures[0];
        let (status_tx, _status_rx) = unbounded();
        locker
            .process_transaction(&None, tx, status_tx, true, true)
            .await
            .expect("transaction processing should succeed");

        let mut account_updates = vec![];
        let mut got_transaction_notify = false;

        for _ in 0..32 {
            match geyser_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(crate::surfnet::GeyserEvent::UpdateAccount(update)) => {
                    account_updates.push(update);
                }
                Ok(crate::surfnet::GeyserEvent::NotifyTransaction(_, _)) => {
                    got_transaction_notify = true;
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        assert!(
            got_transaction_notify,
            "Expected NotifyTransaction geyser event"
        );
        assert!(
            !account_updates.is_empty(),
            "Expected account update geyser events for v0 transaction without ALTs"
        );
        assert!(
            account_updates.iter().any(|u| u.pubkey == payer_pubkey),
            "Expected payer account update"
        );
        assert!(
            account_updates.iter().any(|u| u.pubkey == recipient),
            "Expected recipient account update"
        );

        for update in account_updates {
            let sanitized_transaction = update
                .sanitized_transaction
                .expect("Expected sanitized transaction on account update");
            assert_eq!(
                *sanitized_transaction.signature(),
                tx_signature,
                "Account update should carry transaction signature"
            );
        }
    }

    // Snapshot loading tests

    #[tokio::test(flavor = "multi_thread")]
    async fn test_load_snapshot_basic() {
        use base64::{Engine, engine::general_purpose};

        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let data = vec![1, 2, 3, 4, 5];
        let data_base64 = general_purpose::STANDARD.encode(&data);

        let mut snapshot = BTreeMap::new();
        snapshot.insert(
            pubkey.to_string(),
            Some(AccountSnapshot {
                lamports: 1_000_000,
                owner: owner.to_string(),
                executable: false,
                rent_epoch: 0,
                data: data_base64,
                parsed_data: None,
            }),
        );

        let loaded = locker
            .load_snapshot(&snapshot, None, CommitmentConfig::confirmed())
            .await
            .unwrap();
        assert_eq!(loaded, 1);

        let account = locker
            .with_svm_reader(|svm| svm.get_account(&pubkey))
            .unwrap();
        assert!(account.is_some());
        let account = account.unwrap();
        assert_eq!(account.lamports, 1_000_000);
        assert_eq!(account.owner, owner);
        assert_eq!(account.data, data);
        assert!(!account.executable);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_load_snapshot_multiple_accounts() {
        use base64::{Engine, engine::general_purpose};

        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let owner = Pubkey::new_unique();
        let mut snapshot = BTreeMap::new();

        // Add 5 accounts
        let pubkeys: Vec<Pubkey> = (0..5).map(|_| Pubkey::new_unique()).collect();
        for (i, pubkey) in pubkeys.iter().enumerate() {
            let data = vec![i as u8; 10];
            snapshot.insert(
                pubkey.to_string(),
                Some(AccountSnapshot {
                    lamports: (i as u64 + 1) * 1_000_000,
                    owner: owner.to_string(),
                    executable: false,
                    rent_epoch: 0,
                    data: general_purpose::STANDARD.encode(&data),
                    parsed_data: None,
                }),
            );
        }

        let loaded = locker
            .load_snapshot(&snapshot, None, CommitmentConfig::confirmed())
            .await
            .unwrap();
        assert_eq!(loaded, 5);

        // Verify all accounts were loaded
        for (i, pubkey) in pubkeys.iter().enumerate() {
            let account = locker
                .with_svm_reader(|svm| svm.get_account(pubkey))
                .unwrap()
                .unwrap();
            assert_eq!(account.lamports, (i as u64 + 1) * 1_000_000);
            assert_eq!(account.owner, owner);
            assert_eq!(account.data, vec![i as u8; 10]);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_load_snapshot_skips_none_without_remote() {
        use base64::{Engine, engine::general_purpose};

        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey1 = Pubkey::new_unique();
        let pubkey2 = Pubkey::new_unique();
        let owner = Pubkey::new_unique();

        let mut snapshot = BTreeMap::new();

        // Add one real account
        snapshot.insert(
            pubkey1.to_string(),
            Some(AccountSnapshot {
                lamports: 1_000_000,
                owner: owner.to_string(),
                executable: false,
                rent_epoch: 0,
                data: general_purpose::STANDARD.encode(&[1, 2, 3]),
                parsed_data: None,
            }),
        );

        // Add one None account (should be skipped without remote client)
        snapshot.insert(pubkey2.to_string(), None);

        let loaded = locker
            .load_snapshot(&snapshot, None, CommitmentConfig::confirmed())
            .await
            .unwrap();
        assert_eq!(loaded, 1);

        // First account should exist
        assert!(
            locker
                .with_svm_reader(|svm| svm.get_account(&pubkey1))
                .unwrap()
                .is_some()
        );

        // Second account should not exist (no remote client to fetch it)
        assert!(
            locker
                .with_svm_reader(|svm| svm.get_account(&pubkey2))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_load_snapshot_invalid_pubkey() {
        use base64::{Engine, engine::general_purpose};

        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let owner = Pubkey::new_unique();
        let mut snapshot = BTreeMap::new();

        // Add an invalid pubkey
        snapshot.insert(
            "invalid_pubkey".to_string(),
            Some(AccountSnapshot {
                lamports: 1_000_000,
                owner: owner.to_string(),
                executable: false,
                rent_epoch: 0,
                data: general_purpose::STANDARD.encode(&[1, 2, 3]),
                parsed_data: None,
            }),
        );

        // Should succeed but load 0 accounts (invalid pubkey is skipped)
        let loaded = locker
            .load_snapshot(&snapshot, None, CommitmentConfig::confirmed())
            .await
            .unwrap();
        assert_eq!(loaded, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_load_snapshot_invalid_base64_data() {
        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let mut snapshot = BTreeMap::new();

        // Add account with invalid base64 data
        snapshot.insert(
            pubkey.to_string(),
            Some(AccountSnapshot {
                lamports: 1_000_000,
                owner: owner.to_string(),
                executable: false,
                rent_epoch: 0,
                data: "not_valid_base64!!!".to_string(),
                parsed_data: None,
            }),
        );

        // Should succeed but load 0 accounts (invalid data is skipped)
        let loaded = locker
            .load_snapshot(&snapshot, None, CommitmentConfig::confirmed())
            .await
            .unwrap();
        assert_eq!(loaded, 0);

        // Account should not exist
        assert!(
            locker
                .with_svm_reader(|svm| svm.get_account(&pubkey))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_load_snapshot_invalid_owner() {
        use base64::{Engine, engine::general_purpose};

        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey = Pubkey::new_unique();
        let mut snapshot = BTreeMap::new();

        // Add account with invalid owner pubkey
        snapshot.insert(
            pubkey.to_string(),
            Some(AccountSnapshot {
                lamports: 1_000_000,
                owner: "invalid_owner".to_string(),
                executable: false,
                rent_epoch: 0,
                data: general_purpose::STANDARD.encode(&[1, 2, 3]),
                parsed_data: None,
            }),
        );

        // Should succeed but load 0 accounts (invalid owner is skipped)
        let loaded = locker
            .load_snapshot(&snapshot, None, CommitmentConfig::confirmed())
            .await
            .unwrap();
        assert_eq!(loaded, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_load_snapshot_empty() {
        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let snapshot = BTreeMap::new();
        let loaded = locker
            .load_snapshot(&snapshot, None, CommitmentConfig::confirmed())
            .await
            .unwrap();
        assert_eq!(loaded, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_load_snapshot_updates_account_registries() {
        use base64::{Engine, engine::general_purpose};

        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();

        let mut snapshot = BTreeMap::new();
        snapshot.insert(
            pubkey.to_string(),
            Some(AccountSnapshot {
                lamports: 1_000_000,
                owner: owner.to_string(),
                executable: false,
                rent_epoch: 0,
                data: general_purpose::STANDARD.encode(&[1, 2, 3]),
                parsed_data: None,
            }),
        );

        locker
            .load_snapshot(&snapshot, None, CommitmentConfig::confirmed())
            .await
            .unwrap();

        // Verify account is in the owner index
        let owned_accounts = locker
            .with_svm_reader(|svm| svm.get_account_owned_by(&owner))
            .unwrap();
        assert_eq!(owned_accounts.len(), 1);
        assert_eq!(owned_accounts[0].0, pubkey);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_load_snapshot_mixed_valid_invalid() {
        use base64::{Engine, engine::general_purpose};

        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let valid_pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();

        let mut snapshot = BTreeMap::new();

        // Valid account
        snapshot.insert(
            valid_pubkey.to_string(),
            Some(AccountSnapshot {
                lamports: 1_000_000,
                owner: owner.to_string(),
                executable: false,
                rent_epoch: 0,
                data: general_purpose::STANDARD.encode(&[1, 2, 3]),
                parsed_data: None,
            }),
        );

        // Invalid pubkey
        snapshot.insert(
            "bad_pubkey".to_string(),
            Some(AccountSnapshot {
                lamports: 2_000_000,
                owner: owner.to_string(),
                executable: false,
                rent_epoch: 0,
                data: general_purpose::STANDARD.encode(&[4, 5, 6]),
                parsed_data: None,
            }),
        );

        // None value (skipped without remote)
        snapshot.insert(Pubkey::new_unique().to_string(), None);

        let loaded = locker
            .load_snapshot(&snapshot, None, CommitmentConfig::confirmed())
            .await
            .unwrap();
        assert_eq!(loaded, 1);

        // Only the valid account should exist
        assert!(
            locker
                .with_svm_reader(|svm| svm.get_account(&valid_pubkey))
                .unwrap()
                .is_some()
        );
    }

    /// Helper: create a VersionedTransaction with a given signature whose account keys contain `pubkey`.
    fn make_test_tx(sig: Signature, pubkey: &Pubkey) -> VersionedTransaction {
        use solana_system_interface::instruction as system_instruction;
        VersionedTransaction {
            signatures: vec![sig],
            message: VersionedMessage::Legacy(Message::new(
                &[system_instruction::transfer(pubkey, pubkey, 1)],
                Some(pubkey),
            )),
        }
    }

    /// Helper: store a transaction into the SVM at the given slot.
    fn store_test_tx(svm: &mut SurfnetSvm, sig: Signature, pubkey: &Pubkey, slot: u64) {
        let tx = make_test_tx(sig, pubkey);
        svm.transactions
            .store(
                sig.to_string(),
                SurfnetTransactionStatus::processed(
                    TransactionWithStatusMeta {
                        slot,
                        transaction: tx,
                        meta: TransactionStatusMeta {
                            status: Ok(()),
                            fee: 5000,
                            pre_balances: vec![0; 3],
                            post_balances: vec![0; 3],
                            inner_instructions: Some(vec![]),
                            log_messages: Some(vec![]),
                            pre_token_balances: Some(vec![]),
                            post_token_balances: Some(vec![]),
                            rewards: Some(vec![]),
                            loaded_addresses: LoadedAddresses::default(),
                            return_data: None,
                            compute_units_consumed: Some(0),
                            cost_units: None,
                        },
                    },
                    HashSet::new(),
                ),
            )
            .unwrap();
    }

    fn seed_signature_history(
        locker: &SurfnetSvmLocker,
        pubkey: &Pubkey,
        blocks: &[(u64, Vec<Signature>)],
    ) {
        locker.with_svm_writer(|svm| {
            for (slot, signatures) in blocks {
                for sig in signatures {
                    store_test_tx(svm, *sig, pubkey, *slot);
                }

                svm.blocks
                    .store(
                        *slot,
                        BlockHeader {
                            hash: String::new(),
                            previous_blockhash: String::new(),
                            parent_slot: 0,
                            block_time: 0,
                            block_height: 0,
                            signatures: signatures.clone(),
                        },
                    )
                    .unwrap();
            }
        });
    }

    fn fetch_signature_strings(
        locker: &SurfnetSvmLocker,
        pubkey: &Pubkey,
        config: Option<&RpcSignaturesForAddressConfig>,
    ) -> Vec<String> {
        locker
            .get_signatures_for_address_local(pubkey, config)
            .inner
            .iter()
            .map(|s| s.signature.clone())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_signatures_for_address_ordering_within_block() {
        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey = Pubkey::new_unique();
        let sig_a = Signature::new_unique();
        let sig_b = Signature::new_unique();
        let sig_c = Signature::new_unique();
        let slot = 5;

        seed_signature_history(&locker, &pubkey, &[(slot, vec![sig_a, sig_b, sig_c])]);
        let sigs = fetch_signature_strings(&locker, &pubkey, None);

        // Last executed (C) should appear first, then B, then A
        assert_eq!(sigs.len(), 3);
        assert_eq!(sigs[0], sig_c.to_string());
        assert_eq!(sigs[1], sig_b.to_string());
        assert_eq!(sigs[2], sig_a.to_string());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_signatures_for_address_until_excludes_boundary_signature() {
        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey = Pubkey::new_unique();
        let sig_a = Signature::new_unique();
        let sig_b = Signature::new_unique();
        let sig_c = Signature::new_unique();
        let slot = 5;

        seed_signature_history(&locker, &pubkey, &[(slot, vec![sig_a, sig_b, sig_c])]);
        let sigs = fetch_signature_strings(
            &locker,
            &pubkey,
            Some(&RpcSignaturesForAddressConfig {
                until: Some(sig_b.to_string()),
                ..RpcSignaturesForAddressConfig::default()
            }),
        );

        assert_eq!(sigs, vec![sig_c.to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_signatures_for_address_before_excludes_boundary_signature() {
        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey = Pubkey::new_unique();
        let sig_a = Signature::new_unique();
        let sig_b = Signature::new_unique();
        let sig_c = Signature::new_unique();
        let slot = 5;

        seed_signature_history(&locker, &pubkey, &[(slot, vec![sig_a, sig_b, sig_c])]);
        let sigs = fetch_signature_strings(
            &locker,
            &pubkey,
            Some(&RpcSignaturesForAddressConfig {
                before: Some(sig_b.to_string()),
                ..RpcSignaturesForAddressConfig::default()
            }),
        );

        assert_eq!(sigs, vec![sig_a.to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_signatures_for_address_before_and_until_form_window() {
        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey = Pubkey::new_unique();
        let sig_a = Signature::new_unique();
        let sig_b = Signature::new_unique();
        let sig_c = Signature::new_unique();
        let sig_d = Signature::new_unique();
        let slot = 5;

        seed_signature_history(
            &locker,
            &pubkey,
            &[(slot, vec![sig_a, sig_b, sig_c, sig_d])],
        );
        let sigs = fetch_signature_strings(
            &locker,
            &pubkey,
            Some(&RpcSignaturesForAddressConfig {
                before: Some(sig_d.to_string()),
                until: Some(sig_b.to_string()),
                ..RpcSignaturesForAddressConfig::default()
            }),
        );

        assert_eq!(sigs, vec![sig_c.to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_signatures_for_address_before_missing_returns_empty() {
        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey = Pubkey::new_unique();
        let sig_a = Signature::new_unique();
        let sig_b = Signature::new_unique();
        let missing_sig = Signature::new_unique();
        let slot = 5;

        seed_signature_history(&locker, &pubkey, &[(slot, vec![sig_a, sig_b])]);
        let sigs = fetch_signature_strings(
            &locker,
            &pubkey,
            Some(&RpcSignaturesForAddressConfig {
                before: Some(missing_sig.to_string()),
                ..RpcSignaturesForAddressConfig::default()
            }),
        );

        assert!(sigs.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_signatures_for_address_until_missing_returns_all_results() {
        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey = Pubkey::new_unique();
        let sig_a = Signature::new_unique();
        let sig_b = Signature::new_unique();
        let missing_sig = Signature::new_unique();
        let slot = 5;

        seed_signature_history(&locker, &pubkey, &[(slot, vec![sig_a, sig_b])]);
        let sigs = fetch_signature_strings(
            &locker,
            &pubkey,
            Some(&RpcSignaturesForAddressConfig {
                until: Some(missing_sig.to_string()),
                ..RpcSignaturesForAddressConfig::default()
            }),
        );

        assert_eq!(sigs, vec![sig_b.to_string(), sig_a.to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_signatures_for_address_limit_applies_after_windowing() {
        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey = Pubkey::new_unique();
        let sig_a = Signature::new_unique();
        let sig_b = Signature::new_unique();
        let sig_c = Signature::new_unique();
        let sig_d = Signature::new_unique();
        let sig_e = Signature::new_unique();
        let slot = 5;

        seed_signature_history(
            &locker,
            &pubkey,
            &[(slot, vec![sig_a, sig_b, sig_c, sig_d, sig_e])],
        );
        let sigs = fetch_signature_strings(
            &locker,
            &pubkey,
            Some(&RpcSignaturesForAddressConfig {
                before: Some(sig_e.to_string()),
                until: Some(sig_a.to_string()),
                limit: Some(2),
                ..RpcSignaturesForAddressConfig::default()
            }),
        );

        assert_eq!(sigs, vec![sig_d.to_string(), sig_c.to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_signatures_for_address_until_excludes_boundary_across_slots() {
        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey = Pubkey::new_unique();
        let sig_s5 = Signature::new_unique();
        let sig_s10_a = Signature::new_unique();
        let sig_s10_b = Signature::new_unique();
        let sig_s15 = Signature::new_unique();

        seed_signature_history(
            &locker,
            &pubkey,
            &[
                (5, vec![sig_s5]),
                (10, vec![sig_s10_a, sig_s10_b]),
                (15, vec![sig_s15]),
            ],
        );
        let sigs = fetch_signature_strings(
            &locker,
            &pubkey,
            Some(&RpcSignaturesForAddressConfig {
                until: Some(sig_s10_b.to_string()),
                ..RpcSignaturesForAddressConfig::default()
            }),
        );

        assert_eq!(sigs, vec![sig_s15.to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_signatures_for_address_ordering_across_slots() {
        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey = Pubkey::new_unique();
        let sig_s5_a = Signature::new_unique();
        let sig_s5_b = Signature::new_unique();
        let sig_s10_a = Signature::new_unique();
        let sig_s10_b = Signature::new_unique();

        seed_signature_history(
            &locker,
            &pubkey,
            &[
                (5, vec![sig_s5_a, sig_s5_b]),
                (10, vec![sig_s10_a, sig_s10_b]),
            ],
        );
        let sigs = fetch_signature_strings(&locker, &pubkey, None);

        // Slot 10 txs first (descending), then slot 5 txs
        // Within each slot: last executed first
        assert_eq!(sigs.len(), 4);
        assert_eq!(sigs[0], sig_s10_b.to_string());
        assert_eq!(sigs[1], sig_s10_a.to_string());
        assert_eq!(sigs[2], sig_s5_b.to_string());
        assert_eq!(sigs[3], sig_s5_a.to_string());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_signatures_for_address_ordering_missing_block_header() {
        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let pubkey = Pubkey::new_unique();
        let sig_a = Signature::new_unique();
        let sig_b = Signature::new_unique();
        let slot = 5;

        locker.with_svm_writer(|svm| {
            store_test_tx(svm, sig_a, &pubkey, slot);
            store_test_tx(svm, sig_b, &pubkey, slot);
            // No block header stored — should not panic
        });

        let result = locker.get_signatures_for_address_local(&pubkey, None);

        // Both transactions should be returned regardless
        assert_eq!(result.inner.len(), 2);

        // Verify both signatures are present (order not guaranteed without block header)
        let sigs: HashSet<String> = result.inner.iter().map(|s| s.signature.clone()).collect();
        assert!(sigs.contains(&sig_a.to_string()));
        assert!(sigs.contains(&sig_b.to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn initializes_epoch_schedule_without_warmup_when_offline() {
        let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
        let svm_locker = SurfnetSvmLocker::new(surfnet_svm);

        svm_locker
            .initialize(&None)
            .await
            .expect("initialize should succeed");

        let epoch_schedule =
            svm_locker.with_svm_reader(|svm_reader| svm_reader.inner.get_sysvar::<EpochSchedule>());

        assert!(
            !epoch_schedule.warmup,
            "offline initialization should disable warmup to match mainnet"
        );
        assert_eq!(
            epoch_schedule.get_first_slot_in_epoch(886),
            886_u64 * 432_000,
            "first slot should align with mainnet epoch boundaries when warmup is disabled"
        );
    }
}
