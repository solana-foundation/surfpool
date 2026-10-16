use std::collections::{HashMap, HashSet};

use phoenix_rise_accounts::{
    global_config::GlobalConfig, orderbook::Orderbook, pda::derive_spline_collection_address,
    perp_asset_map::PerpAssetMap, trader::TraderHeader,
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
    error::SurfpoolResult,
    scenarios::protocols::phoenix_eternal::v1::{
        market::{Market, MarketContext, move_market, phoenix_markets},
        state_builder::{
            Exchange, PHOENIX_GLOBAL_CONFIG, PHOENIX_PERP_ASSET_MAP, PHOENIX_PROGRAM_ID,
            keep_markets_usable, local_account, log_authority, phoenix_instruction,
            run_instructions, tests::template_addresses,
        },
        trader::{
            BboView, HAWKEYE_PROGRAM_ID, LeverState, MarginView, VIEW_BBO, VIEW_MARGIN, holdings,
            index_trader_state_ranges, tests::index_trader_state_range, trader_positions,
            trader_positions_in,
        },
    },
    surfnet::{locker::SurfnetSvmLocker, remote::SurfnetRemoteClient, svm::SurfnetSvm},
};

const RPC_URL_ENV: &str = "SURFPOOL_TEST_RPC_URL";
const LAST_RESTART_SLOT: Pubkey =
    Pubkey::from_str_const("SysvarLastRestartS1ot1111111111111111111111");
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
fn trader_is_eligible(
    locker: &SurfnetSvmLocker,
    graph: &PhoenixMainnetGraph,
) -> Result<bool, String> {
    let header = TraderHeader::try_read_from_account_bytes(&graph.account(&graph.trader).data)
        .unwrap_or_else(|e| {
            panic!(
                "{} is in the GlobalTraderIndex but is not a valid Trader: {e}",
                graph.trader
            )
        });
    if !header.trader_state.is_hot() || header.num_markets_with_splines > 0 {
        return Ok(false);
    }
    let margin = try_hawkeye_margin(locker, graph)?;
    // Hawkeye's position count includes markets with resting orders alone.
    let exchange = Exchange::from_config(graph.account(&PHOENIX_GLOBAL_CONFIG)).unwrap();
    let held = locker
        .with_svm_reader(|svm| holdings(svm, &exchange, &graph.trader))
        .map_err(|e| e.to_string())?;
    Ok(margin.collateral_quote_lots > 0
        && held.iter().any(|held| held.base_lots != 0)
        && margin.maintenance_margin_quote_lots > 0
        && margin.is_liquidatable == 0)
}

/// A zero-copy layout cannot be round-tripped against itself, so drift shows up as an
/// invariant that stops holding on mainnet bytes.
#[tokio::test(flavor = "multi_thread")]
async fn mainnet_accounts_satisfy_the_typed_layout_invariants() {
    let graph = phoenix_mainnet_graph().await;

    let global = GlobalConfig::try_from_account_bytes(&graph.account(&PHOENIX_GLOBAL_CONFIG).data)
        .expect("mainnet GlobalConfig should decode through phoenix-rise-accounts");
    assert_eq!(
        Pubkey::new_from_array(global.account_key()),
        PHOENIX_GLOBAL_CONFIG,
        "GlobalConfig stores its own address, so a moved field shows up here first"
    );
    assert_eq!(
        Pubkey::new_from_array(global.perp_asset_map_key()),
        PHOENIX_PERP_ASSET_MAP,
        "the PerpAssetMap the templates carry moved; update it to what GlobalConfig points at"
    );
    for (id, account_type, address) in template_addresses() {
        let named = match account_type {
            "PerpAssetMap" => global.perp_asset_map_key(),
            "GlobalConfiguration" => global.account_key(),
            "WithdrawQueueHeader" => global.withdraw_queue_key(),
            other => panic!("{id} carries an address for {other}, which nothing checks"),
        };
        assert_eq!(
            address,
            Pubkey::new_from_array(named),
            "{id} carries the address GlobalConfig names"
        );
    }

    let map_account = graph.account(&graph.perp_asset_map);
    assert_eq!(map_account.owner, PHOENIX_PROGRAM_ID);
    let markets = phoenix_markets(graph.perp_asset_map, map_account)
        .expect("mainnet PerpAssetMap should decode");
    let map = PerpAssetMap::try_from_account_bytes(&map_account.data)
        .expect("mainnet PerpAssetMap should decode through phoenix-rise-accounts");
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
            PHOENIX_PROGRAM_ID,
            "the derived spline collection must belong to the Eternal program"
        );
    }
}

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

fn mainnet_graph_cache() -> &'static tokio::sync::Mutex<Option<Result<PhoenixMainnetGraph, String>>>
{
    static CACHE: std::sync::OnceLock<
        tokio::sync::Mutex<Option<Result<PhoenixMainnetGraph, String>>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::Mutex::new(None))
}

#[tokio::test(flavor = "multi_thread")]
async fn scenario_keeps_the_trader_usable_as_the_clock_moves_on() {
    let later = |graph: &PhoenixMainnetGraph| {
        let mut clock = graph.clock.clone();
        clock.slot += 1_000_000;
        clock
    };

    let (unplayed, graph) = phoenix_behavior_locker().await;
    unplayed.with_svm_writer(|svm| svm.inner.set_sysvar(&later(&graph)));
    let refused = try_hawkeye_margin(&unplayed, &graph)
        .expect_err("with no Phoenix scenario played, the aged readings are refused");
    assert!(
        refused.contains("staleness or validity check failed"),
        "{refused}"
    );

    let (locker, graph) = phoenix_behavior_locker().await;
    // Any Phoenix override keeps the markets usable; this one leaves SOL's fees as they are.
    let (_, orderbook, _) = graph.markets[0].clone();
    let book = phoenix_rise_accounts::orderbook::Orderbook::try_from_account_bytes(
        &graph.account(&orderbook).data,
    )
    .unwrap()
    .header;
    play(
        &locker,
        graph.clock.slot,
        vec![phoenix_override(
            "phoenix-market-fees",
            graph.perp_asset_map,
            &[
                ("symbol", "SOL"),
                (
                    "defaultTakerFeeMicro",
                    &book.default_taker_fee_micro().to_string(),
                ),
                (
                    "defaultMakerFeeMicro",
                    &book.default_maker_fee_micro().to_string(),
                ),
            ],
        )],
    )
    .await;
    let prepared = hawkeye_margin(&locker, &graph);

    locker.with_svm_writer(|svm| svm.inner.set_sysvar(&later(&graph)));
    let kept = hawkeye_margin(&locker, &graph);
    assert_eq!(
        (
            kept.collateral_quote_lots,
            kept.effective_collateral_quote_lots,
            kept.initial_margin_quote_lots,
            kept.maintenance_margin_quote_lots,
            kept.unrealized_pnl_quote_lots,
        ),
        (
            prepared.collateral_quote_lots,
            prepared.effective_collateral_quote_lots,
            prepared.initial_margin_quote_lots,
            prepared.maintenance_margin_quote_lots,
            prepared.unrealized_pnl_quote_lots,
        ),
        "the trader is priced as when the scenario was played"
    );
}

