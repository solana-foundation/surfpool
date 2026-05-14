use std::{str::FromStr, sync::Arc, time::Duration};

use base64::Engine;
use crossbeam_channel::{unbounded, unbounded as crossbeam_unbounded};
use ed25519_dalek::Signer as DalekSigner;
use jsonrpc_core::{
    Error, Result as JsonRpcResult,
    futures::future::{self, join_all},
};
use jsonrpc_core_client::transports::http;
use solana_account::Account;
use solana_account_decoder::{UiAccountData, UiAccountEncoding, parse_account_data::ParsedAccount};
use solana_address_lookup_table_interface::state::{AddressLookupTable, LookupTableMeta};
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_config::{RpcSendTransactionConfig, RpcSimulateTransactionConfig},
    rpc_response::RpcLogsResponse,
};
use solana_clock::{Clock, Slot};
use solana_commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_ed25519_program::new_ed25519_instruction_with_signature;
use solana_epoch_info::EpochInfo;
use solana_epoch_schedule::EpochSchedule;
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_message::{
    AddressLookupTableAccount, Message, MessageHeader, VersionedMessage, legacy,
    v0::{self, MessageAddressTableLookup},
};
use solana_pubkey::Pubkey;
use solana_rpc_client_api::response::Response as RpcResponse;
use solana_signer::Signer;
use solana_system_interface::{
    instruction as system_instruction, instruction::transfer, program as system_program,
};
use solana_transaction::{Transaction, versioned::VersionedTransaction};
use surfpool_types::{
    CheatcodeConfig, CheatcodeControlConfig, CheatcodeFilter, DEFAULT_SLOT_TIME_MS, Idl,
    RpcProfileDepth, RpcProfileResultConfig, SimnetCommand, SimnetEvent, SurfpoolConfig,
    UiAccountChange, UiAccountProfileState, UiKeyedProfileResult,
    types::{
        BlockProductionMode, RpcConfig, SimnetConfig, SubgraphConfig, TransactionStatusEvent,
        UuidOrSignature,
    },
};
use test_case::test_case;
use tokio::{sync::RwLock, task};
use uuid::Uuid;
pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

use crate::{
    error::SurfpoolError,
    rpc::{
        RunloopContext,
        full::{FullClient, SurfpoolRpcSendTransactionConfig},
        minimal::MinimalClient,
        surfnet_cheatcodes::{SurfnetCheatcodes, SurfnetCheatcodesRpc},
    },
    runloops::start_local_surfnet_runloop,
    storage::tests::TestType,
    surfnet::{
        FINALIZATION_SLOT_THRESHOLD, PluginCommand, SignatureSubscriptionType,
        locker::SurfnetSvmLocker, svm::SurfnetSvm,
    },
    tests::helpers::get_free_port,
    types::{TimeTravelConfig, TransactionLoadedAddresses},
};

fn wait_for_ready_and_connected(simnet_events_rx: &crossbeam_channel::Receiver<SimnetEvent>) {
    let mut ready = false;
    let mut connected = false;
    loop {
        match simnet_events_rx.recv() {
            Ok(SimnetEvent::Ready(_)) => {
                println!("Simnet is ready");
                ready = true;
            }
            Ok(SimnetEvent::Connected(_)) => {
                println!("Simnet is connected");
                connected = true;
            }
            _ => (),
        }
        if ready && connected {
            break;
        }
    }
}

#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test]
async fn test_simnet_ready(test_type: TestType) {
    let config = SurfpoolConfig {
        simnets: vec![SimnetConfig {
            block_production_mode: BlockProductionMode::Manual, // Prevent ticks
            ..SimnetConfig::default()
        }],
        ..SurfpoolConfig::default()
    };

    let (surfnet_svm, simnet_events_rx, geyser_events_rx) = test_type.initialize_svm();
    let (simnet_commands_tx, simnet_commands_rx) = unbounded();

    let svm_locker = SurfnetSvmLocker::new(surfnet_svm);

    let _handle = hiro_system_kit::thread_named("test").spawn(move || {
        let future = start_local_surfnet_runloop(
            svm_locker,
            config,
            simnet_commands_tx,
            simnet_commands_rx,
            geyser_events_rx,
        );
        if let Err(e) = hiro_system_kit::nestable_block_on(future) {
            panic!("{e:?}");
        }
    });

    loop {
        match simnet_events_rx.recv() {
            Ok(SimnetEvent::Ready(_))
            | Ok(SimnetEvent::Connected(_))
            | Ok(SimnetEvent::EpochInfoUpdate(_)) => break,
            e => panic!("Expected startup event: {e:?}"),
        }
    }
}

#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test]
async fn test_simnet_ticks(test_type: TestType) {
    let bind_host = "127.0.0.1";
    let bind_port = get_free_port().unwrap();
    let ws_port = get_free_port().unwrap();
    let config = SurfpoolConfig {
        simnets: vec![SimnetConfig {
            slot_time: 1,
            ..SimnetConfig::default()
        }],
        rpc: RpcConfig {
            bind_host: bind_host.to_string(),
            bind_port,
            ws_port,
            ..Default::default()
        },
        ..SurfpoolConfig::default()
    };

    let (surfnet_svm, simnet_events_rx, geyser_events_rx) = test_type.initialize_svm();
    let (simnet_commands_tx, simnet_commands_rx) = unbounded();

    let (test_tx, test_rx) = unbounded();
    let svm_locker = SurfnetSvmLocker::new(surfnet_svm);

    let _handle = hiro_system_kit::thread_named("test").spawn(move || {
        let future = start_local_surfnet_runloop(
            svm_locker,
            config,
            simnet_commands_tx,
            simnet_commands_rx,
            geyser_events_rx,
        );
        if let Err(e) = hiro_system_kit::nestable_block_on(future) {
            panic!("{e:?}");
        }
    });

    let _ = hiro_system_kit::thread_named("ticks").spawn(move || {
        let mut ticks = 0;
        loop {
            match simnet_events_rx.recv() {
                Ok(SimnetEvent::SystemClockUpdated(_)) => ticks += 1,
                _ => (),
            }

            if ticks > 100 {
                let _ = test_tx.send(true);
            }
        }
    });

    match test_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(_) => (),
        Err(e) => panic!("not enough ticks: {e:?}"),
    }
}

#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test]
async fn test_simnet_some_sol_transfers(test_type: TestType) {
    let n_addresses = 10;
    let airdrop_keypairs = (0..n_addresses).map(|_| Keypair::new()).collect::<Vec<_>>();
    let airdrop_addresses: Vec<Pubkey> = airdrop_keypairs.iter().map(|kp| kp.pubkey()).collect();
    let airdrop_token_amount = LAMPORTS_PER_SOL;
    let bind_host = "127.0.0.1";
    let bind_port = get_free_port().unwrap();
    let ws_port = get_free_port().unwrap();
    let config = SurfpoolConfig {
        simnets: vec![SimnetConfig {
            slot_time: 1,
            airdrop_addresses: airdrop_addresses.clone(),
            airdrop_token_amount,
            ..SimnetConfig::default()
        }],
        rpc: RpcConfig {
            bind_host: bind_host.to_string(),
            bind_port,
            ws_port,
            ..Default::default()
        },
        ..SurfpoolConfig::default()
    };

    let (surfnet_svm, simnet_events_rx, geyser_events_rx) = test_type.initialize_svm();
    let (simnet_commands_tx, simnet_commands_rx) = unbounded();

    let svm_locker = SurfnetSvmLocker::new(surfnet_svm);

    let _handle = hiro_system_kit::thread_named("test").spawn(move || {
        let future = start_local_surfnet_runloop(
            svm_locker,
            config,
            simnet_commands_tx,
            simnet_commands_rx,
            geyser_events_rx,
        );
        if let Err(e) = hiro_system_kit::nestable_block_on(future) {
            panic!("{e:?}");
        }
    });

    wait_for_ready_and_connected(&simnet_events_rx);

    let minimal_client =
        http::connect::<MinimalClient>(format!("http://{bind_host}:{bind_port}").as_str())
            .await
            .expect("Failed to connect to Surfpool");
    let full_client =
        http::connect::<FullClient>(format!("http://{bind_host}:{bind_port}").as_str())
            .await
            .expect("Failed to connect to Surfpool");

    let recent_blockhash = full_client
        .get_latest_blockhash(None)
        .await
        .map(|r| {
            Hash::from_str(r.value.blockhash.as_str()).expect("Failed to deserialize blockhash")
        })
        .expect("Failed to get blockhash");

    let balances = join_all(
        airdrop_addresses
            .iter()
            .map(|pk| minimal_client.get_balance(pk.to_string(), None)),
    )
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .expect("Failed to fetch balances");

    assert!(
        balances.iter().all(|b| b.value == airdrop_token_amount),
        "All addresses did not receive the airdrop"
    );

    let _transfers = join_all(airdrop_keypairs.iter().map(|kp| {
        let msg = Message::new_with_blockhash(
            &[system_instruction::transfer(
                &kp.pubkey(),
                &airdrop_addresses[0],
                airdrop_token_amount / 2,
            )],
            Some(&kp.pubkey()),
            &recent_blockhash,
        );

        let Ok(tx) = VersionedTransaction::try_new(
            VersionedMessage::Legacy(msg),
            &vec![kp.insecure_clone()],
        ) else {
            return Box::pin(future::err(Error::invalid_params("tx")));
        };

        let Ok(encoded) = bincode::serialize(&tx) else {
            return Box::pin(future::err(Error::invalid_params("encoded")));
        };
        let data = bs58::encode(encoded).into_string();

        Box::pin(future::ready(Ok(full_client.send_transaction(data, None))))
    }))
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .expect("Transfers failed");

    // Wait for all transactions to be received
    let expected = airdrop_addresses.len();
    let _ = task::spawn_blocking(move || {
        let mut processed = 0;
        loop {
            match simnet_events_rx.recv() {
                Ok(SimnetEvent::TransactionProcessed(..)) => processed += 1,
                _ => (),
            }

            if processed == expected {
                break;
            }
        }
    })
    .await;

    let final_balances = join_all(
        airdrop_addresses
            .iter()
            .map(|pk| minimal_client.get_balance(pk.to_string(), None)),
    )
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .expect("Failed to fetch final balances");

    assert!(
        final_balances.iter().enumerate().all(|(i, b)| {
            if i == 0 {
                b.value > airdrop_token_amount
            } else {
                b.value < airdrop_token_amount / 2
            }
        }), // TODO: compute fee
        "Some transfers failed"
    );
}

// This test is pretty minimal for lookup tables at this point.
// We are creating a v0 transaction with a lookup table that does exist on mainnet,
// and sending that tx to surfpool. We are verifying that the transaction is processed
// and that the lookup table and its entries are fetched from mainnet and added to the accounts in the SVM.
// However, we are not actually setting up a tx that will use the lookup table internally,
// we are kind of just trusting that LiteSVM will do its job here.
#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_add_alt_entries_fetching(test_type: TestType) {
    let payer = Keypair::new();
    let pk = payer.pubkey();

    let bind_host = "127.0.0.1";
    let bind_port = get_free_port().unwrap();
    let ws_port = get_free_port().unwrap();
    let airdrop_token_amount = LAMPORTS_PER_SOL;
    let config = SurfpoolConfig {
        simnets: vec![SimnetConfig {
            slot_time: 1,
            airdrop_addresses: vec![pk], // just one
            airdrop_token_amount,
            ..SimnetConfig::default()
        }],
        rpc: RpcConfig {
            bind_host: bind_host.to_string(),
            bind_port,
            ws_port,
            ..Default::default()
        },
        ..SurfpoolConfig::default()
    };

    println!("Initializing SVM, binding to port {}", bind_port);
    let (surfnet_svm, simnet_events_rx, geyser_events_rx) = test_type.initialize_svm();
    let (simnet_commands_tx, simnet_commands_rx) = unbounded();

    let svm_locker = Arc::new(RwLock::new(surfnet_svm));

    let moved_svm_locker = svm_locker.clone();
    let _handle = hiro_system_kit::thread_named("test").spawn(move || {
        let future = start_local_surfnet_runloop(
            SurfnetSvmLocker(moved_svm_locker),
            config,
            simnet_commands_tx,
            simnet_commands_rx,
            geyser_events_rx,
        );
        if let Err(e) = hiro_system_kit::nestable_block_on(future) {
            panic!("{e:?}");
        }
    });
    let svm_locker = SurfnetSvmLocker(svm_locker);

    wait_for_ready_and_connected(&simnet_events_rx);

    let full_client =
        http::connect::<FullClient>(format!("http://{bind_host}:{bind_port}").as_str())
            .await
            .expect("Failed to connect to Surfpool");

    let recent_blockhash = full_client
        .get_latest_blockhash(None)
        .await
        .map(|r| {
            Hash::from_str(r.value.blockhash.as_str()).expect("Failed to deserialize blockhash")
        })
        .expect("Failed to get blockhash");

    let random_address = Pubkey::from_str_const("7zdYkYf7yD83j3TLXmkhxn6LjQP9y9bQ4pjfpquP8Hqw");

    let instruction = transfer(&pk, &random_address, 100);

    let alt_address = Pubkey::from_str_const("5KcPJehcpBLcPde2UhmY4dE9zCrv2r9AKFmW5CGtY1io"); // a mainnet lookup table

    let address_lookup_table_account = AddressLookupTableAccount {
        key: alt_address,
        addresses: vec![random_address],
    };

    let tx = VersionedTransaction::try_new(
        VersionedMessage::V0(
            v0::Message::try_compile(
                &payer.pubkey(),
                &[instruction],
                &[address_lookup_table_account],
                recent_blockhash,
            )
            .expect("Failed to compile message"),
        ),
        &[payer],
    )
    .expect("Failed to create transaction");

    let Ok(encoded) = bincode::serialize(&tx) else {
        panic!("Failed to serialize transaction");
    };
    let data = bs58::encode(encoded).into_string();

    // Wait for all transactions to be received
    let _ = match full_client.send_transaction(data, None).await {
        Ok(res) => println!("Send transaction result: {}", res),
        Err(err) => println!("Send transaction error result: {}", err),
    };

    let mut processed = 0;
    let expected = 1;
    let mut alt_updated = false;
    loop {
        match simnet_events_rx.recv() {
            Ok(SimnetEvent::TransactionProcessed(..)) => processed += 1,
            Ok(SimnetEvent::AccountUpdate(_, account)) => {
                if account == alt_address {
                    alt_updated = true;
                }
            }
            Ok(SimnetEvent::ClockUpdate(_)) => {
                // do nothing
            }
            Ok(SimnetEvent::SystemClockUpdated(_)) => {
                // do nothing - clock ticks from time travel or normal progression
            }
            other => println!("Unexpected event: {:?}", other),
        }

        if processed == expected && alt_updated {
            break;
        }
    }

    // get all the account keys + the address lookup tables + table_entries from the txn
    let alts = tx.message.address_table_lookups().clone().unwrap();
    let mut acc_keys = tx.message.static_account_keys().to_vec();
    let mut alt_pubkeys = alts.iter().map(|msg| msg.account_key).collect::<Vec<_>>();
    let mut table_entries = join_all(alts.iter().map(|msg| async {
        let mut loaded_addresses = TransactionLoadedAddresses::new();
        svm_locker
            .get_lookup_table_addresses(&None, msg, &mut loaded_addresses)
            .await?;
        Ok::<_, SurfpoolError>(
            loaded_addresses
                .all_loaded_addresses()
                .into_iter()
                .map(|p| *p)
                .collect::<Vec<Pubkey>>(),
        )
    }))
    .await
    .into_iter()
    .collect::<Result<Vec<Vec<Pubkey>>, SurfpoolError>>()
    .unwrap()
    .into_iter()
    .flatten()
    .collect();

    acc_keys.append(&mut alt_pubkeys);
    acc_keys.append(&mut table_entries);

    assert!(
        acc_keys.iter().all(|key| {
            svm_locker
                .get_account_local(key)
                .inner
                .map_account()
                .is_ok()
        }),
        "account not found"
    );
}

// This test is pretty minimal for lookup tables at this point.
// We are creating a v0 transaction with a lookup table that does exist on mainnet,
// and sending that tx to surfpool. We are verifying that the transaction is processed
// and that the lookup table and its entries are fetched from mainnet and added to the accounts in the SVM.
// However, we are not actually setting up a tx that will use the lookup table internally,
// we are kind of just trusting that LiteSVM will do its job here.
#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_simulate_add_alt_entries_fetching(test_type: TestType) {
    let payer = Keypair::new();
    let pk = payer.pubkey();

    let bind_host = "127.0.0.1";
    let bind_port = get_free_port().unwrap();
    let ws_port = get_free_port().unwrap();
    let airdrop_token_amount = LAMPORTS_PER_SOL;
    let config = SurfpoolConfig {
        simnets: vec![SimnetConfig {
            slot_time: 1,
            airdrop_addresses: vec![pk], // just one
            airdrop_token_amount,
            ..SimnetConfig::default()
        }],
        rpc: RpcConfig {
            bind_host: bind_host.to_string(),
            bind_port,
            ws_port,
            ..Default::default()
        },
        ..SurfpoolConfig::default()
    };

    let (surfnet_svm, simnet_events_rx, geyser_events_rx) = test_type.initialize_svm();
    let (simnet_commands_tx, simnet_commands_rx) = unbounded();

    let svm_locker = Arc::new(RwLock::new(surfnet_svm));

    let moved_svm_locker = svm_locker.clone();
    let _handle = hiro_system_kit::thread_named("test").spawn(move || {
        let future = start_local_surfnet_runloop(
            SurfnetSvmLocker(moved_svm_locker),
            config,
            simnet_commands_tx,
            simnet_commands_rx,
            geyser_events_rx,
        );
        if let Err(e) = hiro_system_kit::nestable_block_on(future) {
            panic!("{e:?}");
        }
    });
    let svm_locker = SurfnetSvmLocker(svm_locker);

    wait_for_ready_and_connected(&simnet_events_rx);

    let full_client =
        http::connect::<FullClient>(format!("http://{bind_host}:{bind_port}").as_str())
            .await
            .expect("Failed to connect to Surfpool");

    let random_address = Pubkey::from_str_const("7zdYkYf7yD83j3TLXmkhxn6LjQP9y9bQ4pjfpquP8Hqw");

    let instruction = transfer(&pk, &random_address, 100);
    let recent_blockhash = svm_locker.with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

    let alt_address = Pubkey::from_str_const("5KcPJehcpBLcPde2UhmY4dE9zCrv2r9AKFmW5CGtY1io"); // a mainnet lookup table

    let address_lookup_table_account = AddressLookupTableAccount {
        key: alt_address,
        addresses: vec![random_address],
    };

    let tx = VersionedTransaction::try_new(
        VersionedMessage::V0(
            v0::Message::try_compile(
                &payer.pubkey(),
                &[instruction],
                &[address_lookup_table_account],
                recent_blockhash,
            )
            .expect("Failed to compile message"),
        ),
        &[payer],
    )
    .expect("Failed to create transaction");

    let Ok(encoded) = bincode::serialize(&tx) else {
        panic!("Failed to serialize transaction");
    };
    let data = bs58::encode(encoded).into_string();

    let simulation_res = full_client
        .simulate_transaction(data.clone(), None)
        .await
        .unwrap();
    assert_eq!(
        simulation_res.value.err, None,
        "Unexpected simulation error"
    );
    assert!(
        simulation_res.value.loaded_accounts_data_size.is_some(),
        "Expected loaded_accounts_data_size to be present"
    );
    assert_eq!(
        simulation_res.value.loaded_accounts_data_size.unwrap(),
        140134,
        "Incorrect loaded_accounts_data_size value"
    );
    let simulation_res2 = full_client
        .simulate_transaction(
            data,
            Some(RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: false,
                commitment: Some(CommitmentConfig::confirmed()),
                encoding: None,
                accounts: None,
                min_context_slot: None,
                inner_instructions: false,
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        simulation_res2.value.err, None,
        "Unexpected simulation error"
    );
    assert!(
        simulation_res2.value.loaded_accounts_data_size.is_some(),
        "Expected loaded_accounts_data_size to be present"
    );
    assert!(
        simulation_res2.value.loaded_accounts_data_size.unwrap() > 0,
        "Expected loaded_accounts_data_size to be greater than 0"
    );
}

