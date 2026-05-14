use std::{
    fs,
    sync::{Mutex, MutexGuard, OnceLock},
};

use solana_program_pack::Pack;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use spl_associated_token_account_interface::address::get_associated_token_address_with_program_id;
use spl_token_interface::state::{Account as TokenAccount, Mint};
use tempfile::tempdir;

use super::{
    Cheatcodes,
    builders::{DeployProgram, ResetAccount, SetAccount, SetTokenAccount, StreamAccount},
    resolve_target_dir, spl_token_program_id,
};
use crate::{Surfnet, error::SurfnetResult};

fn create_test_mint(cheats: &Cheatcodes<'_>, mint: &Pubkey) -> SurfnetResult<()> {
    let mut mint_data = [0u8; Mint::LEN];
    let mint_state = Mint {
        decimals: 6,
        supply: 1_000_000_000,
        is_initialized: true,
        ..Mint::default()
    };
    mint_state.pack_into_slice(&mut mint_data);

    cheats.set_account(mint, 1_461_600, &mint_data, &spl_token_program_id())
}

fn read_token_amount(cheats: &Cheatcodes<'_>, owner: &Pubkey, mint: &Pubkey) -> u64 {
    let ata = cheats.get_ata(owner, mint, None);
    let account = cheats.rpc_client().get_account(&ata).unwrap();
    let token_account = TokenAccount::unpack(&account.data).unwrap();
    token_account.amount
}

fn rpc_client(cheats: &Cheatcodes<'_>) -> RpcClient {
    cheats.rpc_client()
}

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn sample_idl(program_id: &Pubkey) -> serde_json::Value {
    serde_json::json!({
        "address": program_id.to_string(),
        "metadata": {
            "name": "test_program",
            "version": "0.1.0",
            "spec": "0.1.0",
            "description": "Created with Anchor"
        },
        "instructions": [],
        "accounts": [],
        "types": [],
        "events": [],
        "errors": [],
        "constants": []
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn fund_sol_executes_against_surfnet() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let address = Pubkey::new_unique();

    cheats.fund_sol(&address, 123_456_789).unwrap();

    let balance = cheats.rpc_client().get_balance(&address).unwrap();
    assert_eq!(balance, 123_456_789);
}

#[tokio::test(flavor = "multi_thread")]
async fn set_account_executes_against_surfnet() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let address = Pubkey::new_unique();
    let owner = Pubkey::new_unique();
    let data = vec![1, 2, 3, 4, 5];

    cheats
        .set_account(&address, 424_242, &data, &owner)
        .unwrap();

    let account = cheats.rpc_client().get_account(&address).unwrap();
    assert_eq!(account.lamports, 424_242);
    assert_eq!(account.owner, owner);
    assert_eq!(account.data, data);
}

#[tokio::test(flavor = "multi_thread")]
async fn set_account_builder_executes_against_surfnet() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let address = Pubkey::new_unique();
    let owner = Pubkey::new_unique();
    let data = vec![0xAA, 0xBB, 0xCC, 0xDD];

    cheats
        .execute(
            SetAccount::new(address)
                .lamports(808_080)
                .data(data.clone())
                .owner(owner)
                .rent_epoch(42)
                .executable(false),
        )
        .unwrap();

    let account = cheats.rpc_client().get_account(&address).unwrap();
    assert_eq!(account.lamports, 808_080);
    assert_eq!(account.owner, owner);
    assert_eq!(account.data, data);
    assert_eq!(account.rent_epoch, 42);
    assert!(!account.executable);
}

#[tokio::test(flavor = "multi_thread")]
async fn fund_sol_many_executes_against_surfnet() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let first = Pubkey::new_unique();
    let second = Pubkey::new_unique();

    cheats
        .fund_sol_many(&[(&first, 10_000), (&second, 20_000)])
        .unwrap();

    assert_eq!(cheats.rpc_client().get_balance(&first).unwrap(), 10_000);
    assert_eq!(cheats.rpc_client().get_balance(&second).unwrap(), 20_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn fund_token_executes_against_surfnet() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let owner = Pubkey::new_unique();
    let mint = Pubkey::new_unique();

    create_test_mint(&cheats, &mint).unwrap();
    cheats.fund_token(&owner, &mint, 55, None).unwrap();

    assert_eq!(read_token_amount(&cheats, &owner, &mint), 55);
}

