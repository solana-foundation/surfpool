//! OpenBook v2 integration tests.
//!
//! These fetch the real accounts from mainnet rather than embedding captured copies, so they need
//! a network connection and are compiled only behind a feature:
//!
//! ```text
//! cargo test -p surfpool-core --features integration-tests openbook
//! ```
//!
//! Set `SURFPOOL_TEST_RPC_URL` to use a private endpoint if the public one rate-limits.
//!
//! The bundled IDL was converted from OpenBook's legacy Anchor IDL, so these guard what the
//! conversion could get wrong: discriminators, field order and sizes against the live layout. The
//! last test runs the deployed program against an overridden market.

use std::{collections::HashMap, ops::Range};

use solana_account::Account;
use solana_clock::Clock;
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction, error::InstructionError};
use solana_keypair::Keypair;
use solana_program_pack::Pack;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;
use solana_transaction_error::TransactionError;

use crate::{
    scenarios::TemplateRegistry,
    surfnet::{GetAccountResult, remote::SurfnetRemoteClient, svm::SurfnetSvm},
    tests::helpers::diff_indices,
};

const RPC_URL_ENV: &str = "SURFPOOL_TEST_RPC_URL";
const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";

/// Busy SOL, BTC, ETH, WIF and BONK markets against USDC, as samples of the live `Market` layout.
const MARKETS: &[&str] = &[
    SOL_USDC,
    "2HXgKaXKsMUEzQaSBZiXSd54eMHaS3roiefyGWtkW97W",
    "AT1R2jUNb9iTo4EaRfKSTPiNTX4Jb64KSwnVmig6Hu4t",
    "CwGmEwYFo7u5D7vghGwtcCbRToWosytaZa3Ys3JAto6J",
    "D3gZwng2MgZGjktYcKpbR8Bz8653i4qCgzHCf5E4TcZb",
];
/// The SOL/USDC market the overrides and the deployed program run against.
const SOL_USDC: &str = "AFgkED1FUVfBe2trPUDqSqK9QKd4stJrfzq5q1RwAFTa";

/// An open orders account on the SOL/USDC market with a resting bid, so its order slots are
/// populated.
const OPEN_ORDERS: &str = "BAonE8ZAd8bTwZjPqKUz1r3cm38kFU45Smc4XnrcwvgR";

/// `8 + size_of::<Market>()`, which the program asserts is 840.
const MARKET_LEN: usize = 848;
/// `8 + size_of::<OpenOrdersAccount>()`, which the program asserts is 1256.
const OPEN_ORDERS_LEN: usize = 1264;

// Market field offsets, discriminator included.
const TIME_EXPIRY: Range<usize> = 48..56;
const MAX_STALENESS_SLOTS: Range<usize> = 368..376;
const MAKER_FEE: Range<usize> = 480..488;
const TAKER_FEE: Range<usize> = 488..496;
const FEES_AVAILABLE: Range<usize> = 536..544;
const BASE_DEPOSIT_TOTAL: Range<usize> = 672..680;
const QUOTE_DEPOSIT_TOTAL: Range<usize> = 712..720;

// OpenOrdersAccount field offsets: `position` starts at 144.
const BIDS_BASE_LOTS: Range<usize> = 144..152;
const ASKS_BASE_LOTS: Range<usize> = 152..160;
const BASE_FREE_NATIVE: Range<usize> = 160..168;
const QUOTE_FREE_NATIVE: Range<usize> = 168..176;
const LOCKED_MAKER_FEES: Range<usize> = 176..184;
const BIDS_QUOTE_LOTS: Range<usize> = 232..240;

/// The account data of [`fetch_accounts`].
async fn fetch(addresses: &[&str]) -> Vec<Vec<u8>> {
    fetch_accounts(addresses)
        .await
        .into_iter()
        .map(|account| account.data)
        .collect()
}