#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_simulate_transaction_no_signers(test_type: TestType) {
    let payer = Keypair::new();
    let pk = payer.pubkey();
    let lamports = LAMPORTS_PER_SOL;

    let bind_host = "127.0.0.1";
    let bind_port = get_free_port().unwrap();
    let ws_port = get_free_port().unwrap();
    let airdrop_token_amount = LAMPORTS_PER_SOL;
    let config = SurfpoolConfig {
        simnets: vec![SimnetConfig {
            slot_time: 1,
            airdrop_addresses: vec![pk], // just one
            airdrop_token_amount,
            ..SimnetConfig::default()
        }],
        rpc: RpcConfig {
            bind_host: bind_host.to_string(),
            bind_port,
            ws_port,
            ..Default::default()
        },
        ..SurfpoolConfig::default()
    };

    let (surfnet_svm, simnet_events_rx, geyser_events_rx) = test_type.initialize_svm();
    let (simnet_commands_tx, simnet_commands_rx) = unbounded();

    let svm_locker = Arc::new(RwLock::new(surfnet_svm));

    let moved_svm_locker = svm_locker.clone();
    let _handle = hiro_system_kit::thread_named("test").spawn(move || {
        let future = start_local_surfnet_runloop(
            SurfnetSvmLocker(moved_svm_locker),
            config,
            simnet_commands_tx,
            simnet_commands_rx,
            geyser_events_rx,
        );
        if let Err(e) = hiro_system_kit::nestable_block_on(future) {
            panic!("{e:?}");
        }
    });
    let svm_locker = SurfnetSvmLocker(svm_locker);

    wait_for_ready_and_connected(&simnet_events_rx);

    let full_client =
        http::connect::<FullClient>(format!("http://{bind_host}:{bind_port}").as_str())
            .await
            .expect("Failed to connect to Surfpool");
    let _ = full_client
        .request_airdrop(payer.pubkey().to_string(), 2 * lamports, None)
        .await;

    let recent_blockhash = svm_locker
        .get_latest_blockhash(&CommitmentConfig::confirmed())
        .unwrap();
    //build_legacy_transaction
    let mut msg = legacy::Message::new(
        &[system_instruction::transfer(&payer.pubkey(), &pk, lamports)],
        Some(&payer.pubkey()),
    );
    msg.recent_blockhash = recent_blockhash;
    let tx = Transaction::new_unsigned(msg);

    let simulation_res = full_client
        .simulate_transaction(
            bs58::encode(bincode::serialize(&tx).unwrap()).into_string(),
            Some(RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: false,
                commitment: Some(CommitmentConfig::finalized()),
                encoding: None,
                accounts: None,
                min_context_slot: None,
                inner_instructions: false,
            }),
        )
        .await
        .unwrap();

    assert_eq!(
        simulation_res.value.err, None,
        "Unexpected simulation error"
    );
    assert!(
        simulation_res.value.loaded_accounts_data_size.is_some(),
        "Expected loaded_accounts_data_size to be present"
    );
    assert!(
        simulation_res.value.loaded_accounts_data_size.unwrap() > 0,
        "Expected loaded_accounts_data_size to be greater than 0"
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_transaction_with_ed25519_instruction(test_type: TestType) {
    let payer = Keypair::new();
    let (mut svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    svm_instance
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();
    let recent_blockhash = svm_instance.latest_blockhash();

    // Issue #587 previously failed here with:
    // "Transaction simulation failed: Error processing Instruction 2: Unsupported program id".
    let tx = {
        let payer_dalek =
            ed25519_dalek::Keypair::from_bytes(&payer.to_bytes()).expect("invalid dalek keypair");
        let message = b"surfpool ed25519 precompile integration test";
        let signature = payer_dalek.sign(message);
        let ed25519_ix = new_ed25519_instruction_with_signature(
            message,
            &signature.to_bytes(),
            payer.pubkey().as_array(),
        );
        let compute_budget_ixs = [
            ComputeBudgetInstruction::set_compute_unit_limit(100_000),
            ComputeBudgetInstruction::set_compute_unit_price(1),
        ];
        let tx_message = Message::new_with_blockhash(
            &[
                compute_budget_ixs[0].clone(),
                compute_budget_ixs[1].clone(),
                ed25519_ix,
            ],
            Some(&payer.pubkey()),
            &recent_blockhash,
        );

        VersionedTransaction::try_new(VersionedMessage::Legacy(tx_message), &[payer])
            .expect("Failed to create ed25519 transaction")
    };
    let svm_locker = SurfnetSvmLocker::new(svm_instance);
    let simulation_res = svm_locker.simulate_transaction(tx.clone(), true);

    assert!(
        simulation_res.is_ok(),
        "Expected ed25519 transaction simulation to succeed"
    );

    let (status_tx, status_rx) = unbounded();
    let _ = svm_locker
        .process_transaction(&None, tx, status_tx, true, true)
        .await
        .unwrap();

    // Wait for transaction processing
    match status_rx.recv() {
        Ok(TransactionStatusEvent::Success(_)) => {
            println!("Transaction processed successfully");
        }
        Ok(TransactionStatusEvent::SimulationFailure((error, _))) => {
            panic!("Transaction simulation failed: {:?}", error);
        }
        Ok(TransactionStatusEvent::ExecutionFailure((error, _))) => {
            panic!("Transaction execution failed: {:?}", error);
        }
        Ok(TransactionStatusEvent::VerificationFailure(error)) => {
            panic!("Transaction verification failed: {}", error);
        }
        Err(e) => {
            panic!("Failed to receive transaction status: {:?}", e);
        }
    }
}

#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_surfnet_estimate_compute_units(test_type: TestType) {
    let (mut svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let rpc_server = crate::rpc::surfnet_cheatcodes::SurfnetCheatcodesRpc::empty();

    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    let lamports_to_send = 1_000_000;

    svm_instance
        .airdrop(&payer.pubkey(), lamports_to_send * 2)
        .unwrap()
        .unwrap();

    let instruction = transfer(&payer.pubkey(), &recipient, lamports_to_send);
    let latest_blockhash = svm_instance.latest_blockhash();
    let message =
        Message::new_with_blockhash(&[instruction], Some(&payer.pubkey()), &latest_blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message.clone()), &[&payer])
        .unwrap();

    let tx_bytes = bincode::serialize(&tx).unwrap();
    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

    // Manually construct RunloopContext
    let svm_locker_for_context = SurfnetSvmLocker::new(svm_instance);
    let (simnet_cmd_tx, _simnet_cmd_rx) = crossbeam_unbounded::<SimnetCommand>();
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker_for_context.clone(),
        simnet_commands_tx: simnet_cmd_tx,
        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    // Test with None tag
    let response_no_tag_initial: JsonRpcResult<RpcResponse<UiKeyedProfileResult>> = rpc_server
        .profile_transaction(Some(runloop_context.clone()), tx_b64.clone(), None, None)
        .await;

    assert!(
        response_no_tag_initial.is_ok(),
        "RPC call with None tag failed: {:?}",
        response_no_tag_initial.err()
    );
    let rpc_response_value_no_tag = response_no_tag_initial.unwrap().value;

    assert!(
        rpc_response_value_no_tag
            .transaction_profile
            .error_message
            .is_none(),
        "CU estimation with None tag failed"
    );

    assert!(
        rpc_response_value_no_tag
            .transaction_profile
            .compute_units_consumed
            > 0,
        "Invalid compute units consumed for None tag"
    );

    assert!(
        rpc_response_value_no_tag
            .transaction_profile
            .log_messages
            .is_some(),
        "Log messages should be present for None tag"
    );

    // Test 1: Estimate with a tag and retrieve
    let tag1 = "test_tag_1".to_string();
    println!("\nTesting with tag: {}", tag1);
    let response_tagged_1: JsonRpcResult<RpcResponse<UiKeyedProfileResult>> = rpc_server
        .profile_transaction(
            Some(runloop_context.clone()),
            tx_b64.clone(),
            Some(tag1.clone()),
            None,
        )
        .await;
    assert!(
        response_tagged_1.is_ok(),
        "RPC call with tag1 failed: {:?}",
        response_tagged_1.err()
    );
    let rpc_response_tagged_1_value = response_tagged_1.unwrap().value;
    assert!(
        rpc_response_tagged_1_value
            .transaction_profile
            .error_message
            .is_none(),
        "CU estimation with tag1 failed"
    );

    println!("Retrieving profile results for tag: {}", tag1);
    let results_vec_tag1 = rpc_server
        .get_profile_results_by_tag(Some(runloop_context.clone()), tag1.clone(), None)
        .unwrap()
        .value
        .unwrap_or_default();
    assert_eq!(results_vec_tag1.len(), 1, "Expected 1 result for tag1");

    assert_eq!(
        results_vec_tag1[0]
            .transaction_profile
            .compute_units_consumed,
        rpc_response_tagged_1_value
            .transaction_profile
            .compute_units_consumed
    );
    assert_eq!(
        results_vec_tag1[0]
            .transaction_profile
            .error_message
            .is_none(),
        rpc_response_tagged_1_value
            .transaction_profile
            .error_message
            .is_none()
    );

    // Test 2: Retrieve with a non-existent tag
    let tag_non_existent = "non_existent_tag".to_string();
    println!(
        "\nTesting retrieval with non-existent tag: {}",
        tag_non_existent
    );
    let results_non_existent_vec = rpc_server
        .get_profile_results_by_tag(
            Some(runloop_context.clone()),
            tag_non_existent.clone(),
            None,
        )
        .unwrap()
        .value
        .unwrap_or_default();

    assert!(
        results_non_existent_vec.is_empty(),
        "Expected empty vec for non-existent tag"
    );

    // Test 3: Estimate multiple times with the same tag
    let tag2 = "test_tag_2".to_string();
    println!("\nTesting multiple estimations with tag: {}", tag2);
    let response_tagged_2a: JsonRpcResult<RpcResponse<UiKeyedProfileResult>> = rpc_server
        .profile_transaction(
            Some(runloop_context.clone()),
            tx_b64.clone(),
            Some(tag2.clone()),
            None,
        )
        .await;
    assert!(response_tagged_2a.is_ok(), "First call with tag2 failed");
    let cu_2a_profile_result = response_tagged_2a.unwrap().value;
    println!(
        "CU estimation 1 (tag: {}): consumed = {}, success = {}",
        tag2,
        cu_2a_profile_result
            .transaction_profile
            .compute_units_consumed,
        cu_2a_profile_result
            .transaction_profile
            .error_message
            .is_none()
    );

    let response_tagged_2b: JsonRpcResult<RpcResponse<UiKeyedProfileResult>> = rpc_server
        .profile_transaction(
            Some(runloop_context.clone()),
            tx_b64.clone(),
            Some(tag2.clone()),
            None,
        )
        .await;
    assert!(response_tagged_2b.is_ok(), "Second call with tag2 failed");
    let cu_2b_profile_result = response_tagged_2b.unwrap().value;

    println!("Retrieving profile results for tag: {}", tag2);
    let results_response_tag2 =
        rpc_server.get_profile_results_by_tag(Some(runloop_context.clone()), tag2.clone(), None);

    assert!(
        results_response_tag2.is_ok(),
        "get_profile_results for tag2 failed"
    );
    let results_vec_tag2 = results_response_tag2.unwrap().value.unwrap_or_default();
    assert_eq!(results_vec_tag2.len(), 2, "Expected 2 results for tag2");

    assert_eq!(
        results_vec_tag2[0]
            .transaction_profile
            .compute_units_consumed,
        cu_2a_profile_result
            .transaction_profile
            .compute_units_consumed
    );

    assert_eq!(
        results_vec_tag2[1]
            .transaction_profile
            .compute_units_consumed,
        cu_2b_profile_result
            .transaction_profile
            .compute_units_consumed
    );

    // Test 4: Estimate with another None tag, ensure it doesn't affect tagged results for tag1
    println!(
        "\nTesting None tag again to ensure no interference with tag: {}",
        tag1
    );
    let response_no_tag_again: JsonRpcResult<RpcResponse<UiKeyedProfileResult>> = rpc_server
        .profile_transaction(Some(runloop_context.clone()), tx_b64.clone(), None, None)
        .await;
    assert!(
        response_no_tag_again.is_ok(),
        "RPC call with None tag (again) failed"
    );
    let _rpc_response_no_tag_again_value = response_no_tag_again.unwrap().value;

    println!("Retrieving profile results for tag: {} again", tag1);
    let results_response_tag1_again =
        rpc_server.get_profile_results_by_tag(Some(runloop_context), tag1.clone(), None);
    assert!(
        results_response_tag1_again.is_ok(),
        "get_profile_results for tag1 (again) failed"
    );
    let results_vec_tag1_again = results_response_tag1_again
        .unwrap()
        .value
        .unwrap_or_default();
    assert_eq!(
        results_vec_tag1_again.len(),
        1,
        "Expected 1 result for tag1 after another None tag call, was {}",
        results_vec_tag1_again.len()
    );
    assert_eq!(
        results_vec_tag1_again[0]
            .transaction_profile
            .compute_units_consumed,
        rpc_response_tagged_1_value
            .transaction_profile
            .compute_units_consumed
    );

    // Test send_transaction with cu_analysis_enabled = true
    // Create a new SVM instance
    let (mut svm_for_send, simnet_rx_for_send, _geyser_rx_for_send) = test_type.initialize_svm();
    svm_for_send
        .airdrop(&payer.pubkey(), lamports_to_send * 2)
        .unwrap()
        .unwrap();

    let latest_blockhash_for_send = svm_for_send.latest_blockhash();
    let message_for_send = Message::new_with_blockhash(
        &[transfer(&payer.pubkey(), &recipient, lamports_to_send)],
        Some(&payer.pubkey()),
        &latest_blockhash_for_send,
    );
    let tx_for_send =
        VersionedTransaction::try_new(VersionedMessage::Legacy(message_for_send), &[&payer])
            .unwrap();

    let _send_result = svm_for_send.send_transaction(tx_for_send, true, true);

    let mut found_cu_event = false;
    for _ in 0..10 {
        if let Ok(event) = simnet_rx_for_send.try_recv() {
            if let surfpool_types::SimnetEvent::InfoLog(_, msg) = event {
                if msg.starts_with("CU Estimation for tx") {
                    println!("Found CU estimation event: {}", msg);
                    found_cu_event = true;
                    break;
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
    assert!(found_cu_event, "Did not find CU estimation SimnetEvent");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
fn test_enable_and_disable_cheatcodes(test_type: TestType) {
    let valid_cheatcode_method = "surfnet_getActiveIdl".to_string();
    let enable_cheatcode_method = "surfnet_enableCheatcode".to_string();
    let disable_cheatcode_method = "surfnet_disableCheatcode".to_string();
    let invalid_cheatcode_method = "surfnet_invalidCheatcode".to_string();
    let available_methods = vec![
        valid_cheatcode_method.clone(),
        enable_cheatcode_method.clone(),
        disable_cheatcode_method.clone(),
    ];
    let rpc_server = SurfnetCheatcodesRpc {
        registered_methods: Arc::new(std::sync::RwLock::new(available_methods.clone())),
    };
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker_for_context = SurfnetSvmLocker::new(svm_instance);
    let (simnet_cmd_tx, _simnet_cmd_rx) = crossbeam_unbounded::<SimnetCommand>();
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker_for_context.clone(),
        simnet_commands_tx: simnet_cmd_tx,
        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    // Test disable works properly
    let disable_cheatcode_pass_result = rpc_server.disable_cheatcode(
        Some(runloop_context.clone()),
        CheatcodeFilter::List(vec![valid_cheatcode_method.clone()]),
        None,
    );

    assert!(
        disable_cheatcode_pass_result.is_ok(),
        "Expected surfnet_disableCheatcode to pass"
    );

    assert!(
        runloop_context
            .cheatcode_config
            .lock()
            .unwrap()
            .is_cheatcode_disabled(&valid_cheatcode_method),
        "Expected cheatcode to be disabled"
    );

    let disable_enable_cheatcode_lockout_false_fails = rpc_server.disable_cheatcode(
        Some(runloop_context.clone()),
        CheatcodeFilter::List(vec![enable_cheatcode_method.clone()]),
        None,
    );

    let expected_error: jsonrpc_core::Result<()> = Err(SurfpoolError::disable_cheatcode(
        "Cannot disable surfnet_enableCheatcode or surfnet_disableCheatcode rpc method when lockout is not enabled".to_string(),
    )
    .into());

    assert!(
        disable_enable_cheatcode_lockout_false_fails.is_err(),
        "Expected surfnet_disableCheatcode to fail when disabling surfnet_enableCheatcode with lockout == false"
    );
    assert_eq!(
        disable_enable_cheatcode_lockout_false_fails.err().unwrap(),
        expected_error.err().unwrap(),
        "Expected error did not match the resulting error"
    );

    // Test that surfnet_disableCheatcode cannot be disabled without lockout
    let disable_disable_cheatcode_lockout_false_fails = rpc_server.disable_cheatcode(
        Some(runloop_context.clone()),
        CheatcodeFilter::List(vec![disable_cheatcode_method.clone()]),
        None,
    );

    let expected_error_disable_self: jsonrpc_core::Result<()> = Err(SurfpoolError::disable_cheatcode(
        "Cannot disable surfnet_enableCheatcode or surfnet_disableCheatcode rpc method when lockout is not enabled".to_string(),
    )
    .into());

    assert!(
        disable_disable_cheatcode_lockout_false_fails.is_err(),
        "Expected surfnet_disableCheatcode to fail when disabling surfnet_disableCheatcode with lockout == false"
    );
    assert_eq!(
        disable_disable_cheatcode_lockout_false_fails.err().unwrap(),
        expected_error_disable_self.err().unwrap(),
        "Expected error did not match the resulting error"
    );

    let disable_cheatcode_fails_on_invalid_cheatcode = rpc_server.disable_cheatcode(
        Some(runloop_context.clone()),
        CheatcodeFilter::List(vec![invalid_cheatcode_method.clone()]),
        None,
    );

    let expected_error_disable_invalid_cheatcode: jsonrpc_core::Result<()> =
        Err(SurfpoolError::disable_cheatcode("Invalid cheatcode rpc method".to_string()).into());

    assert!(
        disable_cheatcode_fails_on_invalid_cheatcode.is_err(),
        "Expected surfnet_disableCheatcode to fail on providing invalid cheatcode method"
    );

    assert_eq!(
        expected_error_disable_invalid_cheatcode.err().unwrap(),
        disable_cheatcode_fails_on_invalid_cheatcode.err().unwrap(),
        "Resulting error does not match the expected error"
    );

    let disable_all_cheatcodes_fails_with_invalid_config = rpc_server.disable_cheatcode(
        Some(runloop_context.clone()),
        CheatcodeFilter::All("not_all".to_string()),
        None,
    );
    let expected_error_disable_all_with_invalid_config: jsonrpc_core::Result<()> =
        Err(SurfpoolError::disable_cheatcode(
            "Invalid option provided for disabling all cheatcodes. Try using 'all' or providing an array of specific cheatcodes".to_string(),
        )
        .into());

    assert!(
        disable_all_cheatcodes_fails_with_invalid_config.is_err(),
        "Expected surfnet_disableCheatcode to fail when config is wrong"
    );
    assert_eq!(
        disable_all_cheatcodes_fails_with_invalid_config
            .err()
            .unwrap(),
        expected_error_disable_all_with_invalid_config
            .err()
            .unwrap(),
        "Expected error did not match the resulting error"
    );

    let disabled_all_cheatcode_passes_result = rpc_server.disable_cheatcode(
        Some(runloop_context.clone()),
        CheatcodeFilter::All("all".to_string()),
        None,
    );

    assert!(
        disabled_all_cheatcode_passes_result.is_ok(),
        "Expected surfnet_disableCheatcode to pass with valid config"
    );

    assert_eq!(
        runloop_context.cheatcode_config.lock().unwrap().filter,
        CheatcodeConfig::filter_all_list(false, available_methods.clone()),
        "The disabled cheatcodes list doesn't match the expected one"
    );

    let disable_fails_if_cheatcode_already_disabled_result = rpc_server.disable_cheatcode(
        Some(runloop_context.clone()),
        CheatcodeFilter::List(vec![valid_cheatcode_method.clone()]),
        None,
    );

    let expected_error_if_cheatcode_already_disabled: jsonrpc_core::Result<()> =
        Err(SurfpoolError::disable_cheatcode("Cheatcode already disabled".to_string()).into());

    assert!(
        disable_fails_if_cheatcode_already_disabled_result.is_err(),
        "Expected surfnet_disableCheatcode to fail if cheatcode already disabled"
    );

    assert_eq!(
        disable_fails_if_cheatcode_already_disabled_result
            .err()
            .unwrap(),
        expected_error_if_cheatcode_already_disabled.err().unwrap(),
        "The expected error does not match the resulting error"
    );

    // test enable works properly
    let enable_cheatcode_works_properly_result = rpc_server.enable_cheatcode(
        Some(runloop_context.clone()),
        CheatcodeFilter::List(vec![valid_cheatcode_method.clone()]),
    );

    assert!(
        enable_cheatcode_works_properly_result.is_ok(),
        "expected surfnet_enableCheatcode to pass"
    );
    assert!(
        !runloop_context
            .cheatcode_config
            .lock()
            .unwrap()
            .is_cheatcode_disabled(&valid_cheatcode_method),
        "Expected the cheatcode to be enabled after surnet_enableCheatcode rpc method"
    );

    let enable_cheatcode_fails_on_invalid_cheatcode = rpc_server.enable_cheatcode(
        Some(runloop_context.clone()),
        CheatcodeFilter::List(vec![invalid_cheatcode_method]),
    );

    let expected_error_enable_invalid_cheatcode: jsonrpc_core::Result<()> =
        Err(SurfpoolError::enable_cheatcode("Invalid cheatcode rpc method".to_string()).into());

    assert!(
        enable_cheatcode_fails_on_invalid_cheatcode.is_err(),
        "Expected surfnet_disableCheatcode to fail on providing invalid cheatcode method"
    );

    assert_eq!(
        enable_cheatcode_fails_on_invalid_cheatcode.err().unwrap(),
        expected_error_enable_invalid_cheatcode.err().unwrap(),
        "Resulting error does not match the expected error"
    );

    let enable_all_cheatcodes_fails_with_invalid_config = rpc_server.enable_cheatcode(
        Some(runloop_context.clone()),
        CheatcodeFilter::All("not_all".to_string()),
    );
    let expected_error_enable_all_with_invalid_config: jsonrpc_core::Result<()> =
        Err(SurfpoolError::enable_cheatcode(
            "Invalid option provided for enabling all cheatcodes. Try using 'all' or providing an array of specific cheatcodes".to_string(),
        )
        .into());

    assert!(
        enable_all_cheatcodes_fails_with_invalid_config.is_err(),
        "Expected surfnet_disableCheatcode to fail when config is wrong"
    );
    assert_eq!(
        enable_all_cheatcodes_fails_with_invalid_config
            .err()
            .unwrap(),
        expected_error_enable_all_with_invalid_config.err().unwrap(),
        "Expected error did not match the resulting error"
    );

    let enable_all_cheatcodes_pass_result = rpc_server.enable_cheatcode(
        Some(runloop_context.clone()),
        CheatcodeFilter::All("all".to_string()),
    );

    assert!(
        enable_all_cheatcodes_pass_result.is_ok(),
        "Expected surnet_enableCheatcode with correct config to pass"
    );

    assert_eq!(
        runloop_context.cheatcode_config.lock().unwrap().filter,
        CheatcodeFilter::List(vec![]),
        "Expected all the cheatcodes to be enabled"
    );

    let disable_all_with_lockout_passes = rpc_server.disable_cheatcode(
        Some(runloop_context.clone()),
        CheatcodeFilter::All("all".to_string()),
        Some(CheatcodeControlConfig {
            lockout: Some(true),
        }),
    );

    assert!(
        disable_all_with_lockout_passes.is_ok(),
        "Expected surfnet_disableCheatcode with lockout == true to pass"
    );

    assert_eq!(
        runloop_context.cheatcode_config.lock().unwrap().filter,
        CheatcodeConfig::filter_all_list(true, vec![]),
        "Expected all the features to be disabled"
    );

    // assert that surfnet_enableCheatcode and surfnet_disableCheatcode both are disabled so that on_request Middleware sucessfully filters them
    for ref cheatcode in [enable_cheatcode_method, disable_cheatcode_method] {
        assert!(
            runloop_context
                .cheatcode_config
                .lock()
                .unwrap()
                .is_cheatcode_disabled(cheatcode),
            "Expected {} to be disabled",
            cheatcode
        );
    }
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_get_transaction_profile(test_type: TestType) {
    let rpc_server = SurfnetCheatcodesRpc::empty();
    let (mut svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();

    // Set up test accounts
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    let lamports_to_send = 1_000_000;

    svm_instance
        .airdrop(&payer.pubkey(), lamports_to_send * 2)
        .unwrap()
        .unwrap();

    // Create a transaction to profile
    let instruction = transfer(&payer.pubkey(), &recipient, lamports_to_send);
    let latest_blockhash = svm_instance.latest_blockhash();
    let message =
        Message::new_with_blockhash(&[instruction], Some(&payer.pubkey()), &latest_blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message.clone()), &[&payer])
        .unwrap();

    let tx_bytes = bincode::serialize(&tx).unwrap();
    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

    // Manually construct RunloopContext
    let svm_locker_for_context = SurfnetSvmLocker::new(svm_instance);
    let (simnet_cmd_tx, _simnet_cmd_rx) = crossbeam_unbounded::<SimnetCommand>();
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker_for_context.clone(),
        simnet_commands_tx: simnet_cmd_tx,
        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    // Test 1: Profile a transaction with a tag and retrieve by UUID
    let tag = "test_get_transaction_profile_tag".to_string();
    println!("Testing transaction profiling with tag: {}", tag);

    let profile_response: JsonRpcResult<RpcResponse<UiKeyedProfileResult>> = rpc_server
        .profile_transaction(
            Some(runloop_context.clone()),
            tx_b64.clone(),
            Some(tag.clone()),
            None,
        )
        .await;

    assert!(
        profile_response.is_ok(),
        "Profile transaction failed: {:?}",
        profile_response.err()
    );

    let profile_result = profile_response.unwrap().value;
    assert!(
        profile_result.transaction_profile.error_message.is_none(),
        "Transaction profiling failed"
    );

    let UuidOrSignature::Uuid(uuid) = profile_result.key else {
        panic!(
            "Expected a UUID from the profile result, got: {:?}",
            profile_result.key
        );
    };
    println!("Generated UUID: {}", uuid);

    // Test 2: Retrieve profile by UUID
    println!("Testing retrieval by UUID: {}", uuid);
    let uuid_response: JsonRpcResult<RpcResponse<Option<UiKeyedProfileResult>>> = rpc_server
        .get_transaction_profile(
            Some(runloop_context.clone()),
            UuidOrSignature::Uuid(uuid),
            None,
        );

    assert!(
        uuid_response.is_ok(),
        "Get transaction profile by UUID failed: {:?}",
        uuid_response.err()
    );

    let retrieved_profile = uuid_response.unwrap().value;
    assert!(
        retrieved_profile.is_some(),
        "Profile should be found by UUID"
    );

    let retrieved = retrieved_profile.unwrap();
    assert_eq!(
        retrieved.transaction_profile.compute_units_consumed,
        profile_result.transaction_profile.compute_units_consumed,
        "Retrieved profile should match original profile"
    );
    assert_eq!(
        retrieved.transaction_profile.error_message.is_none(),
        profile_result.transaction_profile.error_message.is_none(),
        "Retrieved profile success should match original"
    );
    assert_eq!(
        retrieved.key,
        UuidOrSignature::Uuid(uuid),
        "Retrieved profile should have the same UUID"
    );

    // Test 3: Process the transaction to get a signature and retrieve by signature
    println!("Processing transaction to get signature");
    let (status_tx, status_rx) = crossbeam_unbounded();

    svm_locker_for_context
        .process_transaction(&None, tx.clone(), status_tx, false, true)
        .await
        .unwrap();

    // Wait for transaction processing
    match status_rx.recv() {
        Ok(TransactionStatusEvent::Success(_)) => {
            println!("Transaction processed successfully");
        }
        Ok(TransactionStatusEvent::SimulationFailure((error, _))) => {
            panic!("Transaction simulation failed: {:?}", error);
        }
        Ok(TransactionStatusEvent::ExecutionFailure((error, _))) => {
            panic!("Transaction execution failed: {:?}", error);
        }
        Ok(TransactionStatusEvent::VerificationFailure(error)) => {
            panic!("Transaction verification failed: {}", error);
        }
        Err(e) => {
            panic!("Failed to receive transaction status: {:?}", e);
        }
    }

    let signature = tx.signatures[0];
    println!("Transaction signature: {}", signature);

    // Test 4: Retrieve profile by signature
    println!("Testing retrieval by signature: {}", signature);
    let signature_response: JsonRpcResult<RpcResponse<Option<UiKeyedProfileResult>>> = rpc_server
        .get_transaction_profile(
            Some(runloop_context.clone()),
            UuidOrSignature::Signature(signature),
            None,
        );

    assert!(
        signature_response.is_ok(),
        "Get transaction profile by signature failed: {:?}",
        signature_response.err()
    );

    let retrieved_by_signature = signature_response.unwrap().value;
    assert!(
        retrieved_by_signature.is_some(),
        "Profile should be found by signature"
    );

    let retrieved_sig = retrieved_by_signature.unwrap();
    assert!(
        retrieved_sig.transaction_profile.error_message.is_none(),
        "Retrieved profile by signature should be successful"
    );
    assert!(
        retrieved_sig.transaction_profile.compute_units_consumed > 0,
        "Retrieved profile should have consumed compute units"
    );

    // Test 5: Test retrieval with non-existent UUID
    println!("Testing retrieval with non-existent UUID");
    let non_existent_uuid = uuid::Uuid::new_v4();
    let non_existent_uuid_response: JsonRpcResult<RpcResponse<Option<UiKeyedProfileResult>>> =
        rpc_server.get_transaction_profile(
            Some(runloop_context.clone()),
            UuidOrSignature::Uuid(non_existent_uuid),
            None,
        );

    assert!(
        non_existent_uuid_response.is_ok(),
        "Get transaction profile with non-existent UUID should not fail"
    );

    let non_existent_result = non_existent_uuid_response.unwrap().value;
    assert!(
        non_existent_result.is_none(),
        "Non-existent UUID should return None"
    );

    // Test 6: Test retrieval with non-existent signature
    println!("Testing retrieval with non-existent signature");
    let non_existent_signature = solana_signature::Signature::new_unique();
    let non_existent_sig_response: JsonRpcResult<RpcResponse<Option<UiKeyedProfileResult>>> =
        rpc_server.get_transaction_profile(
            Some(runloop_context.clone()),
            UuidOrSignature::Signature(non_existent_signature),
            None,
        );

    assert!(
        non_existent_sig_response.is_ok(),
        "Get transaction profile with non-existent signature should not fail"
    );

    let non_existent_sig_result = non_existent_sig_response.unwrap().value;
    assert!(
        non_existent_sig_result.is_none(),
        "Non-existent signature should return None"
    );
    println!("All get_transaction_profile tests passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
fn test_register_and_get_idl_without_slot(test_type: TestType) {
    let idl: Idl = serde_json::from_slice(include_bytes!("./assets/idl_v1.json")).unwrap();
    let rpc_server = SurfnetCheatcodesRpc::empty();
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();

    let svm_locker_for_context = SurfnetSvmLocker::new(svm_instance);
    let (simnet_cmd_tx, _simnet_cmd_rx) = crossbeam_unbounded::<SimnetCommand>();
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker_for_context.clone(),
        simnet_commands_tx: simnet_cmd_tx,
        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    // Test 1: Register IDL without slot

    let register_response: JsonRpcResult<RpcResponse<()>> =
        rpc_server.register_idl(Some(runloop_context.clone()), idl.clone(), None);

    assert!(
        register_response.is_ok(),
        "Register IDL failed: {:?}",
        register_response.err()
    );

    // Test 2: Get IDL without slot

    let get_idl_response: JsonRpcResult<RpcResponse<Option<Idl>>> =
        rpc_server.get_idl(Some(runloop_context.clone()), idl.address.to_string(), None);

    assert!(
        get_idl_response.is_ok(),
        "Get IDL failed: {:?}",
        get_idl_response.err()
    );

    let retrieved_idl = get_idl_response.unwrap().value;
    assert!(retrieved_idl.is_some(), "IDL should be found");
    assert_eq!(
        retrieved_idl.unwrap(),
        idl,
        "Retrieved IDL should match registered IDL"
    );

    println!("All IDL registration and retrieval tests passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
fn test_register_and_get_idl_with_slot(test_type: TestType) {
    let idl: Idl = serde_json::from_slice(include_bytes!("./assets/idl_v1.json")).unwrap();
    let rpc_server = SurfnetCheatcodesRpc::empty();
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();

    let svm_locker_for_context = SurfnetSvmLocker::new(svm_instance);
    let (simnet_cmd_tx, _simnet_cmd_rx) = crossbeam_unbounded::<SimnetCommand>();
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker_for_context.clone(),
        simnet_commands_tx: simnet_cmd_tx,
        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    // Test 1: Register IDL with slot

    let register_response: JsonRpcResult<RpcResponse<()>> = rpc_server.register_idl(
        Some(runloop_context.clone()),
        idl.clone(),
        Some(Slot::from(
            svm_locker_for_context.get_latest_absolute_slot(),
        )),
    );

    assert!(
        register_response.is_ok(),
        "Register IDL failed: {:?}",
        register_response.err()
    );

    // Test 2: Get IDL with slot

    let get_idl_response: JsonRpcResult<RpcResponse<Option<Idl>>> = rpc_server.get_idl(
        Some(runloop_context.clone()),
        idl.address.to_string(),
        Some(Slot::from(
            svm_locker_for_context.get_latest_absolute_slot(),
        )),
    );

    assert!(
        get_idl_response.is_ok(),
        "Get IDL failed: {:?}",
        get_idl_response.err()
    );

    let retrieved_idl = get_idl_response.unwrap().value;
    assert!(retrieved_idl.is_some(), "IDL should be found");
    assert_eq!(
        retrieved_idl.unwrap(),
        idl,
        "Retrieved IDL should match registered IDL"
    );

    println!("All IDL registration and retrieval tests passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_register_and_get_same_idl_with_different_slots(test_type: TestType) {
    let idl_v1: Idl = serde_json::from_slice(include_bytes!("./assets/idl_v1.json")).unwrap();
    let idl_v2: Idl = serde_json::from_slice(include_bytes!("./assets/idl_v2.json")).unwrap();
    let idl_v3: Idl = serde_json::from_slice(include_bytes!("./assets/idl_v3.json")).unwrap();
    let rpc_server = SurfnetCheatcodesRpc::empty();
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();

    let svm_locker_for_context = SurfnetSvmLocker::new(svm_instance);

    // Prepare slots for registering different IDLs
    let current_slot = svm_locker_for_context.get_latest_absolute_slot();
    let slot_1 = current_slot.saturating_add(10); // First IDL registration
    let slot_2 = current_slot.saturating_add(50); // Second IDL registration
    let slot_3 = current_slot.saturating_add(100); // Third IDL registration

    println!("Current slot: {}", current_slot);
    println!("Slot 1 (IDL v1): {}", slot_1);
    println!("Slot 2 (IDL v2): {}", slot_2);
    println!("Slot 3 (IDL v3): {}", slot_3);

    // Setup runloop context
    let (simnet_cmd_tx, _simnet_cmd_rx) = crossbeam_unbounded::<SimnetCommand>();
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker_for_context.clone(),
        simnet_commands_tx: simnet_cmd_tx,
        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    // Step 1: Register IDL v1 at slot_1
    println!("  [1] Registering IDL v1 at slot: {}", slot_1);
    let register_response: JsonRpcResult<RpcResponse<()>> = rpc_server.register_idl(
        Some(runloop_context.clone()),
        idl_v1.clone(),
        Some(Slot::from(slot_1)),
    );

    assert!(
        register_response.is_ok(),
        "Register IDL v1 failed at slot {}: {:?}",
        slot_1,
        register_response.err()
    );

    // Step 2: Register IDL v2 at slot_2
    println!("  [2] Registering IDL v2 at slot: {}", slot_2);
    let register_response: JsonRpcResult<RpcResponse<()>> = rpc_server.register_idl(
        Some(runloop_context.clone()),
        idl_v2.clone(),
        Some(Slot::from(slot_2)),
    );

    assert!(
        register_response.is_ok(),
        "Register IDL v2 failed at slot {}: {:?}",
        slot_2,
        register_response.err()
    );

    // Step 3: Register IDL v3 at slot_3
    println!("  [3] Registering IDL v3 at slot: {}", slot_3);
    let register_response: JsonRpcResult<RpcResponse<()>> = rpc_server.register_idl(
        Some(runloop_context.clone()),
        idl_v3.clone(),
        Some(Slot::from(slot_3)),
    );

    assert!(
        register_response.is_ok(),
        "Register IDL v3 failed at slot {}: {:?}",
        slot_3,
        register_response.err()
    );

    // Step 4: Test retrieval at different points in time
    let test_cases = vec![
        (current_slot + 5, "before any registration", None), // Before slot_1
        (slot_1 + 5, "after v1 registration", Some(&idl_v1)), // After slot_1, before slot_2
        (slot_2 + 5, "after v2 registration", Some(&idl_v2)), // After slot_2, before slot_3
        (slot_3 + 5, "after v3 registration", Some(&idl_v3)), // After slot_3
    ];

    for (i, (query_slot, description, expected_idl)) in test_cases.iter().enumerate() {
        println!(
            "  [{}] Querying IDL at slot {} ({})",
            i + 4,
            query_slot,
            description
        );

        let get_idl_response: JsonRpcResult<RpcResponse<Option<Idl>>> = rpc_server.get_idl(
            Some(runloop_context.clone()),
            idl_v1.address.to_string(),
            Some(Slot::from(*query_slot)),
        );

        assert!(
            get_idl_response.is_ok(),
            "Get IDL failed at slot {}: {:?}",
            query_slot,
            get_idl_response.err()
        );

        let retrieved_idl = get_idl_response.unwrap().value;

        match expected_idl {
            None => {
                // Should not have any IDL before first registration
                assert!(
                    retrieved_idl.is_none(),
                    "IDL should not be available when querying at slot {} (before first registration)",
                    query_slot
                );
                println!(
                    "  [{}] Correctly: No IDL available at slot {} (before first registration)",
                    i + 4,
                    query_slot
                );
            }
            Some(expected) => {
                // Should have the appropriate IDL
                assert!(
                    retrieved_idl.is_some(),
                    "IDL should be available when querying at slot {}",
                    query_slot
                );

                let retrieved = retrieved_idl.unwrap();
                assert_eq!(
                    retrieved, **expected,
                    "Retrieved IDL should match expected IDL at slot {}",
                    query_slot
                );
                println!(
                    "  [{}] Correctly: Expected IDL retrieved at slot {}",
                    i + 4,
                    query_slot
                );
            }
        }
    }

    println!("All IDL registration and retrieval tests at different slots passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_profile_transaction_basic(test_type: TestType) {
    // Set up test environment
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // Set up test accounts
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    let lamports_to_send = 1_000_000;

    // Airdrop SOL to payer
    svm_locker
        .with_svm_writer(|svm| svm.airdrop(&payer.pubkey(), lamports_to_send * 2))
        .unwrap()
        .unwrap();

    // Create a simple transfer transaction
    let instruction = transfer(&payer.pubkey(), &recipient, lamports_to_send);
    let latest_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let message =
        Message::new_with_blockhash(&[instruction], Some(&payer.pubkey()), &latest_blockhash);
    let transaction =
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&payer]).unwrap();

    // Test basic profiling without tag
    println!("Testing basic transaction profiling");
    let profile_uuid = svm_locker
        .profile_transaction(&None, transaction.clone(), None)
        .await
        .unwrap()
        .inner;

    let key = UuidOrSignature::Uuid(profile_uuid);
    let profile_result = svm_locker
        .get_profile_result(key, &RpcProfileResultConfig::default())
        .unwrap()
        .expect("Profile result should exist");

    // Verify UUID generation
    let UuidOrSignature::Uuid(uuid) = profile_result.key else {
        panic!(
            "Expected a UUID from the profile result, got: {:?}",
            profile_result.key
        );
    };
    println!("Generated UUID: {}", uuid);

    // Verify transaction profile
    assert!(
        profile_result.transaction_profile.error_message.is_none(),
        "Transaction profiling should succeed"
    );
    assert!(
        profile_result.transaction_profile.compute_units_consumed > 0,
        "Transaction should consume compute units"
    );

    // Verify slot and context
    assert_eq!(
        profile_result.slot,
        svm_locker.get_latest_absolute_slot(),
        "Profile slot should match current slot"
    );

    // Verify storage in SVM
    let stored_profile = svm_locker
        .get_profile_result(
            UuidOrSignature::Uuid(uuid),
            &RpcProfileResultConfig::default(),
        )
        .unwrap();
    assert!(stored_profile.is_some(), "Profile should be stored in SVM");

    let stored = stored_profile.unwrap();
    assert_eq!(
        stored.transaction_profile.compute_units_consumed,
        profile_result.transaction_profile.compute_units_consumed,
        "Stored profile should match returned profile"
    );

    println!("Basic transaction profiling test passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_profile_transaction_multi_instruction_basic(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let payer = Keypair::new();
    let lamports_to_send = 1_000_000;

    svm_locker
        .with_svm_writer(|svm| svm.airdrop(&payer.pubkey(), lamports_to_send * 4))
        .unwrap()
        .unwrap();

    // Create a multi-instruction transaction: 3 transfers to different recipients
    let recipient = Pubkey::new_unique();
    let recipient2 = Pubkey::new_unique();
    let recipient3 = Pubkey::new_unique();
    println!("Sender: {}", payer.pubkey());
    println!("Recipient 1: {}", recipient);
    println!("Recipient 2: {}", recipient2);
    println!("Recipient 3: {}", recipient3);

    let transfer_ix = transfer(&payer.pubkey(), &recipient, lamports_to_send);
    let transfer_ix2 = transfer(&payer.pubkey(), &recipient2, lamports_to_send);
    let transfer_ix3 = transfer(&payer.pubkey(), &recipient3, lamports_to_send);

    let latest_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let message = Message::new_with_blockhash(
        &[transfer_ix, transfer_ix2, transfer_ix3],
        Some(&payer.pubkey()),
        &latest_blockhash,
    );
    let transaction =
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&payer]).unwrap();

    let profile_uuid = svm_locker
        .profile_transaction(&None, transaction.clone(), None)
        .await
        .unwrap()
        .inner;

    let key = UuidOrSignature::Uuid(profile_uuid);

    // Check profile result with Transaction depth
    {
        let rpc_profile_config = RpcProfileResultConfig {
            encoding: Some(UiAccountEncoding::JsonParsed),
            depth: Some(RpcProfileDepth::Transaction),
        };

        let profile_result = svm_locker
            .get_profile_result(key, &rpc_profile_config)
            .unwrap()
            .expect("Profile result should exist");

        // Verify UUID generation
        let UuidOrSignature::Uuid(uuid) = profile_result.key else {
            panic!(
                "Expected a UUID from the profile result, got: {:?}",
                profile_result.key
            );
        };
        println!("Generated UUID: {}", uuid);

        assert!(profile_result.transaction_profile.error_message.is_none(),);
        assert_eq!(
            profile_result.transaction_profile.compute_units_consumed,
            450
        );
        assert_eq!(
            profile_result.instruction_profiles, None,
            "Instruction profiles should be None for Transaction depth config"
        );

        let account_states = profile_result.transaction_profile.account_states;

        let _ = profile_result
            .readonly_account_states
            .get(&system_program::id())
            .expect("System program should be present in readonly account states");
        let UiAccountProfileState::Readonly = account_states
            .get(&system_program::id())
            .expect("System program state should be present")
        else {
            panic!("Expected system program state to be Readonly");
        };

        // assert payer states
        {
            let UiAccountProfileState::Writable(sender_account_change) = account_states
                .get(&payer.pubkey())
                .expect("Payer account state should be present")
            else {
                panic!("Expected account state to be Writable");
            };

            match sender_account_change {
                UiAccountChange::Update(before, after) => {
                    assert_eq!(
                        after.lamports,
                        before.lamports - (lamports_to_send * 3) - 5000,
                        "Payer account should be original balance minus three transfers and fees"
                    );
                }
                other => {
                    panic!("Expected account state to be an Update, got: {:?}", other);
                }
            }
        }

        // assert recipient 1 states
        {
            let UiAccountProfileState::Writable(recipient_1_change) = account_states
                .get(&recipient)
                .expect("Recipient 1 account state should be present")
            else {
                panic!("Expected recipient 1 account state to be Writable");
            };

            match recipient_1_change {
                UiAccountChange::Create(new) => {
                    assert_eq!(
                        new.lamports, lamports_to_send,
                        "Recipient 1 account should have received the transfer amount"
                    );
                }
                other => {
                    panic!("Expected account state to be an Create, got: {:?}", other);
                }
            }
        }

        // assert recipient 2 states
        {
            let UiAccountProfileState::Writable(recipient_2_change) = account_states
                .get(&recipient2)
                .expect("Recipient 2 account state should be present")
            else {
                panic!("Expected recipient 2 account state to be Writable");
            };

            match recipient_2_change {
                UiAccountChange::Create(new) => {
                    assert_eq!(
                        new.lamports, lamports_to_send,
                        "Recipient 2 account should have received the transfer amount"
                    );
                }
                other => {
                    panic!("Expected account state to be an Update, got: {:?}", other);
                }
            }
        }

        // assert recipient 3 states
        {
            let UiAccountProfileState::Writable(recipient_3_change) = account_states
                .get(&recipient3)
                .expect("Recipient 3 account state should be present")
            else {
                panic!("Expected recipient 3 account state to be Writable");
            };

            match recipient_3_change {
                UiAccountChange::Create(new) => {
                    assert_eq!(
                        new.lamports, lamports_to_send,
                        "Recipient 3 account should have received the transfer amount"
                    );
                }
                other => {
                    panic!("Expected account state to be Create, got: {:?}", other);
                }
            }
        }
    }

    // Check profile result with Instruction depth
    {
        let rpc_profile_config = RpcProfileResultConfig {
            encoding: Some(UiAccountEncoding::JsonParsed),
            depth: Some(RpcProfileDepth::Instruction),
        };
        let profile_result = svm_locker
            .get_profile_result(key, &rpc_profile_config)
            .unwrap()
            .expect("Profile result should exist");

        println!(
            "Profile result with Instruction depth: {}",
            serde_json::to_string_pretty(&profile_result).unwrap()
        );

        let instruction_profiles = profile_result
            .instruction_profiles
            .expect("Instruction profiles should be present for Instruction depth config");

        // assert ix 1 data
        {
            let ix_profile = instruction_profiles
                .get(0)
                .expect("Instruction profile should exist");
            assert_eq!(
                ix_profile.compute_units_consumed, 150,
                "Instruction should consume 150 CU"
            );
            assert!(ix_profile.error_message.is_none());
            let account_states = &ix_profile.account_states;
            // assert account sender states
            {
                let UiAccountProfileState::Writable(sender_account_change) = account_states
                    .get(&payer.pubkey())
                    .expect("Payer account state should be present")
                else {
                    panic!("Expected account state to be Writable");
                };

                match sender_account_change {
                    UiAccountChange::Update(before, after) => {
                        assert_eq!(
                            after.lamports,
                            before.lamports - lamports_to_send - 5000,
                            "Payer account should be original balance minus transfer amount"
                        );
                    }
                    other => {
                        panic!("Expected account state to be an Update, got: {:?}", other);
                    }
                }
            }

            // assert recipient 1 states
            {
                let UiAccountProfileState::Writable(recipient_1_change) = account_states
                    .get(&recipient)
                    .expect("Recipient 1 account state should be present")
                else {
                    panic!("Expected recipient 1 account state to be Writable");
                };

                match recipient_1_change {
                    UiAccountChange::Create(new) => {
                        assert_eq!(
                            new.lamports, lamports_to_send,
                            "Recipient 1 account should have received the transfer amount"
                        );
                    }
                    other => {
                        panic!("Expected account state to be an Update, got: {:?}", other);
                    }
                }
            }

            assert!(
                account_states.get(&recipient2).is_none(),
                "Recipient 2 should not be affected by first instruction"
            );
            assert!(
                account_states.get(&recipient3).is_none(),
                "Recipient 3 should not be affected by first instruction"
            );
        }

        // assert ix 2 data
        {
            let ix_profile = instruction_profiles
                .get(1)
                .expect("Instruction profile should exist");
            assert_eq!(
                ix_profile.compute_units_consumed, 150,
                "Instruction should consume 150 CU"
            );
            assert!(ix_profile.error_message.is_none());
            let account_states = &ix_profile.account_states;
            // assert account sender states
            {
                let UiAccountProfileState::Writable(sender_account_change) = account_states
                    .get(&payer.pubkey())
                    .expect("Payer account state should be present")
                else {
                    panic!("Expected account state to be Writable");
                };

                match sender_account_change {
                    UiAccountChange::Update(before, after) => {
                        assert_eq!(after.lamports, before.lamports - lamports_to_send);
                    }
                    other => {
                        panic!("Expected account state to be an Update, got: {:?}", other);
                    }
                }
            }

            println!(
                "Recipient 1 account state: {:?}",
                account_states.get(&recipient)
            );
            assert!(
                account_states.get(&recipient).is_none(),
                "Recipient 1 should not be affected by second instruction"
            );
            // assert recipient 2 states
            {
                let UiAccountProfileState::Writable(recipient_2_change) = account_states
                    .get(&recipient2)
                    .expect("Recipient 2 account state should be present")
                else {
                    panic!("Expected recipient 2 account state to be Writable");
                };

                match recipient_2_change {
                    UiAccountChange::Create(new) => {
                        assert_eq!(
                            new.lamports, lamports_to_send,
                            "Recipient 2 account should have received the transfer amount"
                        );
                    }
                    other => {
                        panic!("Expected account state to be an Update, got: {:?}", other);
                    }
                }
            }

            assert!(
                account_states.get(&recipient3).is_none(),
                "Recipient 3 should not be affected by first instruction"
            );
        }

        // assert ix 3 data
        {
            let ix_profile = instruction_profiles
                .get(2)
                .expect("Instruction profile should exist");
            assert_eq!(
                ix_profile.compute_units_consumed, 150,
                "Instruction should consume 150 CU"
            );
            assert!(ix_profile.error_message.is_none());
            let account_states = &ix_profile.account_states;
            // assert account sender states
            {
                let UiAccountProfileState::Writable(sender_account_change) = account_states
                    .get(&payer.pubkey())
                    .expect("Payer account state should be present")
                else {
                    panic!("Expected account state to be Writable");
                };

                match sender_account_change {
                    UiAccountChange::Update(before, after) => {
                        assert_eq!(after.lamports, before.lamports - lamports_to_send,);
                    }
                    other => {
                        panic!("Expected account state to be an Update, got: {:?}", other);
                    }
                }
            }

            assert!(
                account_states.get(&recipient).is_none(),
                "Recipient 1 should not be affected by second instruction"
            );

            assert!(
                account_states.get(&recipient2).is_none(),
                "Recipient 2 should not be affected by first instruction"
            );
            // assert recipient 3 states
            {
                let UiAccountProfileState::Writable(recipient_3_change) = account_states
                    .get(&recipient3)
                    .expect("Recipient 3 account state should be present")
                else {
                    panic!("Expected recipient 3 account state to be Writable");
                };

                match recipient_3_change {
                    UiAccountChange::Create(new) => {
                        assert_eq!(
                            new.lamports, lamports_to_send,
                            "Recipient 3 account should have received the transfer amount"
                        );
                    }
                    other => {
                        panic!("Expected account state to be an Update, got: {:?}", other);
                    }
                }
            }
        }
    }
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_profile_transaction_with_tag(test_type: TestType) {
    // Set up test environment
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // Set up test accounts
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    let lamports_to_send = 1_000_000;

    // Airdrop SOL to payer
    svm_locker
        .with_svm_writer(|svm| svm.airdrop(&payer.pubkey(), lamports_to_send * 3))
        .unwrap()
        .unwrap();

    // Create a simple transfer transaction
    let instruction = transfer(&payer.pubkey(), &recipient, lamports_to_send);
    let latest_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let message =
        Message::new_with_blockhash(&[instruction], Some(&payer.pubkey()), &latest_blockhash);
    let transaction =
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&payer]).unwrap();

    // Test profiling with a tag
    let tag = "test_profile_transaction_tag".to_string();
    println!("Testing transaction profiling with tag: {}", tag);

    let profile_uuid = svm_locker
        .profile_transaction(&None, transaction.clone(), Some(tag.clone()))
        .await
        .unwrap()
        .inner;

    let key = UuidOrSignature::Uuid(profile_uuid);
    let profile_result = svm_locker
        .get_profile_result(key, &RpcProfileResultConfig::default())
        .unwrap()
        .expect("Profile result should exist");

    // Verify transaction profile
    assert!(
        profile_result.transaction_profile.error_message.is_none(),
        "Transaction profiling should succeed"
    );

    // Verify tag-based retrieval
    let tagged_results = svm_locker
        .get_profile_results_by_tag(tag.clone(), &RpcProfileResultConfig::default())
        .unwrap();
    assert!(tagged_results.is_some(), "Tagged results should be found");

    let tagged_profiles = tagged_results.unwrap();
    assert_eq!(
        tagged_profiles.len(),
        1,
        "Should have exactly one profile for this tag"
    );

    let tagged_profile = &tagged_profiles[0];
    assert_eq!(
        tagged_profile.key,
        UuidOrSignature::Uuid(profile_uuid),
        "Tagged profile should have the same UUID"
    );

    // Test multiple profiles with the same tag
    println!("Testing multiple profiles with the same tag");

    // Create another transaction
    let recipient2 = Pubkey::new_unique();
    let instruction2 = transfer(&payer.pubkey(), &recipient2, lamports_to_send);
    let message2 =
        Message::new_with_blockhash(&[instruction2], Some(&payer.pubkey()), &latest_blockhash);
    let transaction2 =
        VersionedTransaction::try_new(VersionedMessage::Legacy(message2), &[&payer]).unwrap();

    let uuid2 = svm_locker
        .profile_transaction(&None, transaction2.clone(), Some(tag.clone()))
        .await
        .unwrap()
        .inner;

    // Verify both profiles are now associated with the tag
    let tagged_results_updated = svm_locker
        .get_profile_results_by_tag(tag.clone(), &RpcProfileResultConfig::default())
        .unwrap();
    assert!(
        tagged_results_updated.is_some(),
        "Tagged results should still be found after adding second profile"
    );

    let tagged_profiles_updated = tagged_results_updated.unwrap();
    assert_eq!(
        tagged_profiles_updated.len(),
        2,
        "Should have exactly two profiles for this tag"
    );

    // Verify both UUIDs are present
    let uuids: Vec<Uuid> = tagged_profiles_updated
        .iter()
        .filter_map(|profile| {
            if let UuidOrSignature::Uuid(uuid) = profile.key {
                Some(uuid)
            } else {
                None
            }
        })
        .collect();

    assert!(
        uuids.contains(&profile_uuid),
        "First UUID should be in tagged results"
    );
    assert!(
        uuids.contains(&uuid2),
        "Second UUID should be in tagged results"
    );

    // Test retrieval with non-existent tag
    let non_existent_tag = "non_existent_tag".to_string();
    let non_existent_results = svm_locker
        .get_profile_results_by_tag(non_existent_tag, &RpcProfileResultConfig::default())
        .unwrap();
    assert!(
        non_existent_results.is_none(),
        "Non-existent tag should return None"
    );

    // Test retrieval by individual UUIDs
    let stored_profile1 = svm_locker
        .get_profile_result(
            UuidOrSignature::Uuid(profile_uuid),
            &RpcProfileResultConfig::default(),
        )
        .unwrap();
    assert!(
        stored_profile1.is_some(),
        "First profile should be retrievable by UUID"
    );

    let stored_profile2 = svm_locker
        .get_profile_result(
            UuidOrSignature::Uuid(uuid2),
            &RpcProfileResultConfig::default(),
        )
        .unwrap();
    assert!(
        stored_profile2.is_some(),
        "Second profile should be retrievable by UUID"
    );

    println!("Tag-based transaction profiling test passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_profile_transaction_token_transfer(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // Set up test accounts
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    let mint = Keypair::new();
    let lamports_to_send = 2 * LAMPORTS_PER_SOL;
    println!("Sender: {}", payer.pubkey());
    println!("Recipient: {}", recipient);
    println!("Mint: {}", mint.pubkey());

    // Airdrop SOL to payer
    svm_locker
        .airdrop(&payer.pubkey(), lamports_to_send)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    println!("Recent blockhash after airdrop: {}", recent_blockhash);

    // Create account for mint
    let mint_rent = 1461600;
    let create_account_ix = system_instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        mint_rent,
        82,
        &spl_token_2022_interface::id(),
    );

    // Initialize mint
    let initialize_mint_ix = spl_token_2022_interface::instruction::initialize_mint2(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        &payer.pubkey(),
        Some(&payer.pubkey()),
        2, // decimals
    )
    .unwrap();

    // Create associated token accounts
    let source_ata = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
        &payer.pubkey(),
        &mint.pubkey(),
        &spl_token_2022_interface::id(),
    );
    println!("Source ATA: {}", source_ata);
    let _dest_ata = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
        &recipient,
        &mint.pubkey(),
        &spl_token_2022_interface::id(),
    );

    let create_source_ata_ix =
        spl_associated_token_account_interface::instruction::create_associated_token_account(
            &payer.pubkey(),
            &payer.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let _create_dest_ata_ix =
        spl_associated_token_account_interface::instruction::create_associated_token_account(
            &payer.pubkey(),
            &recipient,
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    // Mint tokens
    let mint_amount = 100_00; // 100 tokens with 2 decimals
    let _mint_to_ix = spl_token_2022_interface::instruction::mint_to(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        &source_ata,
        &payer.pubkey(),
        &[&payer.pubkey()],
        mint_amount,
    )
    .unwrap();

    // Create setup transaction
    let setup_message = Message::new_with_blockhash(
        &[
            create_account_ix,
            initialize_mint_ix,
            create_source_ata_ix,
            // create_dest_ata_ix,
            // mint_to_ix,
        ],
        Some(&payer.pubkey()),
        &recent_blockhash,
    );
    let setup_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(setup_message), &[&payer, &mint])
            .unwrap();

    {
        let profile_result = svm_locker
            .profile_transaction(&None, setup_tx.clone(), None)
            .await
            .unwrap();

        let rpc_profile_config = RpcProfileResultConfig {
            encoding: Some(UiAccountEncoding::JsonParsed),
            depth: Some(RpcProfileDepth::Instruction),
        };
        let key = UuidOrSignature::Uuid(profile_result.inner);
        let ui_profile_result = svm_locker
            .get_profile_result(key, &rpc_profile_config)
            .unwrap()
            .expect("Profile result should exist");

        assert!(
            ui_profile_result
                .transaction_profile
                .error_message
                .is_none(),
            "Setup transaction should succeed, found error: {}",
            ui_profile_result.transaction_profile.error_message.unwrap()
        );

        // instruction 1: create_account
        {
            let ix_profile = ui_profile_result
                .instruction_profiles
                .as_ref()
                .unwrap()
                .get(0)
                .expect("instruction profile should exist");
            assert!(
                ix_profile.error_message.is_none(),
                "Profile should succeed, found error: {}",
                ix_profile.error_message.as_ref().unwrap()
            );
            assert_eq!(ix_profile.compute_units_consumed, 150);
            assert!(ix_profile.error_message.is_none());
            let account_states = &ix_profile.account_states;

            let UiAccountProfileState::Writable(sender_account_change) = account_states
                .get(&payer.pubkey())
                .expect("Payer account state should be present")
            else {
                panic!("Expected account state to be Writable");
            };

            match sender_account_change {
                UiAccountChange::Update(before, after) => {
                    assert_eq!(
                        after.lamports,
                        before.lamports - mint_rent - (2 * 5000), // two signers, so 2 * 5000 for fees
                        "Payer account should be original balance minus rent"
                    );
                }
                other => {
                    panic!("Expected account state to be an Update, got: {:?}", other);
                }
            }

            let UiAccountProfileState::Writable(mint_account_change) = account_states
                .get(&mint.pubkey())
                .expect("Mint account state should be present")
            else {
                panic!("Expected mint account state to be Writable");
            };
            match mint_account_change {
                UiAccountChange::Create(mint_account) => {
                    assert_eq!(
                        mint_account.lamports, mint_rent,
                        "Mint account should have the correct rent amount"
                    );
                    assert_eq!(
                        mint_account.owner,
                        spl_token_2022_interface::id().to_string(),
                        "Mint account should be owned by the SPL Token program"
                    );
                    // initialized account data should be empty bytes
                    assert_eq!(
                        mint_account.data,
                        UiAccountData::Binary(
                            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==".into(),
                            UiAccountEncoding::Base64
                        ),
                    );
                }
                other => {
                    panic!("Expected account state to be an Update, got: {:?}", other);
                }
            }
        }

        // instruction 2: initialize mint
        {
            let ix_profile = ui_profile_result
                .instruction_profiles
                .as_ref()
                .unwrap()
                .get(1)
                .expect("instruction profile should exist");
            assert!(
                ix_profile.error_message.is_none(),
                "Profile should succeed, found error: {}",
                ix_profile.error_message.as_ref().unwrap()
            );
            assert_eq!(ix_profile.compute_units_consumed, 1230);
            assert!(ix_profile.error_message.is_none());
            let account_states = &ix_profile.account_states;

            assert!(account_states.get(&payer.pubkey()).is_none());

            let UiAccountProfileState::Writable(mint_account_change) = account_states
                .get(&mint.pubkey())
                .expect("Mint account state should be present")
            else {
                panic!("Expected mint account state to be Writable");
            };
            match mint_account_change {
                UiAccountChange::Update(_before, after) => {
                    assert_eq!(
                        after.lamports, mint_rent,
                        "Mint account should have the correct rent amount"
                    );
                    assert_eq!(
                        after.owner,
                        spl_token_2022_interface::id().to_string(),
                        "Mint account should be owned by the SPL Token program"
                    );
                    // initialized account data should be empty bytes
                    assert_eq!(
                        after.data,
                        UiAccountData::Json(ParsedAccount {
                            program: "spl-token-2022".to_string(),
                            parsed: json!({
                                "info": {
                                    "decimals": 2,
                                    "freezeAuthority": payer.pubkey().to_string(),
                                    "mintAuthority": payer.pubkey().to_string(),
                                    "isInitialized": true,
                                    "supply": "0",
                                },
                                "type": "mint"
                            }),
                            space: 82,
                        }),
                    );
                }
                other => {
                    panic!("Expected account state to be an Update, got: {:?}", other);
                }
            }
        }

        // instruction 3: create token account
        {
            let ix_profile = ui_profile_result
                .instruction_profiles
                .as_ref()
                .unwrap()
                .get(2)
                .expect("instruction profile should exist");
            assert!(
                ix_profile.error_message.is_none(),
                "Profile should succeed, found error: {}",
                ix_profile.error_message.as_ref().unwrap()
            );
            // assert_eq!(ix_profile.compute_units_consumed, 15758);
            assert!(ix_profile.error_message.is_none());
            let account_states = &ix_profile.account_states;

            let UiAccountProfileState::Writable(sender_account_change) = account_states
                .get(&payer.pubkey())
                .expect("Payer account state should be present")
            else {
                panic!("Expected account state to be Writable");
            };

            match sender_account_change {
                UiAccountChange::Update(before, after) => {
                    assert_eq!(
                        after.lamports,
                        before.lamports - 2074080,
                        "Payer account should be original balance minus rent"
                    );
                }
                other => {
                    panic!("Expected account state to be an Update, got: {:?}", other);
                }
            }

            let UiAccountProfileState::Writable(mint_account_change) = account_states
                .get(&mint.pubkey())
                .expect("Mint account state should be present")
            else {
                panic!("Expected mint account state to be Writable");
            };
            match mint_account_change {
                UiAccountChange::Unchanged(mint_account) => {
                    assert!(mint_account.is_some())
                }
                other => {
                    panic!("Expected account state to be Unchanged, got: {:?}", other);
                }
            }

            let UiAccountProfileState::Writable(source_ata_change) = account_states
                .get(&source_ata)
                .expect("account state should be present")
            else {
                panic!("Expected account state to be Writable");
            };

            match source_ata_change {
                UiAccountChange::Create(new) => {
                    assert_eq!(
                        new.lamports, 2074080,
                        "Source ATA should have the correct lamports after creation"
                    );
                    assert_eq!(
                        new.owner,
                        spl_token_2022_interface::id().to_string(),
                        "Source ATA should be owned by the SPL Token program"
                    );
                    // since we're profiling, the "additional data" needed to json parse token 2022 accounts isn't available,
                    // so the result is base64 encoded
                    let UiAccountData::Binary(new_data, UiAccountEncoding::Base64) = &new.data
                    else {
                        panic!("Expected account data to be Base64 encoded");
                    };

                    let mut mint_bs64 =
                        base64::engine::general_purpose::STANDARD.encode(&mint.pubkey().to_bytes());
                    mint_bs64.truncate(42);
                    assert!(new_data.starts_with(mint_bs64.as_str()));
                    assert!(
                        new_data.ends_with(
                            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgcAAAA="
                        )
                    );
                }
                other => {
                    panic!("Expected account state to be Create, got: {:?}", other);
                }
            }
        }

        assert_eq!(
            ui_profile_result.transaction_profile.compute_units_consumed,
            ui_profile_result
                .instruction_profiles
                .as_ref()
                .unwrap()
                .iter()
                .map(|ix| ix.compute_units_consumed)
                .sum::<u64>(),
        );
    }

    // Process setup transaction
    // let (status_tx, status_rx) = crossbeam_channel::unbounded();
    // svm_locker
    //     .process_transaction(&None, setup_tx, status_tx, false, true)
    //     .await
    //     .unwrap();

    // match status_rx.recv() {
    //     Ok(TransactionStatusEvent::Success(_)) => println!("Setup transaction successful"),
    //     other => panic!("Setup transaction failed: {:?}", other),
    // }

    // // Now create a token transfer transaction to profile
    // let transfer_amount = 50; // 0.5 tokens
    // let transfer_ix = spl_token_2022_interface::instruction::transfer_checked(
    //     &spl_token_2022_interface::id(),
    //     &source_ata,
    //     &mint.pubkey(),
    //     &dest_ata,
    //     &payer.pubkey(),
    //     &[&payer.pubkey()],
    //     transfer_amount,
    //     2, // decimals
    // )
    // .unwrap();

    // let transfer_message =
    //     Message::new_with_blockhash(&[transfer_ix], Some(&payer.pubkey()), &recent_blockhash);
    // let transfer_tx =
    //     VersionedTransaction::try_new(VersionedMessage::Legacy(transfer_message), &[&payer])
    //         .unwrap();

    // // Profile the token transfer transaction
    // let profile_result = svm_locker
    //     .profile_transaction(&None, transfer_tx.clone(), None)
    //     .await
    //     .unwrap();

    // let rpc_profile_config = RpcProfileResultConfig {
    //     encoding: Some(UiAccountEncoding::JsonParsed),
    //     depth: Some(RpcProfileDepth::Instruction),
    // };
    // let ui_profile_result = svm_locker
    //     .encode_ui_keyed_profile_result(profile_result.inner.clone(), &rpc_profile_config);

    // println!(
    //     "UI Profile Result: {}",
    //     serde_json::to_string_pretty(&ui_profile_result).unwrap()
    // );

    // // Verify UUID generation
    // let UuidOrSignature::Uuid(_uuid) = profile_result.inner.key else {
    //     panic!("Expected a UUID from the profile result");
    // };

    // // Verify transaction profile
    // assert!(
    //     profile_result
    //         .inner
    //         .transaction_profile
    //         .error_message
    //         .is_none(),
    //     "Token transfer profiling should succeed"
    // );
    // assert!(
    //     profile_result
    //         .inner
    //         .transaction_profile
    //         .compute_units_consumed
    //         > 0,
    //     "Token transfer should consume compute units"
    // );

    // println!("Token transfer profiling test passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_profile_transaction_insufficient_funds(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // Set up test accounts with insufficient funds
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    let insufficient_funds = LAMPORTS_PER_SOL;
    let large_transfer = LAMPORTS_PER_SOL * 10;

    svm_locker
        .airdrop(&payer.pubkey(), insufficient_funds)
        .unwrap()
        .unwrap();

    // Create a transfer transaction that will fail due to insufficient funds
    let instruction = transfer(&payer.pubkey(), &recipient, large_transfer);
    let latest_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let message =
        Message::new_with_blockhash(&[instruction], Some(&payer.pubkey()), &latest_blockhash);
    let transaction =
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&payer]).unwrap();

    // Profile the failing transaction
    let profile_uuid = svm_locker
        .profile_transaction(&None, transaction.clone(), None)
        .await
        .unwrap()
        .inner;

    let key = UuidOrSignature::Uuid(profile_uuid);
    let ui_profile_result = svm_locker
        .get_profile_result(key, &RpcProfileResultConfig::default())
        .unwrap()
        .expect("Profile result should exist");

    // Verify transaction profile shows failure
    assert!(
        ui_profile_result
            .transaction_profile
            .error_message
            .is_some(),
        "Transaction should fail due to insufficient funds"
    );

    let error_msg = ui_profile_result
        .transaction_profile
        .error_message
        .as_ref()
        .unwrap();
    assert!(
        error_msg.eq("Error processing Instruction 0: custom program error: 0x1"),
        "Error message should indicate insufficient funds, got: {}",
        error_msg
    );

    // Verify compute units were still consumed
    assert!(
        ui_profile_result.transaction_profile.compute_units_consumed > 0,
        "Failed transaction should still consume compute units"
    );

    println!("Insufficient funds profiling test passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_profile_transaction_multi_instruction_failure(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // Set up test accounts
    let payer = Keypair::new();
    let payer2 = Keypair::new();
    let recipient1 = Pubkey::new_unique();
    let lamports_to_send = LAMPORTS_PER_SOL;

    // Airdrop SOL to payer
    svm_locker
        .airdrop(&payer.pubkey(), lamports_to_send * 3)
        .unwrap()
        .unwrap();

    // Create a multi-instruction transaction where the second instruction will fail
    let valid_instruction = transfer(&payer.pubkey(), &recipient1, lamports_to_send);
    let invalid_instruction = transfer(&payer2.pubkey(), &recipient1, lamports_to_send * 2); // payer2 has no funds

    let latest_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let message = Message::new_with_blockhash(
        &[valid_instruction, invalid_instruction],
        Some(&payer.pubkey()),
        &latest_blockhash,
    );
    let transaction =
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&payer, &payer2])
            .unwrap();

    // Profile the multi-instruction transaction

    let uuid = svm_locker
        .profile_transaction(&None, transaction.clone(), None)
        .await
        .unwrap()
        .inner;

    let key = UuidOrSignature::Uuid(uuid);
    let profile_result = svm_locker
        .get_profile_result(key, &RpcProfileResultConfig::default())
        .unwrap()
        .expect("Profile result should exist");

    // Verify transaction profile shows failure
    assert!(
        profile_result.transaction_profile.error_message.is_some(),
        "Multi-instruction transaction should fail"
    );

    // Verify instruction profiles exist
    assert!(
        profile_result.instruction_profiles.is_some(),
        "Instruction profiles should be generated for multi-instruction transaction"
    );

    let instruction_profiles = profile_result.instruction_profiles.as_ref().unwrap();
    assert_eq!(instruction_profiles.len(), 2,);

    // Verify the first instruction profile succeeded
    let first_instruction_profile = &instruction_profiles[0];
    assert!(
        first_instruction_profile.error_message.is_none(),
        "First instruction should succeed"
    );
    assert!(
        first_instruction_profile.compute_units_consumed > 0,
        "First instruction should consume compute units"
    );

    let second_profile = &instruction_profiles[1];
    assert!(
        second_profile.error_message.is_some(),
        "Second instruction should fail due to insufficient funds"
    );

    println!("Multi-instruction failure profiling test passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_profile_transaction_with_encoding(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // Set up test accounts
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    let lamports_to_send = LAMPORTS_PER_SOL;

    // Airdrop SOL to payer
    svm_locker
        .with_svm_writer(|svm| svm.airdrop(&payer.pubkey(), lamports_to_send * 2))
        .unwrap()
        .unwrap();

    // Create a simple transfer transaction
    let instruction = transfer(&payer.pubkey(), &recipient, lamports_to_send);
    let latest_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let message =
        Message::new_with_blockhash(&[instruction], Some(&payer.pubkey()), &latest_blockhash);
    let transaction =
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&payer]).unwrap();

    let profile_uuid = svm_locker
        .profile_transaction(&None, transaction.clone(), None)
        .await
        .unwrap()
        .inner;

    let key = UuidOrSignature::Uuid(profile_uuid);
    let profile_result = svm_locker
        .get_profile_result(key, &RpcProfileResultConfig::default())
        .unwrap()
        .expect("Profile result should exist");

    // Verify transaction profile
    assert!(
        profile_result.transaction_profile.error_message.is_none(),
        "Transaction profiling should succeed with base64 encoding"
    );

    // // Verify state snapshots use the specified encoding
    // let pre_execution_accounts = &profile_result.inner.transaction_profile.state.pre_execution;
    // let post_execution_accounts = &profile_result
    //     .inner
    //     .transaction_profile
    //     .state
    //     .post_execution;

    // for (_, account_opt) in pre_execution_accounts
    //     .iter()
    //     .chain(post_execution_accounts.iter())
    // {
    //     if let Some(account) = account_opt {
    //         // Verify the account data is base64 encoded
    //         if let UiAccountData::Binary(_, encoding) = &account.data {
    //             assert!(
    //                 *encoding == UiAccountEncoding::Base64,
    //                 "Account data should be base64 encoded"
    //             );
    //         }
    //     }
    // }

    println!("Encoding profiling test passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_profile_transaction_with_tag_and_retrieval(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // Set up test accounts
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    let lamports_to_send = LAMPORTS_PER_SOL;

    // Airdrop SOL to payer
    svm_locker
        .with_svm_writer(|svm| svm.airdrop(&payer.pubkey(), lamports_to_send * 3))
        .unwrap()
        .unwrap();

    // Create a simple transfer transaction
    let instruction = transfer(&payer.pubkey(), &recipient, lamports_to_send);
    let latest_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let message =
        Message::new_with_blockhash(&[instruction], Some(&payer.pubkey()), &latest_blockhash);
    let transaction =
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&payer]).unwrap();

    // Profile with a tag
    let tag = "test_tag_retrieval".to_string();

    let profile_uuid = svm_locker
        .profile_transaction(&None, transaction.clone(), Some(tag.clone()))
        .await
        .unwrap()
        .inner;

    let key = UuidOrSignature::Uuid(profile_uuid);
    let profile_result = svm_locker
        .get_profile_result(key, &RpcProfileResultConfig::default())
        .unwrap()
        .expect("Profile result should exist");

    // Verify transaction profile
    assert!(
        profile_result.transaction_profile.error_message.is_none(),
        "Transaction profiling should succeed"
    );

    // Test retrieval by UUID
    let retrieved_by_uuid = svm_locker
        .get_profile_result(
            UuidOrSignature::Uuid(profile_uuid),
            &RpcProfileResultConfig::default(),
        )
        .unwrap();
    assert!(
        retrieved_by_uuid.is_some(),
        "Profile should be retrievable by UUID"
    );

    let retrieved = retrieved_by_uuid.unwrap();
    assert_eq!(
        retrieved.transaction_profile.compute_units_consumed,
        profile_result.transaction_profile.compute_units_consumed,
        "Retrieved profile should match original profile"
    );

    // Test retrieval by tag
    let retrieved_by_tag = svm_locker
        .get_profile_results_by_tag(tag.clone(), &RpcProfileResultConfig::default())
        .unwrap();
    assert!(
        retrieved_by_tag.is_some(),
        "Profile should be retrievable by tag"
    );

    let tagged_profiles = retrieved_by_tag.unwrap();
    assert_eq!(
        tagged_profiles.len(),
        1,
        "Should have exactly one profile for this tag"
    );

    let tagged_profile = &tagged_profiles[0];
    assert_eq!(
        tagged_profile.key,
        UuidOrSignature::Uuid(profile_uuid),
        "Tagged profile should have the same UUID"
    );

    // Test retrieval with non-existent tag
    let non_existent_tag = "non_existent_tag".to_string();
    let non_existent_result = svm_locker
        .get_profile_results_by_tag(non_existent_tag, &RpcProfileResultConfig::default())
        .unwrap();
    assert!(
        non_existent_result.is_none(),
        "Non-existent tag should return None"
    );

    println!("Tag and retrieval profiling test passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_profile_transaction_empty_instruction(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // Set up test accounts
    let payer = Keypair::new();
    let lamports_to_send = LAMPORTS_PER_SOL;

    // Airdrop SOL to payer
    svm_locker
        .airdrop(&payer.pubkey(), lamports_to_send)
        .unwrap()
        .unwrap();

    // Create a transaction with no instructions
    let latest_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let message = Message::new_with_blockhash(
        &[], // No instructions
        Some(&payer.pubkey()),
        &latest_blockhash,
    );
    let transaction =
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&payer]).unwrap();

    // Profile the empty transaction
    let profile_uuid = svm_locker
        .profile_transaction(&None, transaction.clone(), None)
        .await
        .unwrap()
        .inner;

    let key = UuidOrSignature::Uuid(profile_uuid);
    let profile_result = svm_locker
        .get_profile_result(key, &RpcProfileResultConfig::default())
        .unwrap()
        .expect("Profile result should exist");

    println!("profile result: {:#?}", profile_result);

    // Verify transaction profile
    assert!(
        profile_result.transaction_profile.error_message.is_none(),
        "Empty transaction profiling should succeed"
    );

    // Verify compute units consumed (should be minimal)
    assert_eq!(profile_result.transaction_profile.compute_units_consumed, 0);

    // Verify no instruction profiles for empty transaction
    assert!(
        profile_result.instruction_profiles.is_none(),
        "Empty transaction should not have instruction profiles"
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_profile_transaction_versioned_message(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // Set up test accounts
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    let lamports_to_send = LAMPORTS_PER_SOL;

    // Airdrop SOL to payer
    svm_locker
        .airdrop(&payer.pubkey(), 2 * lamports_to_send)
        .unwrap()
        .unwrap();

    svm_locker.confirm_current_block(&None).await.unwrap();

    // Create a transfer instruction
    let instruction = transfer(&payer.pubkey(), &recipient, lamports_to_send);
    let latest_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());

    // Create a v0 message
    let v0_message = v0::Message::try_compile(
        &payer.pubkey(),
        &[instruction],
        &[], // No address table lookups
        latest_blockhash,
    )
    .expect("Failed to compile v0 message");

    let transaction =
        VersionedTransaction::try_new(VersionedMessage::V0(v0_message), &[&payer]).unwrap();

    // Profile the versioned transaction
    let profile_uuid = svm_locker
        .profile_transaction(&None, transaction.clone(), None)
        .await
        .unwrap()
        .inner;

    let key = UuidOrSignature::Uuid(profile_uuid);
    let profile_result = svm_locker
        .get_profile_result(key, &RpcProfileResultConfig::default())
        .unwrap()
        .expect("Profile result should exist");

    // Verify transaction profile
    assert!(
        profile_result.transaction_profile.error_message.is_none(),
        "Versioned transaction profiling should succeed, found: {:?}",
        profile_result.transaction_profile.error_message.unwrap()
    );
    assert!(
        profile_result.transaction_profile.compute_units_consumed > 0,
        "Versioned transaction should consume compute units"
    );

    println!("Versioned message profiling test passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_get_local_signatures_without_limit(test_type: TestType) {
    let rpc_server = SurfnetCheatcodesRpc::empty();
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();

    let svm_locker_for_context = SurfnetSvmLocker::new(svm_instance.clone());

    let (simnet_cmd_tx, _simnet_cmd_rx) = crossbeam_unbounded::<SimnetCommand>();
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker_for_context.clone(),
        simnet_commands_tx: simnet_cmd_tx,
        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    let payer = Keypair::new();
    let recipient = Keypair::new();
    let lamports_to_send = 1_000_000;

    svm_locker_for_context
        .airdrop(&payer.pubkey(), lamports_to_send * 2)
        .unwrap()
        .unwrap();

    svm_locker_for_context
        .confirm_current_block(&None)
        .await
        .unwrap();

    let create_account_instruction = system_instruction::create_account(
        &payer.pubkey(),
        &recipient.pubkey(),
        lamports_to_send / 2,
        0,
        &solana_sdk_ids::system_program::id(),
    );

    let create_account_tx = Transaction::new_signed_with_payer(
        &[create_account_instruction],
        Some(&payer.pubkey()),
        &[&payer, &recipient],
        svm_locker_for_context.with_svm_reader(|svm| svm.latest_blockhash()),
    );

    let (create_status_tx, _create_status_rx) = crossbeam_channel::bounded(1);
    svm_locker_for_context
        .process_transaction(
            &None,
            create_account_tx.into(),
            create_status_tx,
            false,
            true,
        )
        .await
        .unwrap();
    // Confirm the block after creating the account
    svm_locker_for_context
        .confirm_current_block(&None)
        .await
        .unwrap();

    // Now create the transfer transaction
    let instruction = transfer(&payer.pubkey(), &recipient.pubkey(), lamports_to_send);
    let latest_blockhash = svm_locker_for_context.with_svm_reader(|svm| svm.latest_blockhash());
    let message =
        Message::new_with_blockhash(&[instruction], Some(&payer.pubkey()), &latest_blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message.clone()), &[&payer])
        .unwrap();

    let (status_tx, _status_rx) = crossbeam_channel::bounded(1);

    svm_locker_for_context
        .process_transaction(&None, tx.clone(), status_tx, false, true)
        .await
        .unwrap();

    // Confirm the current block to create a block with the transaction signature
    svm_locker_for_context
        .confirm_current_block(&None)
        .await
        .unwrap();

    let get_local_signatures_response: JsonRpcResult<RpcResponse<Vec<RpcLogsResponse>>> =
        rpc_server
            .get_local_signatures(Some(runloop_context.clone()), None)
            .await;

    assert!(
        get_local_signatures_response.is_ok(),
        "Get local signatures failed: {:?}",
        get_local_signatures_response.err()
    );

    let local_signatures = get_local_signatures_response.unwrap().value;
    assert!(local_signatures.len() > 0);
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_get_local_signatures_with_limit(test_type: TestType) {
    let rpc_server = SurfnetCheatcodesRpc::empty();
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker_for_context = SurfnetSvmLocker::new(svm_instance.clone());

    let (simnet_cmd_tx, _simnet_cmd_rx) = crossbeam_unbounded::<SimnetCommand>();
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker_for_context.clone(),
        simnet_commands_tx: simnet_cmd_tx,
        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    let payer = Keypair::new();
    let _recipient = Keypair::new();
    let lamports_to_send = 10_000_000;

    svm_locker_for_context
        .airdrop(&payer.pubkey(), lamports_to_send * 10)
        .unwrap()
        .unwrap();

    svm_locker_for_context
        .confirm_current_block(&None)
        .await
        .unwrap();

    // Get the initial number of signatures to establish a baseline
    let initial_signatures_response: JsonRpcResult<RpcResponse<Vec<RpcLogsResponse>>> = rpc_server
        .get_local_signatures(Some(runloop_context.clone()), None)
        .await;

    let initial_count = initial_signatures_response.unwrap().value.len();

    // Create multiple transactions to test limit functionality
    let num_transactions = 10;
    let mut transaction_signatures = Vec::new();

    for _i in 0..num_transactions {
        // Create a unique recipient for each transaction
        let unique_recipient = Keypair::new();

        let instruction = transfer(
            &payer.pubkey(),
            &unique_recipient.pubkey(),
            lamports_to_send / num_transactions as u64,
        );
        let latest_blockhash = svm_locker_for_context.with_svm_reader(|svm| svm.latest_blockhash());
        let message =
            Message::new_with_blockhash(&[instruction], Some(&payer.pubkey()), &latest_blockhash);
        let tx =
            VersionedTransaction::try_new(VersionedMessage::Legacy(message.clone()), &[&payer])
                .unwrap();

        let (status_tx, _status_rx) = crossbeam_channel::bounded(1);

        svm_locker_for_context
            .process_transaction(&None, tx.clone(), status_tx, false, true)
            .await
            .unwrap();

        // Store the signature for verification
        transaction_signatures.push(tx.signatures[0]);

        // Confirm the current block to create a new block with this transaction
        svm_locker_for_context
            .confirm_current_block(&None)
            .await
            .unwrap();
    }

    // Test with different limit values
    let test_limits = vec![1, 3, 5, 10, 15];

    for limit in test_limits {
        let get_local_signatures_response: JsonRpcResult<RpcResponse<Vec<RpcLogsResponse>>> =
            rpc_server
                .get_local_signatures(Some(runloop_context.clone()), Some(limit))
                .await;

        assert!(
            get_local_signatures_response.is_ok(),
            "Get local signatures with limit {} failed: {:?}",
            limit,
            get_local_signatures_response.err()
        );

        let local_signatures = get_local_signatures_response.unwrap().value;

        // Verify that the number of returned signatures respects the limit
        assert!(
            local_signatures.len() <= limit as usize,
            "Expected at most {} signatures, but got {}",
            limit,
            local_signatures.len()
        );

        // Verify that we get the expected number of signatures
        // The total expected count should be min(limit, initial_count + num_transactions)
        let total_expected_signatures = initial_count + num_transactions;
        let expected_count = std::cmp::min(limit as usize, total_expected_signatures);

        assert!(
            local_signatures.len() == expected_count,
            "Expected {} signatures with limit {}, but got {} (initial: {}, new: {})",
            expected_count,
            limit,
            local_signatures.len(),
            initial_count,
            num_transactions
        );
    }

    // Test with limit = 0 (should return empty list)
    let get_local_signatures_response: JsonRpcResult<RpcResponse<Vec<RpcLogsResponse>>> =
        rpc_server
            .get_local_signatures(Some(runloop_context.clone()), Some(0))
            .await;

    assert!(
        get_local_signatures_response.is_ok(),
        "Get local signatures with limit 0 failed: {:?}",
        get_local_signatures_response.err()
    );

    let local_signatures = get_local_signatures_response.unwrap().value;
    assert!(
        local_signatures.is_empty(),
        "Expected empty list with limit 0, but got {} signatures",
        local_signatures.len()
    );

    println!("All local signatures tests passed successfully!");
}

// ============================================================================
// Clock Control Tests (pauseClock, resumeClock, timeTravel)
// ============================================================================

fn boot_simnet(
    block_production_mode: BlockProductionMode,
    slot_time: Option<u64>,
    test_type: TestType,
) -> (
    SurfnetSvmLocker,
    crossbeam_channel::Sender<SimnetCommand>,
    crossbeam_channel::Receiver<SimnetEvent>,
) {
    let bind_host = "127.0.0.1";
    let bind_port = get_free_port().unwrap();
    let ws_port = get_free_port().unwrap();
    let config = SurfpoolConfig {
        simnets: vec![SimnetConfig {
            slot_time: slot_time.unwrap_or(DEFAULT_SLOT_TIME_MS),
            block_production_mode,
            ..SimnetConfig::default()
        }],
        rpc: RpcConfig {
            bind_host: bind_host.to_string(),
            bind_port,
            ws_port,
            gossip_port: 0,
            tpu_port: 0,
            tpu_quic_port: 0,
        },
        subgraph: SubgraphConfig::default(),
        ..SurfpoolConfig::default()
    };

    let (surfnet_svm, simnet_events_rx, geyser_events_rx) = test_type.initialize_svm();
    let (simnet_commands_tx, simnet_commands_rx) = unbounded();

    let svm_locker = SurfnetSvmLocker::new(surfnet_svm);

    let svm_locker_cc: SurfnetSvmLocker = svm_locker.clone();
    let simnet_commands_tx_cc = simnet_commands_tx.clone();
    let _handle = hiro_system_kit::thread_named("test").spawn(move || {
        let future = start_local_surfnet_runloop(
            svm_locker_cc,
            config,
            simnet_commands_tx_cc,
            simnet_commands_rx,
            geyser_events_rx,
        );
        if let Err(e) = hiro_system_kit::nestable_block_on(future) {
            panic!("{e:?}");
        }
    });

    loop {
        if let Ok(SimnetEvent::Ready(_)) =
            simnet_events_rx.recv_timeout(Duration::from_millis(1000))
        {
            break;
        }
    }

    (svm_locker, simnet_commands_tx, simnet_events_rx)
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
fn test_time_travel_resume_paused_clock(test_type: TestType) {
    let rpc_server = SurfnetCheatcodesRpc::empty();
    let (svm_locker, simnet_cmd_tx, _) =
        boot_simnet(BlockProductionMode::Clock, Some(100), test_type);
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker.clone(),
        simnet_commands_tx: simnet_cmd_tx,
        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    // Get initial epoch info
    let initial_slot = svm_locker.get_latest_absolute_slot();

    // Ensure the clock has advanced
    std::thread::sleep(Duration::from_millis(500));
    let new_slot = svm_locker.get_latest_absolute_slot();
    assert!(
        new_slot > initial_slot,
        "Slot should change when clock is not paused"
    );

    // Test pause clock
    let _ = rpc_server.pause_clock(Some(runloop_context.clone()));

    // Buffer to ensure the clock is paused
    std::thread::sleep(Duration::from_millis(100));

    // Get latest slot
    let slot_after_pause = svm_locker.get_latest_absolute_slot();

    // Ensure the clock is paused after 500ms
    std::thread::sleep(Duration::from_millis(500));
    let slot_after_pause_and_500ms = svm_locker.get_latest_absolute_slot();
    assert!(
        slot_after_pause == slot_after_pause_and_500ms,
        "Slot should change when clock is not paused"
    );

    // Ensure the clock is still paused after 2s
    std::thread::sleep(Duration::from_millis(2000));
    let slot_after_pause_and_2500ms = svm_locker.get_latest_absolute_slot();
    assert!(
        slot_after_pause == slot_after_pause_and_2500ms,
        "Slot should change when clock is not paused"
    );

    // Now let's resume the clock
    let resume_response: JsonRpcResult<EpochInfo> =
        rpc_server.resume_clock(Some(runloop_context.clone()));
    let slot_after_resume = svm_locker.get_latest_absolute_slot();

    assert!(
        resume_response.is_ok(),
        "Resume clock failed: {:?}",
        resume_response.err()
    );

    // Ensure the clock is paused after 500ms
    std::thread::sleep(Duration::from_millis(500));
    let slot_after_resume_and_500ms = svm_locker.get_latest_absolute_slot();
    assert!(
        slot_after_resume < slot_after_resume_and_500ms,
        "Slot should change when clock is not paused"
    );

    println!("Resume clock test passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
fn test_time_travel_absolute_timestamp(test_type: TestType) {
    let rpc_server = SurfnetCheatcodesRpc::empty();
    let slot_time = 100;
    let (svm_locker, simnet_cmd_tx, simnet_events_rx) = boot_simnet(
        BlockProductionMode::Clock,
        Some(slot_time.clone()),
        test_type,
    );
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker.clone(),
        simnet_commands_tx: simnet_cmd_tx.clone(),

        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    let clock = Clock {
        slot: 1,
        epoch_start_timestamp: 1,
        epoch: 1,
        leader_schedule_epoch: 100,
        unix_timestamp: 100,
    };

    simnet_cmd_tx
        .send(SimnetCommand::UpdateInternalClock(None, clock))
        .unwrap();
    let Ok(SimnetEvent::SystemClockUpdated(_clock_updated)) =
        simnet_events_rx.recv_timeout(Duration::from_millis(5000))
    else {
        panic!("failed to update internal clock")
    };

    let initial_epoch_info = svm_locker.get_epoch_info();

    println!("Initial epoch info: {:?}", initial_epoch_info);

    let seven_days = 7 * 24 * 60 * 60 * 1000;
    let target_timestamp = svm_locker.0.blocking_read().updated_at + seven_days;

    // Test time travel to absolute timestamp
    // Note: time_travel now uses confirmation mechanism, so it waits internally
    let time_travel_response: JsonRpcResult<EpochInfo> = rpc_server.time_travel(
        Some(runloop_context.clone()),
        Some(TimeTravelConfig::AbsoluteTimestamp(target_timestamp)),
    );

    assert!(
        time_travel_response.is_ok(),
        "Time travel to absolute timestamp failed: {:?}",
        time_travel_response.err()
    );

    let new_epoch_info = time_travel_response.unwrap();
    println!("Response: {:?}", new_epoch_info);

    // Verify the epoch info reflects the time travel
    assert_ne!(
        new_epoch_info.epoch, initial_epoch_info.epoch,
        "Epoch should change after time travel"
    );
    assert_ne!(
        new_epoch_info.absolute_slot, initial_epoch_info.absolute_slot,
        "Slot should change after time travel"
    );

    // Verify the current epoch info in SVM matches
    let current_epoch_info = svm_locker.get_epoch_info();
    println!("Updated epoch info: {:?}", current_epoch_info);

    assert_eq!(current_epoch_info.epoch, new_epoch_info.epoch);
    assert_eq!(
        current_epoch_info.absolute_slot,
        new_epoch_info.absolute_slot
    );

    println!("Time travel to absolute timestamp test passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
fn test_time_travel_absolute_slot(test_type: TestType) {
    let rpc_server = SurfnetCheatcodesRpc::empty();
    let (svm_locker, simnet_cmd_tx, simnet_events_rx) =
        boot_simnet(BlockProductionMode::Clock, Some(400), test_type);
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker.clone(),
        simnet_commands_tx: simnet_cmd_tx.clone(),

        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    let clock = Clock {
        slot: 1,
        epoch_start_timestamp: 1,
        epoch: 1,
        leader_schedule_epoch: 100,
        unix_timestamp: 100,
    };

    simnet_cmd_tx
        .send(SimnetCommand::UpdateInternalClock(None, clock))
        .unwrap();
    let Ok(SimnetEvent::SystemClockUpdated(_clock_updated)) =
        simnet_events_rx.recv_timeout(Duration::from_millis(5000))
    else {
        panic!("failed to update internal clock")
    };

    let initial_epoch_info = svm_locker.get_epoch_info();
    let target_slot = initial_epoch_info.absolute_slot + 1000000; // A future slot number

    // Test time travel to absolute slot
    // Note: time_travel now uses confirmation mechanism, so it waits internally
    let time_travel_response: JsonRpcResult<EpochInfo> = rpc_server.time_travel(
        Some(runloop_context.clone()),
        Some(TimeTravelConfig::AbsoluteSlot(target_slot)),
    );

    assert!(
        time_travel_response.is_ok(),
        "Time travel to absolute slot failed: {:?}",
        time_travel_response.err()
    );

    let new_epoch_info = time_travel_response.unwrap();

    // Verify the epoch info reflects the time travel
    assert_eq!(
        new_epoch_info.absolute_slot, target_slot,
        "Slot should match target slot"
    );
    assert!(
        new_epoch_info.epoch > initial_epoch_info.epoch,
        "Epoch should change after time travel"
    );
    assert!(
        new_epoch_info.absolute_slot > initial_epoch_info.absolute_slot,
        "Epoch should change after time travel"
    );

    // Verify the current epoch info in SVM matches
    let current_epoch_info = svm_locker.get_epoch_info();
    assert_eq!(current_epoch_info.absolute_slot, target_slot);
    assert_eq!(current_epoch_info.epoch, new_epoch_info.epoch);

    println!("Time travel to absolute slot test passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
fn test_time_travel_absolute_epoch(test_type: TestType) {
    let rpc_server = SurfnetCheatcodesRpc::empty();
    let (svm_locker, simnet_cmd_tx, simnet_events_rx) =
        boot_simnet(BlockProductionMode::Clock, Some(400), test_type);
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker.clone(),
        simnet_commands_tx: simnet_cmd_tx.clone(),

        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    let clock = Clock {
        slot: 1,
        epoch_start_timestamp: 1,
        epoch: 1,
        leader_schedule_epoch: 100,
        unix_timestamp: 100,
    };

    simnet_cmd_tx
        .send(SimnetCommand::UpdateInternalClock(None, clock))
        .unwrap();
    let Ok(SimnetEvent::SystemClockUpdated(_clock_updated)) =
        simnet_events_rx.recv_timeout(Duration::from_millis(5000))
    else {
        panic!("failed to update internal clock")
    };

    let initial_epoch_info = svm_locker.get_epoch_info();
    let target_epoch = initial_epoch_info.epoch + 100; // A future epoch number

    // Test time travel to absolute epoch
    // Note: time_travel now uses confirmation mechanism, so it waits internally
    let time_travel_response: JsonRpcResult<EpochInfo> = rpc_server.time_travel(
        Some(runloop_context.clone()),
        Some(TimeTravelConfig::AbsoluteEpoch(target_epoch)),
    );

    assert!(
        time_travel_response.is_ok(),
        "Time travel to absolute epoch failed: {:?}",
        time_travel_response.err()
    );

    let new_epoch_info = time_travel_response.unwrap();

    // Verify the epoch info reflects the time travel
    assert_eq!(
        new_epoch_info.epoch, target_epoch,
        "Epoch should match target epoch"
    );
    assert_ne!(
        new_epoch_info.epoch, initial_epoch_info.epoch,
        "Epoch should change after time travel"
    );
    assert_ne!(
        new_epoch_info.absolute_slot, initial_epoch_info.absolute_slot,
        "Slot should change after time travel"
    );

    // Verify the current epoch info in SVM matches
    let current_epoch_info = svm_locker.get_epoch_info();
    assert_eq!(current_epoch_info.epoch, target_epoch);
    assert_eq!(
        current_epoch_info.absolute_slot,
        new_epoch_info.absolute_slot
    );

    println!("Time travel to absolute epoch test passed successfully!");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ix_profiling_with_alt_tx(test_type: TestType) {
    let (svm_locker, _simnet_cmd_tx, _simnet_events_rx) =
        boot_simnet(BlockProductionMode::Clock, Some(400), test_type);

    let p1 = Keypair::new();
    let p2 = Keypair::new();

    svm_locker
        .airdrop(&p1.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();
    svm_locker
        .airdrop(&p2.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());

    let mint = Keypair::new();
    let mint_rent = 1461600;
    let create_mint_ix = system_instruction::create_account(
        &p1.pubkey(),
        &mint.pubkey(),
        mint_rent,
        82,
        &spl_token_2022_interface::id(),
    );

    let initialize_mint_ix = spl_token_2022_interface::instruction::initialize_mint2(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        &p1.pubkey(),
        Some(&p1.pubkey()),
        2,
    )
    .unwrap();

    let at1 = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
        &p1.pubkey(),
        &mint.pubkey(),
        &spl_token_2022_interface::id(),
    );
    let at2 = spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
        &p2.pubkey(),
        &mint.pubkey(),
        &spl_token_2022_interface::id(),
    );

    let create_at1_ix =
        spl_associated_token_account_interface::instruction::create_associated_token_account(
            &p1.pubkey(),
            &p1.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let create_at2_ix =
        spl_associated_token_account_interface::instruction::create_associated_token_account(
            &p1.pubkey(),
            &p2.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let mint_amount = 100_00;
    let mint_to_ix = spl_token_2022_interface::instruction::mint_to(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        &at1,
        &p1.pubkey(),
        &[&p1.pubkey()],
        mint_amount,
    )
    .unwrap();

    let setup_instructions = vec![
        create_mint_ix,
        initialize_mint_ix,
        create_at1_ix,
        create_at2_ix,
        mint_to_ix,
    ];

    let setup_message =
        Message::new_with_blockhash(&setup_instructions, Some(&p1.pubkey()), &recent_blockhash);

    let setup_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(setup_message), &[&p1, &mint])
            .unwrap();

    let status_tx = crossbeam_unbounded::<TransactionStatusEvent>().0;
    svm_locker
        .process_transaction(&None, setup_tx, status_tx, false, true)
        .await
        .unwrap();

    let alt_key = Pubkey::new_unique();

    let address_lookup_table_account = AddressLookupTableAccount {
        key: alt_key,
        addresses: vec![at1, at2, spl_token_2022_interface::id(), mint.pubkey()],
    };

    println!(
        "addresses present on the ALT: {:?}",
        address_lookup_table_account.addresses
    );

    let alt_account_data = AddressLookupTable {
        meta: LookupTableMeta {
            authority: Some(p1.pubkey()),
            ..Default::default()
        },
        addresses: address_lookup_table_account.addresses.clone().into(),
    };

    svm_locker.with_svm_writer(|svm| {
        let alt_data = alt_account_data.serialize_for_tests().unwrap();
        let alt_account = solana_account::Account {
            lamports: 1000000,
            data: alt_data,
            owner: solana_address_lookup_table_interface::program::id(),
            executable: false,
            rent_epoch: 0,
        };

        svm.set_account(&alt_key, alt_account).unwrap();
    });

    let transfer_amount = 50_00;
    let transfer_ix = spl_token_2022_interface::instruction::transfer_checked(
        &spl_token_2022_interface::id(),
        &at1,
        &mint.pubkey(),
        &at2,
        &p1.pubkey(),
        &[&p1.pubkey()],
        transfer_amount,
        2,
    )
    .unwrap();

    let alt_message = v0::Message::try_compile(
        &p1.pubkey(),
        &[transfer_ix],
        &[address_lookup_table_account.clone()],
        recent_blockhash,
    )
    .expect("Failed to compile ALT message");

    let alt_tx = VersionedTransaction::try_new(VersionedMessage::V0(alt_message.clone()), &[&p1])
        .expect("Failed to create ALT transaction");

    let binding = svm_locker
        .profile_transaction(&None, alt_tx.clone(), None)
        .await
        .unwrap();
    let profile_result_uuid = binding.inner();

    let profile_result = svm_locker
        .get_profile_result(
            UuidOrSignature::Uuid(*profile_result_uuid),
            &RpcProfileResultConfig::default(),
        )
        .unwrap()
        .unwrap();
    let ix_profiles = profile_result
        .instruction_profiles
        .as_ref()
        .expect("instruction profiles should exist");
    assert_eq!(
        ix_profiles.len(),
        1,
        "ALT transfer should have one instruction"
    );
    let ix_profile = ix_profiles.get(0).unwrap();
    assert!(
        ix_profile.error_message.is_none(),
        "Profile should succeed, found error: {:?}",
        ix_profile.error_message.as_ref().unwrap()
    );
    assert!(
        ix_profile.account_states.get(&alt_key).is_none(),
        "ALT account should not be in instruction account states"
    );
    assert!(
        profile_result
            .readonly_account_states
            .get(&alt_key)
            .is_none(),
        "ALT account should not be in readonly account states"
    );

    let table = alt_message
        .address_table_lookups
        .first()
        .expect("ALT lookups should exist");
    let expected_loaded_writable: Vec<Pubkey> = table
        .writable_indexes
        .iter()
        .map(|&i| address_lookup_table_account.addresses[i as usize])
        .collect();
    let expected_loaded_readonly: Vec<Pubkey> = table
        .readonly_indexes
        .iter()
        .map(|&i| address_lookup_table_account.addresses[i as usize])
        .collect();

    for pk in &expected_loaded_writable {
        match ix_profile
            .account_states
            .get(pk)
            .expect("loaded writable address must be present")
        {
            UiAccountProfileState::Writable(_) => {}
            other => panic!(
                "expected Writable for loaded writable address {}, got {:?}",
                pk, other
            ),
        }
    }
    for pk in &expected_loaded_readonly {
        match ix_profile
            .account_states
            .get(pk)
            .expect("loaded readonly address must be present")
        {
            UiAccountProfileState::Readonly => {}
            other => panic!(
                "expected Readonly for loaded readonly address {}, got {:?}",
                pk, other
            ),
        }
    }

    let account_states = ix_profile.account_states.clone();

    let UiAccountProfileState::Readonly = account_states
        .get(&spl_token_2022_interface::id())
        .expect("token-2022 program should be present")
    else {
        panic!("expected token-2022 program to be Readonly");
    };

    let UiAccountProfileState::Readonly = account_states
        .get(&mint.pubkey())
        .expect("mint should be present")
    else {
        panic!("expected mint to be Readonly");
    };

    let UiAccountProfileState::Writable(src_change) = account_states
        .get(&at1)
        .expect("source ATA should be present")
    else {
        panic!("expected source ATA to be Writable");
    };
    match src_change {
        UiAccountChange::Update(before, after) => {
            assert_ne!(
                before.data, after.data,
                "token data should change on source"
            );
        }
        _ => panic!("expected Update for source ATA"),
    }

    let UiAccountProfileState::Writable(dst_change) = account_states
        .get(&at2)
        .expect("destination ATA should be present")
    else {
        panic!("expected destination ATA to be Writable");
    };
    match dst_change {
        UiAccountChange::Update(before, after) => {
            assert_ne!(
                before.data, after.data,
                "token data should change on destination"
            );
        }
        _ => panic!("expected Update for destination ATA"),
    }
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn it_should_delete_accounts_with_no_lamports(test_type: TestType) {
    let (svm_locker, _simnet_cmd_tx, _simnet_events_rx) =
        boot_simnet(BlockProductionMode::Clock, Some(400), test_type);
    let p1 = Keypair::new();
    let p2 = Keypair::new();

    svm_locker
        .airdrop(&p1.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());

    let message = Message::new_with_blockhash(
        &[system_instruction::transfer(
            &p1.pubkey(),
            &p2.pubkey(),
            LAMPORTS_PER_SOL - 5000,
        )],
        Some(&p1.pubkey()),
        &recent_blockhash,
    );
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&p1]).unwrap();

    let (status_tx, rx) = unbounded();
    let _ = svm_locker
        .process_transaction(&None, tx, status_tx, true, false)
        .await
        .unwrap();

    #[allow(clippy::never_loop)]
    loop {
        match rx.recv() {
            Ok(status) => {
                println!("Transaction status: {:?}", status);
                break;
            }
            Err(_) => panic!("status channel closed unexpectedly"),
        }
    }

    assert!(
        svm_locker.get_account_local(&p1.pubkey()).inner.is_none(),
        "Account should be deleted"
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_compute_budget_profiling(test_type: TestType) {
    let (svm_locker, _simnet_cmd_tx, _simnet_events_rx) =
        boot_simnet(BlockProductionMode::Clock, Some(400), test_type);
    let p1 = Keypair::new();
    let p2 = Keypair::new();

    svm_locker
        .airdrop(&p1.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());

    let message = Message::new_with_blockhash(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_000_000),
            ComputeBudgetInstruction::set_compute_unit_price(1),
            system_instruction::transfer(&p1.pubkey(), &p2.pubkey(), LAMPORTS_PER_SOL),
        ],
        Some(&p1.pubkey()),
        &recent_blockhash,
    );
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&p1]).unwrap();

    let uuid = svm_locker
        .profile_transaction(&None, tx, None)
        .await
        .unwrap()
        .inner;
    let profile_result = svm_locker
        .get_profile_result(
            UuidOrSignature::Uuid(uuid),
            &RpcProfileResultConfig::default(),
        )
        .unwrap()
        .unwrap();

    let ix_profile = profile_result.instruction_profiles.unwrap();
    assert_eq!(ix_profile.len(), 3, "Should have 3 instruction profiles");

    let ix = &ix_profile[0];
    assert!(
        ix.error_message.is_none(),
        "Expected no error for instruction, found {}",
        ix.error_message.as_ref().unwrap()
    );
    assert_eq!(ix.compute_units_consumed, 150);

    let ix = &ix_profile[1];
    assert!(
        ix.error_message.is_none(),
        "Expected no error for instruction, found {}",
        ix.error_message.as_ref().unwrap()
    );
    assert_eq!(ix.compute_units_consumed, 150);

    let ix = &ix_profile[1];
    assert!(
        ix.error_message.is_none(),
        "Expected no error for instruction, found {}",
        ix.error_message.as_ref().unwrap()
    );
    assert_eq!(ix.compute_units_consumed, 150);
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
fn test_reset_account(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);
    let p1 = Keypair::new();
    println!("P1 pubkey: {}", p1.pubkey());
    svm_locker
        .airdrop(&p1.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap(); // account is created in the SVM.unwrap()
    println!("Airdropped SOL to p1");

    println!(
        "Account before reset: {:?}",
        svm_locker.get_account_local(&p1.pubkey()).inner
    );

    svm_locker.reset_account(p1.pubkey(), false).unwrap();

    println!("Reset account");

    println!(
        "Account deleted: {:?}",
        svm_locker.get_account_local(&p1.pubkey()).inner
    );

    assert!(
        svm_locker.get_account_local(&p1.pubkey()).inner.is_none(),
        "Account should be deleted"
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
fn test_reset_account_cascade(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // Create owner account and owned account
    let owner = Pubkey::new_unique();
    let owned = Pubkey::new_unique();

    let owner_account = Account {
        lamports: 10 * LAMPORTS_PER_SOL,
        data: vec![0x01, 0x02],
        owner: solana_sdk_ids::system_program::id(),
        executable: false,
        rent_epoch: 0,
    };

    let owned_account = Account {
        lamports: 5 * LAMPORTS_PER_SOL,
        data: vec![0x03, 0x04],
        owner, // Owned by the first account
        executable: false,
        rent_epoch: 0,
    };

    // Insert accounts
    svm_locker
        .with_svm_writer(|svm_writer| {
            svm_writer.set_account(&owner, owner_account).unwrap();
            svm_writer.set_account(&owned, owned_account).unwrap();
            Ok::<(), SurfpoolError>(())
        })
        .unwrap();

    // Verify accounts exist
    assert!(!svm_locker.get_account_local(&owner).inner.is_none());
    assert!(!svm_locker.get_account_local(&owned).inner.is_none());

    // Reset with cascade=true (for regular accounts, doesn't cascade but tests the code path)
    svm_locker.reset_account(owner, true).unwrap();

    // Owner is deleted, owned account is deleted
    assert!(svm_locker.get_account_local(&owner).inner.is_none());
    assert!(svm_locker.get_account_local(&owned).inner.is_none());

    // Clean up
    svm_locker.reset_account(owned, false).unwrap();
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_reset_streamed_account(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);
    let p1 = Keypair::new();
    println!("P1 pubkey: {}", p1.pubkey());
    svm_locker
        .airdrop(&p1.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap(); // account is created in the SVM.unwrap()
    println!("Airdropped SOL to p1");

    let _ = svm_locker.confirm_current_block(&None).await;
    // Account still exists
    assert!(!svm_locker.get_account_local(&p1.pubkey()).inner.is_none());

    svm_locker.stream_account(p1.pubkey(), false).unwrap();

    let _ = svm_locker.confirm_current_block(&None).await;
    // Account is cleaned up as soon as the block is processed
    assert!(
        svm_locker.get_account_local(&p1.pubkey()).inner.is_none(),
        "Streamed account should be deleted"
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_reset_streamed_account_cascade(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // Create owner account and owned account
    let owner = Pubkey::new_unique();
    let owned = Pubkey::new_unique();

    let owner_account = Account {
        lamports: 10 * LAMPORTS_PER_SOL,
        data: vec![0x01, 0x02],
        owner: solana_sdk_ids::system_program::id(),
        executable: false,
        rent_epoch: 0,
    };

    let owned_account = Account {
        lamports: 5 * LAMPORTS_PER_SOL,
        data: vec![0x03, 0x04],
        owner, // Owned by the first account
        executable: false,
        rent_epoch: 0,
    };

    // Insert accounts
    svm_locker
        .with_svm_writer(|svm_writer| {
            svm_writer.set_account(&owner, owner_account).unwrap();
            svm_writer.set_account(&owned, owned_account).unwrap();
            Ok::<(), SurfpoolError>(())
        })
        .unwrap();

    // Verify accounts exist
    assert!(!svm_locker.get_account_local(&owner).inner.is_none());
    assert!(!svm_locker.get_account_local(&owned).inner.is_none());

    let _ = svm_locker.confirm_current_block(&None).await;
    // Accounts still exists
    assert!(!svm_locker.get_account_local(&owner).inner.is_none());
    assert!(!svm_locker.get_account_local(&owned).inner.is_none());

    svm_locker.stream_account(owner, true).unwrap();
    let _ = svm_locker.confirm_current_block(&None).await;

    // Owner is deleted, owned account is deleted
    assert!(svm_locker.get_account_local(&owner).inner.is_none());
    assert!(svm_locker.get_account_local(&owned).inner.is_none());
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_stream_accounts_multiple(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let p1 = Keypair::new();
    let p2 = Keypair::new();
    let p3 = Keypair::new();

    // Airdrop to all three accounts
    for kp in [&p1, &p2, &p3] {
        svm_locker
            .airdrop(&kp.pubkey(), LAMPORTS_PER_SOL)
            .unwrap()
            .unwrap();
    }

    let _ = svm_locker.confirm_current_block(&None).await;

    // All accounts exist
    assert!(!svm_locker.get_account_local(&p1.pubkey()).inner.is_none());
    assert!(!svm_locker.get_account_local(&p2.pubkey()).inner.is_none());
    assert!(!svm_locker.get_account_local(&p3.pubkey()).inner.is_none());

    // Stream all three accounts at once
    for kp in [&p1, &p2, &p3] {
        svm_locker.stream_account(kp.pubkey(), false).unwrap();
    }

    let _ = svm_locker.confirm_current_block(&None).await;

    // All accounts should be evicted
    assert!(
        svm_locker.get_account_local(&p1.pubkey()).inner.is_none(),
        "First streamed account should be deleted"
    );
    assert!(
        svm_locker.get_account_local(&p2.pubkey()).inner.is_none(),
        "Second streamed account should be deleted"
    );
    assert!(
        svm_locker.get_account_local(&p3.pubkey()).inner.is_none(),
        "Third streamed account should be deleted"
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_stream_accounts_with_cascade(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // Create two owners each with an owned account
    let owner1 = Pubkey::new_unique();
    let owned1 = Pubkey::new_unique();
    let owner2 = Pubkey::new_unique();
    let owned2 = Pubkey::new_unique();

    svm_locker
        .with_svm_writer(|svm_writer| {
            svm_writer
                .set_account(
                    &owner1,
                    Account {
                        lamports: 10 * LAMPORTS_PER_SOL,
                        data: vec![0x01],
                        owner: solana_sdk_ids::system_program::id(),
                        executable: false,
                        rent_epoch: 0,
                    },
                )
                .unwrap();
            svm_writer
                .set_account(
                    &owned1,
                    Account {
                        lamports: 5 * LAMPORTS_PER_SOL,
                        data: vec![0x02],
                        owner: owner1,
                        executable: false,
                        rent_epoch: 0,
                    },
                )
                .unwrap();
            svm_writer
                .set_account(
                    &owner2,
                    Account {
                        lamports: 10 * LAMPORTS_PER_SOL,
                        data: vec![0x03],
                        owner: solana_sdk_ids::system_program::id(),
                        executable: false,
                        rent_epoch: 0,
                    },
                )
                .unwrap();
            svm_writer
                .set_account(
                    &owned2,
                    Account {
                        lamports: 5 * LAMPORTS_PER_SOL,
                        data: vec![0x04],
                        owner: owner2,
                        executable: false,
                        rent_epoch: 0,
                    },
                )
                .unwrap();
            Ok::<(), SurfpoolError>(())
        })
        .unwrap();

    // Verify all accounts exist
    assert!(!svm_locker.get_account_local(&owner1).inner.is_none());
    assert!(!svm_locker.get_account_local(&owned1).inner.is_none());
    assert!(!svm_locker.get_account_local(&owner2).inner.is_none());
    assert!(!svm_locker.get_account_local(&owned2).inner.is_none());

    let _ = svm_locker.confirm_current_block(&None).await;

    // Stream both owners with cascade
    svm_locker.stream_account(owner1, true).unwrap();
    svm_locker.stream_account(owner2, true).unwrap();

    let _ = svm_locker.confirm_current_block(&None).await;

    // All four accounts should be evicted
    assert!(
        svm_locker.get_account_local(&owner1).inner.is_none(),
        "First owner should be deleted"
    );
    assert!(
        svm_locker.get_account_local(&owned1).inner.is_none(),
        "First owned account should be deleted"
    );
    assert!(
        svm_locker.get_account_local(&owner2).inner.is_none(),
        "Second owner should be deleted"
    );
    assert!(
        svm_locker.get_account_local(&owned2).inner.is_none(),
        "Second owned account should be deleted"
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_stream_accounts_empty(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let p1 = Keypair::new();
    svm_locker
        .airdrop(&p1.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let _ = svm_locker.confirm_current_block(&None).await;
    assert!(!svm_locker.get_account_local(&p1.pubkey()).inner.is_none());

    // Streaming zero accounts should not error and should not affect existing accounts
    let _ = svm_locker.confirm_current_block(&None).await;

    assert!(
        !svm_locker.get_account_local(&p1.pubkey()).inner.is_none(),
        "Existing account should not be affected when no accounts are streamed"
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
fn test_reset_network(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // Create owner account and owned account
    let owner = Pubkey::new_unique();
    let owned = Pubkey::new_unique();

    let owner_account = Account {
        lamports: 10 * LAMPORTS_PER_SOL,
        data: vec![0x01, 0x02],
        owner: solana_sdk_ids::system_program::id(),
        executable: false,
        rent_epoch: 0,
    };

    let owned_account = Account {
        lamports: 5 * LAMPORTS_PER_SOL,
        data: vec![0x03, 0x04],
        owner, // Owned by the first account
        executable: false,
        rent_epoch: 0,
    };

    // Insert accounts
    svm_locker
        .with_svm_writer(|svm_writer| {
            svm_writer.set_account(&owner, owner_account).unwrap();
            svm_writer.set_account(&owned, owned_account).unwrap();
            Ok::<(), SurfpoolError>(())
        })
        .unwrap();

    // Verify accounts exist
    assert!(!svm_locker.get_account_local(&owner).inner.is_none());
    assert!(!svm_locker.get_account_local(&owned).inner.is_none());

    // Reset with cascade=true (for regular accounts, doesn't cascade but tests the code path)
    hiro_system_kit::nestable_block_on(svm_locker.reset_network(&None)).unwrap();

    // Owner is deleted, owned account is deleted
    assert!(svm_locker.get_account_local(&owner).inner.is_none());
    assert!(svm_locker.get_account_local(&owned).inner.is_none());

    // Clean up
    svm_locker.reset_account(owned, false).unwrap();
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
fn test_reset_network_keeps_latest_blockhash_valid(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    hiro_system_kit::nestable_block_on(svm_locker.reset_network(&None)).unwrap();

    let epoch_schedule =
        svm_locker.with_svm_reader(|svm_reader| svm_reader.inner.get_sysvar::<EpochSchedule>());
    let (genesis_slot, latest_slot) = svm_locker.with_svm_reader(|svm_reader| {
        (
            svm_reader.genesis_slot,
            svm_reader.latest_epoch_info.absolute_slot,
        )
    });
    assert_eq!(genesis_slot, FINALIZATION_SLOT_THRESHOLD);
    assert_eq!(latest_slot, FINALIZATION_SLOT_THRESHOLD);
    assert_eq!(epoch_schedule, EpochSchedule::without_warmup());

    let latest_blockhash = svm_locker.with_svm_reader(|svm_reader| svm_reader.latest_blockhash());
    let is_recent = svm_locker
        .with_svm_reader(|svm_reader| svm_reader.check_blockhash_is_recent(&latest_blockhash));
    assert!(
        is_recent,
        "latest_blockhash returned after reset should be present in RecentBlockhashes sysvar",
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
fn test_reset_network_time_travel_timestamp(test_type: TestType) {
    let rpc_server = SurfnetCheatcodesRpc::empty();
    let (svm_locker, simnet_cmd_tx, simnet_events_rx) =
        boot_simnet(BlockProductionMode::Clock, Some(400), test_type);
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker.clone(),
        simnet_commands_tx: simnet_cmd_tx,
        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    // Calculate a target timestamp in the future
    let seven_days = 7 * 24 * 60 * 60 * 1000;
    let target_timestamp = svm_locker.0.blocking_read().updated_at + seven_days;

    // First time travel to target timestamp
    // Note: time_travel now uses confirmation mechanism, so it waits internally
    let time_travel_response: JsonRpcResult<EpochInfo> = rpc_server.time_travel(
        Some(runloop_context.clone()),
        Some(TimeTravelConfig::AbsoluteTimestamp(target_timestamp)),
    );
    assert!(
        time_travel_response.is_ok(),
        "First time travel should succeed"
    );

    // Reset network
    let reset_response: JsonRpcResult<RpcResponse<()>> =
        hiro_system_kit::nestable_block_on(rpc_server.reset_network(Some(runloop_context.clone())));
    assert!(reset_response.is_ok(), "Reset network should succeed");

    // Second time travel to the same timestamp should now succeed after reset
    // because updated_at was reset to current time
    let time_travel_response2: JsonRpcResult<EpochInfo> = rpc_server.time_travel(
        Some(runloop_context.clone()),
        Some(TimeTravelConfig::AbsoluteTimestamp(target_timestamp)),
    );
    assert!(
        time_travel_response2.is_ok(),
        "Second time travel should succeed after reset. Error: {:?}",
        time_travel_response2.err()
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
fn test_reset_network_time_travel_slot(test_type: TestType) {
    let rpc_server = SurfnetCheatcodesRpc::empty();
    let (svm_locker, simnet_cmd_tx, simnet_events_rx) =
        boot_simnet(BlockProductionMode::Clock, Some(400), test_type);
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker.clone(),
        simnet_commands_tx: simnet_cmd_tx,
        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    // Do an initial reset to ensure we start from the offline baseline slot
    let initial_reset: JsonRpcResult<RpcResponse<()>> =
        hiro_system_kit::nestable_block_on(rpc_server.reset_network(Some(runloop_context.clone())));
    assert!(initial_reset.is_ok(), "Initial reset should succeed");

    // Target slot to time travel to (must be greater than the reset slot)
    let target_slot = 1000;

    // First time travel to target slot
    // Note: time_travel now uses confirmation mechanism, so it waits internally
    let time_travel_response: JsonRpcResult<EpochInfo> = rpc_server.time_travel(
        Some(runloop_context.clone()),
        Some(TimeTravelConfig::AbsoluteSlot(target_slot)),
    );
    assert!(
        time_travel_response.is_ok(),
        "First time travel should succeed"
    );

    // Reset network
    let reset_response: JsonRpcResult<RpcResponse<()>> =
        hiro_system_kit::nestable_block_on(rpc_server.reset_network(Some(runloop_context.clone())));
    assert!(reset_response.is_ok(), "Reset network should succeed");

    // Second time travel to the same slot should now succeed after reset
    // because latest_epoch_info.absolute_slot was reset to the offline baseline
    let time_travel_response2: JsonRpcResult<EpochInfo> = rpc_server.time_travel(
        Some(runloop_context.clone()),
        Some(TimeTravelConfig::AbsoluteSlot(target_slot)),
    );
    assert!(
        time_travel_response2.is_ok(),
        "Second time travel should succeed after reset. Error: {:?}",
        time_travel_response2.err()
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
fn test_reset_network_time_travel_epoch(test_type: TestType) {
    let rpc_server = SurfnetCheatcodesRpc::empty();
    let (svm_locker, simnet_cmd_tx, simnet_events_rx) =
        boot_simnet(BlockProductionMode::Clock, Some(400), test_type);
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker.clone(),
        simnet_commands_tx: simnet_cmd_tx,
        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    // Do an initial reset to ensure we start from epoch 0
    let initial_reset: JsonRpcResult<RpcResponse<()>> =
        hiro_system_kit::nestable_block_on(rpc_server.reset_network(Some(runloop_context.clone())));
    assert!(initial_reset.is_ok(), "Initial reset should succeed");

    // Target epoch to time travel to (must be greater than 0 after reset)
    let target_epoch = 5;

    // First time travel to target epoch
    let time_travel_response: JsonRpcResult<EpochInfo> = rpc_server.time_travel(
        Some(runloop_context.clone()),
        Some(TimeTravelConfig::AbsoluteEpoch(target_epoch)),
    );
    // Note: time_travel now uses confirmation mechanism, so it waits internally
    assert!(
        time_travel_response.is_ok(),
        "First time travel should succeed"
    );

    // Reset network
    let reset_response: JsonRpcResult<RpcResponse<()>> =
        hiro_system_kit::nestable_block_on(rpc_server.reset_network(Some(runloop_context.clone())));
    assert!(reset_response.is_ok(), "Reset network should succeed");

    // Second time travel to the same epoch should now succeed after reset
    // because latest_epoch_info.epoch was reset to 0
    let time_travel_response2: JsonRpcResult<EpochInfo> = rpc_server.time_travel(
        Some(runloop_context.clone()),
        Some(TimeTravelConfig::AbsoluteEpoch(target_epoch)),
    );
    assert!(
        time_travel_response2.is_ok(),
        "Second time travel should succeed after reset. Error: {:?}",
        time_travel_response2.err()
    );
}

fn start_surfnet(
    airdrop_addresses: Vec<Pubkey>,
    datasource_rpc_url: Option<String>,
    test_type: TestType,
) -> Result<(String, SurfnetSvmLocker), String> {
    let bind_host = "127.0.0.1";
    let bind_port = get_free_port().unwrap();
    let ws_port = get_free_port().unwrap();
    let offline_mode = datasource_rpc_url.is_none();

    let config = SurfpoolConfig {
        simnets: vec![SimnetConfig {
            slot_time: 1,
            airdrop_addresses,
            airdrop_token_amount: LAMPORTS_PER_SOL,
            offline_mode,
            remote_rpc_url: datasource_rpc_url,
            ..SimnetConfig::default()
        }],
        rpc: RpcConfig {
            bind_host: bind_host.to_string(),
            bind_port,
            ws_port,
            ..Default::default()
        },
        ..SurfpoolConfig::default()
    };

    let (surfnet_svm, simnet_events_rx, geyser_events_rx) = test_type.initialize_svm();
    let (simnet_commands_tx, simnet_commands_rx) = unbounded();
    let svm_locker = SurfnetSvmLocker::new(surfnet_svm);
    let svm_locker_cc: SurfnetSvmLocker = svm_locker.clone();

    let _handle = std::thread::spawn(move || {
        let future = start_local_surfnet_runloop(
            svm_locker_cc,
            config,
            simnet_commands_tx,
            simnet_commands_rx,
            geyser_events_rx,
        );
        if let Err(e) = hiro_system_kit::nestable_block_on(future) {
            panic!("{e:?}");
        }
    });

    let mut ready = false;
    // don't wait for connection in offline mode
    let mut connected = offline_mode;
    loop {
        match simnet_events_rx.recv() {
            Ok(SimnetEvent::Ready(_)) => {
                ready = true;
            }
            Ok(SimnetEvent::Connected(_)) => {
                connected = true;
            }
            _ => (),
        }
        if ready && connected {
            break;
        }
    }

    Ok((format!("http://{}:{}", bind_host, bind_port), svm_locker))
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[tokio::test(flavor = "multi_thread")]
async fn test_closed_accounts(test_type: TestType) {
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let another_test_type = match &test_type {
        TestType::OnDiskSqlite(_) => TestType::sqlite(),
        TestType::InMemorySqlite => TestType::in_memory(),
        TestType::NoDb => TestType::no_db(),
        #[cfg(feature = "postgres")]
        TestType::Postgres { url, .. } => TestType::Postgres {
            url: url.clone(),
            surfnet_id: crate::storage::tests::random_surfnet_id(),
        },
    };
    // Start datasource surfnet first, which will only have accounts we airdrop to
    let (datasource_surfnet_url, _datasource_svm_locker) =
        start_surfnet(vec![pubkey], None, test_type).expect("Failed to start datasource surfnet");
    println!("Datasource surfnet started at {}", datasource_surfnet_url);

    // Now start the test surfnet which forks the datasource surfnet
    let (surfnet_url, surfnet_svm_locker) =
        start_surfnet(vec![], Some(datasource_surfnet_url), another_test_type)
            .expect("Failed to start surfnet");
    println!("Surfnet started at {}", surfnet_url);

    let rpc_client = RpcClient::new(surfnet_url);

    // Verify that when we fetch our account from the surfnet, it will fetch it from the datasource surfnet
    {
        let account = rpc_client
            .get_account(&pubkey)
            .await
            .expect("Failed to get account from surfnet");
        assert_eq!(
            account.lamports, LAMPORTS_PER_SOL,
            "Account lamports should match airdrop amount"
        );
        println!("Account fetched successfully from datasource surfnet");
    }

    // Verify that if we send the `reset_account` cheatcode, the account will no longer exist locally, but will be fetched from the datasource surfnet again
    {
        let _: serde_json::Value = rpc_client
            .send(
                solana_client::rpc_request::RpcRequest::Custom {
                    method: "surfnet_resetAccount",
                },
                json!([pubkey.to_string()]),
            )
            .await
            .expect("Failed to reset account");

        assert!(
            surfnet_svm_locker
                .get_account_local(&pubkey)
                .inner
                .is_none(),
            "Account should be deleted locally after reset_account"
        );

        let account = rpc_client
            .get_account(&pubkey)
            .await
            .expect("Failed to get account from surfnet after reset_account");
        assert_eq!(
            account.lamports, LAMPORTS_PER_SOL,
            "Account should be re-fetched from remote after reset_account"
        );
        println!("Account re-fetched successfully from datasource surfnet after reset_account");
    }

    // Verify that if we send a transaction that closes the account, the account is deleted locally and will not be re-fetched from the datasource surfnet
    {
        let recent_blockhash = rpc_client
            .get_latest_blockhash()
            .await
            .expect("Failed to get latest blockhash");

        let recipient = Pubkey::new_unique();
        let close_ix = system_instruction::transfer(&pubkey, &recipient, LAMPORTS_PER_SOL - 5000);

        println!(
            "Sending {} from {} to {}",
            LAMPORTS_PER_SOL - 5000,
            pubkey,
            recipient
        );
        let message = Message::new_with_blockhash(&[close_ix], Some(&pubkey), &recent_blockhash);
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&keypair])
            .expect("Failed to create transaction");

        let _sig = rpc_client
            .send_and_confirm_transaction(&tx)
            .await
            .expect("Failed to send and confirm transaction");

        let tx = rpc_client
            .get_transaction(
                &_sig,
                solana_transaction_status::UiTransactionEncoding::JsonParsed,
            )
            .await
            .expect("Failed to get transaction");

        println!("Transaction details: {:?}", tx);
        let _ = rpc_client
            .get_signature_status_with_commitment(&_sig, CommitmentConfig::finalized())
            .await
            .expect("Failed to get transaction status")
            .expect("Transaction status not found")
            .unwrap();

        assert!(
            surfnet_svm_locker
                .get_account_local(&pubkey)
                .inner
                .is_none(),
            "Account should be deleted locally after being closed"
        );

        println!("Fetching account after being closed...");
        let account_result = rpc_client.get_account(&pubkey).await;
        assert!(
            account_result.is_err(),
            "Account should not be re-fetched from remote after being closed; found {:?}",
            account_result.unwrap()
        );
        println!("Account successfully closed and not re-fetched from datasource surfnet");
    }
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[tokio::test(flavor = "multi_thread")]
async fn test_offline_account_including_owned_accounts(test_type: TestType) {
    let owner = Pubkey::new_unique();
    let owned = Pubkey::new_unique();
    let another_test_type = match &test_type {
        TestType::OnDiskSqlite(_) => TestType::sqlite(),
        TestType::InMemorySqlite => TestType::in_memory(),
        TestType::NoDb => TestType::no_db(),
        #[cfg(feature = "postgres")]
        TestType::Postgres { url, .. } => TestType::Postgres {
            url: url.clone(),
            surfnet_id: crate::storage::tests::random_surfnet_id(),
        },
    };

    let (datasource_surfnet_url, datasource_svm_locker) =
        start_surfnet(vec![], None, test_type).expect("Failed to start datasource surfnet");

    datasource_svm_locker
        .with_svm_writer(|svm_writer| {
            svm_writer
                .set_account(
                    &owner,
                    Account {
                        lamports: LAMPORTS_PER_SOL,
                        data: vec![1, 2, 3],
                        owner: system_program::id(),
                        executable: false,
                        rent_epoch: 0,
                    },
                )
                .unwrap();
            svm_writer
                .set_account(
                    &owned,
                    Account {
                        lamports: LAMPORTS_PER_SOL / 2,
                        data: vec![4, 5, 6],
                        owner,
                        executable: false,
                        rent_epoch: 0,
                    },
                )
                .unwrap();
            Ok::<(), SurfpoolError>(())
        })
        .expect("Failed to seed datasource accounts");

    let (surfnet_url, surfnet_svm_locker) =
        start_surfnet(vec![], Some(datasource_surfnet_url), another_test_type)
            .expect("Failed to start surfnet");
    let rpc_client = RpcClient::new(surfnet_url);

    let _: serde_json::Value = rpc_client
        .send(
            solana_client::rpc_request::RpcRequest::Custom {
                method: "surfnet_offlineAccount",
            },
            serde_json::json!([owner.to_string(), { "includeOwnedAccounts": true }]),
        )
        .await
        .expect("Failed to set account offline");

    assert!(
        surfnet_svm_locker.is_account_offline(&owner),
        "Owner should be recorded as offline"
    );
    assert!(
        surfnet_svm_locker
            .get_offline_account_owners()
            .contains(&owner),
        "Owner should be recorded as a offline account owner"
    );

    let owner_result = rpc_client.get_account(&owner).await;
    assert!(
        owner_result.is_err(),
        "Offline owner account should not be fetched from remote"
    );
    assert!(
        surfnet_svm_locker.get_account_local(&owner).inner.is_none(),
        "Offline owner account should remain absent locally"
    );

    let owned_result = rpc_client.get_account(&owned).await;
    assert!(
        owned_result.is_err(),
        "Owned account should not be fetched from remote once marked offline"
    );
    assert!(
        surfnet_svm_locker.get_account_local(&owned).inner.is_none(),
        "Offline owned account should remain absent locally"
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[tokio::test(flavor = "multi_thread")]
async fn test_offline_account_without_owned_accounts(test_type: TestType) {
    let owner = Pubkey::new_unique();
    let owned = Pubkey::new_unique();
    let another_test_type = match &test_type {
        TestType::OnDiskSqlite(_) => TestType::sqlite(),
        TestType::InMemorySqlite => TestType::in_memory(),
        TestType::NoDb => TestType::no_db(),
        #[cfg(feature = "postgres")]
        TestType::Postgres { url, .. } => TestType::Postgres {
            url: url.clone(),
            surfnet_id: crate::storage::tests::random_surfnet_id(),
        },
    };

    let (datasource_surfnet_url, datasource_svm_locker) =
        start_surfnet(vec![], None, test_type).expect("Failed to start datasource surfnet");

    datasource_svm_locker
        .with_svm_writer(|svm_writer| {
            svm_writer
                .set_account(
                    &owner,
                    Account {
                        lamports: LAMPORTS_PER_SOL,
                        data: vec![1, 2, 3],
                        owner: system_program::id(),
                        executable: false,
                        rent_epoch: 0,
                    },
                )
                .unwrap();
            svm_writer
                .set_account(
                    &owned,
                    Account {
                        lamports: LAMPORTS_PER_SOL / 2,
                        data: vec![4, 5, 6],
                        owner,
                        executable: false,
                        rent_epoch: 0,
                    },
                )
                .unwrap();
            Ok::<(), SurfpoolError>(())
        })
        .expect("Failed to seed datasource accounts");

    let (surfnet_url, surfnet_svm_locker) =
        start_surfnet(vec![], Some(datasource_surfnet_url), another_test_type)
            .expect("Failed to start surfnet");
    let rpc_client = RpcClient::new(surfnet_url);

    // Mark owner offline WITHOUT includeOwnedAccounts
    let _: serde_json::Value = rpc_client
        .send(
            solana_client::rpc_request::RpcRequest::Custom {
                method: "surfnet_offlineAccount",
            },
            serde_json::json!([owner.to_string()]),
        )
        .await
        .expect("Failed to set account offline");

    assert!(
        surfnet_svm_locker.is_account_offline(&owner),
        "Owner should be recorded as offline"
    );
    assert!(
        !surfnet_svm_locker
            .get_offline_account_owners()
            .contains(&owner),
        "Owner should NOT be in offline account owners (includeOwnedAccounts=false)"
    );

    // Offline account itself cannot be fetched from remote
    let owner_result = rpc_client.get_account(&owner).await;
    assert!(
        owner_result.is_err(),
        "Offline owner account should not be fetched from remote"
    );

    // Account owned by the offline owner CAN still be fetched
    let owned_result = rpc_client.get_account(&owned).await;
    assert!(
        owned_result.is_ok(),
        "Owned account should still be fetchable when owner blocking is not active"
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[tokio::test(flavor = "multi_thread")]
async fn test_reset_account_after_offline_restores(test_type: TestType) {
    let account = Pubkey::new_unique();
    let another_test_type = match &test_type {
        TestType::OnDiskSqlite(_) => TestType::sqlite(),
        TestType::InMemorySqlite => TestType::in_memory(),
        TestType::NoDb => TestType::no_db(),
        #[cfg(feature = "postgres")]
        TestType::Postgres { url, .. } => TestType::Postgres {
            url: url.clone(),
            surfnet_id: crate::storage::tests::random_surfnet_id(),
        },
    };

    let (datasource_surfnet_url, datasource_svm_locker) =
        start_surfnet(vec![], None, test_type).expect("Failed to start datasource surfnet");

    datasource_svm_locker
        .with_svm_writer(|svm_writer| {
            svm_writer
                .set_account(
                    &account,
                    Account {
                        lamports: LAMPORTS_PER_SOL,
                        data: vec![1, 2, 3],
                        owner: system_program::id(),
                        executable: false,
                        rent_epoch: 0,
                    },
                )
                .unwrap();
            Ok::<(), SurfpoolError>(())
        })
        .expect("Failed to seed datasource accounts");

    let (surfnet_url, surfnet_svm_locker) =
        start_surfnet(vec![], Some(datasource_surfnet_url), another_test_type)
            .expect("Failed to start surfnet");
    let rpc_client = RpcClient::new(surfnet_url);

    // Mark the account offline
    let _: serde_json::Value = rpc_client
        .send(
            solana_client::rpc_request::RpcRequest::Custom {
                method: "surfnet_offlineAccount",
            },
            serde_json::json!([account.to_string()]),
        )
        .await
        .expect("Failed to set account offline");

    assert!(
        surfnet_svm_locker.is_account_offline(&account),
        "Account should be offline"
    );

    // Verify it's blocked
    let result = rpc_client.get_account(&account).await;
    assert!(
        result.is_err(),
        "Offline account should not be fetched from remote"
    );

    // Reset the account — this should bring it back online
    let _: serde_json::Value = rpc_client
        .send(
            solana_client::rpc_request::RpcRequest::Custom {
                method: "surfnet_resetAccount",
            },
            serde_json::json!([account.to_string()]),
        )
        .await
        .expect("Failed to reset account");

    assert!(
        !surfnet_svm_locker.is_account_offline(&account),
        "Account should be online after reset"
    );

    // Verify it can be fetched from remote again
    let result = rpc_client.get_account(&account).await;
    assert!(
        result.is_ok(),
        "Account should be fetchable from remote after reset"
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[tokio::test(flavor = "multi_thread")]
async fn test_reset_network_clears_offline_accounts(test_type: TestType) {
    let account = Pubkey::new_unique();
    let another_test_type = match &test_type {
        TestType::OnDiskSqlite(_) => TestType::sqlite(),
        TestType::InMemorySqlite => TestType::in_memory(),
        TestType::NoDb => TestType::no_db(),
        #[cfg(feature = "postgres")]
        TestType::Postgres { url, .. } => TestType::Postgres {
            url: url.clone(),
            surfnet_id: crate::storage::tests::random_surfnet_id(),
        },
    };

    let (datasource_surfnet_url, datasource_svm_locker) =
        start_surfnet(vec![], None, test_type).expect("Failed to start datasource surfnet");

    datasource_svm_locker
        .with_svm_writer(|svm_writer| {
            svm_writer
                .set_account(
                    &account,
                    Account {
                        lamports: LAMPORTS_PER_SOL,
                        data: vec![1, 2, 3],
                        owner: system_program::id(),
                        executable: false,
                        rent_epoch: 0,
                    },
                )
                .unwrap();
            Ok::<(), SurfpoolError>(())
        })
        .expect("Failed to seed datasource accounts");

    let (surfnet_url, surfnet_svm_locker) =
        start_surfnet(vec![], Some(datasource_surfnet_url), another_test_type)
            .expect("Failed to start surfnet");
    let rpc_client = RpcClient::new(surfnet_url);

    // Mark the account offline
    let _: serde_json::Value = rpc_client
        .send(
            solana_client::rpc_request::RpcRequest::Custom {
                method: "surfnet_offlineAccount",
            },
            serde_json::json!([account.to_string()]),
        )
        .await
        .expect("Failed to set account offline");

    assert!(
        surfnet_svm_locker.is_account_offline(&account),
        "Account should be offline"
    );

    // Reset the network
    let _: serde_json::Value = rpc_client
        .send(
            solana_client::rpc_request::RpcRequest::Custom {
                method: "surfnet_resetNetwork",
            },
            serde_json::json!([]),
        )
        .await
        .expect("Failed to reset network");

    assert!(
        !surfnet_svm_locker.is_account_offline(&account),
        "Account should be online after network reset"
    );

    // Verify the account can be fetched from remote again
    let result = rpc_client.get_account(&account).await;
    assert!(
        result.is_ok(),
        "Account should be fetchable from remote after network reset"
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[tokio::test(flavor = "multi_thread")]
async fn test_remote_get_multiple_accounts_only_program_accounts(test_type: TestType) {
    let program_pubkey = Pubkey::new_unique();
    let another_test_type = match &test_type {
        TestType::OnDiskSqlite(_) => TestType::sqlite(),
        TestType::InMemorySqlite => TestType::in_memory(),
        TestType::NoDb => TestType::no_db(),
        #[cfg(feature = "postgres")]
        TestType::Postgres { url, .. } => TestType::Postgres {
            url: url.clone(),
            surfnet_id: crate::storage::tests::random_surfnet_id(),
        },
    };

    // Start datasource surfnet A (offline, no remote)
    let (datasource_url, datasource_svm_locker) =
        start_surfnet(vec![], None, test_type).expect("Failed to start datasource surfnet");
    println!("Datasource surfnet started at {}", datasource_url);

    // Insert a proper upgradeable program into A.
    // Using write_program ensures the program and program-data accounts are
    // stored with the correct BPF loader owner and serialized state, which is
    // required by LiteSVM's account validation.
    datasource_svm_locker
        .write_program(program_pubkey, None, 0, &[1, 2, 3], &None)
        .await
        .expect("Failed to write program account");

    // Start surfnet B pointing to A as remote
    let (surfnet_url, _surfnet_svm_locker) =
        start_surfnet(vec![], Some(datasource_url), another_test_type)
            .expect("Failed to start surfnet");
    println!("Surfnet B started at {}", surfnet_url);

    let rpc_client = RpcClient::new(surfnet_url);

    // Fetch the executable account via get_multiple_accounts.
    let accounts = rpc_client
        .get_multiple_accounts(&[program_pubkey])
        .await
        .expect("Failed to get multiple accounts");

    let account = accounts[0]
        .as_ref()
        .expect("Program account should be found (not None)");
    assert!(account.executable, "Account should be executable");
    println!("Program account successfully fetched via get_multiple_accounts");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[tokio::test(flavor = "multi_thread")]
async fn test_remote_get_multiple_accounts_ordering(test_type: TestType) {
    let program_pubkey = Pubkey::new_unique();
    let plain_pubkey = Pubkey::new_unique();
    let another_test_type = match &test_type {
        TestType::OnDiskSqlite(_) => TestType::sqlite(),
        TestType::InMemorySqlite => TestType::in_memory(),
        TestType::NoDb => TestType::no_db(),
        #[cfg(feature = "postgres")]
        TestType::Postgres { url, .. } => TestType::Postgres {
            url: url.clone(),
            surfnet_id: crate::storage::tests::random_surfnet_id(),
        },
    };

    // Start datasource surfnet A (offline, no remote)
    let (datasource_url, datasource_svm_locker) =
        start_surfnet(vec![], None, test_type).expect("Failed to start datasource surfnet");

    // Insert a program account on A
    datasource_svm_locker
        .write_program(program_pubkey, None, 0, &[1, 2, 3], &None)
        .await
        .expect("Failed to write program account");

    // Insert a plain SOL account on A
    datasource_svm_locker
        .airdrop(&plain_pubkey, LAMPORTS_PER_SOL)
        .expect("Failed to airdrop to plain account")
        .unwrap();

    // Start surfnet B pointing to A as remote
    let (surfnet_url, _) = start_surfnet(vec![], Some(datasource_url), another_test_type)
        .expect("Failed to start surfnet B");

    let rpc_client = RpcClient::new(surfnet_url);

    // Fetch with program FIRST, plain SECOND — this is the ordering that the old code broke
    let accounts = rpc_client
        .get_multiple_accounts(&[program_pubkey, plain_pubkey])
        .await
        .expect("Failed to get multiple accounts");

    assert_eq!(accounts.len(), 2);

    // Index 0 must be the program account (executable)
    let prog_account = accounts[0]
        .as_ref()
        .expect("Program account should be found at index 0");
    assert!(
        prog_account.executable,
        "accounts[0] should be executable (program)"
    );

    // Index 1 must be the plain account (not executable, has lamports)
    let plain_account = accounts[1]
        .as_ref()
        .expect("Plain account should be found at index 1");
    assert!(
        !plain_account.executable,
        "accounts[1] should not be executable (plain)"
    );
    assert_eq!(
        plain_account.lamports, LAMPORTS_PER_SOL,
        "accounts[1] should have airdrop lamports"
    );
}

// websocket rpc methods tests

#[test_case(SignatureSubscriptionType::processed() ; "processed commitment")]
#[test_case(SignatureSubscriptionType::received() ; "received commitment")]
#[test_case(SignatureSubscriptionType::confirmed() ; "confirmed commitment")]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_signature_subscribe(subscription_type: SignatureSubscriptionType) {
    use crossbeam_channel::unbounded;
    use solana_system_interface::instruction as system_instruction;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();

    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // create a test transaction
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    let lamports_to_send = 1_000_000;
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &recipient, lamports_to_send);
    let tx = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );
    let signature = tx.signatures[0];

    // subscribe with processed commitment
    let notification_rx =
        svm_locker.subscribe_for_signature_updates(&signature, subscription_type.clone());

    // process the transaction
    let (status_tx, _status_rx) = unbounded();
    let result = svm_locker
        .process_transaction(&None, VersionedTransaction::from(tx), status_tx, true, true)
        .await;
    assert!(
        result.is_ok(),
        "Transaction should be processed successfully"
    );

    match subscription_type {
        SignatureSubscriptionType::Commitment(CommitmentLevel::Confirmed) => {
            // confirm the block to trigger confirmed notification
            svm_locker.confirm_current_block(&None).await.unwrap();
        }
        _ => {}
    }

    // wait for the notification
    let notification = notification_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        notification.is_ok(),
        "Should receive {} notification",
        subscription_type
    );

    let (slot, error_opt) = notification.unwrap();
    assert!(
        error_opt.is_none(),
        "Transaction should succeed without error"
    );
    println!(
        "✓ Received {} signature notification at slot {}",
        subscription_type, slot
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_signature_subscribe_failed_transaction(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_system_interface::instruction as system_instruction;

    use crate::surfnet::SignatureSubscriptionType;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // create a test transaction that will fail (insufficient funds)
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let transfer_ix =
        system_instruction::transfer(&payer.pubkey(), &recipient, LAMPORTS_PER_SOL * 2); // Try to send more than we have
    let tx = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );
    let signature = tx.signatures[0];

    // subscribe with processed commitment
    let subscription_type = SignatureSubscriptionType::processed();
    let notification_rx = svm_locker.subscribe_for_signature_updates(&signature, subscription_type);

    // process the transaction (should fail)
    let (status_tx, _status_rx) = unbounded();
    let _ = svm_locker
        .process_transaction(
            &None,
            VersionedTransaction::from(tx),
            status_tx,
            false,
            false,
        )
        .await;

    // wait for the notification with error
    let notification = notification_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        notification.is_ok(),
        "Should receive notification for failed transaction"
    );

    let (slot, error_opt) = notification.unwrap();
    assert!(error_opt.is_some(), "Failed transaction should have error");
    println!(
        "✓ Received signature notification for failed transaction at slot {} with error: {:?}",
        slot, error_opt
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_signature_subscribe_multiple_subscribers(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_system_interface::instruction as system_instruction;

    use crate::surfnet::SignatureSubscriptionType;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // create a test transaction
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &recipient, 1_000_000);
    let tx = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );
    let signature = tx.signatures[0];

    // create multiple subscriptions to the same signature
    let notification_rx1 = svm_locker
        .subscribe_for_signature_updates(&signature, SignatureSubscriptionType::processed());
    let notification_rx2 = svm_locker
        .subscribe_for_signature_updates(&signature, SignatureSubscriptionType::processed());
    let notification_rx3 = svm_locker
        .subscribe_for_signature_updates(&signature, SignatureSubscriptionType::confirmed());

    // process the transaction
    let (status_tx, _status_rx) = unbounded();
    svm_locker
        .process_transaction(
            &None,
            VersionedTransaction::from(tx),
            status_tx,
            false,
            false,
        )
        .await
        .unwrap();

    // all processed subscriptions should receive notification
    assert!(
        notification_rx1
            .recv_timeout(Duration::from_secs(5))
            .is_ok(),
        "Subscriber 1 should receive notification"
    );
    assert!(
        notification_rx2
            .recv_timeout(Duration::from_secs(5))
            .is_ok(),
        "Subscriber 2 should receive notification"
    );

    // confirm the block for confirmed subscription
    svm_locker.confirm_current_block(&None).await.unwrap();
    assert!(
        notification_rx3
            .recv_timeout(Duration::from_secs(5))
            .is_ok(),
        "Confirmed subscriber should receive notification"
    );

    println!("✓ Multiple subscribers all received notifications correctly");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_signature_subscribe_before_transaction_exists(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_system_interface::instruction as system_instruction;

    use crate::surfnet::SignatureSubscriptionType;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &recipient, 1_000_000);
    let tx = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );
    let signature = tx.signatures[0];

    // subscribe before the transaction exists
    let subscription_type = SignatureSubscriptionType::processed();
    let notification_rx = svm_locker.subscribe_for_signature_updates(&signature, subscription_type);

    // small delay to ensure the subscription is registered
    tokio::time::sleep(Duration::from_millis(100)).await;

    // now process the transaction
    let (status_tx, _status_rx) = unbounded();
    svm_locker
        .process_transaction(
            &None,
            VersionedTransaction::from(tx),
            status_tx,
            false,
            false,
        )
        .await
        .unwrap();

    // should still receive notification
    let notification = notification_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        notification.is_ok(),
        "Should receive notification even when subscribed before transaction"
    );

    let (slot, error_opt) = notification.unwrap();
    assert!(error_opt.is_none(), "Transaction should succeed");
    println!(
        "✓ Subscription before transaction works correctly at slot {}",
        slot
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_account_subscribe_balance_change(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_system_interface::instruction as system_instruction;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // create and fund a new account
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    // subscribe to payer account updates
    let account_rx =
        svm_locker.subscribe_for_account_updates(&payer.pubkey(), Some(UiAccountEncoding::Base58));

    // make a transaction to change the account balance
    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &recipient, 1_000_000);
    let tx = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );

    let (status_tx, _status_rx) = unbounded();
    svm_locker
        .process_transaction(
            &None,
            VersionedTransaction::from(tx),
            status_tx,
            false,
            false,
        )
        .await
        .unwrap();

    let account_update = account_rx.recv_timeout(Duration::from_secs(5));
    assert!(account_update.is_ok(), "Should receive account update");

    let updated_account = account_update.unwrap();
    assert_eq!(
        updated_account.lamports,
        LAMPORTS_PER_SOL - 1_000_000 - 5000, // original - transfer amount - fees
        "Account balance should be updated"
    );
    println!(
        "✓ Received account update notification with new balance {}",
        updated_account.lamports
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_account_subscribe_multiple_changes(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_account_decoder::UiAccountEncoding;
    use solana_system_interface::instruction as system_instruction;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // create and fund a new account
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    svm_locker
        .airdrop(&payer.pubkey(), 10 * LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    // subscribe to payer account updates
    let account_rx =
        svm_locker.subscribe_for_account_updates(&payer.pubkey(), Some(UiAccountEncoding::Base58));

    // make multiple transactions
    for i in 0..3 {
        let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());

        let transfer_ix = system_instruction::transfer(&payer.pubkey(), &recipient, 1_000_000);
        let tx = Transaction::new_signed_with_payer(
            &[transfer_ix],
            Some(&payer.pubkey()),
            &[&payer],
            recent_blockhash,
        );

        let (status_tx, _status_rx) = unbounded();
        svm_locker
            .process_transaction(
                &None,
                VersionedTransaction::from(tx),
                status_tx,
                false,
                false,
            )
            .await
            .unwrap();

        let account_update = account_rx.recv_timeout(Duration::from_secs(5));
        assert!(
            account_update.is_ok(),
            "Should receive account update for transaction {}",
            i + 1
        );
        println!(
            "✓ Received account update notification for transaction {}",
            i + 1
        );

        // confirm the block to get fresh blockhash for next transaction
        if i < 2 {
            svm_locker.confirm_current_block(&None).await.unwrap();
        }
    }
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_account_subscribe_multiple_subscribers(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_account_decoder::UiAccountEncoding;
    use solana_system_interface::instruction as system_instruction;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let payer = Keypair::new();
    let sender = Keypair::new();
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();
    svm_locker
        .airdrop(&sender.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    // create multiple subscriptions to the same account
    let account_rx1 =
        svm_locker.subscribe_for_account_updates(&payer.pubkey(), Some(UiAccountEncoding::Base64));
    let account_rx2 =
        svm_locker.subscribe_for_account_updates(&payer.pubkey(), Some(UiAccountEncoding::Base58));
    let account_rx3 = svm_locker.subscribe_for_account_updates(&payer.pubkey(), None);

    // trigger a change with a transfer (not airdrop)
    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let transfer_ix = system_instruction::transfer(&sender.pubkey(), &payer.pubkey(), 1_000_000);
    let tx = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&sender.pubkey()),
        &[&sender],
        recent_blockhash,
    );

    let (status_tx, _status_rx) = unbounded();
    svm_locker
        .process_transaction(
            &None,
            VersionedTransaction::from(tx),
            status_tx,
            false,
            false,
        )
        .await
        .unwrap();

    // all subscribers should receive notifications
    assert!(
        account_rx1.recv_timeout(Duration::from_secs(5)).is_ok(),
        "Subscriber 1 should receive notification"
    );
    assert!(
        account_rx2.recv_timeout(Duration::from_secs(5)).is_ok(),
        "Subscriber 2 should receive notification"
    );
    assert!(
        account_rx3.recv_timeout(Duration::from_secs(5)).is_ok(),
        "Subscriber 3 should receive notification"
    );

    println!("✓ All 3 subscribers received notifications for account change");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_account_subscribe_new_account_creation(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_account_decoder::UiAccountEncoding;
    use solana_system_interface::instruction as system_instruction;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let payer = Keypair::new();
    let new_account = Pubkey::new_unique();
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    // subscribe to an account that doesn't exist yet
    let account_rx =
        svm_locker.subscribe_for_account_updates(&new_account, Some(UiAccountEncoding::Base64));

    // create the account with a transfer
    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &new_account, 1_000_000);
    let tx = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );

    let (status_tx, _status_rx) = unbounded();
    svm_locker
        .process_transaction(
            &None,
            VersionedTransaction::from(tx),
            status_tx,
            false,
            false,
        )
        .await
        .unwrap();

    // should receive notification when account is created
    let account_update = account_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        account_update.is_ok(),
        "Should receive notification when account is created"
    );

    let created_account = account_update.unwrap();
    assert_eq!(
        created_account.lamports, 1_000_000,
        "New account should have correct balance"
    );
    println!(
        "✓ Received account update notification for new account creation with balance {}",
        created_account.lamports
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_account_subscribe_account_closure(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_account_decoder::UiAccountEncoding;
    use solana_system_interface::instruction as system_instruction;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let account_to_close = Keypair::new();
    let recipient = Pubkey::new_unique();

    // give the account some funds
    svm_locker
        .airdrop(&account_to_close.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    // subscribe to the account
    let account_rx = svm_locker
        .subscribe_for_account_updates(&account_to_close.pubkey(), Some(UiAccountEncoding::Base64));

    // close the account by sending all funds minus fee
    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let close_ix = system_instruction::transfer(
        &account_to_close.pubkey(),
        &recipient,
        LAMPORTS_PER_SOL - 5_000,
    );
    let tx = Transaction::new_signed_with_payer(
        &[close_ix],
        Some(&account_to_close.pubkey()),
        &[&account_to_close],
        recent_blockhash,
    );

    let (status_tx, _status_rx) = unbounded();
    svm_locker
        .process_transaction(
            &None,
            VersionedTransaction::from(tx),
            status_tx,
            true,
            false,
        )
        .await
        .unwrap();

    // should receive notification for the closure
    let account_update = account_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        account_update.is_ok(),
        "Should receive notification when account is closed"
    );

    println!("✓ Received notification for account closure");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_slot_subscribe_basic(test_type: TestType) {
    use surfpool_types::types::BlockProductionMode;

    let (svm_locker, _simnet_commands_tx, _simnet_events_rx) =
        boot_simnet(BlockProductionMode::Clock, Some(100), test_type);

    // subscribe to slot updates
    let slot_rx = svm_locker.subscribe_for_slot_updates();

    // wait for the first slot update
    let slot_info_1 = slot_rx.recv_timeout(Duration::from_secs(2));
    assert!(slot_info_1.is_ok(), "Should receive slot update");

    let first_slot = slot_info_1.unwrap();
    println!("✓ Received first slot update: {}", first_slot.slot);

    // wait for the second slot update
    let slot_info_2 = slot_rx.recv_timeout(Duration::from_secs(2));
    assert!(slot_info_2.is_ok(), "Should receive slot update");

    let second_slot = slot_info_2.unwrap();
    println!("✓ Received second slot update: {}", second_slot.slot);

    assert!(
        second_slot.slot > first_slot.slot,
        "Second slot should be greater than first slot"
    );
    println!("✓ Slot updates are progressing correctly");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_slot_subscribe_manual_advancement(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // subscribe to slot updates
    let slot_rx = svm_locker.subscribe_for_slot_updates();

    let initial_slot = svm_locker.get_latest_absolute_slot();

    // manually advance slot by confirming a block
    svm_locker.confirm_current_block(&None).await.unwrap();

    // should receive slot update notification
    let slot_update = slot_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        slot_update.is_ok(),
        "Should receive slot update after block confirmation"
    );

    let slot_info = slot_update.unwrap();
    assert!(
        slot_info.slot > initial_slot,
        "Updated slot should be greater than initial slot"
    );
    println!(
        "✓ Received slot notification after manual block confirmation: slot {} -> {}",
        initial_slot, slot_info.slot
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_slot_subscribe_multiple_subscribers(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // create multiple subscriptions
    let slot_rx1 = svm_locker.subscribe_for_slot_updates();
    let slot_rx2 = svm_locker.subscribe_for_slot_updates();
    let slot_rx3 = svm_locker.subscribe_for_slot_updates();

    // advance slot
    svm_locker.confirm_current_block(&None).await.unwrap();

    // all subscribers should receive notification
    assert!(
        slot_rx1.recv_timeout(Duration::from_secs(5)).is_ok(),
        "Subscriber 1 should receive slot update"
    );
    assert!(
        slot_rx2.recv_timeout(Duration::from_secs(5)).is_ok(),
        "Subscriber 2 should receive slot update"
    );
    assert!(
        slot_rx3.recv_timeout(Duration::from_secs(5)).is_ok(),
        "Subscriber 3 should receive slot update"
    );

    println!("✓ All 3 subscribers received slot update notifications");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_slot_subscribe_multiple_slot_changes(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let slot_rx = svm_locker.subscribe_for_slot_updates();

    // advance slot multiple times
    for i in 0..3 {
        svm_locker.confirm_current_block(&None).await.unwrap();

        let slot_update = slot_rx.recv_timeout(Duration::from_secs(5));
        assert!(
            slot_update.is_ok(),
            "Should receive slot update for advancement {}",
            i + 1
        );

        let slot_info = slot_update.unwrap();
        println!(
            "✓ Received slot notification #{}: slot {}",
            i + 1,
            slot_info.slot
        );
    }
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_logs_subscribe_all_transactions(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_client::rpc_config::RpcTransactionLogsFilter;
    use solana_system_interface::instruction as system_instruction;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    // subscribe to all transaction logs
    let logs_rx = svm_locker
        .subscribe_for_logs_updates(&CommitmentLevel::Processed, &RpcTransactionLogsFilter::All);

    // create and process a test transaction
    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &recipient, 1_000_000);
    let tx = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );
    let signature = tx.signatures[0];

    let (status_tx, _status_rx) = unbounded();
    svm_locker
        .process_transaction(
            &None,
            VersionedTransaction::from(tx),
            status_tx,
            false,
            false,
        )
        .await
        .unwrap();

    // wait for the logs update
    let logs_update = logs_rx.recv_timeout(Duration::from_secs(5));
    assert!(logs_update.is_ok(), "Should receive logs update");

    let (_slot, logs_response) = logs_update.unwrap();
    assert_eq!(
        logs_response.signature,
        signature.to_string(),
        "Signature should match"
    );
    assert!(
        logs_response.err.is_none(),
        "Transaction should succeed without error, got error: {}",
        logs_response.err.unwrap()
    );
    assert!(
        !logs_response.logs.is_empty(),
        "Should have at least one log message"
    );
    println!("✓ Received logs update for transaction: {}", signature);
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_logs_subscribe_mentions_account(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_client::rpc_config::RpcTransactionLogsFilter;
    use solana_commitment_config::CommitmentLevel;
    use solana_system_interface::instruction as system_instruction;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    // subscribe to logs mentioning the system program
    let system_program = solana_sdk_ids::system_program::id();
    let logs_rx = svm_locker.subscribe_for_logs_updates(
        &CommitmentLevel::Processed,
        &RpcTransactionLogsFilter::Mentions(vec![system_program.to_string()]),
    );
    // also subscribe to logs mentioning the token program
    let token_program = spl_token_interface::id();
    let logs_rx_2 = svm_locker.subscribe_for_logs_updates(
        &CommitmentLevel::Processed,
        &RpcTransactionLogsFilter::Mentions(vec![token_program.to_string()]),
    );

    // create transaction that uses system program
    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &recipient, 1_000_000);
    let tx = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );

    let (status_tx, _status_rx) = unbounded();
    svm_locker
        .process_transaction(
            &None,
            VersionedTransaction::from(tx),
            status_tx,
            false,
            false,
        )
        .await
        .unwrap();

    // should receive logs since transaction mentions system program
    let logs_notification = logs_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        logs_notification.is_ok(),
        "Should receive logs for transaction mentioning system program"
    );

    let (_slot, logs_response) = logs_notification.unwrap();
    assert!(
        !logs_response.logs.is_empty(),
        "Should have logs from system program"
    );
    println!("✓ Received logs notification for transaction mentioning system program");

    // should NOT receive logs for token program subscription
    let logs_notification_2 = logs_rx_2.recv_timeout(Duration::from_secs(3));
    assert!(
        logs_notification_2.is_err(),
        "Should NOT receive logs for transaction not mentioning token program"
    );
    println!("✓ Did not receive logs notification for transaction not mentioning token program");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_logs_subscribe_confirmed_commitment(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_client::rpc_config::RpcTransactionLogsFilter;
    use solana_commitment_config::CommitmentLevel;
    use solana_system_interface::instruction as system_instruction;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // subscribe to confirmed logs
    let logs_rx = svm_locker
        .subscribe_for_logs_updates(&CommitmentLevel::Confirmed, &RpcTransactionLogsFilter::All);

    // create and process a transaction
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &recipient, 1_000_000);
    let tx = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );

    let (status_tx, _status_rx) = unbounded();
    svm_locker
        .process_transaction(
            &None,
            VersionedTransaction::from(tx),
            status_tx,
            false,
            false,
        )
        .await
        .unwrap();

    // confirm the block to trigger confirmed logs
    svm_locker.confirm_current_block(&None).await.unwrap();

    // wait for confirmed logs notification
    let logs_notification = logs_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        logs_notification.is_ok(),
        "Should receive confirmed logs notification"
    );

    let (slot, logs_response) = logs_notification.unwrap();
    assert!(
        !logs_response.logs.is_empty(),
        "Confirmed logs should not be empty"
    );
    println!("✓ Received confirmed logs notification at slot {}", slot);
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_logs_subscribe_finalized_commitment(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_client::rpc_config::RpcTransactionLogsFilter;
    use solana_commitment_config::CommitmentLevel;
    use solana_system_interface::instruction as system_instruction;

    use crate::surfnet::FINALIZATION_SLOT_THRESHOLD;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // subscribe to finalized logs
    let logs_rx = svm_locker
        .subscribe_for_logs_updates(&CommitmentLevel::Finalized, &RpcTransactionLogsFilter::All);

    // create and process a transaction
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &recipient, 1_000_000);
    let tx = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );

    let (status_tx, _status_rx) = unbounded();
    svm_locker
        .process_transaction(
            &None,
            VersionedTransaction::from(tx),
            status_tx,
            false,
            false,
        )
        .await
        .unwrap();

    // confirm and finalize the block
    svm_locker.confirm_current_block(&None).await.unwrap();

    // advance enough slots to trigger finalization
    for _ in 0..FINALIZATION_SLOT_THRESHOLD {
        svm_locker.confirm_current_block(&None).await.unwrap();
    }

    // wait for finalized logs notification
    let logs_notification = logs_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        logs_notification.is_ok(),
        "Should receive finalized logs notification"
    );

    let (slot, logs_response) = logs_notification.unwrap();
    assert!(
        !logs_response.logs.is_empty(),
        "Finalized logs should not be empty"
    );
    println!("✓ Received finalized logs notification at slot {}", slot);
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_logs_subscribe_failed_transaction(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_client::rpc_config::RpcTransactionLogsFilter;
    use solana_commitment_config::CommitmentLevel;
    use solana_system_interface::instruction as system_instruction;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // create test accounts
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    // subscribe to all logs
    let logs_rx = svm_locker
        .subscribe_for_logs_updates(&CommitmentLevel::Processed, &RpcTransactionLogsFilter::All);

    // create and process a transaction that will fail (insufficient funds)
    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let transfer_ix =
        system_instruction::transfer(&payer.pubkey(), &recipient, LAMPORTS_PER_SOL * 2);
    let tx = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );

    let (status_tx, _status_rx) = unbounded();
    let _ = svm_locker
        .process_transaction(
            &None,
            VersionedTransaction::from(tx),
            status_tx,
            false,
            false,
        )
        .await;

    // wait for logs notification with error
    let logs_notification = logs_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        logs_notification.is_ok(),
        "Should receive logs for failed transaction"
    );

    let (_slot, logs_response) = logs_notification.unwrap();
    assert!(
        logs_response.err.is_some(),
        "Failed transaction should have error in logs"
    );
    assert!(
        !logs_response.logs.is_empty(),
        "Failed transaction should still have logs"
    );
    println!(
        "✓ Received logs notification for failed transaction with error: {:?}",
        logs_response.err
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_logs_subscribe_multiple_subscribers(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_client::rpc_config::RpcTransactionLogsFilter;
    use solana_commitment_config::CommitmentLevel;
    use solana_system_interface::instruction as system_instruction;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // create multiple subscriptions with different commitment levels
    let logs_rx1 = svm_locker
        .subscribe_for_logs_updates(&CommitmentLevel::Processed, &RpcTransactionLogsFilter::All);
    let logs_rx2 = svm_locker
        .subscribe_for_logs_updates(&CommitmentLevel::Processed, &RpcTransactionLogsFilter::All);
    let logs_rx3 = svm_locker
        .subscribe_for_logs_updates(&CommitmentLevel::Confirmed, &RpcTransactionLogsFilter::All);

    // create and process a transaction
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &recipient, 1_000_000);
    let tx = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );

    let (status_tx, _status_rx) = unbounded();
    svm_locker
        .process_transaction(
            &None,
            VersionedTransaction::from(tx),
            status_tx,
            false,
            false,
        )
        .await
        .unwrap();

    // all processed subscribers should receive notification
    assert!(
        logs_rx1.recv_timeout(Duration::from_secs(5)).is_ok(),
        "Processed subscriber 1 should receive logs"
    );
    assert!(
        logs_rx2.recv_timeout(Duration::from_secs(5)).is_ok(),
        "Processed subscriber 2 should receive logs"
    );

    // confirm block for confirmed subscriber
    svm_locker.confirm_current_block(&None).await.unwrap();
    assert!(
        logs_rx3.recv_timeout(Duration::from_secs(5)).is_ok(),
        "Confirmed subscriber should receive logs"
    );

    println!("✓ All subscribers received logs notifications at their respective commitment levels");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_logs_subscribe_logs_content(test_type: TestType) {
    use crossbeam_channel::unbounded;
    use solana_client::rpc_config::RpcTransactionLogsFilter;
    use solana_commitment_config::CommitmentLevel;
    use solana_system_interface::instruction as system_instruction;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    // create test accounts
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    // subscribe to all logs
    let logs_rx = svm_locker
        .subscribe_for_logs_updates(&CommitmentLevel::Processed, &RpcTransactionLogsFilter::All);

    // create and process a transaction
    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let transfer_ix = system_instruction::transfer(&payer.pubkey(), &recipient, 1_000_000);
    let tx = Transaction::new_signed_with_payer(
        &[transfer_ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent_blockhash,
    );
    let signature = tx.signatures[0];

    let (status_tx, _status_rx) = unbounded();
    svm_locker
        .process_transaction(
            &None,
            VersionedTransaction::from(tx),
            status_tx,
            false,
            false,
        )
        .await
        .unwrap();

    // receive and validate logs content
    let logs_notification = logs_rx.recv_timeout(Duration::from_secs(5));
    assert!(logs_notification.is_ok(), "Should receive logs");

    let (_slot, logs_response) = logs_notification.unwrap();

    // verify logs response structure
    assert_eq!(logs_response.signature, signature.to_string());
    assert!(logs_response.err.is_none());
    assert!(!logs_response.logs.is_empty());

    // logs should contain program invocation messages
    let has_program_log = logs_response.logs.iter().any(|log| log.contains("Program"));
    assert!(
        has_program_log,
        "Logs should contain program execution messages"
    );

    println!("✓ Logs notification contains valid content:");
    for (i, log) in logs_response.logs.iter().enumerate() {
        println!("  Log {}: {}", i + 1, log);
    }
}

/// Token-2022 lifecycle:
/// create mint → initialize → create ATA → mint → transfer → burn → close account
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_token2022_full_lifecycle(test_type: TestType) {
    use solana_system_interface::instruction as system_instruction;
    use spl_token_2022_interface::instruction::{
        burn, close_account, initialize_mint2, mint_to, transfer_checked,
    };

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let payer = Keypair::new();
    let recipient = Keypair::new();
    let mint = Keypair::new();
    let decimals = 6u8;

    svm_locker
        .airdrop(&payer.pubkey(), 10 * LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();
    svm_locker
        .airdrop(&recipient.pubkey(), 1 * LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());

    let mint_rent =
        svm_locker.with_svm_reader(|svm| svm.inner.minimum_balance_for_rent_exemption(82));

    let create_mint_ix = system_instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        mint_rent,
        82,
        &spl_token_2022_interface::id(),
    );

    let init_mint_ix = initialize_mint2(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        &payer.pubkey(),       // mint authority
        Some(&payer.pubkey()), // freeze authority
        decimals,
    )
    .unwrap();

    let payer_ata =
        spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
            &payer.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let recipient_ata =
        spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
            &recipient.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let create_payer_ata_ix =
        spl_associated_token_account_interface::instruction::create_associated_token_account(
            &payer.pubkey(),
            &payer.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let create_recipient_ata_ix =
        spl_associated_token_account_interface::instruction::create_associated_token_account(
            &payer.pubkey(),
            &recipient.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let mint_amount = 1_000_000u64;
    let mint_to_ix = mint_to(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        &payer_ata,
        &payer.pubkey(),
        &[&payer.pubkey()],
        mint_amount,
    )
    .unwrap();

    let setup_msg = Message::new_with_blockhash(
        &[
            create_mint_ix,
            init_mint_ix,
            create_payer_ata_ix,
            create_recipient_ata_ix,
            mint_to_ix,
        ],
        Some(&payer.pubkey()),
        &recent_blockhash,
    );
    let setup_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(setup_msg), &[&payer, &mint])
            .unwrap();

    let setup_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(setup_tx, false, false));
    assert!(
        setup_result.is_ok(),
        "Setup transaction failed: {:?}",
        setup_result.err()
    );

    let transfer_amount = 500_000u64;
    let transfer_ix = transfer_checked(
        &spl_token_2022_interface::id(),
        &payer_ata,
        &mint.pubkey(),
        &recipient_ata,
        &payer.pubkey(),
        &[&payer.pubkey()],
        transfer_amount,
        decimals,
    )
    .unwrap();

    let transfer_msg =
        Message::new_with_blockhash(&[transfer_ix], Some(&payer.pubkey()), &recent_blockhash);
    let transfer_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(transfer_msg), &[&payer]).unwrap();

    let transfer_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(transfer_tx, false, false));
    assert!(
        transfer_result.is_ok(),
        "Transfer transaction failed: {:?}",
        transfer_result.err()
    );

    let burn_amount = 200_000u64;
    let burn_ix = burn(
        &spl_token_2022_interface::id(),
        &recipient_ata,
        &mint.pubkey(),
        &recipient.pubkey(),
        &[&recipient.pubkey()],
        burn_amount,
    )
    .unwrap();

    let burn_msg =
        Message::new_with_blockhash(&[burn_ix], Some(&recipient.pubkey()), &recent_blockhash);
    let burn_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(burn_msg), &[&recipient]).unwrap();

    let burn_result = svm_locker.with_svm_writer(|svm| svm.send_transaction(burn_tx, false, false));
    assert!(
        burn_result.is_ok(),
        "Burn transaction failed: {:?}",
        burn_result.err()
    );

    let remaining = transfer_amount - burn_amount;
    let transfer_back_ix = transfer_checked(
        &spl_token_2022_interface::id(),
        &recipient_ata,
        &mint.pubkey(),
        &payer_ata,
        &recipient.pubkey(),
        &[&recipient.pubkey()],
        remaining,
        decimals,
    )
    .unwrap();

    let close_ix = close_account(
        &spl_token_2022_interface::id(),
        &recipient_ata,
        &recipient.pubkey(), // destination for rent
        &recipient.pubkey(), // owner
        &[&recipient.pubkey()],
    )
    .unwrap();

    let close_msg = Message::new_with_blockhash(
        &[transfer_back_ix, close_ix],
        Some(&recipient.pubkey()),
        &recent_blockhash,
    );
    let close_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(close_msg), &[&recipient]).unwrap();

    let close_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(close_tx, false, false));
    assert!(
        close_result.is_ok(),
        "Close transaction failed: {:?}",
        close_result.err()
    );

    let recipient_account = svm_locker
        .get_account_local(&recipient_ata)
        .inner
        .map_account()
        .ok();
    assert!(
        recipient_account.is_none() || recipient_account.as_ref().map(|a| a.lamports) == Some(0),
        "Recipient ATA should be closed"
    );

    let expected_balance = mint_amount - burn_amount;
    let _ = expected_balance;
}

/// Token-2022 error cases: transfer/burn > balance and close with balance.
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_token2022_error_cases(test_type: TestType) {
    use solana_system_interface::instruction as system_instruction;
    use spl_token_2022_interface::instruction::{
        burn, close_account, initialize_mint2, mint_to, transfer_checked,
    };

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let payer = Keypair::new();
    let recipient = Keypair::new();
    let mint = Keypair::new();
    let decimals = 6u8;

    svm_locker
        .airdrop(&payer.pubkey(), 10 * LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();
    svm_locker
        .airdrop(&recipient.pubkey(), 1 * LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());

    let mint_rent =
        svm_locker.with_svm_reader(|svm| svm.inner.minimum_balance_for_rent_exemption(82));

    let create_mint_ix = system_instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        mint_rent,
        82,
        &spl_token_2022_interface::id(),
    );

    let init_mint_ix = initialize_mint2(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        &payer.pubkey(),
        Some(&payer.pubkey()),
        decimals,
    )
    .unwrap();

    let payer_ata =
        spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
            &payer.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let recipient_ata =
        spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
            &recipient.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let create_payer_ata_ix =
        spl_associated_token_account_interface::instruction::create_associated_token_account(
            &payer.pubkey(),
            &payer.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let create_recipient_ata_ix =
        spl_associated_token_account_interface::instruction::create_associated_token_account(
            &payer.pubkey(),
            &recipient.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let mint_amount = 1_000_000u64;
    let mint_to_ix = mint_to(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        &payer_ata,
        &payer.pubkey(),
        &[&payer.pubkey()],
        mint_amount,
    )
    .unwrap();

    let setup_msg = Message::new_with_blockhash(
        &[
            create_mint_ix,
            init_mint_ix,
            create_payer_ata_ix,
            create_recipient_ata_ix,
            mint_to_ix,
        ],
        Some(&payer.pubkey()),
        &recent_blockhash,
    );
    let setup_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(setup_msg), &[&payer, &mint])
            .unwrap();

    let setup_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(setup_tx, false, false));
    assert!(
        setup_result.is_ok(),
        "Setup failed: {:?}",
        setup_result.err()
    );

    let excessive_transfer = mint_amount + 1;
    let bad_transfer_ix = transfer_checked(
        &spl_token_2022_interface::id(),
        &payer_ata,
        &mint.pubkey(),
        &recipient_ata,
        &payer.pubkey(),
        &[&payer.pubkey()],
        excessive_transfer,
        decimals,
    )
    .unwrap();

    let bad_transfer_msg =
        Message::new_with_blockhash(&[bad_transfer_ix], Some(&payer.pubkey()), &recent_blockhash);
    let bad_transfer_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(bad_transfer_msg), &[&payer])
            .unwrap();

    let transfer_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(bad_transfer_tx, false, false));
    assert!(
        transfer_result.is_err(),
        "Transfer exceeding balance should fail"
    );

    let excessive_burn = mint_amount + 1;
    let bad_burn_ix = burn(
        &spl_token_2022_interface::id(),
        &payer_ata,
        &mint.pubkey(),
        &payer.pubkey(),
        &[&payer.pubkey()],
        excessive_burn,
    )
    .unwrap();

    let bad_burn_msg =
        Message::new_with_blockhash(&[bad_burn_ix], Some(&payer.pubkey()), &recent_blockhash);
    let bad_burn_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(bad_burn_msg), &[&payer]).unwrap();

    let burn_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(bad_burn_tx, false, false));
    assert!(burn_result.is_err(), "Burn exceeding balance should fail");

    let bad_close_ix = close_account(
        &spl_token_2022_interface::id(),
        &payer_ata,
        &payer.pubkey(),
        &payer.pubkey(),
        &[&payer.pubkey()],
    )
    .unwrap();

    let bad_close_msg =
        Message::new_with_blockhash(&[bad_close_ix], Some(&payer.pubkey()), &recent_blockhash);
    let bad_close_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(bad_close_msg), &[&payer]).unwrap();

    let close_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(bad_close_tx, false, false));
    assert!(
        close_result.is_err(),
        "Close account with balance should fail"
    );
}

/// Token-2022 delegate operations: approve, delegated transfer, revoke.
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_token2022_delegate_operations(test_type: TestType) {
    use solana_system_interface::instruction as system_instruction;
    use spl_token_2022_interface::instruction::{
        approve, initialize_mint2, mint_to, revoke, transfer_checked,
    };

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let owner = Keypair::new();
    let delegate = Keypair::new();
    let recipient = Keypair::new();
    let mint = Keypair::new();
    let decimals = 6u8;

    svm_locker
        .airdrop(&owner.pubkey(), 10 * LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();
    svm_locker
        .airdrop(&delegate.pubkey(), 1 * LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());

    let mint_rent =
        svm_locker.with_svm_reader(|svm| svm.inner.minimum_balance_for_rent_exemption(82));

    let create_mint_ix = system_instruction::create_account(
        &owner.pubkey(),
        &mint.pubkey(),
        mint_rent,
        82,
        &spl_token_2022_interface::id(),
    );

    let init_mint_ix = initialize_mint2(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        &owner.pubkey(),
        None, // no freeze authority needed
        decimals,
    )
    .unwrap();

    let owner_ata =
        spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
            &owner.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let recipient_ata =
        spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
            &recipient.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let create_owner_ata_ix =
        spl_associated_token_account_interface::instruction::create_associated_token_account(
            &owner.pubkey(),
            &owner.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let create_recipient_ata_ix =
        spl_associated_token_account_interface::instruction::create_associated_token_account(
            &owner.pubkey(),
            &recipient.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let mint_amount = 1_000_000u64;
    let mint_to_ix = mint_to(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        &owner_ata,
        &owner.pubkey(),
        &[&owner.pubkey()],
        mint_amount,
    )
    .unwrap();

    let setup_msg = Message::new_with_blockhash(
        &[
            create_mint_ix,
            init_mint_ix,
            create_owner_ata_ix,
            create_recipient_ata_ix,
            mint_to_ix,
        ],
        Some(&owner.pubkey()),
        &recent_blockhash,
    );
    let setup_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(setup_msg), &[&owner, &mint])
            .unwrap();

    let setup_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(setup_tx, false, false));
    assert!(
        setup_result.is_ok(),
        "Setup failed: {:?}",
        setup_result.err()
    );

    let delegate_amount = 500_000u64;
    let approve_ix = approve(
        &spl_token_2022_interface::id(),
        &owner_ata,
        &delegate.pubkey(),
        &owner.pubkey(),
        &[&owner.pubkey()],
        delegate_amount,
    )
    .unwrap();

    let approve_msg =
        Message::new_with_blockhash(&[approve_ix], Some(&owner.pubkey()), &recent_blockhash);
    let approve_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(approve_msg), &[&owner]).unwrap();

    let approve_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(approve_tx, false, false));
    assert!(
        approve_result.is_ok(),
        "Approve failed: {:?}",
        approve_result.err()
    );

    let transfer_amount = 200_000u64;
    let delegated_transfer_ix = transfer_checked(
        &spl_token_2022_interface::id(),
        &owner_ata,
        &mint.pubkey(),
        &recipient_ata,
        &delegate.pubkey(), // delegate is the authority
        &[&delegate.pubkey()],
        transfer_amount,
        decimals,
    )
    .unwrap();

    let delegated_transfer_msg = Message::new_with_blockhash(
        &[delegated_transfer_ix],
        Some(&delegate.pubkey()),
        &recent_blockhash,
    );
    let delegated_transfer_tx = VersionedTransaction::try_new(
        VersionedMessage::Legacy(delegated_transfer_msg),
        &[&delegate],
    )
    .unwrap();

    let delegated_transfer_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(delegated_transfer_tx, false, false));
    assert!(
        delegated_transfer_result.is_ok(),
        "Delegated transfer failed: {:?}",
        delegated_transfer_result.err()
    );

    let revoke_ix = revoke(
        &spl_token_2022_interface::id(),
        &owner_ata,
        &owner.pubkey(),
        &[&owner.pubkey()],
    )
    .unwrap();

    let revoke_msg =
        Message::new_with_blockhash(&[revoke_ix], Some(&owner.pubkey()), &recent_blockhash);
    let revoke_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(revoke_msg), &[&owner]).unwrap();

    let revoke_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(revoke_tx, false, false));
    assert!(
        revoke_result.is_ok(),
        "Revoke failed: {:?}",
        revoke_result.err()
    );

    let post_revoke_transfer_ix = transfer_checked(
        &spl_token_2022_interface::id(),
        &owner_ata,
        &mint.pubkey(),
        &recipient_ata,
        &delegate.pubkey(),
        &[&delegate.pubkey()],
        100_000u64,
        decimals,
    )
    .unwrap();

    let post_revoke_msg = Message::new_with_blockhash(
        &[post_revoke_transfer_ix],
        Some(&delegate.pubkey()),
        &recent_blockhash,
    );
    let post_revoke_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(post_revoke_msg), &[&delegate])
            .unwrap();

    let post_revoke_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(post_revoke_tx, false, false));
    assert!(
        post_revoke_result.is_err(),
        "Transfer after revoke should fail"
    );
}

/// Token-2022 freeze/thaw operations.
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_token2022_freeze_thaw(test_type: TestType) {
    use solana_system_interface::instruction as system_instruction;
    use spl_token_2022_interface::instruction::{
        freeze_account, initialize_mint2, mint_to, thaw_account, transfer_checked,
    };

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let owner = Keypair::new();
    let recipient = Keypair::new();
    let mint = Keypair::new();
    let decimals = 6u8;

    svm_locker
        .airdrop(&owner.pubkey(), 10 * LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());

    let mint_rent =
        svm_locker.with_svm_reader(|svm| svm.inner.minimum_balance_for_rent_exemption(82));

    let create_mint_ix = system_instruction::create_account(
        &owner.pubkey(),
        &mint.pubkey(),
        mint_rent,
        82,
        &spl_token_2022_interface::id(),
    );

    let init_mint_ix = initialize_mint2(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        &owner.pubkey(),
        Some(&owner.pubkey()), // freeze authority = owner
        decimals,
    )
    .unwrap();

    let owner_ata =
        spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
            &owner.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let recipient_ata =
        spl_associated_token_account_interface::address::get_associated_token_address_with_program_id(
            &recipient.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let create_owner_ata_ix =
        spl_associated_token_account_interface::instruction::create_associated_token_account(
            &owner.pubkey(),
            &owner.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let create_recipient_ata_ix =
        spl_associated_token_account_interface::instruction::create_associated_token_account(
            &owner.pubkey(),
            &recipient.pubkey(),
            &mint.pubkey(),
            &spl_token_2022_interface::id(),
        );

    let mint_amount = 1_000_000u64;
    let mint_to_ix = mint_to(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        &owner_ata,
        &owner.pubkey(),
        &[&owner.pubkey()],
        mint_amount,
    )
    .unwrap();

    let setup_msg = Message::new_with_blockhash(
        &[
            create_mint_ix,
            init_mint_ix,
            create_owner_ata_ix,
            create_recipient_ata_ix,
            mint_to_ix,
        ],
        Some(&owner.pubkey()),
        &recent_blockhash,
    );
    let setup_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(setup_msg), &[&owner, &mint])
            .unwrap();

    let setup_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(setup_tx, false, false));
    assert!(
        setup_result.is_ok(),
        "Setup failed: {:?}",
        setup_result.err()
    );

    let freeze_ix = freeze_account(
        &spl_token_2022_interface::id(),
        &owner_ata,
        &mint.pubkey(),
        &owner.pubkey(), // freeze authority
        &[&owner.pubkey()],
    )
    .unwrap();

    let freeze_msg =
        Message::new_with_blockhash(&[freeze_ix], Some(&owner.pubkey()), &recent_blockhash);
    let freeze_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(freeze_msg), &[&owner]).unwrap();

    let freeze_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(freeze_tx, false, false));
    assert!(
        freeze_result.is_ok(),
        "Freeze failed: {:?}",
        freeze_result.err()
    );

    let transfer_amount = 100_000u64;
    let frozen_transfer_ix = transfer_checked(
        &spl_token_2022_interface::id(),
        &owner_ata,
        &mint.pubkey(),
        &recipient_ata,
        &owner.pubkey(),
        &[&owner.pubkey()],
        transfer_amount,
        decimals,
    )
    .unwrap();

    let frozen_transfer_msg = Message::new_with_blockhash(
        &[frozen_transfer_ix],
        Some(&owner.pubkey()),
        &recent_blockhash,
    );
    let frozen_transfer_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(frozen_transfer_msg), &[&owner])
            .unwrap();

    let frozen_transfer_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(frozen_transfer_tx, false, false));
    assert!(
        frozen_transfer_result.is_err(),
        "Transfer from frozen account should fail"
    );

    let fresh_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());

    let thaw_ix = thaw_account(
        &spl_token_2022_interface::id(),
        &owner_ata,
        &mint.pubkey(),
        &owner.pubkey(), // freeze authority
        &[&owner.pubkey()],
    )
    .unwrap();

    let thaw_msg = Message::new_with_blockhash(&[thaw_ix], Some(&owner.pubkey()), &fresh_blockhash);
    let thaw_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(thaw_msg), &[&owner]).unwrap();

    let thaw_result = svm_locker.with_svm_writer(|svm| svm.send_transaction(thaw_tx, false, false));
    assert!(thaw_result.is_ok(), "Thaw failed: {:?}", thaw_result.err());
    println!("✓ Step 3: Account thawed");

    // === Step 4: Verify transfer works after thaw ===
    // Use a different amount to ensure unique transaction signature
    let post_thaw_amount = 50_000u64; // Different from step 2's amount
    let post_thaw_transfer_ix = transfer_checked(
        &spl_token_2022_interface::id(),
        &owner_ata,
        &mint.pubkey(),
        &recipient_ata,
        &owner.pubkey(),
        &[&owner.pubkey()],
        post_thaw_amount,
        decimals,
    )
    .unwrap();

    let post_thaw_msg = Message::new_with_blockhash(
        &[post_thaw_transfer_ix],
        Some(&owner.pubkey()),
        &fresh_blockhash,
    );
    let post_thaw_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(post_thaw_msg), &[&owner]).unwrap();

    let post_thaw_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(post_thaw_tx, false, false));
    assert!(
        post_thaw_result.is_ok(),
        "Transfer after thaw failed: {:?}",
        post_thaw_result.err()
    );
    println!(
        "✓ Step 4: Transfer after thaw succeeded ({} tokens)",
        post_thaw_amount
    );

    println!("✓ All freeze/thaw operations work correctly!");
}

use std::sync::Once;

static INIT_LOGGER: Once = Once::new();

fn setup() {
    INIT_LOGGER.call_once(|| {
        env_logger::builder().is_test(true).try_init().unwrap();
    });
}

#[test]
fn test_nonce_accounts() {
    setup();
    use solana_system_interface::instruction::create_nonce_account;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let payer = Keypair::new();
    let nonce_account = Keypair::new();
    println!("Payer Pubkey: {}", payer.pubkey());
    println!("Nonce Account Pubkey: {}", nonce_account.pubkey());
    println!("Nonce authority: {}", payer.pubkey());

    svm_locker
        .airdrop(&payer.pubkey(), 5 * LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let nonce_rent = svm_locker.with_svm_reader(|svm_reader| {
        svm_reader
            .inner
            .minimum_balance_for_rent_exemption(solana_nonce::state::State::size())
    });
    let create_nonce_ix = create_nonce_account(
        &payer.pubkey(),
        &nonce_account.pubkey(),
        &payer.pubkey(), // Make the fee payer the nonce account authority
        nonce_rent,
    );

    let recent_blockhash = svm_locker.latest_absolute_blockhash();

    let create_nonce_msg =
        Message::new_with_blockhash(&create_nonce_ix, Some(&payer.pubkey()), &recent_blockhash);
    let create_nonce_tx = VersionedTransaction::try_new(
        VersionedMessage::Legacy(create_nonce_msg),
        &[&payer, &nonce_account],
    )
    .unwrap();

    let create_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(create_nonce_tx, false, false));
    assert!(
        create_result.is_ok(),
        "Create nonce account failed: {:?}",
        create_result.err()
    );

    // Fetch and verify nonce account state
    let nonce_account_data = svm_locker
        .get_account_local(&nonce_account.pubkey())
        .inner
        .map_account()
        .expect("Failed to fetch nonce account");

    let state: solana_nonce::versions::Versions = bincode::deserialize(&nonce_account_data.data)
        .expect("Failed to deserialize nonce account state");

    let state = state.state();

    let nonce_hash = match state {
        solana_nonce::state::State::Initialized(nonce_data) => {
            println!(
                "✓ Nonce account initialized with nonce: {:?}",
                nonce_data.durable_nonce
            );
            nonce_data.blockhash()
        }
        _ => panic!("Nonce account is not initialized"),
    };

    let to_pubkey = Pubkey::new_unique();
    // Use the nonce in a transaction
    let transfer_ix =
        solana_system_interface::instruction::transfer(&payer.pubkey(), &to_pubkey, 1_000_000);
    let mut nonce_msg = Message::new_with_nonce(
        vec![transfer_ix],
        Some(&payer.pubkey()),
        &nonce_account.pubkey(),
        &payer.pubkey(),
    );
    nonce_msg.recent_blockhash = nonce_hash;
    let nonce_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(nonce_msg), &[&payer]).unwrap();

    let nonce_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(nonce_tx, false, false));
    assert!(
        nonce_result.is_ok(),
        "Transaction using nonce failed: {:?}",
        nonce_result.err()
    );

    // Verify to_pubkey received the funds
    let to_account_data = svm_locker
        .get_account_local(&to_pubkey)
        .inner
        .map_account()
        .expect("Failed to fetch recipient account");
    assert_eq!(
        to_account_data.lamports, 1_000_000,
        "Recipient account did not receive correct amount"
    );
}

/// Tests that profiling a transaction does not mutate the original SVM state.
/// This verifies the OverlayStorage implementation correctly isolates mutations
/// during profiling from the underlying database.
#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_profile_transaction_does_not_mutate_state(test_type: TestType) {
    let (mut svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let rpc_server = crate::rpc::surfnet_cheatcodes::SurfnetCheatcodesRpc::empty();

    // Setup: Create accounts and fund the payer
    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    let lamports_to_send = 1_000_000;
    let initial_payer_balance = lamports_to_send * 10;

    svm_instance
        .airdrop(&payer.pubkey(), initial_payer_balance)
        .unwrap()
        .unwrap();

    // Create a transfer transaction
    let instruction = transfer(&payer.pubkey(), &recipient, lamports_to_send);
    let latest_blockhash = svm_instance.latest_blockhash();
    let message =
        Message::new_with_blockhash(&[instruction], Some(&payer.pubkey()), &latest_blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message.clone()), &[&payer])
        .unwrap();

    let tx_bytes = bincode::serialize(&tx).unwrap();
    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

    // Record initial state BEFORE profiling
    let initial_transactions_processed = svm_instance.transactions_processed;
    let initial_payer_account = svm_instance.get_account(&payer.pubkey()).unwrap();
    let initial_recipient_account = svm_instance.get_account(&recipient).ok().flatten();

    // Create the locker and runloop context
    let svm_locker = SurfnetSvmLocker::new(svm_instance);
    let (simnet_cmd_tx, _simnet_cmd_rx) = crossbeam_unbounded::<SimnetCommand>();
    let (plugin_commands_tx, _plugin_commands_rx) = crossbeam_channel::unbounded::<PluginCommand>();

    let runloop_context = RunloopContext {
        id: None,
        svm_locker: svm_locker.clone(),
        simnet_commands_tx: simnet_cmd_tx,
        remote_rpc_client: None,
        rpc_config: RpcConfig::default(),
        cheatcode_config: CheatcodeConfig::new(),
        plugin_commands_tx,
    };

    // Profile the transaction multiple times to ensure no state leakage
    for i in 0..3 {
        let response: JsonRpcResult<RpcResponse<UiKeyedProfileResult>> = rpc_server
            .profile_transaction(
                Some(runloop_context.clone()),
                tx_b64.clone(),
                Some(format!("test_isolation_{}", i)),
                None,
            )
            .await;

        assert!(
            response.is_ok(),
            "Profile transaction {} failed: {:?}",
            i,
            response.err()
        );

        let profile_result = response.unwrap().value;
        assert!(
            profile_result.transaction_profile.error_message.is_none(),
            "Profile {} had unexpected error: {:?}",
            i,
            profile_result.transaction_profile.error_message
        );

        // The profile should show the transaction would succeed
        assert!(
            profile_result.transaction_profile.compute_units_consumed > 0,
            "Profile {} should show compute units consumed",
            i
        );
    }

    // Verify state is UNCHANGED after profiling
    let final_transactions_processed = svm_locker.with_svm_reader(|svm| svm.transactions_processed);
    let final_payer_account = svm_locker
        .with_svm_reader(|svm| svm.get_account(&payer.pubkey()))
        .unwrap();
    let final_recipient_account = svm_locker
        .with_svm_reader(|svm| svm.get_account(&recipient))
        .ok()
        .flatten();

    // Transaction count should not have increased from profiling
    assert_eq!(
        initial_transactions_processed, final_transactions_processed,
        "transactions_processed should not change from profiling"
    );

    // Payer balance should be unchanged (transfer was only simulated)
    assert_eq!(
        initial_payer_account.as_ref().map(|a| a.lamports),
        final_payer_account.as_ref().map(|a| a.lamports),
        "Payer balance should not change from profiling"
    );

    // Recipient should still not exist or have the same balance
    assert_eq!(
        initial_recipient_account.as_ref().map(|a| a.lamports),
        final_recipient_account.as_ref().map(|a| a.lamports),
        "Recipient balance should not change from profiling"
    );

    // Now actually execute the transaction to prove the state can still be mutated
    let execution_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(tx.clone(), false, false));

    assert!(
        execution_result.is_ok(),
        "Actual transaction execution should succeed: {:?}",
        execution_result.err()
    );

    // Verify state DID change after actual execution
    let post_execution_transactions = svm_locker.with_svm_reader(|svm| svm.transactions_processed);
    let post_execution_recipient = svm_locker
        .with_svm_reader(|svm| svm.get_account(&recipient))
        .unwrap();

    assert_eq!(
        post_execution_transactions,
        initial_transactions_processed + 1,
        "Transaction count should increase after actual execution"
    );

    assert!(
        post_execution_recipient.is_some(),
        "Recipient should exist after actual execution"
    );

    assert_eq!(
        post_execution_recipient.unwrap().lamports,
        lamports_to_send,
        "Recipient should have received funds after actual execution"
    );
}

/// Tests that instruction-level profiling during transaction execution does not
/// mutate the original SVM state for failed transactions.
/// This creates a transaction with two instructions where:
/// - First instruction: a transfer that would succeed
/// - Second instruction: a transfer that will fail (insufficient funds)
/// The test verifies that even though the first instruction is profiled successfully
/// during execution, its effects are not persisted since the overall transaction fails.
#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_instruction_profiling_does_not_mutate_state(test_type: TestType) {
    let (mut svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();

    // Verify instruction profiling is enabled by default
    assert!(
        svm_instance.instruction_profiling_enabled,
        "Instruction profiling should be enabled by default"
    );

    // Setup: Create accounts
    let payer = Keypair::new();
    let payer_without_funds = Keypair::new();
    let recipient = Pubkey::new_unique();
    let lamports_to_send = LAMPORTS_PER_SOL;

    // Fund only the first payer
    svm_instance
        .airdrop(&payer.pubkey(), lamports_to_send * 3)
        .unwrap()
        .unwrap();

    // Record initial state BEFORE processing
    let initial_payer_balance = svm_instance
        .get_account(&payer.pubkey())
        .unwrap()
        .map(|a| a.lamports)
        .unwrap_or(0);
    let initial_recipient_account = svm_instance.get_account(&recipient).ok().flatten();
    let initial_transactions_processed = svm_instance.transactions_processed;

    // Create a multi-instruction transaction where:
    // - First instruction: valid transfer from payer to recipient (would succeed alone)
    // - Second instruction: invalid transfer from unfunded account (will fail)
    let valid_instruction = transfer(&payer.pubkey(), &recipient, lamports_to_send);
    let invalid_instruction = transfer(&payer_without_funds.pubkey(), &recipient, lamports_to_send);

    let latest_blockhash = svm_instance.latest_blockhash();
    let message = Message::new_with_blockhash(
        &[valid_instruction, invalid_instruction],
        Some(&payer.pubkey()),
        &latest_blockhash,
    );
    let transaction = VersionedTransaction::try_new(
        VersionedMessage::Legacy(message),
        &[&payer, &payer_without_funds],
    )
    .unwrap();
    let signature = transaction.signatures[0];

    // Create the locker and status channel for transaction processing
    let svm_locker = SurfnetSvmLocker::new(svm_instance);
    let (status_tx, _status_rx) = crossbeam_unbounded::<TransactionStatusEvent>();

    // Process the transaction using the actual execution path
    // This will trigger instruction-level profiling since it's enabled by default
    let process_result = svm_locker
        .process_transaction(
            &None, // no remote context
            transaction.clone(),
            status_tx,
            true,  // skip_preflight
            false, // sigverify
        )
        .await;

    // The transaction should fail due to the second instruction
    // But the profile result should still be written
    assert!(
        process_result.is_err() || process_result.is_ok(),
        "process_transaction should complete (success or failure)"
    );

    // Retrieve the profile result using the signature
    let key = UuidOrSignature::Signature(signature);
    let profile_result = svm_locker
        .get_profile_result(key, &RpcProfileResultConfig::default())
        .unwrap()
        .expect("Profile result should exist for executed transaction");

    // Verify the overall transaction failed (due to second instruction)
    assert!(
        profile_result.transaction_profile.error_message.is_some(),
        "Transaction should fail due to second instruction's insufficient funds"
    );

    // Verify instruction profiles were generated
    assert!(
        profile_result.instruction_profiles.is_some(),
        "Instruction profiles should be generated when instruction profiling is enabled"
    );

    let instruction_profiles = profile_result.instruction_profiles.as_ref().unwrap();
    assert_eq!(
        instruction_profiles.len(),
        2,
        "Should have profiles for both instructions"
    );

    // Verify first instruction profile shows SUCCESS (it was profiled independently)
    let first_ix_profile = &instruction_profiles[0];
    assert!(
        first_ix_profile.error_message.is_none(),
        "First instruction profile should succeed: {:?}",
        first_ix_profile.error_message
    );
    assert!(
        first_ix_profile.compute_units_consumed > 0,
        "First instruction should have consumed compute units"
    );

    // Verify second instruction profile shows FAILURE
    let second_ix_profile = &instruction_profiles[1];
    assert!(
        second_ix_profile.error_message.is_some(),
        "Second instruction should fail due to insufficient funds"
    );

    // NOW THE CRITICAL PART: Verify that instruction profiling didn't leak state
    // Even though the first instruction was profiled successfully, its effects
    // should NOT be persisted because:
    // 1. The instruction profiling uses clone_for_profiling() with OverlayStorage
    // 2. The overall transaction failed, so no state changes are committed
    //
    // Note: Failed transactions in Solana still deduct fees from the fee payer.
    // This is expected behavior and not related to instruction profiling.

    let final_payer_balance = svm_locker
        .with_svm_reader(|svm| svm.get_account(&payer.pubkey()))
        .unwrap()
        .map(|a| a.lamports)
        .unwrap_or(0);
    let final_recipient_account = svm_locker
        .with_svm_reader(|svm| svm.get_account(&recipient))
        .ok()
        .flatten();
    let final_transactions_processed = svm_locker.with_svm_reader(|svm| svm.transactions_processed);

    // THE KEY ASSERTION: Recipient should NOT have received funds
    // This proves that the first instruction's transfer (which was profiled successfully)
    // was NOT committed to the actual state. The instruction profiling used
    // clone_for_profiling() so its mutations were isolated.
    assert_eq!(
        initial_recipient_account.as_ref().map(|a| a.lamports),
        final_recipient_account.as_ref().map(|a| a.lamports),
        "Recipient should not have received funds - instruction profiling must not leak state"
    );

    // Payer balance should only decrease by the transaction fee (not by the transfer amount)
    // Failed transactions still pay fees in Solana, but the transfer should not have occurred
    let balance_decrease = initial_payer_balance.saturating_sub(final_payer_balance);
    assert!(
        balance_decrease < lamports_to_send,
        "Payer should only lose transaction fee, not the transfer amount. Lost: {} lamports",
        balance_decrease
    );

    // Transaction count increments even for failed transactions (they were still processed)
    // This is expected behavior - we're verifying instruction profiling isolation, not tx count
    assert_eq!(
        final_transactions_processed,
        initial_transactions_processed + 1,
        "Transaction count should increment after processing (even for failed tx)"
    );
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_program_subscribe_basic(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let program_id = Pubkey::new_unique();
    let account_pubkey = Pubkey::new_unique();

    // Subscribe to program updates
    let program_rx = svm_locker.subscribe_for_program_updates(
        &program_id,
        Some(UiAccountEncoding::Base64),
        None,
    );

    // Set an account owned by the program
    let account = Account {
        lamports: 1_000_000,
        data: vec![1, 2, 3, 4],
        owner: program_id,
        executable: false,
        rent_epoch: 0,
    };
    svm_locker.with_svm_writer(|svm| {
        svm.set_account(&account_pubkey, account).unwrap();
    });

    // Should receive a notification
    let update = program_rx.recv_timeout(Duration::from_secs(5));
    assert!(update.is_ok(), "Should receive program account update");

    let keyed_account = update.unwrap();
    assert_eq!(
        keyed_account.pubkey,
        account_pubkey.to_string(),
        "Notification should contain the correct account pubkey"
    );
    assert_eq!(
        keyed_account.account.lamports, 1_000_000,
        "Notification should contain the correct lamports"
    );
    println!("✓ programSubscribe basic flow works correctly");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_program_subscribe_data_size_filter(test_type: TestType) {
    use solana_client::rpc_filter::RpcFilterType;

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let program_id = Pubkey::new_unique();

    // Subscribe with a dataSize filter for accounts with exactly 8 bytes of data
    let filters = vec![RpcFilterType::DataSize(8)];
    let program_rx = svm_locker.subscribe_for_program_updates(
        &program_id,
        Some(UiAccountEncoding::Base64),
        Some(filters),
    );

    // Set an account with 4 bytes (should NOT trigger notification)
    let small_account_pubkey = Pubkey::new_unique();
    let small_account = Account {
        lamports: 1_000_000,
        data: vec![1, 2, 3, 4],
        owner: program_id,
        executable: false,
        rent_epoch: 0,
    };
    svm_locker.with_svm_writer(|svm| {
        svm.set_account(&small_account_pubkey, small_account)
            .unwrap();
    });

    // Should NOT receive a notification for the 4-byte account
    let no_update = program_rx.recv_timeout(Duration::from_millis(200));
    assert!(
        no_update.is_err(),
        "Should NOT receive notification for account that doesn't match dataSize filter"
    );

    // Set an account with 8 bytes (SHOULD trigger notification)
    let matching_account_pubkey = Pubkey::new_unique();
    let matching_account = Account {
        lamports: 2_000_000,
        data: vec![1, 2, 3, 4, 5, 6, 7, 8],
        owner: program_id,
        executable: false,
        rent_epoch: 0,
    };
    svm_locker.with_svm_writer(|svm| {
        svm.set_account(&matching_account_pubkey, matching_account)
            .unwrap();
    });

    // Should receive a notification for the 8-byte account
    let update = program_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        update.is_ok(),
        "Should receive notification for account matching dataSize filter"
    );

    let keyed_account = update.unwrap();
    assert_eq!(
        keyed_account.pubkey,
        matching_account_pubkey.to_string(),
        "Notification should contain the correct matching account pubkey"
    );
    println!("✓ programSubscribe with dataSize filter works correctly");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_program_subscribe_memcmp_filter(test_type: TestType) {
    use solana_client::rpc_filter::{Memcmp, RpcFilterType};

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let program_id = Pubkey::new_unique();

    // Subscribe with a memcmp filter: offset 0, bytes [0xAA, 0xBB]
    let filters = vec![RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
        0,
        vec![0xAA, 0xBB],
    ))];
    let program_rx = svm_locker.subscribe_for_program_updates(
        &program_id,
        Some(UiAccountEncoding::Base64),
        Some(filters),
    );

    // Set an account that doesn't match the memcmp filter
    let non_matching_pubkey = Pubkey::new_unique();
    let non_matching_account = Account {
        lamports: 1_000_000,
        data: vec![0x00, 0x00, 0xCC, 0xDD],
        owner: program_id,
        executable: false,
        rent_epoch: 0,
    };
    svm_locker.with_svm_writer(|svm| {
        svm.set_account(&non_matching_pubkey, non_matching_account)
            .unwrap();
    });

    // Should NOT receive a notification
    let no_update = program_rx.recv_timeout(Duration::from_millis(200));
    assert!(
        no_update.is_err(),
        "Should NOT receive notification for account that doesn't match memcmp filter"
    );

    // Set an account that matches the memcmp filter
    let matching_pubkey = Pubkey::new_unique();
    let matching_account = Account {
        lamports: 2_000_000,
        data: vec![0xAA, 0xBB, 0xCC, 0xDD],
        owner: program_id,
        executable: false,
        rent_epoch: 0,
    };
    svm_locker.with_svm_writer(|svm| {
        svm.set_account(&matching_pubkey, matching_account).unwrap();
    });

    // Should receive a notification
    let update = program_rx.recv_timeout(Duration::from_secs(5));
    assert!(
        update.is_ok(),
        "Should receive notification for account matching memcmp filter"
    );

    let keyed_account = update.unwrap();
    assert_eq!(
        keyed_account.pubkey,
        matching_pubkey.to_string(),
        "Notification should contain the correct matching account pubkey"
    );
    println!("✓ programSubscribe with memcmp filter works correctly");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_program_unsubscribe(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let program_id = Pubkey::new_unique();

    // Subscribe to program updates
    let program_rx = svm_locker.subscribe_for_program_updates(
        &program_id,
        Some(UiAccountEncoding::Base64),
        None,
    );

    // Verify subscription works by setting an account
    let account_pubkey = Pubkey::new_unique();
    let account = Account {
        lamports: 1_000_000,
        data: vec![1, 2, 3, 4],
        owner: program_id,
        executable: false,
        rent_epoch: 0,
    };
    svm_locker.with_svm_writer(|svm| {
        svm.set_account(&account_pubkey, account).unwrap();
    });

    let update = program_rx.recv_timeout(Duration::from_secs(5));
    assert!(update.is_ok(), "Should receive first notification");

    // Drop the receiver to simulate unsubscribe
    drop(program_rx);

    // Set another account - this should not panic or error
    let account_pubkey2 = Pubkey::new_unique();
    let account2 = Account {
        lamports: 2_000_000,
        data: vec![5, 6, 7, 8],
        owner: program_id,
        executable: false,
        rent_epoch: 0,
    };
    svm_locker.with_svm_writer(|svm| {
        svm.set_account(&account_pubkey2, account2).unwrap();
    });

    println!("✓ programUnsubscribe works correctly (no panic after receiver dropped)");
}

#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_program_subscribe_wrong_owner(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let subscribed_program_id = Pubkey::new_unique();
    let other_program_id = Pubkey::new_unique();

    // Subscribe to updates for subscribed_program_id
    let program_rx = svm_locker.subscribe_for_program_updates(
        &subscribed_program_id,
        Some(UiAccountEncoding::Base64),
        None,
    );

    // Set an account owned by a DIFFERENT program
    let account_pubkey = Pubkey::new_unique();
    let account = Account {
        lamports: 1_000_000,
        data: vec![1, 2, 3, 4],
        owner: other_program_id,
        executable: false,
        rent_epoch: 0,
    };
    svm_locker.with_svm_writer(|svm| {
        svm.set_account(&account_pubkey, account).unwrap();
    });

    // Should NOT receive a notification since the account is owned by a different program
    let no_update = program_rx.recv_timeout(Duration::from_millis(200));
    assert!(
        no_update.is_err(),
        "Should NOT receive notification for account owned by a different program"
    );
    println!("✓ programSubscribe correctly ignores accounts owned by other programs");
}

/// Regression test for issue #530: Token-2022's TokenMetadataInstruction::Initialize
/// previously failed with "Failed to reallocate account data".
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_token2022_metadata_realloc(test_type: TestType) {
    use solana_system_interface::instruction as system_instruction;
    use spl_token_2022_interface::{
        extension::{ExtensionType, metadata_pointer},
        instruction::initialize_mint2,
        pod::PodMint,
    };

    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let payer = Keypair::new();
    let mint = Keypair::new();
    let decimals = 6u8;

    svm_locker
        .airdrop(&payer.pubkey(), 10 * LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());

    // Calculate space for mint with MetadataPointer + MintCloseAuthority extensions only.
    // NOT pre-sized for metadata content — the CPI realloc will need to grow it.
    let mint_space =
        ExtensionType::try_calculate_account_len::<PodMint>(&[ExtensionType::MetadataPointer])
            .unwrap();

    // Exact minimum rent
    let mint_rent =
        svm_locker.with_svm_reader(|svm| svm.inner.minimum_balance_for_rent_exemption(mint_space));

    // --- Transaction 1: Setup (create mint with MetadataPointer) ---

    let create_account_ix = system_instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        mint_rent,
        mint_space as u64,
        &spl_token_2022_interface::id(),
    );

    // Initialize metadata pointer to the mint itself (must come before mint init)
    let init_metadata_pointer_ix = metadata_pointer::instruction::initialize(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        Some(payer.pubkey()),
        Some(mint.pubkey()),
    )
    .unwrap();

    let init_mint_ix = initialize_mint2(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        &payer.pubkey(),
        None,
        decimals,
    )
    .unwrap();

    let setup_msg = Message::new_with_blockhash(
        &[create_account_ix, init_metadata_pointer_ix, init_mint_ix],
        Some(&payer.pubkey()),
        &recent_blockhash,
    );

    let setup_tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(setup_msg), &[&payer, &mint])
            .unwrap();
    let setup_result =
        svm_locker.with_svm_writer(|svm| svm.send_transaction(setup_tx, false, false));

    assert!(
        setup_result.is_ok(),
        "Setup transaction (create mint with MetadataPointer) \
         should succeed: {:?}",
        setup_result,
    );
    println!("✓ Setup transaction succeeded");

    // --- Transaction 2: Increase rent and initialize metadata reproduces bug #530) ---

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());

    // Increase mint account lamports to ensure it can cover the cost of realloc during CPI.
    let increase_mint_rent_ix =
        system_instruction::transfer(&payer.pubkey(), &mint.pubkey(), LAMPORTS_PER_SOL);

    // Build the metadata initialize instruction data (discriminator + borsh-serialized fields).
    // We extract the data bytes from a properly-encoded instruction to pass through our CPI fixture.
    let metadata_ix_data = spl_token_metadata_interface::instruction::initialize(
        &spl_token_2022_interface::id(),
        &mint.pubkey(),
        &payer.pubkey(),
        &mint.pubkey(),
        &payer.pubkey(),
        "Test Token".to_string(),
        "TST".to_string(),
        "https://example.com/metadata.json".to_string(),
    );

    let msg = Message::new_with_blockhash(
        &[increase_mint_rent_ix, metadata_ix_data],
        Some(&payer.pubkey()),
        &recent_blockhash,
    );

    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[&payer]).unwrap();
    let result = svm_locker.with_svm_writer(|svm| svm.send_transaction(tx, false, false));

    // Regression test for #530: CPI realloc previously failed when Token-2022
    // tried to realloc the mint account to store metadata
    assert!(
        result.is_ok(),
        "CPI metadata initialization should succeed (regression test for #530): {:?}",
        result,
    );
    println!("✓ Regression test #530: Token-2022 metadata CPI realloc works correctly");
}

