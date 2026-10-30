use core::fmt::Display;

use phoenix_rise_accounts::{
    PhoenixAccountDecodeError,
    orderbook::{ORDERBOOK_CAPACITY, Orderbook},
    pda::{derive_permission_address, derive_spline_collection_address},
    perp_asset_map::{PerpAssetMap, PerpAssetMetadataEntry},
    spline_collection::SplineCollection,
    trader::TraderHeader,
};
use solana_account::Account;
use solana_clock::Clock;
use solana_commitment_config::CommitmentConfig;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use super::{
    state_builder::{
        Exchange, PHOENIX_GLOBAL_CONFIG, PHOENIX_PROGRAM_ID, Sandbox, hydrate, local_account,
        log_authority, phoenix_instruction, symbol_bytes,
    },
    trader::{BboView, HAWKEYE_PROGRAM_ID, bbo},
};
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    surfnet::{remote::SurfnetRemoteClient, svm::SurfnetSvm},
};

/// Crossed resting orders the uncross crank matches in one call: about 12,000 compute units each,
/// so 64 stay well inside a transaction's 1.4 million.
const UNCROSS_MATCH_LIMIT: u64 = 64;
/// Each side holds up to `ORDERBOOK_CAPACITY` resting orders and a match between two removes at
/// least one; spline fills make this a budget, not a guarantee.
const MAX_UNCROSS_CRANKS: usize = 2 * ORDERBOOK_CAPACITY / UNCROSS_MATCH_LIMIT as usize;

pub(crate) fn invalid_perp_asset_map(
    perp_asset_map: &Pubkey,
    reason: impl Display,
) -> SurfpoolError {
    SurfpoolError::invalid_account_data(
        perp_asset_map,
        "Expected the Phoenix Eternal PerpAssetMap",
        Some(reason),
    )
}

/// Every market the PerpAssetMap lists, in storage order.
pub(crate) fn map_entries(
    perp_asset_map: &Pubkey,
    data: &[u8],
) -> SurfpoolResult<Vec<PerpAssetMetadataEntry>> {
    let invalid = |e: PhoenixAccountDecodeError| invalid_perp_asset_map(perp_asset_map, e);
    PerpAssetMap::try_from_account_bytes(data)
        .map_err(invalid)?
        .iter()
        .collect::<Result<_, _>>()
        .map_err(invalid)
}

/// Every market the PerpAssetMap lists, by symbol.
pub fn phoenix_markets(perp_asset_map: Pubkey, account: &Account) -> SurfpoolResult<Vec<Market>> {
    if account.owner != PHOENIX_PROGRAM_ID {
        return Err(SurfpoolError::invalid_account_owner(
            perp_asset_map,
            None::<PhoenixAccountDecodeError>,
        ));
    }
    let mut markets: Vec<Market> = map_entries(&perp_asset_map, &account.data)?
        .iter()
        .map(Market::from_entry)
        .collect();
    markets.sort_unstable_by(|left, right| left.symbol.cmp(&right.symbol));

    Ok(markets)
}

/// One market as the PerpAssetMap describes it.
#[derive(Clone, Debug)]
pub struct Market {
    pub symbol: String,
    pub asset_id: u64,
    pub orderbook: Pubkey,
    pub splines: Pubkey,
    pub tick_size: u64,
    pub base_lot_decimals: i8,
    pub mark_ticks: u64,
    /// The most base lots one liquidation may close.
    pub max_liquidation_size: u64,
    /// Maintenance, backstop and high-risk factors, in basis points of the initial margin.
    pub risk_factors: [u16; 3],
    /// The keys that report this market's oracle prices.
    pub oracle_keys: Vec<Pubkey>,
    pub latest_oracle_timestamp: u64,
}