/// Fetches the accounts in one request, so every account returned is from the same slot.
async fn fetch_accounts(addresses: &[&str]) -> Vec<Account> {
    let client = SurfnetRemoteClient::new(
        std::env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_RPC_URL.to_string()),
    );
    let pubkeys: Vec<Pubkey> = addresses
        .iter()
        .map(|a| Pubkey::from_str_const(a))
        .collect();

    client
        .get_multiple_accounts(&pubkeys, CommitmentConfig::confirmed())
        .await
        .unwrap_or_else(|e| panic!("failed to fetch {addresses:?} from mainnet: {e}"))
        .into_iter()
        .zip(addresses)
        .map(|(result, address)| match result {
            GetAccountResult::FoundAccount(_, account, _)
            | GetAccountResult::FoundCoupledAccount((_, account), _, _) => account,
            GetAccountResult::None(_) => {
                panic!("{address} no longer exists on mainnet; the test needs a new address")
            }
        })
        .collect()
}

fn read_i64(data: &[u8], range: Range<usize>) -> i64 {
    i64::from_le_bytes(data[range].try_into().unwrap())
}

fn read_u64(data: &[u8], range: Range<usize>) -> u64 {
    u64::from_le_bytes(data[range].try_into().unwrap())
}

/// Every target field must have changed, and every byte that changed must belong to one of them.
fn assert_only_fields_changed(forged: &[u8], original: &[u8], fields: &[Range<usize>]) {
    assert_eq!(
        forged.len(),
        original.len(),
        "the override changed the size"
    );
    for field in fields {
        assert_ne!(
            forged[field.clone()],
            original[field.clone()],
            "the override left the field at {field:?} unchanged"
        );
    }
    let stray: Vec<usize> = diff_indices(forged, original)
        .into_iter()
        .filter(|i| !fields.iter().any(|field| field.contains(i)))
        .collect();
    assert!(
        stray.is_empty(),
        "the override changed {} byte(s) outside its fields, first at {:?}",
        stray.len(),
        stray.first()
    );
}

/// A failure here means the converted IDL disagrees with the live on-chain layout.
#[tokio::test]
async fn real_mainnet_accounts_round_trip_unchanged() {
    let mut cases: Vec<(&str, &str, &str, usize)> = MARKETS
        .iter()
        .map(|address| ("openbook-v2-market-custom", "Market", *address, MARKET_LEN))
        .collect();
    cases.push((
        "openbook-v2-open-orders",
        "OpenOrdersAccount",
        OPEN_ORDERS,
        OPEN_ORDERS_LEN,
    ));

    let addresses: Vec<&str> = cases.iter().map(|(_, _, a, _)| *a).collect();
    let accounts = fetch(&addresses).await;

    let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    let registry = TemplateRegistry::new();
    let pubkey = Pubkey::new_unique();

    for ((template_id, account_name, address, len), data) in cases.iter().zip(&accounts) {
        let template = registry
            .get(template_id)
            .unwrap_or_else(|| panic!("template {template_id} should exist"));

        assert_eq!(data.len(), *len, "{address} is not a {account_name}");
        let account_def = template
            .idl()
            .accounts
            .iter()
            .find(|a| a.name == *account_name)
            .unwrap_or_else(|| panic!("{account_name} not in the IDL"));
        assert_eq!(
            &data[..8],
            account_def.discriminator.as_slice(),
            "{address} discriminator does not match the IDL's {account_name}"
        );

        let forged = surfnet_svm
            .get_forged_account_data(&pubkey, data, template.idl(), &HashMap::new())
            .unwrap_or_else(|e| {
                panic!("live {address} failed to decode/re-encode with the bundled IDL: {e}")
            });
        let diffs = diff_indices(&forged, data);
        assert!(
            forged.len() == data.len() && diffs.is_empty(),
            "live {} was altered by a no-op round-trip at {} byte(s), first at {:?}",
            address,
            diffs.len(),
            diffs.first()
        );
    }
}

