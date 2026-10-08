use core::mem::size_of;
use std::collections::HashMap;

use phoenix_rise_accounts::{
    PhoenixAccount, PhoenixAccountDecodeError,
    perp_asset_map::{PerpAssetMap, PerpAssetMetadata, PriceComponent},
    trader::TraderHeader,
};
use solana_account::Account;
use solana_clock::Clock;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use surfpool_types::{AccountAddress, OverrideInstance, Scenario};

use super::collateral::{
    current_quote_lot_collateral, ensure_collateral_floor, index_trader_state_range,
    index_trader_state_ranges, parse_quote_lot_collateral, trader_header,
    validate_hot_trader_fields,
};
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    surfnet::{remote::SurfnetRemoteClient, svm::SurfnetSvm},
};

pub const PHOENIX_ETERNAL_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("EtrnLzgbS7nMMy5fbD42kXiUzGg8XQzJ972Xtk1cjWih");
// Singletons that GlobalConfig points at; the mainnet tests check them against GlobalConfig.
pub const PHOENIX_PERP_ASSET_MAP: Pubkey =
    Pubkey::from_str_const("2nHGAaEw3D5dd4hVueaUNoygkQFmoeKqRQWnSPqSMFUC");
pub const PHOENIX_GLOBAL_TRADER_INDEX: Pubkey =
    Pubkey::from_str_const("HCrPXLByGqRh2szQi3gj7oRdRVBNi1gccAyn4CQCT3HK");

fn phoenix_account_kind(data: &[u8]) -> Option<PhoenixAccount> {
    PhoenixAccount::from_discriminant(data.get(..8)?.try_into().unwrap())
}

const COLLATERAL_TEMPLATE_ID: &str = "phoenix-trader-collateral-stress";
const COLLATERAL_FIELD: &str = "traderState.quoteLotCollateral";
const MARKET_SYMBOL_FIELD: &str = "symbol";
const DIRECT_MARK_TICKS_FIELD: &str = "target_ticks";
const MAINTENANCE_FACTOR_FIELD: &str = "maintenance_risk_factor_bps";
const MAX_RISK_FACTOR_BPS: u16 = 10_000;
const PREPARATION_SLOT: u64 = 0;
/// Phoenix refuses a market once its readings are older than its stale threshold times its `u8`
/// `oracle_hard_stale_multiplier`, or than the threshold alone when that is 0. `u32::MAX` puts the
/// limit beyond any session and keeps within a `u64`.
const RAISED_STALE_THRESHOLD_SLOTS: u64 = u32::MAX as u64;

/// What a caller can name a market by, and the current values a relative change starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoenixMarket {
    pub symbol: String,
    pub orderbook: Pubkey,
    pub mark_ticks: u64,
    pub tick_size: u64,
    pub base_lot_decimals: i8,
    pub maintenance_risk_factor_bps: u16,
    pub backstop_risk_factor_bps: u16,
}

