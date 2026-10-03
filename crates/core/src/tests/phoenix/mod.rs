use std::collections::{HashMap, HashSet};

use bytemuck::{Pod, Zeroable};
use phoenix_rise_accounts::{
    global_config::GlobalConfig,
    pda::derive_spline_collection_address,
    perp_asset_map::PerpAssetMap,
    trader::{TRADER_CAPABILITY_HOT, TraderHeader},
};
use solana_account::Account;
use solana_clock::Clock;
use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;
use surfpool_types::DEFAULT_MAINNET_RPC_URL;

use crate::{
    scenarios::protocols::phoenix_eternal::v1::{
        collateral::{index_trader_state_range, index_trader_state_ranges, trader_header},
        state_builder::{
            PHOENIX_ETERNAL_PROGRAM_ID, PHOENIX_GLOBAL_TRADER_INDEX, PHOENIX_PERP_ASSET_MAP,
            build_phoenix_collateral_scenario, phoenix_markets,
        },
    },
    surfnet::{locker::SurfnetSvmLocker, remote::SurfnetRemoteClient, svm::SurfnetSvm},
};

const RPC_URL_ENV: &str = "SURFPOOL_TEST_RPC_URL";
const PHOENIX_GLOBAL_CONFIG: Pubkey =
    Pubkey::from_str_const("2zskx2iyCvb6Stg7RBZkt1f6MrF4dpYtMG3yMvKwqtUZ");

fn client() -> SurfnetRemoteClient {
    SurfnetRemoteClient::new(
        std::env::var(RPC_URL_ENV).unwrap_or_else(|_| DEFAULT_MAINNET_RPC_URL.to_string()),
    )
}

/// Fetches the accounts in one request, so every account returned is from the same slot.
async fn fetch(addresses: &[Pubkey]) -> Vec<Account> {
    client()
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

/// Hawkeye checks it, because it reads a hot Trader's collateral from the GlobalTraderIndex and
/// its positions from the ActiveTraderBuffer, and the Trader account's copies of both can lag.
/// A margin view that fails rejects the trader, and its error is returned.
fn trader_is_eligible(locker: &SurfnetSvmLocker, graph: &PhoenixLiveGraph) -> Result<bool, String> {
    let header = trader_header(&graph.trader, graph.account(&graph.trader)).unwrap_or_else(|e| {
        panic!(
            "{} is in the GlobalTraderIndex but is not a valid Trader: {e}",
            graph.trader
        )
    });
    // The collateral stress keys off the Trader account's hot flag.
    if !header.trader_state.is_hot() {
        return Ok(false);
    }
    let margin = try_hawkeye_margin(locker, graph)?;
    Ok(margin.collateral_quote_lots > 0
        && margin.position_count > 0
        && margin.maintenance_margin_quote_lots > 0
        && margin.is_liquidatable == 0)
}

/// A zero-copy layout cannot be round-tripped against itself, so drift shows up as an
/// invariant that stops holding on live bytes.
#[tokio::test(flavor = "multi_thread")]
async fn live_accounts_satisfy_the_typed_layout_invariants() {
    let graph = phoenix_live_graph().await;

    let global = GlobalConfig::try_from_account_bytes(&graph.account(&PHOENIX_GLOBAL_CONFIG).data)
        .expect("live GlobalConfig should decode through phoenix-rise-accounts");
    assert_eq!(
        Pubkey::new_from_array(global.account_key()),
        PHOENIX_GLOBAL_CONFIG,
        "GlobalConfig stores its own address, so a moved field shows up here first"
    );
    assert_eq!(
        (
            Pubkey::new_from_array(global.perp_asset_map_key()),
            Pubkey::new_from_array(global.global_trader_index_header_key()),
        ),
        (PHOENIX_PERP_ASSET_MAP, PHOENIX_GLOBAL_TRADER_INDEX),
        "the hardcoded Phoenix singletons moved; update them to what GlobalConfig points at"
    );

    let map_account = graph.account(&graph.perp_asset_map);
    assert_eq!(map_account.owner, PHOENIX_ETERNAL_PROGRAM_ID);
    let markets = phoenix_markets(graph.perp_asset_map, map_account)
        .expect("live PerpAssetMap should decode");
    let map = PerpAssetMap::try_from_account_bytes(&map_account.data)
        .expect("live PerpAssetMap should decode through phoenix-rise-accounts");
    for (symbol, orderbook, spline) in &graph.markets {
        assert!(
            markets
                .iter()
                .any(|market| &market.symbol == symbol && market.orderbook == *orderbook),
            "{symbol} is listed with the orderbook the BBO view reads"
        );
        let entry = map
            .find_by_symbol(symbol)
            .expect("symbol lookup should decode")
            .expect("the symbol came from this map");
        assert!(
            entry
                .metadata
                .oracle_price()
                .mark_price
                .price
                .ticks
                .as_inner()
                > 0,
            "{symbol} is listed with a zero mark price, which the risk engine cannot use"
        );
        // The spline address is derived, so a change in the seeds surfaces as an account the
        // program would no longer find.
        assert_eq!(
            graph.account(spline).owner,
            PHOENIX_ETERNAL_PROGRAM_ID,
            "the derived spline collection must belong to the Eternal program"
        );
    }
}

const HAWKEYE_VIEW_MARGIN_DISCRIMINANT: [u8; 8] = [0xb2, 0x0a, 0x7c, 0xad, 0xec, 0xd2, 0x75, 0x06];
const HAWKEYE_VIEW_BBO_DISCRIMINANT: [u8; 8] = [0x37, 0x5f, 0x23, 0x2d, 0x53, 0xaf, 0x12, 0x52];
const ETERNAL_PROGRAMDATA: Pubkey =
    Pubkey::from_str_const("B5ayDaz9HegiNZqYeBtcFqfZBVSGwjB2CJgHshoSfMQg");
const HAWKEYE_PROGRAMDATA: Pubkey =
    Pubkey::from_str_const("Gv1WgG864CQqF5vedJVbpnhpRpRbTW1A7SyARzSw9B4Y");

/// The deployed bytecode, read from the upgradeable loader's ProgramData account once per test
/// process. The ELF starts 45 bytes in, past the loader's own header.
async fn deployed_program(programdata: Pubkey) -> Vec<u8> {
    static CACHE: std::sync::OnceLock<tokio::sync::Mutex<HashMap<Pubkey, Vec<u8>>>> =
        std::sync::OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().await;
    if let Some(bytes) = cache.get(&programdata) {
        return bytes.clone();
    }
    let bytes = fetch(&[programdata]).await.remove(0).data[45..].to_vec();
    cache.insert(programdata, bytes.clone());
    bytes
}

fn live_graph_cache() -> &'static tokio::sync::Mutex<Option<Result<PhoenixLiveGraph, String>>> {
    static CACHE: std::sync::OnceLock<
        tokio::sync::Mutex<Option<Result<PhoenixLiveGraph, String>>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::Mutex::new(None))
}

