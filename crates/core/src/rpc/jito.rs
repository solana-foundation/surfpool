use std::sync::Arc;

use jsonrpc_core::{BoxFuture, Error, Result};
use jsonrpc_derive::rpc;
use sha2::{Digest, Sha256};
use solana_account_decoder::{UiAccount, UiAccountEncoding, encode_ui_account};
use solana_client::{rpc_config::RpcSendTransactionConfig, rpc_custom_error::RpcCustomError};
use solana_pubkey::Pubkey;
use solana_rpc_client_api::response::{Response as RpcResponse, RpcBlockhash, RpcResponseContext};
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status::{TransactionConfirmationStatus, UiTransactionEncoding};
use surfpool_types::{
    JitoBundleStatus, RpcBundleExecutionError, RpcBundleRequest, RpcBundleSimulationSummary,
    RpcSimulateBundleConfig, RpcSimulateBundleResult, RpcSimulateBundleTransactionResult,
    TransactionStatusEvent,
};

use super::{RunloopContext, utils::decode_and_deserialize};
use crate::{
    rpc::full::SurfpoolFullRpc,
    surfnet::{locker::SurfnetSvmLocker, svm::BundleSandbox},
};

/// Maximum number of transactions allowed in a single bundle, matching Jito's limit.
const MAX_BUNDLE_SIZE: usize = 5;

/// Maximum number of bundle IDs accepted in a single `getBundleStatuses` request, matching
/// Jito's documented limit. Larger batches are rejected with `invalid_params`.
const MAX_BUNDLES_PER_QUERY: usize = 5;

/// Jito-specific RPC methods for bundle submission
#[rpc]
pub trait Jito {
    type Metadata;

    /// Sends a bundle of transactions to be processed atomically.
    ///
    /// This RPC method accepts a bundle of transactions (Jito-compatible format) and processes them
    /// one by one in order against an isolated sandbox VM. **The bundle is all-or-nothing**: if any
    /// transaction in the bundle fails (simulation error, execution error, or verification error),
    /// every other transaction's effects are discarded and the underlying VM is left byte-identical
    /// to its pre-bundle state. No Geyser event, no Simnet event, and no WebSocket subscriber
    /// notification is dispatched for a bundle that fails.
    ///
    /// On full success, the sandbox's state changes — account mutations, transaction storage
    /// writes, token-account index updates, write-version increments, etc. — are atomically
    /// committed onto the original VM under an exclusive writer guard, and Geyser/Simnet events
    /// plus WebSocket subscriber notifications (account, program, signature, logs) are fired
    /// onto the live event channels exactly as if each transaction had been submitted through
    /// the regular `sendTransaction` RPC.
    ///
    /// ## Parameters
    /// - `transactions`: An array of serialized transaction data (base64 or base58 encoded).
    /// - `config`: Optional configuration for encoding format.
    ///
    /// ## Returns
    /// - `BoxFuture<Result<String>>`: A future resolving to the bundle ID (SHA-256 hash of
    ///   comma-separated signatures), or an error if any transaction in the bundle fails.
    ///   Returning a future (rather than blocking) lets the JSON-RPC runtime drive the async
    ///   sandbox execution without spawning a nested tokio runtime on an HTTP worker thread.
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "sendBundle",
    ///   "params": [
    ///     ["base64EncodedTx1", "base64EncodedTx2"],
    ///     { "encoding": "base64" }
    ///   ]
    /// }
    /// ```
    ///
    /// ## Notes
    /// - Bundles are limited to a maximum of 5 transactions, matching Jito's limit.
    /// - Transactions are processed sequentially in the order provided against a single sandbox.
    /// - Atomicity is guaranteed: on any failure the original VM is unaffected.
    /// - The bundle ID is calculated as SHA-256 hash of comma-separated transaction signatures.
    #[rpc(meta, name = "sendBundle")]
    fn send_bundle(
        &self,
        meta: Self::Metadata,
        transactions: Vec<String>,
        config: Option<RpcSendTransactionConfig>,
    ) -> BoxFuture<Result<String>>;

    /// Retrieves Jito-shaped status summaries for one or more previously submitted bundles.
    ///
    /// Mirrors Jito's wire protocol: the first positional parameter is an **array** of bundle IDs
    /// (max [`MAX_BUNDLES_PER_QUERY`]). For each requested id we resolve per-signature status via
    /// the same path as `getSignatureStatuses` and return a single aggregate status object per
    /// bundle, or `null` at that index when the bundle is unknown locally.
    ///
    /// ## Parameters
    /// - `bundle_ids`: Array of bundle identifiers returned by `sendBundle`. May contain up to
    ///   [`MAX_BUNDLES_PER_QUERY`] entries; the empty array is rejected with `invalid_params`.
    ///
    /// ## Returns
    /// On success, the JSON-RPC `result` is the standard Solana contextualized response shape:
    /// - `context.slot`: Context slot from the underlying status query (same idea as `getSignatureStatuses`).
    /// - `value`: An array with exactly **one element per input bundle id**, at the same index:
    ///   - `null` when the bundle id is not known locally (no stored signatures — the id may be
    ///     valid elsewhere, we simply have nothing to report).
    ///   - A `surfpool_types::JitoBundleStatus` object otherwise, with:
    ///     - `bundle_id`: The requested bundle id (snake_case wire field).
    ///     - `transactions`: Base-58 signatures in bundle submission order (from local `jito_bundles` storage).
    ///     - `slot`: Slot from the first per-signature status entry (bundle txs share a landing slot), or `0` if none yet.
    ///     - `confirmation_status`: From that same first entry (defaults to `processed` when absent).
    ///     - `err`: `Ok` if no transaction error was observed on any status; otherwise the first `Err` encountered
    ///       (JSON-serialized like other Solana `Result` values, e.g. `{"Ok": null}` or `{"Err": ...}`).
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getBundleStatuses",
    ///   "params": [
    ///     ["bundleIdHere", "anotherBundleId"]
    ///   ]
    /// }
    /// ```
    ///
    /// ## Example Response (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "result": {
    ///     "context": { "slot": 242806119 },
    ///     "value": [
    ///       {
    ///         "bundle_id": "892b79ed49138bfb3aa5441f0df6e06ef34f9ee8f3976c15b323605bae0cf51d",
    ///         "transactions": [
    ///           "3bC2M9fiACSjkTXZDgeNAuQ4ScTsdKGwR42ytFdhUvikqTmBheUxfsR1fDVsM5ADCMMspuwGkdm1uKbU246x5aE3",
    ///           "8t9hKYEYNbLvNqiSzP96S13XF1C2f1ro271Kdf7bkZ6EpjPLuDff1ywRy4gfaGSTubsM2FeYGDoT64ZwPm1cQUt"
    ///         ],
    ///         "slot": 242804011,
    ///         "confirmation_status": "finalized",
    ///         "err": { "Ok": null }
    ///       },
    ///       null
    ///     ]
    ///   }
    /// }
    /// ```
    ///
    /// ## Notes
    /// - Bundles are stored locally as a mapping from `bundle_id` to a list of base-58 signatures.
    /// - Unknown bundle ids appear as `null` **inside** the `value` array; the outer `result` is
    ///   never `null` (Jito-style: per-index reporting).
    /// - Per-signature status resolution uses the same logic as `getSignatureStatuses` (local store and optional remote datasource).
    #[rpc(meta, name = "getBundleStatuses")]
    fn get_bundle_statuses(
        &self,
        meta: Self::Metadata,
        bundle_ids: Vec<String>,
    ) -> BoxFuture<Result<RpcResponse<Vec<Option<JitoBundleStatus>>>>>;

    /// Simulates a bundle of transactions sequentially against an isolated sandbox VM,
    /// without committing any of the resulting state changes onto the live VM.
    ///
    /// This is the read-only counterpart to [`Self::send_bundle`]. Where `send_bundle` is
    /// **all-or-nothing** (every tx must succeed; on full success the sandbox is committed),
    /// `simulate_bundle` is **all-tx-attempted-or-fail-fast**: each transaction is processed
    /// in order against the sandbox, with subsequent transactions seeing the previous tx's
    /// state mutations, but the moment one transaction errors the loop exits and remaining
    /// transactions are not simulated. The sandbox is **always** discarded — successful or
    /// not — so the live VM is byte-identical to its pre-call state regardless of outcome.
    ///
    /// Per-tx the response carries:
    /// - `err`: `None` on success, the typed `TransactionError` on failure.
    /// - `logs`: program logs the SVM emitted while executing the tx.
    /// - `units_consumed`: compute units burned.
    /// - `pre_execution_accounts` / `post_execution_accounts`: pre/post snapshot of the
    ///   accounts whose pubkeys the caller listed in
    ///   `RpcSimulateBundleConfig.{pre,post}_execution_accounts_configs[i]`. Returned in
    ///   `UiAccountEncoding::Base64` (the only encoding `simulateBundle` supports — the
    ///   request is rejected with `invalid_params` for any other encoding hint).
    /// - `replacement_blockhash`: the bank's latest blockhash, but only when
    ///   `replace_recent_blockhash` is true.
    /// - `return_data`: the program return data, when present.
    ///
    /// The remaining fields on `RpcSimulateBundleTransactionResult` (`pre_token_balances`,
    /// `post_token_balances`, `loaded_addresses`, `loaded_accounts_data_size`, `fee`,
    /// `pre_balances`, `post_balances`) are returned as `None`. This matches Surfpool's
    /// existing single-tx `simulateTransaction` behavior; populating them requires layout
    /// decoding of SPL Token accounts that has not yet been done on either path. Tracked
    /// for a follow-up PR.
    ///
    /// ## Parameters
    /// - `rpc_bundle_request`: Wrapper struct carrying the array of base64-encoded
    ///   transactions. Mirrors Jito's wire format.
    /// - `config`: Optional. When omitted the bundle simulates with no pre/post account
    ///   snapshots, sigverify on, and the originally-encoded blockhashes preserved. When
    ///   `replace_recent_blockhash` is true, `skip_sig_verify` MUST also be true (a
    ///   resigned blockhash invalidates any pre-existing signature) — the request is
    ///   otherwise rejected with `invalid_params`.
    ///
    /// ## Returns
    /// `RpcResponse<RpcSimulateBundleResult>` — the standard Solana contextualized response
    /// shape. `value.summary` is `Succeeded` when every transaction in the bundle simulated
    /// cleanly, otherwise `Failed { error, tx_signature }` where `error` carries the
    /// typed `RpcBundleExecutionError::TransactionFailure(signature, message)` for the
    /// first failing tx. `value.transaction_results[i]` corresponds to
    /// `rpc_bundle_request.encoded_transactions[i]`; on early-exit failure the trailing
    /// indices contain the empty/skipped tx result (err: None, logs: None, etc.) — they
    /// were never simulated.
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "simulateBundle",
    ///   "params": [
    ///     { "encodedTransactions": ["base64Tx1", "base64Tx2"] },
    ///     {
    ///       "preExecutionAccountsConfigs": [null, { "addresses": ["..."] }],
    ///       "postExecutionAccountsConfigs": [null, { "addresses": ["..."] }],
    ///       "skipSigVerify": true,
    ///       "replaceRecentBlockhash": true
    ///     }
    ///   ]
    /// }
    /// ```
    ///
    /// ## Notes
    /// - Bundles are limited to a maximum of [`MAX_BUNDLE_SIZE`] (5) transactions, matching
    ///   `sendBundle` and Jito's documented limit.
    /// - `pre_execution_accounts_configs` and `post_execution_accounts_configs`, when
    ///   provided, MUST have the same length as the bundle.
    /// - `simulation_bank` is accepted for API parity; Surfpool always simulates against
    ///   the working SVM regardless of value.
    /// - The sandbox's storage overlays, buffered Geyser/Simnet events, and cloned LiteSVM
    ///   state are dropped at end-of-call. No notification fires on the live event channels;
    ///   no signature/logs subscriber is woken; no chain state is touched.
    #[rpc(meta, name = "simulateBundle")]
    fn simulate_bundle(
        &self,
        meta: Self::Metadata,
        rpc_bundle_request: RpcBundleRequest,
        config: Option<RpcSimulateBundleConfig>,
    ) -> BoxFuture<Result<RpcResponse<RpcSimulateBundleResult>>>;
}

