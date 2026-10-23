//! Real historical surfnets through the SDK builder: SVM Replay executes two forks of the
//! same boundary side by side. Needs a staged SVM Replay backend and its cached history, so
//! it is ignored unless run explicitly with `--ignored` and the `M59_*` variables set.

use std::path::PathBuf;

use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_system_interface::instruction::transfer;
use solana_transaction::Transaction;
use surfpool_sdk::Surfnet;
use surfpool_types::HistoricalSessionOptions;

fn options() -> HistoricalSessionOptions {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
    HistoricalSessionOptions {
        after: var("M59_AFTER"),
        program: Some(var("M59_SVM_REPLAY_BIN")),
        data_dir: Some(PathBuf::from(var("M59_HISTORICAL_DATA_DIR"))),
        config: Some(PathBuf::from(var("M59_HISTORICAL_CONFIG"))),
        ..Default::default()
    }
}

/// A new system account, defined completely so the fork needs no archived image of it.
fn define(surfnet: &Surfnet, pubkey: &Pubkey, lamports: u64) {
    surfnet
        .rpc_client()
        .send::<serde_json::Value>(
            solana_client::rpc_request::RpcRequest::Custom {
                method: "surfnet_setAccount",
            },
            serde_json::json!([pubkey.to_string(), {"lamports": lamports,
                "owner": "11111111111111111111111111111111", "data": "",
                "executable": false, "rentEpoch": 0}]),
        )
        .expect("define a local account");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a staged SVM Replay backend and cached history (M59_* variables)"]
async fn two_historical_surfnets_are_isolated_and_stop_independently() {
    let slot: u64 = std::env::var("M59_SLOT").unwrap().parse().unwrap();
    let mut first = Surfnet::builder()
        .historical(options())
        .start()
        .await
        .expect("first historical surfnet");
    let mut second = Surfnet::builder()
        .historical(options())
        .start()
        .await
        .expect("second historical surfnet");
    // The SDK's 1 ms slot time would tick the clock at once: a historical fork starts frozen.
    std::thread::sleep(std::time::Duration::from_millis(200));
    for surfnet in [&first, &second] {
        let client = surfnet.rpc_client();
        assert_eq!(client.get_slot().unwrap(), slot);
        assert_eq!(
            client.get_balance(&surfnet.payer().pubkey()).unwrap(),
            10_000_000_000,
            "the payer is a local definition"
        );
    }

    let keys = vec![first.payer().pubkey(); 100];
    assert_eq!(
        first
            .rpc_client()
            .get_multiple_accounts(&keys)
            .unwrap()
            .len(),
        100
    );
    let rejected = first
        .rpc_client()
        .get_multiple_accounts(&[first.payer().pubkey(); 101])
        .unwrap_err();
    assert!(rejected.to_string().contains("at most 100"), "{rejected}");
    // Unknown keys must be refused before historical account acquisition too.
    let rejected = first
        .rpc_client()
        .get_multiple_accounts(&[Pubkey::new_unique(); 101])
        .unwrap_err();
    assert!(rejected.to_string().contains("at most 100"), "{rejected}");
    assert_eq!(first.rpc_client().get_slot().unwrap(), slot);

    let recipient = Keypair::new().pubkey();
    define(&first, &recipient, 0);
    define(&second, &recipient, 0);
    let client = first.rpc_client();
    let blockhash = client.get_latest_blockhash().unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[transfer(&first.payer().pubkey(), &recipient, 1_234_567)],
        Some(&first.payer().pubkey()),
        &[first.payer()],
        blockhash,
    );
    client.send_and_confirm_transaction(&tx).unwrap();
    assert_eq!(client.get_balance(&recipient).unwrap(), 1_234_567);
    assert_eq!(second.rpc_client().get_balance(&recipient).unwrap(), 0);

    first.stop().expect("the first surfnet stops");
    assert_eq!(second.rpc_client().get_slot().unwrap(), slot);
    second.stop().expect("the second surfnet stops");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a staged SVM Replay backend and cached history (M59_* variables)"]
async fn a_historical_surfnet_time_travels_into_a_later_epoch_through_the_sdk() {
    let mut surfnet = Surfnet::builder()
        .historical(options())
        .start()
        .await
        .expect("historical surfnet");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let client = surfnet.rpc_client();
    let before = client.get_epoch_info().unwrap();
    // An explicit synthetic warp: the clock enters the epoch, the runtime stays the boundary's.
    let after = surfnet
        .cheatcodes()
        .time_travel_to_epoch(before.epoch + 1)
        .expect("a synthetic epoch warp");
    assert_eq!(after.epoch, before.epoch + 1);
    assert_eq!(after.slot_index, 0);
    assert_eq!(client.get_epoch_info().unwrap().epoch, before.epoch + 1);

    let recipient = Keypair::new().pubkey();
    define(&surfnet, &recipient, 0);
    let blockhash = client.get_latest_blockhash().unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[transfer(&surfnet.payer().pubkey(), &recipient, 1_000_000)],
        Some(&surfnet.payer().pubkey()),
        &[surfnet.payer()],
        blockhash,
    );
    client.send_and_confirm_transaction(&tx).unwrap();
    assert_eq!(client.get_balance(&recipient).unwrap(), 1_000_000);
    surfnet.stop().expect("the surfnet stops");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a staged SVM Replay backend and cached history (M59_* variables)"]
async fn a_historical_surfnet_that_fails_to_start_returns_its_error() {
    let slot: u64 = std::env::var("M59_SLOT").unwrap().parse().unwrap();
    let mut running = Surfnet::builder()
        .historical(options())
        .start()
        .await
        .expect("a working historical surfnet");
    let started = std::time::Instant::now();
    let failed = Surfnet::builder()
        .historical(HistoricalSessionOptions {
            provider: Some("m59-no-such-provider".into()),
            ..options()
        })
        .start()
        .await;
    let Err(error) = failed else {
        panic!("a surfnet with an unknown provider started");
    };
    eprintln!("failed constructor: {error}");
    assert!(started.elapsed() < std::time::Duration::from_secs(120));
    assert_eq!(running.rpc_client().get_slot().unwrap(), slot);
    running.stop().expect("the working surfnet stops");
}