async fn test_duplicate_transaction_rejected(test_type: TestType) {
    let (svm_instance, _simnet_events_rx, _geyser_events_rx) = test_type.initialize_svm();
    let svm_locker = SurfnetSvmLocker::new(svm_instance);

    let payer = Keypair::new();
    let recipient = Pubkey::new_unique();
    let lamports_to_send = 1_000_000;

    // Airdrop SOL to payer
    svm_locker
        .with_svm_writer(|svm| svm.airdrop(&payer.pubkey(), lamports_to_send * 10))
        .unwrap()
        .unwrap();

    // Build a transfer transaction
    let instruction = transfer(&payer.pubkey(), &recipient, lamports_to_send);
    let latest_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());
    let message =
        Message::new_with_blockhash(&[instruction], Some(&payer.pubkey()), &latest_blockhash);
    let transaction =
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&payer]).unwrap();

    // First submission should succeed
    let (status_tx, status_rx) = crossbeam_unbounded();
    svm_locker
        .process_transaction(&None, transaction.clone(), status_tx, false, true)
        .await
        .unwrap();

    match status_rx.recv() {
        Ok(TransactionStatusEvent::Success(_)) => {
            println!("First transaction processed successfully");
        }
        other => {
            panic!("Expected first transaction to succeed, got: {:?}", other);
        }
    }

    // Second submission of the same transaction should be rejected
    let (status_tx2, status_rx2) = crossbeam_unbounded();
    let result = svm_locker
        .process_transaction(&None, transaction.clone(), status_tx2, false, true)
        .await;

    assert!(result.is_err(), "Duplicate transaction should be rejected");

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("already been processed"),
        "Error should mention 'already been processed', got: {err_msg}"
    );

    // Status channel should receive a VerificationFailure
    match status_rx2.recv() {
        Ok(TransactionStatusEvent::VerificationFailure(msg)) => {
            assert!(
                msg.contains("already been processed"),
                "VerificationFailure should mention 'already been processed', got: {msg}"
            );
            println!("Duplicate correctly rejected with VerificationFailure");
        }
        other => {
            panic!(
                "Expected VerificationFailure on status channel, got: {:?}",
                other
            );
        }
    }

    // Verify the original transaction data is still stored correctly
    let sig = transaction.signatures[0].to_string();
    let stored = svm_locker.with_svm_reader(|svm| svm.transactions.get(&sig));
    assert!(
        matches!(
            stored,
            Ok(Some(crate::types::SurfnetTransactionStatus::Processed(_)))
        ),
        "Original transaction should still be stored as Processed"
    );

    println!("Duplicate transaction rejection test passed!");
}

