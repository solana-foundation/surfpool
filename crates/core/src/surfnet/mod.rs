use std::{collections::HashMap, fmt::Display, sync::Arc};

use crossbeam_channel::{Receiver, Sender};
use jsonrpc_core::Result as RpcError;
use locker::SurfnetSvmLocker;
use solana_account::Account;
use solana_account_decoder::{UiAccount, UiAccountEncoding};
use solana_client::{
    rpc_config::RpcTransactionLogsFilter,
    rpc_filter::RpcFilterType,
    rpc_response::{RpcKeyedAccount, RpcLogsResponse},
};
use solana_clock::Slot;
use solana_commitment_config::CommitmentLevel;
use solana_epoch_info::EpochInfo;
use solana_pubkey::Pubkey;
use solana_rpc_client_api::response::SlotUpdate;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, TransactionConfirmationStatus, TransactionStatus,
};
use svm::SurfnetSvm;

use crate::{
    PluginInfo,
    error::{SurfpoolError, SurfpoolResult},
    types::{GeyserAccountUpdate, TransactionWithStatusMeta},
};

pub mod locker;
pub mod noop_program;
pub mod remote;
pub mod surfnet_lite_svm;
pub mod svm;

pub const FINALIZATION_SLOT_THRESHOLD: u64 = 31;
pub const SLOTS_PER_EPOCH: u64 = 432000;

pub type AccountFactory = Box<dyn Fn(SurfnetSvmLocker) -> GetAccountResult + Send + Sync>;

/// Slot status for geyser plugin notifications.
/// Mirrors `agave_geyser_plugin_interface::geyser_plugin_interface::SlotStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeyserSlotStatus {
    /// Slot is being processed
    Processed,
    /// Slot has been rooted (finalized)
    Rooted,
    /// Slot has been confirmed
    Confirmed,
}

/// Block metadata for geyser plugin notifications.
#[derive(Debug, Clone)]
pub struct GeyserBlockMetadata {
    pub slot: Slot,
    pub blockhash: String,
    pub parent_slot: Slot,
    pub parent_blockhash: String,
    pub block_time: Option<i64>,
    pub block_height: Option<u64>,
    pub executed_transaction_count: u64,
    pub entry_count: u64,
}

/// Entry info for geyser plugin notifications.
/// Surfpool emits one entry per block (simplified model).
#[derive(Debug, Clone)]
pub struct GeyserEntryInfo {
    pub slot: Slot,
    pub index: usize,
    pub num_hashes: u64,
    pub hash: Vec<u8>,
    pub executed_transaction_count: u64,
    pub starting_transaction_index: usize,
}

/// Transaction data forwarded to Geyser plugins.
pub struct GeyserTransactionEvent {
    pub transaction_with_status_meta: TransactionWithStatusMeta,
    pub versioned_transaction: Option<VersionedTransaction>,
    pub index: usize,
}

#[allow(clippy::large_enum_variant)]
pub enum GeyserEvent {
    NotifyTransaction(GeyserTransactionEvent),
    UpdateAccount(GeyserAccountUpdate),
    /// Account update sent at startup (before block production begins).
    /// These updates should be sent to geyser plugins with is_startup=true.
    StartupAccountUpdate(GeyserAccountUpdate),
    /// Notify plugins that startup is complete.
    EndOfStartup,
    /// Update slot status (processed, confirmed, rooted/finalized).
    UpdateSlotStatus {
        slot: Slot,
        parent: Option<Slot>,
        status: GeyserSlotStatus,
    },
    /// Notify plugins of block metadata.
    NotifyBlockMetadata(GeyserBlockMetadata),
    /// Notify plugins of entry execution.
    NotifyEntry(GeyserEntryInfo),
}

/// Commands sent from RPC to the geyser runloop for plugin management.
pub enum PluginCommand {
    Load {
        config_file: String,
        response_tx: Sender<Result<PluginInfo, String>>,
    },
    Unload {
        name: String,
        response_tx: Sender<Result<(), String>>,
    },
    Reload {
        name: String,
        config_file: String,
        response_tx: Sender<Result<(), String>>,
    },
    List {
        response_tx: Sender<Vec<PluginInfo>>,
    },
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub struct BlockIdentifier {
    pub index: u64,
    pub hash: String,
}

impl BlockIdentifier {
    pub fn zero() -> Self {
        Self::new(
            0,
            "0000000000000000000000000000000000000000000000000000000000000000",
        )
    }

