//! Kamino integration tests.
//!
//! These fetch the real accounts from mainnet rather than embedding captured copies, so they need
//! a network connection and are compiled only behind a feature:
//!
//! ```text
//! cargo test -p surfpool-core --features integration-tests kamino
//! ```
//!
//! Set `SURFPOOL_TEST_RPC_URL` to use a private endpoint if the public one rate-limits.
//!
//! What these cover that the unit tests cannot: a synthetic account is built *by* the bundled IDL,
//! so it can never disagree with it. Real accounts carry non-zero padding, live enum
//! discriminants and populated arrays, so an IDL that has drifted from the on-chain layout shows
//! up as a byte diff here and nowhere else.

use std::collections::HashMap;

use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;

use crate::{
    scenarios::TemplateRegistry,
    surfnet::{GetAccountResult, remote::SurfnetRemoteClient, svm::SurfnetSvm},
};

const RPC_URL_ENV: &str = "SURFPOOL_TEST_RPC_URL";
const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";

const RESERVE: &str = "14sqx2pLioXamoBFxE6CvHNth6uEAvJhXuJ2iwZMccAS";
const OBLIGATION: &str = "3iprSGrEQdBxhmqV399tYQQPG8Z1Hh2aYFrBwgqFXjGS";
const SCOPE_PRICES: &str = "3NJYftD5sjVfxSnUdZ1wVML8f3aC6mp1CXCL6L7TnU8C";
const FARM_STATE: &str = "18DizwAbBuuNGwfav3v6yWMbunnye4RnMLwLp67jAtj";
const SWAP_ORDER: &str = "14Buhfy7WBpiv2e6RMZNN5R7w3ua8MY1ZJ3WQyd29uJ";
const STRATEGY: &str = "1EXN5b1z7wucGb2uZoQmqjHdPoK1PNfUNWuwq8AqLTV";
const LENDING_MARKET: &str = "13iJ9S8qW8VGG94qUapfe3zbjvfig8PPgbDyfgHY6UHL";
const ORACLE_MAPPINGS: &str = "4zh6bmb77qX2CL7t5AJYCqa6YqFafbz3QJNeFvZjLowg";
const ORACLE_TWAPS: &str = "6L6vUts9tYqxHVUCEFVc2mzZw6yxMn8C6a44cp5ga7e9";
const FARMS_USER_STATE: &str = "1142jwhL6evoo2Ziqe6FJaj49USXA4JNXHcMH9bUFHz";
const FARMS_GLOBAL_CONFIG: &str = "3UQ2HX2VtY2tuVycTEintP3SSkbH5UkNes3QkG577iYz";
const SWAP_GLOBAL_CONFIG: &str = "3Lvo5giazx2Gyz9a2WWmDWj6eFeugKkcKSNK3qrPu46Y";
const VAULT_STATE: &str = "2BEYDYJFQWHkfVHrA4r9fPnfBm1nguqmgoMBfzrWnBDP";
const VAULT_WHITELIST_ENTRY: &str = "2GYjQAagrcmWDYZAjkeMZsDuT7jDyuiVqjxXuKvHEtcm";

/// Fetches the accounts in one request, so every account returned is from the same slot.
async fn fetch(addresses: &[&str]) -> Vec<Vec<u8>> {
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
            | GetAccountResult::FoundCoupledAccount((_, account), _, _) => account.data,
            GetAccountResult::None(_) => {
                panic!("{address} no longer exists on mainnet; the test needs a new address")
            }
        })
        .collect()
}

/// Byte indices at which two buffers differ.
fn diff_indices(a: &[u8], b: &[u8]) -> Vec<usize> {
    a.iter()
        .zip(b.iter())
        .enumerate()
        .filter(|(_, (x, y))| x != y)
        .map(|(i, _)| i)
        .collect()
}