// ============================================================================
// AccountLoadedTwice: V0 transactions with duplicate accounts in static keys + ALT
// ============================================================================
// When a V0 transaction has an account in both its static keys and an Address
// Lookup Table, Agave rejects pre-execution with AccountLoadedTwice (0 CU).
// Surfpool must match this behavior.
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test(flavor = "multi_thread")]
async fn test_account_loaded_twice_rejected(test_type: TestType) {
    let (svm_locker, _simnet_cmd_tx, _simnet_events_rx) =
        boot_simnet(BlockProductionMode::Clock, Some(400), test_type);

    let payer = Keypair::new();
    svm_locker
        .airdrop(&payer.pubkey(), LAMPORTS_PER_SOL)
        .unwrap()
        .unwrap();

    let recent_blockhash = svm_locker.with_svm_reader(|svm| svm.latest_blockhash());

    // Create an ALT that contains system_program (11111...111)
    let alt_key = Pubkey::new_unique();
    let system_program_id = system_program::id();

    let alt_account_data = AddressLookupTable {
        meta: LookupTableMeta {
            authority: Some(payer.pubkey()),
            ..Default::default()
        },
        addresses: vec![system_program_id].into(),
    };

    svm_locker.with_svm_writer(|svm| {
        let alt_data = alt_account_data.serialize_for_tests().unwrap();
        let alt_account = Account {
            lamports: 1_000_000,
            data: alt_data,
            owner: solana_address_lookup_table_interface::program::id(),
            executable: false,
            rent_epoch: 0,
        };
        svm.set_account(&alt_key, alt_account).unwrap();
    });

    // Build a V0 message that has system_program in BOTH static keys AND the ALT.
    // try_compile would deduplicate, so we build the message manually.
    let recipient = Pubkey::new_unique();
    let v0_message = v0::Message {
        header: MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            // system_program is readonly unsigned
            num_readonly_unsigned_accounts: 1,
        },
        // Static keys: [payer (signer+writable), recipient (writable), system_program (readonly)]
        account_keys: vec![payer.pubkey(), recipient, system_program_id],
        recent_blockhash,
        // A simple transfer instruction: program_id_index=2 (system_program),
        // accounts=[0 (payer), 1 (recipient)]
        instructions: vec![solana_message::compiled_instruction::CompiledInstruction {
            program_id_index: 2,
            accounts: vec![0, 1],
            data: {
                // system_instruction::transfer encodes as: [2,0,0,0] + 8-byte LE amount
                let mut data = vec![2, 0, 0, 0];
                data.extend_from_slice(&100u64.to_le_bytes());
                data
            },
        }],
        // ALT lookup that also loads system_program (index 0 in the ALT) as readonly
        address_table_lookups: vec![MessageAddressTableLookup {
            account_key: alt_key,
            writable_indexes: vec![],
            readonly_indexes: vec![0], // system_program is at index 0 in the ALT
        }],
    };

    let tx = VersionedTransaction::try_new(VersionedMessage::V0(v0_message), &[&payer])
        .expect("Failed to create transaction");

    let (status_tx, _status_rx) = crossbeam_channel::unbounded();
    let result = svm_locker
        .process_transaction(&None, tx, status_tx, false, true)
        .await;

    assert!(
        result.is_err(),
        "Transaction with duplicate account should be rejected, got: {:?}",
        result
    );
    let err_string = result.unwrap_err().to_string();
    assert!(
        err_string.contains("Account loaded twice"),
        "Expected AccountLoadedTwice error, got: {}",
        err_string
    );

    println!("AccountLoadedTwice rejection test passed!");
}

