//! On-chain tests for BisonFi, and for the Orca Whirlpool leg its arbitrage scenario trades
//! against.
//!
//! The account-fetch and byte-diff helpers below are deliberately DUPLICATED from the Kamino suite
//! rather than shared. These suites load state from the upstream datasource into a local VM and are
//! the most likely place to need a one-off change to retry behaviour or account synthesis; a shared
//! helper would couple two unrelated protocols' tests together and make such a change risky for
//! both.

//!
//! Like the Kamino suite these fetch real mainnet accounts rather than embedding captured copies,
//! so they need a network connection and are compiled only behind a feature:
//!
//! ```text
//! cargo test -p surfpool-core --features integration-tests bisonfi
//! ```
//!
//! Set `SURFPOOL_TEST_RPC_URL` to use a private endpoint if the public one rate-limits.
//!
//! What these cover that a unit test cannot: BisonFi publishes no IDL, so there is no schema to
//! check a synthetic account against. The only way to know an offset is right is to run the real
//! deployed program over real account state and watch the fill change.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

use solana_account_decoder::{UiAccountEncoding, UiDataSliceConfig};
use solana_client::rpc_config::RpcAccountInfoConfig;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_sysvar::last_restart_slot::LastRestartSlot;
use surfpool_types::KeepAliveClock;

use crate::{
    scenarios::{TemplateRegistry, template_registry},
    surfnet::{GetAccountResult, remote::SurfnetRemoteClient, svm::SurfnetSvm},
    tests::helpers::diff_indices,
};

/// The block builders' market tick a swap passes tenth, and the program that owns it.
const MARKET_TICK: Pubkey = Pubkey::from_str_const("4cG31VNF9TzFinNc7BmnjhFvGjxkY3sCETVMtMgbrhPs");
const MARKET_TICK_PROGRAM: Pubkey =
    Pubkey::from_str_const("tickUcsEQegChaAuo9VYQQztB4ZGApY6ZT4FkULWY6N");

/// What the surfnet writes every slot while a scenario on a BisonFi pool is registered: `keep_alive`
/// applied to `data` as the shipped templates import it, at `slot` and `block_time_ms`.
fn bisonfi_keep_alive(keep_alive: &str, data: &[u8], slot: u64, block_time_ms: u64) -> Vec<u8> {
    let registry = template_registry();
    let import = registry
        .get("bisonfi-fair-value")
        .and_then(|template| {
            template
                .keep_alive
                .iter()
                .find(|import| import.template == keep_alive)
        })
        .unwrap_or_else(|| panic!("BisonFi templates must import {keep_alive}"));
    let values = import
        .values_at(&KeepAliveClock {
            slot,
            unix_time_ms: block_time_ms,
        })
        .expect("keep-alive values");
    registry
        .keep_alive_template(keep_alive)
        .expect("keep-alive template")
        .materialize_raw_layout(data, &values, slot)
        .unwrap_or_else(|e| panic!("{keep_alive}: {e}"))
}

// ---------------------------------------------------------------- fetch/diff helpers

const RPC_URL_ENV: &str = "SURFPOOL_TEST_RPC_URL";

const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";
const BISONFI_PROGRAM: &str = "BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi";

/// Fetches the accounts in one request, so every account returned is from the same slot.
async fn fetch(addresses: &[&str]) -> Vec<Vec<u8>> {
    let client = SurfnetRemoteClient::new(
        std::env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_RPC_URL.to_string()),
    );
    let pubkeys: Vec<Pubkey> = addresses
        .iter()
        .map(|a| Pubkey::from_str_const(a))
        .collect();

    // The public endpoint throttles and intermittently 503s, which has nothing to do with what these
    // tests assert. Retry a few times with backoff so a transient refusal is not read as a failure.
    let mut attempt = 0;
    let results = loop {
        match client
            .get_multiple_accounts(&pubkeys, CommitmentConfig::confirmed())
            .await
        {
            Ok(r) => break r,
            Err(e) => {
                attempt += 1;
                if attempt >= 5 {
                    panic!(
                        "failed to fetch {addresses:?} from mainnet after {attempt} attempts: {e}"
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(750 * attempt)).await;
            }
        }
    };
    results
        .into_iter()
        .zip(addresses)
        .map(|(result, address)| match result {
            GetAccountResult::FoundAccount(_, account, _)
            | GetAccountResult::FoundCoupledAccount((_, account), _, _) => account.data,
            GetAccountResult::None(_) => {
                panic!("{address} no longer exists on mainnet; the test needs a new address")
            }
        })
        .collect()
}

/// Like [`fetch`] but reports absence instead of panicking.
///
/// Needed for PDAs that are only created lazily. A Whirlpool tick array, for instance, does not exist
/// until someone provides liquidity in that range, so "missing" is a real answer about the market
/// rather than a stale address in the test.
async fn fetch_optional(addresses: &[&str]) -> Vec<Option<Vec<u8>>> {
    let client = SurfnetRemoteClient::new(
        std::env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_RPC_URL.to_string()),
    );
    let pubkeys: Vec<Pubkey> = addresses
        .iter()
        .map(|a| Pubkey::from_str_const(a))
        .collect();
    let results = client
        .get_multiple_accounts(&pubkeys, CommitmentConfig::confirmed())
        .await
        .expect("get_multiple_accounts");
    results
        .into_iter()
        .map(|r| match r {
            GetAccountResult::FoundAccount(_, account, _)
            | GetAccountResult::FoundCoupledAccount((_, account), _, _) => Some(account.data),
            GetAccountResult::None(_) => None,
        })
        .collect()
}

