use std::{collections::HashSet, ops::Range};

use bytemuck::{Pod, Zeroable};
use log::warn;
use phoenix_rise_accounts::{
    PhoenixAccount,
    multi_arena::MultiArenaHeader,
    trader::{TRADER_CAPABILITY_HOT, TraderHeader, TraderState},
};
use solana_account::Account;
use solana_clock::Clock;
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use super::{
    market::{Market, MarketContext, explain_move_failure, phoenix_markets},
    state_builder::{
        Exchange, PHOENIX_GLOBAL_CONFIG, PHOENIX_PROGRAM_ID, Sandbox, hydrate, keep_markets_usable,
        local_account, log_authority, phoenix_instruction,
    },
};
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    surfnet::{
        remote::SurfnetRemoteClient,
        svm::{SurfnetSvm, SurfnetSvmConfig},
    },
};

pub const HAWKEYE_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("RiSeVw3ZjNfsaXPRb4mgaqYaEEt41pNNJoDvVh7pgQj");
pub(crate) const VIEW_MARGIN: [u8; 8] = [0xb2, 0x0a, 0x7c, 0xad, 0xec, 0xd2, 0x75, 0x06];
const MARGIN_VIEW_MAGIC: u64 = 0x955f5b9d3dff253f;
const VIEW_MARGIN_FOR_ASSET: [u8; 8] = [0x20, 0x12, 0xaf, 0xfe, 0xa1, 0xed, 0x4d, 0x9b];
const ASSET_VIEW_MAGIC: u64 = 0xdb58fff8285e1db9;

/// What Hawkeye's `view_margin` returns for one trader.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct MarginView {
    magic: u64,
    pub version: u16,
    pub position_count: u16,
    pub risk_state: u8,
    pub risk_tier: u8,
    pub is_liquidatable: u8,
    padding: u8,
    pub collateral_quote_lots: i64,
    pub effective_collateral_quote_lots: i64,
    pub free_collateral_quote_lots: i64,
    pub withdrawable_collateral_quote_lots: u64,
    pub initial_margin_quote_lots: u64,
    pub maintenance_margin_quote_lots: u64,
    pub cancel_margin_quote_lots: u64,
    pub backstop_margin_quote_lots: u64,
    pub high_risk_margin_quote_lots: u64,
    pub unrealized_pnl_quote_lots: i64,
    pub discounted_unrealized_pnl_quote_lots: i64,
    pub unsettled_funding_quote_lots: i64,
}

impl MarginView {
    /// The view in `view_margin`'s return data.
    pub(crate) fn from_return_data(data: &[u8]) -> SurfpoolResult<Self> {
        read_view(data, "margin", MARGIN_VIEW_MAGIC)
    }
}

/// A Hawkeye view in its return data, which starts with the view's `magic`. Hawkeye is copied
/// from mainnet, where an upgrade may append fields, so bytes past the view are ignored.
fn read_view<T: Pod>(data: &[u8], view: &str, magic: u64) -> SurfpoolResult<T> {
    let len = size_of::<T>();
    let read = data
        .get(..len)
        .map(bytemuck::pod_read_unaligned)
        .ok_or_else(|| {
            SurfpoolError::internal(format!(
                "Hawkeye's {view} view returned {} bytes, fewer than its {len}",
                data.len()
            ))
        })?;
    let found: u64 = bytemuck::pod_read_unaligned(&data[..8]);
    if found != magic {
        return Err(SurfpoolError::internal(format!(
            "Hawkeye's {view} view returned magic {found:#x}"
        )));
    }
    Ok(read)
}

/// What Hawkeye's `view_margin_for_asset` returns for one trader in one market.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct AssetView {
    magic: u64,
    pub asset_id: u32,
    pub version: u16,
    pub has_position_or_orders: u8,
    padding: u8,
    pub risk_state: u8,
    pub risk_tier: u8,
    padding1: [u8; 6],
    /// Positive for a long, negative for a short.
    pub base_lots: i64,
    pub virtual_quote_lots: i64,
    pub mark_price_ticks: u64,
    pub entry_price_quote_lots_per_base_lot: u64,
    pub position_value_quote_lots: i64,
    pub unrealized_pnl_quote_lots: i64,
    pub discounted_unrealized_pnl_quote_lots: i64,
    pub unsettled_funding_quote_lots: i64,
    pub initial_margin_quote_lots: u64,
    pub maintenance_margin_quote_lots: u64,
    pub cancel_margin_quote_lots: u64,
    pub backstop_margin_quote_lots: u64,
    pub high_risk_margin_quote_lots: u64,
}