/// Every field the market template offers, written into the live SOL/USDC market.
#[tokio::test]
async fn market_override_touches_only_target_fields() {
    let data = fetch(&[SOL_USDC]).await.remove(0);

    let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    let registry = TemplateRegistry::new();
    let template = registry.get("openbook-v2-market-custom").unwrap();

    // Offset from the live values, so each write changes its field whatever the market holds.
    let base_deposit_total = read_u64(&data, BASE_DEPOSIT_TOTAL) + 987_654_321;
    let quote_deposit_total = read_u64(&data, QUOTE_DEPOSIT_TOTAL) + 123_456_789;
    let fees_available = read_u64(&data, FEES_AVAILABLE) + 55_555;
    let values = [
        ("maker_fee", serde_json::json!(-77), MAKER_FEE),
        ("taker_fee", serde_json::json!(4242), TAKER_FEE),
        ("time_expiry", serde_json::json!(-1), TIME_EXPIRY),
        (
            "base_deposit_total",
            serde_json::json!(base_deposit_total),
            BASE_DEPOSIT_TOTAL,
        ),
        (
            "quote_deposit_total",
            serde_json::json!(quote_deposit_total),
            QUOTE_DEPOSIT_TOTAL,
        ),
        (
            "fees_available",
            serde_json::json!(fees_available),
            FEES_AVAILABLE,
        ),
        (
            "oracle_config.max_staleness_slots",
            serde_json::json!(77),
            MAX_STALENESS_SLOTS,
        ),
    ];
    let template_paths: Vec<&str> = template
        .properties
        .iter()
        .map(|p| p.path.as_str())
        .collect();
    let written: Vec<&str> = values.iter().map(|(path, _, _)| *path).collect();
    assert_eq!(
        written, template_paths,
        "the test writes every template field"
    );

    let forged = surfnet_svm
        .get_forged_account_data(
            &Pubkey::new_unique(),
            &data,
            template.idl(),
            &values
                .iter()
                .map(|(path, value, _)| (path.to_string(), value.clone()))
                .collect(),
        )
        .expect("override on the live SOL/USDC market");

    let fields: Vec<Range<usize>> = values.iter().map(|(_, _, field)| field.clone()).collect();
    assert_only_fields_changed(&forged, &data, &fields);
    assert_eq!(read_i64(&forged, MAKER_FEE), -77);
    assert_eq!(read_i64(&forged, TAKER_FEE), 4242);
    assert_eq!(read_i64(&forged, TIME_EXPIRY), -1, "the market is expired");
    assert_eq!(read_u64(&forged, BASE_DEPOSIT_TOTAL), base_deposit_total);
    assert_eq!(read_u64(&forged, QUOTE_DEPOSIT_TOTAL), quote_deposit_total);
    assert_eq!(read_u64(&forged, FEES_AVAILABLE), fees_available);
    assert_eq!(read_i64(&forged, MAX_STALENESS_SLOTS), 77);
}

