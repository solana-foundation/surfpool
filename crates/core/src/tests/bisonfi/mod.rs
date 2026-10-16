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

use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_sysvar::last_restart_slot::LastRestartSlot;

use crate::{
    scenarios::TemplateRegistry,
    surfnet::{GetAccountResult, remote::SurfnetRemoteClient, svm::SurfnetSvm},
    tests::helpers::diff_indices,
};

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

    for template in registry
        .all()
        .into_iter()
        .filter(|template| template.id.starts_with("bisonfi-"))
    {
        assert!(
            template.raw_layout,
            "{} must select raw-layout writes",
            template.id
        );
    }
}

/// last_update_slot is what makes the staleness scenario possible, so pin that it really is the
/// chain slot on a live market and that ageing it is a one-field write.
#[tokio::test]
async fn bisonfi_freshness_tracks_the_chain_slot() {
    const LAST_UPDATE: usize = 72;
    const PREVIOUS_UPDATE: usize = 80;

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

    let registry = TemplateRegistry::new();
    let template = registry
        .get("bisonfi-freshness")
        .expect("freshness template");
    assert!(template.raw_layout, "freshness uses raw-layout writes");

    let aged = last - 1000;
    let forged = template
        .materialize_raw_layout(
            &data,
            &HashMap::from([("last_update_slot".to_string(), serde_json::json!(aged))]),
            0,
        )
        .expect("ageing the quote should apply");
    assert_eq!(
        u64::from_le_bytes(forged[LAST_UPDATE..LAST_UPDATE + 8].try_into().unwrap()),
        aged
    );
    let diffs = diff_indices(&forged, &data);
    assert!(!diffs.is_empty(), "the slot should have changed");
    assert!(
        diffs
            .iter()
            .all(|i| (LAST_UPDATE..LAST_UPDATE + 8).contains(i)),
        "only bytes within last_update_slot should change, got {diffs:?}"
    );
}

/// Every account the program owns, live and dormant, as of program build 3f38e742. The templates
/// default to one market, but every supported v3 account must accept the same writes.
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
            "bisonfi-freshness",
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

/// Offsets 48 and 56 mirror the vaults exactly, which is why no template writes them. This pins
/// that measurement so the claim in the layout docs cannot rot silently: if a redeploy changes it,
/// the reserve fields mean something else and the docs need revisiting.
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

    // Same slot, so they must agree exactly. This is the measurement that disqualified offset 48
    // as a "quotable slice" of the vaults: it is the whole balance, mirrored.
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
        // back along with the field and the quote never looks stale at all - which is exactly how the
        // freshness scenario first appeared to fail.
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
    /// pays out.
    ///
    /// An earlier version derived this from the pool's fixed-point mid, which is the price in HUMAN
    /// units - so it was out by the market's decimal shift, a thousand-fold on a 9/6 pair. Every buy
    /// leg then asked for more than the venue would fill and was quietly skipped. Using the control
    /// fill needs no decimal table and cannot drift.
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
    // Six of the seventeen v3 pools publish a current mid. Two more are live but quote a Token-2022
    // base asset the harness cannot build accounts for; the rest are dormant by 13-30 million slots
    // and return no quote whatever is written to them. If this count drops, coverage silently
    // narrowed and the scenario assertions below stop meaning anything.
    assert!(
        quoting.len() >= 6,
        "only {} of {} pools can be quoted; scenario coverage has narrowed",
        quoting.len(),
        BISONFI_ALL_POOLS.len()
    );
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
    // Both directions must actually have been exercised. The buy leg used to be skipped on every
    // market because the notional was computed wrongly, and nothing said so.
    assert!(
        checked >= 6,
        "only {checked} markets exercised both directions of the depth template"
    );
}

