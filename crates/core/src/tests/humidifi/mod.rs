//! HumidiFi v8 raw-layout and deployed-program tests.
//!
//! HumidiFi publishes no IDL and checks its caller through the Instructions sysvar, so the swap
//! proofs replay a captured DFlow route against the deployed ELF in LiteSVM after driving the shipped
//! templates through `materialize_raw_layout` or the Surfnet materializer.

use std::{collections::HashMap, sync::Arc};

use litesvm::LiteSVM;
use solana_account::Account;
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction, error::InstructionError};
use solana_message::Message;
use solana_program_pack::Pack;
use solana_program_runtime::{
    declare_process_instruction, solana_sbpf::program::BuiltinFunctionDefinition,
};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::Transaction;
use solana_transaction_error::TransactionError;
use surfpool_types::{AccountAddress, KeepAliveClock, OverrideInstance, Scenario};

use crate::{
    scenarios::{TemplateRegistry, template_registry},
    surfnet::{GetAccountResult, remote::SurfnetRemoteClient, svm::SurfnetSvm},
    tests::helpers::diff_indices,
};

const RPC_URL_ENV: &str = "SURFPOOL_TEST_RPC_URL";
const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";
const PROGRAM: Pubkey = Pubkey::from_str_const("9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp");
const PROGRAMDATA: Pubkey = Pubkey::from_str_const("G9S64i58RRWJA28vZiNhnP56Ux4Ef7hfMgHNREnZZSom");

const MARKET_SIZE: usize = 1728;
const LAYOUT_TAG: [u8; 8] = [44, 90, 19, 124, 56, 111, 47, 150];
const FAIR_VALUE_MASK: u64 = 0xb957_ed15_dc87_7426;
const SLOT_MASK: u64 = 0x6e9d_e2b3_0b19_f1ea;
const PUBKEY_MASKS: [u64; 4] = [
    0xfb5c_e87a_ae44_3c38,
    0x04a2_1784_51ba_c3c7,
    0x04a1_1787_51b9_c3c6,
    0x04a0_1786_51b8_c3c5,
];
const STALE_QUOTE_ERROR: u32 = 0xfaded;
// The fixtures refuse an empty payout vault themselves; some stopped markets leave it to the token
// transfer, which fails with InsufficientFunds instead.
const EMPTY_PAYOUT_ERROR: u32 = 49;
const FAIR_VALUE: std::ops::Range<usize> = 576..584;
const LAST_UPDATE: std::ops::Range<usize> = 616..624;
const AMOUNT: std::ops::Range<usize> = 64..72;
const MAKER_SPREAD_MASK: u64 = 0x5041_56a2_2548_f8dd;
const TIER_SPREADS: [(usize, u64); 3] = [
    (176, 0x40f8_49d0_0057_07ba),
    (256, 0x40f2_49da_005d_07b4),
    (336, 0x40e4_49cc_004b_07ae),
];
const SPREAD_WORDS: [std::ops::Range<usize>; 4] = [800..808, 176..184, 256..264, 336..344];

async fn fetch(addresses: &[Pubkey]) -> Vec<Account> {
    let client = SurfnetRemoteClient::new(
        std::env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_RPC_URL.to_string()),
    );
    let mut out = Vec::new();
    for batch in addresses.chunks(100) {
        let mut attempt = 0;
        let results = loop {
            match client
                .get_multiple_accounts(batch, CommitmentConfig::confirmed())
                .await
            {
                Ok(v) => break v,
                Err(e) => {
                    attempt += 1;
                    assert!(attempt < 5, "fetch {batch:?}: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(750 * attempt)).await;
                }
            }
        };
        out.extend(
            results
                .into_iter()
                .zip(batch)
                .map(|(result, address)| match result {
                    GetAccountResult::FoundAccount(_, account, _)
                    | GetAccountResult::FoundCoupledAccount((_, account), _, _) => account,
                    GetAccountResult::None(_) => panic!("{address} no longer exists"),
                }),
        );
    }
    out
}

fn word(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn masked_pubkey(data: &[u8], offset: usize) -> Pubkey {
    let mut bytes = [0u8; 32];
    for (i, mask) in PUBKEY_MASKS.iter().enumerate() {
        bytes[i * 8..(i + 1) * 8]
            .copy_from_slice(&(word(data, offset + i * 8) ^ mask).to_le_bytes());
    }
    Pubkey::new_from_array(bytes)
}

fn pubkey_at(data: &[u8], offset: usize) -> Pubkey {
    Pubkey::new_from_array(data[offset..offset + 32].try_into().unwrap())
}

fn amount(data: &[u8]) -> u64 {
    word(data, 64)
}

fn apply_raw(id: &str, data: &[u8], values: &[(&str, serde_json::Value)], slot: u64) -> Vec<u8> {
    let registry = TemplateRegistry::new();
    let t = registry.get(id).unwrap_or_else(|| panic!("missing {id}"));
    assert!(t.raw_layout, "{id} must use raw-layout writes");
    let map = values
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect::<HashMap<_, _>>();
    t.materialize_raw_layout(data, &map, slot)
        .unwrap_or_else(|e| panic!("{id}: {e}"))
}

/// What keep-alive writes to a market republished in `slot`: humidifi-price's shipped import.
fn humidifi_keep_alive(market: &[u8], slot: u64) -> Vec<u8> {
    let registry = template_registry();
    let import = registry
        .get("humidifi-price")
        .and_then(|template| template.keep_alive.first())
        .expect("humidifi-price keeps its market quoting");
    let values = import
        .values_at(&KeepAliveClock {
            slot,
            unix_time_ms: 0,
        })
        .expect("keep-alive values");
    registry
        .keep_alive_template(&import.template)
        .expect("keep-alive template")
        .materialize_raw_layout(market, &values, slot)
        .unwrap_or_else(|e| panic!("{}: {e}", import.template))
}

// A masked write can leave a byte equal to the original, so only "nothing outside moved" holds.
fn assert_only_within(
    before: &[u8],
    after: &[u8],
    ranges: &[std::ops::Range<usize>],
    context: &str,
) {
    assert_eq!(before.len(), after.len(), "{context}: length changed");
    for i in diff_indices(before, after) {
        assert!(
            ranges.iter().any(|r| r.contains(&i)),
            "{context}: byte {i} changed outside {ranges:?}"
        );
    }
}

fn values(entries: &[(&str, serde_json::Value)]) -> HashMap<String, serde_json::Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.clone()))
        .collect()
}