/// A failure here means a bundled IDL disagrees with the live on-chain layout.
#[tokio::test]
async fn real_mainnet_accounts_round_trip_unchanged() {
    let cases: &[(&str, &str, &str)] = &[
        ("kamino-reserve-config", "Reserve", RESERVE),
        ("kamino-obligation-health", "Obligation", OBLIGATION),
        ("kamino-scope-price", "OraclePrices", SCOPE_PRICES),
        ("kamino-farms-reward-accumulator", "FarmState", FARM_STATE),
        ("kamino-swap-order", "Order", SWAP_ORDER),
        (
            "kamino-liquidity-strategy-balances",
            "WhirlpoolStrategy",
            STRATEGY,
        ),
    ];

    let addresses: Vec<&str> = cases.iter().map(|(_, _, a)| *a).collect();
    let accounts = fetch(&addresses).await;

    let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    let registry = TemplateRegistry::new();
    let pubkey = Pubkey::new_unique();

    for ((template_id, account_name, _), data) in cases.iter().zip(&accounts) {
        let template = registry
            .get(template_id)
            .unwrap_or_else(|| panic!("template {template_id} should exist"));

        let account_def = template
            .idl
            .accounts
            .iter()
            .find(|a| a.name == *account_name)
            .unwrap_or_else(|| panic!("{account_name} not in the IDL"));
        assert_eq!(
            &data[..8],
            account_def.discriminator.as_slice(),
            "{account_name} discriminator does not match the IDL - wrong account type?"
        );

        let forged = surfnet_svm
            .get_forged_account_data(&pubkey, data, &template.idl, &HashMap::new())
            .unwrap_or_else(|e| {
                panic!(
                    "live mainnet {account_name} failed to decode/re-encode with the bundled \
                     IDL: {e}"
                )
            });

        assert_eq!(
            forged.len(),
            data.len(),
            "{account_name} changed size on round-trip"
        );
        let diffs = diff_indices(&forged, data);
        assert!(
            diffs.is_empty(),
            "live mainnet {} was altered by a no-op round-trip at {} byte(s), first at {:?}",
            account_name,
            diffs.len(),
            diffs.first()
        );
    }
}

/// Catches collateral damage from the Borsh re-encode against real padding and live enum
/// discriminants, which a synthetic account cannot exercise.
#[tokio::test]
async fn override_on_real_account_touches_only_target_bytes() {
    let accounts = fetch(&[RESERVE, SCOPE_PRICES]).await;
    let (reserve_data, scope_data) = (&accounts[0], &accounts[1]);

    let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    let registry = TemplateRegistry::new();
    let pubkey = Pubkey::new_unique();

    // Reserve: one u8 at a known offset.
    const LIQ_THRESHOLD_PCT: usize = 4873;
    let reserve = registry.get("kamino-reserve-config").unwrap();
    let original_threshold = reserve_data[LIQ_THRESHOLD_PCT];
    assert!(
        original_threshold > 50,
        "the live reserve should start above the value we set, got {original_threshold}"
    );

    let forged = surfnet_svm
        .get_forged_account_data(
            &pubkey,
            reserve_data,
            &reserve.idl,
            &HashMap::from([(
                "config.liquidation_threshold_pct".to_string(),
                serde_json::json!(50u8),
            )]),
        )
        .expect("threshold override on live reserve");

    assert_eq!(
        diff_indices(&forged, reserve_data),
        vec![LIQ_THRESHOLD_PCT],
        "exactly one byte should change, and only the liquidation threshold"
    );
    assert_eq!(forged[LIQ_THRESHOLD_PCT], 50);

    // Scope: one u64 inside a 512-element array.
    const PRICES_BASE: usize = 8 + 32;
    const DATED_PRICE_SIZE: usize = 56;
    const IDX: usize = 0;
    let scope = registry.get("kamino-scope-price").unwrap();
    let value_off = PRICES_BASE + IDX * DATED_PRICE_SIZE;

    let original_value =
        u64::from_le_bytes(scope_data[value_off..value_off + 8].try_into().unwrap());
    assert!(
        original_value > 0,
        "live Scope index {IDX} should be populated, got {original_value}"
    );
    let new_value = original_value / 2;

    let forged = surfnet_svm
        .get_forged_account_data(
            &pubkey,
            scope_data,
            &scope.idl,
            &HashMap::from([(
                format!("prices.{IDX}.price.value"),
                serde_json::json!(new_value),
            )]),
        )
        .expect("price override on live Scope account");

    let diffs = diff_indices(&forged, scope_data);
    assert!(!diffs.is_empty(), "the price should have changed");
    assert!(
        diffs.iter().all(|i| (value_off..value_off + 8).contains(i)),
        "only the 8 bytes of prices[{IDX}].price.value should change, got {diffs:?}"
    );
    assert_eq!(
        u64::from_le_bytes(forged[value_off..value_off + 8].try_into().unwrap()),
        new_value
    );

    let next = PRICES_BASE + DATED_PRICE_SIZE;
    assert_eq!(
        &forged[next..next + DATED_PRICE_SIZE],
        &scope_data[next..next + DATED_PRICE_SIZE],
        "neighbouring Scope entry must not move"
    );
}