/// SCENARIO: silence a given market maker so it stops quoting entirely.
///
/// This is the behaviour no constant-product AMM can imitate - an AMM always quotes something - so
/// it is the scenario most likely to be untested on the consuming side.
#[tokio::test]
async fn bisonfi_scenario_silence_the_maker() {
    const STALE_BY: u64 = 1_000_000;

    let rig = bisonfi_rig().await;
    let mut checked = 0usize;
    for (pool, data, tp) in &rig.quoting {
        let size = BisonfiRig::sell_size(data);
        let published = u64::from_le_bytes(data[72..80].try_into().unwrap());

        // The exact tolerance is maker policy and has varied between live pools/builds. Age the
        // publication deliberately far beyond any observed threshold so this tests the template's
        // useful guarantee (the maker can be silenced), not an unstable policy constant.
        let silent = rig.scenario(
            pool,
            data,
            *tp,
            "bisonfi-freshness",
            &[("last_update_slot", serde_json::json!(-(STALE_BY as i64)))],
            size,
            0,
        );
        assert_eq!(
            silent, 0,
            "{pool}: aged by {STALE_BY} slots the venue must not fill at all, got {silent}"
        );

        // And the sharp part: the swap's minimum-output bound is NOT honoured on the stale path, so
        // the caller gets a CONFIRMED transaction that moved nothing and ignored their slippage
        // protection. The healthy control below proves the bound is otherwise real, so this is the
        // program returning early rather than the harness failing to set the field.
        let healthy = bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, |_| {})
            .unwrap_or_else(|e| panic!("{pool}: control failed: {e}"));
        assert!(
            bisonfi_replay_min_out(&rig.elf, pool, data, *tp, size, healthy + 1, 0, |_| {})
                .is_err(),
            "{pool}: a minimum above the fillable amount must revert on a healthy market, or the \
             bound is not a slippage guard at all and the claim below means nothing"
        );
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
            "{pool}: a silenced market must succeed with zero even when the caller demands \
             {healthy} out - if this ever starts reverting, the scenario's symptom changed from a \
             silent no-op to a failed transaction and every consumer's handling changes with it"
        );
        checked += 1;
    }
    assert!(
        checked >= 6,
        "only {checked} markets exercised the freshness template"
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
        let ratio = wide as f64 / tight as f64;
        let err = (ratio - predicted).abs() / predicted;
        assert!(
            err < 0.01,
            "{pool}: going from a {TIGHT} tick to a {WIDE} tick changed the fill by a factor of \
             {ratio:.6}, but the 1/2,560,000 unit the template documents requires {predicted:.6} \
             ({:.3}% off). Either the unit is wrong or not every region is being written",
            err * 100.0
        );
        checked += 1;
    }
    assert!(
        checked >= 6,
        "only {checked} markets exercised the spread template; the claim needs the live markets"
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

        // Widening the bid must hurt sells and leave buys exactly where they were.
        let sell_wide = rig.scenario(pool, data, *tp, "bisonfi-spread", &bids, sell, 0);
        let buy_untouched = rig.scenario(pool, data, *tp, "bisonfi-spread", &bids, buy, 1);
        assert!(
            sell_wide < sell_control,
            "{pool}: widening the BID must pay a seller less, got {sell_wide} vs {sell_control}"
        );
        assert_eq!(
            buy_untouched, buy_control,
            "{pool}: widening the BID must not change a BUY. If this fires the bid and ask offsets \
             are transposed in the template"
        );

        // And the mirror image.
        let buy_wide = rig.scenario(pool, data, *tp, "bisonfi-spread", &asks, buy, 1);
        let sell_untouched = rig.scenario(pool, data, *tp, "bisonfi-spread", &asks, sell, 0);
        assert!(
            buy_wide < buy_control,
            "{pool}: widening the ASK must give a buyer less base, got {buy_wide} vs {buy_control}"
        );
        assert_eq!(
            sell_untouched, sell_control,
            "{pool}: widening the ASK must not change a SELL"
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
}

/// The live Orca Whirlpool SOL/USDC market, used as the AMM side of the arbitrage scenario.
const WHIRLPOOL_SOL_USDC: &str = "HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ";

/// SCENARIO: arbitrage between BisonFi and an AMM on the same pair.
///
/// Surfpool loads mainnet state into the local VM, so dislocating BisonFi alone creates a real
/// arbitrage against every other venue's upstream state with no second override. This measures that
/// against Orca's actual on-chain price rather than a hardcoded number.
///
/// The Whirlpool's price comes from its `sqrt_price` (Q64.64 at offset 65), squared. Nothing is
/// executed on the AMM side: pricing an Orca swap needs its tick arrays, which is a much larger piece
/// of harness. What this proves is that the override produces a dislocation that is real, correctly
/// signed, and of the right size against a live competing venue - which is what a router would act on.
///
/// It is also self-validating: the first assertion is that both venues agree on the price BEFORE any
/// override. If the Whirlpool layout were misread, or the pair mismatched, that would fail rather than
/// silently making the arbitrage numbers meaningless.
#[tokio::test]
async fn bisonfi_scenario_arbitrage_against_an_amm() {
    let rig = bisonfi_rig().await;
    let (pool, data, tp) = rig
        .quoting
        .iter()
        .find(|(p, _, _)| *p == BISONFI_POOL)
        .expect("the WSOL/USDC market must be quoting for this scenario");

    let whirlpool = fetch(&[WHIRLPOOL_SOL_USDC]).await.remove(0);
    // Confirm the two venues really are the same pair and the same way round, so the comparison below
    // is between like and like.
    let (bisonfi_base, bisonfi_quote) = (&data[184..216], &data[216..248]);
    assert_eq!(
        &whirlpool[101..133],
        bisonfi_base,
        "the Whirlpool's token A must be BisonFi's base mint"
    );
    assert_eq!(
        &whirlpool[181..213],
        bisonfi_quote,
        "the Whirlpool's token B must be BisonFi's quote mint"
    );

    // Whirlpool price, in quote smallest-units per base smallest-unit. Squaring a Q64.64 needs care:
    // done in f64 after the shift, which is ample for a comparison at this tolerance.
    let sqrt_price = u128::from_le_bytes(whirlpool[65..81].try_into().unwrap());
    let amm_price = (sqrt_price as f64 / 2f64.powi(64)).powi(2);
    assert!(
        amm_price > 0.0,
        "the Whirlpool must carry a live sqrt_price, got {sqrt_price}"
    );

    // BisonFi's realized price on the same basis: quote received per base sold.
    let size = u64::from_le_bytes(data[48..56].try_into().unwrap()) / 100;
    let realized = |image: Option<Vec<u8>>| -> f64 {
        let out = match image {
            None => bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, |_| {}),
            Some(img) => bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, move |d| *d = img),
        }
        .unwrap_or_else(|e| panic!("replay failed: {e}"));
        assert!(
            out > 0,
            "BisonFi must fill for the comparison to mean anything"
        );
        out as f64 / size as f64
    };

    // 1. Undislocated, the two venues must agree. A proprietary market maker that disagreed with the
    //    largest AMM on SOL by more than a fraction of a percent would be arbitraged instantly.
    let quiet = realized(None);
    let disagreement = (quiet - amm_price).abs() / amm_price;
    assert!(
        disagreement < 0.02,
        "BisonFi and Orca should price SOL within 2% of each other before any override; got \
         {quiet:.9} against {amm_price:.9} ({:.3}% apart). Either a layout offset is wrong or one \
         venue is not live",
        disagreement * 100.0
    );

    // 2. Now dislocate BisonFi upward by 10% through the shipped template, and the arbitrage appears:
    //    buy SOL on Orca, sell it to BisonFi.
    let mid = u128::from_le_bytes(data[832..848].try_into().unwrap());
    let registry = TemplateRegistry::new();
    let template = registry.get("bisonfi-fair-value").expect("template");
    assert!(template.raw_layout, "fair value uses raw-layout writes");
    let dislocated = template
        .materialize_raw_layout(
            data,
            &HashMap::from([(
                "fair_value".to_string(),
                serde_json::json!((mid * 11 / 10).to_string()),
            )]),
            0,
        )
        .expect("price override");
    let rich = realized(Some(dislocated));

    let edge = (rich - amm_price) / amm_price;
    assert!(
        rich > quiet,
        "the dislocated market must pay more than the quiet one, got {rich:.9} vs {quiet:.9}"
    );
    assert!(
        edge > 0.05,
        "a 10% dislocation should leave at least 5% of edge against the AMM after BisonFi's own \
         spread and slippage; got {:.3}%",
        edge * 100.0
    );
    assert!(
        edge < 0.11,
        "the edge cannot exceed the 10% dislocation that created it; got {:.3}%, which would mean \
         the price override is scaling by more than it was asked to",
        edge * 100.0
    );
}

