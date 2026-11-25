//! Pump / PumpSwap integration tests.
//!
//! These fetch the real accounts from mainnet rather than embedding captured copies, so they
//! need a network connection and are compiled only behind a feature:
//!
//! ```text
//! cargo test -p surfpool-core --features integration-tests pump
//! ```
//!
//! Set `SURFPOOL_TEST_RPC_URL` to use a private endpoint if the public one rate-limits.
//!
//! What these cover that unit tests cannot: real accounts carry live values, populated
//! creator fields and the `extend_account` tail past the Borsh layout, so a pump program
//! upgrade that changes the on-chain layout shows up as a byte diff here and nowhere else.
//!
//! The lifecycle tests start a surfnet that forks mainnet and create their own Token-2022 coin
//! on it with pump's `create_v2`, one ordinary and one in mayhem mode, so they do not depend on
//! which coins trade on mainnet. The pump and pAMM programs run live, so a behavior-changing
//! upgrade fails here first.

use std::collections::HashMap;

use crossbeam_channel::unbounded;
use solana_account::Account;
use solana_account_decoder::UiAccountEncoding;
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_config::{RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig},
    rpc_request::RpcRequest,
};
use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;
use surfpool_types::{
    AccountAddress, OverrideInstance, RpcConfig, Scenario, SimnetConfig, SurfpoolConfig,
};

use crate::{
    error::SurfpoolResult,
    scenarios::{
        TemplateRegistry,
        protocols::pump::v1::graduation_builder::{
            PumpGraduationPreparation, build_pump_graduation_scenario, pump_graduation_addresses,
        },
    },
    storage::tests::TestType,
    surfnet::{
        GetAccountResult, locker::SurfnetSvmLocker, remote::SurfnetRemoteClient, svm::SurfnetSvm,
    },
    tests::{
        helpers::{diff_indices, get_free_port},
        integration::{RunloopGuard, spawn_runloop, wait_for_ready_and_connected},
    },
};

const RPC_URL_ENV: &str = "SURFPOOL_TEST_RPC_URL";
const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";