impl Market {
    fn from_entry(entry: &PerpAssetMetadataEntry) -> Self {
        let params = entry.metadata.static_market_params();
        let orderbook = Pubkey::new_from_array(params.market_account);
        let mark = entry.metadata.oracle_price().mark_price;
        Self {
            symbol: entry.symbol.as_str().to_string(),
            asset_id: u64::from(params.asset_id()),
            orderbook,
            splines: derive_spline_collection_address(&PHOENIX_PROGRAM_ID, &orderbook),
            tick_size: params.tick_size.as_inner(),
            base_lot_decimals: params.base_lot_decimals,
            mark_ticks: mark.price.ticks.as_inner(),
            max_liquidation_size: entry.metadata.risk_params().max_liquidation_size.as_inner(),
            risk_factors: entry.metadata.risk_params().risk_factors,
            oracle_keys: mark
                .oracle_data
                .iter()
                .map(|oracle| Pubkey::new_from_array(oracle.oracle_pubkey))
                .filter(|key| *key != Pubkey::default())
                .collect(),
            latest_oracle_timestamp: mark
                .oracle_last_updated_timestamps
                .iter()
                .copied()
                .max()
                .unwrap_or_default(),
        }
    }

    pub fn find(perp_asset_map: &Pubkey, data: &[u8], symbol: &str) -> SurfpoolResult<Self> {
        let entry = map_entries(perp_asset_map, data)?
            .into_iter()
            .find(|entry| entry.symbol.matches(symbol))
            .ok_or_else(|| SurfpoolError::internal(format!("Phoenix lists no market {symbol}")))?;
        Ok(Self::from_entry(&entry))
    }

    /// The oracle price for `ticks`, as a value and a decimal exponent: USD per base unit is
    /// ticks × tick size × 10^(base lot decimals − 6).
    pub fn oracle_price(&self, ticks: u64) -> SurfpoolResult<(u64, u8)> {
        let overflow =
            || SurfpoolError::internal(format!("{ticks} ticks overflow {}'s price", self.symbol));
        let value = ticks.checked_mul(self.tick_size).ok_or_else(overflow)?;
        let decimals = 6 - i32::from(self.base_lot_decimals);
        if decimals >= 0 {
            Ok((value, decimals as u8))
        } else {
            let scale = 10_u64
                .checked_pow(decimals.unsigned_abs())
                .ok_or_else(overflow)?;
            Ok((value.checked_mul(scale).ok_or_else(overflow)?, 0))
        }
    }
}

/// A market with everything a move needs in the local VM: its accounts, the keys that report its
/// oracle prices with their permissions, and the makers quoting it.
pub struct MarketContext {
    pub exchange: Exchange,
    pub market: Market,
    oracle_permissions: Vec<Pubkey>,
    makers: Vec<Maker>,
    next_timestamp: u64,
}

/// An enabled maker on the market's spline collection.
struct Maker {
    trader: Pubkey,
    /// The wallet that signs for the Trader.
    authority: Pubkey,
    /// The last price sequence number it used.
    sequence: u64,
    /// The slot its updates report.
    update_slot: u64,
}

impl MarketContext {
    pub async fn load(
        svm: &mut SurfnetSvm,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
        symbol: &str,
    ) -> SurfpoolResult<Self> {
        let mut contexts = Self::load_all(svm, remote_ctx, &[symbol]).await?;
        Ok(contexts.remove(0))
    }