#[derive(Clone)]
pub struct SurfpoolJitoRpc;

impl Jito for SurfpoolJitoRpc {
    type Metadata = Option<RunloopContext>;

    fn send_bundle(
        &self,
        meta: Self::Metadata,
        transactions: Vec<String>,
        config: Option<RpcSendTransactionConfig>,
    ) -> BoxFuture<Result<String>> {
        Box::pin(async move {
            if transactions.is_empty() {
                return Err(Error::invalid_params("Bundle cannot be empty"));
            }

            if transactions.len() > MAX_BUNDLE_SIZE {
                return Err(Error::invalid_params(format!(
                    "Bundle exceeds maximum size of {MAX_BUNDLE_SIZE} transactions"
                )));
            }

            let Some(ctx) = meta else {
                return Err(RpcCustomError::NodeUnhealthy {
                    num_slots_behind: None,
                }
                .into());
            };

            let base_config = config.unwrap_or_default();

            // Decode all bundle transactions up front so we can run them against an isolated
            // sandbox.
            let tx_encoding = base_config
                .encoding
                .unwrap_or(UiTransactionEncoding::Base58);
            let binary_encoding = tx_encoding.into_binary_encoding().ok_or_else(|| {
                Error::invalid_params(format!(
                    "unsupported encoding: {tx_encoding}. Supported encodings: base58, base64"
                ))
            })?;

            let mut decoded_txs: Vec<VersionedTransaction> = Vec::with_capacity(transactions.len());
            for (idx, tx_data) in transactions.iter().enumerate() {
                let (_, tx) = decode_and_deserialize::<VersionedTransaction>(
                    tx_data.clone(),
                    binary_encoding,
                )
                .map_err(|e| Error {
                    code: e.code,
                    message: format!(
                        "Failed to decode bundle transaction {}: {}",
                        idx + 1,
                        e.message
                    ),
                    data: e.data,
                })?;
                decoded_txs.push(tx);
            }

            // -- Phase A: Sandbox execution -------------------------------------------------
            // Take a brief read lock on the original VM to construct a sandbox whose storages
            // are overlay-wrapped, whose subscription registries are empty (no live WS leak),
            // and whose event channels buffer into receivers we hold here.
            let bundle_sandbox = ctx
                .svm_locker
                .with_svm_reader(|svm_reader| svm_reader.clone_for_bundle_sandbox());

            let BundleSandbox {
                svm: sandbox_svm,
                geyser_rx,
                simnet_rx,
            } = bundle_sandbox;

            let sandbox_locker = SurfnetSvmLocker::new(sandbox_svm);

            let remote_ctx = &None;
            let skip_preflight = true;
            let sigverify = true;

            let mut bundle_signatures: Vec<Signature> = Vec::with_capacity(decoded_txs.len());
            for (idx, tx) in decoded_txs.iter().enumerate() {
                let (status_tx, status_rx) = crossbeam_channel::bounded(1);

                // Awaiting directly here lets the surrounding JSON-RPC runtime drive the
                // future. We must NOT use `hiro_system_kit::nestable_block_on` because the
                // HTTP worker thread is already inside a tokio runtime and `block_on` on the
                // current handle panics with "Cannot start a runtime from within a runtime".
                let process_res = sandbox_locker
                    .process_transaction(
                        remote_ctx,
                        tx.clone(),
                        status_tx,
                        skip_preflight,
                        sigverify,
                    )
                    .await;

                bundle_signatures.push(tx.signatures[0]);

                if let Err(e) = process_res {
                    // Dropping `sandbox_locker` discards all overlay state and the cloned
                    // LiteSVM, so the original VM is byte-identical to its pre-bundle state.
                    return Err(Error::invalid_params(format!(
                        "Jito bundle couldn't be executed, failed to process transaction {}: {e}",
                        idx + 1
                    )));
                }

                // `process_transaction` only returns after the sandbox has run the tx and
                // dispatched a status event, so `try_recv`/`recv_timeout` will not actually
                // park the worker for any meaningful time; the 2s timeout is a hard ceiling
                // for an unexpectedly missed status.
                match status_rx.recv_timeout(std::time::Duration::from_secs(2)) {
                    Ok(TransactionStatusEvent::Success(_)) => {}
                    Ok(TransactionStatusEvent::SimulationFailure(other)) => {
                        return Err(Error::invalid_params(format!(
                            "Jito bundle couldn't be executed: simulation failed for transaction {}: {:?}",
                            idx + 1,
                            other
                        )));
                    }
                    Ok(TransactionStatusEvent::ExecutionFailure(other)) => {
                        return Err(Error::invalid_params(format!(
                            "Jito bundle couldn't be executed: Execution failed for transaction {}: {:?}",
                            idx + 1,
                            other
                        )));
                    }
                    Ok(TransactionStatusEvent::VerificationFailure(ver_fail_err)) => {
                        return Err(Error::invalid_params(format!(
                            "Jito bundle couldn't be executed: Verification failed for transaction {}: {:?}",
                            idx + 1,
                            ver_fail_err
                        )));
                    }
                    Err(_) => {
                        return Err(RpcCustomError::NodeUnhealthy {
                            num_slots_behind: None,
                        }
                        .into());
                    }
                }
            }

            // -- Phase B: Atomic commit -----------------------------------------------------
            // All bundle transactions succeeded on the sandbox. Extract the sandbox SVM (the
            // only remaining Arc reference is the local `sandbox_locker`), reassemble the
            // BundleSandbox and call commit_sandbox under the original VM's writer lock.
            let sandbox_svm = match Arc::try_unwrap(sandbox_locker.0) {
                Ok(rwlock) => rwlock.into_inner(),
                Err(_) => {
                    // Should never happen: sandbox_locker was constructed locally and never
                    // shared.
                    return Err(Error::internal_error());
                }
            };
            let reassembled = BundleSandbox {
                svm: sandbox_svm,
                geyser_rx,
                simnet_rx,
            };

            // Use a discardable status channel for the bundle. The runloop will use it to
            // attempt sending Confirmed/Finalized updates; nobody reads it so try_send fails
            // silently.
            let (bundle_status_tx, _bundle_status_rx) = crossbeam_channel::unbounded();

            ctx.svm_locker
                .with_svm_writer(move |original| {
                    original.commit_sandbox(reassembled, bundle_status_tx)
                })
                .map_err(|e| {
                    Error::invalid_params(format!(
                        "Jito bundle commit failed after successful sandbox execution: {e}"
                    ))
                })?;

            // Calculate bundle ID by hashing comma-separated signatures (Jito-compatible)
            // https://github.com/jito-foundation/jito-solana/blob/master/sdk/src/bundle/mod.rs#L21
            let concatenated_signatures = bundle_signatures
                .iter()
                .map(|sig| sig.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut hasher = Sha256::new();
            hasher.update(concatenated_signatures.as_bytes());
            let bundle_id = hex::encode(hasher.finalize());

            ctx.svm_locker.store_bundle(
                bundle_id.clone(),
                bundle_signatures
                    .iter()
                    .map(|sig| sig.to_string())
                    .collect(),
            )?;
            Ok(bundle_id)
        })
    }

    fn get_bundle_statuses(
        &self,
        meta: Self::Metadata,
        bundle_ids: Vec<String>,
    ) -> BoxFuture<Result<RpcResponse<Vec<Option<JitoBundleStatus>>>>> {
        Box::pin(async move {
            if bundle_ids.is_empty() {
                return Err(Error::invalid_params("bundle_ids cannot be empty"));
            }
            if bundle_ids.len() > MAX_BUNDLES_PER_QUERY {
                return Err(Error::invalid_params(format!(
                    "bundle_ids exceeds maximum of {MAX_BUNDLES_PER_QUERY} per request"
                )));
            }

            let Some(ctx) = &meta else {
                return Err(RpcCustomError::NodeUnhealthy {
                    num_slots_behind: None,
                }
                .into());
            };

            // We need a single `context.slot` for the outer RpcResponse. The most accurate slot
            // is the one returned by the underlying `get_signature_statuses` call; if all
            // requested bundles are unknown locally we fall back to the locker's latest slot so
            // the response shape still matches Solana's contextualized RPC contract.
            let mut last_context: Option<RpcResponseContext> = None;
            let mut value: Vec<Option<JitoBundleStatus>> = Vec::with_capacity(bundle_ids.len());

            for bundle_id in bundle_ids {
                let Some(signatures) = ctx.svm_locker.get_bundle(&bundle_id) else {
                    value.push(None);
                    continue;
                };
                if signatures.is_empty() {
                    value.push(None);
                    continue;
                }

                let statuses = super::full::Full::get_signature_statuses(
                    &SurfpoolFullRpc,
                    meta.clone(),
                    signatures.clone(),
                    None,
                )
                .await?;

                last_context = Some(statuses.context.clone());

                // Bundle txs are processed sequentially in one go; they share the same landing
                // slot and confirmation level, so we take slot/status from the first status
                // entry only and aggregate `err` across all entries.
                let (slot, confirmation_status, first_err) = {
                    let mut iter = statuses.value.iter().flatten();

                    let (slot, confirmation_status, head_err) = match iter.next() {
                        Some(first) => (
                            first.slot,
                            first.confirmation_status.clone(),
                            first.err.clone(),
                        ),
                        None => (0, None, None),
                    };

                    let first_err = head_err.or_else(|| iter.find_map(|s| s.err.clone()));
                    (slot, confirmation_status, first_err)
                };

                let confirmation_status =
                    confirmation_status.unwrap_or(TransactionConfirmationStatus::Processed);

                value.push(Some(JitoBundleStatus {
                    bundle_id,
                    transactions: signatures,
                    slot,
                    confirmation_status,
                    err: match first_err {
                        Some(e) => Err(e),
                        None => Ok(()),
                    },
                }));
            }

            let context = last_context.unwrap_or_else(|| {
                let slot = ctx
                    .svm_locker
                    .with_svm_reader(|svm| svm.get_latest_absolute_slot());
                RpcResponseContext::new(slot)
            });

            Ok(RpcResponse { context, value })
        })
    }

    fn simulate_bundle(
        &self,
        meta: Self::Metadata,
        rpc_bundle_request: RpcBundleRequest,
        config: Option<RpcSimulateBundleConfig>,
    ) -> BoxFuture<Result<RpcResponse<RpcSimulateBundleResult>>> {
        Box::pin(async move {
            // Validate bundle size up front. Same cap as `send_bundle` (and Jito's
            // documented limit of 5).
            if rpc_bundle_request.encoded_transactions.is_empty() {
                return Err(Error::invalid_params("Bundle cannot be empty"));
            }
            if rpc_bundle_request.encoded_transactions.len() > MAX_BUNDLE_SIZE {
                return Err(Error::invalid_params(format!(
                    "Bundle exceeds maximum size of {MAX_BUNDLE_SIZE} transactions"
                )));
            }

            let Some(ctx) = meta else {
                return Err(RpcCustomError::NodeUnhealthy {
                    num_slots_behind: None,
                }
                .into());
            };

            // Default config preserves Jito's wire-protocol shape: one None entry per
            // tx for both pre/post snapshot configs.
            let bundle_len = rpc_bundle_request.encoded_transactions.len();
            let config = config.unwrap_or_else(|| RpcSimulateBundleConfig {
                pre_execution_accounts_configs: vec![None; bundle_len],
                post_execution_accounts_configs: vec![None; bundle_len],
                ..RpcSimulateBundleConfig::default()
            });

            let RpcSimulateBundleConfig {
                pre_execution_accounts_configs,
                post_execution_accounts_configs,
                transaction_encoding,
                simulation_bank: _simulation_bank, // accepted for API parity, unused
                skip_sig_verify,
                replace_recent_blockhash,
            } = config;

            // Treat omitted/empty pre+post config vecs as "no snapshots
            // requested for any tx" — equivalent to `vec![None; bundle_len]`.
            // The wire types use `#[serde(default)]` so callers may send a
            // partial config (just `skipSigVerify`, etc.) without specifying
            // these arrays. Mismatched non-empty lengths below are rejected.
            let pre_execution_accounts_configs = if pre_execution_accounts_configs.is_empty() {
                vec![None; bundle_len]
            } else {
                pre_execution_accounts_configs
            };
            let post_execution_accounts_configs = if post_execution_accounts_configs.is_empty() {
                vec![None; bundle_len]
            } else {
                post_execution_accounts_configs
            };

            // Length of pre/post configs MUST match the bundle when provided.
            // This matches Jito's contract — a mismatch would silently drop
            // snapshots or panic on indexing.
            if pre_execution_accounts_configs.len() != bundle_len
                || post_execution_accounts_configs.len() != bundle_len
            {
                return Err(Error::invalid_params(
                    "preExecutionAccountsConfigs/postExecutionAccountsConfigs, when provided, must be equal in length to the number of transactions",
                ));
            }

            // We only support base64 for the snapshotted accounts and for tx encoding.
            // Base58 is too slow for byte-blob roundtrips at this size, and matching
            // Jito's reference implementation here keeps the wire shape consistent.
            for cfg in pre_execution_accounts_configs
                .iter()
                .chain(post_execution_accounts_configs.iter())
            {
                if let Some(cfg) = cfg {
                    let enc = cfg.encoding.unwrap_or(UiAccountEncoding::Base64);
                    if enc != UiAccountEncoding::Base64 {
                        return Err(Error::invalid_params(
                            "Base64 is the only supported encoding for pre/post-execution accounts",
                        ));
                    }
                }
            }

            // `replace_recent_blockhash` resigns the message; existing signatures no
            // longer match. Reject the dangerous combination explicitly so callers
            // notice rather than seeing a SignatureFailure mid-bundle.
            if replace_recent_blockhash && !skip_sig_verify {
                return Err(Error::invalid_params(
                    "replaceRecentBlockhash requires skipSigVerify=true (replacing the blockhash invalidates pre-existing signatures)",
                ));
            }

            // Match Jito's reference simulateBundle: only base64 is accepted.
            // base58 is too slow at the byte-blob sizes bundle simulation uses,
            // and accepting both creates client-side ambiguity over which one
            // the server will use when none is specified.
            let tx_encoding = transaction_encoding.unwrap_or(UiTransactionEncoding::Base64);
            if tx_encoding != UiTransactionEncoding::Base64 {
                return Err(Error::invalid_params(
                    "Base64 is the only supported encoding for transactions in simulateBundle",
                ));
            }
            let binary_encoding = tx_encoding
                .into_binary_encoding()
                .expect("Base64 has a binary encoding");

            // Decode every transaction up front — fail-fast on any decode error before
            // we spend cycles cloning the SVM.
            let mut decoded_txs: Vec<VersionedTransaction> = Vec::with_capacity(bundle_len);
            for (idx, tx_data) in rpc_bundle_request.encoded_transactions.iter().enumerate() {
                let (_, tx) = decode_and_deserialize::<VersionedTransaction>(
                    tx_data.clone(),
                    binary_encoding,
                )
                .map_err(|e| Error {
                    code: e.code,
                    message: format!(
                        "Failed to decode bundle transaction {}: {}",
                        idx + 1,
                        e.message
                    ),
                    data: e.data,
                })?;
                // Reject transactions without signatures up front. A versioned
                // transaction is required to carry at least one signature
                // (the fee-payer's); the validator rejects sig-less txs at
                // ingest, so a bundle entry with empty `signatures` is not a
                // valid Solana transaction. Rejecting here avoids having to
                // synthesize a zero-byte placeholder Signature into the
                // RpcBundleExecutionError::TransactionFailure(Signature, _)
                // wire variant downstream — that would mislead clients
                // keying off the sig inside `error`.
                if tx.signatures.is_empty() {
                    return Err(Error::invalid_params(format!(
                        "Bundle transaction {} has no signatures",
                        idx + 1
                    )));
                }
                decoded_txs.push(tx);
            }

            // Pre-resolve the pre/post pubkey lists per tx. Done once before the loop
            // so a malformed pubkey errors out cleanly before we touch the sandbox.
            let pre_pubkeys = parse_account_addresses(&pre_execution_accounts_configs)?;
            let post_pubkeys = parse_account_addresses(&post_execution_accounts_configs)?;

            // ---- Sandbox setup -------------------------------------------------------
            // Same primitive `send_bundle` uses: clone the SVM with overlay-wrapped
            // storages and emptied subscription registries, with event channels
            // redirected into receivers we hold here. Any state mutations made during
            // simulation are confined to the sandbox; on drop the overlay deltas, the
            // cloned LiteSVM, and every buffered event are discarded.
            //
            // Unlike `send_bundle` we never call `commit_sandbox`. The sandbox is
            // dropped at end-of-call regardless of bundle outcome — this is what
            // distinguishes simulate from send.
            let bundle_sandbox = ctx
                .svm_locker
                .with_svm_reader(|svm_reader| svm_reader.clone_for_bundle_sandbox());
            let BundleSandbox {
                svm: sandbox_svm,
                geyser_rx: _geyser_rx, // discarded on drop
                simnet_rx: _simnet_rx, // discarded on drop
            } = bundle_sandbox;
            let sandbox_locker = SurfnetSvmLocker::new(sandbox_svm);

            // ---- Optional blockhash replacement -------------------------------------
            // Replace each tx's recent_blockhash with the sandbox VM's latest blockhash
            // so historical/expired transactions can be replayed. Reproduces the
            // RpcBlockhash payload Jito returns under `replacement_blockhash`.
            let replacement_blockhash: Option<RpcBlockhash> = if replace_recent_blockhash {
                // Pull both fields under a single reader lock and use the
                // bank's actual block height (NOT the absolute slot) for
                // last_valid_block_height — `RpcBlockhash` documents this as
                // a block height, and clients rely on the distinction.
                let (latest_hash, last_valid_block_height) =
                    sandbox_locker.with_svm_reader(|svm| {
                        (svm.latest_blockhash(), svm.latest_epoch_info().block_height)
                    });
                for tx in decoded_txs.iter_mut() {
                    tx.message.set_recent_blockhash(latest_hash);
                }
                Some(RpcBlockhash {
                    blockhash: latest_hash.to_string(),
                    last_valid_block_height,
                })
            } else {
                None
            };

            let remote_ctx = &None;
            let skip_preflight = true;
            let sigverify = !skip_sig_verify;

            // ---- Sequential simulation loop -----------------------------------------
            // Initialize per-tx results to the empty/skipped shape. We overwrite
            // entries as we simulate; on early-exit failure the trailing entries stay
            // empty (matching Jito's "skipped txs after first failure" semantics).
            let mut transaction_results: Vec<RpcSimulateBundleTransactionResult> = (0..bundle_len)
                .map(|_| empty_tx_result(replacement_blockhash.clone()))
                .collect();
            let mut summary: RpcBundleSimulationSummary = RpcBundleSimulationSummary::Succeeded;

            // Move owned txs into the loop — `into_iter()` so we can pass each
            // `tx` by value into `fetch_all_tx_accounts_then_process_tx_returning_profile_res`
            // without cloning. `decoded_txs` is not used after this point.
            for (idx, tx) in decoded_txs.into_iter().enumerate() {
                // We rejected sig-less txs at decode time, so signatures[0]
                // always exists here.
                let signature: Signature = tx.signatures[0];
                let pre_was_some = pre_execution_accounts_configs[idx].is_some();
                let post_was_some = post_execution_accounts_configs[idx].is_some();

                // Snapshot pre-state for the requested pubkeys BEFORE running the
                // tx. When the caller did not ask for a snapshot for this tx, the
                // pubkey list is empty and we surface the field as None — matches
                // Jito's wire shape (null vs []).
                let pre_accounts = snapshot_accounts(&sandbox_locker, &pre_pubkeys[idx]).await?;

                // Pre-pass sigverify when the caller asked us to verify
                // signatures. We do it ourselves so a failure surfaces a typed
                // SignatureFailure (or AlreadyProcessed) directly — the inner
                // locker call's sigverify path erases the typed err into a
                // SurfpoolError on return, which would force us to string-match
                // to recover the variant.
                if sigverify {
                    if let Err(failed) =
                        sandbox_locker.with_svm_reader(|svm_reader| svm_reader.sigverify(&tx))
                    {
                        let post_accounts =
                            snapshot_accounts(&sandbox_locker, &post_pubkeys[idx]).await?;
                        let pre_for_idx = if pre_was_some {
                            Some(pre_accounts)
                        } else {
                            None
                        };
                        let post_for_idx = if post_was_some {
                            Some(post_accounts)
                        } else {
                            None
                        };
                        let typed = failed.err;
                        transaction_results[idx] = build_tx_result(
                            Some(typed.clone()),
                            None,
                            pre_for_idx,
                            post_for_idx,
                            None,
                            replacement_blockhash.clone(),
                        );
                        summary = RpcBundleSimulationSummary::Failed {
                            error: RpcBundleExecutionError::TransactionFailure(
                                signature,
                                typed.to_string(),
                            ),
                            tx_signature: Some(signature.to_string()),
                        };
                        break;
                    }
                }

                // Per-iteration status channel: do_propagate=true causes the
                // locker to emit TransactionStatusEvent::SimulationFailure /
                // ExecutionFailure into our channel on tx error, carrying the
                // typed TransactionError. The sandbox's signature/logs subscriber
                // registries were emptied by clone_for_bundle_sandbox, so the
                // notify_*_subscribers calls upstream of the send fire to nobody.
                // The receiver is dropped at end-of-iteration so events never
                // accumulate across the bundle.
                let (status_tx, status_rx) = crossbeam_channel::unbounded();
                let profile_res = sandbox_locker
                    .fetch_all_tx_accounts_then_process_tx_returning_profile_res(
                        remote_ctx,
                        tx,
                        &status_tx,
                        skip_preflight,
                        // sigverify=false: we already verified above when the
                        // caller asked for it. Avoids double-work on the hot path.
                        false,
                        true, // do_propagate -> status_rx receives typed errors
                    )
                    .await;

                // Always attempt the post-snapshot — the caller asked for it and a
                // failed tx may have partially mutated state. If the snapshot itself
                // errors (sandbox poisoned, etc.) we surface that to the caller.
                let post_accounts = snapshot_accounts(&sandbox_locker, &post_pubkeys[idx]).await?;

                let pre_for_idx = if pre_was_some {
                    Some(pre_accounts)
                } else {
                    None
                };
                let post_for_idx = if post_was_some {
                    Some(post_accounts)
                } else {
                    None
                };

                match profile_res {
                    Ok(keyed) => {
                        let profile = &keyed.transaction_profile;
                        // Drain the status channel non-blockingly to recover the
                        // typed TransactionError when the tx errored. The locker
                        // emits exactly one event per tx; absence (try_recv ->
                        // Empty) means the tx succeeded.
                        let typed_err: Option<solana_transaction_error::TransactionError> =
                            match status_rx.try_recv() {
                                Ok(TransactionStatusEvent::SimulationFailure((err, _meta))) => {
                                    Some(err)
                                }
                                Ok(TransactionStatusEvent::ExecutionFailure((err, _meta))) => {
                                    Some(err)
                                }
                                // VerificationFailure → SignatureFailure: matches
                                // svm::sigverify and the single-tx simulate path
                                // in full.rs. Unreachable in our flow (we run the
                                // inner call with sigverify=false after pre-passing
                                // it ourselves, so the locker's sigverify gate
                                // can't fire), but kept for exhaustiveness against
                                // future locker changes.
                                Ok(TransactionStatusEvent::VerificationFailure(_)) => Some(
                                    solana_transaction_error::TransactionError::SignatureFailure,
                                ),
                                Ok(TransactionStatusEvent::Success(_)) | Err(_) => None,
                            };

                        if let Some(err_msg) = profile.error_message.clone() {
                            // Tx errored. Use the typed err recovered from the
                            // status channel; if for some reason no typed event
                            // arrived (race or future code change), fall back to
                            // None — clients should rely on summary's
                            // TransactionFailure(signature, message) anyway, which
                            // carries the upstream string regardless.
                            transaction_results[idx] = build_tx_result(
                                typed_err,
                                profile.log_messages.clone(),
                                pre_for_idx,
                                post_for_idx,
                                Some(profile.compute_units_consumed),
                                replacement_blockhash.clone(),
                            );
                            summary = RpcBundleSimulationSummary::Failed {
                                error: RpcBundleExecutionError::TransactionFailure(
                                    signature, err_msg,
                                ),
                                tx_signature: Some(signature.to_string()),
                            };
                            break;
                        }

                        // Success path.
                        transaction_results[idx] = build_tx_result(
                            None,
                            profile.log_messages.clone(),
                            pre_for_idx,
                            post_for_idx,
                            Some(profile.compute_units_consumed),
                            replacement_blockhash.clone(),
                        );
                    }
                    Err(e) => {
                        // Pre-processing failure (account loading, ALT resolution,
                        // AccountLoadedTwice). Sigverify was caught + early-exited
                        // above so it can't reach here. The locker doesn't push
                        // to the status channel on this path — process_transaction's
                        // catch-all wrapper does, but we bypass it. try_recv is
                        // expected to be empty; the match is defensive against
                        // future emit-before-bubble changes in the locker.
                        let typed_err: Option<solana_transaction_error::TransactionError> =
                            match status_rx.try_recv() {
                                Ok(TransactionStatusEvent::SimulationFailure((err, _meta))) => {
                                    Some(err)
                                }
                                Ok(TransactionStatusEvent::ExecutionFailure((err, _meta))) => {
                                    Some(err)
                                }
                                Ok(TransactionStatusEvent::VerificationFailure(_)) => Some(
                                    solana_transaction_error::TransactionError::SignatureFailure,
                                ),
                                Ok(_) | Err(_) => None,
                            };
                        transaction_results[idx] = build_tx_result(
                            typed_err,
                            None,
                            pre_for_idx,
                            post_for_idx,
                            None,
                            replacement_blockhash.clone(),
                        );
                        summary = RpcBundleSimulationSummary::Failed {
                            error: RpcBundleExecutionError::TransactionFailure(
                                signature,
                                e.to_string(),
                            ),
                            tx_signature: Some(signature.to_string()),
                        };
                        break;
                    }
                }
            }

            // Sandbox dropped here. Overlay storages, cloned LiteSVM, and buffered
            // event channels are all reclaimed; the live VM is byte-identical to its
            // pre-call state.
            drop(sandbox_locker);

            let slot = ctx
                .svm_locker
                .with_svm_reader(|svm| svm.get_latest_absolute_slot());

            Ok(RpcResponse {
                context: RpcResponseContext::new(slot),
                value: RpcSimulateBundleResult {
                    summary,
                    transaction_results,
                },
            })
        })
    }
}

// ---------------------------------------------------------------------------
// simulate_bundle helpers
//
// Kept private to this module since they're only useful in the bundle-
// simulation path. If/when single-tx simulateTransaction grows similar
// pre/post-snapshot capabilities the parsing/snapshot/encoding helpers can
// move up into a shared module.
// ---------------------------------------------------------------------------

/// Parse the `addresses` lists from a `Vec<Option<RpcSimulateTransactionAccountsConfig>>`
/// into a parallel `Vec<Vec<Pubkey>>`. A `None` entry yields an empty inner vec —
/// the caller treats "no addresses requested" identically to "explicitly empty".
fn parse_account_addresses(
    configs: &[Option<surfpool_types::RpcSimulateTransactionAccountsConfig>],
) -> Result<Vec<Vec<Pubkey>>> {
    configs
        .iter()
        .map(|cfg| {
            let addresses = match cfg {
                Some(c) => &c.addresses,
                None => return Ok(Vec::new()),
            };
            addresses
                .iter()
                .map(|s| {
                    Pubkey::try_from(s.as_str()).map_err(|_| {
                        Error::invalid_params(format!("Invalid pubkey in pre/post accounts: {s}"))
                    })
                })
                .collect()
        })
        .collect()
}

/// Snapshot the requested pubkeys from the sandbox locker, encoding each as
/// a `UiAccount` with base64-encoded data. Missing accounts are encoded as
/// the canonical "empty" shape (zero lamports, system-program owner) so
/// indexes still align with the requested pubkey list — the caller can tell
/// "missing" apart from "empty" by inspecting the returned `lamports`.
async fn snapshot_accounts(
    sandbox_locker: &SurfnetSvmLocker,
    pubkeys: &[Pubkey],
) -> Result<Vec<UiAccount>> {
    if pubkeys.is_empty() {
        return Ok(Vec::new());
    }

    let remote_ctx = &None;
    let contextualized = sandbox_locker
        .get_multiple_accounts(remote_ctx, pubkeys, None)
        .await
        .map_err(|e| Error {
            code: jsonrpc_core::ErrorCode::InternalError,
            message: format!("Failed to fetch pre/post-execution accounts: {e}"),
            data: None,
        })?;

    let mut out = Vec::with_capacity(pubkeys.len());
    for (idx, result) in contextualized.inner.iter().enumerate() {
        let pubkey = pubkeys[idx];
        let account = match result {
            crate::surfnet::GetAccountResult::None(_) => {
                // The account does not exist in the sandbox (or has zero lamports
                // and no data). We surface a canonical "missing" placeholder:
                // zero-lamport, system-program-owned, empty data. Encoding owner
                // as system_program::id() (rather than relying on
                // Account::default(), which produces an all-zero owner pubkey)
                // matches what `getAccountInfo` returns for never-created system
                // accounts and avoids surprising downstream consumers that key
                // on the owner field. simulateTransaction's `accounts` array
                // does the same.
                solana_account::Account {
                    lamports: 0,
                    data: Vec::new(),
                    owner: solana_system_interface::program::id(),
                    executable: false,
                    rent_epoch: 0,
                }
            }
            crate::surfnet::GetAccountResult::FoundAccount(_, account, _)
            | crate::surfnet::GetAccountResult::FoundProgramAccount((_, account), _)
            | crate::surfnet::GetAccountResult::FoundTokenAccount((_, account), _) => {
                account.clone()
            }
        };
        out.push(encode_ui_account(
            &pubkey,
            &account,
            UiAccountEncoding::Base64,
            None,
            None,
        ));
    }
    Ok(out)
}

/// Initial value for each per-tx result slot. Pre-fill the bundle results
/// with this so trailing entries stay self-consistent when the bundle exits
/// early on failure.
fn empty_tx_result(
    replacement_blockhash: Option<RpcBlockhash>,
) -> RpcSimulateBundleTransactionResult {
    RpcSimulateBundleTransactionResult {
        err: None,
        logs: None,
        pre_execution_accounts: None,
        post_execution_accounts: None,
        units_consumed: None,
        loaded_accounts_data_size: None,
        return_data: None,
        replacement_blockhash,
        fee: None,
        pre_balances: None,
        post_balances: None,
        pre_token_balances: None,
        post_token_balances: None,
        loaded_addresses: None,
    }
}

/// Construct a per-tx bundle simulation result from the fields we actually
/// populate. The remaining fields — `return_data`, `fee`, lamport balances,
/// token balances, loaded addresses, `loaded_accounts_data_size` — are
/// uniformly None in this implementation. Closing that gap requires piping
/// richer metadata through `ProfileResult`; tracked for a follow-up PR. See
/// the doc comment on `RpcSimulateBundleTransactionResult` in
/// `surfpool-types::jito_bundles` for the canonical list. Centralizing the
/// construction here keeps the success / typed-error / internal-error
/// branches in lockstep when new fields land.
fn build_tx_result(
    err: Option<solana_transaction_error::TransactionError>,
    logs: Option<Vec<String>>,
    pre_execution_accounts: Option<Vec<UiAccount>>,
    post_execution_accounts: Option<Vec<UiAccount>>,
    units_consumed: Option<u64>,
    replacement_blockhash: Option<RpcBlockhash>,
) -> RpcSimulateBundleTransactionResult {
    RpcSimulateBundleTransactionResult {
        err,
        logs,
        pre_execution_accounts,
        post_execution_accounts,
        units_consumed,
        loaded_accounts_data_size: None,
        return_data: None,
        replacement_blockhash,
        fee: None,
        pre_balances: None,
        post_balances: None,
        pre_token_balances: None,
        post_token_balances: None,
        loaded_addresses: None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use sha2::{Digest, Sha256};
    use solana_keypair::Keypair;
    use solana_message::{VersionedMessage, v0::Message as V0Message};
    use solana_pubkey::Pubkey;
    use solana_signer::Signer;
    use solana_system_interface::instruction as system_instruction;
    use solana_transaction::versioned::VersionedTransaction;
    use solana_transaction_status::TransactionConfirmationStatus as SolanaTxConfirmationStatus;
    use surfpool_types::{SimnetCommand, TransactionConfirmationStatus, TransactionStatusEvent};

    use super::*;
    use crate::{
        tests::helpers::TestSetup,
        types::{SurfnetTransactionStatus, TransactionWithStatusMeta},
    };

    const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

    fn build_v0_transaction(
        payer: &Pubkey,
        signers: &[&Keypair],
        instructions: &[solana_instruction::Instruction],
        recent_blockhash: &solana_hash::Hash,
    ) -> VersionedTransaction {
        let msg = VersionedMessage::V0(
            V0Message::try_compile(payer, instructions, &[], *recent_blockhash).unwrap(),
        );
        VersionedTransaction::try_new(msg, signers).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_empty_bundle_rejected() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let result = setup
            .rpc
            .send_bundle(Some(setup.context.clone()), vec![], None)
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("Bundle cannot be empty"),
            "Expected 'Bundle cannot be empty' error, got: {}",
            err.message
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_exceeds_max_size_rejected() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let transactions = vec!["tx".to_string(); MAX_BUNDLE_SIZE + 1];
        let result = setup
            .rpc
            .send_bundle(Some(setup.context.clone()), transactions, None)
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("exceeds maximum size"),
            "Expected max size error, got: {}",
            err.message
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_no_context_returns_unhealthy() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let result = setup
            .rpc
            .send_bundle(None, vec!["some_tx".to_string()], None)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_bundle_statuses_unknown_bundle_returns_null_entry() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let missing_id = "a".repeat(64);
        let response = setup
            .rpc
            .get_bundle_statuses(Some(setup.context), vec![missing_id])
            .await
            .expect("getBundleStatuses should not return a JSON-RPC error");
        assert_eq!(
            response.value.len(),
            1,
            "value array must have one entry per requested bundle id"
        );
        assert!(
            response.value[0].is_none(),
            "unknown bundle_id should appear as a null entry inside `value`, not as an outer null"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_bundle_statuses_empty_input_rejected() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let result = setup
            .rpc
            .get_bundle_statuses(Some(setup.context), vec![])
            .await;
        assert!(result.is_err(), "empty bundle_ids should be rejected");
        let err = result.unwrap_err();
        assert!(
            err.message.contains("cannot be empty"),
            "Expected empty-input error, got: {}",
            err.message
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_bundle_statuses_exceeds_max_per_query_rejected() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let too_many = vec!["a".repeat(64); MAX_BUNDLES_PER_QUERY + 1];
        let result = setup
            .rpc
            .get_bundle_statuses(Some(setup.context), too_many)
            .await;
        assert!(
            result.is_err(),
            "exceeding MAX_BUNDLES_PER_QUERY should error"
        );
        let err = result.unwrap_err();
        assert!(
            err.message.contains("exceeds maximum"),
            "Expected max-batch error, got: {}",
            err.message
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_bundle_statuses_no_context_returns_unhealthy() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let result = setup
            .rpc
            .get_bundle_statuses(None, vec!["a".repeat(64)])
            .await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_single_transaction() {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        // Airdrop to payer
        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 2 * LAMPORTS_PER_SOL);

        let tx = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );
        let tx_encoded = bs58::encode(bincode::serialize(&tx).unwrap()).into_string();
        let expected_sig = tx.signatures[0];

        let result = setup
            .rpc
            .send_bundle(Some(setup.context.clone()), vec![tx_encoded], None)
            .await;

        assert!(result.is_ok(), "Bundle should succeed: {:?}", result);

        // Verify bundle ID is SHA-256 of the signature
        let bundle_id = result.unwrap();
        let mut hasher = Sha256::new();
        hasher.update(expected_sig.to_string().as_bytes());
        let expected_bundle_id = hex::encode(hasher.finalize());
        assert_eq!(
            bundle_id, expected_bundle_id,
            "Bundle ID should match SHA-256 of signature"
        );

        // Verify recipient balance reflects the committed bundle
        let recipient_lamports = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.get_account(&recipient))
            .ok()
            .flatten()
            .map(|a| a.lamports)
            .unwrap_or(0);
        assert_eq!(
            recipient_lamports, LAMPORTS_PER_SOL,
            "Bundle commit should have applied lamport transfer to recipient"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_multiple_transactions() {
        let payer = Keypair::new();
        let recipient1 = Pubkey::new_unique();
        let recipient2 = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        // Airdrop to payer
        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 5 * LAMPORTS_PER_SOL);

        let tx1 = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient1,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );
        let tx2 = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient2,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );

        let tx1_encoded = bs58::encode(bincode::serialize(&tx1).unwrap()).into_string();
        let tx2_encoded = bs58::encode(bincode::serialize(&tx2).unwrap()).into_string();
        let expected_sig1 = tx1.signatures[0];
        let expected_sig2 = tx2.signatures[0];

        let result = setup
            .rpc
            .send_bundle(
                Some(setup.context.clone()),
                vec![tx1_encoded, tx2_encoded],
                None,
            )
            .await;

        assert!(result.is_ok(), "Bundle should succeed: {:?}", result);

        // Both recipient balances should reflect committed bundle
        let recipient1_lamports = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.get_account(&recipient1))
            .ok()
            .flatten()
            .map(|a| a.lamports)
            .unwrap_or(0);
        let recipient2_lamports = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.get_account(&recipient2))
            .ok()
            .flatten()
            .map(|a| a.lamports)
            .unwrap_or(0);
        assert_eq!(recipient1_lamports, LAMPORTS_PER_SOL);
        assert_eq!(recipient2_lamports, LAMPORTS_PER_SOL);

        // Verify bundle ID is SHA-256 of comma-separated signatures
        let bundle_id = result.unwrap();
        let concatenated = format!("{},{}", expected_sig1, expected_sig2);
        let mut hasher = Sha256::new();
        hasher.update(concatenated.as_bytes());
        let expected_bundle_id = hex::encode(hasher.finalize());
        assert_eq!(
            bundle_id, expected_bundle_id,
            "Bundle ID should match SHA-256 of comma-separated signatures"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_dependent_transaction_failure_aborts_entire_bundle() {
        let payer = Keypair::new();
        let recipient = Keypair::new();

        // Use mempool-backed setup so we can assert that a sandbox failure does NOT enqueue any
        // ProcessTransaction commands
        let (mempool_tx, mempool_rx) = crossbeam_channel::unbounded();
        let setup = TestSetup::new_with_mempool(SurfpoolJitoRpc, mempool_tx);

        // Drain any ProcessTransaction commands so `sendTransaction` cannot block this test even
        // if Phase 2 is accidentally reached. We track whether anything was sent.
        let observed_process_tx = Arc::new(AtomicUsize::new(0));
        let stop_drain = Arc::new(AtomicBool::new(false));
        let observed_process_tx_clone = observed_process_tx.clone();
        let stop_drain_clone = stop_drain.clone();
        let svm_locker_clone = setup.context.svm_locker.clone();
        let drain_handle = hiro_system_kit::thread_named("mempool_drain_dependent_bundle")
            .spawn(move || {
                while !stop_drain_clone.load(Ordering::SeqCst) {
                    let Ok(cmd) = mempool_rx.recv_timeout(Duration::from_millis(200)) else {
                        continue;
                    };
                    match cmd {
                        SimnetCommand::ProcessTransaction(_, tx, status_tx, _, _) => {
                            observed_process_tx_clone.fetch_add(1, Ordering::SeqCst);

                            // Minimal bookkeeping (mirrors other bundle tests) + unblock the RPC.
                            let sig = tx.signatures[0];
                            let mut writer = svm_locker_clone.0.blocking_write();
                            let slot = writer.get_latest_absolute_slot();
                            writer.transactions_queued_for_confirmation.push_back((
                                tx.clone(),
                                status_tx.clone(),
                                None,
                            ));
                            let tx_with_status_meta = TransactionWithStatusMeta {
                                slot,
                                transaction: tx,
                                ..Default::default()
                            };
                            let mutated_accounts = std::collections::HashSet::new();
                            let _ = writer.transactions.store(
                                sig.to_string(),
                                SurfnetTransactionStatus::processed(
                                    tx_with_status_meta,
                                    mutated_accounts,
                                ),
                            );

                            let _ = status_tx.send(TransactionStatusEvent::Success(
                                TransactionConfirmationStatus::Confirmed,
                            ));
                        }
                        _ => continue,
                    }
                }
            })
            .unwrap();

        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        // Airdrop to payer so tx1 can fund the recipient.
        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 5 * LAMPORTS_PER_SOL);
        // tx1: payer -> recipient (funds recipient so it can pay fees for tx2)
        let tx1 = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient.pubkey(),
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );

        // tx2 depends on tx1 having executed (recipient needs funds), but must still fail.
        let tx2 = build_v0_transaction(
            &recipient.pubkey(),
            &[&recipient],
            &[system_instruction::transfer(
                &recipient.pubkey(),
                &payer.pubkey(),
                2 * LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );

        let tx1_encoded = bs58::encode(bincode::serialize(&tx1).unwrap()).into_string();
        let tx2_encoded = bs58::encode(bincode::serialize(&tx2).unwrap()).into_string();

        let result = setup
            .rpc
            .send_bundle(
                Some(setup.context.clone()),
                vec![tx1_encoded, tx2_encoded],
                None,
            )
            .await;

        assert!(
            result.is_err(),
            "Bundle should fail if any sandbox transaction fails"
        );
        let err = result.unwrap_err();
        assert!(
            err.message.contains("Jito bundle couldn't be executed"),
            "Expected sandbox failure for tx2, got: {}",
            err.message
        );

        stop_drain.store(true, Ordering::SeqCst);
        let _ = drain_handle.join();

        let recp_pubkey = recipient.pubkey();
        let recp_bal = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.get_account(&recp_pubkey))
            .ok()
            .flatten()
            .map(|a| a.lamports)
            .unwrap_or(0); // this should be fine, since the recp. kp was new, it's not in the svm state

        assert_eq!(
            recp_bal, 0,
            "expected jito bundle to not take effect after bundle failure"
        );

        // If sandbox failure happens as expected, Phase 2 should never run.
        assert_eq!(
            observed_process_tx.load(Ordering::SeqCst),
            0,
            "Expected zero mempool ProcessTransaction commands; sandbox failure should prevent Phase 2"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_simulation_failure_returns_not_atomic_error() {
        let setup = TestSetup::new(SurfpoolJitoRpc);

        // Build a tx that should fail during `simulateTransaction` because the payer
        // has no lamports (no explicit airdrop in this test).
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        let tx = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );
        let tx_encoded = bs58::encode(bincode::serialize(&tx).unwrap()).into_string();

        let result = setup
            .rpc
            .send_bundle(Some(setup.context.clone()), vec![tx_encoded], None)
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();

        assert!(
            err.message.contains("Jito bundle couldn't be executed"),
            "Expected not-atomic error, got: {}",
            err.message
        );
        assert!(
            err.message.contains("Jito bundle couldn't be executed:"),
            "Expected simulation-failure error for transaction 1, got: {}",
            err.message
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_persists_bundle_signatures() {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let (mempool_tx, _) = crossbeam_channel::unbounded();
        let setup = TestSetup::new_with_mempool(SurfpoolJitoRpc, mempool_tx);

        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        // Airdrop to payer so tx can succeed in our manual processing
        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 2 * LAMPORTS_PER_SOL);

        let tx = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );
        let tx_encoded = bs58::encode(bincode::serialize(&tx).unwrap()).into_string();

        // Build expected signatures locally (what we expect to be persisted under bundle_id)
        let expected_sigs = vec![tx.signatures[0].to_string()];

        let setup_clone = setup.clone();
        let send_bundle_result = setup_clone
            .rpc
            .send_bundle(Some(setup_clone.context), vec![tx_encoded], None)
            .await;

        assert!(send_bundle_result.is_ok(), "Expected send_bundle to pass");

        let bundle_id = send_bundle_result.unwrap();

        // sendBundle stores bundle signatures directly in `jito_bundles`; poll until visible.
        let started = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(2);
        let persisted = loop {
            match setup.context.svm_locker.get_bundle(&bundle_id) {
                Some(sigs) if !sigs.is_empty() => break sigs,
                _ if started.elapsed() > timeout => {
                    panic!("timed out waiting for bundle to be persisted: {bundle_id}");
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        };
        assert!(
            !persisted.is_empty(),
            "svm_locker.get_bundle(bundle_id) should not be empty"
        );
        assert_eq!(
            persisted, expected_sigs,
            "Persisted bundle signatures should match locally built signatures"
        );

        let started = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(2);
        let (bundle, context_slot) = loop {
            let response = setup
                .rpc
                .get_bundle_statuses(Some(setup.context.clone()), vec![bundle_id.clone()])
                .await
                .expect("getBundleStatuses should succeed");

            assert_eq!(
                response.value.len(),
                1,
                "getBundleStatuses should return a single status entry per requested id"
            );

            let context_slot = response.context.slot;
            let bundle = response
                .value
                .into_iter()
                .next()
                .unwrap()
                .expect("bundle should exist locally after sendBundle");
            if bundle.slot != 0 {
                break (bundle, context_slot);
            }

            if started.elapsed() > timeout {
                break (bundle, context_slot);
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        assert!(
            context_slot >= bundle.slot,
            "response.context.slot ({}) should be >= bundle.slot ({}); \
             getBundleStatuses must surface the same context slot as the \
             underlying getSignatureStatuses call",
            context_slot,
            bundle.slot,
        );

        assert_eq!(bundle.bundle_id, bundle_id, "bundle_id should match");
        assert_eq!(
            bundle.transactions, expected_sigs,
            "transactions should match bundle signatures"
        );
        assert!(
            matches!(
                bundle.confirmation_status,
                SolanaTxConfirmationStatus::Processed
                    | SolanaTxConfirmationStatus::Confirmed
                    | SolanaTxConfirmationStatus::Finalized
            ),
            "confirmation_status should be a valid Solana status"
        );
        assert!(bundle.err.is_ok(), "err should be Ok for successful bundle");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_bundle_statuses_multi_transaction_bundle() {
        let payer = Keypair::new();
        let recipient1 = Pubkey::new_unique();
        let recipient2 = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 5 * LAMPORTS_PER_SOL);

        let tx1 = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient1,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );
        let tx2 = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient2,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );

        let tx1_encoded = bs58::encode(bincode::serialize(&tx1).unwrap()).into_string();
        let tx2_encoded = bs58::encode(bincode::serialize(&tx2).unwrap()).into_string();
        let expected_sigs = vec![tx1.signatures[0].to_string(), tx2.signatures[0].to_string()];

        let bundle_id = setup
            .rpc
            .send_bundle(
                Some(setup.context.clone()),
                vec![tx1_encoded, tx2_encoded],
                None,
            )
            .await
            .expect("sendBundle should succeed for a valid 2-tx bundle");

        let response = setup
            .rpc
            .get_bundle_statuses(Some(setup.context.clone()), vec![bundle_id.clone()])
            .await
            .expect("getBundleStatuses should succeed");

        // Multi-tx bundle must still aggregate into exactly one JitoBundleStatus, with the
        // signatures preserved in submission order.
        assert_eq!(
            response.value.len(),
            1,
            "value array must have one entry per requested bundle id"
        );
        let bundle = response
            .value
            .into_iter()
            .next()
            .unwrap()
            .expect("bundle should exist locally after sendBundle");
        assert_eq!(bundle.bundle_id, bundle_id);
        assert_eq!(
            bundle.transactions, expected_sigs,
            "transactions must preserve submission order across all txs in the bundle"
        );
        assert!(
            bundle.err.is_ok(),
            "successful multi-tx bundle should report Ok"
        );
        assert!(
            matches!(
                bundle.confirmation_status,
                SolanaTxConfirmationStatus::Processed
                    | SolanaTxConfirmationStatus::Confirmed
                    | SolanaTxConfirmationStatus::Finalized
            ),
            "confirmation_status should be a valid Solana status"
        );
    }

    #[test]
    fn test_jito_bundle_status_json_shape() {
        use solana_transaction_error::TransactionError;

        // -- Ok case: field names must be snake_case (Jito wire-compatible) and err must
        // serialize as {"Ok": null}. --
        let ok_status = JitoBundleStatus {
            bundle_id: "abc123".to_string(),
            transactions: vec!["sig1".to_string(), "sig2".to_string()],
            slot: 42,
            confirmation_status: SolanaTxConfirmationStatus::Finalized,
            err: Ok(()),
        };
        let json = serde_json::to_value(&ok_status).expect("JitoBundleStatus should serialize");

        assert!(
            json.get("bundle_id").is_some(),
            "expected snake_case `bundle_id` field, got: {json}"
        );
        assert!(json.get("transactions").is_some());
        assert!(json.get("slot").is_some());
        assert!(
            json.get("confirmationStatus").is_some(),
            "expected snake_case `confirmationStatus` field, got: {json}"
        );
        assert!(json.get("err").is_some());

        assert!(
            json.get("bundleId").is_none(),
            "camelCase `bundleId` should not be serialized (Jito uses snake_case on the wire)"
        );
        assert!(
            json.get("confirmation_status").is_none(),
            "camelCase `confirmation_status` should not be serialized"
        );

        // err must serialize as {"Ok": null} for a successful bundle.
        assert_eq!(
            json.get("err"),
            Some(&serde_json::json!({ "Ok": null })),
            "Ok variant of err should serialize as {{\"Ok\": null}}"
        );
        assert_eq!(json.get("bundle_id").unwrap().as_str(), Some("abc123"));
        assert_eq!(json.get("slot").unwrap().as_u64(), Some(42));
        assert_eq!(
            json.get("confirmationStatus").unwrap().as_str(),
            Some("finalized"),
            "confirmationStatus should serialize as a lowercase string"
        );

        // -- Err case: err must serialize as {"Err": ...} carrying the inner TransactionError. --
        let err_status = JitoBundleStatus {
            bundle_id: "abc123".to_string(),
            transactions: vec!["sig1".to_string()],
            slot: 7,
            confirmation_status: SolanaTxConfirmationStatus::Processed,
            err: Err(TransactionError::AccountNotFound),
        };
        let err_json = serde_json::to_value(&err_status).expect("err variant should serialize");
        let err_field = err_json.get("err").expect("err field should be present");
        assert!(
            err_field.get("Err").is_some(),
            "Err variant of err should serialize as {{\"Err\": ...}}, got: {err_field}"
        );

        // Round-trip: deserializing must yield the same struct.
        let round_tripped: JitoBundleStatus =
            serde_json::from_value(json).expect("JitoBundleStatus should round-trip");
        assert_eq!(round_tripped, ok_status);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_bundle_statuses_batched_known_and_unknown() {
        // Submit one real bundle, then call getBundleStatuses with a batch containing the real
        // id plus an unknown id. The response must preserve order and include `null` at the
        // unknown index, matching Jito's wire contract.
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 2 * LAMPORTS_PER_SOL);

        let tx = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );
        let tx_encoded = bs58::encode(bincode::serialize(&tx).unwrap()).into_string();

        let known_id = setup
            .rpc
            .send_bundle(Some(setup.context.clone()), vec![tx_encoded], None)
            .await
            .expect("sendBundle should succeed");
        let unknown_id = "f".repeat(64);

        // Order: [unknown, known] so we also verify positional null-vs-Some mapping isn't
        // accidentally first-only.
        let response = setup
            .rpc
            .get_bundle_statuses(
                Some(setup.context.clone()),
                vec![unknown_id.clone(), known_id.clone()],
            )
            .await
            .expect("getBundleStatuses should succeed");

        assert_eq!(
            response.value.len(),
            2,
            "value must have exactly one entry per requested bundle id"
        );
        assert!(
            response.value[0].is_none(),
            "index 0 (unknown id) should be null"
        );
        let known = response.value[1]
            .as_ref()
            .expect("index 1 (known id) should be Some(JitoBundleStatus)");
        assert_eq!(known.bundle_id, known_id);
    }

    // -----------------------------------------------------------------------
    // simulate_bundle tests
    // -----------------------------------------------------------------------

    /// Build a base64-encoded SOL transfer ready to feed into simulate_bundle.
    fn make_transfer_b64(
        payer: &Keypair,
        recipient: &Pubkey,
        lamports: u64,
        recent_blockhash: &solana_hash::Hash,
    ) -> String {
        let tx = build_v0_transaction(
            &payer.pubkey(),
            &[payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                recipient,
                lamports,
            )],
            recent_blockhash,
        );
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bincode::serialize(&tx).unwrap())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_simulate_bundle_empty_rejected() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let result = setup
            .rpc
            .simulate_bundle(
                Some(setup.context.clone()),
                RpcBundleRequest {
                    encoded_transactions: vec![],
                },
                None,
            )
            .await;
        assert!(result.is_err(), "empty bundle should be rejected");
        assert!(
            result
                .unwrap_err()
                .message
                .contains("Bundle cannot be empty"),
            "expected empty-bundle error message"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_simulate_bundle_exceeds_max_size_rejected() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let result = setup
            .rpc
            .simulate_bundle(
                Some(setup.context.clone()),
                RpcBundleRequest {
                    encoded_transactions: vec!["tx".to_string(); MAX_BUNDLE_SIZE + 1],
                },
                None,
            )
            .await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().message.contains("exceeds maximum size"),
            "expected max-size error message"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_simulate_bundle_no_context_returns_unhealthy() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let result = setup
            .rpc
            .simulate_bundle(
                None,
                RpcBundleRequest {
                    encoded_transactions: vec!["x".to_string()],
                },
                None,
            )
            .await;
        assert!(result.is_err(), "missing meta should yield NodeUnhealthy");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_simulate_bundle_replace_blockhash_requires_skip_sig_verify() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let cfg = RpcSimulateBundleConfig {
            pre_execution_accounts_configs: vec![None],
            post_execution_accounts_configs: vec![None],
            transaction_encoding: Some(UiTransactionEncoding::Base64),
            simulation_bank: None,
            skip_sig_verify: false,
            replace_recent_blockhash: true,
        };
        let result = setup
            .rpc
            .simulate_bundle(
                Some(setup.context.clone()),
                RpcBundleRequest {
                    encoded_transactions: vec!["x".to_string()],
                },
                Some(cfg),
            )
            .await;
        assert!(
            result.is_err(),
            "replace_recent_blockhash + sig verify should be rejected"
        );
        assert!(
            result
                .unwrap_err()
                .message
                .contains("replaceRecentBlockhash requires skipSigVerify=true"),
            "expected explicit camelCase JSON error about the dangerous combination"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_simulate_bundle_pre_post_lengths_must_match() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let cfg = RpcSimulateBundleConfig {
            // Only 1 pre-config but the bundle has 2 txs — must reject.
            pre_execution_accounts_configs: vec![None],
            post_execution_accounts_configs: vec![None, None],
            transaction_encoding: Some(UiTransactionEncoding::Base64),
            simulation_bank: None,
            skip_sig_verify: true,
            replace_recent_blockhash: false,
        };
        let result = setup
            .rpc
            .simulate_bundle(
                Some(setup.context.clone()),
                RpcBundleRequest {
                    encoded_transactions: vec!["a".to_string(), "b".to_string()],
                },
                Some(cfg),
            )
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .message
                .contains("must be equal in length"),
            "expected length-mismatch error"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_simulate_bundle_succeeds_does_not_mutate_live_vm() {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.latest_blockhash());

        // Fund the payer on the LIVE VM so the simulation has something to spend
        // from when its sandbox clones from the live SVM.
        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 2 * LAMPORTS_PER_SOL);

        let tx_b64 = make_transfer_b64(&payer, &recipient, LAMPORTS_PER_SOL, &recent_blockhash);

        // Pre-balance check on the live VM.
        let recipient_pre = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.get_account(&recipient))
            .ok()
            .flatten()
            .map(|a| a.lamports)
            .unwrap_or(0);
        assert_eq!(recipient_pre, 0, "recipient should start at 0 lamports");

        let cfg = RpcSimulateBundleConfig {
            pre_execution_accounts_configs: vec![Some(
                surfpool_types::RpcSimulateTransactionAccountsConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    addresses: vec![recipient.to_string()],
                },
            )],
            post_execution_accounts_configs: vec![Some(
                surfpool_types::RpcSimulateTransactionAccountsConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    addresses: vec![recipient.to_string()],
                },
            )],
            transaction_encoding: Some(UiTransactionEncoding::Base64),
            simulation_bank: None,
            skip_sig_verify: false,
            replace_recent_blockhash: false,
        };