/// Evidence that a Reserve's cached price is derived from Scope, which is why
/// `kamino-scope-price` is the durable lever rather than the Reserve's own cache. Only checkable
/// against a genuine pair - constructing both sides would test our arithmetic against itself.
#[tokio::test]
async fn reserve_price_is_derived_from_scope() {
    // Reserve offsets incl. discriminator.
    const MARKET_PRICE_SF: usize = 248; // u128 scaled fraction (value << 60)
    const SCOPE_PRICE_FEED: usize = 5112;
    const SCOPE_PRICE_CHAIN: usize = 5144; // [u16; 4], 65535 = unused
    const PRICES_BASE: usize = 8 + 32;
    const DATED_PRICE_SIZE: usize = 56;
    const UNUSED_CHAIN_ENTRY: u16 = 65535;

    let accounts = fetch(&[RESERVE, SCOPE_PRICES]).await;
    let (reserve_data, scope_data) = (&accounts[0], &accounts[1]);

    let scope_account = Pubkey::from_str_const(SCOPE_PRICES);
    assert_eq!(
        &reserve_data[SCOPE_PRICE_FEED..SCOPE_PRICE_FEED + 32],
        scope_account.as_ref(),
        "the reserve must price through the Scope account this test fetches"
    );

    let chain: Vec<u16> = (0..4)
        .map(|i| {
            let off = SCOPE_PRICE_CHAIN + i * 2;
            u16::from_le_bytes(reserve_data[off..off + 2].try_into().unwrap())
        })
        .take_while(|entry| *entry != UNUSED_CHAIN_ENTRY)
        .collect();
    assert!(
        !chain.is_empty(),
        "the reserve should name at least one Scope index"
    );

    // A chained price is the product of its entries, each value / 10^exp.
    let mut scope_price = 1.0f64;
    for index in &chain {
        let base = PRICES_BASE + (*index as usize) * DATED_PRICE_SIZE;
        let value = u64::from_le_bytes(scope_data[base..base + 8].try_into().unwrap());
        let exp = u64::from_le_bytes(scope_data[base + 8..base + 16].try_into().unwrap());
        assert!(
            value > 0 && exp < 30,
            "Scope entry {index} looks unpopulated (value {value}, exp {exp})"
        );
        scope_price *= value as f64 / 10f64.powi(exp as i32);
    }

    let cached_sf = u128::from_le_bytes(
        reserve_data[MARKET_PRICE_SF..MARKET_PRICE_SF + 16]
            .try_into()
            .unwrap(),
    );
    let cached_price = cached_sf as f64 / 2f64.powi(60);
    assert!(cached_price > 0.0, "the reserve should have a cached price");

    // The cache is only rewritten when someone calls refresh_reserve, so it lags Scope by however
    // long it has been since the last refresh. The tolerance covers that lag; what is being tested
    // is the interpretation (value << 60, the chain being a product, the offsets), which a wrong
    // reading would miss by orders of magnitude rather than a few percent.
    let relative_error = (scope_price - cached_price).abs() / cached_price;
    assert!(
        relative_error < 0.05,
        "reserve cached price ${cached_price} should track the Scope chain {chain:?} product \
         ${scope_price} - if these have diverged, either the scaled-fraction interpretation \
         (value << 60), the price_chain semantics (a product), or an offset is wrong. \
         Relative error {relative_error}"
    );
}

/// A valid JSON value for a scalar IDL type, or `None` for composites. Mirrors the helper in
/// the registry unit tests; duplicated rather than widening that module's visibility.
fn sample_scalar_value(ty: &anchor_lang_idl::types::IdlType) -> Option<serde_json::Value> {
    use anchor_lang_idl::types::IdlType;
    match ty {
        IdlType::Bool => Some(serde_json::json!(true)),
        IdlType::U8 | IdlType::U16 | IdlType::U32 | IdlType::U64 | IdlType::U128 => {
            Some(serde_json::json!(7u64))
        }
        IdlType::I8 | IdlType::I16 | IdlType::I32 | IdlType::I64 | IdlType::I128 => {
            Some(serde_json::json!(7i64))
        }
        IdlType::Pubkey => Some(serde_json::json!(
            "So11111111111111111111111111111111111111112"
        )),
        _ => None,
    }
}