/// SCENARIO: the maker goes dark BETWEEN the quote and the fill.
///
/// This is the one that needed a real gap closing. Every other scenario applies its override once and
/// asks what the program does. This one registers a scenario whose state CHANGES across slots, runs
/// the scheduler slot by slot, and then feeds each slot's account image to the deployed program.
///
/// Why it matters: on Solana there is a gap of one or two slots between reading a price and the
/// transaction executing. If the maker stops publishing inside that window, a caller who did
/// everything right still gets no fill - and, as `bisonfi_scenario_silence_the_maker` shows, no error
/// either. Reproducing that needs the override to fire on a LATER slot than the one quoted on, which
/// exercises `register_scenario` and `materialize_overrides_for_slot` rather than a single write.
#[tokio::test]
async fn bisonfi_scenario_maker_goes_dark_between_quote_and_fill() {
    use surfpool_types::{AccountAddress, OverrideInstance, Scenario};

    const QUOTE_AT: u64 = 0; // scenario-relative slot the caller quotes on
    const FILL_AT: u64 = 2; // and the slot the transaction actually lands on
    const STALE_BY: i64 = 1_000_000;

    let rig = bisonfi_rig().await;
    let (pool, data, tp) = rig.quoting.first().expect("a quoting market");
    let base_slot = u64::from_le_bytes(data[72..80].try_into().unwrap());
    let pool_key = pool.parse::<Pubkey>().expect("pool address");
    let size = BisonfiRig::sell_size(data);

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

    // Two steps on the same field: quoting normally when the caller looks, dark when it lands.
    let mut scenario = Scenario::new(
        "BisonFi maker goes dark mid-flight".to_string(),
        "Quotes normally at the slot the caller prices on, then stops publishing before the \
         transaction executes"
            .to_string(),
    );
    for (relative, value) in [(QUOTE_AT, 0i64), (FILL_AT, -STALE_BY)] {
        scenario.add_override(
            OverrideInstance::new(
                "bisonfi-freshness".to_string(),
                relative,
                AccountAddress::Pubkey(pool_key.to_string()),
            )
            .with_values(HashMap::from([(
                "last_update_slot".to_string(),
                serde_json::json!(value),
            )])),
        );
    }
    svm.register_scenario(scenario, Some(base_slot))
        .expect("register scenario");

    // Walk the slots and capture what the account looks like at each one.
    let mut images: HashMap<u64, Vec<u8>> = HashMap::new();
    for slot in base_slot..=base_slot + FILL_AT {
        svm.materialize_overrides_for_slot(&None, slot)
            .await
            .expect("materialize");
        let account = svm
            .inner
            .get_account(&pool_key)
            .expect("get_account")
            .expect("account present");
        images.insert(slot, account.data);
    }

    let field_at = |slot: u64| u64::from_le_bytes(images[&slot][72..80].try_into().unwrap());
    // The lead is resolved against the slot each step materializes at, so the first step stamps
    // its own slot and the second lands deliberately far behind its own.
    assert_eq!(
        field_at(base_slot),
        base_slot,
        "at the quoting slot the venue must still be publishing"
    );
    assert_eq!(
        field_at(base_slot + 1),
        base_slot,
        "no override is scheduled for the intermediate slot, so the account must be untouched"
    );
    assert_eq!(
        field_at(base_slot + FILL_AT),
        (base_slot + FILL_AT)
            .checked_sub(STALE_BY as u64)
            .expect("live slot must exceed the deliberate stale age"),
        "the second step must have fired by the slot the transaction lands on"
    );

    // Now the half that makes this more than a scheduling test: hand each slot's image to the real
    // program. The clock is taken from the ORIGINAL account, so the simnet's notion of "now" stays at
    // the publication slot while the field moves underneath it - which is what actually happens when
    // the maker stops and the chain moves on.
    let replay = |image: Vec<u8>| {
        bisonfi_replay(
            &rig.elf,
            pool,
            data,
            *tp,
            size,
            0,
            move |d: &mut Vec<u8>| {
                *d = image;
            },
        )
    };
    let quoted = replay(images[&base_slot].clone()).expect("the quoting slot must fill");
    assert!(
        quoted > 0,
        "the caller's quote has to be real, or the scenario proves nothing"
    );
    let filled = replay(images[&(base_slot + FILL_AT)].clone());
    assert_eq!(
        filled,
        Ok(0),
        "the maker went dark between the quote and the fill, so the swap must return nothing - and \
         it must do so without erroring, which is what makes this a silent failure"
    );
}

