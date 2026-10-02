//! SDK-started surfnets have no startup tasks, so `Surfnet::start` seals an
//! empty plan up front. An unsealed plan would project a forever-pending
//! `surfpool-startup` execution into `getSurfnetInfo`, starving
//! readiness-checking clients (legacy Anchor's readiness loop has no
//! timeout); this test pins the seal in place.

use std::{
    io::{Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

use solana_message::Message;
use solana_signer::Signer;
use solana_system_interface::instruction as system_instruction;
use solana_transaction::Transaction;
use surfpool_sdk::{BlockProductionMode, Keypair, Surfnet};

/// Minimal JSON-RPC POST over a raw socket: HTTP/1.0 so the response is
/// unchunked and terminated by connection close.
fn get_surfnet_info(rpc_url: &str) -> serde_json::Value {
    let address = rpc_url.trim_start_matches("http://");
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"surfnet_getSurfnetInfo"}"#;
    let request = format!(
        "POST / HTTP/1.0\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let attempt = TcpStream::connect(address).and_then(|mut stream| {
            stream.write_all(request.as_bytes())?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            Ok(response)
        });
        match attempt {
            Ok(response) => {
                let json_start = response.find("\r\n\r\n").expect("malformed HTTP response") + 4;
                return serde_json::from_str(&response[json_start..])
                    .expect("response body should be JSON");
            }
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "getSurfnetInfo unreachable at {address}: {error}"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

// Multi-threaded flavor: Surfnet::start uses a blocking RpcClient while
// waiting for startup airdrops, which panics on a current-thread runtime.
#[tokio::test(flavor = "multi_thread")]
async fn sdk_surfnets_report_a_ready_startup() {
    let mut surfnet = Surfnet::start().await.expect("surfnet should start");

    let info = get_surfnet_info(surfnet.rpc_url());
    let value = &info["result"]["value"];
    assert_eq!(
        value["startup"]["phase"], "ready",
        "startup should be sealed and ready: {info}"
    );
    assert_eq!(value["startup"]["planSealed"], true);
    // The legacy-Anchor-visible part: no phantom pending execution.
    assert_eq!(
        value["runbookExecutions"]
            .as_array()
            .expect("runbookExecutions should be an array")
            .len(),
        0,
        "no compat entry should remain once startup is ready: {info}"
    );

    surfnet.stop().expect("surfnet should stop cleanly");
}

// Regression #814: `RpcClient::new` waits for finalized commitment, while the
// SDK defaults to transaction-triggered blocks. The submitted transaction must
// therefore drive enough block production for the client to observe finalization.
#[tokio::test(flavor = "multi_thread")]
async fn sdk_transaction_mode_finalizes_submitted_transactions() {
    let mut surfnet = Surfnet::builder()
        .offline(true)
        .payer(Keypair::new_from_array([0xA5; 32]))
        .block_production_mode(BlockProductionMode::Transaction)
        .start()
        .await
        .expect("surfnet should start");
    let rpc = surfnet.rpc_client();
    let payer = surfnet.payer().insecure_clone();
    let recipient = solana_pubkey::Pubkey::new_unique();

    let blockhash = rpc.get_latest_blockhash().expect("latest blockhash");
    let message = Message::new(
        &[system_instruction::transfer(
            &payer.pubkey(),
            &recipient,
            1_000_000,
        )],
        Some(&payer.pubkey()),
    );
    let mut transaction = Transaction::new_unsigned(message);
    transaction
        .try_sign(&[&payer], blockhash)
        .expect("transaction should sign");
    let signature = transaction.signatures[0];

    let confirmation = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || rpc.send_and_confirm_transaction(&transaction)),
    )
    .await;

    let observed_status = surfnet
        .rpc_client()
        .get_signature_statuses(&[signature])
        .expect("signature status lookup should succeed")
        .value
        .into_iter()
        .next()
        .flatten();
    surfnet.stop().expect("surfnet should stop cleanly");

    let confirmation = confirmation
        .unwrap_or_else(|_| {
            panic!(
                "transaction-mode confirmation should not hang; observed status: {observed_status:?}"
            )
        })
        .expect("confirmation task should not panic")
        .expect("transaction should finalize");
    assert_eq!(confirmation, signature);
    assert!(
        observed_status.as_ref().is_some_and(|status| status
            .satisfies_commitment(solana_commitment_config::CommitmentConfig::finalized())),
        "transaction did not finalize; observed status: {observed_status:?}"
    );
}
