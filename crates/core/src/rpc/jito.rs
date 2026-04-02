use std::str::FromStr;

use jsonrpc_core::{BoxFuture, Error, Result};
use jsonrpc_derive::rpc;
use sha2::{Digest, Sha256};
use solana_client::{rpc_config::RpcSendTransactionConfig, rpc_custom_error::RpcCustomError};
use solana_rpc_client_api::response::Response as RpcResponse;
use solana_signature::Signature;
use solana_transaction_status::TransactionStatus;

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

    /// Retrieves the statuses of all transactions in a previously submitted bundle.
    ///
    /// This RPC method looks up a bundle by its `bundle_id` (the SHA-256 hash returned by
    /// [`sendBundle`](#method.send_bundle)) and returns the signature statuses for the bundle's
    /// transactions in the same order they were recorded.
    ///
    /// ## Parameters
    /// - `bundle_id`: The bundle identifier returned by `sendBundle`.
    ///
    /// ## Returns
    /// A contextualized response containing:
    /// - `value`: A list of optional transaction statuses corresponding to the bundle signatures.
    ///   Each entry can be:
    ///   - `null` if the signature is unknown or not sufficiently confirmed for status reporting
    ///   - a `TransactionStatus` object if the transaction is found and its status can be returned
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
    /// ## Notes
    /// - Bundles are stored locally as a mapping from `bundle_id` to a list of base-58 signatures.
    /// - If the bundle ID is not known locally, an error is returned.
    /// - Status resolution is delegated to the same logic used by `getSignatureStatuses`:
    ///   statuses are computed from locally stored transactions (and may fall back to a remote
    ///   datasource, if configured).
    #[rpc(meta, name = "getBundleStatuses")]
    fn get_bundle_statuses(
        &self,
        meta: Self::Metadata,
        bundle_id: String,
    ) -> BoxFuture<Result<RpcResponse<Vec<Option<TransactionStatus>>>>>;
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

        let _ = ctx
            .simnet_commands_tx
            .send(surfpool_types::SimnetCommand::SendBundle((
                bundle_id.clone(),
                bundle_signatures
                    .iter()
                    .map(|sig| sig.to_string())
                    .collect(),
            )));

        Ok(bundle_id)
    }

    fn get_bundle_statuses(
        &self,
        meta: Self::Metadata,
        bundle_id: String,
    ) -> BoxFuture<Result<RpcResponse<Vec<Option<TransactionStatus>>>>> {
        Box::pin(async move {
            let Some(ctx) = &meta else {
                return Err(RpcCustomError::NodeUnhealthy {
                    num_slots_behind: None,
                }
                .into());
            };

            let signatures = ctx.svm_locker.get_bundle(bundle_id)?;

            SurfpoolFullRpc
                .get_signature_statuses(meta.clone(), signatures, None)
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    use solana_keypair::Keypair;
    use solana_message::{VersionedMessage, v0::Message as V0Message};
    use solana_pubkey::Pubkey;
    use solana_signer::Signer;
    use solana_system_interface::instruction as system_instruction;
    use solana_transaction::versioned::VersionedTransaction;
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

        // Process the transaction from mempool
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
                    break;
                }
                Ok(SimnetCommand::AirdropProcessed) => continue,
                _ => panic!("failed to receive transaction from mempool"),
            }
        }

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

        // Process both transactions from mempool
        let mut processed_count = 0;
        while processed_count < 2 {
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
                    processed_count += 1;
                }
                Ok(SimnetCommand::AirdropProcessed) => continue,
                _ => panic!("failed to receive transaction from mempool"),
            }
        }

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

        let mut processed_tx = false;
        let mut processed_bundle = false;
        let mut bundle_id_from_cmd: Option<String> = None;
        let mut sigs_from_cmd: Option<Vec<String>> = None;

        while !(processed_tx && processed_bundle) {
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
                    writer
                        .transactions
                        .store(
                            sig.to_string(),
                            SurfnetTransactionStatus::processed(
                                tx_with_status_meta,
                                std::collections::HashSet::new(),
                            ),
                        )
                        .unwrap();
                    status_tx
                        .send(TransactionStatusEvent::Success(
                            TransactionConfirmationStatus::Confirmed,
                        ))
                        .unwrap();
                    processed_tx = true;
                }
                Ok(SimnetCommand::SendBundle((bundle_id, signatures))) => {
                    setup
                        .context
                        .svm_locker
                        .process_bundle(bundle_id.clone(), signatures.clone())
                        .unwrap();
                    bundle_id_from_cmd = Some(bundle_id);
                    sigs_from_cmd = Some(signatures);
                    processed_bundle = true;
                }
                Ok(SimnetCommand::AirdropProcessed) => continue,
                other => panic!("unexpected simnet command: {:?}", other),
            }
        }

        let result = handle.join().unwrap().expect("sendBundle should succeed");
        let stored_bundle_id = bundle_id_from_cmd.expect("should have received SendBundle command");
        assert_eq!(
            result, stored_bundle_id,
            "sendBundle result bundle id should match stored bundle id"
        );

        let persisted = setup
            .context
            .svm_locker
            .get_bundle(stored_bundle_id.clone())
            .expect("bundle should be persisted");
        assert!(
            !persisted.is_empty(),
            "svm_locker.get_bundle(bundle_id) should not be empty"
        );

        let sigs_from_cmd = sigs_from_cmd.expect("should have captured signatures from SendBundle");
        assert_eq!(
            sigs_from_cmd, expected_sigs,
            "Signatures in SendBundle command should match locally built signatures"
        );
        assert_eq!(
            persisted, expected_sigs,
            "Persisted bundle signatures should match locally built signatures"
        );
    }
}