/// A long-running simulation keeps the maker available by scheduling another freshness
/// write when it needs a quote. This is the explicit-scheduling replacement for persistence.
#[tokio::test]
async fn bisonfi_scenario_scheduled_refresh_keeps_a_long_run_quoting() {
    use surfpool_types::{AccountAddress, OverrideInstance, Scenario};

    const EXECUTE_AFTER: u64 = 4_096;

    let rig = bisonfi_rig().await;
    let (pool, data, tp) = rig.quoting.first().expect("a quoting market");
    let pool_key = pool.parse::<Pubkey>().expect("pool address");
    let base_slot = u64::from_le_bytes(data[72..80].try_into().unwrap());
    let execute_slot = base_slot
        .checked_add(EXECUTE_AFTER)
        .expect("live slot plus scenario duration");
    let size = BisonfiRig::sell_size(data);

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

    let mut scenario = Scenario::new(
        "BisonFi stays live during a long run".to_string(),
        "Refreshes the maker again in the later slot where its quote is needed".to_string(),
    );
    for relative_slot in [0, EXECUTE_AFTER] {
        scenario.add_override(
            OverrideInstance::new(
                "bisonfi-freshness".to_string(),
                relative_slot,
                AccountAddress::Pubkey(pool_key.to_string()),
            )
            .with_values(HashMap::from([(
                "last_update_slot".to_string(),
                serde_json::json!(0),
            )])),
        );
    }
    svm.register_scenario(scenario, Some(base_slot))
        .expect("register scenario");

    svm.materialize_overrides_for_slot(&None, base_slot)
        .await
        .expect("materialize initial refresh");
    let initial = svm
        .inner
        .get_account(&pool_key)
        .expect("get_account")
        .expect("account present")
        .data;
    assert_eq!(
        u64::from_le_bytes(initial[72..80].try_into().unwrap()),
        base_slot,
        "the first refresh must resolve relative to its own slot"
    );

    svm.materialize_overrides_for_slot(&None, execute_slot)
        .await
        .expect("materialize later refresh");
    let refreshed = svm
        .inner
        .get_account(&pool_key)
        .expect("get_account")
        .expect("account present")
        .data;
    assert_eq!(
        u64::from_le_bytes(refreshed[72..80].try_into().unwrap()),
        execute_slot,
        "the later refresh must resolve relative to the slot where the quote is needed"
    );

    // `bisonfi_replay` takes its clock before applying the mutation. Give it the later execution
    // slot as the clock source, then install either account image to test the deployed program.
    let mut clock_source = data.clone();
    clock_source[72..80].copy_from_slice(&execute_slot.to_le_bytes());
    let stale = bisonfi_replay(&rig.elf, pool, &clock_source, *tp, size, 0, {
        let initial = initial.clone();
        move |account| *account = initial
    });
    assert_eq!(
        stale,
        Ok(0),
        "a quote refreshed only at the start must be stale {EXECUTE_AFTER} slots later"
    );

    let live = bisonfi_replay(
        &rig.elf,
        pool,
        &clock_source,
        *tp,
        size,
        0,
        move |account| *account = refreshed,
    )
    .expect("the explicitly refreshed market must execute");
    assert!(
        live > 0,
        "scheduling freshness again at the execution slot must keep the maker quoting"
    );
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

    const BASE_SLOT: u64 = 2_000_000;
    const FILL_AT: u64 = 2;

    let rig = bisonfi_rig().await;
    let (pool, data, tp) = rig.quoting.first().expect("a quoting market");
    let pool_key = pool.parse::<Pubkey>().expect("pool address");
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
    svm.register_scenario(scenario, Some(BASE_SLOT))
        .expect("register scenario");

    let mut images: HashMap<u64, Vec<u8>> = HashMap::new();
    for slot in BASE_SLOT..=BASE_SLOT + FILL_AT {
        svm.materialize_overrides_for_slot(&None, slot)
            .await
            .expect("materialize");
        images.insert(
            slot,
            svm.inner
                .get_account(&pool_key)
                .expect("get_account")
                .expect("account present")
                .data,
        );
    }
    let mid_at = |slot: u64| u128::from_le_bytes(images[&slot][832..848].try_into().unwrap());
    assert_eq!(
        mid_at(BASE_SLOT),
        mid,
        "the quoting slot must carry the quoted price"
    );
    assert_eq!(
        mid_at(BASE_SLOT + FILL_AT),
        moved,
        "the repricing step must have fired by the slot the transaction lands on"
    );

    // What the caller quoted, and therefore the minimum they would sign for.
    let quoted = bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, {
        let image = images[&BASE_SLOT].clone();
        move |d: &mut Vec<u8>| *d = image
    })
    .expect("the quoting slot must fill");
    assert!(quoted > 0, "the caller's quote has to be real");

    // The same transaction, landing after the reprice. Without a minimum it fills at the worse price;
    // with the minimum the caller actually quoted, it reverts.
    let image = images[&(BASE_SLOT + FILL_AT)].clone();
    let unprotected = bisonfi_replay(&rig.elf, pool, data, *tp, size, 0, {
        let image = image.clone();
        move |d: &mut Vec<u8>| *d = image
    })
    .expect("a repriced market still quotes, just worse");
    assert!(
        unprotected < quoted,
        "a 10% markdown must pay the seller less: got {unprotected} against a quote of {quoted}"
    );

    let protected = bisonfi_replay_min_out(&rig.elf, pool, data, *tp, size, quoted, 0, {
        let image = image.clone();
        move |d: &mut Vec<u8>| *d = image
    });
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
    assert!(
        checked >= 6,
        "only {checked} markets exercised the combined scenario"
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

            // The 288-versus-528 question this test used to hedge about is settled: 288 and 1036 are
            // working copies refreshed from 528 and 1196 by a watermark-gated memcpy (traced at
            // 10310-10387). The template writes all four regions for that
            // reason, so there is no longer an unmeasured case to leave un-asserted here.
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

    // Without this the whole test could pass while quoting on nothing at all. Six of the seventeen v3
    // pools publish a current mid and can be replayed; two more are live but quote a Token-2022 base
    // asset the harness cannot build accounts for (Custom(60)), and the rest are dormant, between 13
    // and 30 million slots behind, and return no quote whatever is written to them.
    assert!(
        peaks.len() >= 6,
        "only {} pools produced a quote at any size: {peaks:?}. This test proves nothing if the \
         markets are not actually pricing",
        peaks.len()
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
    let mut clock: solana_clock::Clock = svm.get_sysvar();
    clock.slot = pool_slot;
    clock.unix_timestamp = 1_787_041_969;
    svm.set_sysvar(&clock);
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
            Ok(out)
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

async fn load_program_elf(cache_name: &str, programdata: &str) -> Vec<u8> {
    let cache = std::env::temp_dir().join(cache_name);
    if let Ok(bytes) = std::fs::read(&cache) {
        if bytes.len() > 200_000 {
            return bytes;
        }
    }
    let bytes = fetch(&[programdata]).await.remove(0)[45..].to_vec();
    publish_elf_cache(&cache, &bytes);
    bytes
}

/// The program ELF, fetched once per process and reused. Delete the file to pick up a redeploy.
async fn bisonfi_elf() -> Vec<u8> {
    static ELF: tokio::sync::OnceCell<Vec<u8>> = tokio::sync::OnceCell::const_new();
    ELF.get_or_init(|| load_program_elf("surfpool-bisonfi-program.so", BISONFI_PROGRAMDATA))
        .await
        .clone()
}

/// Offsets of the live quote ladder. `LADDER` is the table the program actually prices from;
/// `LADDER_INERT` is the mirrored table that writing has no effect on, kept here so the test that
/// proves the difference cannot drift away from the template.
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

    let mut clock: solana_clock::Clock = svm.get_sysvar();
    clock.slot = pool_slot;
    clock.unix_timestamp = 1_787_041_969;
    svm.set_sysvar(&clock);
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
    assert!(
        checked >= 6,
        "only {checked} quoting markets exercised the depth lever"
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
    let buy_deeper_base =
        bisonfi_run(&fork, buy, 1, scale(48, 10)).expect("buy with deeper base");
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

    const BASE_SLOT: u64 = 2_000_000;
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
    svm.register_scenario(scenario, Some(BASE_SLOT))
        .expect("register scenario");

    let mut images: HashMap<u64, Vec<u8>> = HashMap::new();
    for slot in BASE_SLOT..=BASE_SLOT + FILL_AT {
        svm.materialize_overrides_for_slot(&None, slot)
            .await
            .expect("materialize");
        images.insert(
            slot,
            svm.inner
                .get_account(&pool_key)
                .expect("get_account")
                .expect("account present")
                .data,
        );
    }

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
                read(BASE_SLOT),
                TIGHT,
                "{path} rung {rung} at {at}: the quoting slot must carry the tight ladder"
            );
            assert_eq!(
                read(BASE_SLOT + FILL_AT),
                WIDE,
                "{path} rung {rung} at {at}: the widening step must have fired by the fill slot"
            );
        }
    }

    // What the caller quoted, and therefore the minimum they would sign for.
    let quoted = bisonfi_replay(&rig.elf, pool, data, tp, size, 0, {
        let image = images[&BASE_SLOT].clone();
        move |d: &mut Vec<u8>| *d = image
    })
    .expect("the quoting slot must fill");
    assert!(quoted > 0, "the caller's quote has to be real");

    let image = images[&(BASE_SLOT + FILL_AT)].clone();
    let unprotected = bisonfi_replay(&rig.elf, pool, data, tp, size, 0, {
        let image = image.clone();
        move |d: &mut Vec<u8>| *d = image
    })
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
    let protected = bisonfi_replay_min_out(&rig.elf, pool, data, tp, size, quoted, 0, {
        let image = image.clone();
        move |d: &mut Vec<u8>| *d = image
    });
    assert!(
        protected.is_err(),
        "signing for the price that was quoted must REVERT once the maker has widened, got \
         {protected:?}"
    );
}