impl AssetView {
    /// The view in `view_margin_for_asset`'s return data.
    pub fn from_return_data(data: &[u8]) -> SurfpoolResult<Self> {
        read_view(data, "asset", ASSET_VIEW_MAGIC)
    }
}

pub(crate) const VIEW_BBO: [u8; 8] = [0x37, 0x5f, 0x23, 0x2d, 0x53, 0xaf, 0x12, 0x52];
const BBO_VIEW_MAGIC: u64 = 0xefca1fa31fa74171;

/// What Hawkeye's BBO view returns for one market, its makers' splines included.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct BboView {
    magic: u64,
    pub version: u16,
    pub flags: u8,
    padding: [u8; 5],
    pub best_bid_ticks: u64,
    pub best_ask_ticks: u64,
    pub mark_price_ticks: u64,
    pub index_price_ticks: u64,
    pub mark_price_last_updated_slot: u64,
    pub index_price_last_updated_slot: u64,
}

impl BboView {
    /// The view in the BBO view's return data.
    pub fn from_return_data(data: &[u8]) -> SurfpoolResult<Self> {
        read_view(data, "BBO", BBO_VIEW_MAGIC)
    }

    /// Whether a bid still rests above an ask. A side reported as 0 has no price to cross.
    pub fn crossed(&self) -> bool {
        self.best_bid_ticks != 0
            && self.best_ask_ticks != 0
            && self.best_bid_ticks > self.best_ask_ticks
    }
}

/// One market a trader has a position or resting orders in, as Hawkeye reports it.
#[derive(Clone)]
pub struct Holding {
    pub market: Market,
    /// Positive for a long, negative for a short, 0 for resting orders alone.
    pub base_lots: i64,
    pub maintenance_margin_quote_lots: u64,
}

/// Every listed market where Hawkeye reports a position or resting orders of `trader`. Hawkeye
/// reads positions where Phoenix keeps them, and the Trader account's own copy can miss markets.
pub fn holdings(
    svm: &SurfnetSvm,
    exchange: &Exchange,
    trader: &Pubkey,
) -> SurfpoolResult<Vec<Holding>> {
    let map = local_account(svm, &exchange.perp_asset_map)?;
    let mut views = Sandbox::new(svm);
    let mut held = Vec::new();
    for market in phoenix_markets(exchange.perp_asset_map, &map)? {
        let data = views.view(&asset_view(exchange, trader, market.asset_id)?)?;
        let view = AssetView::from_return_data(&data)?;
        if view.has_position_or_orders != 0 {
            held.push(Holding {
                market,
                base_lots: view.base_lots,
                maintenance_margin_quote_lots: view.maintenance_margin_quote_lots,
            });
        }
    }
    Ok(held)
}

/// The trader canceling its resting orders on every market in `held`: Phoenix refuses to
/// liquidate a trader with risk-increasing orders on any market.
async fn cancel_all(
    svm: &mut SurfnetSvm,
    remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    exchange: &Exchange,
    trader: Pubkey,
    authority: Pubkey,
    held: &[Holding],
) -> SurfpoolResult<Vec<Instruction>> {
    let books: Vec<Pubkey> = held
        .iter()
        .flat_map(|held| [held.market.orderbook, held.market.splines])
        .collect();
    hydrate(svm, remote_ctx, &books).await?;
    held.iter()
        .map(|held| {
            phoenix_instruction(
                "CancelAll",
                &[
                    ("phoenixProgram", PHOENIX_PROGRAM_ID),
                    ("phoenixLogAuthority", log_authority()),
                    ("globalConfiguration", PHOENIX_GLOBAL_CONFIG),
                    ("traderWallet", authority),
                    ("traderAccount", trader),
                    ("perpAssetMap", exchange.perp_asset_map),
                    ("globalTraderIndex", exchange.global_trader_index),
                    ("activeTraderBuffer", exchange.active_trader_buffer),
                    ("orderbook", held.market.orderbook),
                    ("splines", held.market.splines),
                ],
                &serde_json::json!({}),
            )
        })
        .collect()
}

/// Hawkeye's margin view of `trader`, read inside the sandbox.
pub fn margin(
    sandbox: &mut Sandbox,
    exchange: &Exchange,
    trader: &Pubkey,
) -> SurfpoolResult<MarginView> {
    let data = sandbox.view(&hawkeye_view(exchange, &[*trader], VIEW_MARGIN.to_vec()))?;
    MarginView::from_return_data(&data)
}

/// Hawkeye's BBO view of `market`, read inside the sandbox.
pub fn bbo(sandbox: &mut Sandbox, exchange: &Exchange, market: &Market) -> SurfpoolResult<BboView> {
    let books = [market.orderbook, market.splines];
    BboView::from_return_data(&sandbox.view(&hawkeye_view(exchange, &books, VIEW_BBO.to_vec()))?)
}