        let response = setup
            .rpc
            .simulate_bundle(
                Some(setup.context.clone()),
                RpcBundleRequest {
                    encoded_transactions: vec![tx_b64],
                },
                Some(cfg),
            )
            .await
            .expect("simulate_bundle should not return a JSON-RPC error");

        // Bundle summary: success.
        match response.value.summary {
            RpcBundleSimulationSummary::Succeeded => {}
            other => panic!("expected Succeeded summary, got {:?}", other),
        }
        assert_eq!(response.value.transaction_results.len(), 1);
        let result = &response.value.transaction_results[0];
        assert!(result.err.is_none(), "tx should not have errored");
        assert!(
            result.units_consumed.is_some(),
            "units_consumed should be populated"
        );
        let pre = result
            .pre_execution_accounts
            .as_ref()
            .expect("pre_execution_accounts requested");
        let post = result
            .post_execution_accounts
            .as_ref()
            .expect("post_execution_accounts requested");
        assert_eq!(pre.len(), 1);
        assert_eq!(post.len(), 1);
        assert_eq!(pre[0].lamports, 0, "recipient pre = 0");
        assert_eq!(
            post[0].lamports, LAMPORTS_PER_SOL,
            "recipient post = transferred amount"
        );

        // Critically: live VM is byte-identical to its pre-call state.
        let recipient_after_sim = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.get_account(&recipient))
            .ok()
            .flatten()
            .map(|a| a.lamports)
            .unwrap_or(0);
        assert_eq!(
            recipient_after_sim, 0,
            "live VM must be untouched after simulate_bundle"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_simulate_bundle_failure_marks_summary_and_skips_remaining() {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.latest_blockhash());

        // Fund payer with only 0.5 SOL — first tx asks for 1 SOL transfer, so
        // the second tx (also 1 SOL) is guaranteed to never run. Actually the
        // first tx itself will fail because payer has insufficient funds — the
        // test asserts on fail-fast semantics: tx_results[0] errored, summary
        // = Failed, tx_results[1] left in skipped (empty) state.
        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL / 2);

        let tx1_b64 = make_transfer_b64(&payer, &recipient, LAMPORTS_PER_SOL, &recent_blockhash);
        let tx2_b64 = make_transfer_b64(&payer, &recipient, LAMPORTS_PER_SOL, &recent_blockhash);

        let response = setup
            .rpc
            .simulate_bundle(
                Some(setup.context.clone()),
                RpcBundleRequest {
                    encoded_transactions: vec![tx1_b64, tx2_b64],
                },
                None,
            )
            .await
            .expect("simulate_bundle should return Ok response (failure encoded in summary)");

        match &response.value.summary {
            RpcBundleSimulationSummary::Failed { tx_signature, .. } => {
                assert!(
                    tx_signature.is_some(),
                    "Failed summary should carry the offending tx signature"
                );
            }
            RpcBundleSimulationSummary::Succeeded => {
                panic!("bundle should have failed (insufficient funds)");
            }
        }
        assert_eq!(response.value.transaction_results.len(), 2);
        assert!(
            response.value.transaction_results[0].err.is_some(),
            "first tx should have errored"
        );
        assert!(
            response.value.transaction_results[1].err.is_none()
                && response.value.transaction_results[1].logs.is_none(),
            "second tx should be in skipped (empty) state — never simulated"
        );
    }

    /// Pins the Jito wire-format contract for null pre/post account configs.
    /// Reviewer @greptile-apps caught that we were emitting `Some([])` where
    /// the reference returns `None` — clients distinguishing "not requested"
    /// from "requested but empty" would see a false positive.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_simulate_bundle_null_account_configs_yield_none_not_empty_array() {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.latest_blockhash());

        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 2 * LAMPORTS_PER_SOL);

        let tx_b64 = make_transfer_b64(&payer, &recipient, LAMPORTS_PER_SOL, &recent_blockhash);

        // Caller does NOT request pre/post snapshots for this tx (None entry).
        let cfg = RpcSimulateBundleConfig {
            pre_execution_accounts_configs: vec![None],
            post_execution_accounts_configs: vec![None],
            transaction_encoding: Some(UiTransactionEncoding::Base64),
            simulation_bank: None,
            skip_sig_verify: false,
            replace_recent_blockhash: false,
        };

        let response = setup
            .rpc
            .simulate_bundle(
                Some(setup.context.clone()),
                RpcBundleRequest {
                    encoded_transactions: vec![tx_b64],
                },
                Some(cfg),
            )
            .await
            .expect("simulate_bundle should not error");

        let result = &response.value.transaction_results[0];
        assert!(
            result.pre_execution_accounts.is_none(),
            "pre_execution_accounts must be None when config entry is None, got {:?}",
            result.pre_execution_accounts,
        );
        assert!(
            result.post_execution_accounts.is_none(),
            "post_execution_accounts must be None when config entry is None, got {:?}",
            result.post_execution_accounts,
        );
    }

    /// Pins the Jito wire-format contract for explicit-empty pubkey lists.
    /// Distinguishing this from the None case lets clients tell "I asked for
    /// nothing" apart from "I didn't ask".
    #[tokio::test(flavor = "multi_thread")]
    async fn test_simulate_bundle_empty_addresses_yield_some_empty_vec() {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.latest_blockhash());

        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 2 * LAMPORTS_PER_SOL);

        let tx_b64 = make_transfer_b64(&payer, &recipient, LAMPORTS_PER_SOL, &recent_blockhash);

        // Caller DID request snapshots, but with an empty pubkey list.
        let cfg = RpcSimulateBundleConfig {
            pre_execution_accounts_configs: vec![Some(
                surfpool_types::RpcSimulateTransactionAccountsConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    addresses: vec![],
                },
            )],
            post_execution_accounts_configs: vec![Some(
                surfpool_types::RpcSimulateTransactionAccountsConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    addresses: vec![],
                },
            )],
            transaction_encoding: Some(UiTransactionEncoding::Base64),
            simulation_bank: None,
            skip_sig_verify: false,
            replace_recent_blockhash: false,
        };

        let response = setup
            .rpc
            .simulate_bundle(
                Some(setup.context.clone()),
                RpcBundleRequest {
                    encoded_transactions: vec![tx_b64],
                },
                Some(cfg),
            )
            .await
            .expect("simulate_bundle should not error");

        let result = &response.value.transaction_results[0];
        assert_eq!(
            result.pre_execution_accounts.as_deref(),
            Some(&[][..]),
            "pre_execution_accounts must be Some(empty) when config addresses is explicitly []",
        );
        assert_eq!(
            result.post_execution_accounts.as_deref(),
            Some(&[][..]),
            "post_execution_accounts must be Some(empty) when config addresses is explicitly []",
        );
    }

    /// Pins the rejection of non-base64 tx encodings — Jito's reference does
    /// the same. Without this guard a base58 caller would silently get a
    /// confusing "unsupported encoding" deeper in the stack.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_simulate_bundle_rejects_non_base64_encoding() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let cfg = RpcSimulateBundleConfig {
            pre_execution_accounts_configs: vec![None],
            post_execution_accounts_configs: vec![None],
            transaction_encoding: Some(UiTransactionEncoding::Base58),
            simulation_bank: None,
            skip_sig_verify: true,
            replace_recent_blockhash: false,
        };
        let result = setup
            .rpc
            .simulate_bundle(
                Some(setup.context.clone()),
                RpcBundleRequest {
                    encoded_transactions: vec!["x".to_string()],
                },
                Some(cfg),
            )
            .await;
        assert!(result.is_err(), "base58 encoding must be rejected");
        assert!(
            result
                .unwrap_err()
                .message
                .contains("Base64 is the only supported encoding"),
            "expected explicit error about base64-only enforcement"
        );
    }

    /// Pins typed `TransactionError` propagation for execution failures.
    /// A bundle whose first tx tries to spend more lamports than the wallet
    /// holds must surface the typed `InstructionError(0, Custom(1))` (or the
    /// SVM's equivalent typed err) — NOT a generic `SanitizeFailure`.
    /// Reviewer @greptile-apps + @copilot both flagged the SanitizeFailure
    /// catch-all as actively misleading.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_simulate_bundle_propagates_typed_execution_error() {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.latest_blockhash());

        // Fund the wallet with FAR less than the transfer amount asks for —
        // SVM execution will reject the tx with a typed error.
        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 1_000); // 0.000001 SOL — not enough for fees, let alone the transfer

        let tx_b64 = make_transfer_b64(&payer, &recipient, LAMPORTS_PER_SOL, &recent_blockhash);

        let response = setup
            .rpc
            .simulate_bundle(
                Some(setup.context.clone()),
                RpcBundleRequest {
                    encoded_transactions: vec![tx_b64],
                },
                None,
            )
            .await
            .expect("simulate_bundle should return Ok with a Failed summary");

        // Bundle summary should be Failed.
        match &response.value.summary {
            RpcBundleSimulationSummary::Failed { tx_signature, .. } => {
                assert!(
                    tx_signature.is_some(),
                    "Failed summary must carry signature"
                );
            }
            other => panic!("expected Failed summary, got {:?}", other),
        }

        let err = response.value.transaction_results[0]
            .err
            .as_ref()
            .expect("err must be populated for a failed tx");
        // The typed error should NOT be SanitizeFailure (the previous
        // implementation's catch-all). For an insufficient-funds-during-
        // execution path the SVM typically reports either an
        // InstructionError or a typed transaction error like
        // InsufficientFundsForRent — anything BUT SanitizeFailure is the
        // win we are pinning.
        assert!(
            !matches!(
                err,
                solana_transaction_error::TransactionError::SanitizeFailure
            ),
            "execution failure must surface a typed err, not SanitizeFailure (got {:?})",
            err,
        );
    }

    /// Pins the SignatureFailure mapping for sigverify failures. A bundle with
    /// a tx whose signature has been corrupted (post-sign mutation) and
    /// `skip_sig_verify=false` must surface `TransactionError::SignatureFailure`
    /// in the typed err — NOT `SanitizeFailure` (the previous catch-all that
    /// reviewer @greptile-apps flagged) and NOT a generic untyped err.
    /// `SanitizeFailure` semantically means structurally malformed; the right
    /// variant here is the same one `svm::sigverify` and the single-tx
    /// simulate path already use.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_simulate_bundle_propagates_typed_signature_failure() {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.latest_blockhash());

        // Build a valid transfer tx, then corrupt the first signature byte —
        // the surrounding message stays well-formed (passes sanitize) but the
        // signature no longer verifies.
        let mut tx = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient,
                1_000,
            )],
            &recent_blockhash,
        );
        let mut sig_bytes = tx.signatures[0].as_ref().to_vec();
        sig_bytes[0] = sig_bytes[0].wrapping_add(1);
        tx.signatures[0] = Signature::try_from(sig_bytes.as_slice()).unwrap();

        use base64::Engine;
        let tx_b64 =
            base64::engine::general_purpose::STANDARD.encode(bincode::serialize(&tx).unwrap());

        let response = setup
            .rpc
            .simulate_bundle(
                Some(setup.context.clone()),
                RpcBundleRequest {
                    encoded_transactions: vec![tx_b64],
                },
                Some(RpcSimulateBundleConfig {
                    pre_execution_accounts_configs: vec![None],
                    post_execution_accounts_configs: vec![None],
                    transaction_encoding: Some(UiTransactionEncoding::Base64),
                    simulation_bank: None,
                    // Sigverify ON — we want to exercise the verification path.
                    skip_sig_verify: false,
                    replace_recent_blockhash: false,
                }),
            )
            .await
            .expect("simulate_bundle should return Ok with a Failed summary");

        match &response.value.summary {
            RpcBundleSimulationSummary::Failed { .. } => {}
            other => panic!("expected Failed summary, got {:?}", other),
        }

        let err = response.value.transaction_results[0]
            .err
            .as_ref()
            .expect("err must be populated for a sig-verify failure");
        assert!(
            matches!(
                err,
                solana_transaction_error::TransactionError::SignatureFailure
            ),
            "sig-verify failure must surface SignatureFailure (got {:?})",
            err,
        );
    }

    /// Pins the up-front rejection of sig-less transactions. Reviewer
    /// @copilot flagged that `unwrap_or_default()` on a None signature
    /// would inject an all-zero `Signature` into the
    /// `RpcBundleExecutionError::TransactionFailure(Signature, _)` wire
    /// shape — clients keying off the sig inside `error` would be misled.
    /// Rejecting the tx at decode time avoids the synthesis entirely.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_simulate_bundle_rejects_sigless_tx() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.latest_blockhash());

        // Build a versioned tx with NO signatures. Note: Solana's signing
        // helpers always inject the fee-payer sig, so we go through the
        // raw constructor and leave signatures empty.
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let msg = VersionedMessage::V0(
            V0Message::try_compile(
                &payer.pubkey(),
                &[system_instruction::transfer(
                    &payer.pubkey(),
                    &recipient,
                    1_000,
                )],
                &[],
                recent_blockhash,
            )
            .unwrap(),
        );
        let tx = VersionedTransaction {
            signatures: vec![], // explicit empty
            message: msg,
        };
        use base64::Engine;
        let tx_b64 =
            base64::engine::general_purpose::STANDARD.encode(bincode::serialize(&tx).unwrap());

        let result = setup
            .rpc
            .simulate_bundle(
                Some(setup.context.clone()),
                RpcBundleRequest {
                    encoded_transactions: vec![tx_b64],
                },
                None,
            )
            .await;

        assert!(
            result.is_err(),
            "sig-less tx must be rejected at decode time"
        );
        let err = result.unwrap_err();
        assert!(
            err.message.contains("has no signatures"),
            "expected explicit no-signatures rejection (got {})",
            err.message,
        );
    }

    /// Pins the partial-config behavior. Reviewer @copilot flagged that
    /// `RpcSimulateBundleConfig` previously required both
    /// `pre_execution_accounts_configs` and `post_execution_accounts_configs`
    /// to be present in JSON, contradicting the docstring's implication
    /// that callers may send a partial config. Now both fields are
    /// `#[serde(default)]` and an empty/omitted vec is treated as
    /// `vec![None; bundle_len]`.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_simulate_bundle_accepts_partial_config_omitting_account_configs() {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.latest_blockhash());

        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 10 * LAMPORTS_PER_SOL);

        // Send 1 SOL — well above rent-exempt minimum so recipient creation
        // doesn't fail with InsufficientFundsForRent.
        let tx_b64 = make_transfer_b64(&payer, &recipient, LAMPORTS_PER_SOL, &recent_blockhash);

        // Partial config: only skip_sig_verify is set, both pre/post account
        // config arrays are omitted (default = empty vec). Server must
        // expand them to vec![None; bundle_len] and accept the request.
        let cfg = RpcSimulateBundleConfig {
            pre_execution_accounts_configs: vec![],
            post_execution_accounts_configs: vec![],
            transaction_encoding: Some(UiTransactionEncoding::Base64),
            simulation_bank: None,
            skip_sig_verify: true,
            replace_recent_blockhash: false,
        };

        let response = setup
            .rpc
            .simulate_bundle(
                Some(setup.context.clone()),
                RpcBundleRequest {
                    encoded_transactions: vec![tx_b64],
                },
                Some(cfg),
            )
            .await
            .expect("partial config must be accepted");

        match &response.value.summary {
            RpcBundleSimulationSummary::Succeeded => {}
            other => panic!("expected Succeeded summary, got {:?}", other),
        }
        let result = &response.value.transaction_results[0];
        assert!(
            result.pre_execution_accounts.is_none(),
            "omitted pre config must yield None (got {:?})",
            result.pre_execution_accounts,
        );
        assert!(
            result.post_execution_accounts.is_none(),
            "omitted post config must yield None (got {:?})",
            result.post_execution_accounts,
        );
    }
}