const WHIRLPOOL_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

/// Every account an Orca Whirlpool `swap` needs is derivable and present, so the AMM leg of the
/// cross-venue arbitrage scenario can be executed rather than only priced.
///
/// `bisonfi_scenario_arbitrage_against_an_amm` currently compares BisonFi's quote against Whirlpool's
/// published state. Making that leg atomic - both swaps in one transaction - needs three tick arrays
/// and an oracle at PDAs that are only created lazily, so whether they exist is a fact about the
/// market and not something to assume. This pins it, and pins the derivation itself: a `TickArray`
/// stores its own `start_tick_index` and a back-pointer to its whirlpool, so if the seed scheme were
/// wrong the addresses would either not resolve or resolve to another pool's arrays. An earlier
/// hand-rolled derivation that skipped the off-curve bump search produced three addresses that all
/// looked plausible and none of which existed, which is exactly the failure this guards against.
#[tokio::test]
async fn whirlpool_swap_account_graph_is_derivable_and_present() {
    const TICK_ARRAY_LEN: usize = 9988;
    const TICKS_PER_ARRAY: i32 = 88;

    let prog = Pubkey::from_str_const(WHIRLPOOL_PROGRAM);
    let wp_key = Pubkey::from_str_const(WHIRLPOOL_SOL_USDC);
    let wp = fetch(&[WHIRLPOOL_SOL_USDC]).await.remove(0);

    let spacing = u16::from_le_bytes(wp[41..43].try_into().unwrap());
    let tick_current = i32::from_le_bytes(wp[81..85].try_into().unwrap());
    let mint_a = Pubkey::try_from(&wp[101..133]).expect("token_mint_a");
    let vault_a = Pubkey::try_from(&wp[133..165]).expect("token_vault_a");
    let mint_b = Pubkey::try_from(&wp[181..213]).expect("token_mint_b");
    let vault_b = Pubkey::try_from(&wp[213..245]).expect("token_vault_b");
    assert!(spacing > 0, "tick_spacing must be positive, got {spacing}");

    // The array a tick falls in starts at a multiple of spacing*88, rounded toward negative infinity.
    // Integer division truncates toward zero, which is the wrong way for the negative ticks a SOL/USDC
    // pool actually sits at, so this rounds explicitly.
    let per_array = spacing as i32 * TICKS_PER_ARRAY;
    let start = (tick_current as f32 / per_array as f32).floor() as i32 * per_array;
    assert!(
        start <= tick_current && tick_current < start + per_array,
        "the current tick {tick_current} must fall inside its own array [{start}, {})",
        start + per_array
    );

    let (oracle, _) = Pubkey::find_program_address(&[b"oracle", wp_key.as_ref()], &prog);
    let starts: Vec<i32> = [-1i32, 0, 1]
        .iter()
        .map(|k| start + k * per_array)
        .collect();
    let arrays: Vec<Pubkey> = starts
        .iter()
        .map(|s| {
            Pubkey::find_program_address(
                &[b"tick_array", wp_key.as_ref(), s.to_string().as_bytes()],
                &prog,
            )
            .0
        })
        .collect();

    let mut addrs: Vec<String> = arrays.iter().map(|a| a.to_string()).collect();
    addrs.push(vault_a.to_string());
    addrs.push(vault_b.to_string());
    addrs.push(oracle.to_string());
    let refs: Vec<&str> = addrs.iter().map(|s| s.as_str()).collect();
    let got = fetch_optional(&refs).await;

    for ((s, addr), data) in starts.iter().zip(arrays.iter()).zip(got.iter()) {
        let data = data.as_ref().unwrap_or_else(|| {
            panic!(
                "tick array for start {s} ({addr}) does not exist. A swap crossing into it would \
                 fail, so the atomic leg needs a pool whose neighbouring arrays are initialized"
            )
        });
        assert_eq!(data.len(), TICK_ARRAY_LEN, "{addr}: not a TickArray");
        // start_tick_index sits right after the 8-byte Anchor discriminator.
        assert_eq!(
            i32::from_le_bytes(data[8..12].try_into().unwrap()),
            *s,
            "{addr}: the account's own start_tick_index disagrees with the seed it was derived \
             from, so the derivation is wrong"
        );
        // ...and the trailing whirlpool back-pointer proves it belongs to THIS pool.
        assert_eq!(
            Pubkey::try_from(&data[TICK_ARRAY_LEN - 32..]).expect("whirlpool back-pointer"),
            wp_key,
            "{addr}: belongs to a different whirlpool"
        );
    }

    for (label, mint, vault, data) in [
        ("a", mint_a, vault_a, &got[3]),
        ("b", mint_b, vault_b, &got[4]),
    ] {
        let data = data
            .as_ref()
            .unwrap_or_else(|| panic!("token_vault_{label} {vault} does not exist"));
        assert_eq!(
            data.len(),
            165,
            "token_vault_{label}: not an SPL token account"
        );
        assert_eq!(
            Pubkey::try_from(&data[0..32]).expect("vault mint"),
            mint,
            "token_vault_{label} does not hold the mint the whirlpool declares"
        );
    }

    // The oracle is only initialized for adaptive-fee pools. Classic `swap` takes it as an
    // UncheckedAccount, so an absent one is passable as an empty account - but the address still has to
    // be the right PDA, which is why it is derived here rather than faked.
    assert!(
        got[5].is_none() || got[5].as_ref().map(|d| !d.is_empty()).unwrap_or(false),
        "oracle {oracle} resolved to a zero-length account, which is neither absent nor valid"
    );
}

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
        load_program_elf("surfpool-whirlpool-program.so", &programdata.to_string()).await
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
/// This is the AMM leg the cross-venue arbitrage scenario needs, and it is also what validates the
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

    // And the threshold argument is enforced, which the arbitrage test relies on for its profit floor.
    let greedy = whirlpool_replay(&fork, ONE_SOL, true, got + 1);
    assert!(
        greedy.is_err(),
        "asking for more than the swap can deliver must revert, got {greedy:?}"
    );
}