/// Hawkeye's `view_margin_for_asset` of `trader` in the market with `asset_id`.
pub fn asset_view(
    exchange: &Exchange,
    trader: &Pubkey,
    asset_id: u64,
) -> SurfpoolResult<Instruction> {
    let asset_id = u32::try_from(asset_id).map_err(|_| {
        SurfpoolError::internal(format!("asset id {asset_id} does not fit Hawkeye's u32"))
    })?;
    let mut data = VIEW_MARGIN_FOR_ASSET.to_vec();
    data.extend(asset_id.to_le_bytes());
    data.extend([0; 4]);
    Ok(hawkeye_view(exchange, &[*trader], data))
}

/// A Hawkeye view over the exchange accounts and `extra`; `data` is the view's discriminator and
/// its parameters.
fn hawkeye_view(exchange: &Exchange, extra: &[Pubkey], data: Vec<u8>) -> Instruction {
    let accounts = [
        PHOENIX_PROGRAM_ID,
        PHOENIX_GLOBAL_CONFIG,
        exchange.global_trader_index,
        exchange.active_trader_buffer,
        exchange.perp_asset_map,
    ]
    .iter()
    .chain(extra)
    .map(|address| AccountMeta::new_readonly(*address, false))
    .collect();
    Instruction {
        program_id: HAWKEYE_PROGRAM_ID,
        accounts,
        data,
    }
}

/// One position of a trader, as a surfnet reports it.
pub struct TraderPosition {
    pub symbol: String,
    pub orderbook: Pubkey,
    /// Positive for a long, negative for a short.
    pub base_lots: i64,
    pub maintenance_margin_quote_lots: u64,
}

/// One trader as a surfnet reports it.
pub struct TraderPositions {
    pub positions: Vec<TraderPosition>,
    /// Whether the local GlobalTraderIndex lists the trader.
    pub hot: bool,
    /// Hawkeye's view as `phoenix-trader-cancel-orders` leaves the trader, a hot trader's orders
    /// canceled.
    pub margin: MarginView,
    /// The same view once the given markets move and uncross the way `phoenix-market-move` and
    /// `phoenix-market-uncross` play them; `None` without moves.
    pub margin_after_moves: Option<MarginView>,
    /// Why `phoenix-trader-cancel-orders` or `phoenix-trader-collateral` would refuse the trader.
    pub refusal: Option<String>,
}

/// Every listed market `trader` holds a position in, as Hawkeye reports it on a copy of the
/// surfnet `client` points at, which loads any account it is missing; nothing is written back.
/// `moves` are markets and the ticks they move to, for the margin after them.
pub async fn trader_positions(
    client: SurfnetRemoteClient,
    trader: Pubkey,
    moves: &[(String, u64)],
) -> SurfpoolResult<TraderPositions> {
    let (mut svm, _events, _geyser) = SurfnetSvm::new(SurfnetSvmConfig::default())?;
    let commitment = CommitmentConfig::confirmed();
    let clock = client
        .get_account(&solana_sysvar::clock::ID, commitment)
        .await?
        .map_account()?;
    let clock: Clock = bincode::deserialize(&clock.data)
        .map_err(|e| SurfpoolError::internal(format!("the surfnet's Clock is invalid: {e}")))?;
    svm.inner.set_sysvar(&clock);
    svm.inner.set_sysvar(&client.get_last_restart_slot().await?);
    trader_positions_in(&mut svm, &Some((client, commitment)), trader, moves).await
}