/// Every account type our templates target that has a live instance on mainnet. `WithdrawTicket`
/// is absent: the feature is new in klend 1.23.0 and none existed when this was written.
const LIVE_ACCOUNTS: &[(&str, &str, &str)] = &[
    ("kamino", "Reserve", RESERVE),
    ("kamino", "Obligation", OBLIGATION),
    ("kamino", "LendingMarket", LENDING_MARKET),
    ("kamino-scope", "OraclePrices", SCOPE_PRICES),
    ("kamino-scope", "OracleMappings", ORACLE_MAPPINGS),
    ("kamino-scope", "OracleTwaps", ORACLE_TWAPS),
    ("kamino-farms", "FarmState", FARM_STATE),
    ("kamino-farms", "UserState", FARMS_USER_STATE),
    ("kamino-farms", "GlobalConfig", FARMS_GLOBAL_CONFIG),
    ("kamino-swap", "Order", SWAP_ORDER),
    ("kamino-swap", "GlobalConfig", SWAP_GLOBAL_CONFIG),
    ("kamino-vault", "VaultState", VAULT_STATE),
    (
        "kamino-vault",
        "ReserveWhitelistEntry",
        VAULT_WHITELIST_ENTRY,
    ),
    ("kamino-liquidity", "WhirlpoolStrategy", STRATEGY),
];

/// Every template, exercised against a live instance of the account it targets: an identity
/// round-trip must not alter bytes, then writing every scalar it advertises must change some.
#[tokio::test]
async fn every_template_round_trips_over_a_live_account() {
    let addresses: Vec<&str> = LIVE_ACCOUNTS.iter().map(|(_, _, a)| *a).collect();
    let fetched = fetch(&addresses).await;

    let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    let registry = TemplateRegistry::new();
    let pubkey = Pubkey::new_unique();
    let mut checked = 0;

    for ((protocol, account_type, address), data) in LIVE_ACCOUNTS.iter().zip(&fetched) {
        for template in registry
            .by_protocol(protocol)
            .into_iter()
            .filter(|t| t.account_type == *account_type)
        {
            let identity = surfnet_svm
                .get_forged_account_data(&pubkey, data, &template.idl, &HashMap::new())
                .unwrap_or_else(|e| {
                    panic!(
                        "identity round-trip failed for {} ({address}): {e}",
                        template.id
                    )
                });
            // A live account may be allocated larger than the struct needs, so the re-encode is
            // a prefix rather than the whole buffer.
            assert!(
                identity.len() <= data.len(),
                "{} re-encoded larger than the live account",
                template.id
            );
            assert_eq!(
                identity,
                data[..identity.len()],
                "identity round-trip changed bytes for {} ({address})",
                template.id
            );

            let mut overrides: HashMap<String, serde_json::Value> = HashMap::new();
            for property in &template.properties {
                let ty = surfpool_types::resolve_idl_type(
                    &template.idl,
                    &template.account_type,
                    &property.path,
                )
                .unwrap_or_else(|e| panic!("[{}] {}: {e}", template.id, property.path));
                if let Some(value) = sample_scalar_value(ty) {
                    overrides.insert(property.path.clone(), value);
                }
            }
            if overrides.is_empty() {
                continue; // composite-only template; its llm_context documents the full shape
            }

            let forged = surfnet_svm
                .get_forged_account_data(&pubkey, data, &template.idl, &overrides)
                .unwrap_or_else(|e| {
                    panic!(
                        "forge failed for {} with {} scalar override(s): {e}",
                        template.id,
                        overrides.len()
                    )
                });
            assert_eq!(
                forged.len(),
                identity.len(),
                "forged size changed for {}",
                template.id
            );
            assert_ne!(
                forged, identity,
                "overrides for {} did not change any bytes",
                template.id
            );
            checked += 1;
        }
    }

    assert!(
        checked >= 25,
        "expected to exercise at least 25 Kamino templates against live accounts, got {checked}"
    );
}