/// Like [`fetch`] but keeps each account's owner instead of its data.
///
/// Needed to tell a classic SPL mint from a Token-2022 one. Pools quoting a Token-2022 asset refuse
/// a swap with `Custom(60)` if handed classic token accounts, so a replay harness that assumes one
/// token program silently cannot exercise them.
async fn fetch_owners(addresses: &[Pubkey]) -> Vec<Pubkey> {
    let client = SurfnetRemoteClient::new(
        std::env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_RPC_URL.to_string()),
    );
    let mut attempt = 0;
    let results = loop {
        match client
            .get_multiple_accounts(addresses, CommitmentConfig::confirmed())
            .await
        {
            Ok(r) => break r,
            Err(e) => {
                attempt += 1;
                if attempt >= 5 {
                    panic!("failed to fetch owners after {attempt} attempts: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_millis(750 * attempt)).await;
            }
        }
    };
    results
        .into_iter()
        .zip(addresses)
        .map(|(result, address)| match result {
            GetAccountResult::FoundAccount(_, account, _)
            | GetAccountResult::FoundCoupledAccount((_, account), _, _) => account.owner,
            GetAccountResult::None(_) => panic!("{address} no longer exists on mainnet"),
        })
        .collect()
}

/// Minimal initialised SPL token account (the 165-byte legacy layout).
fn token_account(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut d = vec![0u8; 165];
    d[0..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    d[108] = 1; // AccountState::Initialized
    d
}

fn spl_amount(data: &[u8]) -> u64 {
    u64::from_le_bytes(data[64..72].try_into().unwrap())
}

/// Like [`fetch`] but keeps each account's lamports. A wrapped-SOL vault's lamports are part of its
/// state, so overwriting them with a placeholder makes the runtime reject the transaction as
/// unbalanced on any path that pays out the base token.
async fn fetch_with_lamports(addresses: &[&str]) -> Vec<(Vec<u8>, u64)> {
    let client = SurfnetRemoteClient::new(
        std::env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_RPC_URL.to_string()),
    );
    let pubkeys: Vec<Pubkey> = addresses
        .iter()
        .map(|a| Pubkey::from_str_const(a))
        .collect();
    let mut attempt = 0;
    let results = loop {
        match client
            .get_multiple_accounts(&pubkeys, CommitmentConfig::confirmed())
            .await
        {
            Ok(r) => break r,
            Err(e) => {
                attempt += 1;
                if attempt >= 5 {
                    panic!("failed to fetch {addresses:?} after {attempt} attempts: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_millis(750 * attempt)).await;
            }
        }
    };
    results
        .into_iter()
        .zip(addresses)
        .map(|(result, address)| match result {
            GetAccountResult::FoundAccount(_, account, _)
            | GetAccountResult::FoundCoupledAccount((_, account), _, _) => {
                (account.data, account.lamports)
            }
            GetAccountResult::None(_) => panic!("{address} no longer exists on mainnet"),
        })
        .collect()
}

const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

// ------------------------------------------------------------------ BisonFi / Orca

/// The token program owning each pool's base and quote mint, resolved for every pool in one request.
async fn bisonfi_token_programs(pools: &[Vec<u8>]) -> Vec<(Pubkey, Pubkey)> {
    let mut mints: Vec<Pubkey> = Vec::new();
    for data in pools {
        for range in [184..216, 216..248] {
            let m = Pubkey::new_from_array(data[range].try_into().unwrap());
            if !mints.contains(&m) {
                mints.push(m);
            }
        }
    }
    let owners = fetch_owners(&mints).await;
    let map: HashMap<Pubkey, Pubkey> = mints.into_iter().zip(owners).collect();
    pools
        .iter()
        .map(|data| {
            let base = Pubkey::new_from_array(data[184..216].try_into().unwrap());
            let quote = Pubkey::new_from_array(data[216..248].try_into().unwrap());
            (map[&base], map[&quote])
        })
        .collect()
}

const BISONFI_POOL: &str = "8FnX3xo2yYw3EUE6w3nQA4GfXGS9wpK6oj3veJpbFzLo";

/// The published mid is the only price lever, and it is a u128 far beyond `u64::MAX`, so it can
/// only be written as a decimal string.
#[tokio::test]
async fn bisonfi_fair_value_override_writes_expected_bytes() {
    const FAIR_VALUE: usize = 832;

    let data = fetch(&[BISONFI_POOL]).await.remove(0);
    let registry = TemplateRegistry::new();
    let template = registry.get("bisonfi-fair-value").unwrap();
    assert!(template.raw_layout, "BisonFi templates use raw layouts");

    // $50.00 scaled by 2^88
    let target: u128 = 50u128 * (1u128 << 88);
    let forged = template
        .materialize_raw_layout(
            &data,
            &HashMap::from([(
                "fair_value".to_string(),
                serde_json::json!(target.to_string()),
            )]),
            0,
        )
        .expect("fair value override should apply");

    assert_eq!(
        u128::from_le_bytes(forged[FAIR_VALUE..FAIR_VALUE + 16].try_into().unwrap()),
        target,
        "the published mid must land at offset 832 as a 2^88 fixed point"
    );
    let diffs = diff_indices(&forged, &data);
    assert!(
        diffs
            .iter()
            .all(|i| (FAIR_VALUE..FAIR_VALUE + 16).contains(i)),
        "only the fair value should change, got {diffs:?}"
    );
}

/// Every shipped BisonFi template must explicitly select the raw byte-write path.
#[test]
fn bisonfi_templates_select_raw_layout_writes() {
    let registry = TemplateRegistry::new();
    let templates: Vec<_> = registry
        .all()
        .into_iter()
        .filter(|template| template.id.starts_with("bisonfi-"))
        .collect();
    assert!(
        !templates.is_empty(),
        "BisonFi templates must be registered"
    );

    for template in &templates {
        assert!(
            template.raw_layout,
            "{} must select raw-layout writes",
            template.id
        );
    }
}

/// last_update_slot is the chain slot on a live market, and keep-alive's publication writes exactly
/// the three fields the maker's publication moves.
#[tokio::test]
async fn bisonfi_keep_alive_republishes_the_quote_on_a_live_market() {
    const LAST_UPDATE: usize = 72;
    const PREVIOUS_UPDATE: usize = 80;
    const PUBLICATION_NS: usize = 88;
    const LAST_REBUILD: usize = 688;

    let client = SurfnetRemoteClient::new(
        std::env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_RPC_URL.to_string()),
    );
    let slot = client
        .get_epoch_info()
        .await
        .expect("epoch info")
        .absolute_slot;
    let data = fetch(&[BISONFI_POOL]).await.remove(0);

    let last = u64::from_le_bytes(data[LAST_UPDATE..LAST_UPDATE + 8].try_into().unwrap());
    let prev = u64::from_le_bytes(
        data[PREVIOUS_UPDATE..PREVIOUS_UPDATE + 8]
            .try_into()
            .unwrap(),
    );
    assert!(
        slot.saturating_sub(last) < 200,
        "a live market should have been updated within the last ~200 slots; chain {slot}, \
         last_update {last}. If this market went dormant, pick another."
    );
    // Not strict. The operator republishes about ten times a second against ~400ms slots, so two
    // publications landing in one slot is normal and leaves these two fields EQUAL. Requiring prev to
    // be strictly behind made this test fail intermittently on nothing more than a busy market; the
    // property actually worth asserting is that previous never LEADS last.
    assert!(
        prev <= last,
        "previous_update_slot ({prev}) must never lead last_update_slot ({last})"
    );

    let (published_at, block_time_ms) = (last + 10, 1_800_000_000_123u64);
    let published = bisonfi_keep_alive(
        "bisonfi-quote-publication",
        &data,
        published_at,
        block_time_ms,
    );
    let at = |offset: usize| u64::from_le_bytes(published[offset..offset + 8].try_into().unwrap());
    assert_eq!(at(LAST_UPDATE), published_at);
    assert_eq!(at(PUBLICATION_NS), block_time_ms * 1_000_000);
    assert_eq!(
        at(LAST_REBUILD),
        published_at - 1,
        "the first swap of the slot must refill the book"
    );
    let diffs = diff_indices(&published, &data);
    assert!(
        diffs
            .iter()
            .all(|i| [LAST_UPDATE, PUBLICATION_NS, LAST_REBUILD]
                .iter()
                .any(|field| (*field..field + 8).contains(i))),
        "only the three publication fields should change, got {diffs:?}"
    );
}

/// Every account the program owns, live and dormant, as of program build 3f38e742. The templates
/// take any market's address, so every supported v3 account must accept the same writes.
const BISONFI_ALL_POOLS: [&str; 17] = [
    "2vPjbPRnz7V1SLGr56CmLLc7JspzfSfccWp3Th5KbrMJ",
    "6b5LxeDVxqCGAhZjjjgieGP71c5GBt2cBwiafCFX6NMU",
    "8FnX3xo2yYw3EUE6w3nQA4GfXGS9wpK6oj3veJpbFzLo",
    "AfaA4CE8C2DWSHANCqvU9RWrxRiXCV7KKVSw4cHi68Wn",
    "DSzgmzz1Ms4qshdeCpE2uWenXXyNbkikw3bzfJRAv7JF",
    "FJnaiidSLXFweWkgbinxEHRykVHsnkzDcYbNDR3RF5LN",
    "GU7Auyn3cMxtuZX8N3ezhKztgJ1bqqpuUk19KWXqnYwv",
    "Hv8FoJFsrQhoyrR6Lcz4KFcpqNHU1Kxj2yaFDKU6vJdp",
    "7ZTpmqKWeAkRwHHgi74Gu1o6vWrHJRBDNsZTAdkKpohv",
    "AWVYnCT2ZdLsWZf1X9KXatZhC2TyruRM22y8KZqVeupr",
    "CKc2gypi1feWLboi7PWRgNTCi6NWhkYaGU4v6rhnZDDJ",
    "4X3seJERbu4xy7sVAndPBsy4JWZVAGEpXv1NcCQ6zo66",
    "Gsu4WmGJf9z4RWiQ9onE9u29rSvh5XsAkVwUJ2bLrGQb",
    "51FQwjrvo8J8zXUaKyAznJ5NYpoiTCuqAqCu3HAMB9NZ",
    "4XkEAUpmQnuKK2N1H73v68GkTpbNrxZZ37ZyfHfELZve",
    "6U1kWANmyBuJRTZGRuPb9o2EJ6KRui3QpqrWDZoZ4bnG",
    "FC9pWtfdtbyGZ5WHTLneoMSUx6jmTDgqKaxDcm2trsND",
];

/// The write half of the matrix: each shipping property, against each of the 17 v3 pools. Asserts the
/// value lands at the offset the template declares, and nothing outside that field moves.
#[tokio::test]
async fn bisonfi_every_property_writes_cleanly_on_every_pool() {
    let all = fetch(&BISONFI_ALL_POOLS).await;
    let registry = TemplateRegistry::new();

    for (pool, data) in BISONFI_ALL_POOLS.iter().zip(all.iter()) {
        assert_eq!(data.len(), 2048, "{pool} should be 2048 bytes");
        assert_eq!(&data[..8], b"POOLSTAT", "{pool} account prefix");
        assert_eq!(
            u64::from_le_bytes(data[8..16].try_into().unwrap()),
            3,
            "{pool} must still use the supported v3 layout"
        );
    }

    // (template id, property name, offset, width, value to write)
    let cases: [(&str, &str, usize, usize, serde_json::Value); 4] = [
        (
            "bisonfi-fair-value",
            "fair_value",
            832,
            16,
            serde_json::json!((50u128 * (1u128 << 88)).to_string()),
        ),
        (
            "bisonfi-quote-publication",
            "last_update_slot",
            72,
            8,
            serde_json::json!(123_456_789u64),
        ),
        (
            "bisonfi-depth",
            "base_reserve",
            48,
            8,
            serde_json::json!(1_000_000_000u64),
        ),
        (
            "bisonfi-depth",
            "quote_reserve",
            56,
            8,
            serde_json::json!(2_000_000_000u64),
        ),
    ];

    for (id, prop, offset, width, value) in cases {
        let template = registry.get(id).unwrap_or_else(|| panic!("{id} template"));
        assert!(template.raw_layout, "{id} carries a raw layout");

        for (pool, data) in BISONFI_ALL_POOLS.iter().zip(all.iter()) {
            let forged = template
                .materialize_raw_layout(
                    data,
                    &HashMap::from([(prop.to_string(), value.clone())]),
                    0,
                )
                .unwrap_or_else(|e| panic!("{id}: {prop} failed on {pool}: {e}"));

            assert_eq!(forged.len(), 2048, "{id} on {pool}: size changed");
            let diffs = diff_indices(&forged, data);
            assert!(
                diffs.iter().all(|i| (offset..offset + width).contains(i)),
                "{id}: writing {prop} on {pool} touched bytes outside {offset}..{}: {diffs:?}",
                offset + width
            );
            // And the value actually landed.
            let mut buf = [0u8; 16];
            buf[..width].copy_from_slice(&forged[offset..offset + width]);
            let got = u128::from_le_bytes(buf);
            let want: u128 = match &value {
                serde_json::Value::String(s) => s.parse().unwrap(),
                // A negative tick lands as two's complement in `width` bytes, so compare against
                // the same truncation rather than treating the field as unsigned.
                v => match v.as_i64() {
                    Some(n) if n < 0 => (n as i128 as u128) & ((1u128 << (width * 8)) - 1),
                    _ => v.as_u64().unwrap() as u128,
                },
            };
            assert_eq!(got, want, "{id}: {prop} on {pool} did not land");
        }
    }
}

/// Offsets 48 and 56 mirror the vaults exactly, which is why bisonfi-depth may only lower them. This
/// pins that measurement so the claim in the layout docs cannot rot silently: if a redeploy changes
/// it, the reserve fields mean something else and the docs need revisiting.
#[tokio::test]
async fn bisonfi_reserves_mirror_the_vaults() {
    const BASE_RESERVE: usize = 48;
    const BASE_VAULT: usize = 120;
    const QUOTE_VAULT: usize = 152;

    // The vaults are named in the pool itself, but the balance comparison is only meaningful if
    // both are read at the same slot - this market turns over thousands of SOL in a few hundred
    // slots. So they are fetched in one batch, which means the addresses have to be known up front
    // and then checked against the pool's own fields.
    const BASE_VAULT_ADDR: &str = "ATRsNGv2nDw7hSMfkUTBoVUDsFDwN7po7KbecyiGWNB4";
    const QUOTE_VAULT_ADDR: &str = "2Y7HATmn9aJBcxCskE5V2U2epmjvkZmB51zTJBbhj4cU";

    let batch = fetch(&[BISONFI_POOL, BASE_VAULT_ADDR, QUOTE_VAULT_ADDR]).await;
    let data = &batch[0];

    assert_eq!(
        Pubkey::new_from_array(data[BASE_VAULT..BASE_VAULT + 32].try_into().unwrap()),
        Pubkey::from_str_const(BASE_VAULT_ADDR),
        "base_vault at offset 120 no longer points at the expected token account"
    );
    assert_eq!(
        Pubkey::new_from_array(data[QUOTE_VAULT..QUOTE_VAULT + 32].try_into().unwrap()),
        Pubkey::from_str_const(QUOTE_VAULT_ADDR),
        "quote_vault at offset 152 no longer points at the expected token account"
    );

    // SPL token account: amount is a u64 at offset 64.
    let base_held = u64::from_le_bytes(batch[1][64..72].try_into().unwrap());
    let cached = u64::from_le_bytes(data[BASE_RESERVE..BASE_RESERVE + 8].try_into().unwrap());

    // Same slot, so they must agree exactly. This is the measurement behind bisonfi-depth's "lower,
    // do not raise" rule: offset 48 is the whole balance, mirrored.
    assert_eq!(
        cached, base_held,
        "offset 48 is expected to mirror the base vault balance exactly; pool says {cached}, \
         vault holds {base_held}"
    );
}

/// The pools a behavioural scenario can actually be asserted on, fetched once.
struct BisonfiRig {
    elf: Vec<u8>,
    /// Address, account bytes, and the token program owning each side's mint.
    quoting: Vec<(&'static str, Vec<u8>, (Pubkey, Pubkey))>,
}

impl BisonfiRig {
    /// Applies one template's values through the real override engine and replays a swap.
    fn scenario(
        &self,
        pool: &str,
        data: &[u8],
        tp: (Pubkey, Pubkey),
        template_id: &str,
        values: &[(&str, serde_json::Value)],
        amount_in: u64,
        direction: u8,
    ) -> u64 {
        // Materialize INSIDE the replay, not before it. `bisonfi_replay` derives the simnet clock
        // from the pool's own last_update_slot, so handing it an already-aged account moves the clock
        // back along with the field and the quote never looks stale at all.
        self.try_scenario(pool, data, tp, template_id, values, amount_in, direction)
            .unwrap_or_else(|e| panic!("{template_id} on {pool}: replay failed: {e}"))
    }

    /// As [`Self::scenario`] but surfaces a refusal instead of panicking, for the scenarios where the
    /// venue declining to fill is the point.
    fn try_scenario(
        &self,
        pool: &str,
        data: &[u8],
        tp: (Pubkey, Pubkey),
        template_id: &str,
        values: &[(&str, serde_json::Value)],
        amount_in: u64,
        direction: u8,
    ) -> Result<u64, String> {
        let registry = TemplateRegistry::new();
        let template = registry
            .get(template_id)
            .unwrap_or_else(|| panic!("{template_id} must exist in the registry"))
            .clone();
        assert!(
            template.raw_layout,
            "{template_id} must use raw-layout writes"
        );
        let map: HashMap<String, serde_json::Value> = values
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        let target_slot = u64::from_le_bytes(data[72..80].try_into().expect("8 bytes"));
        bisonfi_replay(&self.elf, pool, data, tp, amount_in, direction, move |d| {
            let forged = template
                .materialize_raw_layout(d.as_slice(), &map, target_slot)
                .unwrap_or_else(|e| panic!("materialize failed: {e}"));
            *d = forged;
        })
    }

    /// A sell size that every live market fills, at 2% of the base reserve.
    fn sell_size(data: &[u8]) -> u64 {
        u64::from_le_bytes(data[48..56].try_into().unwrap()) / 50
    }

    /// The quote-side notional matching [`Self::sell_size`], taken from what a control sell actually
    /// pays out. The pool's fixed-point mid is the price in HUMAN units, out by the market's decimal
    /// shift; the control fill needs no decimal table and cannot drift.
    fn buy_size(control_sell_out: u64) -> u64 {
        control_sell_out
    }
}

/// One rig per process. Ten tests need it, and each build costs two `getMultipleAccounts` calls
/// against a public endpoint that rate-limits - running them in parallel exhausted it and failed six
/// tests at once, every one of which passed in isolation.
async fn bisonfi_rig() -> std::sync::Arc<BisonfiRig> {
    static CACHE: tokio::sync::OnceCell<std::sync::Arc<BisonfiRig>> =
        tokio::sync::OnceCell::const_new();
    CACHE
        .get_or_init(|| async { std::sync::Arc::new(bisonfi_rig_uncached().await) })
        .await
        .clone()
}

/// The byte the maker sets to halt a pool: while it is non-zero the pool pays nothing in either
/// direction, at any size, though the maker keeps republishing it.
const BISONFI_HALTED: usize = 824;

/// The pools in `all` a replay must be able to quote: the maker still republishes them and has not
/// halted them, they hold reserves, and both their mints are classic SPL tokens the harness can build
/// accounts for. Which pools that is changes with the maker's decisions, so tests assert on this set
/// rather than on a count of pools.
fn bisonfi_pools_expected_to_quote(all: &[Vec<u8>], programs: &[(Pubkey, Pubkey)]) -> Vec<usize> {
    let spl = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
    let published = |data: &[u8]| u64::from_le_bytes(data[72..80].try_into().unwrap());
    let newest = all.iter().map(|data| published(data)).max().unwrap_or(0);
    all.iter()
        .zip(programs)
        .enumerate()
        .filter(|(_, (data, (base, quote)))| {
            // Dormant pools are millions of slots behind; a republished one is a few slots at most.
            newest - published(data) <= 1_000
                && data[BISONFI_HALTED] == 0
                && u64::from_le_bytes(data[48..56].try_into().unwrap()) >= 1_000_000
                && *base == spl
                && *quote == spl
        })
        .map(|(index, _)| index)
        .collect()
}

async fn bisonfi_rig_uncached() -> BisonfiRig {
    let elf = bisonfi_elf().await;
    let all = fetch(&BISONFI_ALL_POOLS).await;
    let programs = bisonfi_token_programs(&all).await;
    let mut quoting = Vec::new();
    for ((pool, data), tp) in BISONFI_ALL_POOLS
        .iter()
        .zip(all.iter())
        .zip(programs.iter())
    {
        let base_reserve = u64::from_le_bytes(data[48..56].try_into().unwrap());
        if base_reserve < 1_000_000 {
            continue;
        }
        let size = BisonfiRig::sell_size(data);
        if size == 0 {
            continue;
        }
        if let Ok(out) = bisonfi_replay(&elf, pool, data, *tp, size, 0, |_| {}) {
            if out > 0 {
                quoting.push((*pool, data.clone(), *tp));
            }
        }
    }
    // Every pool the maker republishes and has not halted must quote, or coverage silently narrowed
    // and the scenario assertions below stop meaning anything. Dormant, halted and Token-2022 pools
    // are skipped: which pools those are is the maker's decision, not a regression.
    let expected = bisonfi_pools_expected_to_quote(&all, &programs);
    let silent: Vec<&str> = expected
        .iter()
        .map(|&index| BISONFI_ALL_POOLS[index])
        .filter(|pool| !quoting.iter().any(|(quoted, _, _)| quoted == pool))
        .collect();
    assert!(
        silent.is_empty(),
        "{silent:?} are republished and not halted but cannot be quoted; scenario coverage has narrowed"
    );
    assert!(!quoting.is_empty(), "no BisonFi pool can be quoted");
    BisonfiRig { elf, quoting }
}

/// SCENARIO: set X mid price for a given market.
///
/// The template's whole promise is that the number you pass becomes the price the venue quotes
/// around. Asserted as proportionality, on every market that quotes, because that is the property a
/// scenario author relies on: ask for double and the fill doubles.
#[tokio::test]
async fn bisonfi_scenario_set_mid_price() {
    let rig = bisonfi_rig().await;
    for (pool, data, tp) in &rig.quoting {
        let size = BisonfiRig::sell_size(data);
        let mid = u128::from_le_bytes(data[832..848].try_into().unwrap());
        let base = bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, |_| {})
            .unwrap_or_else(|e| panic!("{pool}: control failed: {e}"));

        for (label, num, den) in [("double", 2u128, 1u128), ("half", 1, 2)] {
            let out = rig.scenario(
                pool,
                data,
                *tp,
                "bisonfi-fair-value",
                &[(
                    "fair_value",
                    serde_json::json!((mid * num / den).to_string()),
                )],
                size,
                0,
            );
            let want = base as f64 * num as f64 / den as f64;
            let err = (out as f64 - want) / want;
            assert!(
                err.abs() < 0.005,
                "{pool}: setting the mid to {label} the live value paid {out}, {:.3}% off the {want:.0} \
                 that proportionality requires. A scenario asking for a price would not get it",
                err * 100.0
            );
        }
    }
}

/// SCENARIO: thin out a given market so a large trade slips measurably.
#[tokio::test]
async fn bisonfi_scenario_thin_depth_makes_a_trade_slip() {
    let rig = bisonfi_rig().await;
    for (pool, data, tp) in &rig.quoting {
        let size = BisonfiRig::sell_size(data);
        let quote_reserve = u64::from_le_bytes(data[56..64].try_into().unwrap());
        let at = |r: u64| {
            rig.scenario(
                pool,
                data,
                *tp,
                "bisonfi-depth",
                &[("quote_reserve", serde_json::json!(r))],
                size,
                0,
            )
        };
        // Raising a reserve above the vault balance breaks settlement, which the template warns
        // about, so "deep" is the live value and the comparison runs downward from there.
        let deep = at(quote_reserve);
        let thin = at(quote_reserve / 2);
        let thinner = at(quote_reserve / 4);
        assert!(
            deep >= thin && thin >= thinner && deep > thinner,
            "{pool}: reducing the payout reserve must never improve the fill and must make it \
             strictly worse somewhere in the sweep, got {deep} -> {thin} -> {thinner}"
        );
    }
}

/// Asserts that a depth override moves the direction it is documented to move and leaves the
/// opposite direction alone.
///
/// `cross` is the untargeted direction (value, control); `own` is the targeted one. Deliberately not
/// an equality check on `cross`. The claim the template makes - and the only one a consumer relies on
/// - is that each reserve constrains one direction. Exact byte-identity of the untargeted quote is a
/// strictly stronger claim, and it is not one this program guarantees: the working ladder at 288/1036
/// is refreshed from 528/1196 through a watermark-gated memcpy, so a write that tips that gate can
/// shift both directions by a few bps without the documented asymmetry being wrong at all. That was
/// observed once in the wild - quartering base_reserve moved a sell 3.5 bps on DSzgmzz1 - and could
/// not be reproduced across a size sweep from 1/10000 of the reserve up to the whole of it, on any of
/// the six quoting markets, where the cross effect measured exactly 0.0000 bps.
///
/// So the tolerance below is not slack for a claim we cannot prove. It asserts the asymmetry itself:
/// the untargeted direction must stay within 50 bps, AND the targeted direction must move at least
/// ten times further. A lever that genuinely bled into both directions fails the ratio even when both
/// moves are individually small, which is what exact equality was really there to catch.
fn assert_direction_specific(
    pool: &str,
    field: &str,
    cross: (u64, u64),
    own: (Result<u64, String>, u64),
) {
    const CROSS_TOLERANCE: f64 = 0.005; // 50 bps
    const MIN_RATIO: f64 = 10.0;

    let (cross_val, cross_control) = cross;
    let cross_rel = (cross_val as f64 - cross_control as f64).abs() / cross_control as f64;
    assert!(
        cross_rel <= CROSS_TOLERANCE,
        "{pool}: lowering {field} moved the direction it should not constrain by {:.2} bps          ({cross_val} vs control {cross_control}). The template's direction guidance would be wrong",
        cross_rel * 10_000.0
    );

    // A refusal is an unboundedly large move on the targeted side, so the ratio is satisfied outright.
    let (own_val, own_control) = own;
    let own_rel = match own_val {
        Err(_) => f64::INFINITY,
        Ok(v) => (v as f64 - own_control as f64).abs() / own_control as f64,
    };
    assert!(
        own_rel >= cross_rel * MIN_RATIO,
        "{pool}: lowering {field} moved the direction it constrains by {:.2} bps but moved the          other direction by {:.2} bps. The two are within {MIN_RATIO}x, so this is not a          direction-specific lever and the template's guidance would mislead",
        own_rel * 10_000.0,
        cross_rel * 10_000.0
    );
}

/// SCENARIO: make a given market expensive in one direction only.
///
/// The pool pays out of one side, so lowering that side's reserve must hurt trades in that direction
/// and leave the other direction untouched. A router that treats the venue as symmetric fails here.
#[tokio::test]
async fn bisonfi_scenario_one_sided_liquidity() {
    let rig = bisonfi_rig().await;
    // Starve hard rather than gently. Quartering a reserve barely binds when the trade is only 2% of
    // it: on DSzgmzz1 a quartered base_reserve moved the buy it constrains by 0.5 bps while the ladder
    // refresh wobbled the sell by 2.7 bps, so the asymmetry was smaller than the noise and the ratio
    // below could not see it. Starving by 1000x drives the constrained direction to the point where
    // the reserve genuinely limits the fill, which is the regime the template's guidance describes.
    const STARVE: u64 = 1000;
    let mut checked = 0usize;
    for (pool, data, tp) in &rig.quoting {
        let sell = BisonfiRig::sell_size(data);
        let base_reserve = u64::from_le_bytes(data[48..56].try_into().unwrap());
        let quote_reserve = u64::from_le_bytes(data[56..64].try_into().unwrap());
        let sell_control = bisonfi_replay(&rig.elf, pool, data, *tp, sell, 0, |_| {})
            .unwrap_or_else(|e| panic!("{pool}: sell control failed: {e}"));
        let buy = BisonfiRig::buy_size(sell_control);
        let buy_control = bisonfi_replay(&rig.elf, pool, data, *tp, buy, 1, |_| {})
            .unwrap_or_else(|e| panic!("{pool}: buy control failed: {e}"));
        assert!(buy_control > 0, "{pool}: buy control returned nothing");
        checked += 1;

        // A refusal counts as strictly worse than any fill - the venue declining is the extreme end
        // of the same lever, and starving a reserve hard enough reaches it.
        let worse_than = |r: Result<u64, String>, control: u64| match r {
            Ok(o) => o < control,
            Err(_) => true,
        };

        // Starve the quote side: sells get worse, the opposite direction barely moves.
        let vals = [("quote_reserve", serde_json::json!(quote_reserve / STARVE))];
        let sell_starved = rig.try_scenario(pool, data, *tp, "bisonfi-depth", &vals, sell, 0);
        let buy_cross = rig.scenario(pool, data, *tp, "bisonfi-depth", &vals, buy, 1);
        assert!(
            worse_than(sell_starved.clone(), sell_control),
            "{pool}: lowering quote_reserve must make a SELL worse, got {sell_starved:?} vs \
             {sell_control}"
        );
        assert_direction_specific(
            pool,
            "quote_reserve",
            (buy_cross, buy_control),
            (sell_starved.clone(), sell_control),
        );

        // And the mirror image on the base side.
        let vals = [("base_reserve", serde_json::json!(base_reserve / STARVE))];
        let buy_starved = rig.try_scenario(pool, data, *tp, "bisonfi-depth", &vals, buy, 1);
        let sell_cross = rig.scenario(pool, data, *tp, "bisonfi-depth", &vals, sell, 0);
        assert!(
            worse_than(buy_starved.clone(), buy_control),
            "{pool}: lowering base_reserve must make a BUY worse, got {buy_starved:?} vs \
             {buy_control}"
        );
        assert_direction_specific(
            pool,
            "base_reserve",
            (sell_cross, sell_control),
            (buy_starved.clone(), buy_control),
        );
    }
    // Both directions must actually have been exercised, or a wrong notional skips the buy leg
    // silently.
    assert_eq!(
        checked,
        rig.quoting.len(),
        "only {checked} of the quoting markets exercised both directions of the depth template"
    );
}

/// A maker that stopped publishing: the venue pays nothing, and says so with a SUCCESSFUL
/// transaction. Keep-alive keeps a registered scenario's pool from getting here; this pins what the
/// program does when a quote is stale.
///
/// This is the behaviour no constant-product AMM can imitate - an AMM always quotes something - so
/// it is the case most likely to be untested on the consuming side.
#[tokio::test]
async fn bisonfi_a_stale_quote_pays_nothing_and_ignores_min_out() {
    const STALE_BY: u64 = 1_000_000;

    let rig = bisonfi_rig().await;
    let mut checked = 0usize;
    for (pool, data, tp) in &rig.quoting {
        let size = BisonfiRig::sell_size(data);
        let published = u64::from_le_bytes(data[72..80].try_into().unwrap());

        // The swap's minimum-output bound is NOT honoured on the stale path, so the caller gets a
        // CONFIRMED transaction that moved nothing and ignored their slippage protection. The
        // healthy control below proves the bound is otherwise real, so this is the program
        // returning early rather than the harness failing to set the field.
        let healthy = bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, |_| {})
            .unwrap_or_else(|e| panic!("{pool}: control failed: {e}"));
        assert!(
            bisonfi_replay_min_out(&rig.elf, pool, data, *tp, size, healthy + 1, 0, |_| {})
                .is_err(),
            "{pool}: a minimum above the fillable amount must revert on a healthy market, or the \
             bound is not a slippage guard at all and the claim below means nothing"
        );
        // The exact tolerance is maker policy and has varied between live pools/builds, so the
        // quote is aged far beyond any observed threshold.
        let ignored = bisonfi_replay_min_out(
            &rig.elf,
            pool,
            data,
            *tp,
            size,
            healthy,
            0,
            move |d: &mut Vec<u8>| {
                let stale = published
                    .checked_sub(STALE_BY)
                    .expect("live publication slot must exceed the deliberate stale age");
                d[72..80].copy_from_slice(&stale.to_le_bytes());
            },
        );
        assert_eq!(
            ignored,
            Ok(0),
            "{pool}: a stale market must succeed with zero even when the caller demands \
             {healthy} out - if this ever starts reverting, the symptom changed from a silent no-op \
             to a failed transaction and every consumer's handling changes with it"
        );
        checked += 1;
    }
    assert_eq!(
        checked,
        rig.quoting.len(),
        "only {checked} of the quoting markets were checked"
    );
}

/// SCENARIO: make a given market unable to fill a trade at all.
///
/// The extreme end of the depth lever: starve the payout reserve far enough and the program stops
/// negotiating and refuses, rather than quoting a terrible price. That is a distinct thing for a
/// router to handle - it has to split the trade or fall back to another venue - so it is asserted
/// separately from ordinary slippage.
#[tokio::test]
async fn bisonfi_scenario_market_cannot_fill() {
    let rig = bisonfi_rig().await;
    for (pool, data, tp) in &rig.quoting {
        let size = BisonfiRig::sell_size(data);
        let result = rig.try_scenario(
            pool,
            data,
            *tp,
            "bisonfi-depth",
            &[("quote_reserve", serde_json::json!(0))],
            size,
            0,
        );
        assert!(
            matches!(result, Err(_) | Ok(0)),
            "{pool}: quote_reserve 0 must prevent a sell from filling, got {result:?}"
        );
    }
}

/// Every property of the spread template, and the tick offsets each one is supposed to cover.
const BISONFI_SPREAD_PROPS: [(&str, usize, usize); 8] = [
    ("working_levels.0.tick_offset", 300, 4),
    ("working_levels.4.tick_offset", 364, 4),
    ("configured_levels.0.tick_offset", 540, 4),
    ("configured_levels.4.tick_offset", 604, 4),
    ("continuation_levels.0.tick_offset", 1048, 5),
    ("continuation_levels.5.tick_offset", 1128, 5),
    ("continuation_source_levels.0.tick_offset", 1208, 5),
    ("continuation_source_levels.5.tick_offset", 1288, 5),
];

/// The spread template's centre-depth properties: (path, offset, bytes written). Every market fills
/// a slice at the mid itself before the ladder starts, and the program restores that slice from the
/// fresh and aged depth tables, so all four places have to be emptied together.
const BISONFI_CENTRE_DEPTH_PROPS: [(&str, usize, usize); 4] = [
    ("working_levels.8.depth", 416, 8),
    ("configured_levels.8.depth", 656, 8),
    ("fresh_depth_table.0", 1536, 2),
    ("aged_depth_table.0", 1556, 2),
];

/// The centre-depth properties all set to 0, which makes the ladder's spread the spread a trade pays.
fn bisonfi_centre_emptied() -> Vec<(&'static str, serde_json::Value)> {
    BISONFI_CENTRE_DEPTH_PROPS
        .iter()
        .map(|(path, _, _)| (*path, serde_json::json!(0)))
        .collect()
}

/// The bid half of the spread template's properties, all set to `v`.
///
/// The bid runs are the ones starting at rung 0 of each region; the ask runs start mid-region. Both
/// halves are needed to move a two-sided book, but a sell only pays the bid side, so tests that
/// measure a sell set just these.
fn bisonfi_spread_bids(v: i32) -> Vec<(&'static str, serde_json::Value)> {
    BISONFI_SPREAD_PROPS
        .iter()
        .filter(|(path, _, _)| path.contains(".0."))
        .map(|(path, _, _)| (*path, serde_json::json!(v)))
        .collect()
}

/// Builds a mutation closure that applies a shipped template through the real `materialize` path.
///
/// For tests that iterate the raw pool list directly instead of going through `BisonfiRig`, so that
/// they still exercise the template we ship rather than a hand-written copy of its offsets.
fn bisonfi_apply_template(
    template_id: &str,
    values: &[(&str, serde_json::Value)],
) -> impl FnOnce(&mut Vec<u8>) + use<> {
    let id = template_id.to_string();
    let registry = TemplateRegistry::new();
    let template = registry
        .get(template_id)
        .unwrap_or_else(|| panic!("{template_id} must exist in the registry"))
        .clone();
    assert!(
        template.raw_layout,
        "{template_id} must use raw-layout writes"
    );
    let map: HashMap<String, serde_json::Value> = values
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    move |d: &mut Vec<u8>| {
        *d = template
            .materialize_raw_layout(d.as_slice(), &map, 0)
            .unwrap_or_else(|e| panic!("{id}: materialize failed: {e}"));
    }
}

/// Values setting the whole book to one magnitude: bid properties negative, ask properties positive.
/// The centre is emptied too, as the template documents for an exact spread.
fn bisonfi_spread_values(magnitude: i32) -> Vec<(&'static str, serde_json::Value)> {
    BISONFI_SPREAD_PROPS
        .iter()
        .map(|(path, _, _)| {
            // The bid properties are the ones whose run starts at the first rung of a region.
            let is_bid = path.contains(".0.");
            let v = if is_bid {
                -magnitude.abs()
            } else {
                magnitude.abs()
            };
            (*path, serde_json::json!(v))
        })
        .chain(bisonfi_centre_emptied())
        .collect()
}

/// SCENARIO: set X spread for a given market.
///
/// The decisive form of the claim, and the one three earlier attempts got wrong by comparing spread
/// *differences* - which is blind to a change that shifts both legs equally. This compares two uniform
/// settings against each other, so the ratio is fully determined by the unit:
///
///   price(T) = mid * (1 - T/2_560_000)  =>  price(T1)/price(T2) = (1 - T1/u) / (1 - T2/u)
///
/// Any multiplicative term the venue applies regardless - its base spread, a fee - cancels in that
/// ratio, and no per-market token decimals enter it either. So it tests the unit absolutely with
/// nothing fitted.
#[tokio::test]
async fn bisonfi_scenario_set_spread() {
    const UNIT: f64 = 2_560_000.0;
    const TIGHT: i32 = 2_560; // 10 bps
    const WIDE: i32 = 25_600; // 1%
    let predicted = (1.0 - WIDE as f64 / UNIT) / (1.0 - TIGHT as f64 / UNIT);

    let rig = bisonfi_rig().await;
    let mut checked = 0usize;
    for (pool, data, tp) in &rig.quoting {
        let base_reserve = u64::from_le_bytes(data[48..56].try_into().unwrap());
        // Pick the first size this market can fill where the quote ladder is actually engaged.
        let size = [20u64, 50, 100, 200, 1000].into_iter().find_map(|div| {
            let size = base_reserve / div;
            let control = bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, |_| {}).ok()?;
            if control == 0 {
                return None;
            }
            let wide = rig
                .try_scenario(
                    pool,
                    data,
                    *tp,
                    "bisonfi-spread",
                    &bisonfi_spread_values(WIDE),
                    size,
                    0,
                )
                .ok()?;
            (wide < control).then_some(size)
        });
        let Some(size) = size else {
            continue;
        };
        let at = |magnitude: i32| {
            rig.try_scenario(
                pool,
                data,
                *tp,
                "bisonfi-spread",
                &bisonfi_spread_values(magnitude),
                size,
                0,
            )
        };
        let (tight, wide) = match (at(TIGHT), at(WIDE)) {
            (Ok(t), Ok(w)) if t > 0 && w > 0 => (t, w),
            _ => continue,
        };
        assert!(
            wide < tight,
            "{pool}: a 1% ladder must pay the seller less than a 10 bps one, got {wide} vs {tight}"
        );
        // The whole move is under 1%, so a tolerance on the
        // ratio itself would also pass a ladder that barely moved.
        let ratio = wide as f64 / tight as f64;
        let err = ((1.0 - ratio) - (1.0 - predicted)).abs() / (1.0 - predicted);
        assert!(
            err < 0.05,
            "{pool}: going from a {TIGHT} tick to a {WIDE} tick cut the fill by {:.4}%, but the \
             1/2,560,000 unit the template documents requires {:.4}% ({:.2}% off). Either the unit \
             is wrong or not every region is being written",
            (1.0 - ratio) * 100.0,
            (1.0 - predicted) * 100.0,
            err * 100.0
        );
        checked += 1;
    }
    assert_eq!(
        checked,
        rig.quoting.len(),
        "only {checked} of the quoting markets exercised the spread template; the claim needs the live markets"
    );
}