async fn phoenix_behavior_locker() -> (SurfnetSvmLocker, PhoenixMainnetGraph) {
    let eternal_program = deployed_program(ETERNAL_PROGRAMDATA).await;
    let hawkeye_program = deployed_program(HAWKEYE_PROGRAMDATA).await;
    let graph = phoenix_mainnet_graph().await;
    let locker = phoenix_surfnet(&graph, &eternal_program, &hawkeye_program);
    // A surfnet loads this sysvar from its datasource; the bare test VM has none.
    let restart = fetch(&[LAST_RESTART_SLOT]).await.remove(0);
    locker.with_svm_writer(|svm| svm.set_account(&LAST_RESTART_SLOT, restart).unwrap());
    (locker, graph)
}

/// A local VM holding the deployed programs and every graph account, at the graph's clock.
fn phoenix_surfnet(
    graph: &PhoenixMainnetGraph,
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
            .add_program(PHOENIX_PROGRAM_ID, eternal_program)
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

async fn phoenix_mainnet_graph() -> PhoenixMainnetGraph {
    let mut cache = mainnet_graph_cache().lock().await;
    match cache.as_ref() {
        Some(Ok(graph)) => return graph.clone(),
        Some(Err(reason)) => panic!("{reason}"),
        None => {}
    }

    let global_account = fetch(&[PHOENIX_GLOBAL_CONFIG]).await.remove(0);
    let global = GlobalConfig::try_from_account_bytes(&global_account.data)
        .expect("mainnet GlobalConfig decodes");
    let perp_asset_map = Pubkey::new_from_array(global.perp_asset_map_key());
    let global_trader_index = Pubkey::new_from_array(global.global_trader_index_header_key());
    let active_trader_buffer = Pubkey::new_from_array(global.active_trader_buffer_header_key());

    // One request carries both the market list and the trader index discovery walks.
    let mut discovery = fetch(&[perp_asset_map, global_trader_index]).await;
    let map_account = discovery.remove(0);
    let index_account = discovery.remove(0);
    let map = PerpAssetMap::try_from_account_bytes(&map_account.data)
        .expect("mainnet PerpAssetMap decodes");
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
            .expect("mainnet SOL/BTC market");
        let orderbook =
            Pubkey::new_from_array(entry.metadata.static_market_params().market_account);
        let spline = derive_spline_collection_address(&PHOENIX_PROGRAM_ID, &orderbook);
        addresses.extend([orderbook, spline]);
        markets.push((symbol.to_string(), orderbook, spline));
    }

    let eternal_program = deployed_program(ETERNAL_PROGRAMDATA).await;
    let hawkeye_program = deployed_program(HAWKEYE_PROGRAMDATA).await;
    let clock_address = Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111");
    // Every hot Trader is a candidate, in address order, so the pick moves only when it or a
    // trader ahead of it changes.
    let mut candidates: Vec<Pubkey> = index_trader_state_ranges(&index_account)
        .expect("mainnet GlobalTraderIndex should walk")
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

        let mut graph = PhoenixMainnetGraph {
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
                .expect("mainnet GlobalTraderIndex should walk")
                .into_iter()
                .map(|(trader, _)| trader)
                .collect();
        let locker = phoenix_surfnet(&graph, &eternal_program, &hawkeye_program);
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
        "no eligible mainnet candidate: no hot Phoenix Trader in the GlobalTraderIndex has \
         collateral, a position Hawkeye margins and a healthy account{}",
        last_error
            .map(|error| format!("; the last margin view that failed was {error}"))
            .unwrap_or_default()
    );
    *cache = Some(Err(reason.clone()));
    panic!("{reason}")
}

#[derive(Clone)]
struct PhoenixMainnetGraph {
    clock: Clock,
    accounts: Vec<(Pubkey, Account)>,
    global_trader_index: Pubkey,
    active_trader_buffer: Pubkey,
    perp_asset_map: Pubkey,
    trader: Pubkey,
    /// Symbol, orderbook and spline for each market the Hawkeye BBO view reads.
    markets: Vec<(String, Pubkey, Pubkey)>,
}

impl PhoenixMainnetGraph {
    fn account(&self, address: &Pubkey) -> &Account {
        self.accounts
            .iter()
            .find_map(|(key, account)| (key == address).then_some(account))
            .unwrap_or_else(|| panic!("{address} is not in the mainnet graph"))
    }
}

fn hawkeye_view(
    locker: &SurfnetSvmLocker,
    graph: &PhoenixMainnetGraph,
    discriminant: [u8; 8],
    extra_accounts: &[Pubkey],
) -> Vec<u8> {
    try_hawkeye_view(locker, graph, discriminant, extra_accounts)
        .unwrap_or_else(|e| panic!("Hawkeye view failed: {e}"))
}

