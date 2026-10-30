use std::collections::HashMap;

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
use solana_program_pack::Pack;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_sysvar::last_restart_slot::LastRestartSlot;
use solana_transaction::Transaction;
use spl_associated_token_account_interface::address::get_associated_token_address;
use surfpool_types::DEFAULT_MAINNET_RPC_URL;

use crate::{
    error::SurfpoolResult,
    scenarios::protocols::phoenix_eternal::v1::{
        market::{Market, MarketContext, move_market, phoenix_markets, uncross_market},
        state_builder::{
            Exchange, PHOENIX_GLOBAL_CONFIG, PHOENIX_PERP_ASSET_MAP, PHOENIX_PROGRAM_ID,
            keep_markets_usable, local_account, log_authority, phoenix_instruction,
            run_instructions, tests::template_addresses,
        },
        trader::{
            BboView, HAWKEYE_PROGRAM_ID, MarginView, VIEW_BBO, VIEW_MARGIN, holdings,
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

fn mainnet_graph_cache() -> &'static tokio::sync::Mutex<Option<PhoenixMainnetGraph>> {
    static CACHE: std::sync::OnceLock<tokio::sync::Mutex<Option<PhoenixMainnetGraph>>> =
        std::sync::OnceLock::new();
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

/// A fresh surfnet whose trader a new wallet opened locally: long SOL and short BTC, with bids
/// resting on SOL, BTC and ETH that make it hot.
async fn phoenix_behavior_locker() -> (SurfnetSvmLocker, PhoenixMainnetGraph) {
    let mut graph = phoenix_mainnet_graph().await;
    let locker = phoenix_surfnet(&graph).await;
    let trader = LocalTrader::create(&locker, 10_000).await.unwrap();
    trader
        .place_market_order(&locker, "SOL", 1_000)
        .await
        .unwrap();
    trader
        .place_market_order(&locker, "BTC", -100)
        .await
        .unwrap();
    for symbol in ["SOL", "BTC", "ETH"] {
        trader
            .place_limit_order(&locker, symbol, 100, 5)
            .await
            .unwrap();
    }
    graph.trader = trader.account;
    (locker, graph)
}

/// The behaviour tests start from a trader a new wallet opens locally, as a user does on mainnet:
/// hot, as its resting orders make it, healthy, with collateral, positions on SOL and BTC and orders
/// alone on ETH.
#[tokio::test(flavor = "multi_thread")]
async fn the_local_trader_is_hot_and_healthy_with_positions_on_sol_and_btc() {
    let remote = Some((client(), CommitmentConfig::confirmed()));
    let (locker, graph) = phoenix_behavior_locker().await;
    let trader = graph.trader;
    let index = locker.with_svm_reader(|svm| local_account(svm, &graph.global_trader_index));
    assert!(
        index_trader_state_range(&index.unwrap(), &trader.to_bytes()).is_ok(),
        "{trader}: hot"
    );
    let margin = hawkeye_margin(&locker, &graph);
    assert!(
        margin.collateral_quote_lots > 0
            && margin.maintenance_margin_quote_lots > 0
            && margin.is_liquidatable == 0,
        "{trader}: healthy, with collateral and margin: {margin:?}"
    );
    let mut held: Vec<(String, i64)> = {
        let mut svm = locker.0.write().await;
        let exchange = Exchange::load(&mut svm, &remote).await.unwrap();
        holdings(&svm, &exchange, &trader)
            .unwrap()
            .into_iter()
            .map(|held| (held.market.symbol, held.base_lots))
            .collect()
    };
    held.sort();
    assert_eq!(
        held,
        [
            ("BTC".to_string(), -100),
            ("ETH".to_string(), 0),
            ("SOL".to_string(), 1_000)
        ],
        "{trader}: its positions, and orders alone on ETH"
    );
}

/// A local VM holding the deployed programs and every graph account, at the graph's clock.
async fn phoenix_surfnet(graph: &PhoenixMainnetGraph) -> SurfnetSvmLocker {
    let eternal_program = deployed_program(ETERNAL_PROGRAMDATA).await;
    let hawkeye_program = deployed_program(HAWKEYE_PROGRAMDATA).await;
    let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
    let locker = SurfnetSvmLocker::new(svm);
    locker.with_svm_writer(|svm_writer| {
        svm_writer.inner.set_sysvar(&graph.clock);
        svm_writer.inner.set_sysvar(&graph.last_restart_slot);
        svm_writer
            .inner
            .svm
            .add_program(PHOENIX_PROGRAM_ID, &eternal_program)
            .unwrap();
        svm_writer
            .inner
            .svm
            .add_program(HAWKEYE_PROGRAM_ID, &hawkeye_program)
            .unwrap();
        for (address, account) in &graph.accounts {
            svm_writer.set_account(address, account.clone()).unwrap();
        }
    });
    locker
}

async fn phoenix_mainnet_graph() -> PhoenixMainnetGraph {
    let mut cache = mainnet_graph_cache().lock().await;
    if let Some(graph) = cache.as_ref() {
        return graph.clone();
    }

    let global_account = fetch(&[PHOENIX_GLOBAL_CONFIG]).await.remove(0);
    let global = GlobalConfig::try_from_account_bytes(&global_account.data)
        .expect("mainnet GlobalConfig decodes");
    let perp_asset_map = Pubkey::new_from_array(global.perp_asset_map_key());
    let global_trader_index = Pubkey::new_from_array(global.global_trader_index_header_key());
    let active_trader_buffer = Pubkey::new_from_array(global.active_trader_buffer_header_key());

    let map_account = fetch(&[perp_asset_map]).await.remove(0);
    let map = PerpAssetMap::try_from_account_bytes(&map_account.data)
        .expect("mainnet PerpAssetMap decodes");
    let mut markets = Vec::new();
    // A local trader deposits the collateral mint into the global vault.
    let mut addresses = vec![
        PHOENIX_GLOBAL_CONFIG,
        perp_asset_map,
        global_trader_index,
        active_trader_buffer,
        Pubkey::new_from_array(global.canonical_token_mint_key()),
        Pubkey::new_from_array(global.global_vault_key()),
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

    // The clock and every account from one bank, so the programs accept them together.
    let clock_address = Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111");
    let keys: Vec<Pubkey> = addresses
        .iter()
        .copied()
        .chain([LAST_RESTART_SLOT, clock_address])
        .collect();
    let mut fetched = fetch(&keys).await;
    let clock: Clock = bincode::deserialize(&fetched.pop().unwrap().data).unwrap();
    assert!(clock.slot > 0);
    // A surfnet loads the restart sysvar from its datasource; the bare test VM has none.
    let last_restart_slot: LastRestartSlot =
        bincode::deserialize(&fetched.pop().unwrap().data).unwrap();

    let mut graph = PhoenixMainnetGraph {
        clock,
        last_restart_slot,
        accounts: addresses.iter().copied().zip(fetched).collect(),
        global_trader_index,
        active_trader_buffer,
        perp_asset_map,
        trader: Pubkey::default(),
        markets,
    };
    let books = fetch_books(&map_account, perp_asset_map, &addresses).await;
    graph.accounts.extend(books);
    *cache = Some(graph.clone());
    graph
}

/// Every market's book and splines not in `held`, read seconds after the trader index, as a
/// surfnet's first load reads them. A book read minutes later names traders by index nodes that
/// moved, and Phoenix then panics or fails with InvalidAccountData.
async fn fetch_books(
    map: &Account,
    perp_asset_map: Pubkey,
    held: &[Pubkey],
) -> Vec<(Pubkey, Account)> {
    let missing: Vec<Pubkey> = phoenix_markets(perp_asset_map, map)
        .unwrap()
        .into_iter()
        .flat_map(|market| [market.orderbook, market.splines])
        .filter(|address| !held.contains(address))
        .collect();
    let mut books = Vec::new();
    for chunk in missing.chunks(100) {
        let fetched = client()
            .get_multiple_accounts(chunk, CommitmentConfig::confirmed())
            .await
            .unwrap_or_else(|e| panic!("failed to fetch the Phoenix books from mainnet: {e}"));
        books.extend(
            chunk
                .iter()
                .zip(fetched)
                .filter_map(|(address, result)| Some((*address, result.map_account().ok()?))),
        );
    }
    books
}

#[derive(Clone)]
struct PhoenixMainnetGraph {
    clock: Clock,
    last_restart_slot: LastRestartSlot,
    accounts: Vec<(Pubkey, Account)>,
    global_trader_index: Pubkey,
    active_trader_buffer: Pubkey,
    perp_asset_map: Pubkey,
    /// The local trader [`phoenix_behavior_locker`] opens; the mainnet graph alone has none.
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

/// `phoenix-trader-cancel-orders` for `trader` on `symbol`, or on every market, which the template
/// takes as values; its own account is the PerpAssetMap.
fn cancel_orders(trader: Pubkey, symbol: Option<&str>) -> surfpool_types::OverrideInstance {
    let trader = trader.to_string();
    phoenix_override(
        "phoenix-trader-cancel-orders",
        PHOENIX_PERP_ASSET_MAP,
        &[
            ("trader", trader.as_str()),
            ("symbol", symbol.unwrap_or("")),
        ],
    )
}

/// `phoenix-trader-collateral` bringing `trader`'s effective collateral to `quote_lots`, which the
/// template takes in USDC.
fn set_collateral(trader: Pubkey, quote_lots: i64) -> surfpool_types::OverrideInstance {
    let sign = if quote_lots < 0 { "-" } else { "" };
    let lots = quote_lots.unsigned_abs();
    let usdc = format!("{sign}{}.{:06}", lots / 1_000_000, lots % 1_000_000);
    phoenix_override(
        "phoenix-trader-collateral",
        trader,
        &[("effective_collateral_usdc", usdc.as_str())],
    )
}

/// `phoenix-market-move` of `symbol` to `ticks`, then the `phoenix-market-uncross` that matches the
/// resting orders the move crossed.
fn move_and_uncross(symbol: &str, ticks: u64) -> [surfpool_types::OverrideInstance; 2] {
    let ticks = ticks.to_string();
    [
        phoenix_override(
            "phoenix-market-move",
            PHOENIX_PERP_ASSET_MAP,
            &[("symbol", symbol), ("target_ticks", ticks.as_str())],
        ),
        phoenix_override(
            "phoenix-market-uncross",
            PHOENIX_PERP_ASSET_MAP,
            &[("symbol", symbol)],
        ),
    ]
}

/// A market move reprices the oracle readings and the makers' quotes and leaves the resting orders
/// where they are; the uncross after it matches the ones the move crossed, so the book quotes
/// around the target.
#[tokio::test(flavor = "multi_thread")]
async fn a_market_move_reprices_the_oracles_and_the_book_together() {
    let (locker, graph) = phoenix_behavior_locker().await;
    let remote = Some((client(), CommitmentConfig::confirmed()));
    let resting = |orderbook: &Pubkey| {
        let book = locker
            .with_svm_reader(|svm| svm.get_account(orderbook))
            .unwrap()
            .unwrap();
        let book = Orderbook::try_from_account_bytes(&book.data).unwrap();
        let prices = |orders: phoenix_rise_accounts::orderbook::OrderbookSideIter<'_>| {
            orders
                .map(|order| order.price_in_ticks().as_inner())
                .collect::<Vec<_>>()
        };
        (prices(book.bid_orders()), prices(book.ask_orders()))
    };

    for (symbol, orderbook, spline) in graph.markets.clone() {
        let before = hawkeye_bbo_for_market(&graph, &locker, orderbook, spline);
        let target = before.mark_price_ticks * 93 / 100;
        let map_before = locker
            .with_svm_reader(|svm| svm.get_account(&graph.perp_asset_map))
            .unwrap();
        let orders_before = resting(&orderbook);

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
        apply(&locker, writes).await;
        assert_eq!(
            resting(&orderbook),
            orders_before,
            "{symbol}: the move alone leaves the resting orders where they are"
        );

        let writes = {
            let mut svm = locker.0.write().await;
            uncross_market(&mut svm, &remote, &symbol)
                .await
                .unwrap_or_else(|e| panic!("{symbol}: {e}"))
        };
        apply(&locker, writes).await;
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

        let uncrossed = resting(&orderbook);
        let writes = {
            let mut svm = locker.0.write().await;
            uncross_market(&mut svm, &remote, &symbol)
                .await
                .unwrap_or_else(|e| panic!("{symbol}: {e}"))
        };
        apply(&locker, writes).await;
        assert_eq!(
            resting(&orderbook),
            uncrossed,
            "{symbol}: an uncross on a book that is not crossed matches nothing"
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
    {
        let mut svm = locker.0.write().await;
        keep_markets_usable(&mut svm, &remote, graph.clock.slot).await;
    }
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

/// A move leaves the resting bids above the new price crossed, and one uncross crank matches at most
/// 64 of them, so `phoenix-market-uncross` cranks until the book is uncrossed.
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
        let moved = hawkeye_bbo_for_market(&graph, &locker, orderbook, spline);
        assert!(
            moved.crossed(),
            "{symbol}: the move alone leaves the bids above {target} crossed: {moved:?}"
        );
        let bids_from = |price: u64| {
            let book = locker
                .with_svm_reader(|svm| svm.get_account(&orderbook))
                .unwrap()
                .unwrap();
            Orderbook::try_from_account_bytes(&book.data)
                .unwrap()
                .bid_orders()
                .filter(|bid| bid.price_in_ticks().as_inner() >= price)
                .count()
        };
        // Bids between the target and the makers' new ask are not crossed and stay.
        let crossed = bids_from(moved.best_ask_ticks);
        if crossed <= 64 {
            continue;
        }

        let writes = {
            let mut svm = locker.0.write().await;
            uncross_market(&mut svm, &remote, &symbol)
                .await
                .unwrap_or_else(|e| panic!("{symbol}: {e}"))
        };
        apply(&locker, writes).await;
        let after = hawkeye_bbo_for_market(&graph, &locker, orderbook, spline);
        assert!(
            after.best_bid_ticks < after.best_ask_ticks,
            "{symbol}: the book is uncrossed at {target} ticks: {after:?}"
        );
        let matched = crossed - bids_from(moved.best_ask_ticks);
        assert!(
            matched > 64,
            "{symbol}: the cranks matched {matched} of the {crossed} crossed bids, more than one crank does"
        );
        return;
    }
    panic!(
        "no eligible mainnet candidate: no book of {:?} leaves more than 64 bids crossed",
        graph.markets
    );
}

/// A one-lot market buy by `trader` in `symbol`, run on a copy of the test VM; it shows whether
/// Phoenix lets the trader place market orders there.
/// The market `symbol` in the surfnet of `locker`, loaded from mainnet when it is missing.
async fn market_context(locker: &SurfnetSvmLocker, symbol: &str) -> SurfpoolResult<MarketContext> {
    let remote = Some((client(), CommitmentConfig::confirmed()));
    let mut svm = locker.0.write().await;
    MarketContext::load(&mut svm, &remote, symbol).await
}

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
    let wallet = Pubkey::new_from_array(header.authority);
    run_instructions(&svm, &[market_order(&market, wallet, trader, 1)?])
}

/// The accounts of an order `trader` places in the market of `context`, signed by its `wallet`.
fn order_accounts(
    context: &MarketContext,
    wallet: Pubkey,
    trader: Pubkey,
) -> [(&'static str, Pubkey); 10] {
    [
        ("phoenixProgram", PHOENIX_PROGRAM_ID),
        ("phoenixLogAuthority", log_authority()),
        ("globalConfiguration", PHOENIX_GLOBAL_CONFIG),
        ("traderWallet", wallet),
        ("traderAccount", trader),
        ("perpAssetMap", context.exchange.perp_asset_map),
        ("globalTraderIndex", context.exchange.global_trader_index),
        ("activeTraderBuffer", context.exchange.active_trader_buffer),
        ("orderbook", context.market.orderbook),
        ("splines", context.market.splines),
    ]
}

/// A market order of `lots`, a buy when positive and a sell when negative, which fills at once.
fn market_order(
    context: &MarketContext,
    wallet: Pubkey,
    trader: Pubkey,
    lots: i64,
) -> SurfpoolResult<Instruction> {
    let side = if lots > 0 { "Bid" } else { "Ask" };
    let client_order_id = [0_u8; 16];
    phoenix_instruction(
        "PlaceMarketOrder",
        &order_accounts(context, wallet, trader),
        &serde_json::json!({
            "orderPacket": {
                "kind": {
                    "ImmediateOrCancel": {
                        "side": { side: null },
                        "priceInTicks": null,
                        "numBaseLots": { "inner": lots.unsigned_abs() },
                        "numQuoteLots": null,
                        "minBaseLotsToFill": { "inner": lots.unsigned_abs() },
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
    )
}

/// A Trader a new wallet opens in the local VM, as a user opens one on mainnet: registered, then
/// granted every capability by the risk authority, which unfreezes it, and funded with a deposit.
#[derive(Clone, Copy)]
struct LocalTrader {
    wallet: Pubkey,
    account: Pubkey,
}

impl LocalTrader {
    /// A trader with `usdc` of collateral, no positions and no orders.
    async fn create(locker: &SurfnetSvmLocker, usdc: u64) -> SurfpoolResult<Self> {
        let wallet = Keypair::new().pubkey();
        let account = Pubkey::find_program_address(
            &[b"trader", wallet.as_ref(), &[0, 0]],
            &PHOENIX_PROGRAM_ID,
        )
        .0;
        let remote = Some((client(), CommitmentConfig::confirmed()));
        let exchange = {
            let mut svm = locker.0.write().await;
            Exchange::load(&mut svm, &remote).await?
        };
        let mint = Pubkey::new_from_array(exchange.config.canonical_token_mint_key());
        let token_account = get_associated_token_address(&wallet, &mint);
        // A USDC token unit is a quote lot.
        let amount = usdc * 1_000_000;
        let mut data = vec![0; spl_token_interface::state::Account::LEN];
        spl_token_interface::state::Account {
            mint,
            owner: wallet,
            amount,
            state: spl_token_interface::state::AccountState::Initialized,
            ..Default::default()
        }
        .pack_into_slice(&mut data);
        locker.with_svm_writer(|svm| {
            svm.inner.airdrop(&wallet, 10_000_000_000).unwrap();
            let lamports = svm.inner.minimum_balance_for_rent_exemption(data.len());
            svm.set_account(
                &token_account,
                Account {
                    lamports,
                    data,
                    owner: spl_token_interface::ID,
                    executable: false,
                    rent_epoch: 0,
                },
            )
        })?;

        let risk = Pubkey::new_from_array(exchange.config.risk_authority());
        let toggles = [
            "PlaceLimitOrder",
            "PlaceMarketOrder",
            "RiskIncreasingTrade",
            "RiskReducingTrade",
            "DepositCollateral",
            "WithdrawCollateral",
        ]
        .map(|target| serde_json::json!({ "target": { target: null }, "enable": true }));
        let instructions = [
            phoenix_instruction(
                "RegisterTrader",
                &[
                    ("phoenixProgram", PHOENIX_PROGRAM_ID),
                    ("phoenixLogAuthority", log_authority()),
                    ("globalConfiguration", PHOENIX_GLOBAL_CONFIG),
                    ("payer", wallet),
                    ("traderWallet", wallet),
                    ("traderAccount", account),
                    ("systemProgram", solana_sdk_ids::system_program::id()),
                ],
                &serde_json::json!({
                    "params": {
                        "maxPositions": 32,
                        "traderPreferenceBits": 0,
                        "traderPdaIndex": 0,
                        "traderSubaccountIndex": 0,
                    }
                }),
            )?,
            phoenix_instruction(
                "SetTraderCapability",
                &[
                    ("phoenixProgram", PHOENIX_PROGRAM_ID),
                    ("phoenixLogAuthority", log_authority()),
                    ("globalConfiguration", PHOENIX_GLOBAL_CONFIG),
                    ("authority", risk),
                    ("maybePermissionAccount", risk),
                    ("traderAccount", account),
                    ("globalTraderIndex", exchange.global_trader_index),
                    ("activeTraderBuffer", exchange.active_trader_buffer),
                ],
                &serde_json::json!({ "params": { "toggles": toggles } }),
            )?,
            phoenix_instruction(
                "DepositFunds",
                &[
                    ("phoenixProgram", PHOENIX_PROGRAM_ID),
                    ("phoenixLogAuthority", log_authority()),
                    ("globalConfiguration", PHOENIX_GLOBAL_CONFIG),
                    ("traderWallet", wallet),
                    ("traderTokenAccount", token_account),
                    ("traderAccount", account),
                    (
                        "globalVault",
                        Pubkey::new_from_array(exchange.config.global_vault_key()),
                    ),
                    ("tokenProgram", spl_token_interface::ID),
                    ("globalTraderIndex", exchange.global_trader_index),
                    ("activeTraderBuffer", exchange.active_trader_buffer),
                ],
                &serde_json::json!({ "params": { "amount": amount } }),
            )?,
        ];
        let writes = locker.with_svm_reader(|svm| run_instructions(svm, &instructions))?;
        apply(locker, writes).await;
        Ok(Self { wallet, account })
    }

    /// A market order of `lots` in `symbol`, a buy when positive and a sell when negative, which
    /// opens or changes the trader's position there.
    async fn place_market_order(
        &self,
        locker: &SurfnetSvmLocker,
        symbol: &str,
        lots: i64,
    ) -> SurfpoolResult<()> {
        let context = market_context(locker, symbol).await?;
        let order = market_order(&context, self.wallet, self.account, lots)?;
        let writes = locker.with_svm_reader(|svm| run_instructions(svm, &[order]))?;
        apply(locker, writes).await;
        Ok(())
    }

    /// A PostOnly order of `lots` in `symbol`, which rests `away_percent` from the mark: a bid below
    /// it when positive, an ask above it when negative. The first resting order makes the trader
    /// hot, in Phoenix's index.
    async fn place_limit_order(
        &self,
        locker: &SurfnetSvmLocker,
        symbol: &str,
        lots: i64,
        away_percent: u64,
    ) -> SurfpoolResult<()> {
        let context = market_context(locker, symbol).await?;
        let mark = context.market.mark_ticks;
        let (side, price) = if lots > 0 {
            ("Bid", mark - mark * away_percent / 100)
        } else {
            ("Ask", mark + mark * away_percent / 100)
        };
        let client_order_id = [0_u8; 16];
        let order = phoenix_instruction(
            "PlaceLimitOrder",
            &order_accounts(&context, self.wallet, self.account),
            &serde_json::json!({
                "orderPacket": {
                    "kind": {
                        "PostOnly": {
                            "side": { side: null },
                            "priceInTicks": { "inner": price },
                            "numBaseLots": { "inner": lots.unsigned_abs() },
                            "clientOrderId": client_order_id,
                            "slide": false,
                            "lastValidSlot": null,
                            "orderFlags": { "flags": 0 },
                            "cancelExisting": false,
                        }
                    }
                }
            }),
        )?;
        let writes = locker.with_svm_reader(|svm| run_instructions(svm, &[order]))?;
        apply(locker, writes).await;
        Ok(())
    }
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

/// Sends a keeper's liquidation of `trader`'s position in `symbol`, as much as the market allows in
/// one go and at most 10% through the mark, like the keepers on mainnet. It is a real transaction,
/// signed by a funded keeper, with signatures checked as on any surfnet.
async fn send_liquidation(
    locker: &SurfnetSvmLocker,
    trader: Pubkey,
    symbol: &str,
    long: bool,
) -> Result<(), String> {
    let context = market_context(locker, symbol)
        .await
        .map_err(|e| format!("{symbol}: {e}"))?;
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
    send_signed(locker, &keeper, liquidation)
}

/// `instruction` in a transaction `signer` signs and pays for, with signature checks on.
fn send_signed(
    locker: &SurfnetSvmLocker,
    signer: &Keypair,
    instruction: Instruction,
) -> Result<(), String> {
    locker.with_svm_writer(|svm| {
        svm.inner.airdrop(&signer.pubkey(), 1_000_000_000).unwrap();
        let transaction = Transaction::new_signed_with_payer(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
                instruction,
            ],
            Some(&signer.pubkey()),
            &[signer],
            svm.latest_blockhash(),
        );
        svm.send_transaction(transaction.into(), false, true)
            .map(|_| ())
            .map_err(|failed| format!("{:?}, logs: {:?}", failed.err, failed.meta.logs))
    })
}

/// A keeper's ForceCancelRiskIncreasing of `trader`'s orders in `market`, which Phoenix allows any
/// signer while the trader is liquidatable.
fn force_cancel(
    exchange: &Exchange,
    keeper: Pubkey,
    trader: Pubkey,
    market: &Market,
) -> Instruction {
    phoenix_instruction(
        "ForceCancelRiskIncreasing",
        &[
            ("phoenixProgram", PHOENIX_PROGRAM_ID),
            ("phoenixLogAuthority", log_authority()),
            ("globalConfiguration", PHOENIX_GLOBAL_CONFIG),
            ("authority", keeper),
            ("traderAccount", trader),
            ("perpAssetMap", exchange.perp_asset_map),
            ("globalTraderIndex", exchange.global_trader_index),
            ("activeTraderBuffer", exchange.active_trader_buffer),
            ("orderbook", market.orderbook),
            ("splines", market.splines),
        ],
        &serde_json::json!({}),
    )
    .unwrap()
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

/// A keeper's liquidation of `trader`'s position of `lots` in `symbol` (see [`send_liquidation`]),
/// which must land and leave a smaller position; `case` names the case in the failures.
async fn liquidate_position(locker: &SurfnetSvmLocker, trader: Pubkey, symbol: &str, case: &str) {
    let remote = Some((client(), CommitmentConfig::confirmed()));
    let held = position_lots(locker, &remote, &trader, symbol).await;
    assert_ne!(held, 0, "{case}: no {symbol} position to liquidate");
    send_liquidation(locker, trader, symbol, held > 0)
        .await
        .unwrap_or_else(|e| panic!("{case}: the keeper's liquidation failed: {e}"));
    let left = position_lots(locker, &remote, &trader, symbol).await;
    assert!(left.abs() < held.abs(), "{case}: {symbol} {held} -> {left}");
}

/// Sets the locker's Clock `slots` past the graph's, its time with it, and returns the new slot.
fn advance_slots(locker: &SurfnetSvmLocker, graph: &PhoenixMainnetGraph, slots: u64) -> u64 {
    let mut clock = graph.clock.clone();
    clock.slot += slots;
    clock.unix_timestamp += (slots * 2 / 5) as i64;
    locker.with_svm_writer(|svm| svm.inner.set_sysvar(&clock));
    clock.slot
}

/// `phoenix-trader-collateral` lands the effective collateral exactly on its target: for a hot
/// trader in its index record, with its orders resting, and kept when a cancel later takes it out of
/// the index; and again once it is cold. The target comes from the tool's margin, the trader with its
/// orders canceled.
#[tokio::test(flavor = "multi_thread")]
async fn the_collateral_lands_on_its_target_hot_and_cold() {
    let remote = Some((client(), CommitmentConfig::confirmed()));
    // A keeper liquidates right after Play and once longer than any maker's quotes last on their own.
    for delay in [0, 500] {
        let (locker, graph) = phoenix_behavior_locker().await;
        let trader = graph.trader;
        let listed = || {
            let index =
                locker.with_svm_reader(|svm| local_account(svm, &graph.global_trader_index));
            index_trader_state_range(&index.unwrap(), &trader.to_bytes()).is_ok()
        };
        // One liquidation's worth while hot, then every position's worth once cold.
        for (hot, every_position) in [(true, false), (false, true)] {
            let held = {
                let mut svm = locker.0.write().await;
                trader_positions_in(&mut svm, &remote, trader, &[])
                    .await
                    .unwrap()
            };
            assert!(held.refusal.is_none(), "{trader}: {:?}", held.refusal);
            assert_eq!(
                held.hot, hot,
                "{trader}: hot at first, cold after the cancel"
            );
            let view = held.margin;
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
            play(
                &locker,
                graph.clock.slot,
                vec![set_collateral(trader, target)],
            )
            .await;
            let prepared = hawkeye_margin(&locker, &graph);
            assert_eq!(
                prepared.effective_collateral_quote_lots, target,
                "{trader} (hot: {hot}): Play lands on the target"
            );
            assert_eq!(
                prepared.is_liquidatable, 1,
                "{trader}: liquidatable after Play"
            );
            if hot {
                assert!(
                    listed(),
                    "{trader}: the collateral alone leaves it in the index"
                );
                // Phoenix carries the record to the Trader account. The cancel touches the markets,
                // which moves the effective collateral a little, as a keeper's instructions do.
                play(&locker, graph.clock.slot, vec![cancel_orders(trader, None)]).await;
                assert!(!listed(), "{trader}: the cancel takes it out of the index");
                let kept = hawkeye_margin(&locker, &graph).effective_collateral_quote_lots;
                let band = view.maintenance_margin_quote_lots - view.backstop_margin_quote_lots;
                assert!(
                    kept.abs_diff(target) <= band / 20,
                    "{trader}: the cancel keeps the collateral written while hot: {kept} from {target}"
                );
            }
        }

        let index = locker.with_svm_reader(|svm| local_account(svm, &graph.global_trader_index));
        assert!(index_trader_state_range(&index.unwrap(), &trader.to_bytes()).is_err());

        advance_slots(&locker, &graph, delay);
        let (symbol, _) = held_positions(&locker, &remote, &trader).await.remove(0);
        liquidate_position(&locker, trader, &symbol, &format!("{trader} +{delay}")).await;
    }
}

/// A trader that left Phoenix's index upstream after the local VM copied the index, its account
/// copied only then, is cold in its account while the local index still lists it.
/// `phoenix-trader-collateral` lands on its target there, a cancel keeps it, and a keeper
/// liquidates the trader.
#[tokio::test(flavor = "multi_thread")]
async fn the_collateral_lands_on_a_trader_whose_account_went_cold_after_the_index_was_copied() {
    let remote = Some((client(), CommitmentConfig::confirmed()));
    let (locker, graph) = phoenix_behavior_locker().await;
    let trader = graph.trader;
    let listed = || {
        let index = locker.with_svm_reader(|svm| local_account(svm, &graph.global_trader_index));
        index_trader_state_range(&index.unwrap(), &trader.to_bytes()).is_ok()
    };
    let hot = || {
        let account = locker.with_svm_reader(|svm| svm.get_account(&trader));
        TraderHeader::try_read_from_account_bytes(&account.unwrap().unwrap().data)
            .unwrap()
            .trader_state
            .is_hot()
    };
    // Phoenix's own cancel takes the trader out of its index, then every other account goes back
    // to before it: the account copied after the trader left, the index before.
    let mut others: Vec<Pubkey> = graph
        .accounts
        .iter()
        .map(|(key, _)| *key)
        .chain(
            graph
                .markets
                .iter()
                .flat_map(|(_, book, spline)| [*book, *spline]),
        )
        .filter(|key| *key != trader)
        .collect();
    others.sort();
    others.dedup();
    let before: Vec<(Pubkey, Account)> = locker.with_svm_reader(|svm| {
        others
            .iter()
            .filter_map(|key| svm.get_account(key).unwrap().map(|account| (*key, account)))
            .collect()
    });
    play(&locker, graph.clock.slot, vec![cancel_orders(trader, None)]).await;
    apply(&locker, before).await;
    assert!(
        listed() && !hot(),
        "{trader}: listed in the index, cold in its account"
    );

    let held = {
        let mut svm = locker.0.write().await;
        trader_positions_in(&mut svm, &remote, trader, &[])
            .await
            .unwrap()
    };
    assert!(held.refusal.is_none(), "{trader}: {:?}", held.refusal);
    let view = held.margin;
    let target =
        ((view.backstop_margin_quote_lots + view.maintenance_margin_quote_lots) / 2) as i64;
    play(
        &locker,
        graph.clock.slot,
        vec![set_collateral(trader, target)],
    )
    .await;
    let prepared = hawkeye_margin(&locker, &graph);
    assert_eq!(
        prepared.effective_collateral_quote_lots, target,
        "{trader}: Play lands on the target"
    );
    assert_eq!(
        prepared.is_liquidatable, 1,
        "{trader}: liquidatable after Play"
    );
    assert!(
        listed() && hot(),
        "{trader}: hot again, as the index lists it"
    );

    // The cancel copies the index record, which holds the collateral, to the account.
    play(&locker, graph.clock.slot, vec![cancel_orders(trader, None)]).await;
    assert!(!listed(), "{trader}: the cancel takes it out of the index");
    let kept = hawkeye_margin(&locker, &graph).effective_collateral_quote_lots;
    let band = view.maintenance_margin_quote_lots - view.backstop_margin_quote_lots;
    assert!(
        kept.abs_diff(target) <= band / 20,
        "{trader}: the cancel keeps the collateral: {kept} from {target}"
    );
    let (symbol, _) = held_positions(&locker, &remote, &trader).await.remove(0);
    liquidate_position(&locker, trader, &symbol, &trader.to_string()).await;
}

/// `phoenix-trader-cancel-orders` cancels a hot trader's resting orders on every market, which takes
/// it out of Phoenix's index; its positions stay open. On a cold trader it changes nothing, and a
/// trader the surfnet has not loaded yet is fetched first.
#[tokio::test(flavor = "multi_thread")]
async fn canceling_orders_takes_a_hot_trader_out_of_the_index_and_keeps_its_positions() {
    let remote = Some((client(), CommitmentConfig::confirmed()));
    let (locker, graph) = phoenix_behavior_locker().await;
    let trader = graph.trader;
    let listed_trader = |trader: Pubkey| {
        let index = locker.with_svm_reader(|svm| local_account(svm, &graph.global_trader_index));
        index_trader_state_range(&index.unwrap(), &trader.to_bytes()).is_ok()
    };
    let listed = || listed_trader(trader);
    let cancel = || vec![cancel_orders(trader, None)];
    assert!(listed(), "{trader}: hot at first");
    let positions = held_positions(&locker, &remote, &trader).await;

    play(&locker, graph.clock.slot, cancel()).await;
    assert!(!listed(), "{trader}: the cancel takes it out of the index");
    assert_eq!(
        held_positions(&locker, &remote, &trader).await,
        positions,
        "{trader}: positions stay open"
    );
    let order_only = {
        let mut svm = locker.0.write().await;
        let exchange = Exchange::load(&mut svm, &remote).await.unwrap();
        holdings(&svm, &exchange, &trader)
            .unwrap()
            .into_iter()
            .filter(|held| held.base_lots == 0)
            .count()
    };
    assert_eq!(
        order_only, 0,
        "{trader}: no market is left with resting orders alone"
    );
    let canceled = hawkeye_margin(&locker, &graph);

    play(&locker, graph.clock.slot, cancel()).await;
    let again = hawkeye_margin(&locker, &graph);
    assert_eq!(
        (
            again.effective_collateral_quote_lots,
            again.maintenance_margin_quote_lots,
        ),
        (
            canceled.effective_collateral_quote_lots,
            canceled.maintenance_margin_quote_lots,
        ),
        "{trader}: cold, a second cancel changes nothing"
    );

    // Any hot trader the graph did not copy, other than a spline market maker the cancel refuses.
    let unloaded: Vec<Pubkey> =
        index_trader_state_ranges(graph.account(&graph.global_trader_index))
            .unwrap()
            .into_iter()
            .map(|(other, _)| other)
            .filter(|other| {
                locker.with_svm_reader(|svm| svm.inner.get_account(other).unwrap().is_none())
            })
            .take(5)
            .collect();
    let mut taken_out = None;
    for other in unloaded {
        play(&locker, graph.clock.slot, vec![cancel_orders(other, None)]).await;
        if !listed_trader(other) {
            taken_out = Some(other);
            break;
        }
    }
    assert!(
        taken_out.is_some(),
        "a hot trader the surfnet had not loaded is fetched and taken out of the index"
    );
}

/// `phoenix-trader-cancel-orders` with one market cancels the trader's orders there and leaves its
/// orders on the other markets, and its place in the index, as they were; with several markets
/// separated by commas, it cancels on each, and a repeated or empty entry changes nothing. An
/// unknown market, or a symbol that is not text, refuses the whole override, so nothing is
/// canceled. An empty symbol cancels the rest and takes the trader out of the index.
#[tokio::test(flavor = "multi_thread")]
async fn canceling_orders_on_named_markets_leaves_the_others() {
    let graph = phoenix_mainnet_graph().await;
    let locker = phoenix_surfnet(&graph).await;
    let local = LocalTrader::create(&locker, 10_000).await.unwrap();
    let symbols = ["SOL", "BTC", "ETH", "HYPE"];
    for symbol in symbols {
        local
            .place_limit_order(&locker, symbol, 100, 5)
            .await
            .unwrap();
        local
            .place_limit_order(&locker, symbol, -100, 5)
            .await
            .unwrap();
    }
    let trader = local.account;
    let index = || {
        locker
            .with_svm_reader(|svm| local_account(svm, &graph.global_trader_index))
            .unwrap()
    };
    let listed = || index_trader_state_range(&index(), &trader.to_bytes()).is_ok();

    // Resting orders name their trader by its node in the index, numbered from 1 after the
    // index's 96-byte header in 64-byte nodes, the TraderState 48 bytes into its node.
    let range = index_trader_state_range(&index(), &trader.to_bytes()).unwrap();
    let node = ((range.start - 96 - 48) / 64 + 1) as u32;
    let map = graph.account(&graph.perp_asset_map);
    let orders_of = || {
        symbols.map(|symbol| {
            let orderbook = Market::find(&graph.perp_asset_map, &map.data, symbol)
                .unwrap()
                .orderbook;
            let book = locker
                .with_svm_reader(|svm| local_account(svm, &orderbook))
                .unwrap();
            let book = Orderbook::try_from_account_bytes(&book.data).unwrap();
            let count = book
                .bid_orders()
                .chain(book.ask_orders())
                .filter(|order| {
                    order
                        .order()
                        .trader_position_id()
                        .trader_id()
                        .as_u32_checked()
                        == Some(node)
                })
                .count();
            (symbol, count)
        })
    };
    let placed = [("SOL", 2), ("BTC", 2), ("ETH", 2), ("HYPE", 2)];
    assert_eq!(orders_of(), placed, "{trader}: a bid and an ask on each");

    let unknown = configure(
        &locker,
        "phoenix-trader-cancel-orders",
        graph.perp_asset_map,
        &[
            ("trader", trader.to_string()),
            ("symbol", "SOL, NOT-A-MARKET".to_string()),
        ],
    )
    .await;
    assert!(
        unknown
            .as_ref()
            .is_err_and(|e| e.contains("Phoenix lists no market NOT-A-MARKET")),
        "{unknown:?}"
    );
    play(
        &locker,
        graph.clock.slot,
        vec![cancel_orders(trader, Some("SOL, NOT-A-MARKET"))],
    )
    .await;
    assert_eq!(
        orders_of(),
        placed,
        "{trader}: Play cancels nothing, SOL included"
    );

    let mut as_list = cancel_orders(trader, None);
    as_list
        .values
        .insert("symbol".to_string(), serde_json::json!(["SOL"]));
    play(&locker, graph.clock.slot, vec![as_list]).await;
    assert_eq!(
        orders_of(),
        placed,
        "{trader}: a symbol that is not text cancels nothing"
    );

    play(
        &locker,
        graph.clock.slot,
        vec![cancel_orders(trader, Some("BTC"))],
    )
    .await;
    assert_eq!(
        orders_of(),
        [("SOL", 2), ("BTC", 0), ("ETH", 2), ("HYPE", 2)],
        "{trader}: BTC's orders go, the others stay"
    );
    assert!(
        listed(),
        "{trader}: still in the index with orders on other markets"
    );

    let list = "SOL, , ETH, SOL";
    play(
        &locker,
        graph.clock.slot,
        vec![cancel_orders(trader, Some(list))],
    )
    .await;
    assert_eq!(
        orders_of(),
        [("SOL", 0), ("BTC", 0), ("ETH", 0), ("HYPE", 2)],
        "{trader}: '{list}' cancels on SOL and ETH only"
    );
    assert!(listed(), "{trader}: still in the index with orders on HYPE");

    play(&locker, graph.clock.slot, vec![cancel_orders(trader, None)]).await;
    assert_eq!(
        orders_of(),
        [("SOL", 0), ("BTC", 0), ("ETH", 0), ("HYPE", 0)],
        "{trader}: an empty symbol cancels the rest"
    );
    assert!(
        !listed(),
        "{trader}: with no orders left it leaves the index"
    );
}

/// The margin the tool predicts after a move is the margin Phoenix computes once the move plays,
/// and the collateral set from it leaves the trader for a keeper after moves up to 35%. Without that
/// collateral a big move takes a trader straight past the band below zero, where Phoenix refuses to
/// liquidate it.
#[tokio::test(flavor = "multi_thread")]
async fn the_margin_after_moves_is_the_margin_once_the_moves_play() {
    let remote = Some((client(), CommitmentConfig::confirmed()));
    let graph = phoenix_mainnet_graph().await;
    let margin_of = |locker: &SurfnetSvmLocker, trader: Pubkey| {
        MarginView::from_return_data(&hawkeye_view(locker, &graph, VIEW_MARGIN, &[trader])).unwrap()
    };
    let mark = |locker: &SurfnetSvmLocker, symbol: &str| {
        let map = locker.with_svm_reader(|svm| local_account(svm, &graph.perp_asset_map));
        Market::find(&graph.perp_asset_map, &map.unwrap().data, symbol)
            .unwrap()
            .mark_ticks
    };
    // About 10x: 1,000 USDC against some 10,000 USDC of SOL long and BTC short, within the 25x and
    // 40x Phoenix opens, so a 20% move against both takes more than the deposit. The bid makes the
    // trader hot.
    let positions = [("SOL", 8_000_i64), ("BTC", -200)];
    let leveraged = {
        let graph = &graph;
        move || async move {
            let locker = phoenix_surfnet(graph).await;
            let trader = LocalTrader::create(&locker, 1_000).await.unwrap();
            for (symbol, lots) in positions {
                trader
                    .place_market_order(&locker, symbol, lots)
                    .await
                    .unwrap();
            }
            trader
                .place_limit_order(&locker, "SOL", 1_000, 5)
                .await
                .unwrap();
            (locker, trader.account)
        }
    };
    let (biggest, biggest_lots) = positions[0];

    for percent in [2_u64, 10, 20, 35] {
        let case = format!("{percent}% against");
        let (locker, trader) = leveraged().await;
        // Every market moves against the trader's position there.
        let moves: Vec<(String, u64)> = positions
            .iter()
            .map(|(symbol, lots)| {
                let factor = if *lots > 0 {
                    100 - percent
                } else {
                    100 + percent
                };
                (symbol.to_string(), mark(&locker, symbol) * factor / 100)
            })
            .collect();
        let predicted = {
            let mut svm = locker.0.write().await;
            trader_positions_in(&mut svm, &remote, trader, &moves)
                .await
                .unwrap_or_else(|e| panic!("{case}: {e}"))
        };
        assert!(
            predicted.hot && predicted.refusal.is_none(),
            "{case}: hot, and the templates take it: {:?}",
            predicted.refusal
        );
        let now = predicted.margin;
        let after = predicted.margin_after_moves.unwrap();
        let band = after.maintenance_margin_quote_lots - after.backstop_margin_quote_lots;
        let target =
            ((after.backstop_margin_quote_lots + after.maintenance_margin_quote_lots) / 2) as i64;
        let cushion = |view: &MarginView| {
            view.effective_collateral_quote_lots - view.maintenance_margin_quote_lots as i64
        };
        assert!(
            cushion(&after) < cushion(&now),
            "{case}: the move pushes the trader toward liquidation"
        );
        let market_moves: Vec<_> = moves
            .iter()
            .flat_map(|(symbol, ticks)| move_and_uncross(symbol, *ticks))
            .collect();

        // The collateral lands a slot before the move, which changes it by what the tool predicts.
        let wanted =
            now.effective_collateral_quote_lots - after.effective_collateral_quote_lots + target;
        play(
            &locker,
            graph.clock.slot,
            vec![cancel_orders(trader, None), set_collateral(trader, wanted)],
        )
        .await;
        assert_eq!(
            margin_of(&locker, trader).is_liquidatable,
            0,
            "{case}: healthy until the move"
        );
        let next = advance_slots(&locker, &graph, 1);
        play(&locker, next, market_moves.clone()).await;
        for (symbol, ticks) in &moves {
            let reached = mark(&locker, symbol);
            assert!(
                reached.abs_diff(*ticks) <= ticks / 1_000,
                "{case}: {symbol}'s mark reached {reached}, not {ticks} ticks"
            );
        }

        let played = margin_of(&locker, trader);
        let effective = played.effective_collateral_quote_lots;
        assert_eq!(
            played.is_liquidatable, 1,
            "{case}: liquidatable after the move"
        );
        assert!(
            effective > played.backstop_margin_quote_lots as i64
                && effective < played.maintenance_margin_quote_lots as i64,
            "{case}: {effective} is between backstop and maintenance"
        );
        assert!(
            effective.abs_diff(target) <= band / 20,
            "{case}: {effective} is within 5% of the band from {target}"
        );
        liquidate_position(&locker, trader, biggest, &case).await;

        if percent >= 20 {
            let (bare, trader) = leveraged().await;
            play(&bare, graph.clock.slot, vec![cancel_orders(trader, None)]).await;
            let next = advance_slots(&bare, &graph, 1);
            play(&bare, next, market_moves).await;
            let left = margin_of(&bare, trader).effective_collateral_quote_lots;
            assert!(
                left < 0,
                "{case}, without the collateral: {left} is below zero"
            );
            let refused = send_liquidation(&bare, trader, biggest, biggest_lots > 0).await;
            assert!(
                refused
                    .as_ref()
                    .is_err_and(|e| e.to_lowercase().contains("underwater")),
                "{case}, without the collateral: refused as underwater: {refused:?}"
            );
        }
    }
}

/// An oracle-only move against a trader's position moves its margin while the order book stays where
/// it was; with its collateral then set in the band the move leaves, a keeper's liquidation fills
/// against that book.
#[tokio::test(flavor = "multi_thread")]
async fn an_oracle_only_move_leaves_a_trader_liquidatable_for_a_keeper() {
    let remote = Some((client(), CommitmentConfig::confirmed()));
    let (locker, graph) = phoenix_behavior_locker().await;
    let trader = graph.trader;
    play(&locker, graph.clock.slot, vec![cancel_orders(trader, None)]).await;
    // A cancel freezes the mark for risk in its slot, so the move plays a slot later.
    let slot = advance_slots(&locker, &graph, 1);
    let (symbol, lots) = held_positions(&locker, &remote, &trader)
        .await
        .into_iter()
        .max_by_key(|(_, lots)| lots.unsigned_abs())
        .unwrap();
    let map = locker.with_svm_reader(|svm| local_account(svm, &graph.perp_asset_map));
    let market = phoenix_markets(graph.perp_asset_map, &map.unwrap())
        .unwrap()
        .into_iter()
        .find(|market| market.symbol == symbol)
        .unwrap();
    let quotes = || {
        let view = hawkeye_bbo_for_market(&graph, &locker, market.orderbook, market.splines);
        (view.best_bid_ticks, view.best_ask_ticks)
    };
    let book = quotes();
    let before = hawkeye_margin(&locker, &graph);
    let change = if lots > 0 { "-5" } else { "5" };
    let oracle_move = phoenix_override(
        "phoenix-oracle-price",
        graph.perp_asset_map,
        &[("symbol", symbol.as_str()), ("change_percent", change)],
    );
    play(&locker, slot, vec![oracle_move]).await;
    assert_eq!(
        quotes(),
        book,
        "{trader}: the oracle move leaves the {symbol} book's best bid and ask"
    );
    let moved = hawkeye_margin(&locker, &graph);
    assert!(
        moved.effective_collateral_quote_lots < before.effective_collateral_quote_lots,
        "{trader}: the oracle move against its {symbol} position lowers its effective collateral: \
         {} -> {}",
        before.effective_collateral_quote_lots,
        moved.effective_collateral_quote_lots
    );

    let target =
        ((moved.backstop_margin_quote_lots + moved.maintenance_margin_quote_lots) / 2) as i64;
    play(&locker, slot, vec![set_collateral(trader, target)]).await;
    let prepared = hawkeye_margin(&locker, &graph);
    assert_eq!(
        (
            prepared.effective_collateral_quote_lots,
            prepared.is_liquidatable
        ),
        (target, 1),
        "{trader}: liquidatable in the band the oracle move leaves"
    );
    liquidate_position(&locker, trader, &symbol, &trader.to_string()).await;
}

/// A higher maintenance factor on a trader's biggest market makes a trader just above its old
/// maintenance margin liquidatable without any price change, and a keeper liquidates it.
#[tokio::test(flavor = "multi_thread")]
async fn a_higher_maintenance_factor_leaves_a_trader_liquidatable_for_a_keeper() {
    let remote = Some((client(), CommitmentConfig::confirmed()));
    let (locker, graph) = phoenix_behavior_locker().await;
    let trader = graph.trader;
    play(&locker, graph.clock.slot, vec![cancel_orders(trader, None)]).await;
    let biggest = {
        let mut svm = locker.0.write().await;
        let exchange = Exchange::load(&mut svm, &remote).await.unwrap();
        holdings(&svm, &exchange, &trader)
            .unwrap()
            .into_iter()
            .filter(|held| held.base_lots != 0)
            .max_by_key(|held| held.maintenance_margin_quote_lots)
            .unwrap()
    };
    let symbol = biggest.market.symbol.as_str();
    let factor = u64::from(biggest.market.risk_factors[0]);
    let raised = (factor * 3 / 2).min(10_000);
    assert!(
        raised > factor,
        "{trader}: {symbol}'s maintenance factor {factor} is already the highest"
    );
    // Half the margin the raise adds to that market above the total, so the raise outgrows it.
    let added = biggest.maintenance_margin_quote_lots * (raised - factor) / factor;
    let view = hawkeye_margin(&locker, &graph);
    let target = (view.maintenance_margin_quote_lots + added / 2) as i64;
    play(
        &locker,
        graph.clock.slot,
        vec![set_collateral(trader, target)],
    )
    .await;
    assert_eq!(
        hawkeye_margin(&locker, &graph).is_liquidatable,
        0,
        "{trader}: healthy under the old factor"
    );

    // Phoenix keeps the maintenance factor at or below the cancel order factor, which is raised
    // only when it is lower.
    let map = locker.with_svm_reader(|svm| local_account(svm, &graph.perp_asset_map));
    let cancel_factor = PerpAssetMap::try_from_account_bytes(&map.unwrap().data)
        .unwrap()
        .find_by_symbol(symbol)
        .unwrap()
        .unwrap()
        .metadata
        .risk_params()
        .cancel_order_risk_factor;
    let cancel_is_lower = u64::from(cancel_factor) < raised;
    let raised = raised.to_string();
    let mut changes = Vec::new();
    if cancel_is_lower {
        changes.push(phoenix_override(
            "phoenix-market-cancel-risk-factor",
            graph.perp_asset_map,
            &[
                ("symbol", symbol),
                ("cancelOrderRiskFactor", raised.as_str()),
            ],
        ));
    }
    changes.push(phoenix_override(
        "phoenix-market-risk-factors",
        graph.perp_asset_map,
        &[
            ("symbol", symbol),
            ("maintenanceRiskFactor", raised.as_str()),
        ],
    ));
    play(&locker, graph.clock.slot, changes).await;
    let raised_margin = hawkeye_margin(&locker, &graph);
    assert!(
        raised_margin.is_liquidatable == 1
            && raised_margin.effective_collateral_quote_lots
                > raised_margin.backstop_margin_quote_lots as i64,
        "{trader}: liquidatable for one keeper liquidation under the raised factor: {raised_margin:?}"
    );
    liquidate_position(&locker, trader, symbol, &trader.to_string()).await;
}

/// Phoenix refuses to liquidate a hot trader with risk-increasing orders, but once the trader is
/// liquidatable any keeper can force-cancel those orders and then liquidate it. The collateral
/// lands in the trader's index record, without a cancel, on a target from the tool's margin.
#[tokio::test(flavor = "multi_thread")]
async fn a_keeper_force_cancels_a_liquidatable_hot_traders_orders_and_liquidates_it() {
    let remote = Some((client(), CommitmentConfig::confirmed()));
    let mut graph = phoenix_mainnet_graph().await;
    let locker = phoenix_surfnet(&graph).await;
    // Long SOL, with a bid on SOL that would add to the long.
    let opened = LocalTrader::create(&locker, 10_000).await.unwrap();
    opened
        .place_market_order(&locker, "SOL", 1_000)
        .await
        .unwrap();
    opened
        .place_limit_order(&locker, "SOL", 1_000, 5)
        .await
        .unwrap();
    let trader = opened.account;
    graph.trader = trader;
    let held = {
        let mut svm = locker.0.write().await;
        trader_positions_in(&mut svm, &remote, trader, &[])
            .await
            .unwrap()
    };
    assert!(held.hot, "{trader}: hot, as its bid makes it");

    // The middle of the band the orders' cancel leaves, so the trader stays liquidatable for one
    // keeper liquidation once its orders are gone.
    let target = ((held.margin.backstop_margin_quote_lots
        + held.margin.maintenance_margin_quote_lots)
        / 2) as i64;
    play(
        &locker,
        graph.clock.slot,
        vec![set_collateral(trader, target)],
    )
    .await;
    assert_eq!(
        hawkeye_margin(&locker, &graph).is_liquidatable,
        1,
        "{trader}: liquidatable without a cancel"
    );
    let refused = send_liquidation(&locker, trader, "SOL", true).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.contains("risk-increasing")),
        "{trader}: the keeper is refused while the bid rests: {refused:?}"
    );

    let keeper = Keypair::new();
    let sol = market_context(&locker, "SOL").await.unwrap();
    send_signed(
        &locker,
        &keeper,
        force_cancel(&sol.exchange, keeper.pubkey(), trader, &sol.market),
    )
    .unwrap_or_else(|e| panic!("{trader}: the keeper's force cancel failed: {e}"));
    liquidate_position(
        &locker,
        trader,
        "SOL",
        &format!("{trader} after the force cancel"),
    )
    .await;
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

    let (kept, graph, market) = usable(false).await;
    let (lapsing, _, _) = usable(true).await;
    let now = top(&kept, &graph, &market);
    let slot = advance_slots(&kept, &graph, 500);
    advance_slots(&lapsing, &graph, 500);
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
    play(
        &kept,
        slot,
        move_and_uncross(&market.symbol, target).to_vec(),
    )
    .await;
    let moved = top(&kept, &graph, &market);
    advance_slots(&kept, &graph, 1_000);
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
    let symbol = "SOL";
    buy_one_lot(&locker, graph.trader, symbol)
        .await
        .unwrap_or_else(|e| panic!("the trader may place market orders: {e}"));

    // A capability, signed by the risk authority: a blocked market order is refused.
    configure(
        &locker,
        "phoenix-trader-capabilities",
        graph.trader,
        &[("PlaceMarketOrder", "false".to_string())],
    )
    .await
    .unwrap();
    let refused = buy_one_lot(&locker, graph.trader, symbol)
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
    buy_one_lot(&locker, graph.trader, symbol)
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