/// SCENARIO: quote wide on one side only.
///
/// The parity test for the spread template, matching what `bisonfi_scenario_one_sided_liquidity` does
/// for depth. Bid offsets price sells and ask offsets price buys, so widening one side must leave the
/// other untouched. A template whose bid and ask offsets were transposed would still widen a quote and
/// would pass every test that only looks at one direction.
#[tokio::test]
async fn bisonfi_scenario_spread_is_side_specific() {
    const WIDE: i32 = 25_600; // 1%
    let bids: Vec<(&str, serde_json::Value)> = BISONFI_SPREAD_PROPS
        .iter()
        .filter(|(p, _, _)| p.contains(".0."))
        .map(|(p, _, _)| (*p, serde_json::json!(-WIDE)))
        .collect();
    let asks: Vec<(&str, serde_json::Value)> = BISONFI_SPREAD_PROPS
        .iter()
        .filter(|(p, _, _)| !p.contains(".0."))
        .map(|(p, _, _)| (*p, serde_json::json!(WIDE)))
        .collect();

    let rig = bisonfi_rig().await;
    let mut checked = 0usize;
    for (pool, data, tp) in &rig.quoting {
        let base_reserve = u64::from_le_bytes(data[48..56].try_into().unwrap());
        // No single trade size works everywhere: DSzgmzz1 does not consult the ladder below 5% of its
        // reserve, and AfaA4CE8 cannot fill 5% at all. So the size is chosen per market - the first
        // that fills both directions AND actually engages the ladder.
        let usable = [20u64, 50, 100, 200, 1000].into_iter().find_map(|div| {
            let sell = base_reserve / div;
            let sell_control = bisonfi_replay(&rig.elf, pool, data, *tp, sell, 0, |_| {}).ok()?;
            if sell_control == 0 {
                return None;
            }
            let buy = BisonfiRig::buy_size(sell_control);
            let buy_control = bisonfi_replay(&rig.elf, pool, data, *tp, buy, 1, |_| {}).ok()?;
            if buy_control == 0 {
                return None;
            }
            // The ladder has to bite at this size, or the assertions below are vacuous.
            let probe = rig
                .try_scenario(pool, data, *tp, "bisonfi-spread", &bids, sell, 0)
                .ok()?;
            (probe < sell_control).then_some((sell, sell_control, buy, buy_control))
        });
        let Some((sell, sell_control, buy, buy_control)) = usable else {
            continue;
        };

        // Widening the bid must hurt sells and leave buys where they were. Some markets price a trade
        // off both sides' ticks: by a fraction of a basis point, and a few on a trade deep into the
        // ladder. So the widened side must move at least ten times more, relative to its control,
        // than the other; a transposed template would move both sides alike.
        let tenth = |other: (u64, u64), widened: (u64, u64)| {
            other.0.abs_diff(other.1) as u128 * widened.1 as u128 * 10
                <= widened.0.abs_diff(widened.1) as u128 * other.1 as u128
        };
        let sell_wide = rig.scenario(pool, data, *tp, "bisonfi-spread", &bids, sell, 0);
        let buy_untouched = rig.scenario(pool, data, *tp, "bisonfi-spread", &bids, buy, 1);
        assert!(
            sell_wide < sell_control,
            "{pool}: widening the BID must pay a seller less, got {sell_wide} vs {sell_control}"
        );
        assert!(
            tenth((buy_untouched, buy_control), (sell_wide, sell_control)),
            "{pool}: widening the BID must not change a BUY, got {buy_untouched} vs {buy_control}. \
             If this fires the bid and ask offsets are transposed in the template"
        );

        // And the mirror image.
        let buy_wide = rig.scenario(pool, data, *tp, "bisonfi-spread", &asks, buy, 1);
        let sell_untouched = rig.scenario(pool, data, *tp, "bisonfi-spread", &asks, sell, 0);
        assert!(
            buy_wide < buy_control,
            "{pool}: widening the ASK must give a buyer less base, got {buy_wide} vs {buy_control}"
        );
        assert!(
            tenth((sell_untouched, sell_control), (buy_wide, buy_control)),
            "{pool}: widening the ASK must not change a SELL, got {sell_untouched} vs {sell_control}"
        );
        checked += 1;
    }
    assert!(
        checked >= 4,
        "only {checked} markets exercised both directions of the spread template"
    );
}

/// The write half: each property must set exactly its own run of tick fields and nothing else.
///
/// A strided encoding writes several disjoint four-byte spans, so "nothing outside the field moved" is
/// a different assertion from every other property in this protocol - and getting it wrong would mean
/// silently overwriting a rung's share or level.
#[tokio::test]
async fn bisonfi_spread_template_writes_only_its_tick_fields() {
    let all = fetch(&BISONFI_ALL_POOLS).await;
    let registry = TemplateRegistry::new();
    let template = registry.get("bisonfi-spread").expect("spread template");
    assert!(template.raw_layout, "spread uses raw-layout writes");

    for (path, offset, count) in BISONFI_SPREAD_PROPS {
        let expected: Vec<usize> = (0..count)
            .flat_map(|i| {
                let at = offset + i * BISONFI_RUNG;
                at..at + 4
            })
            .collect();
        for (pool, data) in BISONFI_ALL_POOLS.iter().zip(all.iter()) {
            let forged = template
                .materialize_raw_layout(
                    data,
                    &HashMap::from([(path.to_string(), serde_json::json!(-12_345i32))]),
                    0,
                )
                .unwrap_or_else(|e| panic!("{path} on {pool}: {e}"));
            assert_eq!(forged.len(), 2048, "{path} on {pool}: size changed");
            for i in diff_indices(&forged, data) {
                assert!(
                    expected.contains(&i),
                    "{path} on {pool}: byte {i} changed, outside the {count} tick fields at \
                     {offset} stride 16. A strided write must not touch a rung's share or level"
                );
            }
            // And every slot in the run actually received the value.
            for i in 0..count {
                let at = offset + i * BISONFI_RUNG;
                let got = i32::from_le_bytes(forged[at..at + 4].try_into().unwrap());
                assert_eq!(
                    got, -12_345,
                    "{path} on {pool}: rung {i} at offset {at} did not receive the value"
                );
            }
        }
    }

    for (path, offset, width) in BISONFI_CENTRE_DEPTH_PROPS {
        for (pool, data) in BISONFI_ALL_POOLS.iter().zip(all.iter()) {
            let forged = template
                .materialize_raw_layout(
                    data,
                    &HashMap::from([(path.to_string(), serde_json::json!(0))]),
                    0,
                )
                .unwrap_or_else(|e| panic!("{path} on {pool}: {e}"));
            for i in diff_indices(&forged, data) {
                assert!(
                    (offset..offset + width).contains(&i),
                    "{path} on {pool}: byte {i} changed, outside {offset}..{}",
                    offset + width
                );
            }
            assert!(
                forged[offset..offset + width].iter().all(|b| *b == 0),
                "{path} on {pool}: the centre depth was not emptied"
            );
        }
    }
}

/// The live Orca Whirlpool SOL/USDC market the swap harness is checked against.
const WHIRLPOOL_SOL_USDC: &str = "HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ";