    /// The markets of `symbols`, in that order. Their accounts come in two batched fetches, the
    /// books and spline collections first and then what those name, rather than market by market.
    pub async fn load_all(
        svm: &mut SurfnetSvm,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
        symbols: &[&str],
    ) -> SurfpoolResult<Vec<Self>> {
        let exchange = Exchange::load(svm, remote_ctx).await?;
        hydrate(
            svm,
            remote_ctx,
            &[
                exchange.perp_asset_map,
                exchange.global_trader_index,
                exchange.active_trader_buffer,
            ],
        )
        .await?;
        let map = local_account(svm, &exchange.perp_asset_map)?;
        let markets = symbols
            .iter()
            .map(|symbol| {
                let market = Market::find(&exchange.perp_asset_map, &map.data, symbol)?;
                if market.oracle_keys.is_empty() {
                    return Err(SurfpoolError::internal(format!(
                        "Phoenix market {symbol} has no oracle that reports its price"
                    )));
                }
                Ok(market)
            })
            .collect::<SurfpoolResult<Vec<_>>>()?;
        let books: Vec<Pubkey> = markets
            .iter()
            .flat_map(|market| [market.orderbook, market.splines])
            .collect();
        hydrate(svm, remote_ctx, &books).await?;

        // A spline refuses an update slot below its last one, and a running surfnet's slot trails
        // the mainnet slot that last update came from.
        let slot = svm.inner.get_sysvar::<Clock>().slot;
        let oracle_authority = Pubkey::new_from_array(exchange.config.oracle_authority());
        let mut loaded = Vec::with_capacity(markets.len());
        let mut dependencies = Vec::new();
        for market in markets {
            let splines = local_account(svm, &market.splines)?;
            let collection =
                SplineCollection::try_from_account_bytes(&splines.data).map_err(|e| {
                    SurfpoolError::invalid_account_data(
                        market.splines,
                        "Expected a Phoenix spline collection",
                        Some(e),
                    )
                })?;
            let enabled: Vec<(Pubkey, u64, u64)> = collection
                .splines()
                .filter_map(Result::ok)
                .filter(|spline| spline.is_enabled())
                .map(|spline| {
                    (
                        Pubkey::new_from_array(spline.trader()),
                        spline.user_price_sequence_number(),
                        spline.user_update_slot().max(slot),
                    )
                })
                .collect();
            let oracle_permissions: Vec<Pubkey> = market
                .oracle_keys
                .iter()
                .map(|key| derive_permission_address(&PHOENIX_PROGRAM_ID, &oracle_authority, key))
                .collect();
            dependencies.extend(enabled.iter().map(|(trader, ..)| *trader));
            dependencies.extend(&oracle_permissions);
            loaded.push((market, enabled, oracle_permissions));
        }
        hydrate(svm, remote_ctx, &dependencies).await?;

        // An oracle refuses a report older than its last one.
        let clock_ms = (svm.inner.get_sysvar::<Clock>().unix_timestamp.max(0) as u64) * 1000;
        let config = local_account(svm, &PHOENIX_GLOBAL_CONFIG)?;
        loaded
            .into_iter()
            .map(|(market, enabled, oracle_permissions)| {
                let mut makers = Vec::with_capacity(enabled.len());
                for (trader, sequence, update_slot) in enabled {
                    let account = local_account(svm, &trader)?;
                    let header =
                        TraderHeader::try_read_from_account_bytes(&account.data).map_err(|e| {
                            SurfpoolError::invalid_account_data(
                                trader,
                                "Expected a Phoenix Trader",
                                Some(e),
                            )
                        })?;
                    makers.push(Maker {
                        trader,
                        authority: Pubkey::new_from_array(header.authority),
                        sequence,
                        update_slot,
                    });
                }
                Ok(Self {
                    next_timestamp: clock_ms.max(market.latest_oracle_timestamp) + 1,
                    exchange: Exchange::from_config(&config)?,
                    market,
                    oracle_permissions,
                    makers,
                })
            })
            .collect()
    }

    /// Every oracle key reporting `ticks`, which moves the mark there. Each call reports later
    /// than the previous one.
    pub fn oracle_reports(&mut self, ticks: u64) -> SurfpoolResult<Vec<Instruction>> {
        checked_ticks(ticks)?;
        let (value, expo) = self.market.oracle_price(ticks)?;
        let price = serde_json::json!({ "value": value, "expo": expo });
        let symbol = symbol_bytes(&self.market.symbol);
        let timestamp = self.next_timestamp;
        self.next_timestamp += 1;
        self.market
            .oracle_keys
            .iter()
            .zip(&self.oracle_permissions)
            .map(|(key, permission)| {
                phoenix_instruction(
                    "UpdateOraclePricesWithOrdering",
                    &[
                        ("phoenixProgram", PHOENIX_PROGRAM_ID),
                        ("phoenixLogAuthority", log_authority()),
                        ("globalConfiguration", PHOENIX_GLOBAL_CONFIG),
                        ("authority", *key),
                        ("permissionAccount", *permission),
                        ("perpAssetMap", self.exchange.perp_asset_map),
                    ],
                    &serde_json::json!({
                        "params": {
                            "updateTimestamp": timestamp,
                            "updates": [{
                                "perpAssetId": { "symbolBytes": symbol },
                                "newExchangePerpPrice": price,
                                "newExchangeSpotPrice": price,
                            }],
                            "flags": { "flags": 0 },
                        }
                    }),
                )
            })
            .collect()
    }

