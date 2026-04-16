use jsonrpc_core::{Error, Result};
use jsonrpc_derive::rpc;
use sha2::{Digest, Sha256};
use solana_client::{rpc_config::RpcSendTransactionConfig, rpc_custom_error::RpcCustomError};
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status::UiTransactionEncoding;
use surfpool_types::TransactionStatusEvent;

use super::{
    RunloopContext,
    full::{Full, SurfpoolFullRpc, SurfpoolRpcSendTransactionConfig},
    utils::decode_and_deserialize,
};
use crate::surfnet::locker::SurfnetSvmLocker;

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

        let base_config = config.unwrap_or_default();

        // Decode all bundle transactions up front so we can run them against a disposable clone.
        let tx_encoding = base_config
            .encoding
            .unwrap_or(UiTransactionEncoding::Base58);
        let binary_encoding = tx_encoding.into_binary_encoding().ok_or_else(|| {
            Error::invalid_params(format!(
                "unsupported encoding: {tx_encoding}. Supported encodings: base58, base64"
            ))
        })?;

        // execute the whole bundle on a disposable sandbox VM
        // by cloning the svm state into a temporary svm_state
        let sandbox_svm = ctx
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.clone_for_profiling());
        let sandbox_svm_locker = SurfnetSvmLocker::new(sandbox_svm);

        // For now, keep bundle execution local-only; remote ctx / commitment plumbs can be added later.
        let remote_ctx = &None;
        let skip_preflight = true;
        let sigverify = true;

        let mut bundle_signatures: Vec<Signature> = Vec::with_capacity(transactions.len());
        for (idx, tx_data) in transactions.iter().enumerate() {
            let (_, tx) =
                decode_and_deserialize::<VersionedTransaction>(tx_data.clone(), binary_encoding)
                    .map_err(|e| {
                        let err_message = e.message;
                        let code = e.code;
                        let data = e.data;
                        Error {
                            code,
                            message: format!(
                                "Failed to decode bundle transaction {}: {}",
                                idx + 1,
                                err_message
                            ),
                            data,
                        }
                    })?;

            let (status_tx, status_rx) = crossbeam_channel::bounded(transactions.len());
            let process_res =
                hiro_system_kit::nestable_block_on(sandbox_svm_locker.process_transaction(
                    remote_ctx,
                    tx.clone(),
                    status_tx,
                    skip_preflight,
                    sigverify,
                ));

            let signature = tx.signatures[0];
            bundle_signatures.push(signature);

            if let Err(e) = process_res {
                return Err(Error::invalid_params(format!(
                    "Jito bundle couldn't be executed, failed to process transaction {}: {e}",
                    idx + 1
                )));
            }

            // `process_transaction` can return `Ok(())` even when the tx failed, because the
            // failure is communicated through the status channel. Treat any non-success status
            // as a bundle failure.
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

        // TODO: I think it'd be best if we were to copy the changed accounts states and directly change
        // the original svm state? It's a bit complecated, but will avoid having to send the transactions twice
        let full_rpc = SurfpoolFullRpc;
        for (idx, tx_data) in transactions.iter().enumerate() {
            let bundle_config = Some(SurfpoolRpcSendTransactionConfig {
                base: RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..base_config.clone()
                },
                skip_sig_verify: None,
            });
            if let Err(e) = full_rpc.send_transaction(meta.clone(), tx_data.clone(), bundle_config)
            {
                return Err(Error {
                    code: e.code,
                    message: format!("Bundle transaction {} failed: {}", idx, e.message),
                    data: e.data,
                });
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
        Ok(hex::encode(bundle_id))
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
    use surfpool_types::{SimnetCommand, TransactionConfirmationStatus, TransactionStatusEvent};

    use super::*;
    use crate::{
        surfnet::GetAccountResult,
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

        let setup_clone = setup.clone();
        let handle =
            hiro_system_kit::thread_named("send_bundle_dependent_failure").spawn(move || {
                setup_clone.rpc.send_bundle(
                    Some(setup_clone.context),
                    vec![tx1_encoded, tx2_encoded],
                    None,
                )
            });
        let handle = handle.unwrap();

        let result = handle.join().unwrap();

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

    #[test]
    fn test_send_bundle_simulation_failure_returns_not_atomic_error() {
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
            .send_bundle(Some(setup.context), vec![tx_encoded], None);

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
}