pub fn phoenix_markets(
    perp_asset_map: Pubkey,
    account: &Account,
) -> SurfpoolResult<Vec<PhoenixMarket>> {
    if account.owner != PHOENIX_ETERNAL_PROGRAM_ID {
        return Err(SurfpoolError::invalid_account_owner(
            perp_asset_map,
            None::<PhoenixAccountDecodeError>,
        ));
    }
    let invalid = |error: PhoenixAccountDecodeError| {
        SurfpoolError::invalid_account_data(
            perp_asset_map,
            "Expected a valid Phoenix Eternal PerpAssetMap account",
            Some(error),
        )
    };
    let map = PerpAssetMap::try_from_account_bytes(&account.data).map_err(invalid)?;
    let mut markets = map
        .iter()
        .map(|entry| {
            entry.map(|entry| PhoenixMarket {
                symbol: entry.symbol.as_str().to_string(),
                orderbook: Pubkey::new_from_array(
                    entry.metadata.static_market_params().market_account,
                ),
                mark_ticks: entry
                    .metadata
                    .oracle_price()
                    .mark_price
                    .price
                    .ticks
                    .as_inner(),
                tick_size: entry.metadata.static_market_params().tick_size.as_inner(),
                base_lot_decimals: entry.metadata.static_market_params().base_lot_decimals,
                maintenance_risk_factor_bps: entry.metadata.risk_params().risk_factors[0],
                backstop_risk_factor_bps: entry.metadata.risk_params().risk_factors[1],
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid)?;
    markets.sort_unstable_by(|left, right| left.symbol.cmp(&right.symbol));

    Ok(markets)
}

fn price_patch_error(account_pubkey: &Pubkey, message: impl core::fmt::Display) -> SurfpoolError {
    SurfpoolError::invalid_account_data(
        account_pubkey,
        "Expected a valid Phoenix Eternal PerpAssetMap account",
        Some(message),
    )
}

fn checked_ticks(ticks: u64) -> SurfpoolResult<u64> {
    // Margin is the mark times the tick size, so a zero mark would zero the margin of every
    // position in the market instead of shocking it.
    if ticks == 0 || ticks > u64::from(u32::MAX) {
        return Err(SurfpoolError::internal(format!(
            "price ticks {ticks} are outside the Phoenix mark range 1..={}",
            u32::MAX
        )));
    }
    Ok(ticks)
}

fn patch_direct_mark(
    account_pubkey: &Pubkey,
    data: &[u8],
    symbol: &str,
    target_ticks: u64,
    mark_slot: u64,
) -> SurfpoolResult<Vec<u8>> {
    let target_ticks = checked_ticks(target_ticks)?;
    patch_market_metadata(account_pubkey, data, symbol, |_, bytes| {
        let price_len = size_of::<PriceComponent>();
        let mut price = bytemuck::pod_read_unaligned::<PriceComponent>(&bytes[..price_len]);
        price.mark_price.price.slot = mark_slot;
        price.mark_price.price.ticks = bytemuck::cast(target_ticks);
        // Phoenix rebuilds the mark from its oracle inputs on every trade, so they carry the
        // shock too; the book input is clamped around them and needs no write.
        let mark = &mut price.mark_price;
        for sample in mark
            .spot_price_component
            .last_exchange_spot_price
            .iter_mut()
            .chain(
                mark.perp_price_component
                    .last_exchange_perp_price
                    .iter_mut(),
            )
        {
            sample.ticks = mark.price.ticks;
            sample.slot = mark_slot;
        }
        mark.spot_price_component.slot = mark_slot;
        bytes[..price_len].copy_from_slice(bytemuck::bytes_of(&price));
        Ok(())
    })
}

fn patch_maintenance_factor(
    account_pubkey: &Pubkey,
    data: &[u8],
    symbol: &str,
    factor: u16,
) -> SurfpoolResult<Vec<u8>> {
    patch_market_metadata(account_pubkey, data, symbol, |metadata, bytes| {
        // risk_factors is [maintenance, backstop, high_risk], and the risk tier is checked from
        // the backstop up, so a maintenance factor at or below the backstop one leaves no
        // Liquidatable tier.
        let backstop = metadata.risk_params().risk_factors[1];
        if factor <= backstop || factor > MAX_RISK_FACTOR_BPS {
            return Err(SurfpoolError::internal(format!(
                "{MAINTENANCE_FACTOR_FIELD} {factor} must be above {symbol}'s backstop factor \
                 {backstop} and at most {MAX_RISK_FACTOR_BPS}"
            )));
        }
        // The metadata layout type is private to the crate, so the field offset comes from the view.
        let offset = metadata.risk_params().risk_factors.as_ptr() as usize
            - metadata.as_bytes().as_ptr() as usize;
        bytes[offset..offset + 2].copy_from_slice(&factor.to_le_bytes());
        Ok(())
    })
}

/// The map with every market's spot and perp oracle stale threshold raised to
/// [`RAISED_STALE_THRESHOLD_SLOTS`], or `None` when they already are. A higher threshold is kept.
fn raise_stale_thresholds(account_pubkey: &Pubkey, data: &[u8]) -> SurfpoolResult<Option<Vec<u8>>> {
    let decode_error = |error: PhoenixAccountDecodeError| {
        price_patch_error(
            account_pubkey,
            format!("invalid Phoenix PerpAssetMap account: {error}"),
        )
    };
    let map = PerpAssetMap::try_from_account_bytes(data).map_err(decode_error)?;
    let price_len = size_of::<PriceComponent>();
    let mut patched: Option<Vec<u8>> = None;
    // The decoder walks the entries in storage order, and each market's metadata holds its own
    // market account, so it occurs once in the map
    let mut cursor = 0;
    for entry in map.iter() {
        let metadata = entry.map_err(decode_error)?.metadata;
        let metadata_bytes = metadata.as_bytes();
        let offset = data[cursor..]
            .windows(metadata_bytes.len())
            .position(|window| window == metadata_bytes)
            .map(|position| cursor + position)
            .ok_or_else(|| {
                price_patch_error(account_pubkey, "Phoenix market metadata was not found")
            })?;
        cursor = offset + metadata_bytes.len();

        // The PriceComponent is the metadata's first field.
        let mut price = *metadata.oracle_price();
        let mark = &mut price.mark_price;
        let mut raised = false;
        for threshold in [
            &mut mark.spot_price_component.stale_threshold,
            &mut mark.perp_price_component.stale_threshold,
        ] {
            if *threshold < RAISED_STALE_THRESHOLD_SLOTS {
                *threshold = RAISED_STALE_THRESHOLD_SLOTS;
                raised = true;
            }
        }
        if raised {
            patched.get_or_insert_with(|| data.to_vec())[offset..offset + price_len]
                .copy_from_slice(bytemuck::bytes_of(&price));
        }
    }
    Ok(patched)
}

fn patch_market_metadata(
    account_pubkey: &Pubkey,
    data: &[u8],
    symbol: &str,
    update: impl FnOnce(&PerpAssetMetadata, &mut [u8]) -> SurfpoolResult<()>,
) -> SurfpoolResult<Vec<u8>> {
    let decode_error = |error: PhoenixAccountDecodeError| {
        price_patch_error(
            account_pubkey,
            format!("invalid Phoenix PerpAssetMap account: {error}"),
        )
    };
    let map = PerpAssetMap::try_from_account_bytes(data).map_err(decode_error)?;
    let entry = map
        .find_by_symbol(symbol)
        .map_err(decode_error)?
        .ok_or_else(|| SurfpoolError::internal(format!("Phoenix market {symbol} was not found")))?;
    let metadata_bytes = entry.metadata.as_bytes();
    let metadata_offset = unique_subslice_offset(data, metadata_bytes).ok_or_else(|| {
        price_patch_error(
            account_pubkey,
            "selected Phoenix market metadata does not occur exactly once",
        )
    })?;
    let mut patched = data.to_vec();
    update(
        &entry.metadata,
        &mut patched[metadata_offset..metadata_offset + metadata_bytes.len()],
    )?;
    Ok(patched)
}

fn unique_subslice_offset(data: &[u8], needle: &[u8]) -> Option<usize> {
    let mut matches = data
        .windows(needle.len())
        .enumerate()
        .filter(|(_, window)| *window == needle)
        .map(|(offset, _)| offset);
    let offset = matches.next()?;
    matches.next().is_none().then_some(offset)
}

fn forge_phoenix_override(
    account_pubkey: &Pubkey,
    account: &Account,
    account_values: &HashMap<String, serde_json::Value>,
    mark_slot: u64,
) -> SurfpoolResult<Vec<u8>> {
    // Only the codec's inputs are read: other keys, such as PerpAssetMap fields a client copied
    // from the decoded account, cannot be written through this codec.
    let symbol = || {
        account_values
            .get(MARKET_SYMBOL_FIELD)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| SurfpoolError::internal("symbol must be a non-empty string"))
    };
    match (
        account_values.get(DIRECT_MARK_TICKS_FIELD),
        account_values.get(MAINTENANCE_FACTOR_FIELD),
    ) {
        (Some(ticks), None) => patch_direct_mark(
            account_pubkey,
            &account.data,
            symbol()?,
            parse_decimal(ticks, DIRECT_MARK_TICKS_FIELD, "an unsigned 64-bit integer")?,
            mark_slot,
        ),
        (None, Some(factor)) => patch_maintenance_factor(
            account_pubkey,
            &account.data,
            symbol()?,
            parse_decimal(factor, MAINTENANCE_FACTOR_FIELD, "basis points")?,
        ),
        _ => Err(SurfpoolError::internal(
            "Phoenix map overrides take symbol plus exactly one of target_ticks or \
             maintenance_risk_factor_bps",
        )),
    }
}

/// The writes a Phoenix override needs, or `None` when the account takes the generic IDL path.
/// Every Phoenix override also leaves the local PerpAssetMap's markets usable for the rest of the
/// session; see `keep_oracle_readings_usable`.
pub async fn prepare_phoenix_override(
    svm: &mut SurfnetSvm,
    account_pubkey: &Pubkey,
    account: &Account,
    values: &HashMap<String, serde_json::Value>,
    remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    fetch_before_use: bool,
) -> SurfpoolResult<Option<Vec<(Pubkey, Account)>>> {
    if account.owner != PHOENIX_ETERNAL_PROGRAM_ID {
        return Ok(None);
    }
    match phoenix_account_kind(&account.data) {
        Some(PhoenixAccount::PerpAssetMap) => {
            let mark_slot = svm.inner.get_sysvar::<Clock>().slot;
            let data = forge_phoenix_override(account_pubkey, account, values, mark_slot)?;
            // This override writes the map Phoenix reads, so the thresholds go in the same write.
            let data = match raise_stale_thresholds(account_pubkey, &data) {
                Ok(raised) => raised.unwrap_or(data),
                Err(e) => {
                    warn!(
                        "Could not raise the Phoenix PerpAssetMap's oracle stale thresholds: {e}"
                    );
                    data
                }
            };
            Ok(Some(vec![(
                *account_pubkey,
                Account {
                    data,
                    ..account.clone()
                },
            )]))
        }
        kind => {
            keep_oracle_readings_usable(svm, remote_ctx).await;
            match kind {
                Some(PhoenixAccount::Trader) => prepare_trader_override(
                    svm,
                    account_pubkey,
                    account,
                    values,
                    remote_ctx,
                    fetch_before_use,
                )
                .await
                .map(Some),
                _ => Ok(None),
            }
        }
    }
}

/// Oracle updates keep every market's readings fresh, and Phoenix refuses a market whose readings
/// are too old (see [`RAISED_STALE_THRESHOLD_SLOTS`]). Nothing refreshes them locally, so
/// this raises the thresholds of the local PerpAssetMap, fetching it first when it is not local
/// yet. Prices and reading slots stay as they were, and the markets stay usable however far the
/// local Clock moves. A failure leaves the map as it was and never fails the override. It runs on
/// every Phoenix override, before that override's own checks, since the map is needed whether or
/// not the override applies; each run reads the whole map.
async fn keep_oracle_readings_usable(
    svm: &mut SurfnetSvm,
    remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
) {
    let kept = async {
        let map = phoenix_dependency(svm, &PHOENIX_PERP_ASSET_MAP, remote_ctx).await?;
        let Some(data) = raise_stale_thresholds(&PHOENIX_PERP_ASSET_MAP, &map.data)? else {
            return Ok(());
        };
        svm.set_account(&PHOENIX_PERP_ASSET_MAP, Account { data, ..map })
    }
    .await;
    if let Err(e) = kept {
        warn!("Could not raise the Phoenix PerpAssetMap's oracle stale thresholds: {e}");
    }
}

async fn prepare_trader_override(
    svm: &mut SurfnetSvm,
    trader: &Pubkey,
    account: &Account,
    values: &HashMap<String, serde_json::Value>,
    remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
    fetch_before_use: bool,
) -> SurfpoolResult<Vec<(Pubkey, Account)>> {
    let header = trader_header(trader, account)?;
    let hot = header.trader_state.is_hot();
    // Ahead of every check, so a refused override still leaves the fresh record behind.
    if fetch_before_use {
        refresh_index_record(svm, &header, remote_ctx).await?;
    }
    let listed = !hot
        && svm
            .inner
            .get_account(&PHOENIX_GLOBAL_TRADER_INDEX)?
            .is_some_and(|index| index_trader_state_range(&index, &header.key).is_ok());
    if hot || listed {
        validate_hot_trader_fields(values)?;
    }
    let target = values
        .get(COLLATERAL_FIELD)
        .map(parse_quote_lot_collateral)
        .transpose()?;
    // A hot Trader's collateral is read from its GlobalTraderIndex record, so the record is what
    // the target is checked against and patched in.
    let index = match target {
        Some(_) if hot => {
            Some(phoenix_dependency(svm, &PHOENIX_GLOBAL_TRADER_INDEX, remote_ctx).await?)
        }
        // The local GlobalTraderIndex can predate the Trader leaving the hot set, and Phoenix
        // keeps reading a Trader the index lists from its record.
        Some(_) => svm
            .inner
            .get_account(&PHOENIX_GLOBAL_TRADER_INDEX)?
            .filter(|index| index_trader_state_range(index, &header.key).is_ok()),
        _ => None,
    };
    let mut values = values.clone();
    if let Some(target) = target {
        ensure_collateral_is_lowered(
            current_quote_lot_collateral(&header, index.as_ref())?,
            target,
        )?;
        values.insert(
            COLLATERAL_FIELD.to_string(),
            serde_json::Value::from(target),
        );
    }
    let idl_versions = svm
        .registered_idls
        .get(&PHOENIX_ETERNAL_PROGRAM_ID.to_string())?
        .unwrap_or_default();
    let idl = &idl_versions
        .first()
        .ok_or_else(|| SurfpoolError::internal("No IDL registered for Phoenix Eternal"))?
        .1;
    let data = svm.get_forged_account_data(trader, &account.data, idl, &values)?;
    // A listed Trader is capped by its index record above, not by its account copy.
    if !hot && !listed {
        let forged = TraderHeader::try_read_from_account_bytes(&data).map_err(|error| {
            SurfpoolError::invalid_account_data(
                trader,
                "Expected a valid Phoenix Eternal Trader account",
                Some(error),
            )
        })?;
        // Phoenix looks a hot-flagged Trader up in the GlobalTraderIndex, which does not list
        // this one, so every later transaction for it would fail.
        if forged.trader_state.is_hot() {
            return Err(SurfpoolError::internal(
                "Phoenix traderState.flags must keep the HOT bit clear on a Trader the \
                 GlobalTraderIndex does not list",
            ));
        }
        let collateral = forged.trader_state.quote_lot_collateral.as_inner();
        ensure_collateral_floor(collateral)?;
        ensure_collateral_is_lowered(
            header.trader_state.quote_lot_collateral.as_inner(),
            collateral,
        )?;
    }

    let mut writes = Vec::new();
    if let (Some(mut index), Some(collateral)) = (index, values.get(COLLATERAL_FIELD)) {
        let range = index_trader_state_range(&index, &header.key)?;
        let encoded = SurfnetSvm::get_forged_idl_type_data(
            &index.data[range.clone()],
            idl,
            "TraderState",
            &HashMap::from([("quoteLotCollateral".to_string(), collateral.clone())]),
        )?;
        if encoded.len() != range.len() {
            return Err(SurfpoolError::internal(format!(
                "the re-encoded TraderState is {} bytes, the GlobalTraderIndex record holds {}",
                encoded.len(),
                range.len()
            )));
        }
        index.data[range].copy_from_slice(&encoded);
        writes.push((PHOENIX_GLOBAL_TRADER_INDEX, index));
    }
    writes.push((
        *trader,
        Account {
            data,
            ..account.clone()
        },
    ));
    Ok(writes)
}

/// Phoenix reads a Trader's collateral from its GlobalTraderIndex record whenever the local index
/// lists it, so fetchBeforeUse refreshes the collateral there too: from the upstream record, or from
/// the refetched Trader account when a cold Trader has left the upstream index. The rest of the
/// record stays as the local VM has it, and so does the whole record if the fetch fails. The
/// refreshed record is stored at once, as core stores the refetched Trader, so later overrides in
/// the same slot are checked against it.
async fn refresh_index_record(
    svm: &mut SurfnetSvm,
    header: &TraderHeader,
    remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
) -> SurfpoolResult<()> {
    let Some((client, commitment)) = remote_ctx else {
        return Ok(());
    };
    // An index not held locally yet is fetched whole, and so fresh, when the override needs it.
    let Some(mut index) = svm.inner.get_account(&PHOENIX_GLOBAL_TRADER_INDEX)? else {
        return Ok(());
    };
    let Ok(local) = index_trader_state_range(&index, &header.key) else {
        return Ok(());
    };
    let Ok(Ok(remote)) = client
        .get_account(&PHOENIX_GLOBAL_TRADER_INDEX, *commitment)
        .await
        .map(|fetched| fetched.map_account())
    else {
        return Ok(());
    };
    let Ok(upstream_records) = index_trader_state_ranges(&remote) else {
        return Ok(());
    };
    let trader = Pubkey::new_from_array(header.key);
    let account_collateral = header
        .trader_state
        .quote_lot_collateral
        .as_inner()
        .to_le_bytes();
    let fresh = match upstream_records.into_iter().find(|(key, _)| *key == trader) {
        Some((_, upstream)) if upstream.len() == local.len() => {
            &remote.data[upstream.start..upstream.start + 8]
        }
        // A cold Trader that left the upstream index keeps its collateral in its own account, which
        // core just refetched. A hot Trader's account copy is stale, so its record stays.
        None if !header.trader_state.is_hot() => &account_collateral[..],
        _ => return Ok(()),
    };
    index.data[local.start..local.start + 8].copy_from_slice(fresh);
    svm.set_account(&PHOENIX_GLOBAL_TRADER_INDEX, index)?;
    Ok(())
}

async fn phoenix_dependency(
    svm: &mut SurfnetSvm,
    address: &Pubkey,
    remote_ctx: &Option<(SurfnetRemoteClient, CommitmentConfig)>,
) -> SurfpoolResult<Account> {
    if let Some(account) = svm.inner.get_account(address)? {
        return Ok(account);
    }
    if svm.offline_accounts.contains_key(&address.to_string())?
        || svm
            .offline_accounts
            .get(&PHOENIX_ETERNAL_PROGRAM_ID.to_string())?
            .is_some_and(|config| config.include_owned_accounts)
    {
        return Err(SurfpoolError::internal(format!(
            "Phoenix dependency {address} is offline and missing locally"
        )));
    }
    let (client, commitment) = remote_ctx.as_ref().ok_or_else(|| {
        SurfpoolError::internal(format!("Phoenix dependency {address} is missing locally"))
    })?;
    let fetched = client.get_account(address, *commitment).await?;
    let account = fetched.clone().map_account()?;
    // Fetch the dependency once instead of per override, the way any read from the upstream
    // datasource does: the account is also indexed by owner, so getProgramAccounts serves the
    // local copy the override then patches.
    svm.hydrate_scenario_account(address, account.clone())?;
    Ok(account)
}

/// Raising collateral needs a real deposit into the global vault.
fn ensure_collateral_is_lowered(
    current_quote_lots: i64,
    target_quote_lots: i64,
) -> SurfpoolResult<()> {
    if target_quote_lots > current_quote_lots {
        return Err(SurfpoolError::internal(format!(
            "Phoenix collateral stress can only lower collateral: {current_quote_lots} quote lots \
             are backed by the global vault, {target_quote_lots} would not be. Deposit first to \
             raise it."
        )));
    }
    Ok(())
}

pub fn build_phoenix_collateral_scenario(
    trader: Pubkey,
    trader_account: &Account,
    target_quote_lots: &str,
) -> SurfpoolResult<Scenario> {
    let target_quote_lots = parse_quote_lot_collateral(&serde_json::json!(target_quote_lots))?;
    trader_header(&trader, trader_account)?;

    let values = HashMap::from([(
        COLLATERAL_FIELD.to_string(),
        serde_json::json!(target_quote_lots.to_string()),
    )]);
    let mut collateral_override = OverrideInstance::new(
        COLLATERAL_TEMPLATE_ID.to_string(),
        PREPARATION_SLOT,
        AccountAddress::Pubkey(trader.to_string()),
    )
    .with_values(values)
    .with_label("Phoenix Trader collateral stress".to_string());
    collateral_override.fetch_before_use = true;

    let mut scenario = Scenario::new(
        "Phoenix Trader Collateral Stress".to_string(),
        "Set exact signed quote-lot collateral on a Phoenix Trader, and on its record in the local GlobalTraderIndex when the index lists it."
            .to_string(),
    );
    scenario.tags = vec![
        "phoenix-eternal".to_string(),
        "collateral".to_string(),
        "risk".to_string(),
    ];
    scenario.add_override(collateral_override);

    Ok(scenario)
}

fn parse_decimal<T: core::str::FromStr>(
    value: &serde_json::Value,
    field: &str,
    expected: &str,
) -> SurfpoolResult<T> {
    let parsed = match value {
        serde_json::Value::String(text) => text.parse().ok(),
        serde_json::Value::Number(number) if number.is_u64() => number.to_string().parse().ok(),
        _ => None,
    };
    parsed.ok_or_else(|| {
        SurfpoolError::internal(format!(
            "{field} must be {expected}, as a decimal string or a whole number"
        ))
    })
}

#[cfg(test)]
mod tests {
    use base64::{Engine, prelude::BASE64_STANDARD};
    use phoenix_rise_accounts::{PhoenixAccount, trader::TraderHeader};
    use solana_account::Account;

    use super::*;
    use crate::scenarios::TemplateRegistry;

    const TRADER_HEADER_LEN: usize = size_of::<TraderHeader>();
    const COLLATERAL_BYTE_RANGE: core::ops::Range<usize> = 88..96;
    const POSITION_MAP_PREFIX_LEN: usize = 16;
    const POSITION_ENTRY_LEN: usize = 40;
    const PERP_ASSET_MAP_LEN: usize = 1_622_064;
    const SOL_PERP_ASSET_MAP_PREFIX_B64: &str = "jjZz33zvbCYBAAAAAAAAAF+FshYAAAAALAAAAAAAAAAtAAAAAQAAAAAEAAAAAAAAU09MAAAAAAAAAAAAAAAAAJIjVQMAAAAAK+yGGQAAAAAr7IYZAAAAABccAAAAAAAAK+yGGQAAAAAXHAAAAAAAACXshhkAAAAAFxwAAAAAAAAk7IYZAAAAABccAAAAAAAAIuyGGQAAAAAWHAAAAAAAACnshhkAAAAAGBwAAAAAAABkAAAAAAAAABkAAAAAAAAAK+yGGQAAAAAAAAAAAAAAAHUAAAAAAAAAdwEAAAAAAAByAQAAAAAAACvshhkAAAAAERwAAAAAAAAl7IYZAAAAABEcAAAAAAAAJOyGGQAAAAASHAAAAAAAACLshhkAAAAAERwAAAAAAAAp7IYZAAAAABEcAAAAAAAAZAAAAAAAAAAZAAAAAAAAACvshhkAAAAAGRwAAAAAAABkAAAAAAAAAGQAAAAAAAAAcgEAAAAAAAAk7IYZAAAAABkcAAAAAAAAJOyGGQAAAAAaHAAAAAAAAPjrhhkAAAAAFBwAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAICAQAAAAAAAgIBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgIBAAAAAAACAgEAAAAAAAAAAAAAAAAAAAAAAAAAAAACAgEAAAAAAAICAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAICAQAAAAAAAgIBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgIBAAAAAAACAgEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2d/uHkzTMEI+nE0Ymaus9KEPf4oJXVEtWcWP29rQOywAAAAAAAAAAPBv/oFQVyUzn/BwmYuYTfvalmqearF4UMH8Xu10jPi1AAAAAAAAAAC9eIxYdxEuqtIqoFaGCUmDIS3Ki2887zwxOIzii37LZgAAAAAAAAAAp5Qc5gqxc5w9o5gk0/YHpMTClPTT8zaXjCVPspfSfxwAAAAAAAAAAIj2IrJxxvwcSeH0Zi3/xWcn5icVCYuh/OncuwHqSRBjAAAAAAAAAAD0AQEAAAAAACvshhkAAAAAnI6GGQAAAAAAAAAAAAAAAFRyhhkAAAAA5wAF8p4BAAAh9gTyngEAAFj1BPKeAQAAxu8E8p4BAAC6/ATyngEAAAAAAAAAAAAAAAAAAAAAAABZQzBUxbJLqOqoIX/f+QNvuZxLwZEZqXGqSDPsYEwjH2QAAAAAAAAAAAACAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAATMvqAQAAAAAPAAAAAAAAABAnAAAAAAAATcvqAQAAAAABAAAAAAAAABAnAAAAAAAATsvqAQAAAAABAAAAAAAAABAnAAAAAAAAT8vqAQAAAAABAAAAAAAAABAnAAAAAAAAECcAAAAAAABQwwAAAAAAAKCGAQAAAAAAZAAAAAAAAAAgoQcAAAAAAMgAAAAAAAAAQEIPAAAAAAAsAQAAAAAAAICWmAAAAAAAkAEAAAAAAACIE9AH6ANMHWQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAVFYAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAKCQAAAAAAAJDaOWoAAAAAatw5agAAAAAQDgAAAAAAAIBRAQAAAAAAogYAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAnuYhAAAAAABMy+oBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAQlRDAAAAAAAAAAAAAAAAAA==";

    fn trader_fixture(collateral: i64, len: u64, capacity: u64) -> Vec<u8> {
        let capacity = usize::try_from(capacity).expect("fixture capacity");
        let mut data =
            vec![0_u8; TRADER_HEADER_LEN + POSITION_MAP_PREFIX_LEN + capacity * POSITION_ENTRY_LEN];
        data[..8].copy_from_slice(&PhoenixAccount::Trader.discriminant());
        data[COLLATERAL_BYTE_RANGE].copy_from_slice(&collateral.to_le_bytes());
        data[112..116].copy_from_slice(&(capacity as u32).to_le_bytes());
        data[TRADER_HEADER_LEN..TRADER_HEADER_LEN + 8].copy_from_slice(&len.to_le_bytes());
        data[TRADER_HEADER_LEN + 8..TRADER_HEADER_LEN + 16]
            .copy_from_slice(&(capacity as u64).to_le_bytes());
        if len > 0 && capacity > 0 {
            data[TRADER_HEADER_LEN + POSITION_MAP_PREFIX_LEN
                ..TRADER_HEADER_LEN + POSITION_MAP_PREFIX_LEN + 8]
                .copy_from_slice(&42_u64.to_le_bytes());
        }
        data
    }

    fn trader_account() -> Account {
        Account {
            lamports: 1,
            data: trader_fixture(0, 1, 2),
            owner: PHOENIX_ETERNAL_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        }
    }

    fn trader_account_for(trader: Pubkey, collateral: i64) -> Account {
        let mut account = Account {
            data: trader_fixture(collateral, 1, 2),
            ..trader_account()
        };
        account.data[24..56].copy_from_slice(trader.as_ref());
        account
    }

    fn perp_asset_map_fixture() -> Vec<u8> {
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

    fn perp_asset_map_account() -> Account {
        Account {
            lamports: 1,
            data: perp_asset_map_fixture(),
            owner: PHOENIX_ETERNAL_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        }
    }

    #[test]
    fn builds_one_collateral_override() {
        let trader = Pubkey::new_unique();
        let funded = trader_account_for(trader, 500);
        let registry = TemplateRegistry::new();

        for target in ["500", "-9007199254740993"] {
            let preparation = build_phoenix_collateral_scenario(trader, &funded, target).unwrap();
            assert_eq!(preparation.overrides.len(), 1);
            let collateral_override = &preparation.overrides[0];
            assert!(
                registry.contains(&collateral_override.template_id),
                "the override must name a bundled template"
            );
            assert_eq!(
                collateral_override.account,
                AccountAddress::Pubkey(trader.to_string())
            );
            assert_eq!(
                collateral_override.values[COLLATERAL_FIELD],
                serde_json::json!(target)
            );
            assert_eq!(collateral_override.scenario_relative_slot, PREPARATION_SLOT);
            assert!(collateral_override.fetch_before_use);
        }
    }

    #[test]
    fn direct_mark_patches_only_the_selected_mark_ticks_and_slot() {
        let account = perp_asset_map_account();
        let values = HashMap::from([
            (MARKET_SYMBOL_FIELD.to_string(), serde_json::json!("SOL")),
            (DIRECT_MARK_TICKS_FIELD.to_string(), serde_json::json!("1")),
        ]);

        let patched =
            forge_phoenix_override(&Pubkey::new_unique(), &account, &values, 123).unwrap();
        let after = PerpAssetMap::try_from_account_bytes(&patched)
            .unwrap()
            .find_by_symbol("SOL")
            .unwrap()
            .unwrap();
        let mark = after.metadata.oracle_price().mark_price;
        assert_eq!((mark.price.ticks.as_inner(), mark.price.slot), (1, 123));
        for sample in mark
            .spot_price_component
            .last_exchange_spot_price
            .iter()
            .chain(mark.perp_price_component.last_exchange_perp_price.iter())
        {
            assert_eq!((sample.ticks.as_inner(), sample.slot), (1, 123));
        }
        assert_eq!(mark.spot_price_component.slot, 123);
        assert_eq!(patched.len(), account.data.len());

        for rejected in ["0", "4294967296"] {
            let values = HashMap::from([
                (MARKET_SYMBOL_FIELD.to_string(), serde_json::json!("SOL")),
                (
                    DIRECT_MARK_TICKS_FIELD.to_string(),
                    serde_json::json!(rejected),
                ),
            ]);
            assert!(
                forge_phoenix_override(&Pubkey::new_unique(), &account, &values, 123).is_err(),
                "{rejected}"
            );
        }
    }

    #[tokio::test]
    async fn direct_mark_is_stamped_with_the_clock_slot() {
        let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        svm.inner
            .set_account(PHOENIX_PERP_ASSET_MAP, perp_asset_map_account())
            .unwrap();
        let mut clock = svm.inner.get_sysvar::<Clock>();
        clock.slot = 1_000;
        svm.inner.set_sysvar(&clock);
        let template = TemplateRegistry::new()
            .get("phoenix-direct-mark-risk-shock")
            .expect("template")
            .clone();
        let mut scenario = Scenario::new("mark".to_string(), "mark".to_string());
        scenario.add_override(
            OverrideInstance::new(template.id, 0, template.address).with_values(HashMap::from([
                (MARKET_SYMBOL_FIELD.to_string(), serde_json::json!("SOL")),
                (DIRECT_MARK_TICKS_FIELD.to_string(), serde_json::json!("1")),
            ])),
        );
        // Registered for an earlier slot, which surfnet_registerScenario allows, and played at once.
        svm.register_scenario(scenario, Some(900)).unwrap();

        svm.materialize_overrides_for_slot(&None, 900)
            .await
            .unwrap();

        let account = svm
            .inner
            .get_account(&PHOENIX_PERP_ASSET_MAP)
            .unwrap()
            .unwrap();
        let price = PerpAssetMap::try_from_account_bytes(&account.data)
            .unwrap()
            .find_by_symbol("SOL")
            .unwrap()
            .unwrap()
            .metadata
            .oracle_price()
            .mark_price
            .price;
        assert_eq!(
            (price.ticks.as_inner(), price.slot),
            (1, 1_000),
            "the mark must be fresh at the slot the program reads it, not the scheduled one"
        );
    }

    #[test]
    fn maintenance_factor_patches_only_the_selected_market_factor() {
        let account = perp_asset_map_account();
        let factors = |data: &[u8]| {
            PerpAssetMap::try_from_account_bytes(data)
                .unwrap()
                .find_by_symbol("SOL")
                .unwrap()
                .unwrap()
                .metadata
                .risk_params()
                .risk_factors
        };
        let before = factors(&account.data);
        let values = HashMap::from([
            (MARKET_SYMBOL_FIELD.to_string(), serde_json::json!("SOL")),
            (
                MAINTENANCE_FACTOR_FIELD.to_string(),
                serde_json::json!("10000"),
            ),
        ]);

        let patched =
            forge_phoenix_override(&Pubkey::new_unique(), &account, &values, 123).unwrap();
        assert_eq!(factors(&patched), [10_000, before[1], before[2]]);
        let changed = patched
            .iter()
            .zip(&account.data)
            .filter(|(after, before)| after != before)
            .count();
        assert!(changed <= 2, "only the factor's two bytes may change");

        let maintenance = |factor: &str| {
            let values = HashMap::from([
                (MARKET_SYMBOL_FIELD.to_string(), serde_json::json!("SOL")),
                (
                    MAINTENANCE_FACTOR_FIELD.to_string(),
                    serde_json::json!(factor),
                ),
            ]);
            forge_phoenix_override(&Pubkey::new_unique(), &account, &values, 123)
        };
        // Just above the backstop factor still leaves a Liquidatable tier.
        let lowest = before[1] + 1;
        assert_eq!(
            factors(&maintenance(&lowest.to_string()).unwrap()),
            [lowest, before[1], before[2]]
        );
        // At or below the backstop factor, above 100%, or not basis points.
        for rejected in [&before[1].to_string(), "0", "10001", "65536", "1.5"] {
            assert!(maintenance(rejected).is_err(), "{rejected}");
        }
    }

    // These templates' fields are codec inputs, not IDL paths, so the registry's IDL check
    // cannot cover them; this ties the YAML field names to what the codec accepts.
    #[test]
    fn market_list_reports_what_the_market_writers_wrote() {
        let mut account = perp_asset_map_account();
        for (field, value) in [
            (DIRECT_MARK_TICKS_FIELD, "777"),
            (MAINTENANCE_FACTOR_FIELD, "9000"),
        ] {
            let values = HashMap::from([
                (MARKET_SYMBOL_FIELD.to_string(), serde_json::json!("SOL")),
                (field.to_string(), serde_json::json!(value)),
            ]);
            account.data =
                forge_phoenix_override(&Pubkey::new_unique(), &account, &values, 123).unwrap();
        }

        assert_eq!(
            phoenix_markets(PHOENIX_PERP_ASSET_MAP, &account).unwrap(),
            vec![PhoenixMarket {
                symbol: "SOL".to_string(),
                orderbook: Pubkey::from_str_const("71Si24E4uc3oCaPbPZTozC1ptSNNqygjjebxSmErSsC2"),
                mark_ticks: 777,
                tick_size: 100,
                base_lot_decimals: 2,
                maintenance_risk_factor_bps: 9_000,
                backstop_risk_factor_bps: 2_000,
            }]
        );

        let foreign = Account {
            owner: Pubkey::new_unique(),
            ..perp_asset_map_account()
        };
        assert!(phoenix_markets(PHOENIX_PERP_ASSET_MAP, &foreign).is_err());
    }

    #[test]
    fn the_codec_accepts_every_market_template_field_set() {
        let registry = TemplateRegistry::new();
        let market_templates: Vec<_> = registry
            .by_protocol("Phoenix Eternal")
            .into_iter()
            .filter(|template| template.account_type == "PerpAssetMap")
            .collect();
        assert_eq!(
            market_templates.len(),
            2,
            "direct mark and maintenance margin"
        );

        for template in market_templates {
            assert_eq!(
                template.address,
                AccountAddress::Pubkey(PHOENIX_PERP_ASSET_MAP.to_string()),
                "{}",
                template.id
            );
            let values = template
                .properties
                .iter()
                .map(|property| {
                    let value = match property.path.as_str() {
                        MARKET_SYMBOL_FIELD => "SOL",
                        MAINTENANCE_FACTOR_FIELD => "10000",
                        _ => "1",
                    };
                    (property.path.clone(), serde_json::json!(value))
                })
                .collect();
            forge_phoenix_override(&Pubkey::new_unique(), &perp_asset_map_account(), &values, 1)
                .unwrap_or_else(|error| panic!("{}: {error}", template.id));
        }
    }

    #[test]
    fn market_overrides_ignore_keys_outside_the_codec_inputs() {
        // An editor that starts from the decoded map sends its top-level scalars and arrays too.
        let account = perp_asset_map_account();
        for (field, value) in [
            (DIRECT_MARK_TICKS_FIELD, "1"),
            (MAINTENANCE_FACTOR_FIELD, "10000"),
        ] {
            let clean = HashMap::from([
                (MARKET_SYMBOL_FIELD.to_string(), serde_json::json!("SOL")),
                (field.to_string(), serde_json::json!(value)),
            ]);
            let mut editor = clean.clone();
            editor.insert("numAssets".to_string(), serde_json::json!(1));
            editor.insert(
                "padding0".to_string(),
                serde_json::json!([0, 0, 0, 0, 0, 0]),
            );
            assert_eq!(
                forge_phoenix_override(&Pubkey::new_unique(), &account, &editor, 123).unwrap(),
                forge_phoenix_override(&Pubkey::new_unique(), &account, &clean, 123).unwrap(),
            );
        }
    }

    #[test]
    fn market_overrides_need_a_symbol_and_exactly_one_codec_input() {
        let account = perp_asset_map_account();
        let symbol = (MARKET_SYMBOL_FIELD.to_string(), serde_json::json!("SOL"));
        let ticks = (DIRECT_MARK_TICKS_FIELD.to_string(), serde_json::json!("1"));
        let factor = (
            MAINTENANCE_FACTOR_FIELD.to_string(),
            serde_json::json!("10000"),
        );
        for values in [
            vec![symbol.clone()],
            vec![symbol.clone(), ticks.clone(), factor.clone()],
            vec![ticks.clone()],
            vec![factor.clone()],
            vec![
                (MARKET_SYMBOL_FIELD.to_string(), serde_json::json!(1)),
                ticks.clone(),
            ],
        ] {
            let values = HashMap::from_iter(values);
            assert!(
                forge_phoenix_override(&Pubkey::new_unique(), &account, &values, 123).is_err(),
                "{values:?}"
            );
        }
    }

    #[test]
    fn market_inputs_take_whole_json_numbers_as_well_as_strings() {
        let account = perp_asset_map_account();
        let forge = |field: &str, value: serde_json::Value| {
            let values = HashMap::from([
                (MARKET_SYMBOL_FIELD.to_string(), serde_json::json!("SOL")),
                (field.to_string(), value),
            ]);
            forge_phoenix_override(&Pubkey::new_unique(), &account, &values, 123)
        };
        for (field, text, number) in [
            (DIRECT_MARK_TICKS_FIELD, "1", 1),
            (MAINTENANCE_FACTOR_FIELD, "10000", 10_000),
        ] {
            assert_eq!(
                forge(field, serde_json::json!(number)).unwrap(),
                forge(field, serde_json::json!(text)).unwrap(),
                "{field}"
            );
            for rejected in [serde_json::json!(-1), serde_json::json!(1.5)] {
                assert!(
                    forge(field, rejected.clone()).is_err(),
                    "{field}: {rejected}"
                );
            }
        }
    }

    /// Where the fixture markets' PriceComponents start: after the 48-byte map header, each
    /// 1584-byte entry holds a 16-byte symbol and then the metadata, which starts with them.
    const PRICE_COMPONENT_STARTS: [usize; 2] = [48 + 16, 48 + 1_584 + 16];

    /// The SOL fixture followed by a second market, a copy of SOL with its own symbol, mark and
    /// thresholds, so a walk over the entries has a later market to find.
    fn two_market_map_account() -> Account {
        let mut account = perp_asset_map_account();
        let [first, second] = PRICE_COMPONENT_STARTS.map(|start| start - 16);
        account.data.copy_within(first..second, second);
        let mut symbol = [0_u8; 16];
        symbol[..3].copy_from_slice(b"BTC");
        account.data[second..second + 16].copy_from_slice(&symbol);
        let price_range =
            PRICE_COMPONENT_STARTS[1]..PRICE_COMPONENT_STARTS[1] + size_of::<PriceComponent>();
        let mut price: PriceComponent =
            bytemuck::pod_read_unaligned(&account.data[price_range.clone()]);
        price.mark_price.price.ticks = bytemuck::cast(12_345_u64);
        price.mark_price.spot_price_component.stale_threshold = 25;
        price.mark_price.perp_price_component.stale_threshold = 25;
        account.data[price_range].copy_from_slice(bytemuck::bytes_of(&price));
        // Two assets in two used slots.
        account.data[24..26].copy_from_slice(&2_u16.to_le_bytes());
        account.data[32..36].copy_from_slice(&2_u32.to_le_bytes());
        account
    }

    fn market(data: &[u8], symbol: &str) -> PerpAssetMetadata {
        PerpAssetMap::try_from_account_bytes(data)
            .unwrap()
            .find_by_symbol(symbol)
            .unwrap()
            .unwrap()
            .metadata
    }

    fn oracle_thresholds(data: &[u8], symbol: &str) -> (u64, u64) {
        let mark = market(data, symbol).oracle_price().mark_price;
        (
            mark.spot_price_component.stale_threshold,
            mark.perp_price_component.stale_threshold,
        )
    }

    const RAISED_THRESHOLDS: (u64, u64) =
        (RAISED_STALE_THRESHOLD_SLOTS, RAISED_STALE_THRESHOLD_SLOTS);

    #[test]
    fn raising_the_stale_thresholds_keeps_the_readings_of_every_market() {
        let account = two_market_map_account();
        assert_ne!(
            market(&account.data, "SOL").oracle_price(),
            market(&account.data, "BTC").oracle_price(),
            "the markets differ, so a write to the wrong one shows"
        );

        let raised = raise_stale_thresholds(&PHOENIX_PERP_ASSET_MAP, &account.data)
            .unwrap()
            .expect("upstream's thresholds are raised");

        for symbol in ["SOL", "BTC"] {
            let before = market(&account.data, symbol);
            assert!(
                oracle_thresholds(&account.data, symbol).0 < RAISED_STALE_THRESHOLD_SLOTS,
                "{symbol}: the fixture carries upstream's thresholds"
            );
            let after = market(&raised, symbol);
            let mut expected = before.oracle_price().mark_price;
            expected.spot_price_component.stale_threshold = RAISED_STALE_THRESHOLD_SLOTS;
            expected.perp_price_component.stale_threshold = RAISED_STALE_THRESHOLD_SLOTS;
            assert_eq!(
                after.oracle_price().mark_price,
                expected,
                "{symbol}: prices, reading slots and the book component stay as they were"
            );
            assert_eq!(after.risk_params(), before.risk_params(), "{symbol}");
        }
        let in_price_component = |index: usize| {
            PRICE_COMPONENT_STARTS
                .iter()
                .any(|start| (*start..*start + size_of::<PriceComponent>()).contains(&index))
        };
        assert_eq!(raised.len(), account.data.len());
        assert_eq!(
            raised
                .iter()
                .zip(&account.data)
                .enumerate()
                .position(|(index, (after, before))| after != before && !in_price_component(index)),
            None,
            "nothing outside the markets' PriceComponents is written"
        );
        assert_eq!(
            raise_stale_thresholds(&PHOENIX_PERP_ASSET_MAP, &raised).unwrap(),
            None,
            "a raised map is written once"
        );
    }

    #[tokio::test]
    async fn a_map_override_leaves_every_market_usable() {
        let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        svm.inner
            .set_account(PHOENIX_PERP_ASSET_MAP, two_market_map_account())
            .unwrap();
        let template = TemplateRegistry::new()
            .get("phoenix-maintenance-margin-stress")
            .expect("template")
            .clone();
        let mut scenario = Scenario::new("maintenance".to_string(), "maintenance".to_string());
        scenario.add_override(
            OverrideInstance::new(template.id, 0, template.address).with_values(HashMap::from([
                (MARKET_SYMBOL_FIELD.to_string(), serde_json::json!("SOL")),
                (
                    MAINTENANCE_FACTOR_FIELD.to_string(),
                    serde_json::json!("9000"),
                ),
            ])),
        );
        svm.register_scenario(scenario, Some(100)).unwrap();

        svm.materialize_overrides_for_slot(&None, 100)
            .await
            .unwrap();

        let map = svm.get_account(&PHOENIX_PERP_ASSET_MAP).unwrap().unwrap();
        let metadata = PerpAssetMap::try_from_account_bytes(&map.data)
            .unwrap()
            .find_by_symbol("SOL")
            .unwrap()
            .unwrap()
            .metadata;
        assert_eq!(metadata.risk_params().risk_factors[0], 9_000);
        for symbol in ["SOL", "BTC"] {
            assert_eq!(
                oracle_thresholds(&map.data, symbol),
                RAISED_THRESHOLDS,
                "{symbol}"
            );
        }
    }

    #[tokio::test]
    async fn a_map_override_still_applies_when_the_thresholds_cannot_be_raised() {
        // A second entry with a non-ASCII symbol: finding SOL stops before it, raising every
        // market's threshold does not.
        let mut map = perp_asset_map_account();
        map.data[32..36].copy_from_slice(&2_u32.to_le_bytes());
        map.data[1_632] = 0xFF;
        let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        svm.inner.set_account(PHOENIX_PERP_ASSET_MAP, map).unwrap();
        let template = TemplateRegistry::new()
            .get("phoenix-maintenance-margin-stress")
            .expect("template")
            .clone();
        let mut scenario = Scenario::new("maintenance".to_string(), "maintenance".to_string());
        scenario.add_override(
            OverrideInstance::new(template.id, 0, template.address).with_values(HashMap::from([
                (MARKET_SYMBOL_FIELD.to_string(), serde_json::json!("SOL")),
                (
                    MAINTENANCE_FACTOR_FIELD.to_string(),
                    serde_json::json!("9000"),
                ),
            ])),
        );
        svm.register_scenario(scenario, Some(100)).unwrap();

        svm.materialize_overrides_for_slot(&None, 100)
            .await
            .unwrap();

        let map = svm.get_account(&PHOENIX_PERP_ASSET_MAP).unwrap().unwrap();
        let metadata = PerpAssetMap::try_from_account_bytes(&map.data)
            .unwrap()
            .find_by_symbol("SOL")
            .unwrap()
            .unwrap()
            .metadata;
        assert_eq!(metadata.risk_params().risk_factors[0], 9_000);
    }

    #[tokio::test]
    async fn any_phoenix_override_leaves_the_local_map_usable() {
        let trader = Pubkey::new_unique();
        let funded = trader_account_for(trader, 500);
        let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        svm.set_account(&PHOENIX_PERP_ASSET_MAP, perp_asset_map_account())
            .unwrap();
        svm.set_account(&trader, funded.clone()).unwrap();
        let scenario = build_phoenix_collateral_scenario(trader, &funded, "100").unwrap();
        svm.register_scenario(scenario, Some(100)).unwrap();

        svm.materialize_overrides_for_slot(&None, 100)
            .await
            .unwrap();

        let stressed = svm.get_account(&trader).unwrap().unwrap();
        assert_eq!(stressed.data[COLLATERAL_BYTE_RANGE], 100_i64.to_le_bytes());
        let map = svm.get_account(&PHOENIX_PERP_ASSET_MAP).unwrap().unwrap();
        assert_eq!(oracle_thresholds(&map.data, "SOL"), RAISED_THRESHOLDS);
    }

    #[tokio::test]
    async fn override_on_any_other_phoenix_account_leaves_the_local_map_usable() {
        const EXPIRES_AT_BYTE_RANGE: core::ops::Range<usize> = 88..96;
        let permission = Pubkey::new_unique();
        let mut data = vec![0_u8; 168];
        data[..8].copy_from_slice(&PhoenixAccount::PermissionAccount.discriminant());
        let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        svm.set_account(&PHOENIX_PERP_ASSET_MAP, perp_asset_map_account())
            .unwrap();
        svm.set_account(
            &permission,
            Account {
                data,
                ..trader_account()
            },
        )
        .unwrap();
        let mut scenario = Scenario::new("permission".to_string(), "permission".to_string());
        scenario.add_override(
            OverrideInstance::new(
                "phoenix-permission-limits".to_string(),
                0,
                AccountAddress::Pubkey(permission.to_string()),
            )
            .with_values(HashMap::from([(
                "expiresAtTimestamp".to_string(),
                serde_json::json!(1),
            )])),
        );
        svm.register_scenario(scenario, Some(100)).unwrap();

        svm.materialize_overrides_for_slot(&None, 100)
            .await
            .unwrap();

        let expired = svm.get_account(&permission).unwrap().unwrap();
        assert_eq!(expired.data[EXPIRES_AT_BYTE_RANGE], 1_i64.to_le_bytes());
        let map = svm.get_account(&PHOENIX_PERP_ASSET_MAP).unwrap().unwrap();
        assert_eq!(oracle_thresholds(&map.data, "SOL"), RAISED_THRESHOLDS);
    }

    #[tokio::test]
    async fn an_override_still_applies_when_the_map_cannot_be_kept() {
        let trader = Pubkey::new_unique();
        let funded = trader_account_for(trader, 500);
        // No local map and no upstream datasource to fetch one from.
        let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        svm.set_account(&trader, funded.clone()).unwrap();
        let scenario = build_phoenix_collateral_scenario(trader, &funded, "100").unwrap();
        svm.register_scenario(scenario, Some(100)).unwrap();

        svm.materialize_overrides_for_slot(&None, 100)
            .await
            .unwrap();

        let stressed = svm.get_account(&trader).unwrap().unwrap();
        assert_eq!(stressed.data[COLLATERAL_BYTE_RANGE], 100_i64.to_le_bytes());
        assert_eq!(svm.get_account(&PHOENIX_PERP_ASSET_MAP).unwrap(), None);
    }
}