/// The default pubkey "1111...1111" is all hex characters, which the encoder used to misread as
/// hex bytes and panic on.
#[tokio::test]
async fn obligation_array_index_and_pubkey_overrides() {
    // Obligation offsets incl. discriminator: header is 88 bytes, then 136 per deposit.
    const DEPOSIT_0_RESERVE: usize = 8 + 88;
    const DEPOSIT_0_AMOUNT: usize = DEPOSIT_0_RESERVE + 32;
    const DEPOSIT_1_RESERVE: usize = 8 + 88 + 136;

    let data = fetch(&[OBLIGATION]).await.remove(0);
    let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    let registry = TemplateRegistry::new();
    let template = registry
        .get("kamino-obligation-positions")
        .expect("kamino-obligation-positions template should exist");

    let wsol = "So11111111111111111111111111111111111111112";
    let overrides: HashMap<String, serde_json::Value> = HashMap::from([
        (
            "deposits.0.deposit_reserve".to_string(),
            serde_json::json!("11111111111111111111111111111111"),
        ),
        (
            "deposits.0.deposited_amount".to_string(),
            serde_json::json!(4_200_000_000u64),
        ),
        (
            "deposits.1.deposit_reserve".to_string(),
            serde_json::json!(wsol),
        ),
        ("has_debt".to_string(), serde_json::json!(1)),
    ]);

    let forged = surfnet_svm
        .get_forged_account_data(&Pubkey::new_unique(), &data, &template.idl, &overrides)
        .expect("array-index and pubkey overrides should apply");

    assert_eq!(forged.len(), data.len(), "account size must be preserved");
    assert_eq!(
        &forged[DEPOSIT_0_RESERVE..DEPOSIT_0_RESERVE + 32],
        Pubkey::default().as_ref(),
        "deposits[0].deposit_reserve should be the default pubkey"
    );
    assert_eq!(
        u64::from_le_bytes(
            forged[DEPOSIT_0_AMOUNT..DEPOSIT_0_AMOUNT + 8]
                .try_into()
                .unwrap()
        ),
        4_200_000_000u64,
        "deposits[0].deposited_amount should be written at its array index"
    );
    assert_eq!(
        &forged[DEPOSIT_1_RESERVE..DEPOSIT_1_RESERVE + 32],
        Pubkey::from_str_const(wsol).as_ref(),
        "deposits[1].deposit_reserve should be the wSOL mint"
    );
}

#[tokio::test]
async fn scope_price_override_writes_expected_bytes() {
    // OraclePrices: discriminator + oracle_mappings pubkey, then 56 bytes per entry.
    const PRICES_BASE: usize = 8 + 32;
    const DATED_PRICE_SIZE: usize = 56;
    const SOL_INDEX: usize = 0;
    // $125.50 with exp = 8
    const SOL_VALUE: u64 = 12_550_000_000;
    const SOL_EXP: u64 = 8;
    const AT_SLOT: u64 = 370_000_000;
    const AT_TS: u64 = 1_800_000_000;

    let data = fetch(&[SCOPE_PRICES]).await.remove(0);
    let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    let registry = TemplateRegistry::new();
    let template = registry
        .get("kamino-scope-price")
        .expect("kamino-scope-price template should exist");

    let overrides: HashMap<String, serde_json::Value> = HashMap::from([
        (
            format!("prices.{SOL_INDEX}.price.value"),
            serde_json::json!(SOL_VALUE),
        ),
        (
            format!("prices.{SOL_INDEX}.price.exp"),
            serde_json::json!(SOL_EXP),
        ),
        (
            format!("prices.{SOL_INDEX}.last_updated_slot"),
            serde_json::json!(AT_SLOT),
        ),
        (
            format!("prices.{SOL_INDEX}.unix_timestamp"),
            serde_json::json!(AT_TS),
        ),
    ]);

    let forged = surfnet_svm
        .get_forged_account_data(&Pubkey::new_unique(), &data, &template.idl, &overrides)
        .expect("scope price override should apply");

    assert_eq!(forged.len(), data.len(), "account size must be preserved");

    let base = PRICES_BASE + SOL_INDEX * DATED_PRICE_SIZE;
    let read = |off: usize| u64::from_le_bytes(forged[off..off + 8].try_into().unwrap());
    assert_eq!(read(base), SOL_VALUE, "price.value");
    assert_eq!(read(base + 8), SOL_EXP, "price.exp");
    assert_eq!(read(base + 16), AT_SLOT, "last_updated_slot");
    assert_eq!(read(base + 24), AT_TS, "unix_timestamp");

    // price = value / 10^exp
    assert_eq!(SOL_VALUE as f64 / 10f64.powi(SOL_EXP as i32), 125.50);

    // The neighbouring entry is populated on a live account, so require it unchanged rather
    // than zero.
    let next = PRICES_BASE + (SOL_INDEX + 1) * DATED_PRICE_SIZE;
    assert_eq!(
        &forged[next..next + DATED_PRICE_SIZE],
        &data[next..next + DATED_PRICE_SIZE],
        "writing one price index must not disturb the next entry"
    );
}

