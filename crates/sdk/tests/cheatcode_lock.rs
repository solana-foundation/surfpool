//! Locked cheatcodes stay locked: no client, including one holding the RPC
//! endpoint, can turn them back on. This is what lets an embedder set up a
//! surfnet and then hand it to untrusted code (a bot under evaluation, an
//! agent) as a plain validator RPC.

use solana_client::rpc_request::RpcRequest;
use solana_signer::Signer;
use surfpool_sdk::{CheatcodeFilter, Keypair, Pubkey, Surfnet, SurfnetError};

fn call(surfnet: &Surfnet, method: &'static str, params: serde_json::Value) -> Result<(), String> {
    surfnet
        .rpc_client()
        .send::<serde_json::Value>(RpcRequest::Custom { method }, params)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

const ONE_SOL: u64 = 1_000_000_000;

/// The call was stopped by the cheatcode gate, not by anything else.
fn assert_refused<E: std::fmt::Display>(result: Result<(), E>, what: &str) {
    let error = result
        .map_err(|e| e.to_string())
        .expect_err(&format!("{what} should be refused"));
    assert!(error.contains("is currently disabled"), "{what}: {error}");
}

/// `surfnet_enableCheatcode` answered with an error. Its reason is in the
/// JSON-RPC error's `data`, which the client doesn't surface, so callers also
/// check that the method is still refused.
fn assert_rejected(result: Result<(), String>, what: &str) {
    assert!(result.is_err(), "{what} should be rejected");
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_cheatcodes_at_startup_refuses_every_cheatcode_and_airdrop() {
    let surfnet = Surfnet::builder()
        .lock_cheatcodes(CheatcodeFilter::all())
        .start()
        .await
        .unwrap();
    let target = Pubkey::new_unique();

    // Startup airdrops are not RPC calls and still land.
    let payer_balance = surfnet
        .rpc_client()
        .get_balance(&surfnet.payer().pubkey())
        .unwrap();
    assert!(payer_balance > 0);

    assert_refused(
        surfnet.cheatcodes().fund_sol(&target, 1),
        "surfnet_setAccount",
    );
    assert_refused(
        call(
            &surfnet,
            "requestAirdrop",
            serde_json::json!([target.to_string(), ONE_SOL]),
        ),
        "requestAirdrop",
    );
    assert_refused(
        call(
            &surfnet,
            "surfnet_enableCheatcode",
            serde_json::json!(["all"]),
        ),
        "surfnet_enableCheatcode",
    );
    assert_refused(
        call(
            &surfnet,
            "surfnet_disableCheatcode",
            serde_json::json!([[], { "lockout": false }]),
        ),
        "surfnet_disableCheatcode",
    );

    // Nor can it stop the surfnet or load code into it.
    assert_refused(call(&surfnet, "exit", serde_json::json!([])), "exit");
    assert_refused(
        call(
            &surfnet,
            "loadPlugin",
            serde_json::json!(["/tmp/plugin.json"]),
        ),
        "loadPlugin",
    );

    // Standard methods still work.
    surfnet.rpc_client().get_slot().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_after_setup_is_one_way() {
    let surfnet = Surfnet::start().await.unwrap();
    let player = Keypair::new().pubkey();
    let cheats = surfnet.cheatcodes();

    cheats.fund_sol(&player, 1_000_000_000).unwrap();
    cheats.lock(CheatcodeFilter::all()).unwrap();

    assert_refused(
        cheats.fund_sol(&player, 2_000_000_000),
        "surfnet_setAccount",
    );
    assert_refused(
        call(
            &surfnet,
            "requestAirdrop",
            serde_json::json!([player.to_string(), ONE_SOL]),
        ),
        "requestAirdrop",
    );
    assert_refused(cheats.lock(CheatcodeFilter::all()), "a second lock");
    assert_refused(
        call(
            &surfnet,
            "surfnet_enableCheatcode",
            serde_json::json!(["all"]),
        ),
        "surfnet_enableCheatcode",
    );
    assert_eq!(
        surfnet.rpc_client().get_balance(&player).unwrap(),
        1_000_000_000
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn locked_methods_cannot_be_re_enabled() {
    let surfnet = Surfnet::start().await.unwrap();
    let target = Pubkey::new_unique();
    let cheats = surfnet.cheatcodes();

    cheats
        .lock(CheatcodeFilter::list(["surfnet_setAccount"]))
        .unwrap();
    assert_refused(cheats.fund_sol(&target, 1), "surfnet_setAccount");
    // Only a fully locked surfnet refuses airdrops.
    call(
        &surfnet,
        "requestAirdrop",
        serde_json::json!([target.to_string(), ONE_SOL]),
    )
    .unwrap();

    // enableCheatcode is still callable, but can't undo a lock, by name or with "all".
    assert_rejected(
        call(
            &surfnet,
            "surfnet_enableCheatcode",
            serde_json::json!([["surfnet_setAccount"]]),
        ),
        "re-enabling a locked method",
    );
    assert_refused(
        cheats.fund_sol(&target, 1),
        "surfnet_setAccount after re-enabling it",
    );
    call(
        &surfnet,
        "surfnet_enableCheatcode",
        serde_json::json!(["all"]),
    )
    .unwrap();
    assert_refused(
        cheats.fund_sol(&target, 1),
        "surfnet_setAccount after enable all",
    );

    // Unlocked cheatcodes keep working.
    call(&surfnet, "surfnet_pauseClock", serde_json::json!([])).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn locking_a_list_at_startup_leaves_the_rest() {
    let surfnet = Surfnet::builder()
        .lock_cheatcodes(CheatcodeFilter::list(["surfnet_timeTravel"]))
        .start()
        .await
        .unwrap();
    let target = Pubkey::new_unique();

    assert_refused(
        call(
            &surfnet,
            "surfnet_timeTravel",
            serde_json::json!([{ "absoluteSlot": 1_000_000 }]),
        ),
        "surfnet_timeTravel",
    );
    assert_rejected(
        call(
            &surfnet,
            "surfnet_enableCheatcode",
            serde_json::json!([["surfnet_timeTravel"]]),
        ),
        "re-enabling surfnet_timeTravel",
    );
    assert_refused(
        call(
            &surfnet,
            "surfnet_timeTravel",
            serde_json::json!([{ "absoluteSlot": 1_000_000 }]),
        ),
        "surfnet_timeTravel after re-enabling it",
    );
    surfnet.cheatcodes().fund_sol(&target, 5).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn locking_an_unknown_cheatcode_fails_startup() {
    let result = Surfnet::builder()
        .lock_cheatcodes(CheatcodeFilter::list(["surfnet_setAcount"]))
        .start()
        .await;
    match result {
        Err(SurfnetError::Startup(message)) => {
            assert!(message.contains("surfnet_setAcount"), "{message}")
        }
        Err(other) => panic!("expected a startup error, got {other}"),
        Ok(_) => panic!("a lock naming a typo must not start a surfnet"),
    }
}