#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::no_db(); "with no db")]
#[cfg_attr(feature = "postgres", test_case(TestType::postgres(); "with postgres db"))]
#[tokio::test]
async fn test_send_transaction_skip_sig_verify_processes_and_updates_state(test_type: TestType) {
    let payer = Keypair::new();
    let recipient = Keypair::new().pubkey();
    let transfer_amount = LAMPORTS_PER_SOL / 2;
    let airdrop_amount = LAMPORTS_PER_SOL * 2;
    let bind_host = "127.0.0.1";
    let bind_port = get_free_port().unwrap();
    let ws_port = get_free_port().unwrap();

    let config = SurfpoolConfig {
        simnets: vec![SimnetConfig {
            slot_time: 1,
            airdrop_addresses: vec![payer.pubkey()],
            airdrop_token_amount: airdrop_amount,
            ..SimnetConfig::default()
        }],
        rpc: RpcConfig {
            bind_host: bind_host.to_string(),
            bind_port,
            ws_port,
            ..Default::default()
        },
        ..SurfpoolConfig::default()
    };

    let (surfnet_svm, simnet_events_rx, geyser_events_rx) = test_type.initialize_svm();
    let (simnet_commands_tx, simnet_commands_rx) = unbounded();

    let svm_locker = SurfnetSvmLocker::new(surfnet_svm);

    let _handle = hiro_system_kit::thread_named("test").spawn(move || {
        let future = start_local_surfnet_runloop(
            svm_locker,
            config,
            simnet_commands_tx,
            simnet_commands_rx,
            geyser_events_rx,
        );
        if let Err(e) = hiro_system_kit::nestable_block_on(future) {
            panic!("{e:?}");
        }
    });

    wait_for_ready_and_connected(&simnet_events_rx);

    let minimal_client =
        http::connect::<MinimalClient>(format!("http://{bind_host}:{bind_port}").as_str())
            .await
            .expect("Failed to connect to Surfpool");
    let full_client =
        http::connect::<FullClient>(format!("http://{bind_host}:{bind_port}").as_str())
            .await
            .expect("Failed to connect to Surfpool");

    // Verify payer got airdrop
    let payer_balance = minimal_client
        .get_balance(payer.pubkey().to_string(), None)
        .await
        .expect("Failed to get payer balance");
    assert_eq!(
        payer_balance.value, airdrop_amount,
        "Payer should have airdrop amount"
    );

    // Verify recipient starts with zero
    let recipient_balance = minimal_client
        .get_balance(recipient.to_string(), None)
        .await
        .expect("Failed to get recipient balance");
    assert_eq!(
        recipient_balance.value, 0,
        "Recipient should start with zero balance"
    );

    // Get a recent blockhash
    let recent_blockhash = full_client
        .get_latest_blockhash(None)
        .await
        .map(|r| {
            Hash::from_str(r.value.blockhash.as_str()).expect("Failed to deserialize blockhash")
        })
        .expect("Failed to get blockhash");

    // Build a transaction with an INVALID signature (Signature::new_unique instead of real signing)
    let msg = VersionedMessage::Legacy(Message::new_with_blockhash(
        &[system_instruction::transfer(
            &payer.pubkey(),
            &recipient,
            transfer_amount,
        )],
        Some(&payer.pubkey()),
        &recent_blockhash,
    ));
    let tx = VersionedTransaction {
        signatures: vec![solana_signature::Signature::new_unique()],
        message: msg,
    };

    let encoded = bincode::serialize(&tx).expect("Failed to serialize tx");
    let data = bs58::encode(encoded).into_string();

    // Send with skip_sig_verify=true and skip_preflight=true
    let send_config = SurfpoolRpcSendTransactionConfig {
        base: RpcSendTransactionConfig {
            skip_preflight: true,
            ..Default::default()
        },
        skip_sig_verify: Some(true),
    };
    let _sig = full_client
        .send_transaction(data, Some(send_config))
        .await
        .expect("send_transaction with skip_sig_verify should succeed");

    // Wait for the transaction to be processed
    let _ = task::spawn_blocking(move || {
        loop {
            match simnet_events_rx.recv() {
                Ok(SimnetEvent::TransactionProcessed(..)) => break,
                _ => (),
            }
        }
    })
    .await;

    // Assert recipient received the transfer
    let final_recipient_balance = minimal_client
        .get_balance(recipient.to_string(), None)
        .await
        .expect("Failed to get final recipient balance");
    assert_eq!(
        final_recipient_balance.value, transfer_amount,
        "Recipient should have received the transfer amount"
    );

    // Assert payer balance decreased by transfer + fees
    let final_payer_balance = minimal_client
        .get_balance(payer.pubkey().to_string(), None)
        .await
        .expect("Failed to get final payer balance");
    assert!(
        final_payer_balance.value < airdrop_amount - transfer_amount,
        "Payer balance ({}) should be less than airdrop minus transfer ({}) due to fees",
        final_payer_balance.value,
        airdrop_amount - transfer_amount
    );
    assert!(
        final_payer_balance.value > airdrop_amount - transfer_amount - LAMPORTS_PER_SOL / 10,
        "Payer balance ({}) should not have decreased by more than transfer + reasonable fees",
        final_payer_balance.value,
    );
}

