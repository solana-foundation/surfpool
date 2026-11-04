//! `SurfnetBuilder::snapshot` and `SurfnetBuilder::snapshot_file` preload
//! accounts the way the CLI's `--snapshot` flag does, so an offline surfnet
//! can start from state exported by `surfnet_exportSnapshot`.

use std::collections::BTreeMap;

use base64::{Engine, prelude::BASE64_STANDARD};
use solana_client::rpc_request::RpcRequest;
use surfpool_sdk::{AccountSnapshot, Keypair, Pubkey, Signer, Surfnet, SurfnetError};

fn account_snapshot(lamports: u64, owner: &Pubkey, data: &[u8]) -> AccountSnapshot {
    AccountSnapshot::new(
        lamports,
        owner.to_string(),
        false,
        0,
        BASE64_STANDARD.encode(data),
        None,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_preloads_accounts_offline() {
    let address = Pubkey::new_unique();
    let owner = Pubkey::new_unique();
    let snapshot = BTreeMap::from([(
        address.to_string(),
        Some(account_snapshot(1_000_000, &owner, &[1, 2, 3, 4])),
    )]);

    let surfnet = Surfnet::builder()
        .offline(true)
        .snapshot(snapshot)
        .start()
        .await
        .unwrap();

    let account = surfnet.rpc_client().get_account(&address).unwrap();
    assert_eq!(account.lamports, 1_000_000);
    assert_eq!(account.owner, owner);
    assert_eq!(account.data, vec![1, 2, 3, 4]);
}

#[tokio::test(flavor = "multi_thread")]
async fn later_snapshots_override_earlier_ones() {
    let overridden = Pubkey::new_unique();
    let kept = Pubkey::new_unique();
    let owner = Pubkey::new_unique();

    let file = tempfile::NamedTempFile::new().unwrap();
    let from_file = BTreeMap::from([
        (
            overridden.to_string(),
            Some(account_snapshot(1_000_000, &owner, &[1])),
        ),
        (
            kept.to_string(),
            Some(account_snapshot(2_000_000, &owner, &[2])),
        ),
    ]);
    std::fs::write(file.path(), serde_json::to_string(&from_file).unwrap()).unwrap();
    let from_map = BTreeMap::from([(
        overridden.to_string(),
        Some(account_snapshot(3_000_000, &owner, &[3])),
    )]);

    let surfnet = Surfnet::builder()
        .snapshot_file(file.path())
        .snapshot(from_map)
        .start()
        .await
        .unwrap();
    let rpc = surfnet.rpc_client();

    let overridden = rpc.get_account(&overridden).unwrap();
    assert_eq!(overridden.lamports, 3_000_000);
    assert_eq!(overridden.data, vec![3]);
    let kept = rpc.get_account(&kept).unwrap();
    assert_eq!(kept.lamports, 2_000_000);
    assert_eq!(kept.data, vec![2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn exported_snapshot_restores_into_a_new_surfnet() {
    let address = Pubkey::new_unique();
    let owner = Pubkey::new_unique();

    let source = Surfnet::start().await.unwrap();
    source
        .cheatcodes()
        .set_account(&address, 5_000_000, &[7, 7, 7], &owner)
        .unwrap();
    let exported: serde_json::Value = source
        .rpc_client()
        .send(
            RpcRequest::Custom {
                method: "surfnet_exportSnapshot",
            },
            serde_json::json!([]),
        )
        .unwrap();
    // The RPC wraps the snapshot in `{ context, value }`; files hold `value`.
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), exported["value"].to_string()).unwrap();

    let restored = Surfnet::builder()
        .offline(true)
        .snapshot_file(file.path())
        .start()
        .await
        .unwrap();

    let account = restored.rpc_client().get_account(&address).unwrap();
    assert_eq!(account.lamports, 5_000_000);
    assert_eq!(account.owner, owner);
    assert_eq!(account.data, vec![7, 7, 7]);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_snapshot_file_fails_startup() {
    let result = Surfnet::builder()
        .snapshot_file("/nonexistent/surfpool-snapshot.json")
        .start()
        .await;

    match result {
        Err(SurfnetError::Startup(message)) => {
            assert!(
                message.contains("failed to read snapshot file"),
                "{message}"
            )
        }
        Err(other) => panic!("expected a startup error, got {other}"),
        Ok(_) => panic!("expected a startup error, got a running surfnet"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_restores_last_restart_slot_offline() {
    // Surfpool maintains LastRestartSlot and refuses account writes to it; a
    // snapshot that carries it must still set it, or programs that compare it
    // against their own record (Phoenix) treat the restored market as halted.
    let sysvar_owner: Pubkey = "Sysvar1111111111111111111111111111111111111"
        .parse()
        .unwrap();
    let snapshot = BTreeMap::from([(
        "SysvarLastRestartS1ot1111111111111111111111".to_string(),
        Some(account_snapshot(
            1_009_200,
            &sysvar_owner,
            &458_000_123u64.to_le_bytes(),
        )),
    )]);

    let surfnet = Surfnet::builder()
        .offline(true)
        .snapshot(snapshot)
        .start()
        .await
        .unwrap();

    let account = surfnet
        .rpc_client()
        .send::<serde_json::Value>(
            RpcRequest::GetAccountInfo,
            serde_json::json!([
                "SysvarLastRestartS1ot1111111111111111111111",
                { "encoding": "base64", "commitment": "processed" }
            ]),
        )
        .unwrap();
    let data = BASE64_STANDARD
        .decode(account["value"]["data"][0].as_str().unwrap())
        .unwrap();
    assert_eq!(
        u64::from_le_bytes(data[..8].try_into().unwrap()),
        458_000_123
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restored_balances_win_over_startup_airdrops() {
    // The payer is airdropped at startup, then the snapshot restores it with a
    // different balance: startup must not wait for the airdropped amount.
    let payer = Keypair::new();
    let payer_pubkey = payer.pubkey();
    let system_program: Pubkey = "11111111111111111111111111111111".parse().unwrap();
    let snapshot = BTreeMap::from([(
        payer_pubkey.to_string(),
        Some(account_snapshot(123_456_789, &system_program, &[])),
    )]);

    let surfnet = Surfnet::builder()
        .offline(true)
        .payer(payer)
        .snapshot(snapshot)
        .start()
        .await
        .unwrap();

    assert_eq!(
        surfnet.rpc_client().get_balance(&payer_pubkey).unwrap(),
        123_456_789
    );
}
