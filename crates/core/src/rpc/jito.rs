use std::str::FromStr;

use jsonrpc_core::{BoxFuture, Error, Result};
use jsonrpc_derive::rpc;
use sha2::{Digest, Sha256};
use solana_client::{rpc_config::RpcSendTransactionConfig, rpc_custom_error::RpcCustomError};
use solana_rpc_client_api::response::Response as RpcResponse;
use solana_signature::Signature;
use solana_transaction_error::TransactionError;
use solana_transaction_status::TransactionConfirmationStatus;
use surfpool_types::JitoBundleStatus;

use super::{
    RunloopContext,
    full::{Full, SurfpoolFullRpc, SurfpoolRpcSendTransactionConfig},
};

/// Maximum number of transactions allowed in a single bundle, matching Jito's limit.
const MAX_BUNDLE_SIZE: usize = 5;

/// Jito-specific RPC methods for bundle submission
#[rpc]
pub trait Jito {
    type Metadata;

    /// Sends a bundle of transactions to be processed sequentially.
    ///
    /// This RPC method accepts a bundle of transactions (Jito-compatible format) and processes them
    /// one by one in order. All transactions in the bundle must succeed for the bundle to be accepted.
    ///
    /// ## Parameters
    /// - `transactions`: An array of serialized transaction data (base64 or base58 encoded).
    /// - `config`: Optional configuration for encoding format.
    ///
    /// ## Returns
    /// - `Result<String>`: A bundle ID (SHA-256 hash of comma-separated signatures).
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
    /// - Bundles are limited to a maximum of 5 transactions, matching Jito's limit
    /// - Transactions are processed sequentially in the order provided
    /// - Each transaction must complete successfully before the next one starts
    /// - If any transaction fails, an error is returned — however, earlier transactions in the
    ///   bundle that already succeeded are NOT rolled back (not atomic)
    /// - TODO(#594): implement atomic all-or-nothing bundle execution
    /// - The bundle ID is calculated as SHA-256 hash of comma-separated transaction signatures
    #[rpc(meta, name = "sendBundle")]
    fn send_bundle(
        &self,
        meta: Self::Metadata,
        transactions: Vec<String>,
        config: Option<RpcSendTransactionConfig>,
    ) -> Result<String>;