    /// Every enabled maker re-centring its spline on `ticks`.
    pub fn spline_moves(&mut self, ticks: u64) -> SurfpoolResult<Vec<Instruction>> {
        checked_ticks(ticks)?;
        let mut instructions = Vec::with_capacity(self.makers.len());
        for maker in &mut self.makers {
            maker.sequence += 1;
            let client_order_id = [0_u8; 16];
            instructions.push(phoenix_instruction(
                "UpdateSplinePrice",
                &[
                    ("phoenixProgram", PHOENIX_PROGRAM_ID),
                    ("phoenixLogAuthority", log_authority()),
                    ("signerAccount", maker.authority),
                    ("traderAccount", maker.trader),
                    ("splineAccount", self.market.splines),
                    ("orderbook", self.market.orderbook),
                ],
                &serde_json::json!({
                    "params": {
                        "newMidPrice": ticks,
                        "userUpdateSlot": maker.update_slot,
                        "refreshRegions": true,
                        "userSequenceNumber": maker.sequence,
                        "clientOrderId": client_order_id,
                        "overrideSequenceNumber": false,
                    }
                }),
            )?);
        }
        Ok(instructions)
    }

    /// The crank that matches resting orders a move left crossed.
    pub fn uncross(&self) -> SurfpoolResult<Instruction> {
        phoenix_instruction(
            "UncrossCrank",
            &[
                ("phoenixProgram", PHOENIX_PROGRAM_ID),
                ("phoenixLogAuthority", log_authority()),
                ("globalConfiguration", PHOENIX_GLOBAL_CONFIG),
                ("perpAssetMap", self.exchange.perp_asset_map),
                ("globalTraderIndex", self.exchange.global_trader_index),
                ("activeTraderBuffer", self.exchange.active_trader_buffer),
                ("orderbook", self.market.orderbook),
                ("splines", self.market.splines),
            ],
            &serde_json::json!({ "params": { "matchLimit": UNCROSS_MATCH_LIMIT } }),
        )
    }

    /// The move to `ticks` in `sandbox`: oracle reports and spline moves. Resting orders the moved
    /// splines cross stay until [`MarketContext::uncross_in`].
    pub fn move_in(&mut self, sandbox: &mut Sandbox, ticks: u64) -> SurfpoolResult<()> {
        let mut instructions = self.oracle_reports(ticks)?;
        instructions.extend(self.spline_moves(ticks)?);
        sandbox.run(&instructions)
    }

    /// The uncross crank in `sandbox` until [`uncross_done`]. Hawkeye must be in the local VM.
    pub fn uncross_in(&mut self, sandbox: &mut Sandbox) -> SurfpoolResult<()> {
        let uncross = self.uncross()?;
        let mut before = self.book_top(sandbox)?;
        for _ in 0..MAX_UNCROSS_CRANKS {
            sandbox.run(std::slice::from_ref(&uncross))?;
            let after = self.book_top(sandbox)?;
            if uncross_done(&before, &after) {
                return Ok(());
            }
            before = after;
        }
        Err(SurfpoolError::internal(format!(
            "the {} book stayed crossed after {MAX_UNCROSS_CRANKS} uncross cranks",
            self.market.symbol
        )))
    }

    /// Hawkeye's best bid and ask in `sandbox`, with the number of resting orders in the book.
    fn book_top(&self, sandbox: &mut Sandbox) -> SurfpoolResult<(BboView, usize)> {
        let account = sandbox.account(&self.market.orderbook)?;
        let book = Orderbook::try_from_account_bytes(&account.data).map_err(|e| {
            SurfpoolError::invalid_account_data(
                self.market.orderbook,
                "Expected a Phoenix orderbook",
                Some(e),
            )
        })?;
        let resting = book.num_bids() + book.num_asks();
        Ok((bbo(sandbox, &self.exchange, &self.market)?, resting))
    }
}