/// A long run stays quoting with nothing scheduled after its first override: keep-alive republishes
/// the pool every slot while the scenario is registered.
#[tokio::test]
async fn bisonfi_keep_alive_keeps_a_long_run_quoting() {
    use surfpool_types::{AccountAddress, OverrideInstance, Scenario};

    const RUN_FOR: u64 = 64;

    let rig = bisonfi_rig().await;
    let (pool, data, tp) = rig.quoting.first().expect("a quoting market");
    let pool_key = pool.parse::<Pubkey>().expect("pool address");
    let base_slot = u64::from_le_bytes(data[72..80].try_into().unwrap());
    let size = BisonfiRig::sell_size(data);

    // The surfnet starts at the pool's own slot, as a surfnet forked at that moment would.
    let (mut svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    svm.set_latest_absolute_slot(base_slot);
    svm.inner
        .set_account(
            pool_key,
            solana_account::Account {
                lamports: 1_000_000,
                data: data.clone(),
                owner: Pubkey::from_str_const(BISONFI_PROGRAM),
                executable: false,
                rent_epoch: 0,
            },
        )
        .expect("seed the pool account");

    // Any BisonFi override keeps its pool alive; this one restates the mid the pool already quotes.
    let fair_value = u128::from_le_bytes(data[832..848].try_into().unwrap());
    let mut scenario = Scenario::new(
        "BisonFi stays live during a long run".to_string(),
        "Restates the mid once; keep-alive republishes the quote every later slot".to_string(),
    );
    scenario.add_override(
        OverrideInstance::new(
            "bisonfi-fair-value".to_string(),
            0,
            AccountAddress::Pubkey(pool_key.to_string()),
        )
        .with_values(HashMap::from([(
            "fair_value".to_string(),
            serde_json::json!(fair_value.to_string()),
        )])),
    );
    svm.register_scenario(scenario, Some(base_slot))
        .expect("register scenario");
    svm.materialize_overrides_for_slot(&None, base_slot)
        .await
        .expect("materialize the override");
    for _ in 0..RUN_FOR {
        svm.confirm_current_block().expect("produce a block");
        svm.materialize_overrides(&None)
            .await
            .expect("materialize keep-alive");
    }

    let slot = svm.get_latest_absolute_slot();
    assert!(slot >= base_slot + RUN_FOR, "the run must have advanced");
    let kept = svm
        .inner
        .get_account(&pool_key)
        .expect("get_account")
        .expect("account present")
        .data;
    assert_eq!(
        u64::from_le_bytes(kept[72..80].try_into().unwrap()),
        slot,
        "keep-alive must have republished the quote in the current slot"
    );
    assert_eq!(
        u64::from_le_bytes(kept[88..96].try_into().unwrap()),
        svm.updated_at * 1_000_000,
        "keep-alive must have stamped the quote with the Clock's time"
    );

    // A replay runs its clock at the pool's publication, so the kept image swaps at the surfnet's
    // current slot, and the pool as fetched swaps at that same clock as a quote RUN_FOR slots old.
    let live = bisonfi_replay(&rig.elf, pool, &kept, *tp, size, 0, |_| {})
        .expect("the kept-alive market must execute");
    assert!(live > 0, "a kept-alive market must keep quoting");
    let stale = bisonfi_replay(&rig.elf, pool, &kept, *tp, size, 0, {
        let fetched = data.clone();
        move |account| *account = fetched
    });
    assert_eq!(
        stale,
        Ok(0),
        "without keep-alive the quote must be stale {RUN_FOR} slots later"
    );
}

/// Plays a registered scenario on `svm` from `base` through `last` as the block producer does, and
/// returns the pool's account image at each slot. Keep-alive republishes the pool in every slot, so
/// each image swaps at its own clock.
async fn bisonfi_play(
    svm: &mut SurfnetSvm,
    pool: Pubkey,
    base: u64,
    last: u64,
) -> HashMap<u64, Vec<u8>> {
    let mut images = HashMap::new();
    for slot in base..=last {
        if slot == base {
            svm.materialize_overrides_for_slot(&None, slot)
                .await
                .expect("materialize the first slot");
        } else {
            svm.confirm_current_block().expect("produce a block");
            svm.materialize_overrides(&None)
                .await
                .expect("materialize the slot");
        }
        assert_eq!(svm.get_latest_absolute_slot(), slot);
        let image = svm
            .inner
            .get_account(&pool)
            .expect("get_account")
            .expect("account present")
            .data;
        assert_eq!(
            u64::from_le_bytes(image[72..80].try_into().unwrap()),
            slot,
            "keep-alive must republish the pool in every slot"
        );
        images.insert(slot, image);
    }
    images
}

/// SCENARIO: the mid MOVES between the quote and the fill - adverse selection.
///
/// The other half of the mid-flight pair, and the contrast is the point. When the maker goes dark the
/// swap silently returns zero. When the maker simply reprices against the taker, the caller's own
/// minimum-output bound catches it and the transaction REVERTS. Same timing, same mechanism, two
/// completely different things for a consumer to handle - one detectable, one not.
#[tokio::test]
async fn bisonfi_scenario_mid_moves_between_quote_and_fill() {
    use surfpool_types::{AccountAddress, OverrideInstance, Scenario};

    const FILL_AT: u64 = 2;

    let rig = bisonfi_rig().await;
    let (pool, data, tp) = rig.quoting.first().expect("a quoting market");
    let pool_key = pool.parse::<Pubkey>().expect("pool address");
    let base_slot = u64::from_le_bytes(data[72..80].try_into().unwrap());
    let mid = u128::from_le_bytes(data[832..848].try_into().unwrap());
    let size = BisonfiRig::sell_size(data);
    let moved = mid * 9 / 10; // the maker marks the asset down 10% while the taker is in flight

    let (mut svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    svm.inner
        .set_account(
            pool_key,
            solana_account::Account {
                lamports: 1_000_000,
                data: data.clone(),
                owner: Pubkey::from_str_const(BISONFI_PROGRAM),
                executable: false,
                rent_epoch: 0,
            },
        )
        .expect("seed the pool account");
    // The surfnet starts at the pool's own slot, as a surfnet forked at that moment would.
    svm.set_latest_absolute_slot(base_slot);

    let mut scenario = Scenario::new(
        "BisonFi reprices mid-flight".to_string(),
        "Quotes one price at the slot the caller prices on and a worse one before the transaction \
         executes"
            .to_string(),
    );
    for (relative, value) in [(0u64, mid), (FILL_AT, moved)] {
        scenario.add_override(
            OverrideInstance::new(
                "bisonfi-fair-value".to_string(),
                relative,
                AccountAddress::Pubkey(pool_key.to_string()),
            )
            .with_values(HashMap::from([(
                "fair_value".to_string(),
                serde_json::json!(value.to_string()),
            )])),
        );
    }
    svm.register_scenario(scenario, Some(base_slot))
        .expect("register scenario");

    let images = bisonfi_play(&mut svm, pool_key, base_slot, base_slot + FILL_AT).await;
    let mid_at = |slot: u64| u128::from_le_bytes(images[&slot][832..848].try_into().unwrap());
    assert_eq!(
        mid_at(base_slot),
        mid,
        "the quoting slot must carry the quoted price"
    );
    assert_eq!(
        mid_at(base_slot + FILL_AT),
        moved,
        "the repricing step must have fired by the slot the transaction lands on"
    );

    // What the caller quoted, and therefore the minimum they would sign for.
    let quoted = bisonfi_replay(&rig.elf, pool, &images[&base_slot], *tp, size, 0, |_| {})
        .expect("the quoting slot must fill");
    assert!(quoted > 0, "the caller's quote has to be real");

    // The same transaction, landing after the reprice. Without a minimum it fills at the worse price;
    // with the minimum the caller actually quoted, it reverts.
    let image = images[&(base_slot + FILL_AT)].clone();
    let unprotected = bisonfi_replay(&rig.elf, pool, &image, *tp, size, 0, |_| {})
        .expect("a repriced market still quotes, just worse");
    assert!(
        unprotected < quoted,
        "a 10% markdown must pay the seller less: got {unprotected} against a quote of {quoted}"
    );

    let protected = bisonfi_replay_min_out(&rig.elf, pool, &image, *tp, size, quoted, 0, |_| {});
    assert!(
        protected.is_err(),
        "signing for the price that was quoted must REVERT once the maker has repriced, got \
         {protected:?}. This is the case a consumer can actually detect, unlike a dark maker"
    );
}

/// SCENARIO: a dislocated price behind thin depth, so an arbitrage looks profitable at the quoted
/// mid and is worth materially less once the trade is actually filled.
///
/// This is the composition of two templates in one scenario, and it is the one that catches a
/// consumer whose price-impact model is wrong rather than one that simply misreads a price.
#[tokio::test]
async fn bisonfi_scenario_dislocated_price_behind_thin_depth() {
    let rig = bisonfi_rig().await;
    let mut checked = 0usize;
    for (pool, data, tp) in &rig.quoting {
        let size = BisonfiRig::sell_size(data);
        let mid = u128::from_le_bytes(data[832..848].try_into().unwrap());
        let quote_reserve = u64::from_le_bytes(data[56..64].try_into().unwrap());
        let dislocated = mid * 11 / 10; // the venue claims 10% above the market

        let control = bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, |_| {})
            .unwrap_or_else(|e| panic!("{pool}: control failed: {e}"));
        // Price alone: the full 10% should show up in the fill.
        let price_only = rig.scenario(
            pool,
            data,
            *tp,
            "bisonfi-fair-value",
            &[("fair_value", serde_json::json!(dislocated.to_string()))],
            size,
            0,
        );
        // Now the same dislocation with the payout side starved. Both templates target the same
        // account, so the scenario applies them together.
        let registry = TemplateRegistry::new();
        let template = registry
            .get("bisonfi-fair-value")
            .expect("fair value template");
        assert!(template.raw_layout, "fair value uses raw-layout writes");
        let priced = template
            .materialize_raw_layout(
                data,
                &HashMap::from([(
                    "fair_value".to_string(),
                    serde_json::json!(dislocated.to_string()),
                )]),
                0,
            )
            .expect("price override");
        let both = rig.scenario(
            pool,
            &priced,
            *tp,
            "bisonfi-depth",
            &[("quote_reserve", serde_json::json!(quote_reserve / 4))],
            size,
            0,
        );

        assert!(
            price_only > control,
            "{pool}: a 10% higher mid must pay more, got {price_only} vs {control}"
        );
        assert!(
            both < price_only,
            "{pool}: starving the payout side must claw back part of the dislocation; the arb has \
             to look better on paper ({price_only}) than it fills ({both})"
        );
        assert!(
            both > control,
            "{pool}: the dislocation should still be worth something after slippage, got {both} vs \
             {control}"
        );
        checked += 1;
    }
    assert_eq!(
        checked,
        rig.quoting.len(),
        "only {checked} of the quoting markets exercised the combined scenario"
    );
}

/// The spread template writes fixed offsets - 540 for level 1 of the bid, 604 for level 1 of the ask
/// and so on - which is only correct if every pool lays its ladder out the same way. This asserts the
/// invariant the template depends on, so a pool that ordered its rungs differently would fail here
/// rather than silently take a price offset meant for the other side of the book.
#[tokio::test]
async fn bisonfi_ladder_layout_is_uniform_across_every_pool() {
    let all = fetch(&BISONFI_ALL_POOLS).await;
    let i32_at = |d: &[u8], o: usize| i32::from_le_bytes(d[o..o + 4].try_into().unwrap());
    let u32_at = |d: &[u8], o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());

    for (pool, data) in BISONFI_ALL_POOLS.iter().zip(all.iter()) {
        for table in [BISONFI_LADDER, BISONFI_LADDER_MIRROR] {
            let mut ask_share_total = 0u64;
            for rung in 0..8usize {
                let o = table + rung * BISONFI_RUNG;
                let level = i32_at(data, o + 8);
                let tick = i32_at(data, o + 12);
                // Rungs 0..3 are the bid side at levels -1..-4, rungs 4..7 the ask side at 1..4.
                let expected = if rung < 4 {
                    -(rung as i32 + 1)
                } else {
                    rung as i32 - 3
                };
                assert_eq!(
                    level, expected,
                    "{pool} table {table} rung {rung}: level is {level}, expected {expected}. The \
                     spread template writes offsets on the assumption that rungs 0-3 are the bid \
                     side and 4-7 the ask side"
                );
                // A bid offset must never be above the mid and an ask offset never below it, or the
                // venue would be quoting through itself.
                if rung < 4 {
                    assert!(
                        tick <= 0,
                        "{pool} table {table} rung {rung}: bid tick {tick} > 0"
                    );
                } else {
                    assert!(
                        tick >= 0,
                        "{pool} table {table} rung {rung}: ask tick {tick} < 0"
                    );
                }
                ask_share_total += u32_at(data, o) as u64;
            }
            // Offsets must widen outward, otherwise "level 4 dominates a large trade" is not true and
            // the template's guidance would mislead.
            for rung in [0usize, 1, 2, 4, 5, 6] {
                let inner = i32_at(data, table + rung * BISONFI_RUNG + 12).abs();
                let outer = i32_at(data, table + (rung + 1) * BISONFI_RUNG + 12).abs();
                assert!(
                    outer >= inner,
                    "{pool} table {table}: rung {} offset {outer} is closer to the mid than rung \
                     {rung}'s {inner}; the ladder is supposed to widen outward",
                    rung + 1
                );
            }
            // Shares are basis points of the book, so the side cannot allocate more than all of it.
            // Note the 9999 the program checks at instruction 19120 is an overflow guard on the high
            // word of a share*amount product, NOT a bound on this sum: pool 7ZTpmqKW... allocates a
            // full 10000, and reading the code's 9999 as a sum limit is what this assertion caught.
            assert!(
                ask_share_total <= 10_000,
                "{pool} table {table}: ask shares sum to {ask_share_total} bps, more than the whole \
                 book"
            );
        }
    }
}

/// The behavioural half of the spread claim, on every pool that can quote: widening the ladder must
/// make a sell strictly worse, tightening it must make it strictly better, and writing the mirrored
/// table at 288 must change nothing at all.
///
/// The last assertion is the one that matters most. Table 288 looks exactly like a ladder, is the
/// same size, sits at a lower offset, and is what an earlier version of this work assumed was live.
/// It is inert, so a template pointed at it would appear to write cleanly and silently do nothing.
#[tokio::test]
async fn bisonfi_spread_lever_moves_the_quote_on_every_pool() {
    let elf = bisonfi_elf().await;
    let all = fetch(&BISONFI_ALL_POOLS).await;
    let programs = bisonfi_token_programs(&all).await;
    const WIDE: i32 = -25_600; // 1% below mid
    const TIGHT: i32 = -13; // about 5 ppm below mid
    /// The spread the two tick values differ by. A uniform write puts every slice of the trade at the
    /// same offset, so the realized gap should approach this and can never exceed it.
    const EXPECTED_GAP: f64 = (TIGHT - WIDE) as f64 / 2_560_000.0;

    // The ladder engages over a window of trade size that differs per market and falls away again on
    // very large trades, so the claim is per pool: SOME size pays essentially the whole configured
    // spread. Asserting a single fixed size would be asserting a coincidence.
    let divs: [u64; 8] = [1000, 200, 100, 50, 20, 10, 4, 2];
    let mut peaks: Vec<(&str, f64, u64)> = Vec::new();

    for ((pool, data), tp) in BISONFI_ALL_POOLS
        .iter()
        .zip(all.iter())
        .zip(programs.iter())
    {
        let base_reserve = u64::from_le_bytes(data[48..56].try_into().unwrap());
        if base_reserve < 1_000_000 {
            continue; // dormant market, nothing to price
        }
        // Drive the SHIPPED template, not raw offsets. This matters and is not stylistic: the
        // template writes the bid ticks of all four ladder regions, where the older raw-offset version
        // of this test wrote only two of them. A market whose watermark gate happens to be blocking
        // the 528 -> 288 refresh prices out of the working copy, so writing only the source region
        // moves nothing at all. DSzgmzz1 was in exactly that state and reported 0.0000% of the
        // configured spread while the template reaches it. Going through the template also means this
        // per-pool proof covers what we actually ship rather than a parallel implementation of it.
        let set_bids = |v: i32| bisonfi_apply_template("bisonfi-spread", &bisonfi_spread_bids(v));
        let mut best = (0.0f64, 0u64);
        let mut quoted = false;

        for div in divs {
            let size = base_reserve / div;
            let baseline = match bisonfi_replay(&elf, pool, data, *tp, size, 0, |_| {}) {
                Ok(out) if out > 0 => out,
                _ => continue,
            };
            quoted = true;
            let wide = bisonfi_replay(&elf, pool, data, *tp, size, 0, set_bids(WIDE))
                .unwrap_or_else(|e| panic!("{pool} at 1/{div} of reserve: widening failed: {e}"));
            let tight = bisonfi_replay(&elf, pool, data, *tp, size, 0, set_bids(TIGHT))
                .unwrap_or_else(|e| panic!("{pool} at 1/{div} of reserve: tightening failed: {e}"));
            assert!(
                tight >= wide,
                "{pool} at 1/{div} of reserve: a 5 ppm spread paid {tight} and a 1% spread paid \
                 {wide}; widening the ladder must never pay the seller more"
            );

            let gap = (tight - wide) as f64 / tight as f64;
            // The hard ceiling. A uniform write puts every slice at the same offset, so the realized
            // gap cannot exceed what the two tick values differ by - if it does, the unit is wrong.
            assert!(
                gap <= EXPECTED_GAP * 1.02,
                "{pool} at 1/{div} of reserve: realized gap {:.4}% exceeds the {:.4}% the tick \
                 difference allows, so the 1/2,560,000 unit the template documents is wrong",
                gap * 100.0,
                EXPECTED_GAP * 100.0
            );
            if gap > best.0 {
                best = (gap, div);
            }

            // 288 and 1036 are working copies refreshed from 528 and 1196 by a watermark-gated
            // memcpy (traced at 10310-10387), and the template writes all four regions, so there is
            // no unmeasured case to leave un-asserted here.
            let _ = baseline;
        }

        if quoted {
            assert!(
                best.0 >= EXPECTED_GAP * 0.80,
                "{pool}: the best of {} trade sizes paid only {:.4}% of spread where {:.4}% was \
                 configured. The lever has to reach close to what it is set to on every market that \
                 quotes, or the template's unit and guidance would mislead",
                divs.len(),
                best.0 * 100.0,
                EXPECTED_GAP * 100.0
            );
            peaks.push((pool, best.0, best.1));
        }
    }

    // Without this the whole test could pass while quoting on nothing at all. Every pool the maker
    // republishes and has not halted must have priced; dormant, halted and Token-2022 pools, which the
    // harness cannot build accounts for (Custom(60)), return no quote whatever is written to them.
    let silent: Vec<&str> = bisonfi_pools_expected_to_quote(&all, &programs)
        .into_iter()
        .map(|index| BISONFI_ALL_POOLS[index])
        .filter(|pool| !peaks.iter().any(|(priced, _, _)| priced == pool))
        .collect();
    assert!(
        silent.is_empty() && !peaks.is_empty(),
        "{silent:?} are republished and not halted but produced no quote at any size ({peaks:?}). \
         This test proves nothing if the markets are not actually pricing"
    );
}

const BISONFI_PROGRAMDATA: &str = "42snJ7ip4zKKsip3EtaMoBo8wzoRsQJSzgUSFXAVJFfG";

const BISONFI_NINTH: &str = "8xeaWCsJYxRoudEZGJWURdfrtFhLYZz9b4iHJnW5tb3d";

/// The control the whole exercise needed: the deployed program, entered against pool state loaded
/// from the upstream datasource, prices a swap. BisonFi also requires LastRestartSlot to describe
/// the local VM's start slot; Surfpool initializes the same sysvar during startup.
///
/// Asserts the fill lands just below the pool's own published mid, which is the end-to-end check
/// that `fair_value` is the price this venue actually quotes on.
#[tokio::test]
async fn bisonfi_swap_replay_prices_near_the_published_mid() {
    const ONE_SOL: u64 = 1_000_000_000;

    let fork = bisonfi_fork(BISONFI_POOL).await;
    let mid = u128::from_le_bytes(fork.pool[832..848].try_into().unwrap()) as f64 / 2f64.powi(88);
    let out = bisonfi_run(&fork, ONE_SOL, 0, |_| {})
        .expect("the loaded pool should price a one SOL sell");
    assert!(out > 0, "a usable pool should quote a non-zero amount");

    // USDC has six decimals, so `out` is the quote in micro-units for one whole SOL.
    let realized = out as f64 / 1e6;
    let shortfall_ppm = (mid - realized) / mid * 1e6;
    assert!(
        (0.0..2_000.0).contains(&shortfall_ppm),
        "a one SOL sell should fill just below the published mid of {mid}, got {realized} \
         ({shortfall_ppm:.1} ppm away)"
    );
}

/// Harness control. Proves the replay rig propagates a signer and has the token program loaded,
/// so a MissingRequiredSignature from BisonFi means something about BisonFi.
#[tokio::test]
async fn bisonfi_replay_rig_propagates_signers() {
    use litesvm::LiteSVM;
    use solana_account::Account;
    use solana_instruction::{AccountMeta, Instruction};
    use solana_keypair::Keypair;
    use solana_signer::Signer;
    use solana_transaction::Transaction;

    let token_program = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
    let usdc = Pubkey::from_str_const(USDC_MINT);
    let mut svm = LiteSVM::new()
        .with_sigverify(false)
        .with_blockhash_check(false);

    let taker = Keypair::new();
    svm.airdrop(&taker.pubkey(), 10_000_000_000).unwrap();
    let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
    let mk = |amount| Account {
        lamports: 10_000_000_000,
        data: token_account(&usdc, &taker.pubkey(), amount),
        owner: token_program,
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(a, mk(1_000_000)).unwrap();
    svm.set_account(b, mk(0)).unwrap();

    // SPL Token Transfer: tag 3, u64 amount. Authority must be a signer.
    let mut data = vec![3u8];
    data.extend_from_slice(&500_000u64.to_le_bytes());
    let ix = Instruction {
        program_id: token_program,
        accounts: vec![
            AccountMeta::new(a, false),
            AccountMeta::new(b, false),
            AccountMeta::new_readonly(taker.pubkey(), true),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&taker.pubkey()),
        &[&taker],
        svm.latest_blockhash(),
    );
    let res = svm.send_transaction(tx);
    assert!(
        res.is_ok(),
        "the rig cannot even authorise an SPL transfer, so it cannot test BisonFi: {:?}",
        res.err().map(|e| (e.err, e.meta.logs))
    );
    assert_eq!(spl_amount(&svm.get_account(&b).unwrap().data), 500_000);
}

/// Replays a swap against arbitrary pool bytes with no RPC of its own, synthesizing the vaults from
/// the reserves they were measured to mirror.
///
/// This exists so a behavioural claim can be made about *every* pool rather than the one the
/// templates point at. Seventeen pools times several mutations times both directions is several
/// hundred swaps: fine in LiteSVM, and impossible against a live endpoint. The compute limit is
/// raised because the 200k default cannot finish a full rung walk, which is what made the ladder
/// look inert the first time it was tested.
fn bisonfi_replay(
    elf: &[u8],
    pool_addr: &str,
    pool_bytes: &[u8],
    token_programs: (Pubkey, Pubkey),
    amount_in: u64,
    direction: u8,
    mutate: impl FnOnce(&mut Vec<u8>),
) -> Result<u64, String> {
    bisonfi_replay_min_out(
        elf,
        pool_addr,
        pool_bytes,
        token_programs,
        amount_in,
        0,
        direction,
        mutate,
    )
}

/// As [`bisonfi_replay`] but sets the swap's second u64, which the instruction layout suggests is a
/// minimum-output bound. Every other caller passes zero, so this is the only place its behaviour is
/// exercised - and whether it is enforced decides what a silenced venue looks like to a real
/// integration: a transaction that quietly moves nothing, or one that reverts.
#[allow(clippy::too_many_arguments)]
fn bisonfi_replay_min_out(
    elf: &[u8],
    pool_addr: &str,
    pool_bytes: &[u8],
    token_programs: (Pubkey, Pubkey),
    amount_in: u64,
    min_out: u64,
    direction: u8,
    mutate: impl FnOnce(&mut Vec<u8>),
) -> Result<u64, String> {
    bisonfi_replay_with_state(
        elf,
        pool_addr,
        pool_bytes,
        token_programs,
        amount_in,
        min_out,
        direction,
        mutate,
    )
    .map(|(out, _)| out)
}

/// As [`bisonfi_replay_min_out`], also returning the pool account as the swap left it.
#[allow(clippy::too_many_arguments)]
fn bisonfi_replay_with_state(
    elf: &[u8],
    pool_addr: &str,
    pool_bytes: &[u8],
    token_programs: (Pubkey, Pubkey),
    amount_in: u64,
    min_out: u64,
    direction: u8,
    mutate: impl FnOnce(&mut Vec<u8>),
) -> Result<(u64, Vec<u8>), String> {
    use litesvm::LiteSVM;
    use solana_account::Account;
    use solana_instruction::{AccountMeta, Instruction};
    use solana_keypair::Keypair;
    use solana_signer::Signer;
    use solana_transaction::Transaction;

    let mut pool = pool_bytes.to_vec();
    if pool.len() != 2048 {
        return Err(format!("pool is {} bytes, expected 2048", pool.len()));
    }
    let g64 = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let base_reserve = g64(&pool, 48);
    let quote_reserve = g64(&pool, 56);
    let base_vault = Pubkey::new_from_array(pool[120..152].try_into().unwrap());
    let quote_vault = Pubkey::new_from_array(pool[152..184].try_into().unwrap());
    let base_mint = Pubkey::new_from_array(pool[184..216].try_into().unwrap());
    let quote_mint = Pubkey::new_from_array(pool[216..248].try_into().unwrap());
    let pool_slot = g64(&pool, 72);
    mutate(&mut pool);

    let program_id = Pubkey::from_str_const(BISONFI_PROGRAM);
    let pool_key = pool_addr
        .parse::<Pubkey>()
        .map_err(|_| format!("bad pool address {pool_addr}"))?;
    // One program per side: slots 6 and 7 of the instruction are the base and quote token programs,
    // which is why the account list appears to name the token program twice.
    let (base_program, quote_program) = token_programs;

    let mut svm = LiteSVM::new()
        .with_sigverify(false)
        .with_blockhash_check(false);
    svm.add_program(program_id, elf)
        .map_err(|e| format!("add_program: {e:?}"))?;
    let block_time_ms = publish_millis(pool_bytes);
    let mut clock: solana_clock::Clock = svm.get_sysvar();
    clock.slot = pool_slot;
    clock.unix_timestamp = (block_time_ms / 1_000) as i64;
    svm.set_sysvar(&clock);
    let tick = install_market_tick(&mut svm, pool_slot, block_time_ms);
    svm.set_sysvar(&LastRestartSlot {
        // Captured from the upstream LastRestartSlot sysvar. Product startup fetches this value.
        last_restart_slot: 246_464_040,
    });

    let owned = |data: Vec<u8>, owner: Pubkey| Account {
        lamports: 10_000_000_000,
        data,
        owner,
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(pool_key, owned(pool, program_id))
        .map_err(|e| format!("{e:?}"))?;
    svm.set_account(
        base_vault,
        owned(
            token_account(&base_mint, &pool_key, base_reserve),
            base_program,
        ),
    )
    .map_err(|e| format!("{e:?}"))?;
    svm.set_account(
        quote_vault,
        owned(
            token_account(&quote_mint, &pool_key, quote_reserve + 79_168),
            quote_program,
        ),
    )
    .map_err(|e| format!("{e:?}"))?;

    let taker = Keypair::new();
    svm.airdrop(&taker.pubkey(), 10_000_000_000)
        .map_err(|e| format!("{e:?}"))?;
    let (src_ta, dst_ta) = (Pubkey::new_unique(), Pubkey::new_unique());
    let (base_amt, quote_amt) = if direction == 0 {
        (amount_in.saturating_mul(10), 0)
    } else {
        (0, amount_in.saturating_mul(10))
    };
    svm.set_account(
        src_ta,
        owned(
            token_account(&base_mint, &taker.pubkey(), base_amt),
            base_program,
        ),
    )
    .map_err(|e| format!("{e:?}"))?;
    svm.set_account(
        dst_ta,
        owned(
            token_account(&quote_mint, &taker.pubkey(), quote_amt),
            quote_program,
        ),
    )
    .map_err(|e| format!("{e:?}"))?;

    let mut data = Vec::with_capacity(19);
    data.push(0x07);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    data.push(direction);
    data.push(0);

    let mut budget = vec![2u8];
    budget.extend_from_slice(&1_400_000u32.to_le_bytes());
    let ixs = vec![
        Instruction {
            program_id: Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
            accounts: vec![],
            data: budget,
        },
        Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(taker.pubkey(), true),
                AccountMeta::new(pool_key, false),
                AccountMeta::new(base_vault, false),
                AccountMeta::new(quote_vault, false),
                AccountMeta::new(src_ta, false),
                AccountMeta::new(dst_ta, false),
                AccountMeta::new_readonly(base_program, false),
                AccountMeta::new_readonly(quote_program, false),
                AccountMeta::new_readonly(Pubkey::from_str_const(BISONFI_NINTH), true),
            ],
            data,
        },
    ];
    let mut ixs = ixs;
    ixs[1].accounts.extend(tick);
    let mut msg = solana_message::Message::new(&ixs, Some(&taker.pubkey()));
    msg.recent_blockhash = svm.latest_blockhash();
    let nsig = msg.header.num_required_signatures as usize;
    let mut tx = Transaction::new_unsigned(msg);
    tx.signatures = vec![solana_signature::Signature::default(); nsig];
    let sig = taker.sign_message(&tx.message.serialize());
    tx.signatures[0] = sig;

    match svm.send_transaction(tx) {
        Ok(_) => {
            let out = if direction == 0 {
                spl_amount(&svm.get_account(&dst_ta).unwrap().data)
            } else {
                spl_amount(&svm.get_account(&src_ta).unwrap().data)
            };
            Ok((out, svm.get_account(&pool_key).unwrap().data))
        }
        Err(e) => Err(format!("{:?}", e.err)),
    }
}