const PUMP: Pubkey = Pubkey::from_str_const("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
const PAMM: Pubkey = Pubkey::from_str_const("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
const FEE_PROGRAM: Pubkey = Pubkey::from_str_const("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
const MAYHEM_PROGRAM: Pubkey =
    Pubkey::from_str_const("MAyhSmzXzV1pTf7LsNkrNwkWKTo4ougAJ1PPg47MD4e");
const TOKEN_2022: Pubkey = Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const TOKENKEG: Pubkey = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const WSOL: Pubkey = Pubkey::from_str_const("So11111111111111111111111111111111111111112");
const ATA_PROGRAM: Pubkey = Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const SYSTEM_PROGRAM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
const RENT_SYSVAR: Pubkey = Pubkey::from_str_const("SysvarRent111111111111111111111111111111111");
const USDC_MINT: Pubkey = Pubkey::from_str_const("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

const MINT: Pubkey = Pubkey::from_str_const("HRTzNRJNnY78xe8e4a9DuMotw6qA97GwSQLzpVw9pump");
const CURVE: Pubkey = Pubkey::from_str_const("GBpTHrtF8dGwxC7thRD7T6VfGtbVYEabKkQ7k6g3u7QF");
const BASE_VAULT: Pubkey = Pubkey::from_str_const("9sXf9hAtryY1mncMxKGZnLMJzQbnTsUoSu8GJTX3FpFh");
const PUMP_GLOBAL: Pubkey = Pubkey::from_str_const("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf");
const AMM_GLOBAL_CONFIG: Pubkey =
    Pubkey::from_str_const("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw");

/// The bonding curve of the migrated coin 7LSsEoJG…pump, documented in pump-public-docs and
/// checked against mainnet on 2026-08-06 (complete = true, so it never changes again). It is
/// 150 bytes, shorter than the bundled 166-byte `BondingCurve` type.
const LLS_CURVE: Pubkey = Pubkey::from_str_const("3MUkKMbuornHohtAtzrToSzqkj1gEEhQqYVz8sZnmQg1");
/// The canonical PumpSwap pool of the same migrated coin.
const AMM_POOL: Pubkey = Pubkey::from_str_const("GseMAnNDvntR5uFePZ51yZBXzNSn7GdFPkfHwfr6d77J");
/// The 150-byte bonding curve of the migrated Fartcoin, 9BB6NFEc…pump.
const FARTCOIN_CURVE: Pubkey =
    Pubkey::from_str_const("TBHe5tJnuT4CQbHorJ1uVdfUoaYGPKgfCpiv2jgesVN");
/// A 256-byte bonding curve, 90 bytes longer than the bundled `BondingCurve` type.
const LONG_CURVE: Pubkey = Pubkey::from_str_const("JEKFMnxdTCH88x9HR6xPJb2jQxSaz5J5izwdWqrdW7m1");
/// A 245-byte pool, idle since 2026-03-06.
const POOL_245: Pubkey = Pubkey::from_str_const("12Tmc1bcYnY3dJc8Gh8awuH77FLQvMema2Qmy8aF1nK");
/// A 271-byte pool, idle since 2026-09-16.
const POOL_271: Pubkey = Pubkey::from_str_const("1CnDGYt2LYJUjQQM1yHAz1wF7YKMKQVqyXMfpgeJHQQ");

const BUY_V2_DISCRIMINATOR: [u8; 8] = [184, 23, 238, 97, 103, 197, 211, 61];
const MIGRATE_V2_DISCRIMINATOR: [u8; 8] = [187, 203, 18, 31, 206, 237, 254, 41];
const SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

const CREATE_V2_DISCRIMINATOR: [u8; 8] = [214, 144, 76, 236, 95, 139, 49, 180];
const MAX_SOL_COST: u64 = 1_000_000_000;

const CURVE_VIRTUAL_TOKEN_RESERVES_OFFSET: usize = 8;
const CURVE_VIRTUAL_QUOTE_RESERVES_OFFSET: usize = 16;
const CURVE_REAL_TOKEN_RESERVES_OFFSET: usize = 24;
const CURVE_REAL_QUOTE_RESERVES_OFFSET: usize = 32;
const CURVE_COMPLETE_OFFSET: usize = 48;
const CURVE_CREATOR_OFFSET: usize = 49;
const CURVE_IS_MAYHEM_MODE_OFFSET: usize = 81;
const CURVE_QUOTE_MINT_OFFSET: usize = 83;
const GLOBAL_FEE_RECIPIENT_OFFSET: usize = 41;
const GLOBAL_WITHDRAW_AUTHORITY_OFFSET: usize = 113;
const GLOBAL_RESERVED_FEE_RECIPIENT_OFFSET: usize = 483;
const GLOBAL_BUYBACK_RECIPIENTS_OFFSET: usize = 741;
const TOKEN_AMOUNT_OFFSET: usize = 64;
const AMM_LP_FEE_BASIS_POINTS_OFFSET: usize = 40;
const AMM_DISABLE_FLAGS_OFFSET: usize = 56;
const AMM_PROTOCOL_FEE_RECIPIENTS_OFFSET: usize = 57;
const AMM_GLOBAL_CONFIG_IDL_LEN: usize = 949;
const CURVE_IDL_LEN: usize = 166;
const POOL_QUOTE_TOKEN_ACCOUNT_OFFSET: usize = 171;
const POOL_LP_SUPPLY_OFFSET: usize = 203;
const POOL_COIN_CREATOR_OFFSET: usize = 211;
const POOL_IDL_LEN: usize = 287;
const POOL_VIRTUAL_QUOTE_RESERVES: std::ops::Range<usize> = 245..261;
const LLS_MINT: Pubkey = Pubkey::from_str_const("7LSsEoJGhLeZzGvDofTdNg7M3JttxQqGWNLo6vWMpump");

/// Fetches the accounts in one request, so every account returned is from the same slot.
async fn fetch(addresses: &[Pubkey]) -> Vec<Account> {
    let client = SurfnetRemoteClient::new(
        std::env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_RPC_URL.to_string()),
    );

    client
        .get_multiple_accounts(addresses, CommitmentConfig::confirmed())
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

/// A failure here means a bundled IDL disagrees with the live on-chain layout.
#[tokio::test]
async fn real_mainnet_accounts_round_trip_unchanged() {
    let cases: &[(&str, &str, Pubkey)] = &[
        ("pump-bonding-curve-custom", "BondingCurve", LONG_CURVE),
        ("pump-bonding-curve-custom", "BondingCurve", CURVE),
        ("pump-global", "Global", PUMP_GLOBAL),
        ("pump-amm-canonical-pool", "Pool", AMM_POOL),
        ("pump-amm-global-config", "GlobalConfig", AMM_GLOBAL_CONFIG),
    ];

    let addresses: Vec<Pubkey> = cases.iter().map(|(_, _, a)| *a).collect();
    let accounts = fetch(&addresses).await;

    let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    let registry = TemplateRegistry::new();
    let pubkey = Pubkey::new_unique();

    for ((template_id, account_name, address), account) in cases.iter().zip(&accounts) {
        let template = registry
            .get(template_id)
            .unwrap_or_else(|| panic!("template {template_id} should exist"));
        let idl = template
            .idl
            .as_ref()
            .unwrap_or_else(|| panic!("Pump template {template_id} must carry an IDL"));

        let account_def = idl
            .accounts
            .iter()
            .find(|a| a.name == *account_name)
            .unwrap_or_else(|| panic!("{account_name} not in the IDL"));
        assert_eq!(
            &account.data[..8],
            account_def.discriminator.as_slice(),
            "{address}: discriminator does not match the IDL - wrong account type?"
        );

        let forged = surfnet_svm
            .get_forged_account_data(&pubkey, &account.data, idl, &HashMap::new())
            .unwrap_or_else(|e| {
                panic!(
                    "live mainnet {account_name} {address} failed to decode/re-encode with the \
                     bundled IDL: {e}"
                )
            });

        assert_eq!(
            forged.len(),
            account.data.len(),
            "{account_name} {address} changed size on round-trip"
        );
        let diffs = diff_indices(&forged, &account.data);
        assert!(
            diffs.is_empty(),
            "live mainnet {account_name} {address} was altered by a no-op round-trip at {} \
             byte(s), first at {:?}",
            diffs.len(),
            diffs.first()
        );
    }
}

/// The bundled IDLs come from pump-public-docs, and their `BondingCurve` (166 bytes) and `Pool`
/// (287 bytes) types are longer than many live accounts. An IDL write decodes the whole type, so
/// on a shorter account the override is skipped with a warning and the account keeps every
/// byte; the same override on a long enough account is written.
#[tokio::test(flavor = "multi_thread")]
async fn live_accounts_shorter_than_their_idl_type_are_left_unchanged() {
    let cases: &[(Pubkey, &str, usize, bool)] = &[
        (LLS_CURVE, "pump-bonding-curve-custom", 150, false),
        (FARTCOIN_CURVE, "pump-bonding-curve-custom", 150, false),
        (CURVE, "pump-bonding-curve-custom", CURVE_IDL_LEN, true),
        (LONG_CURVE, "pump-bonding-curve-custom", 256, true),
        (POOL_245, "pump-amm-pool-state", 245, false),
        (POOL_271, "pump-amm-pool-state", 271, false),
        (AMM_POOL, "pump-amm-pool-state", 300, true),
    ];
    let accounts = fetch(&cases.iter().map(|case| case.0).collect::<Vec<_>>()).await;
    let registry = TemplateRegistry::new();

    for (&(address, template_id, len, writable), live) in cases.iter().zip(accounts) {
        assert_eq!(live.data.len(), len, "{address} changed size");
        let template = registry.get(template_id).expect("template");
        let idl = template.idl.as_ref().expect("IDL");
        let (svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
        let decoded = svm.get_forged_account_data(&address, &live.data, idl, &HashMap::new());
        assert_eq!(
            decoded.is_ok(),
            writable,
            "{address} ({len} bytes): {decoded:?}"
        );

        let virtual_quote = 7_000_000_007u64;
        let instance = OverrideInstance::new(
            template.id.clone(),
            0,
            AccountAddress::Pubkey(address.to_string()),
        )
        .with_values(HashMap::from([(
            "virtual_quote_reserves".to_string(),
            serde_json::json!(virtual_quote),
        )]));
        let (mut svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
        svm.set_account(&address, live.clone()).unwrap();
        play(&mut svm, vec![instance], false)
            .await
            .unwrap_or_else(|e| panic!("{address} must not fail the scenario: {e}"));

        let after = svm.get_account(&address).unwrap().expect("account");
        if writable {
            assert_eq!(after.data.len(), len);
            let written = if template_id == "pump-amm-pool-state" {
                read_virtual_quote_reserves(&after.data)
            } else {
                i128::from(read_u64(&after.data, CURVE_VIRTUAL_QUOTE_RESERVES_OFFSET))
            };
            assert_eq!(
                written,
                i128::from(virtual_quote),
                "{address} ({len} bytes) must be written"
            );
        } else {
            assert_eq!(
                after, live,
                "{address} ({len} bytes) must be left unchanged"
            );
        }
    }
}

/// Catches collateral damage from the Borsh re-encode against real bytes: only the overridden
/// fields may change, and the `extend_account` tail past the Borsh layout must survive.
#[tokio::test]
async fn override_on_real_account_touches_only_target_bytes() {
    let accounts = fetch(&[LONG_CURVE, AMM_POOL, AMM_GLOBAL_CONFIG]).await;
    let (curve_data, pool_data, config_data) =
        (&accounts[0].data, &accounts[1].data, &accounts[2].data);

    let (surfnet_svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    let registry = TemplateRegistry::new();
    let pubkey = Pubkey::new_unique();

    // A semantically valid just-completed curve: complete=true requires
    // real_token_reserves = 0, and the curve-lifetime invariants hold
    // (virtual - real stays at 279.9T tokens / 30 SOL of quote).
    let curve = registry.get("pump-bonding-curve-custom").expect("template");
    let overrides = HashMap::from([
        (
            "virtual_token_reserves".to_string(),
            serde_json::json!(279_900_000_000_000u64),
        ),
        (
            "virtual_quote_reserves".to_string(),
            serde_json::json!(115_000_000_000u64),
        ),
        ("real_token_reserves".to_string(), serde_json::json!(0u64)),
        (
            "real_quote_reserves".to_string(),
            serde_json::json!(85_000_000_000u64),
        ),
        ("complete".to_string(), serde_json::json!(true)),
    ]);
    let curve_idl = curve
        .idl
        .as_ref()
        .expect("Pump curve template must carry an IDL");
    assert!(
        curve_data.len() > CURVE_IDL_LEN,
        "the live curve is {} bytes, no longer than its {CURVE_IDL_LEN}-byte IDL type",
        curve_data.len()
    );
    let forged = surfnet_svm
        .get_forged_account_data(&pubkey, curve_data, curve_idl, &overrides)
        .expect("curve override on the live bonding curve");
    assert_eq!(
        forged.len(),
        curve_data.len(),
        "the curve's extend_account tail must survive"
    );
    assert_eq!(forged[CURVE_IDL_LEN..], curve_data[CURVE_IDL_LEN..]);
    let reserves = CURVE_VIRTUAL_TOKEN_RESERVES_OFFSET..CURVE_REAL_QUOTE_RESERVES_OFFSET + 8;
    let diffs = diff_indices(&forged, curve_data);
    assert!(
        diffs
            .iter()
            .all(|i| reserves.contains(i) || *i == CURVE_COMPLETE_OFFSET),
        "only the overridden curve fields may change, got {diffs:?}"
    );
    assert_eq!(
        read_u64(&forged, CURVE_VIRTUAL_TOKEN_RESERVES_OFFSET),
        279_900_000_000_000
    );
    assert_eq!(
        read_u64(&forged, CURVE_VIRTUAL_QUOTE_RESERVES_OFFSET),
        115_000_000_000
    );
    assert_eq!(read_u64(&forged, CURVE_REAL_TOKEN_RESERVES_OFFSET), 0);
    assert_eq!(
        read_u64(&forged, CURVE_REAL_QUOTE_RESERVES_OFFSET),
        85_000_000_000
    );
    assert_eq!(forged[CURVE_COMPLETE_OFFSET], 1);

    assert!(
        pool_data.len() > POOL_IDL_LEN,
        "the live pool is {} bytes, no longer than its {POOL_IDL_LEN}-byte IDL type",
        pool_data.len()
    );
    let pool = registry.get("pump-amm-canonical-pool").expect("template");
    let coin_creator = Pubkey::new_unique();
    let overrides = HashMap::from([
        ("lp_supply".to_string(), serde_json::json!(9_876_543_210u64)),
        (
            "coin_creator".to_string(),
            serde_json::json!(coin_creator.to_string()),
        ),
    ]);
    let pool_idl = pool
        .idl
        .as_ref()
        .expect("Pump AMM template must carry an IDL");
    let forged = surfnet_svm
        .get_forged_account_data(&pubkey, pool_data, pool_idl, &overrides)
        .expect("pool override on the live canonical pool");
    assert_eq!(
        forged.len(),
        pool_data.len(),
        "the pool's bytes past its IDL type must survive"
    );
    let lp_supply = POOL_LP_SUPPLY_OFFSET..POOL_LP_SUPPLY_OFFSET + 8;
    let creator = POOL_COIN_CREATOR_OFFSET..POOL_COIN_CREATOR_OFFSET + 32;
    let diffs = diff_indices(&forged, pool_data);
    assert!(
        diffs
            .iter()
            .all(|i| lp_supply.contains(i) || creator.contains(i)),
        "only lp_supply and coin_creator may change, got {diffs:?}"
    );
    assert_eq!(forged[POOL_IDL_LEN..], pool_data[POOL_IDL_LEN..]);
    assert_eq!(read_u64(&forged, POOL_LP_SUPPLY_OFFSET), 9_876_543_210);
    assert_eq!(forged[creator], coin_creator.to_bytes());

    let overrides = HashMap::from([(
        "virtual_quote_reserves".to_string(),
        serde_json::json!(-5_000_000_000i64),
    )]);
    let forged = surfnet_svm
        .get_forged_account_data(&pubkey, pool_data, pool_idl, &overrides)
        .expect("virtual_quote_reserves override on the live canonical pool");
    assert_eq!(forged.len(), pool_data.len());
    let diffs = diff_indices(&forged, pool_data);
    assert!(
        diffs
            .iter()
            .all(|i| POOL_VIRTUAL_QUOTE_RESERVES.contains(i)),
        "only the i128 virtual_quote_reserves may change, got {diffs:?}"
    );
    assert_eq!(read_virtual_quote_reserves(&forged), -5_000_000_000);

    assert!(
        config_data.len() >= AMM_GLOBAL_CONFIG_IDL_LEN,
        "the live GlobalConfig is {} bytes, shorter than its {AMM_GLOBAL_CONFIG_IDL_LEN}-byte IDL \
         type",
        config_data.len()
    );
    let config = registry.get("pump-amm-global-config").expect("template");
    let overrides = HashMap::from([
        ("lp_fee_basis_points".to_string(), serde_json::json!(77u64)),
        ("disable_flags".to_string(), serde_json::json!(0b1_0101u8)),
    ]);
    let config_idl = config
        .idl
        .as_ref()
        .expect("Pump AMM template must carry an IDL");
    let forged = surfnet_svm
        .get_forged_account_data(&pubkey, config_data, config_idl, &overrides)
        .expect("GlobalConfig override on the live account");
    assert_eq!(
        forged.len(),
        config_data.len(),
        "GlobalConfig's bytes past its IDL type must survive"
    );
    let lp_fee = AMM_LP_FEE_BASIS_POINTS_OFFSET..AMM_LP_FEE_BASIS_POINTS_OFFSET + 8;
    let diffs = diff_indices(&forged, config_data);
    assert!(
        diffs
            .iter()
            .all(|i| lp_fee.contains(i) || *i == AMM_DISABLE_FLAGS_OFFSET),
        "only lp_fee_basis_points and disable_flags may change, got {diffs:?}"
    );
    assert_eq!(
        forged[AMM_GLOBAL_CONFIG_IDL_LEN..],
        config_data[AMM_GLOBAL_CONFIG_IDL_LEN..]
    );
    assert_eq!(read_u64(&forged, AMM_LP_FEE_BASIS_POINTS_OFFSET), 77);
    assert_eq!(forged[AMM_DISABLE_FLAGS_OFFSET], 0b1_0101);
}

/// The production builder's validation against live pump state. Single-byte tweaks pin each
/// rejection branch regardless of where the live coin currently is in its lifecycle.
#[tokio::test]
async fn builder_rejects_bad_live_graduation_state() {
    let accounts = fetch(&[MINT, CURVE, BASE_VAULT, PUMP_GLOBAL]).await;
    let (mint, curve, vault, global) = (&accounts[0], &accounts[1], &accounts[2], &accounts[3]);

    let mut complete_curve = curve.clone();
    complete_curve.data[CURVE_COMPLETE_OFFSET] = 1;
    let error = build_pump_graduation_scenario(MINT, mint, &complete_curve, vault, None, global)
        .unwrap_err();
    assert!(
        error.to_string().contains("already complete"),
        "unexpected error: {error}"
    );

    let mut incomplete_curve = curve.clone();
    incomplete_curve.data[CURVE_COMPLETE_OFFSET] = 0;
    let error = build_pump_graduation_scenario(
        MINT,
        mint,
        &incomplete_curve,
        vault,
        Some(&Account::default()),
        global,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("canonical PumpSwap pool already exists"),
        "unexpected error: {error}"
    );

    let mut usdc_curve = incomplete_curve.clone();
    usdc_curve.data[CURVE_QUOTE_MINT_OFFSET..CURVE_QUOTE_MINT_OFFSET + 32]
        .copy_from_slice(USDC_MINT.as_ref());
    let error =
        build_pump_graduation_scenario(MINT, mint, &usdc_curve, vault, None, global).unwrap_err();
    assert!(
        error.to_string().contains("SOL-quoted bonding curves only"),
        "unexpected error: {error}"
    );
}

/// Every graduation account, derived for the discovered coin: template PDAs through the
/// builder, instruction-only accounts through the IDL-documented seeds, and the fee wiring
/// read from the live `Global` so recipient rotation cannot break the test.
struct GraduationFixture {
    mint: Pubkey,
    curve: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
    fee_recipient: Pubkey,
    fee_recipient_quote: Pubkey,
    buyback_recipient: Pubkey,
    buyback_recipient_quote: Pubkey,
    creator_vault: Pubkey,
    creator_vault_quote: Pubkey,
    sharing_config: Pubkey,
    global_volume_accumulator: Pubkey,
    fee_config: Pubkey,
    withdraw_authority: Pubkey,
    user: Pubkey,
    user_base: Pubkey,
    user_quote: Pubkey,
    user_volume_accumulator: Pubkey,
    user_volume_accumulator_quote: Pubkey,
    pool_authority: Pubkey,
    pool: Pubkey,
    lp_mint: Pubkey,
    pool_authority_base: Pubkey,
    pool_authority_quote: Pubkey,
    pool_authority_lp: Pubkey,
    pool_base: Pubkey,
    pool_quote: Pubkey,
    pump_event_authority: Pubkey,
    pamm_event_authority: Pubkey,
    boost_vault_authority: Pubkey,
    boost_vault: Pubkey,
}

impl GraduationFixture {
    fn new(user: Pubkey, mint: Pubkey, curve_data: &[u8], global_data: &[u8]) -> Self {
        let addresses =
            pump_graduation_addresses(&mint).expect("graduation addresses should resolve");
        let creator =
            Pubkey::try_from(&curve_data[CURVE_CREATOR_OFFSET..CURVE_CREATOR_OFFSET + 32])
                .expect("curve creator");
        let creator_vault =
            Pubkey::find_program_address(&[b"creator-vault", creator.as_ref()], &PUMP).0;
        // A mayhem-mode coin pays its fees to the reserved recipient; any other is NotAuthorized.
        let fee_recipient_offset = if curve_data[CURVE_IS_MAYHEM_MODE_OFFSET] != 0 {
            GLOBAL_RESERVED_FEE_RECIPIENT_OFFSET
        } else {
            GLOBAL_FEE_RECIPIENT_OFFSET
        };
        let fee_recipient =
            Pubkey::try_from(&global_data[fee_recipient_offset..fee_recipient_offset + 32])
                .expect("global fee recipient");
        let buyback_recipient = Pubkey::try_from(
            &global_data[GLOBAL_BUYBACK_RECIPIENTS_OFFSET..GLOBAL_BUYBACK_RECIPIENTS_OFFSET + 32],
        )
        .expect("global buyback recipient");
        let withdraw_authority = Pubkey::try_from(
            &global_data[GLOBAL_WITHDRAW_AUTHORITY_OFFSET..GLOBAL_WITHDRAW_AUTHORITY_OFFSET + 32],
        )
        .expect("global withdraw authority");
        let user_volume_accumulator =
            Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], &PUMP).0;
        let pool_authority =
            Pubkey::find_program_address(&[b"pool-authority", mint.as_ref()], &PUMP).0;
        let pool = Pubkey::find_program_address(
            &[
                b"pool",
                &0u16.to_le_bytes(),
                pool_authority.as_ref(),
                mint.as_ref(),
                WSOL.as_ref(),
            ],
            &PAMM,
        )
        .0;
        let lp_mint = Pubkey::find_program_address(&[b"pool_lp_mint", pool.as_ref()], &PAMM).0;
        let boost_vault_authority =
            Pubkey::find_program_address(&[b"boost_vault", pool.as_ref()], &PAMM).0;

        Self {
            mint,
            curve: addresses.bonding_curve,
            base_vault: addresses.curve_vault,
            quote_vault: associated_token_address(&addresses.bonding_curve, &WSOL, &TOKENKEG),
            fee_recipient,
            fee_recipient_quote: associated_token_address(&fee_recipient, &WSOL, &TOKENKEG),
            buyback_recipient,
            buyback_recipient_quote: associated_token_address(&buyback_recipient, &WSOL, &TOKENKEG),
            creator_vault,
            creator_vault_quote: associated_token_address(&creator_vault, &WSOL, &TOKENKEG),
            sharing_config: Pubkey::find_program_address(
                &[b"sharing-config", mint.as_ref()],
                &FEE_PROGRAM,
            )
            .0,
            global_volume_accumulator: Pubkey::find_program_address(
                &[b"global_volume_accumulator"],
                &PUMP,
            )
            .0,
            fee_config: Pubkey::find_program_address(&[b"fee_config", PUMP.as_ref()], &FEE_PROGRAM)
                .0,
            withdraw_authority,
            user,
            user_base: associated_token_address(&user, &mint, &TOKEN_2022),
            user_quote: associated_token_address(&user, &WSOL, &TOKENKEG),
            user_volume_accumulator,
            user_volume_accumulator_quote: associated_token_address(
                &user_volume_accumulator,
                &WSOL,
                &TOKENKEG,
            ),
            pool_authority,
            pool,
            lp_mint,
            pool_authority_base: associated_token_address(&pool_authority, &mint, &TOKEN_2022),
            pool_authority_quote: associated_token_address(&pool_authority, &WSOL, &TOKENKEG),
            pool_authority_lp: associated_token_address(&pool_authority, &lp_mint, &TOKEN_2022),
            pool_base: associated_token_address(&pool, &mint, &TOKEN_2022),
            pool_quote: associated_token_address(&pool, &WSOL, &TOKENKEG),
            pump_event_authority: Pubkey::find_program_address(&[b"__event_authority"], &PUMP).0,
            pamm_event_authority: Pubkey::find_program_address(&[b"__event_authority"], &PAMM).0,
            boost_vault_authority,
            boost_vault: associated_token_address(&boost_vault_authority, &WSOL, &TOKENKEG),
        }
    }
}

fn associated_token_address(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    )
    .0
}

fn account_meta(pubkey: Pubkey, signer: bool, writable: bool) -> AccountMeta {
    if writable {
        AccountMeta::new(pubkey, signer)
    } else {
        AccountMeta::new_readonly(pubkey, signer)
    }
}

fn start_live_surfnet() -> (RpcClient, SurfnetSvmLocker, RunloopGuard) {
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
    let (surfnet_svm, simnet_events_rx, geyser_events_rx) = TestType::no_db().initialize_svm();
    let (simnet_commands_tx, simnet_commands_rx) = unbounded();
    let locker = SurfnetSvmLocker::new(surfnet_svm);
    let runloop = spawn_runloop(
        locker.clone(),
        config,
        (simnet_commands_tx, simnet_commands_rx),
        geyser_events_rx,
    )
    .expect("the runloop should start");
    wait_for_ready_and_connected(&simnet_events_rx)
        .expect("surfnet should be ready and connected to the datasource");
    let rpc = RpcClient::new_with_commitment(
        format!("http://{bind_host}:{bind_port}"),
        CommitmentConfig::confirmed(),
    );

    (rpc, locker, runloop)
}

/// Creates the test's own Token-2022 coin on the fork with pump's `create_v2`, so the test does
/// not depend on which coins trade on mainnet, and returns its graduation plan.
async fn create_graduation_candidate(
    surfnet: &RpcClient,
    mayhem: bool,
) -> (Pubkey, PumpGraduationPreparation) {
    let creator = Keypair::new();
    let mint = Keypair::new();
    cheatcode(
        surfnet,
        "surfnet_setAccount",
        serde_json::json!([creator.pubkey().to_string(), { "lamports": 1_000_000_000u64 }]),
    )
    .await;

    let addresses = pump_graduation_addresses(&mint.pubkey()).unwrap();
    let mayhem_pda = |seeds: &[&[u8]]| Pubkey::find_program_address(seeds, &MAYHEM_PROGRAM).0;
    let sol_vault = mayhem_pda(&[b"sol-vault"]);
    let accounts = vec![
        account_meta(mint.pubkey(), true, true), // 0 mint
        account_meta(
            Pubkey::find_program_address(&[b"mint-authority"], &PUMP).0,
            false,
            false,
        ), // 1 mint_authority
        account_meta(addresses.bonding_curve, false, true), // 2 bonding_curve
        account_meta(addresses.curve_vault, false, true), // 3 associated_bonding_curve
        account_meta(PUMP_GLOBAL, false, false), // 4 global
        account_meta(creator.pubkey(), true, true), // 5 user
        account_meta(SYSTEM_PROGRAM, false, false), // 6 system_program
        account_meta(TOKEN_2022, false, false),  // 7 token_program
        account_meta(ATA_PROGRAM, false, false), // 8 associated_token_program
        account_meta(MAYHEM_PROGRAM, false, true), // 9 mayhem_program_id
        account_meta(mayhem_pda(&[b"global-params"]), false, false), // 10 global_params
        account_meta(sol_vault, false, true),    // 11 sol_vault
        account_meta(
            mayhem_pda(&[b"mayhem-state", mint.pubkey().as_ref()]),
            false,
            true,
        ), // 12 mayhem_state
        account_meta(
            associated_token_address(&sol_vault, &mint.pubkey(), &TOKEN_2022),
            false,
            true,
        ), // 13 mayhem_token_vault
        account_meta(
            Pubkey::find_program_address(&[b"__event_authority"], &PUMP).0,
            false,
            false,
        ), // 14 event_authority
        account_meta(PUMP, false, false),        // 15 program
    ];
    let mut data = CREATE_V2_DISCRIMINATOR.to_vec();
    for text in ["Surfpool test", "SURF", ""] {
        data.extend_from_slice(&(text.len() as u32).to_le_bytes());
        data.extend_from_slice(text.as_bytes());
    }
    data.extend_from_slice(creator.pubkey().as_ref());
    data.extend_from_slice(&[mayhem as u8, 0]); // is_mayhem_mode, is_cashback_enabled

    let create = Instruction {
        program_id: PUMP,
        accounts,
        data,
    };
    let blockhash = surfnet.get_latest_blockhash().await.unwrap();
    let transaction = Transaction::new_signed_with_payer(
        &[create],
        Some(&creator.pubkey()),
        &[&creator, &mint],
        blockhash,
    );
    surfnet
        .send_and_confirm_transaction(&transaction)
        .await
        .unwrap_or_else(|error| panic!("create_v2 failed: {error:?}"));

    let account = |address: Pubkey| async move { surfnet.get_account(&address).await.unwrap() };
    let preparation = build_pump_graduation_scenario(
        mint.pubkey(),
        &account(mint.pubkey()).await,
        &account(addresses.bonding_curve).await,
        &account(addresses.curve_vault).await,
        None,
        &account(PUMP_GLOBAL).await,
    )
    .unwrap();
    (mint.pubkey(), preparation)
}

async fn cheatcode(rpc: &RpcClient, method: &'static str, params: serde_json::Value) {
    let _: serde_json::Value = rpc
        .send(RpcRequest::Custom { method }, params)
        .await
        .unwrap_or_else(|error| panic!("{method} cheatcode failed: {error:?}"));
}

fn read_virtual_quote_reserves(data: &[u8]) -> i128 {
    i128::from_le_bytes(data[POOL_VIRTUAL_QUOTE_RESERVES].try_into().unwrap())
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        data.get(offset..offset + 8)
            .unwrap_or_else(|| panic!("missing u64 at account-data offset {offset}"))
            .try_into()
            .unwrap(),
    )
}

async fn token_amount(rpc: &RpcClient, address: &Pubkey) -> u64 {
    let account = rpc
        .get_account(address)
        .await
        .unwrap_or_else(|error| panic!("token account {address} should exist: {error:?}"));
    read_u64(&account.data, TOKEN_AMOUNT_OFFSET)
}

async fn send_transaction(rpc: &RpcClient, payer: &Keypair, instructions: Vec<Instruction>) {
    let transaction = signed_transaction(rpc, payer, instructions).await;
    rpc.send_and_confirm_transaction(&transaction)
        .await
        .unwrap_or_else(|error| panic!("transaction failed: {error:?}"));
}

async fn signed_transaction(
    rpc: &RpcClient,
    payer: &Keypair,
    instructions: Vec<Instruction>,
) -> Transaction {
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .expect("recent blockhash should be available");
    Transaction::new_signed_with_payer(&instructions, Some(&payer.pubkey()), &[payer], blockhash)
}

async fn simulate_token_amount_after_transaction(
    rpc: &RpcClient,
    payer: &Keypair,
    instruction: Instruction,
    token_account: Pubkey,
) -> u64 {
    let transaction = signed_transaction(rpc, payer, vec![instruction]).await;
    let simulation = rpc
        .simulate_transaction_with_config(
            &transaction,
            RpcSimulateTransactionConfig {
                sig_verify: true,
                commitment: Some(CommitmentConfig::confirmed()),
                accounts: Some(RpcSimulateTransactionAccountsConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    addresses: vec![token_account.to_string()],
                }),
                ..RpcSimulateTransactionConfig::default()
            },
        )
        .await
        .unwrap();
    if simulation.value.err.is_some() {
        for line in simulation.value.logs.iter().flatten() {
            eprintln!("{line}");
        }
    }
    assert_eq!(simulation.value.err, None, "swap simulation should succeed");
    let account_data = simulation
        .value
        .accounts
        .unwrap()
        .into_iter()
        .next()
        .flatten()
        .and_then(|account| account.data.decode())
        .expect("simulation should return the requested token account");
    read_u64(&account_data, TOKEN_AMOUNT_OFFSET)
}

async fn fund_user(rpc: &RpcClient, fixture: &GraduationFixture) {
    cheatcode(
        rpc,
        "surfnet_setAccount",
        serde_json::json!([fixture.user.to_string(), { "lamports": 2_000_000_000u64 }]),
    )
    .await;
    cheatcode(
        rpc,
        "surfnet_setTokenAccount",
        serde_json::json!([fixture.user.to_string(), WSOL.to_string(), { "amount": 1_500_000_000u64 }, null]),
    )
    .await;
    // buy_v2 requires the user's Token-2022 base ATA to exist.
    cheatcode(
        rpc,
        "surfnet_setTokenAccount",
        serde_json::json!([fixture.user.to_string(), fixture.mint.to_string(), { "amount": 0u64 }, TOKEN_2022.to_string()]),
    )
    .await;
}

fn build_buy_v2(fixture: &GraduationFixture, completing_buy_amount: u64) -> Instruction {
    let accounts = vec![
        account_meta(PUMP_GLOBAL, false, false),          // 0 global
        account_meta(fixture.mint, false, false),         // 1 base_mint
        account_meta(WSOL, false, false),                 // 2 quote_mint
        account_meta(TOKEN_2022, false, false),           // 3 base_token_program
        account_meta(TOKENKEG, false, false),             // 4 quote_token_program
        account_meta(ATA_PROGRAM, false, false),          // 5 associated_token_program
        account_meta(fixture.fee_recipient, false, true), // 6 fee_recipient
        account_meta(fixture.fee_recipient_quote, false, true), // 7 associated_quote_fee_recipient
        account_meta(fixture.buyback_recipient, false, true), // 8 buyback_fee_recipient
        account_meta(fixture.buyback_recipient_quote, false, true), // 9 associated_quote_buyback_fee_recipient
        account_meta(fixture.curve, false, true),                   // 10 bonding_curve
        account_meta(fixture.base_vault, false, true), // 11 associated_base_bonding_curve
        account_meta(fixture.quote_vault, false, true), // 12 associated_quote_bonding_curve
        account_meta(fixture.user, true, true),        // 13 user
        account_meta(fixture.user_base, false, true),  // 14 associated_base_user
        account_meta(fixture.user_quote, false, true), // 15 associated_quote_user
        account_meta(fixture.creator_vault, false, true), // 16 creator_vault
        account_meta(fixture.creator_vault_quote, false, true), // 17 associated_creator_vault
        account_meta(fixture.sharing_config, false, false), // 18 sharing_config
        account_meta(fixture.global_volume_accumulator, false, false), // 19 global_volume_accumulator
        account_meta(fixture.user_volume_accumulator, false, true),    // 20 user_volume_accumulator
        account_meta(fixture.user_volume_accumulator_quote, false, true), // 21 associated_user_volume_accumulator
        account_meta(fixture.fee_config, false, false),                   // 22 fee_config
        account_meta(FEE_PROGRAM, false, false),                          // 23 fee_program
        account_meta(SYSTEM_PROGRAM, false, false),                       // 24 system_program
        account_meta(fixture.pump_event_authority, false, false),         // 25 event_authority
        account_meta(PUMP, false, false),                                 // 26 program
    ];
    let mut data = BUY_V2_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&completing_buy_amount.to_le_bytes());
    data.extend_from_slice(&MAX_SOL_COST.to_le_bytes());

    Instruction {
        program_id: PUMP,
        accounts,
        data,
    }
}

fn build_migrate_v2(fixture: &GraduationFixture) -> Vec<Instruction> {
    let accounts = vec![
        account_meta(PUMP_GLOBAL, false, false), // 0 global
        account_meta(fixture.withdraw_authority, false, true), // 1 withdraw_authority
        account_meta(fixture.mint, false, false), // 2 base_mint
        account_meta(WSOL, false, false),        // 3 quote_mint
        account_meta(fixture.curve, false, true), // 4 bonding_curve
        account_meta(fixture.base_vault, false, true), // 5 associated_base_bonding_curve
        account_meta(fixture.quote_vault, false, true), // 6 associated_quote_bonding_curve
        account_meta(fixture.user, true, false), // 7 user
        account_meta(SYSTEM_PROGRAM, false, false), // 8 system_program
        account_meta(PAMM, false, false),        // 9 pump_amm_program
        account_meta(fixture.pool, false, true), // 10 pool
        account_meta(fixture.pool_authority, false, true), // 11 pool_authority
        account_meta(fixture.pool_authority_base, false, true), // 12 pool_authority_mint_account
        account_meta(fixture.pool_authority_quote, false, true), // 13 pool_authority_quote_account
        account_meta(AMM_GLOBAL_CONFIG, false, false), // 14 amm_global_config
        account_meta(fixture.lp_mint, false, true), // 15 pool_lp_mint
        account_meta(fixture.pool_authority_lp, false, true), // 16 user_pool_token_account
        account_meta(fixture.pool_base, false, true), // 17 pool_base_token_account
        account_meta(fixture.pool_quote, false, true), // 18 pool_quote_token_account
        account_meta(TOKEN_2022, false, false),  // 19 base_token_program
        account_meta(TOKENKEG, false, false),    // 20 quote_token_program
        account_meta(TOKEN_2022, false, false),  // 21 token_2022_program
        account_meta(ATA_PROGRAM, false, false), // 22 associated_token_program
        account_meta(fixture.pamm_event_authority, false, false), // 23 pump_amm_event_authority
        account_meta(RENT_SYSVAR, false, false), // 24 rent
        account_meta(fixture.pump_event_authority, false, false), // 25 event_authority
        account_meta(PUMP, false, false),        // 26 program
        // Remaining accounts feed the pAMM init_boost CPI: the pool's boost vault
        // authority PDA and its quote ATA (per the pump-amm IDL).
        account_meta(fixture.boost_vault_authority, false, true), // 27 boost_vault_authority
        account_meta(fixture.boost_vault, false, true),           // 28 boost_vault
    ];

    vec![
        ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
        Instruction {
            program_id: PUMP,
            accounts,
            data: MIGRATE_V2_DISCRIMINATOR.to_vec(),
        },
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pump_token2022_graduation_lifecycle() {
    graduation_lifecycle(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pump_token2022_mayhem_graduation_lifecycle() {
    graduation_lifecycle(true).await;
}

/// Graduates a coin the test creates on the fork, while the pump and pAMM programs run live from
/// mainnet.
async fn graduation_lifecycle(mayhem: bool) {
    let (rpc, locker, _runloop) = start_live_surfnet();
    let (mint, preparation) = create_graduation_candidate(&rpc, mayhem).await;
    let completing_buy_amount = preparation.completing_buy_amount;
    let migration_reserve = preparation.migration_reserve;

    // The builder must derive its plan from the supplied curve state, not from
    // constants: a doubled virtual quote reserve must change the finishing buy.
    let curve_account = rpc
        .get_account(&preparation.addresses.bonding_curve)
        .await
        .unwrap();
    assert_eq!(
        curve_account.data[CURVE_IS_MAYHEM_MODE_OFFSET], mayhem as u8,
        "create_v2 should set the coin's mayhem mode"
    );
    let mut scaled_curve = curve_account.clone();
    let virtual_quote = read_u64(&scaled_curve.data, CURVE_VIRTUAL_QUOTE_RESERVES_OFFSET);
    scaled_curve.data[CURVE_VIRTUAL_QUOTE_RESERVES_OFFSET..CURVE_VIRTUAL_QUOTE_RESERVES_OFFSET + 8]
        .copy_from_slice(&(virtual_quote * 2).to_le_bytes());
    let scaled = build_pump_graduation_scenario(
        mint,
        &rpc.get_account(&mint).await.unwrap(),
        &scaled_curve,
        &rpc.get_account(&preparation.addresses.curve_vault)
            .await
            .unwrap(),
        None,
        &rpc.get_account(&PUMP_GLOBAL).await.unwrap(),
    )
    .unwrap();
    assert_ne!(
        scaled.completing_buy_amount, completing_buy_amount,
        "the graduation plan must be driven by the supplied curve state"
    );

    let user = Keypair::new();
    let global_data = rpc.get_account(&PUMP_GLOBAL).await.unwrap().data;
    let fixture = GraduationFixture::new(user.pubkey(), mint, &curve_account.data, &global_data);

    locker
        .with_svm_writer(|svm| svm.register_scenario(preparation.scenario, Some(0)))
        .unwrap();
    locker
        .materialize_overrides_for_slot(&None, 1)
        .await
        .unwrap();
    fund_user(&rpc, &fixture).await;

    send_transaction(
        &rpc,
        &user,
        vec![build_buy_v2(&fixture, completing_buy_amount)],
    )
    .await;

    let curve_after = rpc.get_account(&fixture.curve).await.unwrap();
    assert_eq!(
        read_u64(&curve_after.data, CURVE_REAL_TOKEN_RESERVES_OFFSET),
        0,
        "buy should exhaust the curve's real token reserves"
    );
    assert_eq!(
        curve_after.data[CURVE_COMPLETE_OFFSET], 1,
        "buy should complete the curve"
    );
    assert_eq!(
        token_amount(&rpc, &fixture.user_base).await,
        completing_buy_amount,
        "user should receive the purchased base tokens"
    );
    assert_eq!(
        token_amount(&rpc, &fixture.base_vault).await,
        migration_reserve,
        "buy should leave the migration reserve in the curve vault"
    );

    send_transaction(&rpc, &user, build_migrate_v2(&fixture)).await;

    assert_eq!(
        rpc.get_account(&fixture.pool).await.unwrap().owner,
        PAMM,
        "migrate should create a pAMM-owned pool"
    );
    assert_eq!(
        rpc.get_account(&fixture.lp_mint).await.unwrap().owner,
        TOKEN_2022,
        "migrate should create a Token-2022 LP mint"
    );
    assert_eq!(
        token_amount(&rpc, &fixture.pool_base).await,
        migration_reserve,
        "migrate should seed the pool with the reserved base liquidity"
    );
    assert!(
        token_amount(&rpc, &fixture.pool_quote).await > 0,
        "migrate should seed the pool with quote liquidity"
    );
}

/// Sell against an established canonical pool; newly migrated pools use a different
/// fee wiring (an open follow-up).
async fn build_sell(
    rpc: &RpcClient,
    user: Pubkey,
    base_mint: Pubkey,
    base_token_program: Pubkey,
    pool: Pubkey,
    base_amount_in: u64,
) -> Instruction {
    let global_config = rpc
        .get_account(&AMM_GLOBAL_CONFIG)
        .await
        .expect("AMM global config should load");
    let protocol_fee_recipient = Pubkey::try_from(
        &global_config.data
            [AMM_PROTOCOL_FEE_RECIPIENTS_OFFSET..AMM_PROTOCOL_FEE_RECIPIENTS_OFFSET + 32],
    )
    .unwrap();
    let pool_data = rpc
        .get_account(&pool)
        .await
        .expect("live pool should exist")
        .data;
    let coin_creator =
        Pubkey::try_from(&pool_data[POOL_COIN_CREATOR_OFFSET..POOL_COIN_CREATOR_OFFSET + 32])
            .unwrap();
    let coin_creator_vault_authority =
        Pubkey::find_program_address(&[b"creator_vault", coin_creator.as_ref()], &PAMM).0;
    let sell_fee_config =
        Pubkey::find_program_address(&[b"fee_config", PAMM.as_ref()], &FEE_PROGRAM).0;
    let pool_v2 = Pubkey::find_program_address(&[b"pool-v2", base_mint.as_ref()], &PAMM).0;
    let breaking_fee_recipient =
        Pubkey::from_str_const("EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL");
    let pamm_event_authority = Pubkey::find_program_address(&[b"__event_authority"], &PAMM).0;
    let accounts = vec![
        account_meta(pool, false, true),               // 0 pool
        account_meta(user, true, true),                // 1 user
        account_meta(AMM_GLOBAL_CONFIG, false, false), // 2 global_config
        account_meta(base_mint, false, false),         // 3 base_mint
        account_meta(WSOL, false, false),              // 4 quote_mint
        account_meta(
            associated_token_address(&user, &base_mint, &base_token_program),
            false,
            true,
        ), // 5 user_base_token_account
        account_meta(
            associated_token_address(&user, &WSOL, &TOKENKEG),
            false,
            true,
        ), // 6 user_quote_token_account
        account_meta(
            associated_token_address(&pool, &base_mint, &base_token_program),
            false,
            true,
        ), // 7 pool_base_token_account
        account_meta(
            associated_token_address(&pool, &WSOL, &TOKENKEG),
            false,
            true,
        ), // 8 pool_quote_token_account
        account_meta(protocol_fee_recipient, false, false), // 9 protocol_fee_recipient
        account_meta(
            associated_token_address(&protocol_fee_recipient, &WSOL, &TOKENKEG),
            false,
            true,
        ), // 10 protocol_fee_recipient_token_account
        account_meta(base_token_program, false, false), // 11 base_token_program
        account_meta(TOKENKEG, false, false),          // 12 quote_token_program
        account_meta(SYSTEM_PROGRAM, false, false),    // 13 system_program
        account_meta(ATA_PROGRAM, false, false),       // 14 associated_token_program
        account_meta(pamm_event_authority, false, false), // 15 event_authority
        account_meta(PAMM, false, false),              // 16 program
        account_meta(
            associated_token_address(&coin_creator_vault_authority, &WSOL, &TOKENKEG),
            false,
            true,
        ), // 17 coin_creator_vault_ata
        account_meta(coin_creator_vault_authority, false, false), // 18 coin_creator_vault_authority
        account_meta(sell_fee_config, false, false),   // 19 fee_config
        account_meta(FEE_PROGRAM, false, false),       // 20 fee_program
        account_meta(pool_v2, false, false),           // 21 pool_v2
        account_meta(breaking_fee_recipient, false, false), // 22 fee_recipient
        account_meta(
            associated_token_address(&breaking_fee_recipient, &WSOL, &TOKENKEG),
            false,
            true,
        ), // 23 fee_recipient_token_account
    ];
    let mut data = SELL_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&base_amount_in.to_le_bytes());
    // Zero slippage protection is acceptable only in this regression test.
    data.extend_from_slice(&0u64.to_le_bytes());

    Instruction {
        program_id: PAMM,
        accounts,
        data,
    }
}

/// The virtual quote reserve shock must change what the deployed AMM actually quotes, proven on
/// the established live canonical pool (VERIFY-12 / MODEL-06). The override carries only the
/// mint and the new value, the payload Studio's price-shock preset sends.
#[tokio::test(flavor = "multi_thread")]
async fn price_shock_changes_a_live_pool_swap() {
    let (rpc, locker, _runloop) = start_live_surfnet();
    let user = Keypair::new();
    let base_token_program = rpc.get_account(&LLS_MINT).await.unwrap().owner;

    cheatcode(
        &rpc,
        "surfnet_setAccount",
        serde_json::json!([user.pubkey().to_string(), { "lamports": 2_000_000_000u64 }]),
    )
    .await;
    let user_quote_start = 1_000_000_000u64;
    cheatcode(
        &rpc,
        "surfnet_setTokenAccount",
        serde_json::json!([user.pubkey().to_string(), WSOL.to_string(), { "amount": user_quote_start }, null]),
    )
    .await;
    cheatcode(
        &rpc,
        "surfnet_setTokenAccount",
        serde_json::json!([
            user.pubkey().to_string(),
            LLS_MINT.to_string(),
            { "amount": 1_000_000_000u64 },
            base_token_program.to_string()
        ]),
    )
    .await;

    let user_quote = associated_token_address(&user.pubkey(), &WSOL, &TOKENKEG);
    let pool_data = rpc.get_account(&AMM_POOL).await.unwrap().data;
    let pool_quote_vault = Pubkey::try_from(
        &pool_data[POOL_QUOTE_TOKEN_ACCOUNT_OFFSET..POOL_QUOTE_TOKEN_ACCOUNT_OFFSET + 32],
    )
    .unwrap();
    let pool_quote_balance = token_amount(&rpc, &pool_quote_vault).await;
    let live_virtual_quote = read_virtual_quote_reserves(&pool_data);
    let shocked_virtual_quote = i128::from(pool_quote_balance) * 9;
    assert!(
        shocked_virtual_quote > live_virtual_quote,
        "the shock must raise the effective quote reserves"
    );
    let sell = build_sell(
        &rpc,
        user.pubkey(),
        LLS_MINT,
        base_token_program,
        AMM_POOL,
        500_000_000,
    )
    .await;

    let baseline =
        simulate_token_amount_after_transaction(&rpc, &user, sell.clone(), user_quote).await;

    let registry = TemplateRegistry::new();
    let template = registry
        .get("pump-amm-canonical-pool")
        .expect("PumpSwap template");
    let values = HashMap::from([
        (
            "base_mint".to_string(),
            serde_json::json!(LLS_MINT.to_string()),
        ),
        (
            "virtual_quote_reserves".to_string(),
            serde_json::json!(i64::try_from(shocked_virtual_quote).unwrap()),
        ),
    ]);
    let mut pool_override = OverrideInstance::new(template.id.clone(), 0, template.address.clone())
        .with_values(values)
        .with_label("PumpSwap virtual quote reserve shock".to_string());
    pool_override.fetch_before_use = true;
    let mut price_shock = Scenario::new(
        "PumpSwap Price Shock".to_string(),
        "Shift a canonical PumpSwap pool price through its virtual quote reserves.".to_string(),
    );
    price_shock.tags = vec!["pumpswap".to_string(), "price-shock".to_string()];
    price_shock.add_override(pool_override);
    locker
        .with_svm_writer(|svm| svm.register_scenario(price_shock, Some(100)))
        .unwrap();
    locker
        .materialize_overrides_for_slot(&None, 100)
        .await
        .unwrap();

    let shocked_pool = rpc.get_account(&AMM_POOL).await.unwrap().data;
    assert_eq!(
        read_virtual_quote_reserves(&shocked_pool),
        shocked_virtual_quote
    );
    assert_eq!(
        token_amount(&rpc, &pool_quote_vault).await,
        pool_quote_balance,
        "the shock must not touch the quote vault"
    );

    let shocked =
        simulate_token_amount_after_transaction(&rpc, &user, sell.clone(), user_quote).await;
    let effective_before = i128::from(pool_quote_balance) + live_virtual_quote;
    let effective_after = i128::from(pool_quote_balance) + shocked_virtual_quote;
    assert!(
        shocked > baseline,
        "raising the effective quote reserves from {effective_before} to {effective_after} should \
         raise the sell output, got {baseline} before and {shocked} after"
    );
    // A sell's output scales with the effective quote reserves; fees, which are a fraction of
    // the output, keep the two ratios within a couple of percent.
    let output_ratio = (shocked - user_quote_start) as f64 / (baseline - user_quote_start) as f64;
    let reserve_ratio = effective_after as f64 / effective_before as f64;
    assert!(
        (output_ratio / reserve_ratio - 1.0).abs() < 0.02,
        "the sell output grew {output_ratio}x while the effective quote reserves grew \
         {reserve_ratio}x"
    );

    send_transaction(&rpc, &user, vec![sell]).await;
    assert!(
        token_amount(&rpc, &user_quote).await > 0,
        "sell should credit the user's quote tokens"
    );
}

fn pool_override(
    template_id: &str,
    values: &[(&str, serde_json::Value)],
    fetch_before_use: bool,
) -> OverrideInstance {
    let registry = TemplateRegistry::new();
    let template = registry.get(template_id).expect("template");
    let values = values
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .chain([(
            "base_mint".to_string(),
            serde_json::json!(LLS_MINT.to_string()),
        )])
        .collect();
    let mut instance =
        OverrideInstance::new(template.id.clone(), 0, template.address.clone()).with_values(values);
    instance.fetch_before_use = fetch_before_use;
    instance
}

/// Registers `overrides` as one scenario at one slot and plays them through the materializer.
async fn play(
    svm: &mut SurfnetSvm,
    overrides: Vec<OverrideInstance>,
    remote: bool,
) -> SurfpoolResult<()> {
    const SLOT: u64 = 100;
    let mut scenario = Scenario::new("PumpSwap pool".to_string(), String::new());
    for instance in overrides {
        scenario.add_override(instance);
    }
    svm.register_scenario(scenario, Some(SLOT))?;
    let remote = remote.then(|| {
        (
            SurfnetRemoteClient::new(
                std::env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_RPC_URL.to_string()),
            ),
            CommitmentConfig::confirmed(),
        )
    });
    svm.materialize_overrides_for_slot(&remote, SLOT).await
}

/// Two IDL overrides of one pool at one slot keep each other's writes in either order, with
/// both fetching the pool first: the second override does not fetch again over the first one's
/// write.
#[tokio::test(flavor = "multi_thread")]
async fn two_overrides_of_one_pool_keep_each_other() {
    let coin_creator = Pubkey::new_unique();
    let virtual_quote = -123_456_789_012_345_678_901_234_567_890i128;
    let creator_values = [
        ("lp_supply", serde_json::json!(9_876_543_210u64)),
        ("coin_creator", serde_json::json!(coin_creator.to_string())),
    ];
    let virtual_quote_values = [(
        "virtual_quote_reserves",
        serde_json::json!(virtual_quote.to_string()),
    )];
    let lp_supply = POOL_LP_SUPPLY_OFFSET..POOL_LP_SUPPLY_OFFSET + 8;
    let creator = POOL_COIN_CREATOR_OFFSET..POOL_COIN_CREATOR_OFFSET + 32;

    for virtual_quote_first in [false, true] {
        let mut overrides = vec![
            pool_override("pump-amm-canonical-pool", &creator_values, true),
            pool_override("pump-amm-canonical-pool", &virtual_quote_values, true),
        ];
        if virtual_quote_first {
            overrides.reverse();
        }
        let live = fetch(&[AMM_POOL]).await.remove(0);
        let (mut svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
        play(&mut svm, overrides, true).await.unwrap();

        let written = svm.get_account(&AMM_POOL).unwrap().expect("pool").data;
        assert_eq!(written.len(), live.data.len());
        let diffs = diff_indices(&written, &live.data);
        assert!(
            diffs.iter().all(|i| lp_supply.contains(i)
                || creator.contains(i)
                || POOL_VIRTUAL_QUOTE_RESERVES.contains(i)),
            "virtual_quote_first = {virtual_quote_first}: only the three overridden fields may \
             change, got {diffs:?}"
        );
        assert_eq!(read_u64(&written, POOL_LP_SUPPLY_OFFSET), 9_876_543_210);
        assert_eq!(written[creator.clone()], coin_creator.to_bytes());
        assert_eq!(read_virtual_quote_reserves(&written), virtual_quote);
    }

    // One override writes all three fields.
    let live = fetch(&[AMM_POOL]).await.remove(0);
    let (mut svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    svm.set_account(&AMM_POOL, live.clone()).unwrap();
    let mut values = creator_values.to_vec();
    values.extend(virtual_quote_values.clone());
    play(
        &mut svm,
        vec![pool_override("pump-amm-canonical-pool", &values, false)],
        false,
    )
    .await
    .unwrap();
    let written = svm.get_account(&AMM_POOL).unwrap().expect("pool").data;
    assert_eq!(written.len(), live.data.len());
    let diffs = diff_indices(&written, &live.data);
    assert!(
        diffs.iter().all(|i| lp_supply.contains(i)
            || creator.contains(i)
            || POOL_VIRTUAL_QUOTE_RESERVES.contains(i)),
        "only the three overridden fields may change, got {diffs:?}"
    );
    assert_eq!(read_u64(&written, POOL_LP_SUPPLY_OFFSET), 9_876_543_210);
    assert_eq!(written[creator], coin_creator.to_bytes());
    assert_eq!(read_virtual_quote_reserves(&written), virtual_quote);
}

/// The by-address template writes only bytes 245..261 of a live pool.
#[tokio::test(flavor = "multi_thread")]
async fn the_pool_template_writes_virtual_quote_reserves_by_address() {
    let live = fetch(&[AMM_POOL]).await.remove(0);
    let virtual_quote = 98_765_432_109_876_543_210_987i128;
    assert_ne!(read_virtual_quote_reserves(&live.data), virtual_quote);

    let registry = TemplateRegistry::new();
    let template = registry.get("pump-amm-pool-state").expect("template");
    let instance = OverrideInstance::new(
        template.id.clone(),
        0,
        AccountAddress::Pubkey(AMM_POOL.to_string()),
    )
    .with_values(HashMap::from([(
        "virtual_quote_reserves".to_string(),
        serde_json::json!(virtual_quote.to_string()),
    )]));
    let (mut svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
    svm.set_account(&AMM_POOL, live.clone()).unwrap();
    play(&mut svm, vec![instance], false).await.unwrap();

    let written = svm.get_account(&AMM_POOL).unwrap().expect("pool");
    assert_eq!(written.data.len(), live.data.len());
    let diffs = diff_indices(&written.data, &live.data);
    assert!(
        !diffs.is_empty()
            && diffs
                .iter()
                .all(|i| POOL_VIRTUAL_QUOTE_RESERVES.contains(i)),
        "only the i128 virtual_quote_reserves may change, got {diffs:?}"
    );
    assert_eq!(read_virtual_quote_reserves(&written.data), virtual_quote);
    assert_eq!(
        Account {
            data: live.data.clone(),
            ..written
        },
        live
    );
}

/// A pool shorter than the bundled 287-byte `Pool` type cannot be decoded, so the whole override
/// is skipped with a warning: the scenario registers and plays without an error, and the pool
/// keeps every byte, `lp_supply` included. A 287-byte pool is the shortest that takes the write.
#[tokio::test(flavor = "multi_thread")]
async fn a_pool_shorter_than_its_idl_type_is_left_unchanged() {
    let live = fetch(&[AMM_POOL]).await.remove(0);
    let values = [
        ("lp_supply", serde_json::json!(9_876_543_210u64)),
        (
            "virtual_quote_reserves",
            serde_json::json!(1_000_000_000u64),
        ),
    ];

    for len in [211, 243, 244, 245, 260, 261, 270, 271, 286, 287] {
        let short = Account {
            data: live.data[..len].to_vec(),
            ..live.clone()
        };
        let (mut svm, _simnet_events_rx, _geyser_events_rx) = SurfnetSvm::default();
        svm.set_account(&AMM_POOL, short.clone()).unwrap();
        play(
            &mut svm,
            vec![pool_override("pump-amm-canonical-pool", &values, false)],
            false,
        )
        .await
        .unwrap_or_else(|e| panic!("a {len}-byte pool must not fail the scenario: {e}"));

        let after = svm.get_account(&AMM_POOL).unwrap().expect("pool");
        if len < POOL_IDL_LEN {
            assert_eq!(after, short, "the {len}-byte pool must be left unchanged");
        } else {
            let lp_supply = POOL_LP_SUPPLY_OFFSET..POOL_LP_SUPPLY_OFFSET + 8;
            let diffs = diff_indices(&after.data, &short.data);
            assert!(
                diffs
                    .iter()
                    .all(|i| lp_supply.contains(i) || POOL_VIRTUAL_QUOTE_RESERVES.contains(i)),
                "only lp_supply and virtual_quote_reserves may change, got {diffs:?}"
            );
            assert_eq!(read_u64(&after.data, POOL_LP_SUPPLY_OFFSET), 9_876_543_210);
            assert_eq!(read_virtual_quote_reserves(&after.data), 1_000_000_000);
        }
    }
}
