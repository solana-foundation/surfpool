//! Chainlink integration tests.
//!
//! These read a live data feed and the deployed Store program from mainnet, so they need a
//! network connection and are compiled only behind a feature:
//!
//! ```text
//! cargo test -p surfpool-core --features integration-tests chainlink
//! ```
//!
//! Set `SURFPOOL_TEST_RPC_URL` to use a private endpoint if the public one rate-limits.

use std::collections::HashMap;

use base64::{Engine, prelude::BASE64_STANDARD};
use solana_account::Account;
use solana_account_decoder::{UiAccountData, UiAccountEncoding};
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_loader_v3_interface::state::UpgradeableLoaderState;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;
use surfpool_types::{AccountAddress, DEFAULT_MAINNET_RPC_URL, OverrideInstance, Scenario};

use crate::{
    scenarios::TemplateRegistry,
    surfnet::{locker::SurfnetSvmLocker, remote::SurfnetRemoteClient, svm::SurfnetSvm},
};

const RPC_URL_ENV: &str = "SURFPOOL_TEST_RPC_URL";
const STORE_PROGRAM: Pubkey =
    Pubkey::from_str_const("HEvSKofvBgfaexv23kMabbYqxasxU3mQ4ibBMEmJWHny");
const SOL_USD_FEED: Pubkey = Pubkey::from_str_const("CH31Xns5z3M1cTAbKW34jcxPPciazARpijcHj9rxtemt");
const STORE_PROGRAMDATA: Pubkey =
    Pubkey::from_str_const("6TDapMxCnEzss5YCJZXZtTSHwe6ZJeWikwvPzLnZvq7m");
const STORE_QUERY: [u8; 8] = [39, 251, 130, 159, 46, 136, 164, 169];
/// The `Scope::LatestRoundData` variant of the `query` argument.
const LATEST_ROUND_DATA: u8 = 4;

/// The round chainlink_solana::v2::read_feed_v2 reads: slot, timestamp and answer.
const ROUND_FIELDS: [(usize, usize); 3] = [(200, 8), (208, 4), (216, 16)];

/// Fetches the accounts in one request, so every account returned is from the same slot.
async fn fetch(addresses: &[Pubkey]) -> Vec<Account> {
    SurfnetRemoteClient::new(
        std::env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_MAINNET_RPC_URL.to_string()),
    )
    .get_multiple_accounts(addresses, CommitmentConfig::confirmed())
    .await
    .unwrap_or_else(|e| panic!("failed to fetch {addresses:?} from mainnet: {e}"))
    .into_iter()
    .zip(addresses)
    .map(|(result, address)| {
        result.map_account().unwrap_or_else(|_| {
            panic!("{address} no longer exists on mainnet; the test needs a new address")
        })
    })
    .collect()
}

fn live_round(feed: &Account) -> (u64, u32, i128) {
    let [slot, timestamp, answer] =
        ROUND_FIELDS.map(|(offset, width)| &feed.data[offset..offset + width]);
    (
        u64::from_le_bytes(slot.try_into().unwrap()),
        u32::from_le_bytes(timestamp.try_into().unwrap()),
        i128::from_le_bytes(answer.try_into().unwrap()),
    )
}

/// Registers and plays a one-override scenario at `slot` and returns the feed it produced.
async fn play(
    locker: &SurfnetSvmLocker,
    slot: u64,
    values: [(&str, serde_json::Value); 3],
) -> Account {
    let mut scenario = Scenario::new(
        "Chainlink SOL / USD".to_string(),
        "Rewrite the live round".to_string(),
    );
    scenario.add_override(
        OverrideInstance::new(
            "chainlink-data-feed".to_string(),
            0,
            AccountAddress::Pubkey(SOL_USD_FEED.to_string()),
        )
        .with_values(
            values
                .into_iter()
                .map(|(path, value)| (path.to_string(), value))
                .collect(),
        ),
    );
    locker.register_scenario(scenario, Some(slot)).unwrap();
    locker
        .materialize_overrides_for_slot(&None, slot)
        .await
        .unwrap();
    locker
        .with_svm_reader(|svm| svm.get_account(&SOL_USD_FEED))
        .unwrap()
        .expect("the feed should exist")
}