/// Whether a crank that took the book from `before` to `after` was the last: the book is uncrossed,
/// or the crank matched nothing because it leaves that cross alone. Every match moves a price or
/// removes a resting order; prices alone miss fills that all sit at the best price.
fn uncross_done(before: &(BboView, usize), after: &(BboView, usize)) -> bool {
    let ((before, before_orders), (after, after_orders)) = (before, after);
    !after.crossed()
        || (after.best_bid_ticks, after.best_ask_ticks, after_orders)
            == (before.best_bid_ticks, before.best_ask_ticks, before_orders)
}

/// The writes that move one market to `target_ticks` the way mainnet does: oracle reports and
/// makers' splines re-centred. Resting orders the new price crosses stay until [`uncross_market`].
pub async fn move_market(
    svm: &mut SurfnetSvm,
    remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    symbol: &str,
    target_ticks: u64,
) -> SurfpoolResult<Vec<(Pubkey, Account)>> {
    let mut context = MarketContext::load(svm, remote_ctx, symbol).await?;
    let mut sandbox = Sandbox::new(svm);
    context.move_in(&mut sandbox, target_ticks)?;
    sandbox.writes()
}

/// The writes of Phoenix's uncross crank matching one market's crossed resting orders, which anyone
/// can send after a move.
pub async fn uncross_market(
    svm: &mut SurfnetSvm,
    remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    symbol: &str,
) -> SurfpoolResult<Vec<(Pubkey, Account)>> {
    let mut context = MarketContext::load(svm, remote_ctx, symbol).await?;
    hydrate(svm, remote_ctx, &[HAWKEYE_PROGRAM_ID]).await?;
    let mut sandbox = Sandbox::new(svm);
    context
        .uncross_in(&mut sandbox)
        .map_err(explain_move_failure)?;
    sandbox.writes()
}

/// Phoenix's uncross crank breaks on books that do not match the local trader index, which
/// happens when surfnet fetched the two at different times; the raw program panic says nothing.
pub(crate) fn explain_move_failure(error: SurfpoolError) -> SurfpoolError {
    let text = error.to_string();
    if !text.contains("Uncross Crank") {
        return error;
    }
    SurfpoolError::internal(format!(
        "Phoenix's uncross crank failed, which happens when the local order books and Phoenix's \
         trader index were fetched at different times; restart the surfnet (or reset the Phoenix \
         accounts) and play again. {text}"
    ))
}