    /// Retrieves a Jito-shaped status summary for a previously submitted bundle.
    ///
    /// Looks up the bundle by `bundle_id` (the hex string returned by
    /// [`sendBundle`](#method.send_bundle)), resolves per-signature status via the same path as
    /// `getSignatureStatuses`, and returns a single aggregate status object (not one entry per signature).
    ///
    /// ## Parameters
    /// - `bundle_id`: The bundle identifier returned by `sendBundle`.
    ///
    /// ## Returns
    /// On success, the JSON-RPC `result` is either:
    /// - **`null`**: No bundle is known for this `bundle_id` locally (no stored signatures — the id may be
    ///   valid elsewhere, we simply have nothing to report).
    /// - **An object** matching Solana’s contextualized RPC shape:
    /// - `context.slot`: Context slot from the underlying status query (same idea as `getSignatureStatuses`).
    /// - `value`: An array with **one** element of type `surfpool_types::JitoBundleStatus`, with:
    ///   - `bundle_id`: The requested bundle id.
    ///   - `transactions`: Base-58 signatures in bundle submission order (from local `jito_bundles` storage).
    ///   - `slot`: Slot from the first per-signature status entry (bundle txs share a landing slot), or `0` if none yet.
    ///   - `confirmation_status`: From that same first entry (defaults to `processed` when absent).
    ///   - `err`: `Ok` if no transaction error was observed on any status; otherwise the first `Err` encountered
    ///     (JSON-serialized like other Solana `Result` values, e.g. `{"Ok": null}` or `{"Err": ...}`).
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getBundleStatuses",
    ///   "params": [
    ///     "bundleIdHere"
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
    ///       }
    ///     ]
    ///   }
    /// }
    /// ```
    ///
    /// When the bundle is not known locally, `result` is JSON `null`:
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "result": null
    /// }
    /// ```
    ///
    /// ## Notes
    /// - Bundles are stored locally as a mapping from `bundle_id` to a list of base-58 signatures.
    /// - If the bundle ID is not known locally, **`result` is `null`**, not a JSON-RPC error (Jito-style).
    /// - Per-signature status resolution uses the same logic as `getSignatureStatuses` (local store and optional remote datasource).
    #[rpc(meta, name = "getBundleStatuses")]
    fn get_bundle_statuses(
        &self,
        meta: Self::Metadata,
        bundle_id: String,
    ) -> BoxFuture<Result<Option<RpcResponse<Vec<JitoBundleStatus>>>>>;
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
    ) -> Result<String> {
        if transactions.is_empty() {
            return Err(Error::invalid_params("Bundle cannot be empty"));
        }

        if transactions.len() > MAX_BUNDLE_SIZE {
            return Err(Error::invalid_params(format!(
                "Bundle exceeds maximum size of {MAX_BUNDLE_SIZE} transactions"
            )));
        }

        let Some(ctx) = &meta else {
            return Err(RpcCustomError::NodeUnhealthy {
                num_slots_behind: None,
            }
            .into());
        };

        let full_rpc = SurfpoolFullRpc;
        let mut bundle_signatures = Vec::new();
        let base_config = config.unwrap_or_default();

        // Process each transaction in the bundle sequentially using Full RPC
        // Force skip_preflight to match Jito Block Engine behavior (no simulation on sendBundle)
        // NOTE: this is not atomic — earlier transactions are NOT rolled back if a later one fails.
        // TODO(#594): implement atomic all-or-nothing bundle execution
        for (idx, tx_data) in transactions.iter().enumerate() {
            let bundle_config = Some(SurfpoolRpcSendTransactionConfig {
                base: RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..base_config
                },
                skip_sig_verify: None,
            });
            // Delegate to Full RPC's sendTransaction method
            match full_rpc.send_transaction(meta.clone(), tx_data.clone(), bundle_config) {
                Ok(signature_str) => {
                    // Parse the signature to collect for bundle ID calculation
                    let signature = Signature::from_str(&signature_str).map_err(|e| {
                        Error::invalid_params(format!("Failed to parse signature: {e}"))
                    })?;
                    bundle_signatures.push(signature);
                }
                Err(e) => {
                    // Add bundle transaction index to error message
                    return Err(Error {
                        code: e.code,
                        message: format!("Bundle transaction {} failed: {}", idx, e.message),
                        data: e.data,
                    });
                }
            }
        }

        // Calculate bundle ID by hashing comma-separated signatures (Jito-compatible)
        // https://github.com/jito-foundation/jito-solana/blob/master/sdk/src/bundle/mod.rs#L21
        let concatenated_signatures = bundle_signatures
            .iter()
            .map(|sig| sig.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut hasher = Sha256::new();
        hasher.update(concatenated_signatures.as_bytes());
        let bundle_id = hasher.finalize();
        let bundle_id = hex::encode(bundle_id);

        ctx.svm_locker.store_bundle(
            bundle_id.clone(),
            bundle_signatures
                .iter()
                .map(|sig| sig.to_string())
                .collect(),
        )?;

        Ok(bundle_id)
    }

    fn get_bundle_statuses(
        &self,
        meta: Self::Metadata,
        bundle_id: String,
    ) -> BoxFuture<Result<Option<RpcResponse<Vec<JitoBundleStatus>>>>> {
        Box::pin(async move {
            let Some(ctx) = &meta else {
                return Err(RpcCustomError::NodeUnhealthy {
                    num_slots_behind: None,
                }
                .into());
            };

            let Some(signatures) = ctx.svm_locker.get_bundle(&bundle_id) else {
                return Ok(None);
            };
            if signatures.is_empty() {
                return Ok(None);
            }

            let statuses = SurfpoolFullRpc
                .get_signature_statuses(meta.clone(), signatures.clone(), None)
                .await?;

            // Bundle txs are processed sequentially in one go; they share the same landing slot and
            // confirmation level, so we take slot/status from the first status entry only.
            let mut slot = 0u64;
            let mut confirmation_status: Option<TransactionConfirmationStatus> = None;
            let mut first_err: Option<TransactionError> = None;
            let mut took_bundle_fields = false;

            for status in statuses.value.iter().flatten() {
                if !took_bundle_fields {
                    slot = status.slot;
                    confirmation_status = status.confirmation_status.clone();
                    took_bundle_fields = true;
                }

                if first_err.is_none()
                    && let Some(err) = status.err.clone()
                {
                    first_err = Some(err);
                }
            }

            let confirmation_status =
                confirmation_status.unwrap_or(TransactionConfirmationStatus::Processed);

            let bundle_status = JitoBundleStatus {
                bundle_id,
                transactions: signatures,
                slot,
                confirmation_status,
                err: match first_err {
                    Some(e) => Err(e),
                    None => Ok(()),
                },
            };

            Ok(Some(RpcResponse {
                context: statuses.context,
                value: vec![bundle_status],
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use crossbeam_channel::Receiver;
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

    /// `send_transaction` / `send_bundle` enqueue `ProcessTransaction` on `simnet_commands_tx` and
    /// block until a consumer completes the work and signals on `status_update_tx`. Unit tests must
    /// drain the mempool like the real simnet loop (see `full.rs` `send_and_await_transaction`).
    async fn drain_one_process_transaction(
        setup: &TestSetup<SurfpoolJitoRpc>,
        mempool_rx: &Receiver<SimnetCommand>,
    ) {
        loop {
            match mempool_rx.recv() {
                Ok(SimnetCommand::ProcessTransaction(_, tx, status_tx, _, _)) => {
                    let mut writer = setup.context.svm_locker.0.write().await;
                    let slot = writer.get_latest_absolute_slot();
                    writer.transactions_queued_for_confirmation.push_back((
                        tx.clone(),
                        status_tx.clone(),
                        None,
                    ));
                    let sig = tx.signatures[0];
                    let tx_with_status_meta = TransactionWithStatusMeta {
                        slot,
                        transaction: tx,
                        ..Default::default()
                    };
                    let mutated_accounts = std::collections::HashSet::new();
                    writer
                        .transactions
                        .store(
                            sig.to_string(),
                            SurfnetTransactionStatus::processed(
                                tx_with_status_meta,
                                mutated_accounts,
                            ),
                        )
                        .unwrap();
                    status_tx
                        .send(TransactionStatusEvent::Success(
                            TransactionConfirmationStatus::Confirmed,
                        ))
                        .unwrap();
                    return;
                }
                Ok(_) => continue,
                Err(e) => panic!("mempool recv failed: {e}"),
            }
        }
    }

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

    #[test]
    fn test_send_bundle_empty_bundle_rejected() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let result = setup.rpc.send_bundle(Some(setup.context), vec![], None);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("Bundle cannot be empty"),
            "Expected 'Bundle cannot be empty' error, got: {}",
            err.message
        );
    }

    #[test]
    fn test_send_bundle_exceeds_max_size_rejected() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let transactions = vec!["tx".to_string(); MAX_BUNDLE_SIZE + 1];
        let result = setup
            .rpc
            .send_bundle(Some(setup.context), transactions, None);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("exceeds maximum size"),
            "Expected max size error, got: {}",
            err.message
        );
    }

    #[test]
    fn test_send_bundle_no_context_returns_unhealthy() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let result = setup
            .rpc
            .send_bundle(None, vec!["some_tx".to_string()], None);

        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_bundle_statuses_unknown_bundle_returns_null() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let missing_id = "a".repeat(64);
        let out = setup
            .rpc
            .get_bundle_statuses(Some(setup.context), missing_id)
            .await
            .expect("getBundleStatuses should not return a JSON-RPC error");
        assert!(
            out.is_none(),
            "unknown bundle_id should serialize as null result (Ok(None))"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_bundle_statuses_no_context_returns_unhealthy() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let result = setup.rpc.get_bundle_statuses(None, "a".repeat(64)).await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_single_transaction() {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let (mempool_tx, mempool_rx) = crossbeam_channel::unbounded();
        let setup = TestSetup::new_with_mempool(SurfpoolJitoRpc, mempool_tx);
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

        let setup_clone = setup.clone();
        let handle = hiro_system_kit::thread_named("send_bundle")
            .spawn(move || {
                setup_clone
                    .rpc
                    .send_bundle(Some(setup_clone.context), vec![tx_encoded], None)
            })
            .unwrap();

        drain_one_process_transaction(&setup, &mempool_rx).await;

        let result = handle.join().unwrap();
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
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_multiple_transactions() {
        let payer = Keypair::new();
        let recipient1 = Pubkey::new_unique();
        let recipient2 = Pubkey::new_unique();
        let (mempool_tx, mempool_rx) = crossbeam_channel::unbounded();
        let setup = TestSetup::new_with_mempool(SurfpoolJitoRpc, mempool_tx);
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

        let setup_clone = setup.clone();
        let handle = hiro_system_kit::thread_named("send_bundle")
            .spawn(move || {
                setup_clone.rpc.send_bundle(
                    Some(setup_clone.context),
                    vec![tx1_encoded, tx2_encoded],
                    None,
                )
            })
            .unwrap();

        drain_one_process_transaction(&setup, &mempool_rx).await;
        drain_one_process_transaction(&setup, &mempool_rx).await;

        let result = handle.join().unwrap();

        assert!(result.is_ok(), "Bundle should succeed: {:?}", result);

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
    async fn test_send_bundle_persists_bundle_signatures() {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let (mempool_tx, mempool_rx) = crossbeam_channel::unbounded();
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
        let handle = hiro_system_kit::thread_named("send_bundle")
            .spawn(move || {
                setup_clone
                    .rpc
                    .send_bundle(Some(setup_clone.context), vec![tx_encoded], None)
            })
            .unwrap();

        drain_one_process_transaction(&setup, &mempool_rx).await;

        let send_bundle_result = handle.join().unwrap();

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
        let bundle = loop {
            let response = setup
                .rpc
                .get_bundle_statuses(Some(setup.context.clone()), bundle_id.clone())
                .await
                .expect("getBundleStatuses should succeed")
                .expect("bundle should exist locally after sendBundle");

            assert_eq!(
                response.value.len(),
                1,
                "getBundleStatuses should return a single bundle status"
            );

            let bundle = response.value.into_iter().next().unwrap();
            if bundle.slot != 0 {
                break bundle;
            }

            if started.elapsed() > timeout {
                break bundle;
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        };

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
}