#[tokio::test(flavor = "multi_thread")]
async fn set_token_account_builder_executes_against_surfnet() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let owner = Pubkey::new_unique();
    let mint = Pubkey::new_unique();

    create_test_mint(&cheats, &mint).unwrap();
    cheats
        .execute(SetTokenAccount::new(owner, mint).amount(321))
        .unwrap();

    assert_eq!(read_token_amount(&cheats, &owner, &mint), 321);
}

#[tokio::test(flavor = "multi_thread")]
async fn set_token_balance_executes_against_surfnet() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let owner = Pubkey::new_unique();
    let mint = Pubkey::new_unique();

    create_test_mint(&cheats, &mint).unwrap();
    cheats.fund_token(&owner, &mint, 10, None).unwrap();
    cheats.set_token_balance(&owner, &mint, 777, None).unwrap();

    assert_eq!(read_token_amount(&cheats, &owner, &mint), 777);
}

#[tokio::test(flavor = "multi_thread")]
async fn fund_token_many_executes_against_surfnet() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let mint = Pubkey::new_unique();
    let first = Pubkey::new_unique();
    let second = Pubkey::new_unique();

    create_test_mint(&cheats, &mint).unwrap();
    cheats
        .fund_token_many(&[&first, &second], &mint, 999, None)
        .unwrap();

    assert_eq!(read_token_amount(&cheats, &first, &mint), 999);
    assert_eq!(read_token_amount(&cheats, &second, &mint), 999);
}

#[tokio::test(flavor = "multi_thread")]
async fn time_travel_to_slot_executes_against_surfnet() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let client = rpc_client(&cheats);
    let before = client.get_epoch_info().unwrap();
    let target_slot = before.absolute_slot + 100;

    let after = cheats.time_travel_to_slot(target_slot).unwrap();
    assert!(after.absolute_slot >= target_slot);
}

#[tokio::test(flavor = "multi_thread")]
async fn time_travel_to_epoch_executes_against_surfnet() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let client = rpc_client(&cheats);
    let before = client.get_epoch_info().unwrap();
    let target_epoch = before.epoch + 1;

    let after = cheats.time_travel_to_epoch(target_epoch).unwrap();
    assert!(after.epoch >= target_epoch);
}

#[tokio::test(flavor = "multi_thread")]
async fn time_travel_to_timestamp_executes_against_surfnet() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let client = rpc_client(&cheats);
    let before = client.get_epoch_info().unwrap();
    let target_timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 5_000;

    let after = cheats
        .time_travel_to_timestamp(target_timestamp_ms)
        .unwrap();
    assert!(after.absolute_slot > before.absolute_slot);
}

