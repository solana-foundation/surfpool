//! Wire types for the Jito `simulateBundle` JSON-RPC method.
//!
//! These mirror the schema implemented in jito-foundation/jito-solana
//! (`rpc-client-api/src/bundles.rs` upstream). They are vendored here rather
//! than imported from `solana-rpc-client-api` because the Jito-specific bundle
//! types only exist in Jito's fork of the Solana monorepo, not in mainline
//! `solana-rpc-client-api`. Keeping the wire format byte-identical to Jito's
//! reference implementation lets clients written against Jito's spec target
//! Surfpool with no JSON-shape adjustments.
//!
//! `RpcSimulateBundleResult` and friends are the public surface; the rest are
//! sub-shapes referenced from it.

use serde::{Deserialize, Serialize};
use solana_account_decoder_client_types::{UiAccount, UiAccountEncoding};
use solana_clock::Slot;
use solana_commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_rpc_client_api::response::RpcBlockhash;
use solana_signature::Signature;
use solana_transaction_error::TransactionError;
use solana_transaction_status::{
    UiLoadedAddresses, UiTransactionEncoding, UiTransactionReturnData, UiTransactionTokenBalance,
};
use thiserror::Error;

/// Request payload for `simulateBundle`. Carries the encoded transactions; the
/// per-tx pre/post-account hints + flags live in the optional config below.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcBundleRequest {
    pub encoded_transactions: Vec<String>,
}

/// Per-tx account-fetch hint. Mirrors `solana_rpc_client_api::config::
/// RpcSimulateTransactionAccountsConfig` (which `simulateTransaction` already
/// uses) — vendored to keep the bundles module self-contained.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RpcSimulateTransactionAccountsConfig {
    /// Optional output encoding for the returned account data. Only
    /// `UiAccountEncoding::Base64` is supported by `simulateBundle`.
    pub encoding: Option<UiAccountEncoding>,
    /// Pubkeys whose state to return alongside the simulation result.
    pub addresses: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSimulateBundleConfig {
    /// Per-tx pre-execution account snapshot hints. When provided MUST have
    /// the same length as `RpcBundleRequest.encoded_transactions`. Omitting
    /// the field (or sending an empty array) is allowed — the server treats
    /// it as "no snapshots requested for any tx", equivalent to sending
    /// `vec![None; bundle_len]`. Mismatched non-empty lengths are rejected
    /// with `invalid_params`.
    #[serde(default)]
    pub pre_execution_accounts_configs: Vec<Option<RpcSimulateTransactionAccountsConfig>>,
    /// Per-tx post-execution account snapshot hints. Same shape and
    /// "omitted = no snapshots" rules as `pre_execution_accounts_configs`.
    #[serde(default)]
    pub post_execution_accounts_configs: Vec<Option<RpcSimulateTransactionAccountsConfig>>,
    /// Encoding the transactions are submitted in. Only `Base64` is supported
    /// — the server rejects any other value with `invalid_params`. Matches
    /// Jito's reference simulateBundle, which also enforces base64 only.
    pub transaction_encoding: Option<UiTransactionEncoding>,
    /// Which bank to simulate against. Surfpool always treats this as
    /// `Tip`-equivalent (the working SVM); accepted for API compatibility.
    pub simulation_bank: Option<SimulationSlotConfig>,
    /// Skip signature verification. Required when `replace_recent_blockhash`
    /// is true (the resigned blockhash invalidates any pre-existing sig).
    #[serde(default)]
    pub skip_sig_verify: bool,
    /// Replace each tx's recent blockhash with the bank's current latest
    /// blockhash. Useful for replaying historical transactions.
    #[serde(default)]
    pub replace_recent_blockhash: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "camelCase")]
pub enum SimulationSlotConfig {
    Commitment(CommitmentConfig),
    Slot(Slot),
    Tip,
}

impl Default for SimulationSlotConfig {
    fn default() -> Self {
        Self::Commitment(CommitmentConfig {
            commitment: CommitmentLevel::Confirmed,
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RpcSimulateBundleResult {
    pub summary: RpcBundleSimulationSummary,
    pub transaction_results: Vec<RpcSimulateBundleTransactionResult>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RpcBundleSimulationSummary {
    /// At least one tx in the bundle errored. `error` carries the typed cause
    /// (with the offending signature if known) and `tx_signature` is the
    /// signature of the first failing tx.
    Failed {
        error: RpcBundleExecutionError,
        tx_signature: Option<String>,
    },
    /// Every tx in the bundle simulated cleanly.
    Succeeded,
}

#[derive(Error, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RpcBundleExecutionError {
    #[error("The bank has hit the max allotted time for processing transactions")]
    BankProcessingTimeLimitReached,
    #[error("Error locking bundle because a transaction is malformed")]
    BundleLockError,
    #[error("Bundle execution timed out")]
    BundleExecutionTimeout,
    #[error("The bundle exceeds the cost model")]
    ExceedsCostModel,
    #[error("Invalid pre or post accounts")]
    InvalidPreOrPostAccounts,
    #[error("PoH record error: {0}")]
    PohRecordError(String),
    #[error("Tip payment error: {0}")]
    TipError(String),
    #[error("A transaction in the bundle failed to execute: [signature={0}, error={1}]")]
    TransactionFailure(Signature, String),
}

/// Per-transaction simulation outcome inside a bundle. Matches the wire shape
/// Jito-Solana returns from `simulateBundle`. Fields are `Option` because not
/// every backend populates every enrichment. Surfpool currently populates
/// `err`, `logs`, `pre/post_execution_accounts`, `units_consumed`, and
/// `replacement_blockhash`. The remaining fields — `return_data`, `fee`,
/// `pre/post_balances`, `pre/post_token_balances`, `loaded_addresses`,
/// `loaded_accounts_data_size` — are uniformly `None` from this backend.
/// This is the same gap that already exists on the single-tx
/// `simulateTransaction` path's bundle-only fields; closing it requires
/// piping richer metadata through `ProfileResult` and is tracked for a
/// follow-up PR. Wire-format clients should treat `None` as "not provided
/// by this server" rather than "field unsupported".
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RpcSimulateBundleTransactionResult {
    pub err: Option<TransactionError>,
    pub logs: Option<Vec<String>>,
    pub pre_execution_accounts: Option<Vec<UiAccount>>,
    pub post_execution_accounts: Option<Vec<UiAccount>>,
    pub units_consumed: Option<u64>,
    pub loaded_accounts_data_size: Option<u32>,
    pub return_data: Option<UiTransactionReturnData>,
    pub replacement_blockhash: Option<RpcBlockhash>,
    pub fee: Option<u64>,
    pub pre_balances: Option<Vec<u64>>,
    pub post_balances: Option<Vec<u64>>,
    pub pre_token_balances: Option<Vec<UiTransactionTokenBalance>>,
    pub post_token_balances: Option<Vec<UiTransactionTokenBalance>>,
    pub loaded_addresses: Option<UiLoadedAddresses>,
}