#[tokio::test(flavor = "multi_thread")]
async fn data_feed_override_writes_only_the_live_round() {
    let live = fetch(&[SOL_USD_FEED]).await.remove(0);
    assert_eq!(live.owner, STORE_PROGRAM);
    assert_eq!(live.data.len(), 248, "a single-round feed is 248 bytes");
    assert_eq!(
        u32::from_le_bytes(live.data[148..152].try_into().unwrap()),
        1,
        "live_length must be 1, the only shape the Chainlink SDK reads"
    );

    let registry = TemplateRegistry::new();
    let template = registry.get("chainlink-data-feed").unwrap();
    // The llm_context finds a feed by the description at byte 106; its example must match.
    assert_eq!(&live.data[106..116], b"SOL / USD\0");
    assert!(
        template
            .llm_context
            .as_deref()
            .unwrap()
            .contains(&BASE64_STANDARD.encode(b"SOL / USD\0"))
    );

    let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
    let identity = svm
        .get_forged_account_data(&SOL_USD_FEED, &live.data, template.idl(), &HashMap::new())
        .unwrap();
    assert_eq!(identity, live.data, "no-op round trip changed bytes");

    let (live_slot, live_timestamp, live_answer) = live_round(&live);
    let parsed = match svm
        .encode_ui_account(
            &SOL_USD_FEED,
            &live,
            UiAccountEncoding::JsonParsed,
            None,
            None,
        )
        .data
    {
        UiAccountData::Json(parsed) => parsed.parsed,
        other => panic!("the feed should decode through the bundled IDL, got {other:?}"),
    };
    let round = &parsed["live"];
    assert_eq!(round["slot"], serde_json::json!(live_slot), "{parsed}");
    assert_eq!(
        round["timestamp"],
        serde_json::json!(live_timestamp),
        "{parsed}"
    );
    assert_eq!(
        round["answer"].to_string().trim_matches('"'),
        live_answer.to_string(),
        "{parsed}"
    );

    let locker = SurfnetSvmLocker::new(svm);
    locker.with_svm_writer(|svm| svm.set_account(&SOL_USD_FEED, live.clone()).unwrap());
    let (slot, timestamp, answer) = (live_slot + 7, live_timestamp + 3, live_answer / 3);
    let forged = play(
        &locker,
        100,
        [
            ("live.slot", serde_json::json!(slot)),
            ("live.timestamp", serde_json::json!(timestamp)),
            ("live.answer", serde_json::json!(answer.to_string())),
        ],
    )
    .await;

    assert_eq!(forged.owner, live.owner);
    assert_eq!(forged.lamports, live.lamports);
    assert_eq!(forged.data.len(), live.data.len());
    assert_eq!(live_round(&forged), (slot, timestamp, answer));
    let outside_round: Vec<usize> = (0..live.data.len())
        .filter(|&i| forged.data[i] != live.data[i])
        .filter(|i| {
            !ROUND_FIELDS
                .iter()
                .any(|(offset, width)| (*offset..offset + width).contains(i))
        })
        .collect();
    assert!(outside_round.is_empty(), "changed {outside_round:?}");
}

/// The Store program's own `query` instruction is the reader every Chainlink client is built on, so
/// it must return the round the template wrote.
#[tokio::test(flavor = "multi_thread")]
async fn store_query_returns_the_overridden_round() {
    let [feed, programdata]: [Account; 2] = fetch(&[SOL_USD_FEED, STORE_PROGRAMDATA])
        .await
        .try_into()
        .unwrap();
    let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
    let locker = SurfnetSvmLocker::new(svm);
    locker.with_svm_writer(|svm| {
        svm.inner
            .svm
            .add_program(
                STORE_PROGRAM,
                &programdata.data[UpgradeableLoaderState::size_of_programdata_metadata()..],
            )
            .unwrap();
        svm.set_account(&SOL_USD_FEED, feed.clone()).unwrap();
    });
    // Returns the Borsh `Round` the program serializes: round_id, slot, timestamp, answer.
    let latest_round = || -> (u32, u64, u32, i128) {
        let payer = Keypair::new();
        let data = locker.with_svm_writer(|svm| {
            svm.inner.airdrop(&payer.pubkey(), 1_000_000_000).unwrap();
            let transaction = Transaction::new_signed_with_payer(
                &[Instruction {
                    program_id: STORE_PROGRAM,
                    accounts: vec![AccountMeta::new_readonly(SOL_USD_FEED, false)],
                    data: [STORE_QUERY.as_slice(), &[LATEST_ROUND_DATA]].concat(),
                }],
                Some(&payer.pubkey()),
                &[&payer],
                svm.inner.svm.latest_blockhash(),
            );
            svm.inner
                .send_transaction(transaction)
                .map(|meta| meta.return_data.data)
                .unwrap_or_else(|failed| {
                    panic!(
                        "query failed: {:?}, logs: {:?}",
                        failed.err, failed.meta.logs
                    )
                })
        });
        (
            u32::from_le_bytes(data[0..4].try_into().unwrap()),
            u64::from_le_bytes(data[4..12].try_into().unwrap()),
            u32::from_le_bytes(data[12..16].try_into().unwrap()),
            i128::from_le_bytes(data[16..32].try_into().unwrap()),
        )
    };

    let round_id = u32::from_le_bytes(feed.data[143..147].try_into().unwrap());
    let (live_slot, live_timestamp, live_answer) = live_round(&feed);
    assert_eq!(
        latest_round(),
        (round_id, live_slot, live_timestamp, live_answer)
    );

    let (slot, timestamp, answer) = (live_slot + 7, live_timestamp + 3, live_answer / 4);
    play(
        &locker,
        100,
        [
            ("live.slot", serde_json::json!(slot)),
            ("live.timestamp", serde_json::json!(timestamp)),
            ("live.answer", serde_json::json!(answer.to_string())),
        ],
    )
    .await;
    assert_eq!(latest_round(), (round_id, slot, timestamp, answer));
}