/// Publishes an ELF cache only after the complete file has been written. A unique temporary path
/// prevents parallel test processes from observing or overwriting one another's partial writes.
fn publish_elf_cache(cache: &std::path::Path, bytes: &[u8]) {
    static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let temporary = cache.with_extension(format!("so.{}.{}.tmp", std::process::id(), sequence));
    if std::fs::write(&temporary, bytes).is_ok() {
        let _ = std::fs::rename(&temporary, cache);
    }
    let _ = std::fs::remove_file(temporary);
}

/// The program's executable, saved in the temp dir under the slot it was last deployed at, so an
/// upgrade gets a new file instead of old code running against freshly fetched accounts.
async fn load_program_elf(cache_name: &str, programdata: &str) -> Vec<u8> {
    let cache = std::env::temp_dir().join(format!(
        "{cache_name}-{}.so",
        last_deployed_slot(programdata).await
    ));
    if let Ok(bytes) = std::fs::read(&cache) {
        return bytes;
    }
    let bytes = fetch(&[programdata]).await.remove(0)[45..].to_vec();
    publish_elf_cache(&cache, &bytes);
    bytes
}

/// The slot a program was last deployed at, read from bytes 4..12 of its programdata account.
async fn last_deployed_slot(programdata: &str) -> u64 {
    let client = SurfnetRemoteClient::new(
        std::env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_RPC_URL.to_string()),
    );
    let config = RpcAccountInfoConfig {
        encoding: Some(UiAccountEncoding::Base64),
        data_slice: Some(UiDataSliceConfig {
            offset: 4,
            length: 8,
        }),
        commitment: Some(CommitmentConfig::confirmed()),
        min_context_slot: None,
    };
    // Retried for the same reason as [`fetch`]: the public endpoint throttles.
    let mut attempt = 0;
    let data = loop {
        match client
            .client
            .get_ui_account_with_config(&Pubkey::from_str_const(programdata), config.clone())
            .await
        {
            Ok(response) => {
                break response
                    .value
                    .unwrap_or_else(|| panic!("{programdata} no longer exists on mainnet"))
                    .data
                    .decode()
                    .expect("base64 programdata slice");
            }
            Err(e) => {
                attempt += 1;
                if attempt >= 5 {
                    panic!("failed to fetch {programdata} after {attempt} attempts: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_millis(750 * attempt)).await;
            }
        }
    };
    u64::from_le_bytes(data[..8].try_into().expect("programdata slot"))
}

/// The program ELF, fetched once per process and reused.
/// Also loads the market tick: anything that runs the program needs both.
async fn bisonfi_elf() -> Vec<u8> {
    static ELF: tokio::sync::OnceCell<Vec<u8>> = tokio::sync::OnceCell::const_new();
    load_market_tick().await;
    ELF.get_or_init(|| load_program_elf("surfpool-bisonfi-program", BISONFI_PROGRAMDATA))
        .await
        .clone()
}

/// BisonFi's market tick as upstream holds it, fetched once per process. Every replay restamps it to
/// its own clock with the shipped keep-alive, as Surfpool does every slot.
static BISONFI_TICK: std::sync::OnceLock<solana_account::Account> = std::sync::OnceLock::new();

async fn load_market_tick() {
    if BISONFI_TICK.get().is_some() {
        return;
    }
    let (data, lamports) = fetch_with_lamports(&[&MARKET_TICK.to_string()])
        .await
        .remove(0);
    let _ = BISONFI_TICK.set(solana_account::Account {
        lamports,
        data,
        owner: MARKET_TICK_PROGRAM,
        executable: false,
        rent_epoch: 0,
    });
}

thread_local! {
    /// Set only by the test that pins what a swap without the tick does.
    static OMIT_MARKET_TICK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Installs the market tick stamped to `slot` and `block_time_ms` and returns the account a swap
/// passes tenth, or `None` while a test is omitting it.
fn install_market_tick(
    svm: &mut litesvm::LiteSVM,
    slot: u64,
    block_time_ms: u64,
) -> Option<solana_instruction::AccountMeta> {
    if OMIT_MARKET_TICK.with(|omit| omit.get()) {
        return None;
    }
    let tick = BISONFI_TICK
        .get()
        .expect("load the market tick (bisonfi_elf does) before replaying a swap");
    let mut stamped = tick.clone();
    stamped.data = bisonfi_keep_alive("bisonfi-market-tick", &tick.data, slot, block_time_ms);
    svm.set_account(MARKET_TICK, stamped)
        .expect("tick should install");
    Some(solana_instruction::AccountMeta::new_readonly(
        MARKET_TICK,
        false,
    ))
}

/// The pool's publication time in milliseconds. Replays run their clock at it, so a quote is exactly
/// as fresh as the maker made it.
fn publish_millis(pool: &[u8]) -> u64 {
    u64::from_le_bytes(pool[88..96].try_into().unwrap()) / 1_000_000
}

/// Offsets of the quote ladder. `LADDER` is the source table the maker configures; `LADDER_MIRROR` is
/// the working copy swaps price from and consume. The program copies the source over the working copy
/// on the first swap of a slot the pool was published in, so the working copy only differs - and only
/// matters - within a slot after that copy, or while the quote is one or two slots old.
const BISONFI_LADDER: usize = 528;

const BISONFI_LADDER_MIRROR: usize = 288;

/// A rung is 16 bytes: share-if-ask, share-if-bid, level, tick offset.
const BISONFI_RUNG: usize = 16;

/// Pool state loaded from the upstream datasource plus the deployed program, ready for local swaps.
#[derive(Clone)]
struct BisonfiFork {
    elf: Vec<u8>,
    pool_addr: Pubkey,
    pool: Vec<u8>,
    base_vault: (Pubkey, Vec<u8>, u64),
    quote_vault: (Pubkey, Vec<u8>, u64),
}

/// Cached per process, keyed by pool. Several tests load the same market, and refetching it for each
/// one is what exhausts the public endpoint. One snapshot per suite run is also more consistent:
/// tests then compare against identical state rather than a market that moved between them.
fn bisonfi_fork_cache() -> &'static std::sync::Mutex<HashMap<String, BisonfiFork>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, BisonfiFork>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Two batched reads: the program and pool, then the vaults the pool names.
async fn bisonfi_fork(pool_addr: &str) -> BisonfiFork {
    if let Some(hit) = bisonfi_fork_cache()
        .lock()
        .ok()
        .and_then(|c| c.get(pool_addr).cloned())
    {
        return hit;
    }
    let fork = bisonfi_fork_uncached(pool_addr).await;
    if let Ok(mut c) = bisonfi_fork_cache().lock() {
        c.insert(pool_addr.to_string(), fork.clone());
    }
    fork
}

async fn bisonfi_fork_uncached(pool_addr: &str) -> BisonfiFork {
    let elf = bisonfi_elf().await;
    // The vault addresses live in the pool, so learning them takes one read - but the pool's cached
    // reserves and the vault balances must come from the SAME slot or they disagree. This market
    // turns over tens of thousands of dollars between two requests, which is enough to make the pool
    // look like it claims more than it holds. So the first read is only used for the addresses and
    // everything is then re-read together.
    let probe = fetch(&[pool_addr]).await.remove(0);
    assert_eq!(probe.len(), 2048, "{pool_addr} should be a 2048-byte pool");
    let bv = Pubkey::new_from_array(probe[120..152].try_into().unwrap());
    let qv = Pubkey::new_from_array(probe[152..184].try_into().unwrap());
    let snap = fetch_with_lamports(&[pool_addr, &bv.to_string(), &qv.to_string()]).await;
    BisonfiFork {
        elf,
        pool_addr: Pubkey::from_str_const(pool_addr),
        pool: snap[0].0.clone(),
        base_vault: (bv, snap[1].0.clone(), snap[1].1),
        quote_vault: (qv, snap[2].0.clone(), snap[2].1),
    }
}

/// Runs one swap against a mutated copy of the loaded state. `direction` 0 sells the base token and
/// 1 buys it.
fn bisonfi_run(
    fork: &BisonfiFork,
    amount_in: u64,
    direction: u8,
    mutate: impl FnOnce(&mut Vec<u8>),
) -> Result<u64, String> {
    use litesvm::LiteSVM;
    use solana_account::Account;
    use solana_instruction::{AccountMeta, Instruction};
    use solana_keypair::Keypair;
    use solana_signer::Signer;
    use solana_transaction::Transaction;

    let mut pool = fork.pool.clone();
    let pool_slot = u64::from_le_bytes(pool[72..80].try_into().unwrap());
    let base_mint = Pubkey::new_from_array(pool[184..216].try_into().unwrap());
    let quote_mint = Pubkey::new_from_array(pool[216..248].try_into().unwrap());
    mutate(&mut pool);

    let program_id = Pubkey::from_str_const(BISONFI_PROGRAM);
    let token_program = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
    let mut svm = LiteSVM::new()
        .with_sigverify(false)
        .with_blockhash_check(false);
    svm.add_program(program_id, &fork.elf)
        .map_err(|e| format!("add_program: {e:?}"))?;

    let block_time_ms = publish_millis(&fork.pool);
    let mut clock: solana_clock::Clock = svm.get_sysvar();
    clock.slot = pool_slot;
    clock.unix_timestamp = (block_time_ms / 1_000) as i64;
    svm.set_sysvar(&clock);
    let tick = install_market_tick(&mut svm, pool_slot, block_time_ms);
    // Match Surfpool startup: this local VM begins at the account snapshot's slot.
    svm.set_sysvar(&LastRestartSlot {
        // Captured from the upstream LastRestartSlot sysvar. Product startup fetches this value.
        last_restart_slot: 246_464_040,
    });

    let owned = |data: Vec<u8>, owner: Pubkey| Account {
        lamports: 10_000_000_000,
        data,
        owner,
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(fork.pool_addr, owned(pool, program_id))
        .map_err(|e| format!("{e:?}"))?;
    let vault_acct = |data: Vec<u8>, lamports: u64| Account {
        lamports,
        data,
        owner: token_program,
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(
        fork.base_vault.0,
        vault_acct(fork.base_vault.1.clone(), fork.base_vault.2),
    )
    .map_err(|e| format!("{e:?}"))?;
    svm.set_account(
        fork.quote_vault.0,
        vault_acct(fork.quote_vault.1.clone(), fork.quote_vault.2),
    )
    .map_err(|e| format!("{e:?}"))?;

    let taker = Keypair::new();
    svm.airdrop(&taker.pubkey(), 10_000_000_000)
        .map_err(|e| format!("{e:?}"))?;
    // Slots 4 and 5 are the user's base and quote accounts, fixed by mint; direction decides flow.
    let (user_base, user_quote) = (Pubkey::new_unique(), Pubkey::new_unique());
    let (base_amt, quote_amt) = if direction == 0 {
        (amount_in.saturating_mul(2), 0)
    } else {
        (0, amount_in.saturating_mul(2))
    };
    // A wrapped-SOL account's lamports must cover its balance plus rent, or paying out the base
    // token leaves the instruction unbalanced.
    const TOKEN_RENT: u64 = 2_039_280;
    svm.set_account(
        user_base,
        vault_acct(
            token_account(&base_mint, &taker.pubkey(), base_amt),
            base_amt.saturating_add(TOKEN_RENT),
        ),
    )
    .map_err(|e| format!("{e:?}"))?;
    svm.set_account(
        user_quote,
        vault_acct(
            token_account(&quote_mint, &taker.pubkey(), quote_amt),
            TOKEN_RENT,
        ),
    )
    .map_err(|e| format!("{e:?}"))?;

    let mut data = Vec::with_capacity(19);
    data.push(0x07);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());
    data.push(direction);
    data.push(0);

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(taker.pubkey(), true),
            AccountMeta::new(fork.pool_addr, false),
            AccountMeta::new(fork.base_vault.0, false),
            AccountMeta::new(fork.quote_vault.0, false),
            AccountMeta::new(user_base, false),
            AccountMeta::new(user_quote, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(Pubkey::from_str_const(BISONFI_NINTH), true),
        ],
        data,
    };
    // Walking several rungs costs well over the 200k default; the routed swaps observed on mainnet
    // run with ~600k available. SetComputeUnitLimit is discriminant 2 followed by a u32.
    let mut cu_data = vec![2u8];
    cu_data.extend_from_slice(&1_400_000u32.to_le_bytes());
    let cu_ix = Instruction {
        program_id: Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: cu_data,
    };

    let mut ix = ix;
    ix.accounts.extend(tick);
    let mut msg = solana_message::Message::new(&[cu_ix, ix], Some(&taker.pubkey()));
    msg.recent_blockhash = svm.latest_blockhash();
    let nsig = msg.header.num_required_signatures as usize;
    let mut tx = Transaction::new_unsigned(msg);
    tx.signatures = vec![solana_signature::Signature::default(); nsig];
    tx.signatures[0] = taker.sign_message(&tx.message.serialize());

    match svm.send_transaction(tx) {
        Ok(_) => Ok(spl_amount(
            &svm.get_account(if direction == 0 {
                &user_quote
            } else {
                &user_base
            })
            .unwrap()
            .data,
        )),
        Err(e) => Err(format!("{:?}", e.err)),
    }
}

/// `fair_value` is claimed to be the price the venue quotes on. This pins the exact relationship:
/// scaling it must scale the quote by the same factor, against the deployed program.
#[tokio::test]
async fn bisonfi_fair_value_scales_the_quote_exactly() {
    const ONE_SOL: u64 = 1_000_000_000;
    let fork = bisonfi_fork(BISONFI_POOL).await;
    let mid = u128::from_le_bytes(fork.pool[832..848].try_into().unwrap());

    let base = bisonfi_run(&fork, ONE_SOL, 0, |_| {}).expect("control should price");
    let doubled = bisonfi_run(&fork, ONE_SOL, 0, |d| {
        d[832..848].copy_from_slice(&(mid * 2).to_le_bytes())
    })
    .expect("doubled mid should price");
    let halved = bisonfi_run(&fork, ONE_SOL, 0, |d| {
        d[832..848].copy_from_slice(&(mid / 2).to_le_bytes())
    })
    .expect("halved mid should price");

    // Integer maths, so allow a unit of rounding either way rather than demanding bit equality.
    assert!(
        doubled.abs_diff(base * 2) <= 2,
        "doubling fair_value should double the quote: {base} -> {doubled}"
    );
    assert!(
        halved.abs_diff(base / 2) <= 2,
        "halving fair_value should halve the quote: {base} -> {halved}"
    );
}

/// The depth template's claim, on several markets with different reserve ratios rather than one.
/// Lowering the reserve the pool pays out of must make the same trade fill worse.
#[tokio::test]
async fn bisonfi_depth_lever_is_monotonic_on_every_quoting_market() {
    let rig = bisonfi_rig().await;
    let mut checked = 0usize;

    for (pool, data, tp) in &rig.quoting {
        let base_reserve = u64::from_le_bytes(data[48..56].try_into().unwrap());
        let quote_reserve = u64::from_le_bytes(data[56..64].try_into().unwrap());
        let name = String::from_utf8_lossy(
            &data[256..288]
                .iter()
                .copied()
                .take_while(|b| *b != 0)
                .collect::<Vec<_>>(),
        )
        .to_string();

        let at = |scaled: u64, size: u64| {
            rig.try_scenario(
                pool,
                data,
                *tp,
                "bisonfi-depth",
                &[("quote_reserve", serde_json::json!(scaled))],
                size,
                0,
            )
        };

        // Assert on every size where all three legs price, rather than one hand-picked size. The
        // ladder engages over a window that differs per market, so a fixed size would be asserting a
        // coincidence about today's state - but wherever the market CAN price all three, the ordering
        // is a claim the template makes and must hold.
        let mut ordered_points = 0usize;
        let mut strict_points = 0usize;
        for div in [200u64, 100, 50, 20, 10, 5] {
            let size = base_reserve / div;
            if size == 0 {
                continue;
            }
            let deep = at(quote_reserve.saturating_mul(10), size);
            let control = bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, |_| {});
            let thin = at(quote_reserve / 2, size);
            let (deep, control, thin) = match (deep, control, thin) {
                (Ok(d), Ok(c), Ok(t)) if d > 0 && c > 0 && t > 0 => (d, c, t),
                _ => continue, // this market cannot price all three at this size
            };
            assert!(
                deep >= control && control >= thin,
                "{name} at 1/{div} of base reserve: a deeper quote reserve must never pay out less \
                 and a thinner one never more, got deep={deep} control={control} thin={thin}"
            );
            ordered_points += 1;
            if deep > control && control > thin {
                strict_points += 1;
            }
        }

        assert!(
            ordered_points > 0,
            "{name}: no trade size priced under all three depths, so the depth lever was never \
             actually exercised on this market"
        );
        // Monotonic everywhere measurable is necessary but not sufficient - a lever that did nothing
        // would satisfy it with equalities. At least one size has to respond strictly.
        assert!(
            strict_points > 0,
            "{name}: the ordering held at {ordered_points} sizes but never strictly, so a 20x range \
             of quote reserve changed nothing. The depth template would not be a lever at all"
        );
        checked += 1;
    }

    // Previously this test covered three hardcoded markets while the other three templates were proven
    // on every pool that quotes. Without this floor it could silently narrow back to one.
    assert_eq!(
        checked,
        rig.quoting.len(),
        "only {checked} of the quoting markets exercised the depth lever"
    );
}

/// Buying pays out of the base reserve, so that is the side that constrains a buy. Confirms the
/// template's direction guidance is the right way round.
#[tokio::test]
async fn bisonfi_depth_lever_is_direction_specific() {
    let fork = bisonfi_fork(BISONFI_POOL).await;
    let base_reserve = u64::from_le_bytes(fork.pool[48..56].try_into().unwrap());
    let scale = |off: usize, num: u64| {
        move |d: &mut Vec<u8>| {
            let v = u64::from_le_bytes(d[off..off + 8].try_into().unwrap());
            d[off..off + 8].copy_from_slice(&v.saturating_mul(num).to_le_bytes())
        }
    };

    // Size each direction from this snapshot rather than assuming the live market can settle a
    // fixed notional. The control sell also gives the matching quote-side input for the buy leg,
    // without hardcoding either mint's decimals or price.
    let sell = base_reserve / 50;
    assert!(
        sell > 0,
        "the reference market must have a non-zero base reserve"
    );
    let sell_base = bisonfi_run(&fork, sell, 0, |_| {}).expect("control sell");
    assert_eq!(
        bisonfi_run(&fork, sell, 0, scale(48, 10)).expect("sell with deeper base"),
        sell_base,
        "the base reserve must not affect a sell, which pays out quote"
    );
    let sell_deeper_quote =
        bisonfi_run(&fork, sell, 0, scale(56, 10)).expect("sell with deeper quote");
    assert!(
        sell_deeper_quote > sell_base,
        "the quote reserve must affect a sell"
    );
    assert!(
        sell_deeper_quote <= spl_amount(&fork.quote_vault.1),
        "the deeper sell must remain within the real quote vault balance"
    );

    let buy = sell_base;
    let buy_base = bisonfi_run(&fork, buy, 1, |_| {}).expect("control buy");
    let buy_deeper_base = bisonfi_run(&fork, buy, 1, scale(48, 10)).expect("buy with deeper base");
    assert!(
        buy_deeper_base > buy_base,
        "the base reserve must affect a buy, which pays out base"
    );
    assert!(
        buy_deeper_base <= spl_amount(&fork.base_vault.1),
        "the deeper buy must remain within the real base vault balance"
    );
}

/// The template warns that raising a reserve above the vault's real balance breaks settlement. That
/// warning is only worth printing if it is true.
#[tokio::test]
async fn bisonfi_raising_a_reserve_past_the_vault_fails_to_settle() {
    let fork = bisonfi_fork(BISONFI_POOL).await;
    let held = spl_amount(&fork.quote_vault.1);
    let cached = u64::from_le_bytes(fork.pool[56..64].try_into().unwrap());
    // Same-slot snapshot, so the pool's cached quote must not exceed what the vault actually holds.
    assert!(
        cached <= held,
        "same-slot pool and vault disagree: pool claims {cached} quote, vault holds {held}"
    );

    // Claim a thousand times the quote the vault actually has, then try to draw more than it holds.
    let sell = 10_000_000_000_000u64; // 10k SOL, worth far more than the vault at 1000x depth
    let res = bisonfi_run(&fork, sell, 0, move |d| {
        d[56..64].copy_from_slice(&cached.saturating_mul(1000).to_le_bytes())
    });
    match res {
        Err(e) => assert!(
            !e.is_empty(),
            "raising the reserve past the vault should fail, and it did: {e}"
        ),
        Ok(out) => assert!(
            out <= held,
            "if it settles at all it can only pay out what the vault holds ({held}), paid {out}"
        ),
    }
}

/// Find the current reference market's liveness threshold without treating it as a stable template
/// constant. The threshold is maker policy and can vary between markets or deployed builds.
#[tokio::test]
async fn bisonfi_staleness_threshold_exists() {
    const ONE_SOL: u64 = 1_000_000_000;
    let fork = bisonfi_fork(BISONFI_POOL).await;
    let last = u64::from_le_bytes(fork.pool[72..80].try_into().unwrap());
    let age_by =
        |n: u64| move |d: &mut Vec<u8>| d[72..80].copy_from_slice(&(last - n).to_le_bytes());

    assert!(bisonfi_run(&fork, ONE_SOL, 0, age_by(0)).expect("fresh") > 0);

    // Smallest age that stops the quote, by binary search over a generous range.
    let (mut lo, mut hi) = (0u64, 4096u64);
    assert_eq!(
        bisonfi_run(&fork, ONE_SOL, 0, age_by(hi))
            .expect("the stale quote check must execute successfully"),
        0,
        "aging by {hi} slots should stop the venue quoting"
    );
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        if bisonfi_run(&fork, ONE_SOL, 0, age_by(mid)).unwrap_or_else(|error| {
            panic!("the staleness search must execute successfully: {error}")
        }) > 0
        {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    println!("  staleness cliff: quotes at -{lo} slots, refuses at -{hi}");
    assert!(
        (1..=4096).contains(&hi),
        "expected a cliff inside the searched range, found {hi}"
    );
}

/// SCENARIO: the maker widens its quote ladder between the caller pricing and the caller filling.
///
/// The spread counterpart to `bisonfi_scenario_mid_moves_between_quote_and_fill`, and the last of the
/// four templates to get a proof that it works as a scheduled, across-slots override rather than a
/// single write. It is also the most realistic way a PMM degrades: a maker that has stopped liking the
/// flow widens before it goes dark, so a taker sees a fill that is legal, non-zero, and worse than the
/// number it priced on.
///
/// The trade size is searched rather than fixed. The ladder only engages over a window of size that
/// differs per market, so a hardcoded size would be asserting a coincidence about today's live state.
#[tokio::test]
async fn bisonfi_scenario_spread_widens_between_quote_and_fill() {
    use surfpool_types::{AccountAddress, OverrideInstance, Scenario};

    const FILL_AT: u64 = 2;
    const TIGHT: i32 = -13; // about 5 ppm below mid
    const WIDE: i32 = -25_600; // 1% below mid
    /// Widening from TIGHT to WIDE cannot cost the seller more than the tick difference.
    const MAX_GAP: f64 = (TIGHT - WIDE) as f64 / 2_560_000.0;

    let rig = bisonfi_rig().await;

    // Find a market and a size where the ladder is genuinely engaged, so that widening it has to show
    // up in the fill. Without this the test could pass on a size where the spread is simply inert.
    let mut chosen: Option<(&str, &Vec<u8>, (Pubkey, Pubkey), u64, u64, u64)> = None;
    'search: for (pool, data, tp) in &rig.quoting {
        let base_reserve = u64::from_le_bytes(data[48..56].try_into().unwrap());
        for div in [1000u64, 200, 100, 50, 20, 10, 4, 2] {
            let size = base_reserve / div;
            if size == 0 {
                continue;
            }
            let tight = match bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, {
                bisonfi_apply_template("bisonfi-spread", &bisonfi_spread_bids(TIGHT))
            }) {
                Ok(o) if o > 0 => o,
                _ => continue,
            };
            let wide = match bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, {
                bisonfi_apply_template("bisonfi-spread", &bisonfi_spread_bids(WIDE))
            }) {
                Ok(o) if o > 0 => o,
                _ => continue,
            };
            // Require most of the configured spread to be reachable at this size.
            if (tight - wide) as f64 / tight as f64 >= MAX_GAP * 0.5 {
                chosen = Some((pool, data, *tp, size, tight, wide));
                break 'search;
            }
        }
    }
    let (pool, data, tp, size, _, _) = chosen.expect(
        "no quoting market engaged its ladder at any of the eight sizes tried, so a mid-flight \
         widening cannot be demonstrated. Investigate before relaxing this",
    );
    let pool_key = pool.parse::<Pubkey>().expect("pool address");
    let base_slot = u64::from_le_bytes(data[72..80].try_into().unwrap());

    let (mut svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    svm.inner
        .set_account(
            pool_key,
            solana_account::Account {
                lamports: 1_000_000,
                data: data.clone(),
                owner: Pubkey::from_str_const(BISONFI_PROGRAM),
                executable: false,
                rent_epoch: 0,
            },
        )
        .expect("seed the pool account");
    // The surfnet starts at the pool's own slot, as a surfnet forked at that moment would.
    svm.set_latest_absolute_slot(base_slot);

    let mut scenario = Scenario::new(
        "BisonFi widens mid-flight".to_string(),
        "Quotes a tight ladder at the slot the caller prices on and a 1% ladder before the \
         transaction executes"
            .to_string(),
    );
    for (relative, tick) in [(0u64, TIGHT), (FILL_AT, WIDE)] {
        scenario.add_override(
            OverrideInstance::new(
                "bisonfi-spread".to_string(),
                relative,
                AccountAddress::Pubkey(pool_key.to_string()),
            )
            .with_values(
                bisonfi_spread_bids(tick)
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect::<HashMap<String, serde_json::Value>>(),
            ),
        );
    }
    svm.register_scenario(scenario, Some(base_slot))
        .expect("register scenario");

    let images = bisonfi_play(&mut svm, pool_key, base_slot, base_slot + FILL_AT).await;

    // The scheduled writes landed in the right slots, in every region the template covers.
    for (path, offset, count) in BISONFI_SPREAD_PROPS {
        if !path.contains(".0.") {
            continue; // only the bid half is scheduled here
        }
        for rung in 0..count {
            let at = offset + rung * BISONFI_RUNG;
            let read =
                |slot: u64| i32::from_le_bytes(images[&slot][at..at + 4].try_into().unwrap());
            assert_eq!(
                read(base_slot),
                TIGHT,
                "{path} rung {rung} at {at}: the quoting slot must carry the tight ladder"
            );
            assert_eq!(
                read(base_slot + FILL_AT),
                WIDE,
                "{path} rung {rung} at {at}: the widening step must have fired by the fill slot"
            );
        }
    }

    // What the caller quoted, and therefore the minimum they would sign for.
    let quoted = bisonfi_replay(&rig.elf, pool, &images[&base_slot], tp, size, 0, |_| {})
        .expect("the quoting slot must fill");
    assert!(quoted > 0, "the caller's quote has to be real");

    let image = images[&(base_slot + FILL_AT)].clone();
    let unprotected = bisonfi_replay(&rig.elf, pool, &image, tp, size, 0, |_| {})
        .expect("a widened market still quotes, just worse");
    assert!(
        unprotected < quoted,
        "widening the ladder from 5 ppm to 1% must pay the seller less: got {unprotected} against \
         a quote of {quoted}"
    );
    let realized = (quoted - unprotected) as f64 / quoted as f64;
    assert!(
        realized <= MAX_GAP * 1.02,
        "the fill lost {:.4}% but the tick difference only allows {:.4}%, so the 1/2,560,000 unit \
         the template documents is wrong",
        realized * 100.0,
        MAX_GAP * 100.0
    );

    // And the case a consumer can actually detect: signing for the quoted price reverts.
    let protected = bisonfi_replay_min_out(&rig.elf, pool, &image, tp, size, quoted, 0, |_| {});
    assert!(
        protected.is_err(),
        "signing for the price that was quoted must REVERT once the maker has widened, got \
         {protected:?}"
    );
}