/// Every field the open orders template offers, inside the nested `position`, written into a live
/// open orders account.
#[tokio::test]
async fn open_orders_override_touches_only_target_fields() {
    let data = fetch(&[OPEN_ORDERS]).await.remove(0);

    let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    let registry = TemplateRegistry::new();
    let template = registry.get("openbook-v2-open-orders").unwrap();

    let base_free = read_u64(&data, BASE_FREE_NATIVE) + 1_000_000_000;
    let quote_free = read_u64(&data, QUOTE_FREE_NATIVE) + 5_000_000;
    let bids_base_lots = read_i64(&data, BIDS_BASE_LOTS) + 7;
    let bids_quote_lots = read_i64(&data, BIDS_QUOTE_LOTS) + 13;
    let asks_base_lots = read_i64(&data, ASKS_BASE_LOTS) + 11;
    let locked_maker_fees = read_u64(&data, LOCKED_MAKER_FEES) + 17;
    let values = [
        (
            "position.base_free_native",
            serde_json::json!(base_free),
            BASE_FREE_NATIVE,
        ),
        (
            "position.quote_free_native",
            serde_json::json!(quote_free),
            QUOTE_FREE_NATIVE,
        ),
        (
            "position.bids_base_lots",
            serde_json::json!(bids_base_lots),
            BIDS_BASE_LOTS,
        ),
        (
            "position.bids_quote_lots",
            serde_json::json!(bids_quote_lots),
            BIDS_QUOTE_LOTS,
        ),
        (
            "position.asks_base_lots",
            serde_json::json!(asks_base_lots),
            ASKS_BASE_LOTS,
        ),
        (
            "position.locked_maker_fees",
            serde_json::json!(locked_maker_fees),
            LOCKED_MAKER_FEES,
        ),
    ];
    let template_paths: Vec<&str> = template
        .properties
        .iter()
        .map(|p| p.path.as_str())
        .collect();
    let written: Vec<&str> = values.iter().map(|(path, _, _)| *path).collect();
    assert_eq!(
        written, template_paths,
        "the test writes every template field"
    );

    let forged = surfnet_svm
        .get_forged_account_data(
            &Pubkey::new_unique(),
            &data,
            template.idl(),
            &values
                .iter()
                .map(|(path, value, _)| (path.to_string(), value.clone()))
                .collect(),
        )
        .expect("override on the live open orders account");

    let fields: Vec<Range<usize>> = values.iter().map(|(_, _, field)| field.clone()).collect();
    assert_only_fields_changed(&forged, &data, &fields);
    assert_eq!(read_u64(&forged, BASE_FREE_NATIVE), base_free);
    assert_eq!(read_u64(&forged, QUOTE_FREE_NATIVE), quote_free);
    assert_eq!(read_i64(&forged, BIDS_BASE_LOTS), bids_base_lots);
    assert_eq!(read_i64(&forged, BIDS_QUOTE_LOTS), bids_quote_lots);
    assert_eq!(read_i64(&forged, ASKS_BASE_LOTS), asks_base_lots);
    assert_eq!(read_u64(&forged, LOCKED_MAKER_FEES), locked_maker_fees);
}

const PROGRAM_ID: Pubkey = Pubkey::from_str_const("opnb2LAfJYbRMAHHvqjCwQxanZn7ReEHp1k81EohpZb");
/// `MarketHasExpired` in OpenBook's `OpenBookError`.
const MARKET_HAS_EXPIRED: u32 = 6030;

/// Reads the pubkey a market stores at `offset`.
fn market_key(market: &[u8], offset: usize) -> Pubkey {
    Pubkey::new_from_array(market[offset..offset + 32].try_into().unwrap())
}

/// A token account of `mint` owned by `owner`.
fn token_account(mint: Pubkey, owner: Pubkey, amount: u64) -> Account {
    let mut data = vec![0; spl_token_interface::state::Account::LEN];
    spl_token_interface::state::Account {
        mint,
        owner,
        amount,
        state: spl_token_interface::state::AccountState::Initialized,
        ..Default::default()
    }
    .pack_into_slice(&mut data);
    Account {
        lamports: 2_039_280,
        data,
        owner: spl_token_interface::id(),
        executable: false,
        rent_epoch: 0,
    }
}

/// A small immediate-or-cancel buy on `market`, which the deployed program can always accept: the
/// limit price is far above the book, and an empty book leaves it unfilled rather than failing.
fn take_order(
    signer: Pubkey,
    market: Pubkey,
    market_data: &[u8],
    user: [Pubkey; 2],
) -> Instruction {
    // place_take_order's discriminator, then PlaceTakeOrderArgs: side Bid, price_lots,
    // max_base_lots, max_quote_lots_including_fees, order_type ImmediateOrCancel, limit.
    let mut data = vec![3, 44, 71, 3, 26, 199, 203, 85, 0];
    data.extend_from_slice(&10_000_000i64.to_le_bytes());
    data.extend_from_slice(&10i64.to_le_bytes());
    data.extend_from_slice(&10_000_000i64.to_le_bytes());
    data.extend_from_slice(&[1, 10]);

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(signer, true),
            AccountMeta::new(signer, true),
            AccountMeta::new(market, false),
            AccountMeta::new_readonly(market_key(market_data, 16), false),
            AccountMeta::new(market_key(market_data, 200), false),
            AccountMeta::new(market_key(market_data, 232), false),
            AccountMeta::new(market_key(market_data, 640), false),
            AccountMeta::new(market_key(market_data, 680), false),
            AccountMeta::new(market_key(market_data, 264), false),
            AccountMeta::new(user[0], false),
            AccountMeta::new(user[1], false),
            // Anchor reads the program id as an absent optional account: neither oracle, nor an
            // open orders admin, is set on the SOL/USDC market.
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(spl_token_interface::id(), false),
            AccountMeta::new_readonly(solana_sdk_ids::system_program::id(), false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
        ],
        data,
    }
}