#[tokio::test(flavor = "multi_thread")]
async fn reset_account_builder_executes_against_surfnet() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let client = rpc_client(&cheats);
    let address = Pubkey::new_unique();
    let owner = Pubkey::new_unique();

    cheats
        .set_account(&address, 77_777, &[1, 2, 3], &owner)
        .unwrap();
    assert_eq!(client.get_balance(&address).unwrap(), 77_777);

    cheats
        .execute(ResetAccount::new(address).include_owned_accounts(false))
        .unwrap();

    assert_eq!(client.get_balance(&address).unwrap(), 0);
    assert!(client.get_account(&address).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_account_builder_executes_against_surfnet() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let client = rpc_client(&cheats);
    let address = Pubkey::new_unique();

    cheats
        .execute(StreamAccount::new(address).include_owned_accounts(true))
        .unwrap();

    let response = client
        .send::<serde_json::Value>(
            solana_client::rpc_request::RpcRequest::Custom {
                method: "surfnet_getStreamedAccounts",
            },
            serde_json::json!([]),
        )
        .unwrap();

    let accounts = response
        .get("value")
        .and_then(|value| value.get("accounts"))
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();

    assert!(accounts.iter().any(|account| {
        account.get("pubkey").and_then(|value| value.as_str()) == Some(&address.to_string())
            && account
                .get("includeOwnedAccounts")
                .and_then(|value| value.as_bool())
                == Some(true)
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn deploy_program_builder_executes_against_surfnet() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let client = rpc_client(&cheats);
    let program_id = Pubkey::new_unique();
    let temp = tempdir().unwrap();
    let idl_path = temp.path().join("program.json");

    fs::write(
        &idl_path,
        serde_json::to_vec(&sample_idl(&program_id)).unwrap(),
    )
    .unwrap();

    cheats
        .deploy(
            DeployProgram::new(program_id)
                .so_bytes(vec![1, 2, 3, 4, 5, 6])
                .idl_path(&idl_path),
        )
        .unwrap();

    let account = client.get_account(&program_id).unwrap();
    assert!(account.executable);
}

#[tokio::test(flavor = "multi_thread")]
async fn deploy_program_discovers_workspace_artifacts() {
    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let cheats = surfnet.cheatcodes();
    let client = rpc_client(&cheats);
    let temp = tempdir().unwrap();
    let previous_dir = std::env::current_dir().unwrap();
    let deploy_dir = temp.path().join("target/deploy");
    let idl_dir = temp.path().join("target/idl");
    let keypair = crate::Keypair::new();
    let program_name = "fixture_program";

    fs::create_dir_all(&deploy_dir).unwrap();
    fs::create_dir_all(&idl_dir).unwrap();
    fs::write(
        deploy_dir.join(format!("{program_name}.so")),
        vec![9, 8, 7, 6],
    )
    .unwrap();
    fs::write(
        deploy_dir.join(format!("{program_name}-keypair.json")),
        serde_json::to_vec(&keypair.to_bytes().to_vec()).unwrap(),
    )
    .unwrap();
    fs::write(
        idl_dir.join(format!("{program_name}.json")),
        serde_json::to_vec(&sample_idl(&keypair.pubkey())).unwrap(),
    )
    .unwrap();

    std::env::set_current_dir(temp.path()).unwrap();
    let result = cheats.deploy_program(program_name);
    std::env::set_current_dir(previous_dir).unwrap();
    result.unwrap();

    let account = client.get_account(&keypair.pubkey()).unwrap();
    assert!(account.executable);
}

#[test]
fn get_ata_matches_associated_token_derivation() {
    let owner = Pubkey::new_unique();
    let mint = Pubkey::new_unique();
    let cheats = Cheatcodes::new("http://127.0.0.1:8899");

    let derived = cheats.get_ata(&owner, &mint, None);
    let expected =
        get_associated_token_address_with_program_id(&owner, &mint, &spl_token_program_id());

    assert_eq!(derived, expected);
}

#[test]
fn resolve_target_dir_walks_up_to_project_root_target() {
    let temp = tempdir().unwrap();
    let previous_dir = std::env::current_dir().unwrap();
    let nested_crate_dir = temp.path().join("programs/simple-pda/tests");
    let deploy_dir = temp.path().join("target/deploy");
    let keypair = crate::Keypair::new();
    let program_name = "simple_pda";

    fs::create_dir_all(&nested_crate_dir).unwrap();
    fs::create_dir_all(&deploy_dir).unwrap();
    fs::write(deploy_dir.join(format!("{program_name}.so")), vec![1, 2, 3]).unwrap();
    fs::write(
        deploy_dir.join(format!("{program_name}-keypair.json")),
        serde_json::to_vec(&keypair.to_bytes().to_vec()).unwrap(),
    )
    .unwrap();

    std::env::set_current_dir(&nested_crate_dir).unwrap();
    let result = resolve_target_dir(program_name);
    std::env::set_current_dir(previous_dir).unwrap();

    assert_eq!(
        result.unwrap().canonicalize().unwrap(),
        temp.path().join("target").canonicalize().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn withdraw_excess_lamports_executes_against_surfnet() {
    use solana_instruction::{AccountMeta, Instruction};
    use solana_message::Message;
    use solana_signer::Signer;
    use solana_transaction::Transaction;

    const WITHDRAW_EXCESS_LAMPORTS: u8 = 38;

    let _guard = test_lock();
    let surfnet = Surfnet::start().await.unwrap();
    let client = surfnet.rpc_client();
    let payer = surfnet.payer();

    let source = crate::Keypair::new();
    surfnet
        .cheatcodes()
        .fund_sol(&source.pubkey(), 1_000_000_000)
        .unwrap();

    let ix = Instruction {
        program_id: spl_token_program_id(),
        accounts: vec![
            AccountMeta::new(source.pubkey(), false),
            AccountMeta::new(payer.pubkey(), false),
            AccountMeta::new_readonly(payer.pubkey(), true),
        ],
        data: vec![WITHDRAW_EXCESS_LAMPORTS],
    };
    let recent_blockhash = client.get_latest_blockhash().unwrap();
    let tx = Transaction::new(
        &[payer],
        Message::new(&[ix], Some(&payer.pubkey())),
        recent_blockhash,
    );

    let sim = client.simulate_transaction(&tx).unwrap().value;
    if let Some(err) = sim.err {
        let msg = format!("{err:?}");
        assert!(
            !msg.contains("Custom(12)") && !msg.contains("custom program error: 0xc"),
            "WithdrawExcessLamports returned InvalidInstruction: {msg}\nlogs: {:?}",
            sim.logs
        );
    }
}