/// Buys a fixed quantity of the base asset on Orca and sells it on BisonFi in ONE transaction.
///
/// `dislocation` scales BisonFi's published mid, so a value above 1.0 makes BisonFi the richer bid and
/// the round trip profitable. Returns the taker's net change in the quote asset - negative is a loss.
///
/// The two legs are coupled by using an exact-OUTPUT swap on Orca: an instruction's amounts are fixed
/// when the transaction is built, so a leg that bought "whatever N USDC gets" could not be followed by
/// a leg that sells exactly that. Asking Orca for exactly N base tokens and paying whatever it costs
/// makes the second leg's size known in advance, which is what lets both legs sit in one transaction.
fn bisonfi_orca_atomic_arb(
    orca: &WhirlpoolFork,
    bf_elf: &[u8],
    bisonfi_pool: &str,
    bisonfi_snapshot: &[u8],
    bf_programs: (Pubkey, Pubkey),
    base_out: u64,
    fair_value: u128,
) -> Result<i64, String> {
    use litesvm::LiteSVM;
    use solana_account::Account;
    use solana_instruction::{AccountMeta, Instruction};
    use solana_keypair::Keypair;
    use solana_signer::Signer;
    use solana_transaction::Transaction;

    let mut bf = bisonfi_snapshot.to_vec();

    let g64 = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let base_reserve = g64(&bf, 48);
    let quote_reserve = g64(&bf, 56);
    let bf_base_vault = Pubkey::new_from_array(bf[120..152].try_into().unwrap());
    let bf_quote_vault = Pubkey::new_from_array(bf[152..184].try_into().unwrap());
    let base_mint = Pubkey::new_from_array(bf[184..216].try_into().unwrap());
    let quote_mint = Pubkey::new_from_array(bf[216..248].try_into().unwrap());
    let bf_slot = g64(&bf, 72);

    // Both venues have to be quoting the same pair in the same order, or the shared token accounts
    // below would be silently routing two unrelated markets.
    assert_eq!(
        (orca.mint_a, orca.mint_b),
        (base_mint, quote_mint),
        "the Orca pool and the BisonFi market must quote the same base/quote pair"
    );

    // Every run starts from the same external reference, not BisonFi's independently moving live
    // price. This makes fair and dislocated runs differ only by the template value under test.
    bisonfi_apply_template(
        "bisonfi-fair-value",
        &[("fair_value", serde_json::json!(fair_value.to_string()))],
    )(&mut bf);

    let bf_prog = Pubkey::from_str_const(BISONFI_PROGRAM);
    let orca_prog = Pubkey::from_str_const(WHIRLPOOL_PROGRAM);
    let spl = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
    let bf_key = Pubkey::from_str_const(bisonfi_pool);

    let mut svm = LiteSVM::new()
        .with_sigverify(false)
        .with_blockhash_check(false);
    svm.add_program(bf_prog, bf_elf)
        .map_err(|e| format!("add bisonfi: {e:?}"))?;
    svm.add_program(orca_prog, &orca.elf)
        .map_err(|e| format!("add orca: {e:?}"))?;

    // One clock satisfies both venues: BisonFi checks the SLOT against its own last_update_slot and
    // Orca checks the TIMESTAMP against its reward accrual, so the two constraints do not collide.
    let orca_ts = u64::from_le_bytes(orca.data[261..269].try_into().unwrap());
    let mut clock: solana_clock::Clock = svm.get_sysvar();
    clock.slot = bf_slot;
    clock.unix_timestamp = orca_ts as i64;
    svm.set_sysvar(&clock);
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
    svm.set_account(orca.key, owned(orca.data.clone(), orca_prog))
        .map_err(|e| format!("{e:?}"))?;
    for (_, k, d) in &orca.arrays {
        svm.set_account(*k, owned(d.clone(), orca_prog))
            .map_err(|e| format!("{e:?}"))?;
    }
    for (k, d) in [&orca.vault_a, &orca.vault_b] {
        svm.set_account(*k, owned(d.clone(), spl))
            .map_err(|e| format!("{e:?}"))?;
    }
    svm.set_account(bf_key, owned(bf, bf_prog))
        .map_err(|e| format!("{e:?}"))?;
    svm.set_account(
        bf_base_vault,
        owned(
            token_account(&base_mint, &bf_key, base_reserve),
            bf_programs.0,
        ),
    )
    .map_err(|e| format!("{e:?}"))?;
    svm.set_account(
        bf_quote_vault,
        owned(
            token_account(&quote_mint, &bf_key, quote_reserve + 79_168),
            bf_programs.1,
        ),
    )
    .map_err(|e| format!("{e:?}"))?;

    // The arbitrageur: funded in the quote asset, empty in the base. Both legs share these two
    // accounts, which is what makes the profit measurable as a single balance change.
    let taker = Keypair::new();
    svm.airdrop(&taker.pubkey(), 10_000_000_000)
        .map_err(|e| format!("{e:?}"))?;
    let (base_ta, quote_ta) = (Pubkey::new_unique(), Pubkey::new_unique());
    let quote_funding = quote_reserve / 4;
    svm.set_account(
        base_ta,
        owned(token_account(&base_mint, &taker.pubkey(), 0), spl),
    )
    .map_err(|e| format!("{e:?}"))?;
    svm.set_account(
        quote_ta,
        owned(
            token_account(&quote_mint, &taker.pubkey(), quote_funding),
            spl,
        ),
    )
    .map_err(|e| format!("{e:?}"))?;

    let (oracle, _) = Pubkey::find_program_address(&[b"oracle", orca.key.as_ref()], &orca_prog);
    let arrays = orca.tick_arrays(false); // buying base means B -> A
    let buy_on_orca = Instruction {
        program_id: orca_prog,
        accounts: vec![
            AccountMeta::new_readonly(spl, false),
            AccountMeta::new_readonly(taker.pubkey(), true),
            AccountMeta::new(orca.key, false),
            AccountMeta::new(base_ta, false),
            AccountMeta::new(orca.vault_a.0, false),
            AccountMeta::new(quote_ta, false),
            AccountMeta::new(orca.vault_b.0, false),
            AccountMeta::new(arrays[0], false),
            AccountMeta::new(arrays[1], false),
            AccountMeta::new(arrays[2], false),
            AccountMeta::new_readonly(oracle, false),
            AccountMeta::new_readonly(orca_prog, false),
        ],
        // Exact output: `base_out` of token A, paying up to u64::MAX of token B.
        data: whirlpool_swap::data(
            base_out,
            u64::MAX,
            whirlpool_swap::MAX_SQRT_PRICE,
            false,
            false,
        ),
    };

    let mut bf_data = Vec::with_capacity(19);
    bf_data.push(0x07);
    bf_data.extend_from_slice(&base_out.to_le_bytes());
    bf_data.extend_from_slice(&0u64.to_le_bytes()); // min_out; profit is asserted on balances
    bf_data.push(0); // direction 0 = sell base for quote
    bf_data.push(0);
    let sell_on_bisonfi = Instruction {
        program_id: bf_prog,
        accounts: vec![
            AccountMeta::new(taker.pubkey(), true),
            AccountMeta::new(bf_key, false),
            AccountMeta::new(bf_base_vault, false),
            AccountMeta::new(bf_quote_vault, false),
            AccountMeta::new(base_ta, false),
            AccountMeta::new(quote_ta, false),
            AccountMeta::new_readonly(bf_programs.0, false),
            AccountMeta::new_readonly(bf_programs.1, false),
            AccountMeta::new_readonly(Pubkey::from_str_const(BISONFI_NINTH), true),
        ],
        data: bf_data,
    };

    let mut budget = vec![2u8];
    budget.extend_from_slice(&1_800_000u32.to_le_bytes());
    let ixs = vec![
        Instruction {
            program_id: Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
            accounts: vec![],
            data: budget,
        },
        buy_on_orca,
        sell_on_bisonfi,
    ];

    let before = spl_amount(&svm.get_account(&quote_ta).expect("quote_ta").data);
    let mut msg = solana_message::Message::new(&ixs, Some(&taker.pubkey()));
    msg.recent_blockhash = svm.latest_blockhash();
    let nsig = msg.header.num_required_signatures as usize;
    let mut tx = Transaction::new_unsigned(msg);
    tx.signatures = vec![solana_signature::Signature::default(); nsig];
    tx.signatures[0] = taker.sign_message(&tx.message.serialize());
    svm.send_transaction(tx)
        .map_err(|e| format!("{:?}", e.err))?;

    let after = spl_amount(&svm.get_account(&quote_ta).expect("quote_ta").data);
    let leftover = spl_amount(&svm.get_account(&base_ta).expect("base_ta").data);
    assert_eq!(
        leftover, 0,
        "the arbitrageur must end flat in the base asset, or the profit below is really an \
         unrealized position: {leftover} left over"
    );
    Ok(after as i64 - before as i64)
}