const WHIRLPOOL_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

/// Orca's `swap`, transcribed from the IDL the program itself publishes on chain.
///
/// Taken from the Anchor IDL account at `2KFqE4RWoPVbvodo8vbggCFeHPS8TDvgpwp79ALMrcyn`, which carries
/// whirlpool v0.9.0, spec 0.1.0, and a self-declared address matching the program. To re-derive it:
/// the address is `create_with_seed(find_program_address([], program).0, "anchor:idl", program)`, and
/// the account holds zlib-compressed JSON behind a 44-byte header (8 discriminator, 32 authority,
/// 4 length). No copy is kept in the repo - it is 105 KB, nothing reads it, and a stale copy would
/// be worse than none if Orca redeploys.
///
/// Transcribed rather than parsed at runtime because the IDL account stores zlib-compressed JSON and
/// this crate has no direct zlib dependency. The transcription is not load-bearing on trust: a wrong
/// account order or argument encoding cannot produce a swap that succeeds AND moves four balances
/// consistently, which is what the test below asserts.
mod whirlpool_swap {
    /// `sha256("global:swap")[..8]`, and byte-identical to the IDL's declared discriminator.
    pub const DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
    /// Lower bound on sqrt price; passing it as the limit for an a-to-b swap imposes no constraint.
    pub const MIN_SQRT_PRICE: u128 = 4295048016;
    /// Upper bound, for the b-to-a direction.
    pub const MAX_SQRT_PRICE: u128 = 79226673515401279992447579055;

    /// `amount, other_amount_threshold, sqrt_price_limit, amount_specified_is_input, a_to_b`
    pub fn data(amount: u64, threshold: u64, limit: u128, is_input: bool, a_to_b: bool) -> Vec<u8> {
        let mut d = DISCRIMINATOR.to_vec();
        d.extend_from_slice(&amount.to_le_bytes());
        d.extend_from_slice(&threshold.to_le_bytes());
        d.extend_from_slice(&limit.to_le_bytes());
        d.push(is_input as u8);
        d.push(a_to_b as u8);
        debug_assert_eq!(d.len(), 42);
        d
    }
}

/// The Whirlpool program's executable, cached in the temp dir like [`bisonfi_elf`].
async fn whirlpool_elf() -> Vec<u8> {
    static ELF: tokio::sync::OnceCell<Vec<u8>> = tokio::sync::OnceCell::const_new();
    ELF.get_or_init(|| async {
        let prog = Pubkey::from_str_const(WHIRLPOOL_PROGRAM);
        let loader = Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");
        let (programdata, _) = Pubkey::find_program_address(&[prog.as_ref()], &loader);
        // 45 bytes of UpgradeableLoaderState::ProgramData precede the ELF.
        load_program_elf("surfpool-whirlpool-program", &programdata.to_string()).await
    })
    .await
    .clone()
}

/// Everything needed to replay a swap against one Whirlpool's live state.
struct WhirlpoolFork {
    elf: Vec<u8>,
    key: Pubkey,
    data: Vec<u8>,
    mint_a: Pubkey,
    mint_b: Pubkey,
    vault_a: (Pubkey, Vec<u8>),
    vault_b: (Pubkey, Vec<u8>),
    /// Tick arrays keyed by start index, only those that exist on chain.
    arrays: Vec<(i32, Pubkey, Vec<u8>)>,
    start: i32,
    per_array: i32,
}

impl WhirlpoolFork {
    /// The three tick arrays a swap in `a_to_b` order must be handed, in sequence from the current
    /// one. Uninitialized neighbours are replaced by repeating the last existing array, which is what
    /// Orca's own SDK does - the program only requires the sequence be valid for the direction.
    fn tick_arrays(&self, a_to_b: bool) -> Vec<Pubkey> {
        let step = if a_to_b {
            -self.per_array
        } else {
            self.per_array
        };
        let mut out = Vec::new();
        for k in 0..3 {
            let want = self.start + step * k;
            let found = self
                .arrays
                .iter()
                .find(|(s, _, _)| *s == want)
                .map(|(_, k, _)| *k);
            match found {
                Some(k) => out.push(k),
                None => out.push(*out.last().expect("the current array must exist")),
            }
        }
        out
    }
}

async fn whirlpool_fork(pool: &str) -> WhirlpoolFork {
    let prog = Pubkey::from_str_const(WHIRLPOOL_PROGRAM);
    let key = Pubkey::from_str_const(pool);
    let data = fetch(&[pool]).await.remove(0);
    let spacing = u16::from_le_bytes(data[41..43].try_into().unwrap());
    let tick_current = i32::from_le_bytes(data[81..85].try_into().unwrap());
    let mint_a = Pubkey::try_from(&data[101..133]).expect("mint_a");
    let vault_a_key = Pubkey::try_from(&data[133..165]).expect("vault_a");
    let mint_b = Pubkey::try_from(&data[181..213]).expect("mint_b");
    let vault_b_key = Pubkey::try_from(&data[213..245]).expect("vault_b");

    let per_array = spacing as i32 * 88;
    let start = (tick_current as f32 / per_array as f32).floor() as i32 * per_array;

    // Two arrays below and one above, so either direction has a sequence to walk.
    let starts: Vec<i32> = (-2..=1).map(|k| start + k * per_array).collect();
    let array_keys: Vec<Pubkey> = starts
        .iter()
        .map(|s| {
            Pubkey::find_program_address(
                &[b"tick_array", key.as_ref(), s.to_string().as_bytes()],
                &prog,
            )
            .0
        })
        .collect();

    // The probe above is only for discovering dependent addresses. Re-read the pool together with
    // its tick arrays and vaults so the replay cannot combine state from different upstream slots.
    let mut addrs = vec![pool.to_string()];
    addrs.extend(array_keys.iter().map(|k| k.to_string()));
    addrs.push(vault_a_key.to_string());
    addrs.push(vault_b_key.to_string());
    let refs: Vec<&str> = addrs.iter().map(|s| s.as_str()).collect();
    let got = fetch_optional(&refs).await;
    let data = got[0].clone().expect("pool exists in the final snapshot");
    let snapshot_tick = i32::from_le_bytes(data[81..85].try_into().unwrap());
    let snapshot_start = (snapshot_tick as f32 / per_array as f32).floor() as i32 * per_array;

    let arrays: Vec<(i32, Pubkey, Vec<u8>)> = starts
        .iter()
        .zip(array_keys.iter())
        .zip(got[1..=array_keys.len()].iter())
        .filter_map(|((s, k), d)| d.as_ref().map(|d| (*s, *k, d.clone())))
        .collect();
    assert!(
        arrays.iter().any(|(s, _, _)| *s == snapshot_start),
        "{pool}: the tick array holding the current tick does not exist, so no swap can be replayed"
    );

    WhirlpoolFork {
        elf: whirlpool_elf().await,
        key,
        data,
        mint_a,
        mint_b,
        vault_a: (
            vault_a_key,
            got[1 + array_keys.len()].clone().expect("vault_a exists"),
        ),
        vault_b: (
            vault_b_key,
            got[2 + array_keys.len()].clone().expect("vault_b exists"),
        ),
        arrays,
        start: snapshot_start,
        per_array,
    }
}