/// [`trader_positions`] computed in `svm`, which also takes the markets' upkeep and every account
/// it loads.
pub(crate) async fn trader_positions_in(
    svm: &mut SurfnetSvm,
    remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    trader: Pubkey,
    moves: &[(String, u64)],
) -> SurfpoolResult<TraderPositions> {
    hydrate(svm, remote_ctx, &[trader]).await?;
    let account = svm.inner.get_account(&trader)?.ok_or_else(|| {
        SurfpoolError::internal(format!("there is no Phoenix Trader at {trader}"))
    })?;
    if account.owner != PHOENIX_PROGRAM_ID {
        return Err(SurfpoolError::invalid_account_owner(
            trader,
            Some("Expected a Phoenix Trader"),
        ));
    }
    let slot = svm.inner.get_sysvar::<Clock>().slot;
    keep_markets_usable(svm, remote_ctx, slot).await;
    let symbols: Vec<&str> = moves.iter().map(|(symbol, _)| symbol.as_str()).collect();
    let mut moved = MarketContext::load_all(svm, remote_ctx, &symbols).await?;
    let (mut lever, still_listed) = LeverState::load(svm, remote_ctx, trader, &[]).await?;
    let (margin_now, quote_lot_collateral) = lever.reading(trader)?;
    let refusal = still_listed
        .or_else(|| {
            let left = lever.state.account(&trader).ok()?;
            with_collateral(trader, &left, quote_lot_collateral).err()
        })
        .map(|refusal| refusal.to_string());
    let LeverState {
        exchange,
        hot,
        held,
        mut state,
    } = lever;
    let margin_after_moves = if moves.is_empty() {
        None
    } else {
        // A cancel freezes the mark for risk in its slot, so the moves play a slot later, each
        // followed by its uncross, the way the scenarios play them after the cancel.
        state.next_slot();
        for (context, (_, ticks)) in moved.iter_mut().zip(moves) {
            context.move_in(&mut state, *ticks)?;
            context
                .uncross_in(&mut state)
                .map_err(explain_move_failure)?;
        }
        Some(margin(&mut state, &exchange, &trader)?)
    };
    Ok(TraderPositions {
        positions: held
            .into_iter()
            .filter(|held| held.base_lots != 0)
            .map(|held| TraderPosition {
                symbol: held.market.symbol,
                orderbook: held.market.orderbook,
                base_lots: held.base_lots,
                maintenance_margin_quote_lots: held.maintenance_margin_quote_lots,
            })
            .collect(),
        hot,
        margin: margin_now,
        margin_after_moves,
        refusal,
    })
}

/// The state `phoenix-trader-cancel-orders` leaves, which the collateral is worked out from: a hot
/// trader's orders canceled, a cold trader left as it is, since a cancel changes its margin too.
pub(crate) struct LeverState<'a> {
    pub(crate) exchange: Exchange,
    pub(crate) hot: bool,
    held: Vec<Holding>,
    pub(crate) state: Sandbox<'a>,
}

impl<'a> LeverState<'a> {
    /// The state, with the refusal when Phoenix still lists the trader after the cancel. With
    /// `markets`, only their orders are canceled and a trader still listed is expected; without,
    /// all markets' are.
    pub(crate) async fn load(
        svm: &'a mut SurfnetSvm,
        remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
        trader: Pubkey,
        markets: &[&str],
    ) -> SurfpoolResult<(LeverState<'a>, Option<SurfpoolError>)> {
        let account = local_account(svm, &trader)?;
        let header = TraderHeader::try_read_from_account_bytes(&account.data).map_err(|e| {
            SurfpoolError::invalid_account_data(trader, "Expected a Phoenix Trader", Some(e))
        })?;
        let exchange = Exchange::load(svm, remote_ctx).await?;
        hydrate(
            svm,
            remote_ctx,
            &[
                HAWKEYE_PROGRAM_ID,
                exchange.perp_asset_map,
                exchange.active_trader_buffer,
            ],
        )
        .await?;
        let hot = indexed(&local_account(svm, &exchange.global_trader_index)?, &trader)?;
        refuse_unindexed_hot_trader(trader, &header, hot)?;
        let held = holdings(svm, &exchange, &trader)?;
        let canceled = if markets.is_empty() {
            held.clone()
        } else {
            let map = local_account(svm, &exchange.perp_asset_map)?;
            let symbols = markets
                .iter()
                .map(|symbol| Ok(Market::find(&exchange.perp_asset_map, &map.data, symbol)?.symbol))
                .collect::<SurfpoolResult<HashSet<_>>>()?;
            held.iter()
                .filter(|held| symbols.contains(&held.market.symbol))
                .cloned()
                .collect()
        };
        let cancels = if hot {
            let authority = Pubkey::new_from_array(header.authority);
            cancel_all(svm, remote_ctx, &exchange, trader, authority, &canceled).await?
        } else {
            Vec::new()
        };
        let mut state = Sandbox::new(svm);
        if !cancels.is_empty() {
            state.run(&cancels)?;
        }
        let still_listed = markets.is_empty()
            && hot
            && indexed(&state.account(&exchange.global_trader_index)?, &trader)?;
        let refusal = still_listed.then(|| {
            let why = if cancels.is_empty() {
                "has no orders whose cancel would take it out"
            } else {
                "stays there after its orders are canceled"
            };
            SurfpoolError::internal(format!(
                "{trader} is in Phoenix's active trader index and {why}"
            ))
        });
        Ok((
            LeverState {
                exchange,
                hot,
                held,
                state,
            },
            refusal,
        ))
    }