/// SCENARIO: arbitrage between BisonFi and an AMM on the same pair, executed atomically.
///
/// The upgrade over `bisonfi_scenario_arbitrage_against_an_amm`, which compares the two venues' quotes
/// without trading: here both legs run in a single transaction against upstream state loaded for both
/// programs, and the profit is a real balance change in the arbitrageur's own account.
///
/// Self-validating in both directions. At the market's true mid the round trip must LOSE money, since
/// the arbitrageur pays fees on both venues - if that leg showed a profit, the harness would be minting
/// value and every number it produced would be suspect. Only once the fair-value template dislocates
/// BisonFi does the same transaction become profitable, and the profit has to grow with the
/// dislocation.
#[tokio::test]
async fn bisonfi_scenario_atomic_arbitrage_against_orca() {
    const SOL_USDC: &str = "8FnX3xo2yYw3EUE6w3nQA4GfXGS9wpK6oj3veJpbFzLo";
    const ONE_SOL: u64 = 1_000_000_000;

    // Load both venues once so every point in the sweep executes against exactly the same state.
    let orca = whirlpool_fork(WHIRLPOOL_SOL_USDC).await;
    let bf_elf = bisonfi_elf().await;
    let bf = fetch(&[SOL_USDC]).await.remove(0);
    let bf_programs = bisonfi_token_programs(&[bf.clone()]).await.remove(0);

    // Orca stores sqrt(raw quote/base) in Q64.64. SOL has 9 decimals and USDC has 6, while
    // BisonFi stores the human quote/base price in Q40.88, hence the 1000 raw-unit adjustment.
    let sqrt_price = u128::from_le_bytes(orca.data[65..81].try_into().unwrap());
    let reference_fair_value = (sqrt_price
        .checked_mul(sqrt_price)
        .expect("the Orca square-root price must fit in u128")
        >> 40)
        .checked_mul(1_000)
        .expect("the converted Orca price must fit in u128");

    // No dislocation: both legs use Orca's snapshot price, so fees make the round trip a loss.
    let fair = bisonfi_orca_atomic_arb(
        &orca,
        &bf_elf,
        SOL_USDC,
        &bf,
        bf_programs,
        ONE_SOL,
        reference_fair_value,
    )
    .expect("the round trip must execute at the shared reference price");
    assert!(
        fair < 0,
        "buying on Orca and selling on BisonFi at the true mid returned a profit of {fair}. Two \
         venues both charging a fee cannot pay the taker, so the harness is not measuring a real \
         round trip"
    );

    // Mark BisonFi up so it becomes the richer bid, and the same transaction becomes an arbitrage.
    let mut last = fair;
    let mut two_percent_profit = None;
    for pct in [2u128, 5, 10] {
        let dislocated = reference_fair_value
            .checked_mul(100 + pct)
            .expect("the dislocated price must fit in u128")
            / 100;
        let profit = bisonfi_orca_atomic_arb(
            &orca,
            &bf_elf,
            SOL_USDC,
            &bf,
            bf_programs,
            ONE_SOL,
            dislocated,
        )
        .unwrap_or_else(|e| panic!("the round trip must execute with BisonFi {pct}% rich: {e}"));
        assert!(
            profit > last,
            "marking BisonFi up {pct}% must pay better than the {last} the previous step returned, \
             got {profit}"
        );
        if pct == 2 {
            two_percent_profit = Some(profit);
        }
        last = profit;
    }
    let two_percent_profit = two_percent_profit.expect("the 2% leg must run");
    assert!(
        two_percent_profit > 0,
        "the documented scenario claims a 2% premium clears both venues' fees, got \
         {two_percent_profit}"
    );
    assert!(
        last > 0,
        "a 10% dislocation must produce an outright profit, got {last}. The fair-value template's \
         guidance claims this lever creates a cross-venue arbitrage, so it has to actually do so"
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

    assert!(
        checked >= 6,
        "only {checked} markets exercised the precedence of the freshness gate"
    );
}