/// Executes a Whirlpool swap in LiteSVM against state loaded from the upstream datasource.
///
/// Returns `(amount_in_spent, amount_out_received)` measured from the taker's own token accounts.
fn whirlpool_replay(
    fork: &WhirlpoolFork,
    amount_in: u64,
    a_to_b: bool,
    min_out: u64,
) -> Result<(u64, u64), String> {
    use litesvm::LiteSVM;
    use solana_account::Account;
    use solana_instruction::{AccountMeta, Instruction};
    use solana_keypair::Keypair;
    use solana_signer::Signer;
    use solana_transaction::Transaction;

    let prog = Pubkey::from_str_const(WHIRLPOOL_PROGRAM);
    let spl = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

    let mut svm = LiteSVM::new()
        .with_sigverify(false)
        .with_blockhash_check(false);
    svm.add_program(prog, &fork.elf)
        .map_err(|e| format!("add_program: {e:?}"))?;

    // The pool accrues rewards against wall-clock time and refuses to run if the clock is behind its
    // own `reward_last_updated_timestamp` (error 6022, InvalidTimestamp). LiteSVM starts near zero,
    // which is millions of seconds behind an account loaded from upstream, so the clock is advanced
    // to the pool's own notion of now.
    let pool_ts = u64::from_le_bytes(fork.data[261..269].try_into().unwrap());
    let mut clock: solana_clock::Clock = svm.get_sysvar();
    clock.unix_timestamp = pool_ts as i64;
    clock.slot = 300_000_000;
    svm.set_sysvar(&clock);

    let owned = |data: Vec<u8>, owner: Pubkey| Account {
        lamports: 10_000_000_000,
        data,
        owner,
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(fork.key, owned(fork.data.clone(), prog))
        .map_err(|e| format!("seed whirlpool: {e:?}"))?;
    for (_, key, data) in &fork.arrays {
        svm.set_account(*key, owned(data.clone(), prog))
            .map_err(|e| format!("seed tick array: {e:?}"))?;
    }
    for (key, data) in [&fork.vault_a, &fork.vault_b] {
        svm.set_account(*key, owned(data.clone(), spl))
            .map_err(|e| format!("seed vault: {e:?}"))?;
    }

    let taker = Keypair::new();
    svm.airdrop(&taker.pubkey(), 10_000_000_000)
        .map_err(|e| format!("airdrop: {e:?}"))?;
    // The taker starts funded on the side they are selling and empty on the side they are buying, so
    // the balances below measure the swap and nothing else.
    let (ta_a, ta_b) = (Pubkey::new_unique(), Pubkey::new_unique());
    let (amt_a, amt_b) = if a_to_b {
        (amount_in.saturating_mul(2), 0)
    } else {
        (0, amount_in.saturating_mul(2))
    };
    svm.set_account(
        ta_a,
        owned(token_account(&fork.mint_a, &taker.pubkey(), amt_a), spl),
    )
    .map_err(|e| format!("seed taker a: {e:?}"))?;
    svm.set_account(
        ta_b,
        owned(token_account(&fork.mint_b, &taker.pubkey(), amt_b), spl),
    )
    .map_err(|e| format!("seed taker b: {e:?}"))?;

    let (oracle, _) = Pubkey::find_program_address(&[b"oracle", fork.key.as_ref()], &prog);
    let arrays = fork.tick_arrays(a_to_b);
    // Account order is the IDL's, exactly: see `whirlpool_swap`.
    let metas = vec![
        AccountMeta::new_readonly(spl, false),
        AccountMeta::new_readonly(taker.pubkey(), true),
        AccountMeta::new(fork.key, false),
        AccountMeta::new(ta_a, false),
        AccountMeta::new(fork.vault_a.0, false),
        AccountMeta::new(ta_b, false),
        AccountMeta::new(fork.vault_b.0, false),
        AccountMeta::new(arrays[0], false),
        AccountMeta::new(arrays[1], false),
        AccountMeta::new(arrays[2], false),
        AccountMeta::new_readonly(oracle, false),
        AccountMeta::new_readonly(prog, false),
    ];
    let limit = if a_to_b {
        whirlpool_swap::MIN_SQRT_PRICE
    } else {
        whirlpool_swap::MAX_SQRT_PRICE
    };
    let swap = Instruction {
        program_id: prog,
        accounts: metas,
        data: whirlpool_swap::data(amount_in, min_out, limit, true, a_to_b),
    };
    // Crossing tick arrays costs well over the 200k default.
    let mut budget = vec![2u8];
    budget.extend_from_slice(&600_000u32.to_le_bytes());
    let cu = Instruction {
        program_id: Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: budget,
    };

    let before_a = spl_amount(&svm.get_account(&ta_a).expect("ta_a").data);
    let before_b = spl_amount(&svm.get_account(&ta_b).expect("ta_b").data);
    let tx = Transaction::new_signed_with_payer(
        &[cu, swap],
        Some(&taker.pubkey()),
        &[&taker],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx)
        .map_err(|e| format!("{:?}", e.err))?;
    let after_a = spl_amount(&svm.get_account(&ta_a).expect("ta_a").data);
    let after_b = spl_amount(&svm.get_account(&ta_b).expect("ta_b").data);

    if a_to_b {
        Ok((before_a - after_a, after_b - before_b))
    } else {
        Ok((before_b - after_b, after_a - before_a))
    }
}

/// A real Orca Whirlpool swap executes against state loaded from upstream, in both directions.
///
/// This is the AMM leg the cross-pair arbitrage scenario runs on, and it is also what validates the
/// instruction layout transcribed in `whirlpool_swap`: the assertions below pin all four balances that
/// move, so a wrong account order or argument encoding cannot pass by coincidence.
#[tokio::test]
async fn whirlpool_swap_executes_against_loaded_upstream_state() {
    let fork = whirlpool_fork(WHIRLPOOL_SOL_USDC).await;
    // 1 SOL. Small enough to stay inside the current tick array on a pool this deep, which keeps the
    // test about the instruction rather than about tick-crossing.
    const ONE_SOL: u64 = 1_000_000_000;

    let (spent, got) = whirlpool_replay(&fork, ONE_SOL, true, 0).expect("a_to_b swap must execute");
    assert_eq!(
        spent, ONE_SOL,
        "the swap must consume exactly the input it was given"
    );
    assert!(got > 0, "selling 1 SOL must return USDC");

    // Sanity-check the rate against the pool's own published price rather than a hardcoded number, so
    // this does not rot as SOL moves. sqrt_price is Q64.64 over raw units.
    let sqrt_price = u128::from_le_bytes(fork.data[65..81].try_into().unwrap());
    let price_raw = (sqrt_price as f64 / 2f64.powi(64)).powi(2); // USDC-raw per SOL-raw
    let expected = ONE_SOL as f64 * price_raw;
    let ratio = got as f64 / expected;
    assert!(
        (0.97..=1.0).contains(&ratio),
        "1 SOL returned {got} USDC-raw where the pool's own sqrt_price implies about {expected:.0}; \
         ratio {ratio:.4} is outside the fee-and-slippage band, so the swap is not pricing off this \
         pool's state"
    );

    // The other direction, sized from what the first leg produced so it is the same notional.
    let (spent_b, got_b) =
        whirlpool_replay(&fork, got, false, 0).expect("b_to_a swap must execute");
    assert_eq!(
        spent_b, got,
        "the reverse swap must consume exactly its input"
    );
    assert!(
        got_b > 0 && got_b < ONE_SOL,
        "round-tripping must return less than the 1 SOL it started with after fees, got {got_b}"
    );

    // And the threshold argument is enforced.
    let greedy = whirlpool_replay(&fork, ONE_SOL, true, got + 1);
    assert!(
        greedy.is_err(),
        "asking for more than the swap can deliver must revert, got {greedy:?}"
    );
}

/// BisonFi's SOL/USDT pool, the second pair of the cross-pair arbitrage scenario.
const BISONFI_SOL_USDT: &str = "FJnaiidSLXFweWkgbinxEHRykVHsnkzDcYbNDR3RF5LN";
/// Orca's USDC/USDT Whirlpool, where the cross-pair arbitrage turns its USDT back into USDC.
const WHIRLPOOL_USDC_USDT: &str = "4fuUiYxTQ6QCrdSq9ouBYcTM7bqSwYTSyLueGZLTy4T4";

/// SCENARIO: Studio's "Cross-Pair Arbitrage" example, end to end.
///
/// BisonFi's SOL/USDC pool is held at its price and its SOL/USDT pool set 1.5% higher through the
/// shipped template and `SurfnetSvm`, then played for several slots so keep-alive republishes both
/// pools. The cycle runs against the deployed programs: USDC buys SOL on BisonFi, the SOL sells for
/// USDT on BisonFi, and Orca turns the USDT back into USDC - for more than was spent, while the same
/// cycle on mainnet state does not pay.
#[tokio::test]
async fn bisonfi_scenario_cross_pair_arbitrage_pays_end_to_end() {
    use surfpool_types::{AccountAddress, OverrideInstance, Scenario};

    const PLAY_FOR: u64 = 10;
    const USDC_IN: u64 = 1_000_000_000;

    let elf = bisonfi_elf().await;
    let pools = fetch(&[BISONFI_POOL, BISONFI_SOL_USDT]).await;
    let programs = bisonfi_token_programs(&pools).await;
    let (usdc_pool, usdt_pool) = (pools[0].clone(), pools[1].clone());
    let (usdc_programs, usdt_programs) = (programs[0], programs[1]);
    let orca = whirlpool_fork(WHIRLPOOL_USDC_USDT).await;
    let mint = |data: &[u8], offset: usize| Pubkey::try_from(&data[offset..offset + 32]).unwrap();
    assert_eq!(
        mint(&usdc_pool, 184),
        mint(&usdt_pool, 184),
        "both BisonFi pools must trade the same base"
    );
    assert_eq!(
        (orca.mint_a, orca.mint_b),
        (mint(&usdc_pool, 216), mint(&usdt_pool, 216)),
        "the Whirlpool must trade the two pools' quote tokens, USDC as A and USDT as B"
    );

    // USDC gained by the cycle, in basis points of the USDC spent. Each replay runs at its pool's own
    // publication, so a played image swaps at the slot keep-alive republished it in.
    let cycle = |usdc_image: &[u8], usdt_image: &[u8]| -> i64 {
        let sol = bisonfi_replay(
            &elf,
            BISONFI_POOL,
            usdc_image,
            usdc_programs,
            USDC_IN,
            1,
            |_| {},
        )
        .unwrap_or_else(|e| panic!("buy SOL for USDC on BisonFi: {e}"));
        assert!(sol > 0, "BisonFi must sell SOL for USDC");
        let usdt = bisonfi_replay(
            &elf,
            BISONFI_SOL_USDT,
            usdt_image,
            usdt_programs,
            sol,
            0,
            |_| {},
        )
        .unwrap_or_else(|e| panic!("sell {sol} SOL for USDT on BisonFi: {e}"));
        assert!(usdt > 0, "BisonFi must buy SOL for USDT");
        let (_, usdc_out) = whirlpool_replay(&orca, usdt, false, 0)
            .unwrap_or_else(|e| panic!("swap {usdt} USDT for USDC on Orca: {e}"));
        (i128::from(usdc_out) - i128::from(USDC_IN)) as i64 * 10_000 / USDC_IN as i64
    };
    let control = cycle(&usdc_pool, &usdt_pool);

    // What the AI writes for the prompt: SOL/USDC at the mid it quotes, SOL/USDT 1.5% above its own.
    let fair_value = |data: &[u8]| u128::from_le_bytes(data[832..848].try_into().unwrap());
    let published = |data: &[u8]| u64::from_le_bytes(data[72..80].try_into().unwrap());
    let base_slot = published(&usdc_pool).max(published(&usdt_pool));
    let (mut svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    svm.set_latest_absolute_slot(base_slot);
    let mut scenario = Scenario::new(
        "Cross-Pair Arbitrage".to_string(),
        "BisonFi prices SOL 1.5% higher in USDT than in USDC".to_string(),
    );
    for (pool, data, fair_value) in [
        (BISONFI_POOL, &usdc_pool, fair_value(&usdc_pool)),
        (
            BISONFI_SOL_USDT,
            &usdt_pool,
            fair_value(&usdt_pool) * 1_015 / 1_000,
        ),
    ] {
        svm.inner
            .set_account(
                Pubkey::from_str_const(pool),
                solana_account::Account {
                    lamports: 1_000_000,
                    data: data.clone(),
                    owner: Pubkey::from_str_const(BISONFI_PROGRAM),
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap_or_else(|e| panic!("seed {pool}: {e:?}"));
        scenario.add_override(
            OverrideInstance::new(
                "bisonfi-fair-value".to_string(),
                0,
                AccountAddress::Pubkey(pool.to_string()),
            )
            .with_values(HashMap::from([(
                "fair_value".to_string(),
                serde_json::json!(fair_value.to_string()),
            )])),
        );
    }
    svm.register_scenario(scenario, Some(base_slot))
        .expect("register the Cross-Pair Arbitrage scenario");
    let last = base_slot + PLAY_FOR;
    let usdt_image = bisonfi_play(
        &mut svm,
        Pubkey::from_str_const(BISONFI_SOL_USDT),
        base_slot,
        last,
    )
    .await
    .remove(&last)
    .expect("the SOL/USDT pool at the last slot");
    let usdc_image = svm
        .inner
        .get_account(&Pubkey::from_str_const(BISONFI_POOL))
        .expect("get_account")
        .expect("the SOL/USDC pool")
        .data;
    assert_eq!(
        published(&usdc_image),
        last,
        "keep-alive must republish the SOL/USDC pool too"
    );

    let profit = cycle(&usdc_image, &usdt_image);
    assert!(
        control < 50,
        "the same cycle on mainnet state must not hold an arbitrage: {control} bp"
    );
    assert!(
        profit > 100,
        "a 1.5% cross-pair gap less BisonFi's and Orca's costs must pay: {profit} bp (mainnet {control} bp)"
    );
}

/// A stale quote suppresses the price and spread levers entirely, on every market that quotes.
///
/// This is a PRECEDENCE property: the freshness gate is evaluated before the venue consults its mid
/// or its ladder, so an override that lands byte-perfectly in the account has no effect at all and
/// the transaction still succeeds. It is the most consequential thing to know about combining these
/// templates, and the failure it describes is invisible - no revert, no log, correct bytes.
///
/// It is also the property most likely to break silently. If a redeploy ever evaluated the quote
/// before the freshness check, every scenario in this suite would keep passing while meaning
/// something different.
///
/// The fresh leg is what stops this passing vacuously: doubling the published mid on a fresh market
/// has to double the fill, so a run where everything returned zero fails rather than looking green.
#[tokio::test]
async fn bisonfi_staleness_suppresses_the_price_and_spread_levers() {
    /// Comfortably past every threshold observed across live markets and deployed builds.
    const STALE_BY: u64 = 1_000_000;

    let rig = bisonfi_rig().await;
    let mut checked = 0usize;

    for (pool, data, tp) in &rig.quoting {
        let size = BisonfiRig::sell_size(data);
        let published = u64::from_le_bytes(data[72..80].try_into().unwrap());
        let stale_publication = published
            .checked_sub(STALE_BY)
            .expect("live publication slot must exceed the deliberate stale age");
        let mid = u128::from_le_bytes(data[832..848].try_into().unwrap());
        let doubled = mid * 2;
        let double_mid = || {
            bisonfi_apply_template(
                "bisonfi-fair-value",
                &[("fair_value", serde_json::json!(doubled.to_string()))],
            )
        };

        let baseline = bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, |_| {})
            .unwrap_or_else(|e| panic!("{pool}: control sell must price: {e}"));
        assert!(baseline > 0, "{pool}: control sell returned nothing");

        // Fresh: the price lever works. Without this leg the assertions below would be satisfied by
        // a market that simply never quotes.
        let fresh_doubled = bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, double_mid())
            .unwrap_or_else(|e| panic!("{pool}: fresh market with a doubled mid must price: {e}"));
        let ratio = fresh_doubled as f64 / baseline as f64;
        assert!(
            (1.9..=2.1).contains(&ratio),
            "{pool}: doubling the mid on a FRESH market should about double the fill, got \
             {fresh_doubled} against {baseline} (ratio {ratio:.3}). The price lever is not working, \
             so this test cannot say anything about staleness suppressing it"
        );

        // Stale: the same override, byte-identical, now does nothing.
        let stale_doubled = bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, {
            let apply = double_mid();
            move |d: &mut Vec<u8>| {
                d[72..80].copy_from_slice(&stale_publication.to_le_bytes());
                apply(d);
            }
        })
        .unwrap_or_else(|error| panic!("{pool}: stale price check must execute: {error}"));
        assert_eq!(
            stale_doubled, 0,
            "{pool}: a market {STALE_BY} slots stale must ignore a doubled mid, but it paid \
             {stale_doubled}. The freshness gate no longer runs first, and every scenario that sets \
             a price after spending slots would now behave differently"
        );

        // And the same for the spread lever.
        let stale_spread = bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, {
            let apply = bisonfi_apply_template("bisonfi-spread", &bisonfi_spread_bids(-13));
            move |d: &mut Vec<u8>| {
                d[72..80].copy_from_slice(&stale_publication.to_le_bytes());
                apply(d);
            }
        })
        .unwrap_or_else(|error| panic!("{pool}: stale spread check must execute: {error}"));
        assert_eq!(
            stale_spread, 0,
            "{pool}: a market {STALE_BY} slots stale must ignore a spread override, but it paid \
             {stale_spread}"
        );

        checked += 1;
    }

    assert_eq!(
        checked,
        rig.quoting.len(),
        "only {checked} of the quoting markets exercised the precedence of the freshness gate"
    );
}

/// The market tick is a tenth swap account. Without it a gated market succeeds and pays nothing.
/// Pinned so a program upgrade that changes this is noticed.
#[tokio::test]
async fn bisonfi_a_swap_without_the_market_tick_pays_nothing() {
    let rig = bisonfi_rig().await;
    for (pool, data, tp) in &rig.quoting {
        let size = BisonfiRig::sell_size(data);
        let with_tick = bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, |_| {})
            .unwrap_or_else(|e| panic!("{pool}: control failed: {e}"));
        assert!(with_tick > 0, "{pool}: the control must fill");

        OMIT_MARKET_TICK.with(|omit| omit.set(true));
        let without = bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, |_| {});
        OMIT_MARKET_TICK.with(|omit| omit.set(false));
        assert_eq!(
            without,
            Ok(0),
            "{pool}: a swap without the tick must succeed and pay nothing"
        );
    }
}

/// How the program uses the market tick: it grades a quote's age - the tick's time minus the pool's
/// publication time (byte 88) - to the millisecond. Up to 150 ms it prices the full book, older it
/// falls back to a thinner one, and either way it still fills. So a late quote must never fill
/// better than a punctual one and must fill worse on most markets.
#[tokio::test]
async fn bisonfi_quote_age_grades_the_book() {
    let rig = bisonfi_rig().await;
    let mut thinner = 0usize;
    for (pool, data, tp) in &rig.quoting {
        let size = BisonfiRig::sell_size(data);
        // Replays run their clock at the pool's own publication time, so moving byte 88 back by
        // `late_ms` makes the quote exactly that old.
        let at = |late_ms: u64| {
            bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, |d| {
                let published = u64::from_le_bytes(d[88..96].try_into().unwrap());
                d[88..96].copy_from_slice(&(published - late_ms * 1_000_000).to_le_bytes());
            })
            .unwrap_or_else(|e| panic!("{pool}: {e}"))
        };
        let (fresh, late, aged) = (at(0), at(250), at(500));
        assert!(
            fresh > 0 && late > 0 && aged > 0,
            "{pool}: a late quote must still fill"
        );
        assert!(
            late <= fresh && aged <= fresh,
            "{pool}: a late quote filled better than a fresh one: {fresh} vs {late} / {aged}"
        );
        if late < fresh {
            thinner += 1;
        }
    }
    assert!(
        thinner * 2 >= rig.quoting.len(),
        "only {thinner} of {} markets thinned a 250 ms old quote; the program's quote-age grading \
         seems to have changed",
        rig.quoting.len()
    );
}