/// The deployed OpenBook program, given the live SOL/USDC market with and without the custom
/// template's expiry override: it takes the order on the live market and refuses it as expired on
/// the overridden one, so the override is what the program reads.
#[tokio::test]
async fn the_deployed_program_refuses_orders_on_an_expired_market() {
    let market_address = SOL_USDC;
    let market = Pubkey::from_str_const(market_address);
    let program = fetch_accounts(&[&PROGRAM_ID.to_string()]).await.remove(0);
    // An upgradeable program's account holds its ProgramData address after a 4-byte tag; the ELF
    // starts 45 bytes into the ProgramData account, past the loader's header.
    let programdata = Pubkey::new_from_array(program.data[4..36].try_into().unwrap());

    let market_account = fetch_accounts(&[market_address]).await.remove(0);
    // The book sides, event heap and vaults. The market authority is a PDA that only signs for the
    // vaults and holds no account, so there is nothing to fetch for it.
    let linked: Vec<String> = [200, 232, 264, 640, 680]
        .iter()
        .map(|offset| market_key(&market_account.data, *offset).to_string())
        .collect();
    let mut addresses = vec![programdata.to_string()];
    addresses.extend(linked.iter().cloned());
    let addresses: Vec<&str> = addresses.iter().map(String::as_str).collect();
    let mut accounts = fetch_accounts(&addresses).await;
    let elf = accounts.remove(0).data[45..].to_vec();

    let registry = TemplateRegistry::new();
    let template = registry.get("openbook-v2-market-custom").unwrap();
    let signer = Keypair::new();
    let user = [Pubkey::new_unique(), Pubkey::new_unique()];

    let run = |market_data: Vec<u8>| -> Result<(), TransactionError> {
        let (mut svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        svm.inner.set_sysvar(&Clock {
            unix_timestamp: now,
            ..svm.inner.get_sysvar::<Clock>()
        });
        svm.inner.svm.add_program(PROGRAM_ID, &elf).unwrap();
        svm.set_account(
            &market,
            Account {
                data: market_data,
                ..market_account.clone()
            },
        )
        .unwrap();
        for (address, account) in linked.iter().zip(&accounts) {
            svm.set_account(&Pubkey::from_str_const(address), account.clone())
                .unwrap();
        }
        let base_mint = market_key(&market_account.data, 576);
        let quote_mint = market_key(&market_account.data, 608);
        svm.set_account(&user[0], token_account(base_mint, signer.pubkey(), 0))
            .unwrap();
        svm.set_account(
            &user[1],
            token_account(quote_mint, signer.pubkey(), 100_000_000),
        )
        .unwrap();
        svm.inner.airdrop(&signer.pubkey(), 1_000_000_000).unwrap();

        let transaction = Transaction::new_signed_with_payer(
            &[take_order(
                signer.pubkey(),
                market,
                &market_account.data,
                user,
            )],
            Some(&signer.pubkey()),
            &[&signer],
            svm.latest_blockhash(),
        );
        svm.send_transaction(transaction.into(), false, true)
            .map(|_| ())
            .map_err(|failed| failed.err)
    };

    run(market_account.data.clone())
        .unwrap_or_else(|e| panic!("the live market should take the order, got {e:?}"));

    let expired = SurfnetSvm::default()
        .0
        .get_forged_account_data(
            &market,
            &market_account.data,
            template.idl(),
            &HashMap::from([("time_expiry".to_string(), serde_json::json!(-1))]),
        )
        .unwrap();
    assert_eq!(
        run(expired),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::Custom(MARKET_HAS_EXPIRED)
        )),
        "the deployed program should refuse the order as MarketHasExpired"
    );
}