#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[test_case(TestType::no_db(); "with no db")]
#[tokio::test]
async fn test_airdrop_amount_zero_skips_airdrops(test_type: TestType) {
    let recipient = Keypair::new().pubkey();
    let bind_host = "127.0.0.1";
    let bind_port = get_free_port().unwrap();
    let ws_port = get_free_port().unwrap();

    let config = SurfpoolConfig {
        simnets: vec![SimnetConfig {
            slot_time: 1,
            airdrop_addresses: vec![recipient],
            airdrop_token_amount: 0,
            ..SimnetConfig::default()
        }],
        rpc: RpcConfig {
            bind_host: bind_host.to_string(),
            bind_port,
            ws_port,
            ..Default::default()
        },
        ..SurfpoolConfig::default()
    };

    let (surfnet_svm, simnet_events_rx, geyser_events_rx) = test_type.initialize_svm();
    let (simnet_commands_tx, simnet_commands_rx) = unbounded();

    let svm_locker = SurfnetSvmLocker::new(surfnet_svm);

    let _handle = hiro_system_kit::thread_named("test").spawn(move || {
        let future = start_local_surfnet_runloop(
            svm_locker,
            config,
            simnet_commands_tx,
            simnet_commands_rx,
            geyser_events_rx,
        );
        if let Err(e) = hiro_system_kit::nestable_block_on(future) {
            panic!("{e:?}");
        }
    });

    // Startup must reach Ready without panicking. This is the core regression
    // assertion for https://github.com/solana-foundation/surfpool/issues/651.
    wait_for_ready_and_connected(&simnet_events_rx);

    let minimal_client =
        http::connect::<MinimalClient>(format!("http://{bind_host}:{bind_port}").as_str())
            .await
            .expect("Failed to connect to Surfpool");

    let recipient_balance = minimal_client
        .get_balance(recipient.to_string(), None)
        .await
        .expect("Failed to get recipient balance");
    assert_eq!(
        recipient_balance.value, 0,
        "Recipient should have zero balance when --airdrop-amount=0"
    );
}

