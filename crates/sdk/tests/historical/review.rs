use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the same staged backend and cached history as the other historical SDK tests"]
async fn historical_bundle_simulation_preserves_isolation_and_signature_checks() {
    use base64::Engine;
    use serde_json::{Value, json};
    use solana_client::rpc_request::RpcRequest;

    let mut surfnet = Surfnet::builder()
        .historical(options())
        .start()
        .await
        .unwrap();
    let recipient = Keypair::new();
    let destination = Keypair::new().pubkey();
    define(&surfnet, &recipient.pubkey(), 1_000_000);
    define(&surfnet, &destination, 1_000_000);
    let client = surfnet.rpc_client();
    let blockhash = client.get_latest_blockhash().unwrap();
    let encode = |tx: &Transaction| {
        base64::engine::general_purpose::STANDARD.encode(bincode::serialize(tx).unwrap())
    };
    let first = Transaction::new_signed_with_payer(
        &[transfer(
            &surfnet.payer().pubkey(),
            &recipient.pubkey(),
            2_000_000,
        )],
        Some(&surfnet.payer().pubkey()),
        &[surfnet.payer()],
        blockhash,
    );
    let second = Transaction::new_signed_with_payer(
        &[transfer(&recipient.pubkey(), &destination, 2_000_000)],
        Some(&recipient.pubkey()),
        &[&recipient],
        blockhash,
    );
    let simulate = |transactions: Vec<String>, config: Value| {
        client.send::<Value>(
            RpcRequest::Custom {
                method: "simulateBundle",
            },
            json!([{ "encodedTransactions": transactions }, config]),
        )
    };
    let valid = simulate(
        vec![encode(&first), encode(&second)],
        json!({"skipSigVerify":false}),
    );
    let mut unsigned = first.clone();
    unsigned.signatures[0] = Default::default();
    let accepted_unsigned = simulate(vec![encode(&unsigned)], json!({"skipSigVerify":true}));
    let rejected_unsigned = simulate(vec![encode(&unsigned)], json!({"skipSigVerify":false}));
    let slot = client.get_slot().unwrap();
    surfnet.cheatcodes().time_travel_to_slot(slot + 1).unwrap();
    let replacement_hash = client.get_latest_blockhash().unwrap();
    assert_ne!(replacement_hash, blockhash);
    let replaced = simulate(
        vec![encode(&first)],
        json!({"skipSigVerify":true,"replaceRecentBlockhash":true}),
    );
    let live_balance = client.get_balance(&recipient.pubkey()).unwrap();
    let live_destination = client.get_balance(&destination).unwrap();
    surfnet.stop().unwrap();
    let valid = valid.expect("valid historical bundles simulate");
    assert_eq!(valid["value"]["summary"], "succeeded", "{valid}");
    let accepted_unsigned = accepted_unsigned.unwrap();
    assert_eq!(
        accepted_unsigned["value"]["summary"], "succeeded",
        "{accepted_unsigned}"
    );
    let rejected_unsigned = rejected_unsigned.unwrap();
    assert_eq!(
        rejected_unsigned["value"]["transactionResults"][0]["err"], "SignatureFailure",
        "{rejected_unsigned}"
    );
    let replaced = replaced.unwrap();
    assert_eq!(replaced["value"]["summary"], "succeeded", "{replaced}");
    assert_eq!(
        replaced["value"]["transactionResults"][0]["replacementBlockhash"]["blockhash"],
        replacement_hash.to_string()
    );
    assert_eq!(live_balance, 1_000_000);
    assert_eq!(live_destination, 1_000_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the same staged backend and cached history as the other historical SDK tests"]
async fn review_repeated_airdrops_and_absolute_time_travel() {
    let mut surfnet = Surfnet::builder()
        .historical(options())
        .start()
        .await
        .unwrap();
    let recipient = Keypair::new().pubkey();
    define(&surfnet, &recipient, 1_000_000);
    let client = surfnet.rpc_client();
    let slot = client.get_slot().unwrap();
    let first = client.request_airdrop(&recipient, 1_000_000);
    let second = client.request_airdrop(&recipient, 1_000_000);
    let balance = client.get_balance(&recipient);
    let frozen = client.get_slot();
    let records = [&first, &second].map(|result| {
        result.as_ref().ok().map(|signature| {
            client.get_transaction_with_config(
                signature,
                solana_client::rpc_config::RpcTransactionConfig {
                    encoding: Some(serde_json::from_value(serde_json::json!("base64")).unwrap()),
                    max_supported_transaction_version: Some(1),
                    ..Default::default()
                },
            )
        })
    });
    let mut travels = Vec::new();
    for _ in 0..8 {
        let target = client.get_slot().unwrap() + 1;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let results = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    let barrier = barrier.clone();
                    let client = &client;
                    scope.spawn(move || {
                        barrier.wait();
                        client.send::<serde_json::Value>(
                            solana_client::rpc_request::RpcRequest::Custom {
                                method: "surfnet_timeTravel",
                            },
                            serde_json::json!([{"absoluteSlot": target}]),
                        )
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        travels.push((target, client.get_slot().unwrap(), results));
    }
    let block = client.get_block_with_config(
        slot,
        solana_client::rpc_config::RpcBlockConfig {
            encoding: Some(serde_json::from_value(serde_json::json!("base64")).unwrap()),
            max_supported_transaction_version: Some(1),
            ..Default::default()
        },
    );
    surfnet
        .stop()
        .expect("stop before checking results, including the unfixed implementation");
    eprintln!(
        "airdrop results: {first:?}, {second:?}; travel targets/final slots: {:?}",
        travels
            .iter()
            .map(|(target, slot, _)| (*target, *slot))
            .collect::<Vec<_>>()
    );
    let first = first.expect("first airdrop");
    let second = second.expect("repeated identical airdrop");
    assert_ne!(first, second);
    for record in records {
        let tx = record
            .unwrap()
            .unwrap()
            .transaction
            .transaction
            .decode()
            .unwrap();
        assert_eq!(
            tx.verify_with_results(),
            vec![true],
            "returned airdrop bytes were not signed"
        );
    }
    let block_transactions: Vec<_> = block
        .unwrap()
        .transactions
        .unwrap()
        .into_iter()
        .map(|entry| entry.transaction.decode().unwrap())
        .collect();
    for signature in [first, second] {
        let tx = block_transactions
            .iter()
            .find(|tx| tx.signatures.first() == Some(&signature))
            .expect("confirmed block includes each airdrop");
        assert_eq!(
            tx.verify_with_results(),
            vec![true],
            "block airdrop bytes were not signed"
        );
    }
    assert_eq!(balance.unwrap(), 3_000_000);
    assert_eq!(
        frozen.unwrap(),
        slot,
        "airdrop must not move the user's clock"
    );
    for (target, final_slot, results) in travels {
        assert_eq!(
            final_slot, target,
            "concurrent time travel overshot its absolute target"
        );
        for result in results {
            match result {
                Ok(result) => assert_eq!(result["absoluteSlot"].as_u64(), Some(target)),
                Err(error) => assert!(
                    error.to_string().contains("UNSUPPORTED_IN_HISTORICAL_MODE"),
                    "{error}"
                ),
            }
        }
    }
}