/// A reward accrues from the gap between the farm accumulator and the user's tally, so both
/// halves must be writable.
#[tokio::test]
async fn farms_reward_override_writes_both_halves() {
    let fetched = fetch(&[FARM_STATE, FARMS_USER_STATE]).await;
    let (farm_data, user_data) = (&fetched[0], &fetched[1]);

    let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    let registry = TemplateRegistry::new();
    let pubkey = Pubkey::new_unique();

    let farm = registry
        .get("kamino-farms-reward-accumulator")
        .expect("kamino-farms-reward-accumulator template");
    let farm_overrides: HashMap<String, serde_json::Value> = HashMap::from([
        (
            "reward_infos.0.reward_per_share_scaled".to_string(),
            serde_json::json!(5_000_000u64),
        ),
        (
            "total_active_stake_scaled".to_string(),
            serde_json::json!(1_000_000u64),
        ),
    ]);
    let forged_farm = surfnet_svm
        .get_forged_account_data(&pubkey, farm_data, &farm.idl, &farm_overrides)
        .expect("farm accumulator override should apply");
    assert_eq!(forged_farm.len(), farm_data.len());
    assert_ne!(&forged_farm, farm_data);

    // UserState offsets incl. discriminator: 80-byte header, then the [u128; 10] tally.
    const TALLY_0: usize = 88;
    const UNCLAIMED_0: usize = TALLY_0 + 160;

    let user = registry
        .get("kamino-farms-user-rewards")
        .expect("kamino-farms-user-rewards template");
    let user_overrides: HashMap<String, serde_json::Value> = HashMap::from([
        (
            "rewards_issued_unclaimed.0".to_string(),
            serde_json::json!(777_000u64),
        ),
        (
            "rewards_tally_scaled.0".to_string(),
            serde_json::json!(0u64),
        ),
        (
            "active_stake_scaled".to_string(),
            serde_json::json!(1_000u64),
        ),
    ]);
    let forged_user = surfnet_svm
        .get_forged_account_data(&pubkey, user_data, &user.idl, &user_overrides)
        .expect("user reward override should apply");

    assert_eq!(forged_user.len(), user_data.len());
    assert_eq!(
        u64::from_le_bytes(
            forged_user[UNCLAIMED_0..UNCLAIMED_0 + 8]
                .try_into()
                .unwrap()
        ),
        777_000u64,
        "rewards_issued_unclaimed[0] should be written at its array index"
    );
}