    pub fn new(index: u64, hash: &str) -> Self {
        Self {
            index,
            hash: hash.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockHeader {
    pub hash: String,
    pub previous_blockhash: String,
    pub parent_slot: Slot,
    pub block_time: i64,
    pub block_height: u64,
    pub signatures: Vec<Signature>,
}

#[derive(PartialEq, Eq, Clone)]
pub enum SurfnetDataConnection {
    Offline,
    Connected(String, EpochInfo),
}

pub type SignatureSubscriptionData = (
    SignatureSubscriptionType,
    Sender<(Slot, Option<TransactionError>)>,
);

/// The status returned by an atomic signature lookup.
///
/// This deliberately contains only the fields needed to produce a
/// `signatureNotification`; serializing the transaction is both unnecessary and would make the
/// registration path needlessly expensive.
pub struct LocalSignatureStatus {
    pub slot: Slot,
    pub err: Option<TransactionError>,
}

/// The outcome of atomically checking a local signature and registering for updates.
pub enum LocalSignatureStatusOrSubscription {
    Status(LocalSignatureStatus),
    Subscription(Receiver<(Slot, Option<TransactionError>)>),
}

pub type AccountSubscriptionData =
    HashMap<Pubkey, Vec<(Option<UiAccountEncoding>, Sender<UiAccount>)>>;

pub type ProgramSubscriptionData = HashMap<
    Pubkey,
    Vec<(
        Option<UiAccountEncoding>,
        Option<Vec<RpcFilterType>>,
        Sender<RpcKeyedAccount>,
    )>,
>;

pub type LogsSubscriptionData = (
    CommitmentLevel,
    RpcTransactionLogsFilter,
    Sender<(Slot, RpcLogsResponse)>,
);

pub type SnapshotSubscriptionData = Sender<SnapshotImportNotification>;

/// Subscription channel for `slotsUpdatesSubscribe` notifications.
///
/// Each subscribed client gets one `Sender<Arc<SlotUpdate>>`; the SVM fans
/// out tagged slot-lifecycle updates (`createdBank`, `frozen`,
/// `optimisticConfirmation`, `root`) to every active sender. Updates are
/// wrapped in `Arc` so we only allocate the payload once per emission and
/// share it across all subscribers.
pub type SlotsUpdatesSubscriptionData = Sender<Arc<SlotUpdate>>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SnapshotImportNotification {
    pub snapshot_id: String,
    pub status: SnapshotImportStatus,
    pub accounts_loaded: u64,
    pub total_accounts: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SnapshotImportStatus {
    Started,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SignatureSubscriptionType {
    Received,
    Commitment(CommitmentLevel),
}

impl Display for SignatureSubscriptionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignatureSubscriptionType::Received => write!(f, "received"),
            SignatureSubscriptionType::Commitment(level) => write!(f, "{level}"),
        }
    }
}

/// Identifies where an account result was read from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountSource {
    /// The account is already present in the live LiteSVM state.
    Svm,
    /// The account was read from the configured database and is not yet in LiteSVM.
    Database,
    /// The account was fetched from the remote RPC.
    Remote,
    /// The account was created locally by a default factory or mutation path.
    Generated,
}

/// The kind of secondary account returned with a coupled account result.
#[derive(Clone, Debug)]
pub enum CoupledAccount {
    /// Upgradeable programs may be returned with their program-data account.
    ProgramData(Pubkey, Option<Account>),
    /// Token accounts may be returned with their mint account.
    Mint(Pubkey, Option<Account>),
}

#[derive(Clone, Debug)]
/// Represents the result of a `get_account` operation.
///
/// The result records provenance, while the caller chooses how that result may
/// affect the SVM through [svm::AccountUpdatePolicy]. In particular,
/// provenance does not imply authoritative replacement. See the policy
/// documentation for the complete result-by-policy decision table.
pub enum GetAccountResult {
    /// Represents that the account was not found.
    None(Pubkey),
    /// Represents an account found in one of the account stores.
    FoundAccount(Pubkey, Account, AccountSource),
    /// Represents an account coupled to a program-data or mint account.
    FoundCoupledAccount((Pubkey, Account), CoupledAccount, AccountSource),
}

impl GetAccountResult {
    pub fn expected_data(&self) -> &Vec<u8> {
        match &self {
            Self::None(_) => unreachable!(),
            Self::FoundAccount(_, account, _) | Self::FoundCoupledAccount((_, account), _, _) => {
                &account.data
            }
        }
    }

    pub fn apply_update<T>(&mut self, update: T) -> RpcError<()>
    where
        T: Fn(&mut Account) -> RpcError<()>,
    {
        match self {
            Self::None(_) => unreachable!(),
            Self::FoundAccount(_, account, source) => {
                update(account)?;
                // Applying an override turns a read result into an explicit
                // local mutation, regardless of where the original account came from.
                *source = AccountSource::Generated;
            }
            Self::FoundCoupledAccount((_, account), _, source) => {
                update(account)?;
                *source = AccountSource::Generated;
            }
        }
        Ok(())
    }