    /// Hawkeye's margin of `trader` in this state, and its collateral field.
    pub(crate) fn reading(&mut self, trader: Pubkey) -> SurfpoolResult<(MarginView, i64)> {
        let view = margin(&mut self.state, &self.exchange, &trader)?;
        let account = self.state.account(&trader)?;
        let header = TraderHeader::try_read_from_account_bytes(&account.data).map_err(|e| {
            SurfpoolError::invalid_account_data(trader, "Expected a Phoenix Trader", Some(e))
        })?;
        Ok((
            view,
            bytemuck::cast(header.trader_state.quote_lot_collateral),
        ))
    }
}

/// The writes of `trader` canceling all its resting orders on `markets`, or on every market when
/// there are none, which takes a hot trader out of Phoenix's index; a cold trader has none and
/// nothing is written. A trader Phoenix still lists after a cancel on every market is refused.
pub async fn cancel_order_writes(
    svm: &mut SurfnetSvm,
    remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    trader: Pubkey,
    markets: &[&str],
) -> SurfpoolResult<Vec<(Pubkey, Account)>> {
    // The template's own account is the PerpAssetMap, so the trader may not be local yet.
    hydrate(svm, remote_ctx, &[trader]).await?;
    let (lever, refusal) = LeverState::load(svm, remote_ctx, trader, markets).await?;
    if let Some(refusal) = refusal {
        return Err(refusal);
    }
    lever.state.writes()
}

/// The writes that bring `trader`'s effective collateral to `wanted` quote lots from Hawkeye's view
/// when it plays. Effective collateral moves one for one with the collateral field, which is
/// written where Phoenix reads it: a hot trader's record in the GlobalTraderIndex, which Phoenix
/// copies back to the Trader account when the trader leaves the index, or a cold trader's account.
/// A spline market maker is refused.
pub async fn collateral_writes(
    svm: &mut SurfnetSvm,
    remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    trader: Pubkey,
    account: &Account,
    wanted: i64,
) -> SurfpoolResult<Vec<(Pubkey, Account)>> {
    let header = TraderHeader::try_read_from_account_bytes(&account.data).map_err(|e| {
        SurfpoolError::invalid_account_data(trader, "Expected a Phoenix Trader", Some(e))
    })?;
    let exchange = Exchange::load(svm, remote_ctx).await?;
    hydrate(
        svm,
        remote_ctx,
        &[
            HAWKEYE_PROGRAM_ID,
            exchange.perp_asset_map,
            exchange.active_trader_buffer,
        ],
    )
    .await?;
    let index = local_account(svm, &exchange.global_trader_index)?;
    let record = index_record(&index, &trader)?;
    refuse_unindexed_hot_trader(trader, &header, record.is_some())?;
    let view = margin(&mut Sandbox::new(svm), &exchange, &trader)?;
    let field: i64 = bytemuck::cast(match &record {
        Some(range) => {
            bytemuck::pod_read_unaligned::<TraderState>(&index.data[range.clone()])
                .quote_lot_collateral
        }
        None => header.trader_state.quote_lot_collateral,
    });
    let quote_lots = wanted
        .checked_sub(view.effective_collateral_quote_lots)
        .and_then(|gap| field.checked_add(gap))
        .ok_or_else(|| {
            SurfpoolError::internal(format!(
                "an effective collateral of {wanted} quote lots does not fit {trader}'s collateral"
            ))
        })?;
    if wanted < view.high_risk_margin_quote_lots as i64 {
        warn!(
            "{trader}: an effective collateral of {wanted} quote lots is below its high-risk margin \
             of {}; a keeper's liquidation there can leave it underwater and fail, and below 0 \
             Phoenix refuses it",
            view.high_risk_margin_quote_lots
        );
    }
    let left = with_collateral(trader, account, quote_lots)?;
    let Some(range) = record else {
        return Ok(vec![(trader, left)]);
    };
    let mut data = index.data.clone();
    let mut state: TraderState = bytemuck::pod_read_unaligned(&data[range.clone()]);
    state.quote_lot_collateral = bytemuck::cast(quote_lots);
    data[range].copy_from_slice(bytemuck::bytes_of(&state));
    let index_write = (exchange.global_trader_index, Account { data, ..index });
    if header.trader_state.is_hot() {
        return Ok(vec![index_write]);
    }
    // The local index was copied before the trader left it upstream, its account after. Phoenix
    // reads that account while its HOT flag is clear and copies the stale record over it on a
    // cancel, so the account goes back to hot, matching the index.
    let mut hot = header;
    hot.trader_state.quote_lot_collateral = bytemuck::cast(quote_lots);
    hot.trader_state.flags |= TRADER_CAPABILITY_HOT;
    let mut repaired = left;
    repaired.data[..size_of::<TraderHeader>()].copy_from_slice(bytemuck::bytes_of(&hot));
    Ok(vec![index_write, (trader, repaired)])
}