#[cfg_attr(feature = "ignore_tests_ci", ignore = "flaky CI tests")]
#[test_case(TestType::no_db(); "with no db")]
#[tokio::test]
async fn test_airdrop_amount_below_rent_skips_airdrops(test_type: TestType) {
    let recipient = Keypair::new().pubkey();
    let bind_host = "127.0.0.1";
    let bind_port = get_free_port().unwrap();
    let ws_port = get_free_port().unwrap();

    // 1 lamport is far below any rent-exempt minimum, so airdrop must be skipped.
    let config = SurfpoolConfig {
        simnets: vec![SimnetConfig {
            slot_time: 1,
            airdrop_addresses: vec![recipient],
            airdrop_token_amount: 1,
            ..SimnetConfig::default()
        }],
        rpc: RpcConfig {
            bind_host: bind_host.to_string(),
            bind_port,
            ws_port,
            ..Default::default()
        },
        ..SurfpoolConfig::default()
    };

    let (surfnet_svm, simnet_events_rx, geyser_events_rx) = test_type.initialize_svm();
    let (simnet_commands_tx, simnet_commands_rx) = unbounded();

    let svm_locker = SurfnetSvmLocker::new(surfnet_svm);

    let _handle = hiro_system_kit::thread_named("test").spawn(move || {
        let future = start_local_surfnet_runloop(
            svm_locker,
            config,
            simnet_commands_tx,
            simnet_commands_rx,
            geyser_events_rx,
        );
        if let Err(e) = hiro_system_kit::nestable_block_on(future) {
            panic!("{e:?}");
        }
    });

    wait_for_ready_and_connected(&simnet_events_rx);

    let minimal_client =
        http::connect::<MinimalClient>(format!("http://{bind_host}:{bind_port}").as_str())
            .await
            .expect("Failed to connect to Surfpool");

    let recipient_balance = minimal_client
        .get_balance(recipient.to_string(), None)
        .await
        .expect("Failed to get recipient balance");
    assert_eq!(
        recipient_balance.value, 0,
        "Recipient should have zero balance when --airdrop-amount is below rent-exempt minimum"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_airdrop_pubkeys_zero_amount_unit() {
    use crate::{rpc::minimal::SurfpoolMinimalRpc, tests::helpers::TestSetup};

    let setup = TestSetup::new(SurfpoolMinimalRpc);
    let recipient = Pubkey::new_unique();
    setup.context.svm_locker.airdrop_pubkeys(0, &[recipient]);

    let account = setup
        .context
        .svm_locker
        .with_svm_reader(|svm| svm.get_account(&recipient).ok().flatten());
    assert!(
        account.is_none() || account.unwrap().lamports == 0,
        "Recipient must not be funded when airdrop amount is 0"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_airdrop_pubkeys_below_rent_unit() {
    use crate::{rpc::minimal::SurfpoolMinimalRpc, tests::helpers::TestSetup};

    let setup = TestSetup::new(SurfpoolMinimalRpc);
    let recipient = Pubkey::new_unique();

    let min_rent = setup
        .context
        .svm_locker
        .with_svm_reader(|svm| svm.inner.minimum_balance_for_rent_exemption(0));
    assert!(min_rent > 1, "Rent-exempt minimum should be greater than 1");

    setup
        .context
        .svm_locker
        .airdrop_pubkeys(min_rent - 1, &[recipient]);

    let account = setup
        .context
        .svm_locker
        .with_svm_reader(|svm| svm.get_account(&recipient).ok().flatten());
    assert!(
        account.is_none() || account.unwrap().lamports == 0,
        "Recipient must not be funded when airdrop amount is below rent-exempt minimum"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_airdrop_pubkeys_at_rent_minimum_unit() {
    use crate::{rpc::minimal::SurfpoolMinimalRpc, tests::helpers::TestSetup};

    let setup = TestSetup::new(SurfpoolMinimalRpc);
    let recipient = Pubkey::new_unique();

    let min_rent = setup
        .context
        .svm_locker
        .with_svm_reader(|svm| svm.inner.minimum_balance_for_rent_exemption(0));

    setup
        .context
        .svm_locker
        .airdrop_pubkeys(min_rent, &[recipient]);

    let account = setup
        .context
        .svm_locker
        .with_svm_reader(|svm| svm.get_account(&recipient).ok().flatten())
        .expect("Recipient should be funded when airdrop amount equals rent-exempt minimum");
    assert_eq!(
        account.lamports, min_rent,
        "Recipient should be funded with exactly the rent-exempt minimum"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_airdrop_pubkeys_empty_addresses_unit() {
    use crate::{rpc::minimal::SurfpoolMinimalRpc, tests::helpers::TestSetup};

    let setup = TestSetup::new(SurfpoolMinimalRpc);
    // Must not panic with an empty address slice, regardless of amount.
    setup.context.svm_locker.airdrop_pubkeys(0, &[]);
    setup
        .context
        .svm_locker
        .airdrop_pubkeys(LAMPORTS_PER_SOL, &[]);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_airdrop_returns_zero_amount_variant() {
    use crate::{error::AirdropError, rpc::minimal::SurfpoolMinimalRpc, tests::helpers::TestSetup};

    let setup = TestSetup::new(SurfpoolMinimalRpc);
    let recipient = Pubkey::new_unique();

    let err = setup
        .context
        .svm_locker
        .airdrop(&recipient, 0)
        .expect_err("airdrop with 0 lamports must error");
    assert!(
        matches!(err, AirdropError::ZeroAmount),
        "expected ZeroAmount, got {err:?}",
    );

    let account = setup
        .context
        .svm_locker
        .with_svm_reader(|svm| svm.get_account(&recipient).ok().flatten());
    assert!(
        account.is_none(),
        "recipient must not be created when airdrop is rejected"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_airdrop_returns_below_rent_variant() {
    use crate::{error::AirdropError, rpc::minimal::SurfpoolMinimalRpc, tests::helpers::TestSetup};

    let setup = TestSetup::new(SurfpoolMinimalRpc);
    let recipient = Pubkey::new_unique();

    let min_rent = setup
        .context
        .svm_locker
        .with_svm_reader(|svm| svm.inner.minimum_balance_for_rent_exemption(0));
    assert!(min_rent > 1);

    let err = setup
        .context
        .svm_locker
        .airdrop(&recipient, 1)
        .expect_err("airdrop with 1 lamport must error");
    assert!(
        matches!(
            err,
            AirdropError::BelowRentExemption { lamports: 1, min_rent: m } if m == min_rent
        ),
        "expected BelowRentExemption, got {err:?}",
    );

    let account = setup
        .context
        .svm_locker
        .with_svm_reader(|svm| svm.get_account(&recipient).ok().flatten());
    assert!(
        account.is_none(),
        "recipient must not be created when airdrop is rejected"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_request_airdrop_rejects_zero_amount() {
    use crate::{
        rpc::full::{Full, SurfpoolFullRpc},
        tests::helpers::TestSetup,
    };

    let setup = TestSetup::new(SurfpoolFullRpc);
    let recipient = Pubkey::new_unique();
    let err = setup
        .rpc
        .request_airdrop(Some(setup.context.clone()), recipient.to_string(), 0, None)
        .expect_err("requestAirdrop with 0 lamports must return an RPC error");
    assert_eq!(err.code, jsonrpc_core::ErrorCode::InvalidParams);
    assert!(err.message.contains("greater than zero"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_request_airdrop_rejects_below_rent_amount() {
    use crate::{
        rpc::full::{Full, SurfpoolFullRpc},
        tests::helpers::TestSetup,
    };

    let setup = TestSetup::new(SurfpoolFullRpc);
    let recipient = Pubkey::new_unique();
    let err = setup
        .rpc
        .request_airdrop(Some(setup.context.clone()), recipient.to_string(), 1, None)
        .expect_err("requestAirdrop below rent exemption must return an RPC error");
    assert_eq!(err.code, jsonrpc_core::ErrorCode::InvalidParams);
    assert!(err.message.contains("rent-exempt minimum"));
}