    pub fn map_account(self) -> SurfpoolResult<Account> {
        match self {
            Self::None(pubkey) => Err(SurfpoolError::account_not_found(pubkey)),
            Self::FoundAccount(_, account, _) | Self::FoundCoupledAccount((_, account), _, _) => {
                Ok(account)
            }
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn map_account_with_token_data(
        self,
    ) -> Option<((Pubkey, Account), Option<(Pubkey, Option<Account>)>)> {
        match self {
            Self::None(_) => None,
            Self::FoundAccount(pubkey, account, _) => Some(((pubkey, account), None)),
            Self::FoundCoupledAccount((pubkey, account), coupled, _) => match coupled {
                CoupledAccount::ProgramData(_, _) => Some(((pubkey, account), None)),
                CoupledAccount::Mint(coupled_pubkey, coupled_account) => {
                    Some(((pubkey, account), Some((coupled_pubkey, coupled_account))))
                }
            },
        }
    }

    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None(_))
    }

    pub const fn source(&self) -> Option<AccountSource> {
        match self {
            Self::None(_) => None,
            Self::FoundAccount(_, _, source) | Self::FoundCoupledAccount(_, _, source) => {
                Some(*source)
            }
        }
    }
}

impl From<GetAccountResult> for Result<Account, SurfpoolError> {
    fn from(value: GetAccountResult) -> Self {
        value.map_account()
    }
}

impl SignatureSubscriptionType {
    pub const fn received() -> Self {
        SignatureSubscriptionType::Received
    }

    pub const fn processed() -> Self {
        SignatureSubscriptionType::Commitment(CommitmentLevel::Processed)
    }

    pub const fn confirmed() -> Self {
        SignatureSubscriptionType::Commitment(CommitmentLevel::Confirmed)
    }

    pub const fn finalized() -> Self {
        SignatureSubscriptionType::Commitment(CommitmentLevel::Finalized)
    }

    /// Whether a transaction at `confirmation_status` has reached this subscription's target.
    pub const fn is_satisfied_by(
        &self,
        confirmation_status: TransactionConfirmationStatus,
    ) -> bool {
        matches!(
            (self, confirmation_status),
            (Self::Received, _)
                | (
                    Self::Commitment(CommitmentLevel::Processed),
                    TransactionConfirmationStatus::Processed
                        | TransactionConfirmationStatus::Confirmed
                        | TransactionConfirmationStatus::Finalized
                )
                | (
                    Self::Commitment(CommitmentLevel::Confirmed),
                    TransactionConfirmationStatus::Confirmed
                        | TransactionConfirmationStatus::Finalized
                )
                | (
                    Self::Commitment(CommitmentLevel::Finalized),
                    TransactionConfirmationStatus::Finalized
                )
        )
    }
}

#[allow(clippy::large_enum_variant)]
pub enum GetTransactionResult {
    None(Signature),
    FoundTransaction(
        Signature,
        EncodedConfirmedTransactionWithStatusMeta,
        TransactionStatus,
    ),
}

impl GetTransactionResult {
    pub fn found_transaction(
        signature: Signature,
        tx: EncodedConfirmedTransactionWithStatusMeta,
        latest_absolute_slot: u64,
    ) -> Self {
        let is_finalized = latest_absolute_slot >= tx.slot + FINALIZATION_SLOT_THRESHOLD;
        let is_confirmed = latest_absolute_slot >= tx.slot + 1;
        let (confirmation_status, confirmations) = if is_finalized {
            (
                Some(solana_transaction_status::TransactionConfirmationStatus::Finalized),
                None,
            )
        } else if is_confirmed {
            (
                Some(solana_transaction_status::TransactionConfirmationStatus::Confirmed),
                Some((latest_absolute_slot - tx.slot) as usize),
            )
        } else {
            (
                Some(solana_transaction_status::TransactionConfirmationStatus::Processed),
                Some((latest_absolute_slot - tx.slot) as usize),
            )
        };
        let status = TransactionStatus {
            slot: tx.slot,
            confirmations,
            status: tx
                .transaction
                .clone()
                .meta
                .map_or(Ok(()), |m| m.status.map_err(|e| e.into())),
            err: tx
                .transaction
                .clone()
                .meta
                .and_then(|m| m.err.map(|e| e.into())),
            confirmation_status,
        };

        Self::FoundTransaction(signature, tx, status)
    }

    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None(_))
    }

    pub fn map_found_transaction(&self) -> SurfpoolResult<TransactionStatus> {
        match self {
            Self::None(sig) => Err(SurfpoolError::transaction_not_found(sig)),
            Self::FoundTransaction(_, _, status) => Ok(status.clone()),
        }
    }

    pub fn map_some_transaction_status(&self) -> Option<TransactionStatus> {
        match self {
            Self::None(_) => None,
            Self::FoundTransaction(_, _, status) => Some(status.clone()),
        }
    }
}