/// The Trader `account` with its collateral field set to `quote_lots`, or the refusal of a spline
/// market maker.
pub(crate) fn with_collateral(
    trader: Pubkey,
    account: &Account,
    quote_lots: i64,
) -> SurfpoolResult<Account> {
    let mut header = TraderHeader::try_read_from_account_bytes(&account.data).map_err(|e| {
        SurfpoolError::invalid_account_data(trader, "Expected a Phoenix Trader", Some(e))
    })?;
    if header.num_markets_with_splines > 0 {
        return Err(SurfpoolError::internal(format!(
            "{trader} quotes splines on {} markets; liquidating a spline market maker is not \
             supported",
            header.num_markets_with_splines
        )));
    }
    header.trader_state.quote_lot_collateral = bytemuck::cast(quote_lots);
    let mut data = account.data.clone();
    data[..size_of::<TraderHeader>()].copy_from_slice(bytemuck::bytes_of(&header));
    Ok(Account {
        data,
        ..account.clone()
    })
}

/// Phoenix looks a hot trader up in the index, and fails its cancels and liquidations with
/// TraderNotFound when it is missing there.
fn refuse_unindexed_hot_trader(
    trader: Pubkey,
    header: &TraderHeader,
    listed: bool,
) -> SurfpoolResult<()> {
    if header.trader_state.is_hot() && !listed {
        return Err(SurfpoolError::internal(format!(
            "{trader} is marked hot, but the local GlobalTraderIndex does not list it, so Phoenix \
             can neither cancel its orders nor liquidate it; the two accounts were fetched at \
             different times. Restart the surfnet (or reset the Phoenix accounts) and play again"
        )));
    }
    Ok(())
}

/// Whether the GlobalTraderIndex `index` lists `trader`, so Phoenix keeps its state there.
fn indexed(index: &Account, trader: &Pubkey) -> SurfpoolResult<bool> {
    Ok(index_record(index, trader)?.is_some())
}

/// The byte range of `trader`'s TraderState in the GlobalTraderIndex `index`, while it lists the
/// trader.
fn index_record(index: &Account, trader: &Pubkey) -> SurfpoolResult<Option<Range<usize>>> {
    Ok(index_trader_state_ranges(index)?
        .into_iter()
        .find_map(|(listed, range)| (listed == *trader).then_some(range)))
}