/// A pool fetched while the surfnet is behind mainnet carries mainnet's later rebuild slot (byte
/// 688), and the program refills a consumed book only while that slot is in the past. Keep-alive's
/// publication moves it back, so a republished slot refills such a pool too - and a publication of
/// the slot alone would not.
#[tokio::test]
async fn bisonfi_keep_alive_refills_a_pool_fetched_ahead_of_the_surfnet() {
    let rig = bisonfi_rig().await;
    let mut refilled_somewhere = false;
    for (pool, data, tp) in &rig.quoting {
        let size = BisonfiRig::sell_size(data) / 2;
        let (first, after) =
            bisonfi_replay_with_state(&rig.elf, pool, data, *tp, size, 0, 0, |_| {})
                .unwrap_or_else(|e| panic!("{pool}: first sell failed: {e}"));

        // The next slot, with the reserves put back so only the book differs, and a rebuild slot
        // from mainnet's future, as a fetch behind mainnet leaves it.
        let slot = u64::from_le_bytes(after[72..80].try_into().unwrap()) + 1;
        let mut next = after.clone();
        next[48..64].copy_from_slice(&data[48..64]);
        next[688..696].copy_from_slice(&(slot + 30).to_le_bytes());

        let mut slot_only = next.clone();
        slot_only[72..80].copy_from_slice(&slot.to_le_bytes());
        let kept_alive = bisonfi_keep_alive(
            "bisonfi-quote-publication",
            &next,
            slot,
            publish_millis(&next),
        );
        let sell = |published: Vec<u8>| {
            bisonfi_replay(&rig.elf, pool, &published, *tp, size, 0, |_| {})
                .unwrap_or_else(|e| panic!("{pool}: republished sell failed: {e}"))
        };
        let (stuck, refilled) = (sell(slot_only), sell(kept_alive));

        let drift_bps = (refilled as f64 / first as f64 - 1.0).abs() * 1e4;
        assert!(
            refilled >= stuck && drift_bps < 5.0,
            "{pool}: a kept-alive slot should fill like the first swap: first {first}, slot only \
             {stuck}, kept alive {refilled} ({drift_bps:.2} bps)"
        );
        refilled_somewhere |= refilled > stuck;
    }
    assert!(
        refilled_somewhere,
        "no market refilled better with the rebuild slot moved back; the program's rebuild gate \
         seems to have changed"
    );
}

/// Swaps consume the working ladder, so a second swap in the same slot fills worse. The program
/// refills the ladder from its source on the first swap of a slot the pool was published in, which on
/// a surfnet means every slot keep-alive republishes it in.
#[tokio::test]
async fn bisonfi_a_fresh_slot_refills_what_a_swap_consumed() {
    let rig = bisonfi_rig().await;
    let mut consumed_somewhere = false;
    for (pool, data, tp) in &rig.quoting {
        let size = BisonfiRig::sell_size(data) / 2;
        let (first, after) =
            bisonfi_replay_with_state(&rig.elf, pool, data, *tp, size, 0, 0, |_| {})
                .unwrap_or_else(|e| panic!("{pool}: first sell failed: {e}"));
        assert!(first > 0, "{pool}: the first sell must fill");

        // Put the reserves back so only the ladder state differs from the first swap's.
        let mut same_slot = after.clone();
        same_slot[48..64].copy_from_slice(&data[48..64]);
        let second = bisonfi_replay(&rig.elf, pool, &same_slot, *tp, size, 0, |_| {})
            .unwrap_or_else(|e| panic!("{pool}: second sell failed: {e}"));
        assert!(
            second <= first,
            "{pool}: a second sell in the same slot filled better ({second} > {first})"
        );
        consumed_somewhere |= second < first;

        // The next slot, republished by keep-alive: the replay's clock follows the publication.
        let slot = u64::from_le_bytes(same_slot[72..80].try_into().unwrap());
        let next_slot = bisonfi_keep_alive(
            "bisonfi-quote-publication",
            &same_slot,
            slot + 1,
            publish_millis(&same_slot) + 400,
        );
        let refilled = bisonfi_replay(&rig.elf, pool, &next_slot, *tp, size, 0, |_| {})
            .unwrap_or_else(|e| panic!("{pool}: refreshed sell failed: {e}"));
        let drift_bps = (refilled as f64 / first as f64 - 1.0).abs() * 1e4;
        assert!(
            refilled >= second && drift_bps < 5.0,
            "{pool}: a refreshed slot should fill like the first swap: first {first}, same slot \
             {second}, refreshed {refilled} ({drift_bps:.2} bps)"
        );
    }
    assert!(
        consumed_somewhere,
        "no market filled a second swap worse; the ladder is no longer consumed by swaps"
    );
}

// ---------------------------------------------------------------- live surfnet

mod live_surfnet {
    //! The same swaps, end to end: a surfnet forking mainnet, driven only over JSON-RPC. The
    //! replay tests above prove what the program does with given bytes; these prove Surfpool
    //! produces those bytes on its own - the pool's publication and the tick, kept alive every block
    //! by a registered scenario - with nothing written into the fork by hand.

    use std::time::{Duration, Instant};

    use base64::Engine;
    use crossbeam_channel::unbounded;
    use solana_client::{nonblocking::rpc_client::RpcClient, rpc_request::RpcRequest};
    use solana_commitment_config::CommitmentConfig;
    use solana_compute_budget_interface::ComputeBudgetInstruction;
    use solana_instruction::{AccountMeta, Instruction};
    use solana_keypair::Keypair;
    use solana_pubkey::Pubkey;
    use solana_signature::Signature;
    use solana_signer::Signer;
    use solana_transaction::Transaction;
    use surfpool_types::{
        AccountAddress, OverrideInstance, RpcConfig, Scenario, SimnetConfig, SurfpoolConfig,
    };

    use super::{BISONFI_NINTH, BISONFI_PROGRAM, DEFAULT_RPC_URL, MARKET_TICK, RPC_URL_ENV};
    use crate::{
        storage::tests::TestType,
        surfnet::locker::SurfnetSvmLocker,
        tests::{
            helpers::get_free_port,
            integration::{RunloopGuard, spawn_runloop, wait_for_ready_and_connected},
        },
    };

    /// SOL-USDC-Pool2: live, tick-gated, and both mints are plain SPL tokens.
    const POOL: Pubkey = Pubkey::from_str_const("8FnX3xo2yYw3EUE6w3nQA4GfXGS9wpK6oj3veJpbFzLo");
    const TOKENKEG: Pubkey = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
    const ATA_PROGRAM: Pubkey =
        Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
    const CLOCK_SYSVAR: Pubkey =
        Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111");
    /// Ten USDC per purchase: small against the pool, so every fill sits on the inner book.
    const QUOTE_IN: u64 = 10_000_000;

    fn start_surfnet() -> (RpcClient, RunloopGuard) {
        let bind_host = "127.0.0.1";
        let bind_port = get_free_port().unwrap();
        let ws_port = get_free_port().unwrap();
        let config = SurfpoolConfig {
            simnets: vec![SimnetConfig {
                remote_rpc_url: Some(
                    std::env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_RPC_URL.to_string()),
                ),
                ..SimnetConfig::default()
            }],
            rpc: RpcConfig {
                bind_host: bind_host.to_string(),
                bind_port,
                ws_port,
                ..RpcConfig::default()
            },
            ..SurfpoolConfig::default()
        };
        let (svm, events_rx, geyser_rx) = TestType::no_db().initialize_svm();
        let (commands_tx, commands_rx) = unbounded();
        let runloop = spawn_runloop(
            SurfnetSvmLocker::new(svm),
            config,
            (commands_tx, commands_rx),
            geyser_rx,
        )
        .expect("the runloop should start");
        wait_for_ready_and_connected(&events_rx)
            .expect("surfnet should be ready and connected to the datasource");
        let rpc = RpcClient::new_with_commitment(
            format!("http://{bind_host}:{bind_port}"),
            CommitmentConfig::processed(),
        );
        (rpc, runloop)
    }

    async fn cheatcode(rpc: &RpcClient, method: &'static str, params: serde_json::Value) {
        rpc.send::<serde_json::Value>(RpcRequest::Custom { method }, params)
            .await
            .unwrap_or_else(|e| panic!("{method} failed: {e}"));
    }

    fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[owner.as_ref(), TOKENKEG.as_ref(), mint.as_ref()],
            &ATA_PROGRAM,
        )
        .0
    }

    fn u64_at(data: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
    }

    async fn wait_for_slot(rpc: &RpcClient, slot: u64) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while rpc.get_slot().await.expect("slot") < slot {
            assert!(
                Instant::now() < deadline,
                "the surfnet stopped producing blocks"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Registers a scenario that restates the pool's mid once, which keeps the pool and the tick
    /// alive every later slot, and waits until the pool shows it. Nothing is written into the pool
    /// except through the scenario.
    ///
    /// No base slot is sent. Surfpool never applies an override scheduled for a slot that has
    /// already begun, so a base read here and sent in a later request loses its first slot whenever
    /// a block boundary falls between the two; without one the server schedules from its next slot.
    async fn keep_alive(rpc: &RpcClient) {
        let pool = rpc.get_account(&POOL).await.expect("pool").data;
        let fair_value = u128::from_le_bytes(pool[832..848].try_into().unwrap());
        let mut scenario = Scenario::new(
            "BisonFi keep-alive".to_string(),
            "Keeps SOL-USDC-Pool2 quoting".to_string(),
        );
        scenario.add_override(
            OverrideInstance::new(
                "bisonfi-fair-value".to_string(),
                1,
                AccountAddress::Pubkey(POOL.to_string()),
            )
            .with_values(
                [(
                    "fair_value".to_string(),
                    serde_json::json!(fair_value.to_string()),
                )]
                .into(),
            ),
        );
        cheatcode(
            rpc,
            "surfnet_registerScenario",
            serde_json::json!([scenario]),
        )
        .await;

        // The override sits one slot past the server's next slot, so at most two slots past the slot
        // read here, and keep-alive runs in every slot from it on: once the surfnet reaches it the
        // pool holds keep-alive's image. The pool's own publication slot proves nothing: fetched
        // from a mainnet running ahead of the surfnet, it already reads newer than the surfnet's.
        let scheduled = rpc.get_slot().await.expect("slot") + 2;
        wait_for_slot(rpc, scheduled).await;
        assert_publication_follows_the_clock(rpc).await;
    }

    /// Keep-alive republished the pool: its publication time follows the surfnet clock, and its last
    /// book rebuild is in the past, so the next swap refills it.
    async fn assert_publication_follows_the_clock(rpc: &RpcClient) {
        let accounts = rpc
            .get_multiple_accounts(&[POOL, CLOCK_SYSVAR])
            .await
            .expect("pool and clock");
        let pool = accounts[0].as_ref().expect("pool");
        let clock = accounts[1].as_ref().expect("clock sysvar");
        assert!(
            u64_at(&pool.data, 688) < u64_at(&clock.data, 0),
            "the pool's last book rebuild must be in the past for the next swap to refill it"
        );
        let published_seconds = u64_at(&pool.data, 88) / 1_000_000_000;
        let clock_seconds = i64::from_le_bytes(clock.data[32..40].try_into().unwrap()) as u64;
        assert!(
            published_seconds.abs_diff(clock_seconds) <= 1,
            "the pool reads published at {published_seconds}s, the Clock {clock_seconds}s"
        );
    }

    /// The tick and the Clock read in one request, so they describe the same moment.
    async fn assert_tick_matches_clock(rpc: &RpcClient, when: &str) {
        let accounts = rpc
            .get_multiple_accounts(&[MARKET_TICK, CLOCK_SYSVAR])
            .await
            .expect("tick and clock");
        let tick = accounts[0]
            .as_ref()
            .unwrap_or_else(|| panic!("{when}: the surfnet holds no market tick"));
        let clock = accounts[1].as_ref().expect("clock sysvar");
        let (clock_slot, clock_seconds) = (
            u64_at(&clock.data, 0),
            i64::from_le_bytes(clock.data[32..40].try_into().unwrap()),
        );
        assert_eq!(
            u64_at(&tick.data, 40),
            clock_slot,
            "{when}: the tick must name the Clock's slot"
        );
        let tick_seconds = (u64_at(&tick.data, 56) + 500_000_000) / 1_000_000_000;
        assert!(
            tick_seconds.abs_diff(clock_seconds as u64) <= 1,
            "{when}: the tick reads {tick_seconds}s, the Clock {clock_seconds}s"
        );
    }

    struct Taker {
        keypair: Keypair,
        base_vault: Pubkey,
        quote_vault: Pubkey,
        user_base: Pubkey,
        user_quote: Pubkey,
    }

    /// Buys SOL with `QUOTE_IN` USDC through `sendTransaction`, returning the SOL received. The
    /// ninth account is a signer the program requires and no test holds, so the swap is sent with
    /// `skipSigVerify` - the surfnet's way of acting for an account without its key.
    async fn buy(rpc: &RpcClient, taker: &Taker) -> u64 {
        let before = rpc
            .get_account(&taker.user_base)
            .await
            .map(|a| u64_at(&a.data, 64))
            .expect("base account");

        let mut data = vec![0x07];
        data.extend_from_slice(&QUOTE_IN.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes());
        data.extend_from_slice(&[1, 0]); // direction 1 buys the base token
        let swap = Instruction {
            program_id: Pubkey::from_str_const(BISONFI_PROGRAM),
            accounts: vec![
                AccountMeta::new(taker.keypair.pubkey(), true),
                AccountMeta::new(POOL, false),
                AccountMeta::new(taker.base_vault, false),
                AccountMeta::new(taker.quote_vault, false),
                AccountMeta::new(taker.user_base, false),
                AccountMeta::new(taker.user_quote, false),
                AccountMeta::new_readonly(TOKENKEG, false),
                AccountMeta::new_readonly(TOKENKEG, false),
                AccountMeta::new_readonly(Pubkey::from_str_const(BISONFI_NINTH), true),
                AccountMeta::new_readonly(MARKET_TICK, false),
            ],
            data,
        };
        let budget = ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);
        let blockhash = rpc.get_latest_blockhash().await.expect("blockhash");
        let message = solana_message::Message::new_with_blockhash(
            &[budget, swap],
            Some(&taker.keypair.pubkey()),
            &blockhash,
        );
        let mut tx = Transaction::new_unsigned(message);
        tx.signatures =
            vec![Signature::default(); tx.message.header.num_required_signatures as usize];
        tx.signatures[0] = taker.keypair.sign_message(&tx.message.serialize());

        let encoded =
            base64::engine::general_purpose::STANDARD.encode(bincode::serialize(&tx).unwrap());
        let signature: String = rpc
            .send(
                RpcRequest::SendTransaction,
                serde_json::json!([encoded, {
                    "encoding": "base64",
                    "skipPreflight": true,
                    "skipSigVerify": true,
                }]),
            )
            .await
            .expect("the swap should be accepted");
        let signature: Signature = signature.parse().unwrap();

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = rpc
                .get_signature_statuses(&[signature])
                .await
                .expect("signature status")
                .value
                .remove(0);
            if let Some(status) = status {
                assert!(status.err.is_none(), "the swap failed: {:?}", status.err);
                break;
            }
            assert!(Instant::now() < deadline, "the swap was never processed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let after = rpc
            .get_account(&taker.user_base)
            .await
            .map(|a| u64_at(&a.data, 64))
            .expect("base account");
        after - before
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bisonfi_swaps_on_a_live_surfnet_across_blocks_and_time_travel() {
        let (rpc, _runloop) = start_surfnet();

        // Reading the pool through the surfnet forks it from mainnet.
        let pool = rpc.get_account(&POOL).await.expect("pool").data;
        let base_mint = Pubkey::new_from_array(pool[184..216].try_into().unwrap());
        let quote_mint = Pubkey::new_from_array(pool[216..248].try_into().unwrap());
        let keypair = Keypair::new();
        let owner = keypair.pubkey();
        cheatcode(
            &rpc,
            "surfnet_setAccount",
            serde_json::json!([owner.to_string(), { "lamports": 10_000_000_000u64 }]),
        )
        .await;
        for (mint, amount) in [(base_mint, 0u64), (quote_mint, 1_000_000_000)] {
            cheatcode(
                &rpc,
                "surfnet_setTokenAccount",
                serde_json::json!([
                    owner.to_string(),
                    mint.to_string(),
                    { "amount": amount },
                    TOKENKEG.to_string()
                ]),
            )
            .await;
        }
        let taker = Taker {
            base_vault: Pubkey::new_from_array(pool[120..152].try_into().unwrap()),
            quote_vault: Pubkey::new_from_array(pool[152..184].try_into().unwrap()),
            user_base: ata(&owner, &base_mint),
            user_quote: ata(&owner, &quote_mint),
            keypair,
        };

        // 1. A registered scenario keeps the pool and the tick alive, so the pool fills. Keep-alive
        //    loads the tick from upstream itself; no swap has named it yet.
        keep_alive(&rpc).await;
        assert_tick_matches_clock(&rpc, "before the first swap").await;
        let first = buy(&rpc, &taker).await;
        assert!(first > 0, "a kept-alive pool must fill on a surfnet");

        // 2. Many blocks later it still fills, with nothing scheduled after the first override.
        let later = rpc.get_slot().await.expect("slot") + 20;
        wait_for_slot(&rpc, later).await;
        assert_tick_matches_clock(&rpc, "many blocks later").await;
        let kept = buy(&rpc, &taker).await;
        assert!(kept > 0, "a kept-alive pool must keep filling");

        // 3. Time travel with the clock paused: keep-alive republishes the pool and the tick for the
        //    slot the jump lands on, before any block is produced.
        cheatcode(&rpc, "surfnet_pauseClock", serde_json::json!([])).await;
        let target = rpc.get_slot().await.expect("slot") + 10_000;
        cheatcode(
            &rpc,
            "surfnet_timeTravel",
            serde_json::json!([{ "absoluteSlot": target }]),
        )
        .await;
        let landed = rpc.get_slot().await.expect("slot");
        assert!(landed >= target);
        assert_tick_matches_clock(&rpc, "right after time travel").await;
        assert_publication_follows_the_clock(&rpc).await;
        // A swap executes at once, but its status is only published with a block, so read the
        // balance instead of waiting for the status.
        let before = rpc
            .get_account(&taker.user_base)
            .await
            .map(|a| u64_at(&a.data, 64))
            .expect("base account");
        let _ = tokio::time::timeout(Duration::from_secs(3), buy(&rpc, &taker)).await;
        let after = rpc
            .get_account(&taker.user_base)
            .await
            .map(|a| u64_at(&a.data, 64))
            .expect("base account");
        assert_eq!(
            rpc.get_slot().await.expect("slot"),
            landed,
            "the clock is paused, so no block can have refreshed anything"
        );
        let travelled = after - before;
        assert!(
            travelled > 0,
            "a kept-alive pool must fill right after time travel"
        );
        cheatcode(&rpc, "surfnet_resumeClock", serde_json::json!([])).await;

        // The same purchase against the same price should buy about the same SOL each time.
        for fill in [kept, travelled] {
            let drift_bps = (fill as f64 / first as f64 - 1.0).abs() * 1e4;
            assert!(
                drift_bps < 50.0,
                "fills drifted {drift_bps:.1} bps from the first: {first} vs {fill}"
            );
        }
    }
}
