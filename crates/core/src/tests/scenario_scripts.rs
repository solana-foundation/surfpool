//! Scenario scripts on a running surfnet: registered over JSON-RPC with their scenario, and
//! called by the clock as it produces slots. The engine's own tests are in
//! `crate::scenarios::script`.

use std::time::Duration;

use serde_json::json;
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_request::RpcRequest};
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_system_interface::instruction as system_instruction;
use solana_transaction::versioned::VersionedTransaction;
use test_case::test_case;

use super::integration::start_surfnet;
use crate::{storage::tests::TestType, surfnet::locker::SurfnetSvmLocker};

/// A scenario holding `scripts`, as a client sends it.
fn scenario(scripts: serde_json::Value) -> serde_json::Value {
    json!({
        "id": "test",
        "name": "test",
        "description": "",
        "overrides": [],
        "tags": [],
        "scripts": scripts,
    })
}

/// A script that counts its calls in `counter`'s lamports.
fn counting(id: &str, counter: &Pubkey) -> serde_json::Value {
    json!({
        "id": id,
        "name": id,
        "source": format!(
            r#"n = 0
            function on_slot(ctx)
              n = n + 1
              ctx.set_account("{counter}", {{ lamports = n }})
            end"#
        ),
    })
}

async fn cheatcode(
    rpc: &RpcClient,
    method: &'static str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    rpc.send(RpcRequest::Custom { method }, params)
        .await
        .map_err(|e| e.to_string())
}

fn lamports(locker: &SurfnetSvmLocker, pubkey: &Pubkey) -> u64 {
    locker
        .with_svm_reader(|svm| svm.get_account(pubkey))
        .unwrap()
        .map_or(0, |account| account.lamports)
}

fn scripts(locker: &SurfnetSvmLocker) -> usize {
    locker.with_svm_reader(|svm| svm.scenario_scripts.len())
}

/// Waits until `done` holds, for at most ten seconds.
async fn eventually(what: &str, done: impl Fn() -> bool) {
    for _ in 0..1_000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn transfer(rpc: &RpcClient, payer: &Keypair, recipient: &Pubkey, lamports: u64) {
    let blockhash = rpc.get_latest_blockhash().await.unwrap();
    let message = Message::new_with_blockhash(
        &[system_instruction::transfer(
            &payer.pubkey(),
            recipient,
            lamports,
        )],
        Some(&payer.pubkey()),
        &blockhash,
    );
    let transaction =
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[payer]).unwrap();
    rpc.send_and_confirm_transaction(&transaction)
        .await
        .unwrap();
}