#[derive(Clone, Copy)]
struct MarketDef {
    pair: &'static str,
    market: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
}

/// The markets Studio offers as shortcuts, in its order: the live markets with funded vaults. The
/// behavioural tests replay the ones whose vaults are classic SPL-token accounts.
const FEATURED_MARKETS: [MarketDef; 4] = [
    MarketDef {
        pair: "SOL / USDC",
        market: Pubkey::from_str_const("FksffEqnBRixYGR791Qw2MgdU7zNCpHVFYBL4Fa4qVuH"),
        base_mint: Pubkey::from_str_const("So11111111111111111111111111111111111111112"),
        quote_mint: Pubkey::from_str_const("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
        base_vault: Pubkey::from_str_const("C3FzbX9n1YD2dow2dCmEv5uNyyf22Gb3TLAEqGBhw5fY"),
        quote_vault: Pubkey::from_str_const("3RWFAQBRkNGq7CMGcTLK3kXDgFTe9jgMeFYqk8nHwcWh"),
    },
    MarketDef {
        pair: "PENGU / USDC",
        market: Pubkey::from_str_const("2866MvCKPGz9LdnPcmPueoV3mA2Ac1ceEQ8Xqb9VNefu"),
        base_mint: Pubkey::from_str_const("2zMMhcVQEXDtdE6vsFS7S7D5oUodfJHE8vd1gnBouauv"),
        quote_mint: Pubkey::from_str_const("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
        base_vault: Pubkey::from_str_const("8UwYWkJ78uG7Cy8bZJURzwFfDUTX4oeiAriBQVWwmY8"),
        quote_vault: Pubkey::from_str_const("6iNgLk2CUJYA6ZVTcaTeSQiaaUfUbYgZZShWCSsTTkPn"),
    },
    MarketDef {
        pair: "WET / USDC",
        market: Pubkey::from_str_const("Cc2oMdeNDmZ7ZD8b7igQg1EHjBEinR1Vi6nx2VHpVm6H"),
        base_mint: Pubkey::from_str_const("WETZjtprkDMCcUxPi9PfWnowMRZkiGGHDb9rABuRZ2U"),
        quote_mint: Pubkey::from_str_const("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
        base_vault: Pubkey::from_str_const("7gxM8fzfJ331g1yddEDUDrz4BLwEEPPPcCDV57pDQWyT"),
        quote_vault: Pubkey::from_str_const("F9JdjxCTiyy9xB4BjvGQAzzxetVJQWAjWzAMxzMMD23D"),
    },
    MarketDef {
        pair: "PUMP / USDC",
        market: Pubkey::from_str_const("HyKbc1vUxNL9xJC2Q44qdGiuKC1Ey95KNdokfHivrFnZ"),
        base_mint: Pubkey::from_str_const("pumpCmXqMfrsAkQ5r49WcJnRayYRqmXz6ae8H7H9Dfn"),
        quote_mint: Pubkey::from_str_const("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
        base_vault: Pubkey::from_str_const("7CdAWd32k1c9ywpyErij2xRbBDRCLFqrZAXTUjD3ziek"),
        quote_vault: Pubkey::from_str_const("Cad4XrQrtXdKEhZKtWX5YAkwumUvphZKw1cqpYXYnZDr"),
    },
];

const DFLOW: Pubkey = Pubkey::from_str_const("DF1ow4tspfHX9JwWJsAb9epbkA8hmpSEAtxXy1V27QBH");
const SIGNER0: Pubkey = Pubkey::from_str_const("4HJaX8K9mH9fMLGn4Xc5DjGdjDWXNSnnX5kkXFuzk2ET");
const USER_BASE: Pubkey = Pubkey::from_str_const("CsqNfUnwbVbDQRQFUGCRCik8auK1ExfvwTYvM6Zc1uPe");
const USER_QUOTE: Pubkey = Pubkey::from_str_const("FMc3ZxJSYyT9JuJ21iDzQs5P2wRpS6JSwvT6ZRkYv6J9");
const CLOCK: Pubkey = Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111");
const TOKEN: Pubkey = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const SYS0: Pubkey = Pubkey::from_str_const("8xeaWCsJYxRoudEZGJWURdfrtFhLYZz9b4iHJnW5tb3d");
const AUX12: Pubkey = Pubkey::from_str_const("8vqruQc1wB3YQpaP4fr1woJGULBQG3c7uj8A8nnSWo9");
const VOTE: Pubkey = Pubkey::from_str_const("J1to1yufRnoWn81KYg1XkTWzmKjnYSnmE2VY8DGUJ9Qv");
const ROUTE_STATE: Pubkey = Pubkey::from_str_const("EXNBiVYTJErnLRaz9hae8P4nePswG21qZJZX9wJLUDnY");
const AUX15: Pubkey = Pubkey::from_str_const("7Qca6CS6sExGXKh3UmJ5fpapc3YFwScVpjfhmi4ScWUM");
const AUX16: Pubkey = Pubkey::from_str_const("6iL7bcqz6tmLo821xcvgDDSvrPh7knjoMQpyZEuSxML3");
const AUX17: Pubkey = Pubkey::from_str_const("1kUMdzAeH1uEdNch7ZJrtfvSo351p3b2DJ6qf5TUWhC");

const INPUT_QUOTE_AMOUNT: u64 = 2_427_890;
// Obfuscated by HumidiFi; it encodes the fixed quote input above.
const SWAP_DATA_HEX: &str = "6f86a350a21ac2b9c9f40bffe3baeac339ff2dffe0bae9c309";

// Captured from mainnet transaction 4SHLBz1tXdCqCkgg… (slot 459573239), a Jupiter route selling
// SOL for USDC through HumidiFi's SOL / USDC market. Obfuscated by HumidiFi; it encodes the fixed
// base input below, so the sale replays on SOL / USDC only.
const SELL_SWAP_DATA_HEX: &str = "13926b06e3baebc33aff2effe3baeac338ff2dffe0bae9c3876236eff7aee95fc2fcc20c19a3bb3f93711dc2a70e4d9300636b99bf2cc11b78e8bcc46918f44f331e111cd77c1725676f51b26f047b79504ef071faa2b9a9eaf8076446a3b2f907b0e849f2e3bcaf37ff22ffefbae6c335ff21ffecbae5c3c57844e4edbae4c36a775be4f2bafbc319";
const INPUT_BASE_AMOUNT: u64 = 8_476_978_473;
const JUPITER: Pubkey = Pubkey::from_str_const("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");
// HumidiFi checks the sale's next four accounts by address: a fresh address for any of them fails
// the sale. The seller's accounts are not bound to the captured data, so the sale reuses SIGNER0,
// USER_BASE and USER_QUOTE.
/// The shared account holding the fallback spread a Jupiter route pays (README, Troubleshooting).
const FALLBACK_SPREAD_STATE: Pubkey =
    Pubkey::from_str_const("Fnf3RYYN4uHEzUnCNAbeN4LrezyAbSJZ6Digr2ActVXC");
const SELL_AUX13: Pubkey = Pubkey::from_str_const("CuKh9HUCtz1zbgfrNG4swvj43EoCnrtnQyMNPiGA1NsN");
const SELL_AUX18: Pubkey = Pubkey::from_str_const("9VF2mzXrdhxa5XtvEC4oYa1UCK4LXadhWjcD2anWQHQF");
/// Named by the sale, but its contents do not affect it: an empty account fills the same.
const SELL_AUX19: Pubkey = Pubkey::from_str_const("AG66SJrudMqiRFVrQafq767cjgFjAz1PifDHQ8xTCeK4");

/// How many featured markets the behavioural tests replay.
const FIXTURES: usize = 3;
/// A fixture's base vault holds at least this many times the replayed swap's output.
const VAULT_COVER: u64 = 10;

/// The direction of a replayed swap, each through the router it was captured on.
#[derive(Clone, Copy)]
enum Side {
    Buy,
    Sell,
}

impl Side {
    fn router(self) -> Pubkey {
        match self {
            Side::Buy => DFLOW,
            Side::Sell => JUPITER,
        }
    }

    fn swap_data(self) -> Vec<u8> {
        hex::decode(match self {
            Side::Buy => SWAP_DATA_HEX,
            Side::Sell => SELL_SWAP_DATA_HEX,
        })
        .unwrap()
    }
}

thread_local! {
    // The shim is a plain builtin with no arguments, so the replay hands it the call to forward.
    static FORWARD: std::cell::RefCell<Option<Instruction>> = const { std::cell::RefCell::new(None) };
}

// The routers sign for SIGNER0, and DFlow for SYS0, as PDAs; sigverify is off, so the replay marks
// them signers.
fn humidifi_metas(def: MarketDef, side: Side) -> Vec<AccountMeta> {
    let mut accounts = vec![
        (SIGNER0, true, true),
        (def.market, true, false),
        (def.base_vault, true, false),
        (def.quote_vault, true, false),
        (USER_BASE, true, false),
        (USER_QUOTE, true, false),
        (CLOCK, false, false),
        (TOKEN, false, false),
        (TOKEN, false, false),
    ];
    accounts.extend(match side {
        Side::Buy => vec![
            (SYS0, false, true),
            (def.base_mint, false, false),
            (def.quote_mint, false, false),
            (AUX12, false, false),
            (VOTE, false, false),
            (ROUTE_STATE, false, false),
            (AUX15, false, false),
            (AUX16, false, false),
            (AUX17, false, false),
        ],
        Side::Sell => vec![
            (solana_sdk_ids::sysvar::instructions::ID, false, false),
            (def.base_mint, false, false),
            (def.quote_mint, false, false),
            (FALLBACK_SPREAD_STATE, false, false),
            (SELL_AUX13, false, false),
            (Pubkey::default(), false, false),
            (VOTE, false, false),
            (AUX15, false, false),
            (AUX16, false, false),
            (SELL_AUX18, false, false),
            (SELL_AUX19, true, false),
        ],
    });
    accounts
        .into_iter()
        .map(|(key, writable, signer)| {
            if writable {
                AccountMeta::new(key, signer)
            } else {
                AccountMeta::new_readonly(key, signer)
            }
        })
        .collect()
}

// Registered under the router's id so HumidiFi's Instructions-sysvar caller check sees its router.
declare_process_instruction!(HumidiFiRouteShim, 1, |invoke_context| {
    let call = FORWARD
        .with(|forward| forward.borrow().clone())
        .expect("the replay sets the call to forward");
    invoke_context.native_invoke_signed(call, &[])?;
    Ok(())
});

#[derive(Clone)]
struct HumidifiFixture {
    def: MarketDef,
    elf: Arc<Vec<u8>>,
    market: Account,
    base_vault: Account,
    quote_vault: Account,
    shared: Vec<(Pubkey, Account)>,
    /// A clock the upstream quote is fresh at, so price proofs are not staleness proofs.
    slot: u64,
    max_staleness: u64,
}

/// Base atoms the replayed quote input buys at `fair_value`, before spread.
fn expected_output(fair_value: u64) -> u64 {
    (u128::from(INPUT_QUOTE_AMOUNT) * (1u128 << 48) / u128::from(fair_value)) as u64
}

async fn fixtures() -> Arc<Vec<HumidifiFixture>> {
    static CACHE: tokio::sync::OnceCell<Arc<Vec<HumidifiFixture>>> =
        tokio::sync::OnceCell::const_new();
    CACHE
        .get_or_init(|| async {
            let programdata = fetch(&[PROGRAMDATA]).await.remove(0);
            assert!(
                programdata.data.len() > 300_000,
                "programdata is unexpectedly short"
            );
            let elf = Arc::new(programdata.data[45..].to_vec());
            let mut out = Vec::new();
            let mut skipped = Vec::new();
            for def in FEATURED_MARKETS {
                if out.len() == FIXTURES {
                    break;
                }
                let shared_keys = [
                    def.base_mint,
                    def.quote_mint,
                    VOTE,
                    ROUTE_STATE,
                    FALLBACK_SPREAD_STATE,
                    SELL_AUX18,
                ];
                let a = fetch(
                    &[
                        &[def.market, def.base_vault, def.quote_vault][..],
                        &shared_keys,
                    ]
                    .concat(),
                )
                .await;
                let classic_vaults = [&a[1], &a[2]]
                    .iter()
                    .all(|vault| vault.owner == TOKEN && vault.data.len() == 165);
                let upstream_fair_value = word(&a[0].data, 576) ^ FAIR_VALUE_MASK;
                let unusable = if !classic_vaults {
                    Some("its vaults are not classic SPL-token accounts".to_string())
                } else if upstream_fair_value == 0 {
                    Some("its quote is zero".to_string())
                } else if amount(&a[1].data)
                    < expected_output(upstream_fair_value).saturating_mul(VAULT_COVER)
                {
                    Some(format!(
                        "its base vault holds {} atoms, under {VAULT_COVER}x the replayed swap's \
                         output",
                        amount(&a[1].data)
                    ))
                } else {
                    None
                };
                if let Some(reason) = unusable {
                    skipped.push(format!("{}: {reason}", def.pair));
                    continue;
                }
                assert_eq!(
                    masked_pubkey(&a[0].data, 480),
                    def.base_vault,
                    "{}",
                    def.pair
                );
                assert_eq!(
                    masked_pubkey(&a[0].data, 448),
                    def.quote_vault,
                    "{}",
                    def.pair
                );
                out.push(HumidifiFixture {
                    def,
                    elf: elf.clone(),
                    slot: word(&a[0].data, 616) ^ SLOT_MASK,
                    max_staleness: word(&a[0].data, 608) ^ SLOT_MASK,
                    market: a[0].clone(),
                    base_vault: a[1].clone(),
                    quote_vault: a[2].clone(),
                    shared: shared_keys
                        .into_iter()
                        .zip(a[3..].iter().cloned())
                        .collect(),
                });
            }
            assert_eq!(
                out.len(),
                FIXTURES,
                "the behavioural tests need {FIXTURES} featured markets with funded classic vaults; \
                 skipped: {skipped:?}"
            );
            let first = &out[0];
            assert_eq!(
                first.def.pair, "SOL / USDC",
                "SOL / USDC must be the first fixture"
            );
            Arc::new(out)
        })
        .await
        .clone()
}

fn token_account(mint: Pubkey, owner: Pubkey, amount: u64) -> Account {
    let mut data = vec![0u8; spl_token_interface::state::Account::LEN];
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
        owner: TOKEN,
        ..Account::default()
    }
}

fn system_account(lamports: u64) -> Account {
    Account {
        lamports,
        owner: Pubkey::default(),
        ..Account::default()
    }
}

/// Swaps the captured input on `side` against the supplied market and vault bytes, returning what
/// the user received: base atoms for a buy, quote atoms for a sale.
fn replay(
    fixture: &HumidifiFixture,
    side: Side,
    clock_slot: u64,
    market: Vec<u8>,
    base_vault: Vec<u8>,
    quote_vault: Vec<u8>,
) -> Result<u64, TransactionError> {
    const BASE_HELD: u64 = 100_000_000_000;
    const QUOTE_HELD: u64 = 1_000_000_000;
    let mut svm = LiteSVM::new()
        .with_sigverify(false)
        .with_blockhash_check(false);
    svm.add_program(PROGRAM, &fixture.elf)
        .expect("load HumidiFi ELF");
    svm.add_builtin(side.router(), HumidiFiRouteShim::register);
    let mut clock: solana_clock::Clock = svm.get_sysvar();
    clock.slot = clock_slot;
    clock.unix_timestamp = 1_788_000_000;
    svm.set_sysvar(&clock);

    for (key, template, data) in [
        (fixture.def.market, &fixture.market, market),
        (fixture.def.base_vault, &fixture.base_vault, base_vault),
        (fixture.def.quote_vault, &fixture.quote_vault, quote_vault),
    ] {
        svm.set_account(
            key,
            Account {
                data,
                ..template.clone()
            },
        )
        .unwrap();
    }
    for (key, account) in &fixture.shared {
        svm.set_account(*key, account.clone()).unwrap();
    }
    svm.set_account(SIGNER0, system_account(1_000_000_000))
        .unwrap();
    svm.set_account(SYS0, system_account(1_244_010)).unwrap();
    for aux in [AUX12, AUX15, AUX16, AUX17] {
        svm.set_account(aux, system_account(1_000_000)).unwrap();
    }
    svm.set_account(
        USER_BASE,
        token_account(fixture.def.base_mint, SIGNER0, BASE_HELD),
    )
    .unwrap();
    svm.set_account(
        USER_QUOTE,
        token_account(fixture.def.quote_mint, SIGNER0, QUOTE_HELD),
    )
    .unwrap();

    let metas = humidifi_metas(fixture.def, side);
    let mut accounts = vec![AccountMeta::new_readonly(PROGRAM, false)];
    accounts.extend(metas.clone());
    // HumidiFi only checks which router called it: the router's own instruction data does not
    // change the fill on either side, so the route carries none.
    let route = Instruction {
        program_id: side.router(),
        accounts,
        data: vec![],
    };
    let compute_limit = Instruction {
        program_id: Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: [vec![2], 1_400_000u32.to_le_bytes().to_vec()].concat(),
    };
    let message = Message::new_with_blockhash(
        &[compute_limit, route],
        Some(&SIGNER0),
        &svm.latest_blockhash(),
    );
    let tx = Transaction {
        signatures: vec![Signature::default(); message.header.num_required_signatures as usize],
        message,
    };
    FORWARD.with(|forward| {
        *forward.borrow_mut() = Some(Instruction {
            program_id: PROGRAM,
            accounts: metas,
            data: side.swap_data(),
        })
    });
    svm.send_transaction(tx).map_err(|failed| failed.err)?;

    let base = amount(&svm.get_account(&USER_BASE).unwrap().data);
    let quote = amount(&svm.get_account(&USER_QUOTE).unwrap().data);
    let (input, captured_input, output) = match side {
        Side::Buy => (QUOTE_HELD - quote, INPUT_QUOTE_AMOUNT, base - BASE_HELD),
        Side::Sell => (BASE_HELD - base, INPUT_BASE_AMOUNT, quote - QUOTE_HELD),
    };
    assert_eq!(
        input, captured_input,
        "the captured instruction's input changed"
    );
    Ok(output)
}

/// A buy of the captured quote input against the fixture's own vaults.
fn run(
    fixture: &HumidifiFixture,
    clock_slot: u64,
    market: Vec<u8>,
) -> Result<u64, TransactionError> {
    replay(
        fixture,
        Side::Buy,
        clock_slot,
        market,
        fixture.base_vault.data.clone(),
        fixture.quote_vault.data.clone(),
    )
}

fn fixed_spread(bps: u64) -> [(&'static str, serde_json::Value); 4] {
    [
        ("spread", serde_json::json!(bps * 1_000)),
        ("tier_1_spread", serde_json::json!("0")),
        ("tier_2_spread", serde_json::json!("0")),
        ("tier_3_spread", serde_json::json!("0")),
    ]
}

fn priced_market(fixture: &HumidifiFixture, fair_value: u64) -> Vec<u8> {
    let priced = apply_raw(
        "humidifi-price",
        &fixture.market.data,
        &[("fair_value", serde_json::json!(fair_value.to_string()))],
        fixture.slot,
    );
    humidifi_keep_alive(&priced, fixture.slot)
}

/// Every template writes only its own bytes on every featured market, and every vault is a token
/// account its market owns, which is what the vault guidance relies on.
#[tokio::test]
async fn humidifi_templates_write_only_proven_bytes_on_every_market() {
    let keys: Vec<Pubkey> = FEATURED_MARKETS
        .iter()
        .flat_map(|def| [def.market, def.base_vault, def.quote_vault])
        .collect();
    let accounts = fetch(&keys).await;
    for (def, fetched) in FEATURED_MARKETS.iter().zip(accounts.chunks(3)) {
        let market = def.market;
        let address = def.pair;
        let (base_vault, quote_vault) = (def.base_vault, def.quote_vault);
        let data = &fetched[0].data;
        assert_eq!(fetched[0].owner, PROGRAM, "{address}");
        assert_eq!(data.len(), MARKET_SIZE, "{address}");
        assert_eq!(&data[8..16], &LAYOUT_TAG, "{address} lost the layout tag");
        assert_eq!(word(data, 1720), 8, "{address} is no longer schema 8");
        for (vault_offset, mint_offset, vault, account) in [
            (480, 416, base_vault, &fetched[1]),
            (448, 384, quote_vault, &fetched[2]),
        ] {
            assert_eq!(
                masked_pubkey(data, vault_offset),
                vault,
                "{address} vault at {vault_offset}"
            );
            assert_eq!(
                pubkey_at(&account.data, 0),
                masked_pubkey(data, mint_offset),
                "{address}: {vault} mint"
            );
            assert_eq!(
                pubkey_at(&account.data, 32),
                market,
                "{address}: {vault} must be owned by its market"
            );
        }

        for template in ["humidifi-price", "humidifi-spread"] {
            assert_eq!(
                apply_raw(template, data, &[], 0),
                *data,
                "{template} must round-trip {address}"
            );
        }
        let slot = (word(data, 616) ^ SLOT_MASK) + 1_000;
        let priced = apply_raw(
            "humidifi-price",
            data,
            &[("fair_value", serde_json::json!("58546795155816"))],
            slot,
        );
        assert_only_within(data, &priced, &[FAIR_VALUE], address);
        assert_eq!(word(&priced, 576) ^ FAIR_VALUE_MASK, 58_546_795_155_816);

        let republished = humidifi_keep_alive(data, slot);
        assert_only_within(data, &republished, &[LAST_UPDATE], address);
        assert_eq!(word(&republished, 616) ^ SLOT_MASK, slot);

        for (offset, mask) in TIER_SPREADS {
            assert!(
                (word(data, offset) ^ mask) >> 48 < 10_000,
                "{address}: tier spread at {offset} no longer decodes to basis points"
            );
        }
        let spread = apply_raw("humidifi-spread", data, &fixed_spread(25), slot);
        assert_only_within(data, &spread, &SPREAD_WORDS, address);
        assert_eq!(word(&spread, 800) ^ MAKER_SPREAD_MASK, 25_000);
        for (offset, mask) in TIER_SPREADS {
            assert_eq!(
                word(&spread, offset) ^ mask,
                0,
                "{address}: tier spread at {offset}"
            );
        }

        for vault in [&fetched[1].data, &fetched[2].data] {
            assert_eq!(
                apply_raw("humidifi-vault-balance", vault, &[], slot),
                *vault
            );
            let changed = apply_raw(
                "humidifi-vault-balance",
                vault,
                &[("amount", serde_json::json!(123u64))],
                slot,
            );
            assert_only_within(vault, &changed, &[AMOUNT], address);
            assert_eq!(amount(&changed), 123);
        }
    }
}

/// The featured addresses Studio and the README name unmask from the market's own fields, so a
/// replaced market fails here instead of silently targeting the wrong account.
#[tokio::test]
async fn humidifi_featured_markets_match_their_market_accounts() {
    for def in FEATURED_MARKETS {
        let a = fetch(&[
            def.market,
            def.base_vault,
            def.quote_vault,
            def.base_mint,
            def.quote_mint,
        ])
        .await;
        let data = &a[0].data;
        assert_eq!(a[0].owner, PROGRAM, "{}", def.pair);
        assert_eq!(data.len(), MARKET_SIZE, "{}", def.pair);
        assert_eq!(&data[8..16], &LAYOUT_TAG, "{}", def.pair);
        assert_eq!(word(data, 1720), 8, "{} is no longer schema 8", def.pair);
        for (offset, expected, what) in [
            (384, def.quote_mint, "quote mint"),
            (416, def.base_mint, "base mint"),
            (448, def.quote_vault, "quote vault"),
            (480, def.base_vault, "base vault"),
        ] {
            assert_eq!(masked_pubkey(data, offset), expected, "{} {what}", def.pair);
        }
        for (vault, mint, what) in [(&a[1], &a[3], "base"), (&a[2], &a[4], "quote")] {
            assert_eq!(
                vault.owner, mint.owner,
                "{} {what} vault token program",
                def.pair
            );
            assert_eq!(
                pubkey_at(&vault.data, 32),
                def.market,
                "{} {what} vault owner",
                def.pair
            );
        }
    }
}

#[tokio::test]
async fn humidifi_price_moves_the_executed_fill_on_the_fixture_markets() {
    for fixture in fixtures().await.iter() {
        let upstream = word(&fixture.market.data, 576) ^ FAIR_VALUE_MASK;
        let mut outputs = Vec::new();
        for fair_value in [upstream, upstream * 2] {
            let output = run(fixture, fixture.slot, priced_market(fixture, fair_value))
                .unwrap_or_else(|e| {
                    panic!(
                        "{} fair value {fair_value} must fill: {e:?}",
                        fixture.def.pair
                    )
                });
            let expected = expected_output(fair_value);
            assert!(
                output.abs_diff(expected) <= expected / 100,
                "{} fair value {fair_value}: output {output} is more than 1% from {expected}",
                fixture.def.pair
            );
            outputs.push(output);
        }
        let ratio = outputs[0] as f64 / outputs[1] as f64;
        assert!(
            (ratio - 2.0).abs() < 0.02,
            "{}: a doubled fair value must halve the base bought, got {ratio}",
            fixture.def.pair
        );
    }
}

#[tokio::test]
async fn humidifi_spread_widens_the_executed_fill_on_the_fixture_markets() {
    for fixture in fixtures().await.iter() {
        let upstream = word(&fixture.market.data, 576) ^ FAIR_VALUE_MASK;
        let priced = priced_market(fixture, upstream);
        let outputs: Vec<u64> = [0, 100]
            .into_iter()
            .map(|bps| {
                let market = apply_raw("humidifi-spread", &priced, &fixed_spread(bps), 0);
                run(fixture, fixture.slot, market).unwrap_or_else(|e| {
                    panic!("{} spread {bps} bps must fill: {e:?}", fixture.def.pair)
                })
            })
            .collect();
        let ratio = outputs[1] as f64 / outputs[0] as f64;
        assert!(
            (ratio - 1.0 / 1.01).abs() < 1e-4,
            "{}: a 100 bps spread must cut the base bought by 1/1.01, got {ratio}",
            fixture.def.pair
        );
    }
}

#[tokio::test]
async fn humidifi_price_scenario_materializes_through_surfnet() {
    let fixture = &fixtures().await[0];
    let slot = fixture.slot + 500;
    let (mut svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    svm.set_latest_absolute_slot(slot);
    svm.inner
        .set_account(fixture.def.market, fixture.market.clone())
        .expect("seed market");

    // Keep-alive republishes the quote from the slot the price plays: no freshness override.
    let mut scenario = Scenario::new(
        "HumidiFi SOL/USDC at 208".to_string(),
        "Reprice SOL to 208 USDC".to_string(),
    );
    scenario.add_override(
        OverrideInstance::new(
            "humidifi-price".to_string(),
            0,
            AccountAddress::Pubkey(fixture.def.market.to_string()),
        )
        .with_values(values(&[(
            "fair_value",
            serde_json::json!("58546795155816"),
        )])),
    );
    svm.register_scenario(scenario, Some(slot))
        .expect("register the price scenario");
    svm.materialize_overrides_for_slot(&None, slot)
        .await
        .expect("materialize the price scenario");

    let market = svm
        .inner
        .get_account(&fixture.def.market)
        .unwrap()
        .unwrap()
        .data;
    assert_only_within(
        &fixture.market.data,
        &market,
        &[FAIR_VALUE, LAST_UPDATE],
        "scenario market",
    );
    assert_eq!(word(&market, 576) ^ FAIR_VALUE_MASK, 58_546_795_155_816);
    assert_eq!(word(&market, 616) ^ SLOT_MASK, slot);

    let output = run(fixture, slot, market).expect("the materialized quote must fill");
    let expected = expected_output(58_546_795_155_816);
    assert!(
        output.abs_diff(expected) <= expected / 100,
        "output {output} is more than 1% from {expected}"
    );
}

/// HumidiFi fills while the Clock's slot is at most the market's stored staleness limit past the
/// quote's slot. A republication that old still fills and one slot older rejects, so keep-alive
/// writing the current slot every slot is what keeps a market quoting.
#[tokio::test]
async fn humidifi_keep_alive_staleness_boundary_is_real_program_behavior_on_the_fixture_markets() {
    for fixture in fixtures().await.iter() {
        let clock = fixture.slot + 100;
        let limit = fixture.max_staleness;
        for age in [0, limit, limit + 1] {
            let republished = humidifi_keep_alive(&fixture.market.data, clock - age);
            let result = run(fixture, clock, republished);
            let context = format!("{} limit {limit}, age {age}", fixture.def.pair);
            if age > limit {
                assert_eq!(
                    result,
                    Err(TransactionError::InstructionError(
                        1,
                        InstructionError::Custom(STALE_QUOTE_ERROR)
                    )),
                    "{context}"
                );
            } else {
                assert!(
                    result.unwrap_or_else(|e| panic!("{context}: {e:?}")) > 0,
                    "{context}: a quote within the limit must fill"
                );
            }
        }
    }
}

/// A vault override names its market as an input, and keep-alive republishes that market's quote
/// from the slot the override plays, writing nothing else of the market.
#[tokio::test]
async fn humidifi_vault_override_keeps_its_market_quoting() {
    let fixture = &fixtures().await[0];
    let slot = fixture.slot + 500;
    let (mut svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    svm.set_latest_absolute_slot(slot);
    for (address, seeded) in [
        (fixture.def.market, fixture.market.clone()),
        (fixture.def.quote_vault, fixture.quote_vault.clone()),
    ] {
        svm.inner
            .set_account(address, seeded)
            .expect("seed account");
    }
    let mut scenario = Scenario::new(
        "HumidiFi vault inventory".to_string(),
        "Lower the quote vault and keep its market quoting".to_string(),
    );
    scenario.add_override(
        OverrideInstance::new(
            "humidifi-vault-balance".to_string(),
            0,
            AccountAddress::Pubkey(fixture.def.quote_vault.to_string()),
        )
        .with_values(values(&[
            ("amount", serde_json::json!(1_000_000u64)),
            ("market", serde_json::json!(fixture.def.market.to_string())),
        ])),
    );
    svm.register_scenario(scenario, Some(slot))
        .expect("register the vault scenario");
    svm.materialize_overrides_for_slot(&None, slot)
        .await
        .expect("materialize the vault scenario");

    let vault = svm
        .inner
        .get_account(&fixture.def.quote_vault)
        .unwrap()
        .unwrap()
        .data;
    assert_eq!(amount(&vault), 1_000_000);
    let market = svm
        .inner
        .get_account(&fixture.def.market)
        .unwrap()
        .unwrap()
        .data;
    assert_only_within(
        &fixture.market.data,
        &market,
        &[LAST_UPDATE],
        "kept-alive market",
    );
    assert_eq!(word(&market, 616) ^ SLOT_MASK, slot);
}

async fn materialized_vault(slot: u64, address: Pubkey, seeded: Account, balance: u64) -> Vec<u8> {
    let (mut svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    svm.inner.set_account(address, seeded).expect("seed vault");
    let mut scenario = Scenario::new(
        "HumidiFi vault inventory".to_string(),
        "Lower one HumidiFi vault".to_string(),
    );
    scenario.add_override(
        OverrideInstance::new(
            "humidifi-vault-balance".to_string(),
            0,
            AccountAddress::Pubkey(address.to_string()),
        )
        .with_values(values(&[("amount", serde_json::json!(balance))])),
    );
    svm.register_scenario(scenario, Some(slot))
        .expect("register the vault scenario");
    svm.materialize_overrides_for_slot(&None, slot)
        .await
        .expect("materialize the vault scenario");
    svm.inner.get_account(&address).unwrap().unwrap().data
}

#[tokio::test]
async fn humidifi_vault_scenario_drains_only_the_selected_side() {
    for fixture in fixtures().await.iter() {
        let context = fixture.def.pair;
        let baseline =
            run(fixture, fixture.slot, fixture.market.data.clone()).expect("baseline swap");
        let base_swap = |base_vault: Vec<u8>| {
            replay(
                fixture,
                Side::Buy,
                fixture.slot,
                fixture.market.data.clone(),
                base_vault,
                fixture.quote_vault.data.clone(),
            )
        };

        let drained_base = materialized_vault(
            fixture.slot,
            fixture.def.base_vault,
            fixture.base_vault.clone(),
            0,
        )
        .await;
        assert_only_within(&fixture.base_vault.data, &drained_base, &[AMOUNT], context);
        assert_eq!(amount(&drained_base), 0);
        assert_eq!(
            base_swap(drained_base),
            Err(TransactionError::InstructionError(
                1,
                InstructionError::Custom(EMPTY_PAYOUT_ERROR)
            )),
            "{context}"
        );

        let thin_balance = amount(&fixture.base_vault.data) / 200;
        let thin_base = materialized_vault(
            fixture.slot,
            fixture.def.base_vault,
            fixture.base_vault.clone(),
            thin_balance,
        )
        .await;
        let thin_output = base_swap(thin_base).expect("a thin base vault still settles");
        assert!(
            thin_output < baseline,
            "{context}: thin inventory {thin_output} must quote below {baseline}"
        );

        // The quote vault only receives this direction's input.
        let drained_quote = materialized_vault(
            fixture.slot,
            fixture.def.quote_vault,
            fixture.quote_vault.clone(),
            0,
        )
        .await;
        assert_eq!(amount(&drained_quote), 0);
        assert_eq!(
            replay(
                fixture,
                Side::Buy,
                fixture.slot,
                fixture.market.data.clone(),
                fixture.base_vault.data.clone(),
                drained_quote
            ),
            Ok(baseline),
            "{context}"
        );
    }
}

/// Base atoms of `base_sold` are worth this many quote atoms at `fair_value`, before any spread.
fn quote_value(base_sold: u64, fair_value: u64) -> f64 {
    ((u128::from(base_sold) * u128::from(fair_value)) >> 48) as f64
}

/// The buy route only proves the templates for buyers. Through the Jupiter route mainnet uses today,
/// a doubled fair value must double the quote a sale receives, and a sale must receive
/// `fair / (1 + S)`, where S adds the spread template to the fallback spread a Jupiter route pays in
/// the local VM (README, Troubleshooting).
#[tokio::test]
async fn humidifi_price_and_spread_move_a_sale_on_sol_usdc() {
    let fixture = &fixtures().await[0];
    assert_eq!(
        fixture.def.pair, "SOL / USDC",
        "the sale was captured on SOL / USDC"
    );
    let upstream = word(&fixture.market.data, 576) ^ FAIR_VALUE_MASK;
    let sale = |market: Vec<u8>| {
        replay(
            fixture,
            Side::Sell,
            fixture.slot,
            market,
            fixture.base_vault.data.clone(),
            fixture.quote_vault.data.clone(),
        )
        .unwrap_or_else(|e| panic!("the sale must fill: {e:?}"))
    };

    let received = sale(priced_market(fixture, upstream));
    let doubled = sale(priced_market(fixture, upstream * 2));
    let ratio = doubled as f64 / received as f64;
    assert!(
        (ratio - 2.0).abs() < 1e-4,
        "a doubled fair value must double the quote a sale receives, got {ratio}"
    );

    let priced = priced_market(fixture, upstream);
    let spread = |bps: u64| sale(apply_raw("humidifi-spread", &priced, &fixed_spread(bps), 0));
    let (control, widened) = (spread(0), spread(100));
    let value = quote_value(INPUT_BASE_AMOUNT, upstream);
    let fallback = value / control as f64 - 1.0;
    assert!(
        (0.0095..0.011).contains(&fallback),
        "the Jupiter route should pay about the documented 100 bps fallback, got {:.1} bps",
        fallback * 1e4
    );
    let expected = value / (1.0 + fallback + 0.01);
    assert!(
        (widened as f64 / expected - 1.0).abs() < 1e-4,
        "a 100 bps spread must price a sale at fair / (1 + S): got {widened}, expected {expected:.0}"
    );
}

/// The spread word is stored XORed with `MAKER_SPREAD_MASK`. A mask one bit off still writes
/// working spreads, only 0.001 bps wrong, so fills cannot catch it; the live markets can. Makers
/// store a zero spread most of the time, which the right mask decodes to exactly 0 and a mask with
/// the low bit flipped decodes to 1; the non-zero spreads they publish are hundreds of units.
#[tokio::test]
async fn humidifi_spread_mask_decodes_zero_spreads_to_zero() {
    // Makers sometimes publish a spread on every market at once, which says nothing about the mask,
    // so read again until one market shows a zero (or one) spread.
    const READS: u64 = 5;
    let markets: Vec<Pubkey> = FEATURED_MARKETS.iter().map(|def| def.market).collect();
    let (mut zero, mut one) = (0, 0);
    for read in 0..READS {
        if read > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        for account in fetch(&markets).await {
            match word(&account.data, 800) ^ MAKER_SPREAD_MASK {
                0 => zero += 1,
                1 => one += 1,
                _ => {}
            }
        }
        if zero + one > 0 {
            break;
        }
    }
    assert!(
        zero > one,
        "{zero} market reads decode to a zero spread and {one} to 1 of 1000 bps over up to {READS} \
         reads; the spread mask looks one bit off"
    );
}