#[tokio::test(flavor = "multi_thread")]
async fn phoenix_state_preparation_changes_hawkeye_risk_outcomes() {
    let (collateral_locker, graph) = phoenix_behavior_locker().await;

    let account = |key: &Pubkey| {
        collateral_locker
            .with_svm_reader(|svm| svm.get_account(key))
            .unwrap()
            .unwrap()
    };
    let before_trader = account(&graph.trader);
    let before_index = account(&graph.global_trader_index);
    let header = TraderHeader::try_read_from_account_bytes(&before_trader.data).unwrap();
    assert!(
        header.trader_state.is_hot(),
        "the regression requires a hot trader"
    );
    let range = index_trader_state_range(&before_index, &header.key).unwrap();
    let before = hawkeye_margin(&collateral_locker, &graph);
    assert!(
        before.collateral_quote_lots > 0,
        "no eligible live candidate: the discovered trader has no collateral to stress"
    );
    assert!(
        before.position_count > 0,
        "no eligible live candidate: the program sees no position for the discovered trader"
    );
    assert!(
        before.maintenance_margin_quote_lots > 0,
        "no eligible live candidate: the discovered trader's positions require no margin"
    );
    assert_eq!(
        before.is_liquidatable, 0,
        "no eligible live candidate: the discovered trader is already liquidatable"
    );

    // Effective collateral moves one-for-one with collateral, whatever else it counts (uPnL,
    // funding, ...), so this target puts it at half the maintenance margin. A fixed target of 1
    // left about one live trader in four healthy.
    let maintenance =
        i64::try_from(before.maintenance_margin_quote_lots).expect("maintenance margin fits i64");
    let target =
        before.collateral_quote_lots - before.effective_collateral_quote_lots + maintenance / 2;

    let scenario =
        build_phoenix_collateral_scenario(graph.trader, &before_trader, &target.to_string())
            .unwrap();
    collateral_locker
        .register_scenario(scenario, Some(graph.clock.slot))
        .unwrap();
    assert_eq!(account(&graph.trader), before_trader);
    assert_eq!(account(&graph.global_trader_index), before_index);
    collateral_locker
        .materialize_overrides_for_slot(&None, graph.clock.slot)
        .await
        .unwrap();
    let after_collateral = hawkeye_margin(&collateral_locker, &graph);
    assert_eq!(
        after_collateral.collateral_quote_lots, target,
        "the program reads the collateral the preparation wrote"
    );
    assert!(
        after_collateral.effective_collateral_quote_lots < before.effective_collateral_quote_lots,
        "stressing collateral must lower what the risk engine can count on"
    );

    assert_eq!(
        after_collateral.is_liquidatable,
        1,
        "the stress must leave the trader liquidatable: effective collateral {} against \
         maintenance margin {}",
        after_collateral.effective_collateral_quote_lots,
        after_collateral.maintenance_margin_quote_lots
    );
    let mut expected_trader = before_trader;
    expected_trader.data[88..96].copy_from_slice(&target.to_le_bytes());
    let mut expected_index = before_index;
    expected_index.data[range.start..range.start + 8].copy_from_slice(&target.to_le_bytes());
    assert_eq!(account(&graph.trader), expected_trader);
    assert_eq!(account(&graph.global_trader_index), expected_index);

    // Whether the position liquidates depends on its side, so the cascade asserts what the
    // program reads, not the outcome.
    let (mark_locker, graph) = phoenix_behavior_locker().await;
    let (symbol, orderbook, spline) = graph.markets[0].clone();
    let trader_account = mark_locker
        .with_svm_reader(|svm| svm.get_account(&graph.trader))
        .unwrap()
        .unwrap();
    let prepared_collateral = hawkeye_margin(&mark_locker, &graph).collateral_quote_lots / 2;
    let mut cascade = build_phoenix_collateral_scenario(
        graph.trader,
        &trader_account,
        &prepared_collateral.to_string(),
    )
    .unwrap();
    let mut shock = phoenix_market_override(
        "phoenix-direct-mark-risk-shock",
        graph.perp_asset_map,
        &[("symbol", symbol.as_str()), ("target_ticks", "1")],
    );
    shock.scenario_relative_slot = 1;
    cascade.add_override(shock);
    mark_locker
        .register_scenario(cascade, Some(graph.clock.slot))
        .unwrap();
    mark_locker
        .materialize_overrides_for_slot(&None, graph.clock.slot)
        .await
        .unwrap();
    let before_mark = hawkeye_bbo_for_market(&graph, &mark_locker, orderbook, spline);
    assert_eq!(
        hawkeye_margin(&mark_locker, &graph).collateral_quote_lots,
        prepared_collateral,
        "stage 0 prepares the collateral the cascade was built with"
    );
    assert_ne!(before_mark.mark_price_ticks, 1);
    mark_locker.with_svm_writer(|svm| {
        let mut clock = graph.clock.clone();
        clock.slot += 1;
        svm.inner.set_sysvar(&clock);
    });
    mark_locker
        .materialize_overrides_for_slot(&None, graph.clock.slot + 1)
        .await
        .unwrap();
    let after_mark = hawkeye_bbo_for_market(&graph, &mark_locker, orderbook, spline);
    assert_eq!(
        after_mark.mark_price_last_updated_slot,
        graph.clock.slot + 1
    );
    assert_eq!(
        after_mark.mark_price_ticks, 1,
        "stage 1 shocks the mark the program itself reads"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn collateral_stress_follows_the_index_when_the_hot_flag_lags() {
    let (locker, graph) = phoenix_behavior_locker().await;
    let set_trader = |trader: &Account| {
        locker.with_svm_writer(|svm| svm.set_account(&graph.trader, trader.clone()).unwrap())
    };
    // A Trader refetched after it left the hot set has its HOT flag cleared while the fork's
    // index still holds its record.
    let mut lagging = locker
        .with_svm_reader(|svm| svm.get_account(&graph.trader))
        .unwrap()
        .unwrap();
    let flags = u32::from_le_bytes(lagging.data[96..100].try_into().unwrap());
    lagging.data[96..100].copy_from_slice(&(flags & !TRADER_CAPABILITY_HOT).to_le_bytes());
    set_trader(&lagging);
    let before = hawkeye_margin(&locker, &graph);
    let maintenance =
        i64::try_from(before.maintenance_margin_quote_lots).expect("maintenance margin fits i64");
    let target =
        before.collateral_quote_lots - before.effective_collateral_quote_lots + maintenance / 2;

    // The program keeps reading the index record, so the Trader account alone changes nothing.
    let mut account_only = lagging.clone();
    account_only.data[88..96].copy_from_slice(&target.to_le_bytes());
    set_trader(&account_only);
    assert_eq!(
        hawkeye_margin(&locker, &graph).collateral_quote_lots,
        before.collateral_quote_lots
    );
    set_trader(&lagging);

    let scenario =
        build_phoenix_collateral_scenario(graph.trader, &lagging, &target.to_string()).unwrap();
    locker
        .register_scenario(scenario, Some(graph.clock.slot))
        .unwrap();
    locker
        .materialize_overrides_for_slot(&None, graph.clock.slot)
        .await
        .unwrap();
    let after = hawkeye_margin(&locker, &graph);
    assert_eq!(
        after.collateral_quote_lots, target,
        "the stress lands in the index record the program reads"
    );
    assert_eq!(
        after.is_liquidatable, 1,
        "the stress must leave the trader liquidatable: effective collateral {} against \
         maintenance margin {}",
        after.effective_collateral_quote_lots, after.maintenance_margin_quote_lots
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn maintenance_margin_stress_raises_the_live_requirement() {
    let (locker, graph) = phoenix_behavior_locker().await;
    let map = PerpAssetMap::try_from_account_bytes(&graph.account(&graph.perp_asset_map).data)
        .expect("live PerpAssetMap decodes");

    // For a hot Trader the program reads positions from the ActiveTraderBuffer, which the Trader
    // account's copy can lag behind, so every market's factor is doubled rather than the one
    // the copy names. The overrides share a slot, and each reads the map the previous one wrote.
    let mut scenario = surfpool_types::Scenario::new(
        "phoenix-maintenance-margin-stress".to_string(),
        "Double every Phoenix market's maintenance factor".to_string(),
    );
    let mut doubled = Vec::new();
    for entry in map.iter() {
        let entry = entry.expect("live map entry decodes");
        // Doubled up to 100%, the most a factor can be.
        let factor = entry.metadata.risk_params().risk_factors[0]
            .saturating_mul(2)
            .min(10_000);
        scenario.add_override(phoenix_market_override(
            "phoenix-maintenance-margin-stress",
            graph.perp_asset_map,
            &[
                ("symbol", entry.symbol.as_str()),
                ("maintenance_risk_factor_bps", &factor.to_string()),
            ],
        ));
        doubled.push((entry.symbol.as_str().to_string(), factor));
    }

    let before = hawkeye_margin(&locker, &graph);
    assert!(
        before.maintenance_margin_quote_lots > 0,
        "no eligible live candidate: the discovered trader's positions require no margin"
    );
    locker
        .register_scenario(scenario, Some(graph.clock.slot))
        .unwrap();
    locker
        .materialize_overrides_for_slot(&None, graph.clock.slot)
        .await
        .unwrap();
    // A rejected override is only logged, so each market is checked for the factor it was given.
    let stressed = locker
        .with_svm_reader(|svm| svm.get_account(&graph.perp_asset_map))
        .unwrap()
        .unwrap();
    let stressed = PerpAssetMap::try_from_account_bytes(&stressed.data)
        .expect("stressed PerpAssetMap decodes");
    for (symbol, factor) in &doubled {
        let entry = stressed
            .find_by_symbol(symbol)
            .expect("symbol lookup should decode")
            .expect("the stress keeps every market listed");
        assert_eq!(
            entry.metadata.risk_params().risk_factors[0],
            *factor,
            "{symbol} takes its own doubled factor"
        );
    }
    let after = hawkeye_margin(&locker, &graph);

    assert_eq!(after.collateral_quote_lots, before.collateral_quote_lots);
    assert!(
        after.maintenance_margin_quote_lots > before.maintenance_margin_quote_lots,
        "a stricter factor must raise the maintenance margin the program computes: {} -> {}",
        before.maintenance_margin_quote_lots,
        after.maintenance_margin_quote_lots
    );
}

async fn phoenix_behavior_locker() -> (SurfnetSvmLocker, PhoenixLiveGraph) {
    let eternal_program = deployed_program(ETERNAL_PROGRAMDATA).await;
    let hawkeye_program = deployed_program(HAWKEYE_PROGRAMDATA).await;
    let graph = phoenix_live_graph().await;
    let locker = phoenix_fork(&graph, &eternal_program, &hawkeye_program);
    (locker, graph)
}

/// A fork holding the deployed programs and every graph account, at the graph's clock.
fn phoenix_fork(
    graph: &PhoenixLiveGraph,
    eternal_program: &[u8],
    hawkeye_program: &[u8],
) -> SurfnetSvmLocker {
    let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
    let locker = SurfnetSvmLocker::new(svm);
    locker.with_svm_writer(|svm_writer| {
        svm_writer.inner.set_sysvar(&graph.clock);
        svm_writer
            .inner
            .svm
            .add_program(PHOENIX_ETERNAL_PROGRAM_ID, eternal_program)
            .unwrap();
        svm_writer
            .inner
            .svm
            .add_program(HAWKEYE_PROGRAM_ID, hawkeye_program)
            .unwrap();
        for (address, account) in &graph.accounts {
            svm_writer.set_account(address, account.clone()).unwrap();
        }
    });
    locker
}

async fn phoenix_live_graph() -> PhoenixLiveGraph {
    let mut cache = live_graph_cache().lock().await;
    match cache.as_ref() {
        Some(Ok(graph)) => return graph.clone(),
        Some(Err(reason)) => panic!("{reason}"),
        None => {}
    }

    let global_account = fetch(&[PHOENIX_GLOBAL_CONFIG]).await.remove(0);
    let global = GlobalConfig::try_from_account_bytes(&global_account.data)
        .expect("live GlobalConfig decodes");
    let perp_asset_map = Pubkey::new_from_array(global.perp_asset_map_key());
    let global_trader_index = Pubkey::new_from_array(global.global_trader_index_header_key());
    let active_trader_buffer = Pubkey::new_from_array(global.active_trader_buffer_header_key());

    // One request carries both the market list and the trader index discovery walks.
    let mut discovery = fetch(&[perp_asset_map, global_trader_index]).await;
    let map_account = discovery.remove(0);
    let index_account = discovery.remove(0);
    let map =
        PerpAssetMap::try_from_account_bytes(&map_account.data).expect("live PerpAssetMap decodes");
    let mut markets = Vec::new();
    let mut addresses = vec![
        PHOENIX_GLOBAL_CONFIG,
        perp_asset_map,
        global_trader_index,
        active_trader_buffer,
    ];
    for symbol in ["SOL", "BTC"] {
        let entry = map
            .find_by_symbol(symbol)
            .expect("symbol lookup")
            .expect("live SOL/BTC market");
        let orderbook =
            Pubkey::new_from_array(entry.metadata.static_market_params().market_account);
        let spline = derive_spline_collection_address(&PHOENIX_ETERNAL_PROGRAM_ID, &orderbook);
        addresses.extend([orderbook, spline]);
        markets.push((symbol.to_string(), orderbook, spline));
    }

    let eternal_program = deployed_program(ETERNAL_PROGRAMDATA).await;
    let hawkeye_program = deployed_program(HAWKEYE_PROGRAMDATA).await;
    let clock_address = Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111");
    // Every hot Trader is a candidate, in address order, so the pick moves only when it or a
    // trader ahead of it changes.
    let mut candidates: Vec<Pubkey> = index_trader_state_ranges(&index_account)
        .expect("live GlobalTraderIndex should walk")
        .into_iter()
        .map(|(trader, _)| trader)
        .collect();
    candidates.sort_unstable();
    // Every batch re-reads the dependencies and the clock, so each fills a request to the
    // 100-account cap.
    let batch_len = 100 - addresses.len() - 1;
    let mut last_error = None;
    for batch in candidates.chunks(batch_len) {
        // Read the clock, every dependency and this batch of traders from one bank, so a trader
        // is checked, and then tested, on state the programs accept together.
        let keys: Vec<Pubkey> = addresses
            .iter()
            .chain(batch)
            .chain([&clock_address])
            .copied()
            .collect();
        let mut fetched = client()
            .get_multiple_accounts(&keys, CommitmentConfig::confirmed())
            .await
            .unwrap_or_else(|e| panic!("failed to fetch {keys:?} from mainnet: {e}"))
            .into_iter();
        let dependencies: Vec<(Pubkey, Account)> = addresses
            .iter()
            .zip(fetched.by_ref())
            .map(|(address, result)| {
                let account = result.map_account().unwrap_or_else(|_| {
                    panic!("{address} no longer exists on mainnet; the test needs a new address")
                });
                (*address, account)
            })
            .collect();
        // A trader closed since the index was read is skipped, not an error.
        let traders: Vec<(Pubkey, Account)> = batch
            .iter()
            .zip(fetched.by_ref())
            .filter_map(|(trader, result)| Some((*trader, result.map_account().ok()?)))
            .collect();
        let clock_account = fetched
            .next()
            .and_then(|result| result.map_account().ok())
            .expect("the clock is read with the graph");
        let clock: Clock = bincode::deserialize(&clock_account.data).unwrap();
        assert!(clock.slot > 0);

        let mut graph = PhoenixLiveGraph {
            clock,
            accounts: dependencies.iter().chain(&traders).cloned().collect(),
            global_trader_index,
            active_trader_buffer,
            perp_asset_map,
            trader: Pubkey::default(),
            markets: markets.clone(),
        };
        // A trader that left the index since it was listed is no longer hot.
        let indexed: HashSet<Pubkey> =
            index_trader_state_ranges(graph.account(&global_trader_index))
                .expect("live GlobalTraderIndex should walk")
                .into_iter()
                .map(|(trader, _)| trader)
                .collect();
        let locker = phoenix_fork(&graph, &eternal_program, &hawkeye_program);
        for (trader, account) in traders
            .iter()
            .filter(|(trader, _)| indexed.contains(trader))
        {
            graph.trader = *trader;
            match trader_is_eligible(&locker, &graph) {
                Ok(false) => {}
                Err(error) => last_error = Some(format!("{trader}: {error}")),
                Ok(true) => {
                    // The rest of the batch was read only to be checked.
                    graph.accounts = dependencies
                        .into_iter()
                        .chain([(*trader, account.clone())])
                        .collect();
                    *cache = Some(Ok(graph.clone()));
                    return graph;
                }
            }
        }
    }

    let reason = format!(
        "no eligible live candidate: no hot Phoenix Trader in the GlobalTraderIndex has \
         collateral, a position Hawkeye margins and a healthy account{}",
        last_error
            .map(|error| format!("; the last margin view that failed was {error}"))
            .unwrap_or_default()
    );
    *cache = Some(Err(reason.clone()));
    panic!("{reason}")
}

#[derive(Clone)]
struct PhoenixLiveGraph {
    clock: Clock,
    accounts: Vec<(Pubkey, Account)>,
    global_trader_index: Pubkey,
    active_trader_buffer: Pubkey,
    perp_asset_map: Pubkey,
    trader: Pubkey,
    /// Symbol, orderbook and spline for each market the Hawkeye BBO view reads.
    markets: Vec<(String, Pubkey, Pubkey)>,
}

impl PhoenixLiveGraph {
    fn account(&self, address: &Pubkey) -> &Account {
        self.accounts
            .iter()
            .find_map(|(key, account)| (key == address).then_some(account))
            .unwrap_or_else(|| panic!("{address} is not in the live graph"))
    }
}

fn hawkeye_view(
    locker: &SurfnetSvmLocker,
    graph: &PhoenixLiveGraph,
    discriminant: [u8; 8],
    extra_accounts: &[Pubkey],
) -> Vec<u8> {
    try_hawkeye_view(locker, graph, discriminant, extra_accounts)
        .unwrap_or_else(|e| panic!("Hawkeye view failed: {e}"))
}

/// The view's return data, or the transaction error and logs when the view fails.
fn try_hawkeye_view(
    locker: &SurfnetSvmLocker,
    graph: &PhoenixLiveGraph,
    discriminant: [u8; 8],
    extra_accounts: &[Pubkey],
) -> Result<Vec<u8>, String> {
    let payer = Keypair::new();
    let accounts = [
        PHOENIX_ETERNAL_PROGRAM_ID,
        PHOENIX_GLOBAL_CONFIG,
        graph.global_trader_index,
        graph.active_trader_buffer,
        graph.perp_asset_map,
    ]
    .iter()
    .chain(extra_accounts)
    .map(|address| AccountMeta::new_readonly(*address, false))
    .collect();
    locker.with_svm_writer(|svm| {
        svm.inner.airdrop(&payer.pubkey(), 1_000_000_000).unwrap();
        let transaction = Transaction::new_signed_with_payer(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
                Instruction {
                    program_id: HAWKEYE_PROGRAM_ID,
                    accounts,
                    data: discriminant.to_vec(),
                },
            ],
            Some(&payer.pubkey()),
            &[&payer],
            svm.inner.svm.latest_blockhash(),
        );
        svm.inner
            .send_transaction(transaction)
            .map(|meta| meta.return_data.data)
            .map_err(|failed| format!("{:?}, logs: {:?}", failed.err, failed.meta.logs))
    })
}

fn hawkeye_margin(locker: &SurfnetSvmLocker, graph: &PhoenixLiveGraph) -> HawkeyeMarginView {
    try_hawkeye_margin(locker, graph)
        .unwrap_or_else(|e| panic!("Hawkeye margin view failed for {}: {e}", graph.trader))
}

fn try_hawkeye_margin(
    locker: &SurfnetSvmLocker,
    graph: &PhoenixLiveGraph,
) -> Result<HawkeyeMarginView, String> {
    let data = try_hawkeye_view(
        locker,
        graph,
        HAWKEYE_VIEW_MARGIN_DISCRIMINANT,
        &[graph.trader],
    )?;
    let margin = bytemuck::try_pod_read_unaligned::<HawkeyeMarginView>(&data)
        .map_err(|e| format!("the margin view returned {} bytes: {e:?}", data.len()))?;
    if margin.magic != HAWKEYE_MARGIN_RETURN_MAGIC {
        return Err(format!(
            "the margin view returned magic {:#x}",
            margin.magic
        ));
    }
    Ok(margin)
}

fn hawkeye_bbo_for_market(
    graph: &PhoenixLiveGraph,
    locker: &SurfnetSvmLocker,
    orderbook: Pubkey,
    spline: Pubkey,
) -> HawkeyeBboView {
    let data = hawkeye_view(
        locker,
        graph,
        HAWKEYE_VIEW_BBO_DISCRIMINANT,
        &[orderbook, spline],
    );
    let bbo = bytemuck::pod_read_unaligned::<HawkeyeBboView>(&data);
    assert_eq!(bbo.magic, HAWKEYE_BBO_RETURN_MAGIC);
    bbo
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct HawkeyeMarginView {
    magic: u64,
    version: u16,
    position_count: u16,
    risk_state: u8,
    risk_tier: u8,
    is_liquidatable: u8,
    padding: u8,
    collateral_quote_lots: i64,
    effective_collateral_quote_lots: i64,
    free_collateral_quote_lots: i64,
    withdrawable_collateral_quote_lots: u64,
    initial_margin_quote_lots: u64,
    maintenance_margin_quote_lots: u64,
    cancel_margin_quote_lots: u64,
    backstop_margin_quote_lots: u64,
    high_risk_margin_quote_lots: u64,
    unrealized_pnl_quote_lots: i64,
    discounted_unrealized_pnl_quote_lots: i64,
    unsettled_funding_quote_lots: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct HawkeyeBboView {
    magic: u64,
    version: u16,
    flags: u8,
    padding: [u8; 5],
    best_bid_ticks: u64,
    best_ask_ticks: u64,
    mark_price_ticks: u64,
    index_price_ticks: u64,
    mark_price_last_updated_slot: u64,
    index_price_last_updated_slot: u64,
}

const HAWKEYE_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("RiSeVw3ZjNfsaXPRb4mgaqYaEEt41pNNJoDvVh7pgQj");

const HAWKEYE_MARGIN_RETURN_MAGIC: u64 = 0x955f5b9d3dff253f;

const HAWKEYE_BBO_RETURN_MAGIC: u64 = 0xefca1fa31fa74171;

fn phoenix_market_override(
    template_id: &str,
    perp_asset_map: Pubkey,
    values: &[(&str, &str)],
) -> surfpool_types::OverrideInstance {
    surfpool_types::OverrideInstance::new(
        template_id.to_string(),
        0,
        surfpool_types::AccountAddress::Pubkey(perp_asset_map.to_string()),
    )
    .with_values(
        values
            .iter()
            .map(|(field, value)| (field.to_string(), serde_json::Value::from(*value)))
            .collect(),
    )
}