/// The whole path a client takes: a scenario with a script sent to `surfnet_registerScenario`,
/// then a transaction sent to the RPC. The clock produces the slots; the script sees what the
/// transaction did and writes its answer.
#[test_case(TestType::no_db(); "with no db")]
#[test_case(TestType::in_memory(); "with in-memory sqlite db")]
#[test_case(TestType::sqlite(); "with on-disk sqlite db")]
#[tokio::test(flavor = "multi_thread")]
async fn a_script_registered_over_rpc_reacts_to_a_transaction(test_type: TestType) {
    let payer = Keypair::new();
    let (url, locker, _runloop) = start_surfnet(vec![payer.pubkey()], None, test_type).unwrap();
    let rpc = RpcClient::new(url);
    let (recipient, mirror) = (Pubkey::new_unique(), Pubkey::new_unique());

    cheatcode(
        &rpc,
        "surfnet_registerScenario",
        json!([scenario(json!([{
            "id": "mirror",
            "name": "mirror",
            "source": format!(
                r#"function on_slot(ctx)
                     local paid = ctx.account("{recipient}")
                     if paid then ctx.set_account("{mirror}", {{ lamports = 2 * paid.lamports }}) end
                   end"#
            ),
        }]))]),
    )
    .await
    .unwrap();
    assert_eq!(scripts(&locker), 1);

    // Slots pass and nothing was paid: the script has nothing to answer.
    let start = locker.get_latest_absolute_slot();
    eventually("slots to pass", || {
        locker.get_latest_absolute_slot() > start + 20
    })
    .await;
    assert_eq!(rpc.get_balance(&mirror).await.unwrap(), 0);

    transfer(&rpc, &payer, &recipient, 1_000_000).await;
    eventually("the script to see the transfer", || {
        lamports(&locker, &mirror) == 2_000_000
    })
    .await;
    // A client reads the script's write as any account.
    assert_eq!(rpc.get_balance(&mirror).await.unwrap(), 2_000_000);

    transfer(&rpc, &payer, &recipient, 500_000).await;
    eventually("the script to see the second transfer", || {
        lamports(&locker, &mirror) == 3_000_000
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scenario_whose_script_does_not_load_is_refused_over_rpc() {
    let (url, locker, _runloop) = start_surfnet(vec![], None, TestType::no_db()).unwrap();
    let rpc = RpcClient::new(url);
    let counter = Pubkey::new_unique();

    for (source, message) in [
        ("function on_slot(", "expected"),
        ("x = 1", "defines no on_slot function"),
        ("error('at load')", "at load"),
        ("while true do end", "limit of"),
    ] {
        let refused = cheatcode(
            &rpc,
            "surfnet_registerScenario",
            json!([scenario(json!([
                counting("good", &counter),
                { "id": "bad", "name": "bad", "source": source },
            ]))]),
        )
        .await
        .unwrap_err();
        assert!(refused.contains(message), "{source}: {refused}");
        // Nothing of the scenario registered, the good script included.
        assert_eq!(scripts(&locker), 0, "{source}");
    }

    // A script field the type does not know is a client's mistake, not a silent default.
    let wrong_type = cheatcode(
        &rpc,
        "surfnet_registerScenario",
        json!([scenario(json!([{ "id": "a", "name": "a", "source": 5 }]))]),
    )
    .await;
    assert!(wrong_type.is_err());
    assert_eq!(scripts(&locker), 0);
    assert_eq!(lamports(&locker, &counter), 0);
}

/// A paused clock produces no slots, so it calls no script.
#[tokio::test(flavor = "multi_thread")]
async fn a_paused_clock_calls_no_script() {
    let (url, locker, _runloop) = start_surfnet(vec![], None, TestType::no_db()).unwrap();
    let rpc = RpcClient::new(url);
    let counter = Pubkey::new_unique();

    cheatcode(&rpc, "surfnet_pauseClock", json!([]))
        .await
        .unwrap();
    cheatcode(
        &rpc,
        "surfnet_registerScenario",
        json!([scenario(json!([counting("count", &counter)]))]),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(lamports(&locker, &counter), 0);

    cheatcode(&rpc, "surfnet_resumeClock", json!([]))
        .await
        .unwrap();
    eventually("the script to run again", || {
        lamports(&locker, &counter) > 10
    })
    .await;
}

/// A time travel skips slots. A script whose interval ends inside the jump is removed without
/// being called for the slots it never saw; one without an end goes on from the landing slot.
#[tokio::test(flavor = "multi_thread")]
async fn a_time_travel_ends_the_intervals_it_jumps_over() {
    let (url, locker, _runloop) = start_surfnet(vec![], None, TestType::no_db()).unwrap();
    let rpc = RpcClient::new(url);
    let (short, long, landed) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );

    cheatcode(&rpc, "surfnet_pauseClock", json!([]))
        .await
        .unwrap();
    let mut ends = counting("short", &short);
    ends["activeUntil"] = json!(2_000);
    let slot_of = json!({
        "id": "slot",
        "name": "slot",
        "source": format!(
            r#"function on_slot(ctx) ctx.set_account("{landed}", {{ lamports = ctx.slot }}) end"#
        ),
    });
    cheatcode(
        &rpc,
        "surfnet_registerScenario",
        json!([scenario(json!([ends, counting("long", &long), slot_of]))]),
    )
    .await
    .unwrap();
    let base = locker.get_latest_absolute_slot();
    let target = base + 50_000;
    cheatcode(
        &rpc,
        "surfnet_timeTravel",
        json!([{ "absoluteSlot": target }]),
    )
    .await
    .unwrap();
    cheatcode(&rpc, "surfnet_resumeClock", json!([]))
        .await
        .unwrap();

    eventually("the long script to run past the landing", || {
        lamports(&locker, &landed) > target
    })
    .await;
    // The short one saw at most the block the travel closed, and is gone.
    assert!(lamports(&locker, &short) <= 1);
    assert_eq!(scripts(&locker), 2);
    assert!(lamports(&locker, &long) >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_network_reset_over_rpc_drops_the_scripts() {
    let (url, locker, _runloop) = start_surfnet(vec![], None, TestType::no_db()).unwrap();
    let rpc = RpcClient::new(url);
    let counter = Pubkey::new_unique();
    cheatcode(
        &rpc,
        "surfnet_registerScenario",
        json!([scenario(json!([counting("count", &counter)]))]),
    )
    .await
    .unwrap();
    eventually("the script to run", || lamports(&locker, &counter) > 5).await;

    cheatcode(&rpc, "surfnet_resetNetwork", json!([]))
        .await
        .unwrap();
    assert_eq!(scripts(&locker), 0);
    let start = locker.get_latest_absolute_slot();
    eventually("slots to pass", || {
        locker.get_latest_absolute_slot() > start + 20
    })
    .await;
    assert_eq!(lamports(&locker, &counter), 0);
}

/// Many slots under the clock: the script is called exactly once a slot, in order, and never
/// twice for one slot.
#[tokio::test(flavor = "multi_thread")]
async fn the_clock_calls_a_script_once_per_slot_in_order() {
    let (url, locker, _runloop) = start_surfnet(vec![], None, TestType::no_db()).unwrap();
    let rpc = RpcClient::new(url);
    let (calls, broken) = (Pubkey::new_unique(), Pubkey::new_unique());
    // Registered with the clock paused, so the base slot is the slot the scenario lands in: a
    // slot that passes between the two would be one the script was not there for.
    cheatcode(&rpc, "surfnet_pauseClock", json!([]))
        .await
        .unwrap();
    cheatcode(
        &rpc,
        "surfnet_registerScenario",
        json!([scenario(json!([{
            "id": "order",
            "name": "order",
            "source": format!(
                r#"last, n, gaps = nil, 0, 0
                function on_slot(ctx)
                  if last and ctx.slot ~= last + 1 then gaps = gaps + 1 end
                  last = ctx.slot
                  n = n + 1
                  ctx.set_account("{calls}", {{ lamports = n }})
                  if gaps > 0 then ctx.set_account("{broken}", {{ lamports = gaps }}) end
                end"#
            ),
            "activeUntil": 400,
        }]))]),
    )
    .await
    .unwrap();
    cheatcode(&rpc, "surfnet_resumeClock", json!([]))
        .await
        .unwrap();
    eventually("the interval to end", || scripts(&locker) == 0).await;
    // Slots 1 to 399 of the scenario.
    assert_eq!(lamports(&locker, &calls), 399);
    assert_eq!(lamports(&locker, &broken), 0);
}