/// The view's return data, or the transaction error and logs when the view fails.
fn try_hawkeye_view(
    locker: &SurfnetSvmLocker,
    graph: &PhoenixMainnetGraph,
    discriminant: [u8; 8],
    extra_accounts: &[Pubkey],
) -> Result<Vec<u8>, String> {
    let payer = Keypair::new();
    let accounts = [
        PHOENIX_PROGRAM_ID,
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

fn hawkeye_margin(locker: &SurfnetSvmLocker, graph: &PhoenixMainnetGraph) -> MarginView {
    try_hawkeye_margin(locker, graph)
        .unwrap_or_else(|e| panic!("Hawkeye margin view failed for {}: {e}", graph.trader))
}

fn try_hawkeye_margin(
    locker: &SurfnetSvmLocker,
    graph: &PhoenixMainnetGraph,
) -> Result<MarginView, String> {
    let data = try_hawkeye_view(locker, graph, VIEW_MARGIN, &[graph.trader])?;
    MarginView::from_return_data(&data).map_err(|e| e.to_string())
}

fn hawkeye_bbo_for_market(
    graph: &PhoenixMainnetGraph,
    locker: &SurfnetSvmLocker,
    orderbook: Pubkey,
    spline: Pubkey,
) -> BboView {
    let data = hawkeye_view(locker, graph, VIEW_BBO, &[orderbook, spline]);
    BboView::from_return_data(&data).unwrap()
}

fn phoenix_override(
    template_id: &str,
    account: Pubkey,
    values: &[(&str, &str)],
) -> surfpool_types::OverrideInstance {
    surfpool_types::OverrideInstance::new(
        template_id.to_string(),
        0,
        surfpool_types::AccountAddress::Pubkey(account.to_string()),
    )
    .with_values(
        values
            .iter()
            .map(|(field, value)| (field.to_string(), serde_json::Value::from(*value)))
            .collect(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_market_move_reprices_the_oracles_and_the_book_together() {
    let (locker, graph) = phoenix_behavior_locker().await;
    let remote = Some((client(), CommitmentConfig::confirmed()));

    for (symbol, orderbook, spline) in graph.markets.clone() {
        let before = hawkeye_bbo_for_market(&graph, &locker, orderbook, spline);
        let target = before.mark_price_ticks * 93 / 100;
        let map_before = locker
            .with_svm_reader(|svm| svm.get_account(&graph.perp_asset_map))
            .unwrap();

        let writes = {
            let mut svm = locker.0.write().await;
            move_market(&mut svm, &remote, &symbol, target)
                .await
                .unwrap_or_else(|e| panic!("{symbol}: {e}"))
        };
        assert_eq!(
            locker
                .with_svm_reader(|svm| svm.get_account(&graph.perp_asset_map))
                .unwrap(),
            map_before,
            "{symbol}: the move runs on a copy of the local VM"
        );
        let written: Vec<Pubkey> = writes.iter().map(|(pubkey, _)| *pubkey).collect();
        assert!(
            written.contains(&graph.perp_asset_map) && written.contains(&spline),
            "{symbol}: the oracles and the splines move together, wrote {written:?}"
        );
        locker.with_svm_writer(|svm| {
            for (pubkey, account) in writes {
                svm.set_account(&pubkey, account).unwrap();
            }
        });

        let after = hawkeye_bbo_for_market(&graph, &locker, orderbook, spline);
        assert_eq!(
            after.index_price_ticks, target,
            "{symbol}: the index is the oracle reports"
        );
        // The mark also weighs in the book, which now quotes around the target.
        assert!(
            after.mark_price_ticks.abs_diff(target) <= (target / 1_000).max(2),
            "{symbol}: the mark follows the oracles and the book to the target"
        );
        assert!(
            after.best_bid_ticks < after.best_ask_ticks,
            "{symbol}: resting orders the move crossed are matched"
        );
        let band = target / 100;
        assert!(
            after.best_bid_ticks + band >= target && after.best_ask_ticks <= target + band,
            "{symbol}: the makers quote around the new price"
        );
    }
}

/// The oracle price template moves only the oracle readings, to the ticks given or by a percentage
/// of the mark; the book and the makers' quotes stay as they were.
#[tokio::test(flavor = "multi_thread")]
async fn an_oracle_price_override_moves_the_oracles_and_leaves_the_book() {
    let (locker, graph) = phoenix_behavior_locker().await;
    let remote = Some((client(), CommitmentConfig::confirmed()));
    // The maintenance every Phoenix override runs first, so the book and splines compared below
    // change only through the template.
    keep_markets_usable(&mut *locker.0.write().await, &remote, graph.clock.slot).await;
    let account = |address| locker.with_svm_reader(|svm| local_account(svm, &address).unwrap());

    for (symbol, orderbook, spline) in graph.markets.clone() {
        let before = hawkeye_bbo_for_market(&graph, &locker, orderbook, spline);
        let (book, splines) = (account(orderbook), account(spline));
        let target = before.mark_price_ticks * 93 / 100;
        let ticks = [
            ("symbol", symbol.clone()),
            ("target_ticks", target.to_string()),
        ];
        configure(
            &locker,
            "phoenix-oracle-price",
            graph.perp_asset_map,
            &ticks,
        )
        .await
        .unwrap_or_else(|e| panic!("{symbol}: {e}"));

        let after = hawkeye_bbo_for_market(&graph, &locker, orderbook, spline);
        assert_eq!(
            after.index_price_ticks, target,
            "{symbol}: the oracles report the target"
        );
        assert_eq!(
            (after.best_bid_ticks, after.best_ask_ticks),
            (before.best_bid_ticks, before.best_ask_ticks),
            "{symbol}: the book keeps its prices"
        );
        assert!(
            account(orderbook) == book && account(spline) == splines,
            "{symbol}: the order book and the makers' splines are not written"
        );

        let map = account(graph.perp_asset_map);
        let mark = Market::find(&graph.perp_asset_map, &map.data, &symbol)
            .unwrap()
            .mark_ticks;
        let percent = [
            ("symbol", symbol.clone()),
            ("change_percent", "-2.5".to_string()),
        ];
        configure(
            &locker,
            "phoenix-oracle-price",
            graph.perp_asset_map,
            &percent,
        )
        .await
        .unwrap_or_else(|e| panic!("{symbol}: {e}"));
        // Within a tick: the template rounds a float.
        let index = hawkeye_bbo_for_market(&graph, &locker, orderbook, spline).index_price_ticks;
        assert!(
            index.abs_diff(mark * 975 / 1_000) <= 1,
            "{symbol}: a percentage is taken from the mark, {index} from {mark}"
        );
    }
}

/// One uncross crank matches at most 64 crossed orders, so a move past more resting bids than that
/// needs several before the book is uncrossed.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_past_more_resting_bids_than_one_crank_matches_uncrosses_the_book() {
    let (locker, graph) = phoenix_behavior_locker().await;
    let remote = Some((client(), CommitmentConfig::confirmed()));
    for (symbol, orderbook, spline) in graph.markets.clone() {
        let bids: Vec<u64> = Orderbook::try_from_account_bytes(&graph.account(&orderbook).data)
            .unwrap()
            .bid_orders()
            .map(|bid| bid.price_in_ticks().as_inner())
            .collect();
        // Just below the 81st best bid, at least 81 resting bids cross the re-centred splines.
        let Some(target) = bids.get(80).map(|price| price - 1) else {
            continue;
        };
        let writes = {
            let mut svm = locker.0.write().await;
            move_market(&mut svm, &remote, &symbol, target)
                .await
                .unwrap_or_else(|e| panic!("{symbol}: {e}"))
        };
        apply(&locker, writes).await;
        let after = hawkeye_bbo_for_market(&graph, &locker, orderbook, spline);
        assert!(
            after.best_bid_ticks < after.best_ask_ticks,
            "{symbol}: the book is uncrossed at {target} ticks: {after:?}"
        );
        let book = locker
            .with_svm_reader(|svm| svm.get_account(&orderbook))
            .unwrap()
            .unwrap();
        let crossed = Orderbook::try_from_account_bytes(&book.data)
            .unwrap()
            .bid_orders()
            .filter(|bid| bid.price_in_ticks().as_inner() >= target)
            .count();
        assert_eq!(
            crossed, 0,
            "{symbol}: no resting bid is left at or above {target}"
        );
        return;
    }
    panic!(
        "no eligible mainnet candidate: no book of {:?} rests 81 bids",
        graph.markets
    );
}

/// A one-lot market buy by `trader` in `symbol`, run on a copy of the test VM; it shows whether
/// Phoenix lets the trader place market orders there.
async fn buy_one_lot(
    locker: &SurfnetSvmLocker,
    trader: Pubkey,
    symbol: &str,
) -> SurfpoolResult<Vec<(Pubkey, Account)>> {
    let remote = Some((client(), CommitmentConfig::confirmed()));
    let mut svm = locker.0.write().await;
    let market = MarketContext::load(&mut svm, &remote, symbol).await?;
    let header = TraderHeader::try_read_from_account_bytes(&local_account(&svm, &trader)?.data)
        .expect("a Phoenix Trader");
    let client_order_id = [0_u8; 16];
    let order = phoenix_instruction(
        "PlaceMarketOrder",
        &[
            ("phoenixProgram", PHOENIX_PROGRAM_ID),
            ("phoenixLogAuthority", log_authority()),
            ("globalConfiguration", PHOENIX_GLOBAL_CONFIG),
            ("traderWallet", Pubkey::new_from_array(header.authority)),
            ("traderAccount", trader),
            ("perpAssetMap", market.exchange.perp_asset_map),
            ("globalTraderIndex", market.exchange.global_trader_index),
            ("activeTraderBuffer", market.exchange.active_trader_buffer),
            ("orderbook", market.market.orderbook),
            ("splines", market.market.splines),
        ],
        &serde_json::json!({
            "orderPacket": {
                "kind": {
                    "ImmediateOrCancel": {
                        "side": { "Bid": null },
                        "priceInTicks": null,
                        "numBaseLots": { "inner": 1 },
                        "numQuoteLots": null,
                        "minBaseLotsToFill": { "inner": 0 },
                        "minQuoteLotsToFill": { "inner": 0 },
                        "selfTradeBehavior": { "DecrementTake": null },
                        "matchLimit": null,
                        "clientOrderId": client_order_id,
                        "lastValidSlot": null,
                        "orderFlags": { "flags": 0 },
                        "cancelExisting": false,
                    }
                }
            }
        }),
    )?;
    run_instructions(&svm, &[order])
}

/// Plays a scenario of `overrides`, in this order, at `slot` through the materializer.
async fn play(
    locker: &SurfnetSvmLocker,
    slot: u64,
    overrides: Vec<surfpool_types::OverrideInstance>,
) {
    let mut scenario = surfpool_types::Scenario::new("play".to_string(), "play".to_string());
    for instance in overrides {
        scenario.add_override(instance);
    }
    locker.register_scenario(scenario, Some(slot)).unwrap();
    locker
        .materialize_overrides_for_slot(&Some((client(), CommitmentConfig::confirmed())), slot)
        .await
        .unwrap();
}

/// Runs a template through the Phoenix dispatch the materializer calls and applies its writes,
/// returning the program's refusal when there is one.
async fn configure(
    locker: &SurfnetSvmLocker,
    template_id: &str,
    account: Pubkey,
    values: &[(&str, String)],
) -> Result<(), String> {
    let values: std::collections::HashMap<String, serde_json::Value> = values
        .iter()
        .map(|(field, value)| (field.to_string(), serde_json::json!(value)))
        .collect();
    let mut svm = locker.0.write().await;
    let target = svm.inner.get_account(&account).unwrap().unwrap_or_default();
    let writes =
        crate::scenarios::protocols::phoenix_eternal::v1::state_builder::prepare_phoenix_override(
            &mut svm,
            template_id,
            &account,
            &target,
            &values,
            &Some((client(), CommitmentConfig::confirmed())),
            0,
        )
        .await
        .map_err(|e| format!("{template_id}: {e}"))?
        .unwrap_or_default();
    for (pubkey, written) in writes {
        svm.set_account(&pubkey, written).unwrap();
    }
    Ok(())
}

fn sol(field: &'static str, value: String) -> Vec<(&'static str, String)> {
    vec![("symbol", "SOL".to_string()), (field, value)]
}

fn sol_metadata(
    locker: &SurfnetSvmLocker,
    graph: &PhoenixMainnetGraph,
) -> phoenix_rise_accounts::perp_asset_map::PerpAssetMetadata {
    let map = locker
        .with_svm_reader(|svm| svm.get_account(&graph.perp_asset_map))
        .unwrap()
        .unwrap();
    PerpAssetMap::try_from_account_bytes(&map.data)
        .unwrap()
        .find_by_symbol("SOL")
        .unwrap()
        .unwrap()
        .metadata
}

#[tokio::test(flavor = "multi_thread")]
async fn market_and_exchange_configuration_reaches_the_accounts_phoenix_reads() {
    let (locker, graph) = phoenix_behavior_locker().await;
    let (_, orderbook, _) = graph.markets[0].clone();
    let before = sol_metadata(&locker, &graph);
    let slot = graph.clock.slot;

    // Studio's risk factor preset sends only the maintenance factor; the others, left out or
    // empty, keep their values.
    play(
        &locker,
        slot,
        vec![phoenix_override(
            "phoenix-market-risk-factors",
            graph.perp_asset_map,
            &[
                ("symbol", "SOL"),
                ("maintenanceRiskFactor", "7000"),
                ("backstopRiskFactor", ""),
            ],
        )],
    )
    .await;
    let [_, backstop, high_risk] = before.risk_params().risk_factors;
    assert_eq!(
        sol_metadata(&locker, &graph).risk_params().risk_factors,
        [7000, backstop, high_risk],
        "the risk authority set SOL's factors"
    );

    configure(
        &locker,
        "phoenix-market-cancel-risk-factor",
        graph.perp_asset_map,
        &sol("cancelOrderRiskFactor", "8000".into()),
    )
    .await
    .unwrap();
    assert_eq!(
        sol_metadata(&locker, &graph)
            .risk_params()
            .cancel_order_risk_factor,
        8000
    );

    configure(
        &locker,
        "phoenix-market-max-liquidation-size",
        graph.perp_asset_map,
        &sol("maxLiquidationSize", "123456".into()),
    )
    .await
    .unwrap();
    assert_eq!(
        sol_metadata(&locker, &graph)
            .risk_params()
            .max_liquidation_size
            .as_inner(),
        123456
    );

    let cap = before.open_interest_params().open_interest_cap.as_inner() * 2;
    configure(
        &locker,
        "phoenix-market-open-interest-cap",
        graph.perp_asset_map,
        &sol("openInterestCap", cap.to_string()),
    )
    .await
    .unwrap();
    assert_eq!(
        sol_metadata(&locker, &graph)
            .open_interest_params()
            .open_interest_cap
            .as_inner(),
        cap
    );

    let max_rate = before.funding_accumulator().max_funding_rate.as_inner() / 2;
    configure(
        &locker,
        "phoenix-market-funding",
        graph.perp_asset_map,
        &sol("maxFundingRate", max_rate.to_string()),
    )
    .await
    .unwrap();
    let funding = *sol_metadata(&locker, &graph).funding_accumulator();
    assert_eq!(funding.max_funding_rate.as_inner(), max_rate);
    assert_eq!(
        funding.funding_period_seconds,
        before.funding_accumulator().funding_period_seconds,
        "a funding field left out keeps its value"
    );

    configure(
        &locker,
        "phoenix-market-fees",
        graph.perp_asset_map,
        &[
            ("symbol", "SOL".to_string()),
            ("defaultTakerFeeMicro", "1000".to_string()),
            ("defaultMakerFeeMicro", "-50".to_string()),
        ],
    )
    .await
    .unwrap();
    let book = locker
        .with_svm_reader(|svm| svm.get_account(&orderbook))
        .unwrap()
        .unwrap();
    let header = phoenix_rise_accounts::orderbook::Orderbook::try_from_account_bytes(&book.data)
        .unwrap()
        .header;
    assert_eq!(
        (
            header.default_taker_fee_micro(),
            header.default_maker_fee_micro()
        ),
        (1000, -50)
    );

    let global =
        GlobalConfig::try_from_account_bytes(&graph.account(&PHOENIX_GLOBAL_CONFIG).data).unwrap();
    let queue = Pubkey::new_from_array(global.withdraw_queue_key());
    configure(
        &locker,
        "phoenix-withdraw-limits",
        queue,
        &[
            ("maxBudget", "1000000000".to_string()),
            ("replenishAmountPerSlot", "0".to_string()),
        ],
    )
    .await
    .unwrap();
    configure(
        &locker,
        "phoenix-withdraw-parameters",
        queue,
        &[
            ("depositCooldownPeriodInSlots", "300".to_string()),
            ("withdrawalFee", "1000000".to_string()),
            ("enqueueingFee", "2000000".to_string()),
        ],
    )
    .await
    .unwrap();
    let queue_account = locker
        .with_svm_reader(|svm| svm.get_account(&queue))
        .unwrap()
        .unwrap();
    let queue_header =
        phoenix_rise_accounts::withdraw_queue::WithdrawQueueHeader::try_from_account_bytes(
            &queue_account.data,
        )
        .unwrap();
    assert_eq!(
        (
            queue_header.withdraw_throttle().max_budget().as_inner(),
            queue_header
                .withdraw_throttle()
                .replenish_amount_per_slot()
                .as_inner(),
            queue_header.withdrawal_fee().as_inner(),
            queue_header.enqueueing_fee().as_inner(),
        ),
        (1_000_000_000, 0, 1_000_000, 2_000_000)
    );
    let global_data = || {
        locker
            .with_svm_reader(|svm| svm.get_account(&PHOENIX_GLOBAL_CONFIG))
            .unwrap()
            .unwrap()
            .data
    };
    assert_eq!(
        GlobalConfig::try_from_account_bytes(&global_data())
            .unwrap()
            .deposit_cooldown_period_in_slots(),
        300
    );

    play(
        &locker,
        slot + 1,
        vec![phoenix_override(
            "phoenix-market-status",
            graph.perp_asset_map,
            &[("symbol", "SOL"), ("nextMarketStatus", "Paused")],
        )],
    )
    .await;
    let refused = buy_one_lot(&locker, graph.trader, "SOL")
        .await
        .expect_err("a paused SOL market refuses a market order")
        .to_string();
    assert!(
        refused.contains("Market status Paused does not satisfy"),
        "{refused}"
    );

    let global_before = global_data();
    play(
        &locker,
        slot + 2,
        vec![phoenix_override(
            "phoenix-exchange-status",
            PHOENIX_GLOBAL_CONFIG,
            &[("maintenance", "true")],
        )],
    )
    .await;
    assert_ne!(
        global_data(),
        global_before,
        "the root authority switched a status flag"
    );
}

/// Sends a keeper's liquidation of `trader`'s position in the market of `context`, as much as the
/// market allows in one go and at most 10% through the mark, like the keepers on mainnet. It is a
/// real transaction, signed by a funded keeper, with signatures checked as on any surfnet.
async fn send_liquidation(
    locker: &SurfnetSvmLocker,
    context: &MarketContext,
    trader: Pubkey,
    long: bool,
) -> Result<(), String> {
    let keeper = Keypair::new();
    let mark = context.market.mark_ticks;
    let limit = if long {
        mark - mark / 10
    } else {
        mark + mark / 10
    };
    let mint = Pubkey::new_from_array(context.exchange.config.canonical_token_mint_key());
    let liquidation = phoenix_instruction(
        "LiquidateViaMarketOrder",
        &[
            ("phoenixProgram", PHOENIX_PROGRAM_ID),
            ("phoenixLogAuthority", log_authority()),
            ("globalConfiguration", PHOENIX_GLOBAL_CONFIG),
            ("liquidatorWallet", keeper.pubkey()),
            ("liquidatedTrader", trader),
            ("traderAccount", trader),
            ("perpAssetMap", context.exchange.perp_asset_map),
            ("globalTraderIndex", context.exchange.global_trader_index),
            ("activeTraderBuffer", context.exchange.active_trader_buffer),
            ("orderbook", context.market.orderbook),
            ("splines", context.market.splines),
        ],
        &serde_json::json!({
            "params": {
                "assetMint": mint.to_string(),
                "liquidationSize": { "inner": context.market.max_liquidation_size },
                "liquidationPrice": { "inner": limit },
                "fillOrKill": false,
            }
        }),
    )
    .unwrap();
    locker.with_svm_writer(|svm| {
        svm.inner.airdrop(&keeper.pubkey(), 1_000_000_000).unwrap();
        let transaction = Transaction::new_signed_with_payer(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
                liquidation,
            ],
            Some(&keeper.pubkey()),
            &[&keeper],
            svm.latest_blockhash(),
        );
        svm.send_transaction(transaction.into(), false, true)
            .map(|_| ())
            .map_err(|failed| format!("{:?}, logs: {:?}", failed.err, failed.meta.logs))
    })
}

async fn apply(locker: &SurfnetSvmLocker, writes: Vec<(Pubkey, Account)>) {
    locker.with_svm_writer(|svm| {
        for (pubkey, account) in writes {
            svm.set_account(&pubkey, account).unwrap();
        }
    });
}

/// The symbols of the markets where Hawkeye reports a position of `trader`, with its base lots.
async fn held_positions(
    locker: &SurfnetSvmLocker,
    remote: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    trader: &Pubkey,
) -> Vec<(String, i64)> {
    let mut svm = locker.0.write().await;
    let exchange = Exchange::load(&mut svm, remote).await.unwrap();
    holdings(&svm, &exchange, trader)
        .unwrap()
        .into_iter()
        .filter(|held| held.base_lots != 0)
        .map(|held| (held.market.symbol, held.base_lots))
        .collect()
}

/// The base lots of `trader`'s position in `symbol`, as Hawkeye reports it; 0 when it has none.
async fn position_lots(
    locker: &SurfnetSvmLocker,
    remote: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    trader: &Pubkey,
    symbol: &str,
) -> i64 {
    held_positions(locker, remote, trader)
        .await
        .into_iter()
        .find_map(|(held, lots)| (held == symbol).then_some(lots))
        .unwrap_or(0)
}

/// The value `list_phoenix_trader_positions` reports lands the collateral lever exactly on its target:
/// for a hot trader, whose orders the lever cancels first, and again for the same trader once it is
/// cold. Both read the same `LeverState`, which keeps them from drifting apart.
#[tokio::test(flavor = "multi_thread")]
async fn the_reported_collateral_lands_the_lever_on_its_target_hot_and_cold() {
    let remote = Some((client(), CommitmentConfig::confirmed()));
    let lever = |trader, collateral: i64| {
        let value = collateral.to_string();
        let values = [("traderState.quoteLotCollateral", value.as_str())];
        phoenix_override("phoenix-trader-collateral", trader, &values)
    };
    // A keeper liquidates right after Play and once longer than any maker's quotes last on their own.
    for delay in [0, 500] {
        let (locker, graph) = phoenix_behavior_locker().await;
        let trader = graph.trader;
        // One liquidation's worth while hot, then every position's worth once cold.
        for (hot, every_position) in [(true, false), (false, true)] {
            let (view, field, listed) = {
                let mut svm = locker.0.write().await;
                let (mut state, refusal) =
                    LeverState::load(&mut svm, &remote, trader).await.unwrap();
                assert!(refusal.is_none(), "{trader}: {refusal:?}");
                let (view, field) = state.reading(trader).unwrap();
                (view, field, state.hot)
            };
            assert_eq!(
                listed, hot,
                "{trader}: hot at first, cold after the first write"
            );
            let (lower, upper) = if every_position {
                (
                    view.high_risk_margin_quote_lots,
                    view.backstop_margin_quote_lots,
                )
            } else {
                (
                    view.backstop_margin_quote_lots,
                    view.maintenance_margin_quote_lots,
                )
            };
            let target = ((lower + upper) / 2) as i64;
            let collateral = field - view.effective_collateral_quote_lots + target;
            play(&locker, graph.clock.slot, vec![lever(trader, collateral)]).await;
            let prepared = hawkeye_margin(&locker, &graph);
            assert_eq!(
                prepared.effective_collateral_quote_lots, target,
                "{trader} (hot: {hot}): Play lands on the reported target"
            );
            assert_eq!(
                prepared.is_liquidatable, 1,
                "{trader}: liquidatable after Play"
            );
        }

        let index = locker.with_svm_reader(|svm| local_account(svm, &graph.global_trader_index));
        assert!(index_trader_state_range(&index.unwrap(), &trader.to_bytes()).is_err());

        let mut clock = graph.clock.clone();
        clock.slot += delay;
        clock.unix_timestamp += (delay * 2 / 5) as i64;
        locker.with_svm_writer(|svm| svm.inner.set_sysvar(&clock));
        let (symbol, lots) = held_positions(&locker, &remote, &trader).await.remove(0);
        let context = MarketContext::load(&mut *locker.0.write().await, &remote, &symbol)
            .await
            .unwrap();
        send_liquidation(&locker, &context, trader, lots > 0)
            .await
            .unwrap_or_else(|e| panic!("{trader} +{delay}: the keeper's liquidation failed: {e}"));
        let left = position_lots(&locker, &remote, &trader, &symbol).await;
        assert!(
            left.abs() < lots.abs(),
            "{trader} +{delay}: {symbol} {lots} -> {left}"
        );
    }
}

/// The margin the tool predicts after a move is the margin Phoenix computes once the move plays.
#[tokio::test(flavor = "multi_thread")]
async fn the_margin_after_moves_is_the_margin_once_the_moves_play() {
    let remote = Some((client(), CommitmentConfig::confirmed()));
    let graph = phoenix_mainnet_graph().await;
    let eternal = deployed_program(ETERNAL_PROGRAMDATA).await;
    let hawkeye = deployed_program(HAWKEYE_PROGRAMDATA).await;
    let restart = fetch(&[LAST_RESTART_SLOT]).await.remove(0);
    let fresh = || {
        let locker = phoenix_surfnet(&graph, &eternal, &hawkeye);
        locker.with_svm_writer(|svm| {
            svm.set_account(&LAST_RESTART_SLOT, restart.clone())
                .unwrap()
        });
        locker
    };
    let margin_of = |locker: &SurfnetSvmLocker, trader: Pubkey| {
        MarginView::from_return_data(&hawkeye_view(locker, &graph, VIEW_MARGIN, &[trader])).unwrap()
    };
    let hot = index_trader_state_ranges(graph.account(&graph.global_trader_index)).unwrap();

    // Two landed liquidations for each move size, or at least one when the index runs out.
    let percents = [2_u64, 10];
    let mut landed = [0_usize; 2];
    for (trader, _) in hot.into_iter().take(40) {
        for (size, percent) in percents.into_iter().enumerate() {
            if landed[size] >= 2 {
                continue;
            }
            let locker = fresh();
            let held = {
                let mut svm = locker.0.write().await;
                trader_positions_in(&mut svm, &remote, trader, &[]).await
            };
            // Traders and books outside the graph load later than its trader index and can
            // disagree with it.
            let held = match held {
                Ok(held) => held,
                Err(e) => {
                    eprintln!("{trader}: skipped, {e}");
                    continue;
                }
            };
            if !held.hot
                || held.refusal.is_some()
                || held.positions.is_empty()
                || held.margin.is_liquidatable != 0
            {
                continue;
            }
            let map = locker.with_svm_reader(|svm| local_account(svm, &graph.perp_asset_map));
            let markets = phoenix_markets(graph.perp_asset_map, &map.unwrap()).unwrap();
            // Every market moves against the trader's position there.
            let moves: Vec<(String, u64)> = held
                .positions
                .iter()
                .map(|position| {
                    let mark = markets
                        .iter()
                        .find(|market| market.symbol == position.symbol)
                        .unwrap()
                        .mark_ticks;
                    let factor = if position.base_lots > 0 {
                        100 - percent
                    } else {
                        100 + percent
                    };
                    (position.symbol.clone(), mark * factor / 100)
                })
                .collect();
            let predicted = {
                let mut svm = locker.0.write().await;
                trader_positions_in(&mut svm, &remote, trader, &moves)
                    .await
                    .unwrap()
            };
            let now = predicted.margin;
            let after = predicted.margin_after_moves.unwrap();
            let target = ((after.backstop_margin_quote_lots + after.maintenance_margin_quote_lots)
                / 2) as i64;
            let collateral =
                predicted.quote_lot_collateral - after.effective_collateral_quote_lots + target;
            // Studio's checks: the move pushes the trader toward liquidation, and the collateral
            // keeps it healthy until then.
            let cushion = |view: &MarginView| {
                view.effective_collateral_quote_lots - view.maintenance_margin_quote_lots as i64
            };
            let healthy_until_the_move = now.effective_collateral_quote_lots + collateral
                - predicted.quote_lot_collateral
                > now.maintenance_margin_quote_lots as i64;
            if cushion(&after) >= cushion(&now) || !healthy_until_the_move {
                continue;
            }

            let value = collateral.to_string();
            let lever = phoenix_override(
                "phoenix-trader-collateral",
                trader,
                &[("traderState.quoteLotCollateral", value.as_str())],
            );
            play(&locker, graph.clock.slot, vec![lever]).await;
            assert_eq!(
                margin_of(&locker, trader).is_liquidatable,
                0,
                "{trader} {percent}% against: healthy until the move"
            );
            let mut clock = graph.clock.clone();
            clock.slot += 1;
            locker.with_svm_writer(|svm| svm.inner.set_sysvar(&clock));
            let market_moves = moves
                .iter()
                .map(|(symbol, ticks)| {
                    let ticks = ticks.to_string();
                    let values = [
                        ("symbol", symbol.as_str()),
                        ("target_ticks", ticks.as_str()),
                    ];
                    phoenix_override("phoenix-market-move", graph.perp_asset_map, &values)
                })
                .collect();
            play(&locker, clock.slot, market_moves).await;

            let played = margin_of(&locker, trader);
            let effective = played.effective_collateral_quote_lots;
            let band = after.maintenance_margin_quote_lots - after.backstop_margin_quote_lots;
            assert_eq!(
                played.is_liquidatable, 1,
                "{trader} {percent}% against: liquidatable after the move"
            );
            assert!(
                effective > played.backstop_margin_quote_lots as i64
                    && effective < played.maintenance_margin_quote_lots as i64,
                "{trader} {percent}% against: {effective} is between backstop and maintenance"
            );
            assert!(
                effective.abs_diff(target) <= band / 20,
                "{trader} {percent}% against: {effective} is within 5% of the band from {target}"
            );
            let biggest = held
                .positions
                .iter()
                .max_by_key(|position| position.base_lots.unsigned_abs())
                .unwrap();
            let context =
                MarketContext::load(&mut *locker.0.write().await, &remote, &biggest.symbol)
                    .await
                    .unwrap();
            send_liquidation(&locker, &context, trader, biggest.base_lots > 0)
                .await
                .unwrap_or_else(|e| {
                    panic!("{trader} {percent}% against: the keeper's liquidation failed: {e}")
                });
            let left = position_lots(&locker, &remote, &trader, &biggest.symbol).await;
            assert!(
                left.abs() < biggest.base_lots.abs(),
                "{trader}: {} {left}",
                biggest.symbol
            );
            landed[size] += 1;
        }
        if landed.iter().all(|count| *count >= 2) {
            break;
        }
    }
    assert!(
        landed.iter().all(|count| *count >= 1),
        "no eligible live candidate: liquidations landed per move size {percents:?}: {landed:?}"
    );
}

/// Keeping the markets usable, which every Phoenix override does first, keeps every maker's spline
/// regions quoting while the clock runs, a moved book's too.
#[tokio::test(flavor = "multi_thread")]
async fn a_thin_book_still_quotes_500_slots_later() {
    // A stock market, which only makers' splines keep quoting.
    let usable = |upstream_splines: bool| async move {
        let (locker, graph) = phoenix_behavior_locker().await;
        let markets = phoenix_markets(graph.perp_asset_map, graph.account(&graph.perp_asset_map));
        let market = markets
            .unwrap()
            .into_iter()
            .find(|market| ["AAPL", "NVDA", "TSLA", "GOOGL"].contains(&market.symbol.as_str()))
            .expect("Phoenix lists a stock market");
        let remote = Some((client(), CommitmentConfig::confirmed()));
        let mut svm = locker.0.write().await;
        keep_markets_usable(&mut svm, &remote, graph.clock.slot).await;
        MarketContext::load(&mut svm, &remote, &market.symbol)
            .await
            .unwrap();
        if upstream_splines {
            let upstream = fetch(&[market.splines]).await.remove(0);
            svm.set_account(&market.splines, upstream).unwrap();
        }
        drop(svm);
        (locker, graph, market)
    };
    let top = |locker: &SurfnetSvmLocker, graph: &PhoenixMainnetGraph, market: &Market| {
        let view = hawkeye_bbo_for_market(graph, locker, market.orderbook, market.splines);
        (view.best_bid_ticks, view.best_ask_ticks)
    };
    let later = |locker: &SurfnetSvmLocker, graph: &PhoenixMainnetGraph, slots: u64| {
        let mut clock = graph.clock.clone();
        clock.slot += slots;
        clock.unix_timestamp += (slots * 2 / 5) as i64;
        locker.with_svm_writer(|svm| svm.inner.set_sysvar(&clock));
        clock.slot
    };

    let (kept, graph, market) = usable(false).await;
    let (lapsing, _, _) = usable(true).await;
    let now = top(&kept, &graph, &market);
    let slot = later(&kept, &graph, 500);
    later(&lapsing, &graph, 500);
    let lapsed = top(&lapsing, &graph, &market);
    assert_eq!(
        top(&kept, &graph, &market),
        now,
        "{}: the makers still quote",
        market.symbol
    );
    assert_ne!(
        lapsed, now,
        "{}: mainnet's own lifespans lapse",
        market.symbol
    );

    let target = (now.0 + now.1) / 2 * 98 / 100;
    let ticks = target.to_string();
    let values = [("symbol", market.symbol.as_str()), ("target_ticks", &ticks)];
    let moving = phoenix_override("phoenix-market-move", graph.perp_asset_map, &values);
    play(&kept, slot, vec![moving]).await;
    let moved = top(&kept, &graph, &market);
    later(&kept, &graph, 1_000);
    println!(
        "{}: {now:?}, mainnet's lifespans +500 {lapsed:?}, moved to {target} {moved:?}",
        market.symbol
    );
    assert!(
        moved.0 < target && target < moved.1,
        "{}: quotes around {target}",
        market.symbol
    );
    assert_eq!(
        top(&kept, &graph, &market),
        moved,
        "{}: the moved makers still quote",
        market.symbol
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_load_brings_the_index_with_the_buffer_and_every_book() {
    let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
    let remote = Some((client(), CommitmentConfig::confirmed()));
    let exchange = Exchange::load(&mut svm, &remote).await.unwrap();

    let map = svm
        .inner
        .get_account(&exchange.perp_asset_map)
        .unwrap()
        .unwrap();
    let books: Vec<Pubkey> = phoenix_markets(exchange.perp_asset_map, &map)
        .unwrap()
        .into_iter()
        .map(|market| market.orderbook)
        .collect();
    assert!(!books.is_empty());
    for address in [exchange.global_trader_index, exchange.active_trader_buffer]
        .into_iter()
        .chain(books)
    {
        assert!(
            svm.inner.get_account(&address).unwrap().is_some(),
            "{address} did not come with the index"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn positions_of_an_address_without_a_trader_say_so() {
    // `Pubkey::new_unique` counts up from low addresses, and some of those exist on mainnet.
    let address = Keypair::new().pubkey();
    let error = trader_positions(client(), address, &[])
        .await
        .err()
        .expect("an address with no account has no positions")
        .to_string();
    assert!(
        error.contains(&format!("there is no Phoenix Trader at {address}")),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn trader_configuration_reaches_what_phoenix_reads() {
    let (locker, graph) = phoenix_behavior_locker().await;
    // A trader's account can be full, so the orders go to a market it already holds.
    let symbol = {
        let mut svm = locker.0.write().await;
        let exchange = Exchange::load(&mut svm, &Some((client(), CommitmentConfig::confirmed())))
            .await
            .unwrap();
        holdings(&svm, &exchange, &graph.trader)
            .unwrap()
            .first()
            .expect("the graph's trader holds a position or orders")
            .market
            .symbol
            .clone()
    };
    buy_one_lot(&locker, graph.trader, &symbol)
        .await
        .unwrap_or_else(|e| panic!("the trader may place market orders on mainnet: {e}"));

    // A capability, signed by the risk authority: a blocked market order is refused.
    configure(
        &locker,
        "phoenix-trader-capabilities",
        graph.trader,
        &[("PlaceMarketOrder", "false".to_string())],
    )
    .await
    .unwrap();
    let refused = buy_one_lot(&locker, graph.trader, &symbol)
        .await
        .expect_err("PlaceMarketOrder false blocks market orders")
        .to_string();
    assert!(
        refused.contains("disallows place market orders"),
        "{refused}"
    );
    configure(
        &locker,
        "phoenix-trader-capabilities",
        graph.trader,
        &[("PlaceMarketOrder", "true".to_string())],
    )
    .await
    .unwrap();
    buy_one_lot(&locker, graph.trader, &symbol)
        .await
        .unwrap_or_else(|e| panic!("true allows them again: {e}"));

    // Fee multipliers, signed by the market authority, land in the record Phoenix reads.
    configure(
        &locker,
        "phoenix-trader-fees",
        graph.trader,
        &[
            ("makerFeeOverrideMultiplier", "2".to_string()),
            ("takerFeeOverrideMultiplier", "3".to_string()),
        ],
    )
    .await
    .unwrap();
    let index = locker
        .with_svm_reader(|svm| svm.get_account(&graph.global_trader_index))
        .unwrap()
        .unwrap();
    let range = index_trader_state_range(&index, &graph.trader.to_bytes()).unwrap();
    let state = bytemuck::pod_read_unaligned::<phoenix_rise_accounts::trader::TraderState>(
        &index.data
            [range.start..range.start + size_of::<phoenix_rise_accounts::trader::TraderState>()],
    );
    assert_eq!(
        (
            state.maker_fee_override_multiplier,
            state.taker_fee_override_multiplier
        ),
        (2, 3)
    );
}