fn checked_ticks(ticks: u64) -> SurfpoolResult<()> {
    if ticks == 0 || ticks > u64::from(u32::MAX) {
        return Err(SurfpoolError::internal(format!(
            "{ticks} ticks are outside Phoenix's price range 1..={}",
            u32::MAX
        )));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use base64::{Engine, prelude::BASE64_STANDARD};
    use phoenix_rise_accounts::perp_asset_map::PriceComponent;

    use super::{super::state_builder::PHOENIX_PERP_ASSET_MAP, *};

    const PERP_ASSET_MAP_LEN: usize = 1_622_064;
    const SOL_PERP_ASSET_MAP_PREFIX_B64: &str = "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";

    pub(crate) fn perp_asset_map_fixture() -> Vec<u8> {
        let prefix = BASE64_STANDARD
            .decode(SOL_PERP_ASSET_MAP_PREFIX_B64)
            .unwrap();
        let mut data = vec![0_u8; PERP_ASSET_MAP_LEN];
        data[..prefix.len()].copy_from_slice(&prefix);
        data[24..26].copy_from_slice(&1_u16.to_le_bytes());
        data[32..36].copy_from_slice(&1_u32.to_le_bytes());
        data[36..40].copy_from_slice(&0_u32.to_le_bytes());
        data
    }

    pub(crate) fn perp_asset_map_account() -> Account {
        Account {
            lamports: 1,
            data: perp_asset_map_fixture(),
            owner: PHOENIX_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        }
    }

    fn market(tick_size: u64, base_lot_decimals: i8) -> Market {
        Market {
            symbol: "SOL".to_string(),
            asset_id: 0,
            orderbook: Pubkey::new_unique(),
            splines: Pubkey::new_unique(),
            tick_size,
            base_lot_decimals,
            mark_ticks: 1,
            max_liquidation_size: 1,
            risk_factors: [5_000, 2_000, 1_000],
            oracle_keys: vec![Pubkey::new_unique()],
            latest_oracle_timestamp: 0,
        }
    }

    #[test]
    fn converts_ticks_to_the_oracle_price() {
        // SOL: tick size 100, base lot decimals 2, so 10730 ticks are 107.30 USD.
        assert_eq!(market(100, 2).oracle_price(10_730).unwrap(), (1_073_000, 4));
        assert_eq!(market(1, -3).oracle_price(5).unwrap(), (5, 9));
        assert_eq!(market(3, 8).oracle_price(7).unwrap(), (2_100, 0));
        assert!(market(u64::MAX, 2).oracle_price(2).is_err());
    }

    #[test]
    fn lists_the_markets_a_caller_can_name() {
        let markets = phoenix_markets(PHOENIX_PERP_ASSET_MAP, &perp_asset_map_account()).unwrap();
        let [sol] = markets.as_slice() else {
            panic!("the fixture lists one market: {markets:?}");
        };
        assert_eq!(
            (
                sol.symbol.as_str(),
                sol.orderbook,
                sol.tick_size,
                sol.base_lot_decimals,
                &sol.risk_factors[..2],
            ),
            (
                "SOL",
                Pubkey::from_str_const("71Si24E4uc3oCaPbPZTozC1ptSNNqygjjebxSmErSsC2"),
                100,
                2,
                &[5_000, 2_000][..],
            )
        );
        let foreign = Account {
            owner: Pubkey::new_unique(),
            ..perp_asset_map_account()
        };
        assert!(phoenix_markets(PHOENIX_PERP_ASSET_MAP, &foreign).is_err());
    }

    #[test]
    fn configuration_finds_a_market_without_oracles() {
        let mut data = perp_asset_map_fixture();
        // The fixture's only market starts after the 48-byte header with its 16-byte symbol.
        let range = 64..64 + size_of::<PriceComponent>();
        let mut price: PriceComponent = bytemuck::pod_read_unaligned(&data[range.clone()]);
        for oracle in &mut price.mark_price.oracle_data {
            oracle.oracle_pubkey = [0; 32];
        }
        data[range].copy_from_slice(bytemuck::bytes_of(&price));
        let market = Market::find(&PHOENIX_PERP_ASSET_MAP, &data, "SOL").unwrap();
        assert!(market.oracle_keys.is_empty());
    }

    fn book_at(bid: u64, ask: u64, orders: usize) -> (BboView, usize) {
        let mut view: BboView = bytemuck::Zeroable::zeroed();
        view.best_bid_ticks = bid;
        view.best_ask_ticks = ask;
        (view, orders)
    }

    #[test]
    fn uncross_cranks_stop_once_the_book_uncrosses_or_a_crank_changes_nothing() {
        let crossed = book_at(120, 100, 200);
        assert!(
            !uncross_done(&crossed, &book_at(110, 100, 200)),
            "bid moved"
        );
        assert!(
            !uncross_done(&crossed, &book_at(120, 105, 200)),
            "ask moved"
        );
        assert!(
            !uncross_done(&crossed, &book_at(120, 100, 136)),
            "filled at the best price"
        );
        assert!(uncross_done(&crossed, &book_at(99, 100, 200)), "uncrossed");
        assert!(uncross_done(&crossed, &book_at(100, 100, 200)), "locked");
        assert!(uncross_done(&crossed, &book_at(0, 100, 200)), "no bids");
        assert!(uncross_done(&crossed, &book_at(120, 0, 200)), "no asks");
        assert!(uncross_done(&crossed, &crossed), "no progress");
    }
}