/// Every hot Trader the GlobalTraderIndex tree reaches, with its TraderState byte range. Freed
/// nodes keep stale keys and collateral, so the walk starts at the root; a repeated key fails.
pub fn index_trader_state_ranges(index: &Account) -> SurfpoolResult<Vec<(Pubkey, Range<usize>)>> {
    let invalid = || SurfpoolError::internal("Invalid Phoenix GlobalTraderIndex tree");
    if index.owner != PHOENIX_PROGRAM_ID {
        return Err(SurfpoolError::internal(
            "Expected a Phoenix-owned GlobalTraderIndex account",
        ));
    }
    let header = MultiArenaHeader::try_from_account_bytes(
        "GlobalTraderIndex",
        &index.data,
        PhoenixAccount::GlobalTraderIndexHeader.discriminant(),
    )
    .map_err(|error| {
        SurfpoolError::internal(format!("Invalid Phoenix GlobalTraderIndex: {error}"))
    })?;
    if header.num_arenas() != 1 || header.superblock().num_active_arenas() != 1 {
        return Err(SurfpoolError::internal(
            "Phoenix trader lookups require a single-arena GlobalTraderIndex",
        ));
    }
    // MultiArenaHeader (48), superblock (32), tree root and padding (16), then
    // 1-based Sokoban nodes: four u32 registers, a 32-byte key, and IDL TraderState.
    const NODES_START: usize = 96;
    const NODE_LEN: usize = 64;
    let data = &index.data;
    if data.len() < NODES_START || !(data.len() - NODES_START).is_multiple_of(NODE_LEN) {
        return Err(invalid());
    }
    let capacity = (data.len() - NODES_START) / NODE_LEN;
    let read_u32 = |offset| u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
    let mut pending = vec![read_u32(80)];
    let mut visited = HashSet::new();
    let mut keys = HashSet::new();
    let mut ranges = Vec::new();
    while let Some(node) = pending.pop() {
        if node == 0 {
            continue;
        }
        if node >= header.superblock().bump_index()
            || node as usize > capacity
            || !visited.insert(node)
        {
            return Err(invalid());
        }
        let start = NODES_START + (node as usize - 1) * NODE_LEN;
        pending.extend([read_u32(start), read_u32(start + 4)]);
        let key = Pubkey::new_from_array(data[start + 16..start + 48].try_into().unwrap());
        if !keys.insert(key) {
            return Err(invalid());
        }
        ranges.push((key, start + 48..start + 64));
    }
    if visited.len() != header.superblock().size() as usize {
        return Err(invalid());
    }
    Ok(ranges)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn index_trader_state_range(
        index: &Account,
        trader_key: &[u8; 32],
    ) -> SurfpoolResult<Range<usize>> {
        index_record(index, &Pubkey::new_from_array(*trader_key))?.ok_or_else(|| {
            SurfpoolError::internal("Hot Phoenix Trader has no reachable GlobalTraderIndex entry")
        })
    }

    #[test]
    fn a_view_with_appended_fields_decodes_and_a_short_one_is_refused() {
        let view = MarginView {
            magic: MARGIN_VIEW_MAGIC,
            collateral_quote_lots: 42,
            ..MarginView::zeroed()
        };
        let mut data = bytemuck::bytes_of(&view).to_vec();
        data.extend([7; 16]);
        let read = MarginView::from_return_data(&data).unwrap();
        assert_eq!(read.collateral_quote_lots, 42);

        let short = &data[..size_of::<MarginView>() - 1];
        assert!(MarginView::from_return_data(short).is_err());
        assert!(AssetView::from_return_data(&data).is_err(), "wrong magic");
    }

    const FIRST_KEY: [u8; 32] = [11; 32];
    const SECOND_KEY: [u8; 32] = [22; 32];

    fn write_u32(data: &mut [u8], offset: usize, value: u32) {
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn index_account() -> Account {
        let mut data = vec![0; 96 + 3 * 64];
        data[..8].copy_from_slice(&PhoenixAccount::GlobalTraderIndexHeader.discriminant());
        write_u32(&mut data, 48, 2);
        data[52..54].copy_from_slice(&1_u16.to_le_bytes());
        data[54..56].copy_from_slice(&1_u16.to_le_bytes());
        write_u32(&mut data, 56, 3);
        write_u32(&mut data, 60, 4);
        write_u32(&mut data, 64, 3);
        write_u32(&mut data, 80, 2);
        write_u32(&mut data, 160, 1);
        write_u32(&mut data, 104, 2);
        for (slot, key, collateral) in [
            (0, FIRST_KEY, 111_i64),
            (1, SECOND_KEY, 222_i64),
            (2, FIRST_KEY, 999_i64),
        ] {
            let start = 96 + slot * 64;
            data[start + 16..start + 48].copy_from_slice(&key);
            data[start + 48..start + 56].copy_from_slice(&collateral.to_le_bytes());
            write_u32(&mut data, start + 56, TRADER_CAPABILITY_HOT);
        }
        Account {
            data,
            owner: PHOENIX_PROGRAM_ID,
            ..Account::default()
        }
    }

    #[test]
    fn a_hot_trader_the_index_does_not_list_is_refused() {
        let index = index_account();
        let mut header = TraderHeader::zeroed();
        header.trader_state.flags = TRADER_CAPABILITY_HOT;
        let listed = Pubkey::new_from_array(FIRST_KEY);
        let missing = Pubkey::new_unique();
        let in_index = |trader: &Pubkey| indexed(&index, trader).unwrap();
        assert!(refuse_unindexed_hot_trader(listed, &header, in_index(&listed)).is_ok());
        let error = refuse_unindexed_hot_trader(missing, &header, in_index(&missing))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!(
                "{missing} is marked hot, but the local GlobalTraderIndex does not list it"
            )),
            "{error}"
        );
        header.trader_state.flags = 0;
        assert!(
            refuse_unindexed_hot_trader(missing, &header, in_index(&missing)).is_ok(),
            "a cold trader is not looked up"
        );
    }

    fn trader_account(header: TraderHeader) -> Account {
        let mut data = bytemuck::bytes_of(&header).to_vec();
        // The position map follows the header and must stay as it is.
        data.extend([7; 48]);
        Account {
            lamports: 1,
            data,
            owner: PHOENIX_PROGRAM_ID,
            ..Account::default()
        }
    }

    fn cold_trader_header() -> TraderHeader {
        let mut header = TraderHeader::zeroed();
        header.discriminant = u64::from_le_bytes(PhoenixAccount::Trader.discriminant());
        header.trader_state.quote_lot_collateral = bytemuck::cast(123_i64);
        header
    }

    #[test]
    fn collateral_lands_where_the_idl_puts_trader_state_quote_lot_collateral() {
        let trader = Pubkey::new_unique();
        let account = trader_account(cold_trader_header());
        let quote_lots = -5_030_916_850_i64;

        let written = with_collateral(trader, &account, quote_lots).unwrap();

        // Discriminant (8), sequence number (16), key and authority (32 each), then TraderState,
        // which opens with the collateral.
        assert_eq!(written.data[88..96], quote_lots.to_le_bytes());
        let changed: Vec<usize> = (0..account.data.len())
            .filter(|index| written.data[*index] != account.data[*index])
            .collect();
        assert!(
            changed.iter().all(|index| (88..96).contains(index)),
            "{changed:?}"
        );
        assert_eq!(
            (written.lamports, written.owner),
            (account.lamports, account.owner)
        );

        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let through_idl = svm
            .get_forged_account_data(
                &trader,
                &account.data,
                super::super::state_builder::phoenix_idl(),
                &std::collections::HashMap::from([(
                    "traderState.quoteLotCollateral".to_string(),
                    serde_json::json!(quote_lots),
                )]),
            )
            .unwrap();
        assert_eq!(written.data, through_idl);
    }

    #[test]
    fn the_idl_writes_max_positions_and_leaves_the_trader_preference_bits() {
        let mut header = cold_trader_header();
        header.max_positions = 128;
        header.trader_preference_bits = 5;
        let account = trader_account(header);

        let (svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        let written = svm
            .get_forged_account_data(
                &Pubkey::new_unique(),
                &account.data,
                super::super::state_builder::phoenix_idl(),
                &std::collections::HashMap::from([(
                    "maxPositions".to_string(),
                    serde_json::json!(10),
                )]),
            )
            .unwrap();
        let header: TraderHeader =
            bytemuck::pod_read_unaligned(&written[..size_of::<TraderHeader>()]);
        assert_eq!(
            (header.max_positions, header.trader_preference_bits),
            (10, 5)
        );
    }

    #[test]
    fn collateral_refuses_a_spline_market_maker() {
        let mut maker = cold_trader_header();
        maker.num_markets_with_splines = 3;
        let trader = Pubkey::new_unique();
        let error = with_collateral(trader, &trader_account(maker), 1)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!(
                "{trader} quotes splines on 3 markets; liquidating a spline market maker is not \
                 supported"
            )),
            "{error}"
        );
    }

    #[test]
    fn index_lookup_selects_reachable_keys_and_rejects_malformed_trees() {
        let index = index_account();
        assert_eq!(
            index_trader_state_range(&index, &FIRST_KEY).unwrap(),
            144..160
        );
        assert_eq!(
            index_trader_state_range(&index, &SECOND_KEY).unwrap(),
            208..224
        );
        assert!(index_trader_state_range(&index, &[44; 32]).is_err());

        type Corrupt = fn(&mut Account);
        let corruptions: [(&str, Corrupt); 8] = [
            ("only a freed duplicate matches", |index| {
                index.data[112..144].copy_from_slice(&[33; 32])
            }),
            ("cycle", |index| write_u32(&mut index.data, 96, 2)),
            ("size disagrees with the reachable nodes", |index| {
                write_u32(&mut index.data, 48, 3)
            }),
            ("child beyond capacity", |index| {
                write_u32(&mut index.data, 60, 100);
                write_u32(&mut index.data, 164, 4);
            }),
            ("child above the bump index", |index| {
                write_u32(&mut index.data, 60, 2)
            }),
            ("duplicate reachable key", |index| {
                index.data[176..208].copy_from_slice(&FIRST_KEY)
            }),
            ("wrong owner", |index| index.owner = Pubkey::new_unique()),
            ("wrong discriminator", |index| index.data[..8].fill(0)),
        ];
        for (case, corrupt) in corruptions {
            let mut index = index_account();
            corrupt(&mut index);
            assert!(
                index_trader_state_range(&index, &FIRST_KEY).is_err(),
                "{case}"
            );
        }
        for (arenas, active) in [(0_u16, 1_u16), (2, 1), (1, 0), (1, 2)] {
            let mut index = index_account();
            index.data[52..54].copy_from_slice(&arenas.to_le_bytes());
            index.data[54..56].copy_from_slice(&active.to_le_bytes());
            assert!(index_trader_state_range(&index, &FIRST_KEY).is_err());
        }
        for len in [0, 79, 95, 96 + 3 * 64 - 1] {
            let mut index = index_account();
            index.data.truncate(len);
            assert!(index_trader_state_range(&index, &FIRST_KEY).is_err());
        }
    }
}