/// The two overrides that survive `refresh_obligation`: crash the Scope price, then tighten the
/// deposit reserve's liquidation threshold.
#[tokio::test]
async fn liquidation_setup_writes_durable_inputs() {
    const LTV_PCT: usize = 4872;
    const LIQ_THRESHOLD_PCT: usize = 4873;
    const SCOPE_PRICES_BASE: usize = 8 + 32;
    const DATED_PRICE_SIZE: usize = 56;

    let fetched = fetch(&[SCOPE_PRICES, RESERVE]).await;
    let (scope_data, reserve_data) = (&fetched[0], &fetched[1]);

    let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    let registry = TemplateRegistry::new();
    let pubkey = Pubkey::new_unique();

    // Crash the Scope price the reserve prices from.
    const IDX: usize = 45;
    const CRASHED: u64 = 15_000_000;
    let scope = registry.get("kamino-scope-price").expect("scope template");
    let scope_overrides: HashMap<String, serde_json::Value> = HashMap::from([
        (
            format!("prices.{IDX}.price.value"),
            serde_json::json!(CRASHED),
        ),
        (format!("prices.{IDX}.price.exp"), serde_json::json!(8u64)),
    ]);
    let forged_scope = surfnet_svm
        .get_forged_account_data(&pubkey, scope_data, &scope.idl, &scope_overrides)
        .expect("scope crash should apply");

    let off = SCOPE_PRICES_BASE + IDX * DATED_PRICE_SIZE;
    assert_eq!(
        u64::from_le_bytes(forged_scope[off..off + 8].try_into().unwrap()),
        CRASHED,
        "crashed price must land at the Scope entry the reserve names"
    );
    assert_eq!(
        CRASHED as f64 / 10f64.powi(8),
        0.15,
        "value/exp must decode to $0.15"
    );

    // Tighten the live reserve's liquidation threshold, leaving its loan-to-value alone.
    let reserve = registry
        .get("kamino-reserve-config")
        .expect("reserve config template");
    let live_ltv = reserve_data[LTV_PCT];
    let reserve_overrides: HashMap<String, serde_json::Value> = HashMap::from([
        (
            "config.liquidation_threshold_pct".to_string(),
            serde_json::json!(50u8),
        ),
        (
            "config.max_liquidation_bonus_bps".to_string(),
            serde_json::json!(1000u16),
        ),
    ]);
    let forged_reserve = surfnet_svm
        .get_forged_account_data(&pubkey, reserve_data, &reserve.idl, &reserve_overrides)
        .expect("reserve config override should apply");

    assert_eq!(
        forged_reserve[LIQ_THRESHOLD_PCT], 50,
        "liquidation threshold must be lowered"
    );
    assert_eq!(
        forged_reserve[LTV_PCT], live_ltv,
        "loan-to-value must be left untouched, so a position above the new 50% liquidation \
         threshold becomes liquidatable"
    );
    assert_eq!(
        forged_reserve.len(),
        reserve_data.len(),
        "reserve size must be preserved"
    );
}

/// A ticket becomes redeemable once the reserve's queue cursor reaches its sequence number. The
/// ticket half is synthetic because no `WithdrawTicket` exists on mainnet yet; the reserve half
/// uses a live account.
#[tokio::test]
async fn withdraw_ticket_and_queue_cursor() {
    let reserve_data = fetch(&[RESERVE]).await.remove(0);

    let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    let registry = TemplateRegistry::new();
    let pubkey = Pubkey::new_unique();

    let ticket = registry
        .get("kamino-withdraw-ticket")
        .expect("withdraw ticket template");
    let ticket_disc = &ticket
        .idl
        .accounts
        .iter()
        .find(|a| a.name == "WithdrawTicket")
        .expect("WithdrawTicket")
        .discriminator;
    let mut ticket_data = vec![0u8; 520];
    ticket_data[..8].copy_from_slice(ticket_disc);

    let ticket_overrides: HashMap<String, serde_json::Value> = HashMap::from([
        ("sequence_number".to_string(), serde_json::json!(7u64)),
        (
            "queued_collateral_amount".to_string(),
            serde_json::json!(500u64),
        ),
        ("invalid".to_string(), serde_json::json!(0u8)),
    ]);
    let forged_ticket = surfnet_svm
        .get_forged_account_data(&pubkey, &ticket_data, &ticket.idl, &ticket_overrides)
        .expect("withdraw ticket override should apply");
    assert_eq!(
        u64::from_le_bytes(forged_ticket[8..16].try_into().unwrap()),
        7,
        "ticket sequence number"
    );

    // Advance the live reserve's cursor to 7, making ticket 7 serveable.
    let limits = registry
        .get("kamino-reserve-limits")
        .expect("reserve limits template");
    let queue_overrides: HashMap<String, serde_json::Value> = HashMap::from([
        (
            "withdraw_queue.queued_collateral_amount".to_string(),
            serde_json::json!(500u64),
        ),
        (
            "withdraw_queue.next_withdrawable_ticket_sequence_number".to_string(),
            serde_json::json!(7u64),
        ),
        (
            "withdraw_queue.next_issued_ticket_sequence_number".to_string(),
            serde_json::json!(8u64),
        ),
        (
            "liquidity.total_available_amount".to_string(),
            serde_json::json!(0u64),
        ),
    ]);
    let forged_reserve = surfnet_svm
        .get_forged_account_data(&pubkey, &reserve_data, &limits.idl, &queue_overrides)
        .expect("withdraw queue override should apply");

    assert_eq!(forged_reserve.len(), reserve_data.len());
    assert_ne!(forged_reserve, reserve_data);
}
