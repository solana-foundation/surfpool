# Phoenix Eternal

Surfpool bundles the Phoenix Eternal IDL and 19 override templates, so a scenario can put
Phoenix perpetuals into the state your code needs to see: a market moved to a new price, traders
set a planned distance from liquidation, a paused market, throttled withdrawals. The templates only
prepare state. Liquidations and trades are sent by your own code.

Every field's meaning and units are on the template itself, visible in Studio and through
`get_override_templates`. This page covers what the templates cannot say on their own. For how
Phoenix itself works, see the [Phoenix docs](https://docs.phoenix.trade/).

## How the templates change state

Phoenix keeps one market's state across several accounts that must agree: the oracle readings and
mark in the PerpAssetMap, the makers' liquidity in the spline collection, resting orders in the
orderbook, and each trader's collateral and positions in its Trader account, the
GlobalTraderIndex and the ActiveTraderBuffer. Writing one of them alone leaves the others behind;
a mark moved without the liquidity, for example, lets a liquidation fill at the old price.

The instruction templates run Phoenix's own instructions on a copy of the local VM. They name the
same signer accounts as on mainnet: the oracle keys, the makers, the trader, or the GlobalConfig
role that owns a setting. Their private keys are not used: signature checks are disabled in the
copy, and only a temporary local fee payer signs. Phoenix still checks the instructions' account
and state constraints: a maintenance factor above the cancel order factor, a withdrawal beyond
the free margin, or an order the margin cannot carry is refused. A refused template is skipped
with a warning naming the program's reason.

After the instructions succeed, Play writes back the accounts they changed, all or none: when a
write fails, the accounts written before it are put back. That holds for one template. The
templates of a scenario run one after another in their slot and are not one unit: a refused
template is skipped and the ones before it stay applied.

The collateral, stop-loss and permission templates patch one field of one account. The maintenance
described under "Keeping markets usable" also patches the PerpAssetMap and the spline collections
directly.

## Terms

| Term                 | Meaning                                                                                                                                                                                                                       |
| -------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Trader               | A trader's account, with its USDC collateral and capability flags in `traderState`.                                                                                                                                           |
| GlobalTraderIndex    | Phoenix keeps the state of active traders here instead of in their Trader account, with their positions in the ActiveTraderBuffer. Phoenix reads those, so the templates check traders through Hawkeye, not the account copy. |
| Hot / cold trader    | A trader the GlobalTraderIndex lists, such as one with resting orders, is hot in Phoenix's SDK; any other trader is cold, with its whole state in its Trader account.                                                         |
| PerpAssetMap         | The single account that holds every market's oracle readings, mark price and risk parameters.                                                                                                                                 |
| Spline               | A market maker's liquidity curve around its mid price, in the market's spline collection. Liquidations mostly fill against splines.                                                                                           |
| Resting order        | A limit order waiting in a market's book. Resting orders hold margin and keep their trader hot, and a keeper cannot liquidate a trader while any of them add risk.                                                            |
| Crossed book         | Resting orders left on the wrong side of the makers' quotes after a move, such as bids above the new ask. Phoenix's uncross crank matches them, and anyone can send it.                                                       |
| Hawkeye              | A Phoenix program that returns margin and risk views, such as `view_margin`.                                                                                                                                                  |
| Effective collateral | What Phoenix measures margin against: USDC collateral, plus the discounted value of SOL collateral and unrealized gains, minus unrealized losses, plus unsettled funding.                                                     |
| Ticks                | Phoenix's price unit. USD per base unit = `markTicks * tickSize * 10^(baseLotDecimals - 6)`, all three from `list_phoenix_markets`.                                                                                           |
| Quote lots           | Units of Phoenix USDC (mint `PhUsd11YkbjSaWjFncfAAmatntsjx3MgDR9B6g1ks3A`), which has 6 decimals. `1000000` is 1 USDC.                                                                                                        |

## Number formats

| Value                                          | Unit                               | Example                  |
| ---------------------------------------------- | ---------------------------------- | ------------------------ |
| Prices                                         | Ticks                              |                          |
| Collateral, withdraw budgets and withdraw fees | Quote lots                         | `1000000` = 1 USDC       |
| Position and liquidation sizes                 | Base lots                          |                          |
| Risk factors                                   | Basis points of the initial margin | `5000` = 50%             |
| Trading fees                                   | Millionths of the filled notional  | `350` = 0.035% (3.5 bps) |

The instruction templates take numbers as decimal strings, so large values stay exact in
JavaScript; the stop-loss and permission templates take JSON numbers. A configuration field left
out or left empty keeps its current value, unless its template says the field is required.

## fetchBeforeUse

`fetchBeforeUse: true` makes Play fetch the override's own account from the upstream datasource
first, replacing the local copy. Without it, an override whose account is not in the local VM yet
is skipped.

Leave it `false` on the templates that change the PerpAssetMap or a Trader. Phoenix keeps one
market across several accounts, and a refetch replaces only the override's own account:
refetching the PerpAssetMap puts every market's prices and factors back to mainnet's, while the
order books and traders keep what earlier scenarios prepared, and the two no longer agree.
`list_phoenix_markets` and `list_phoenix_trader_positions` load those accounts into the surfnet.
The GlobalConfig, WithdrawQueue, StopLosses and permission accounts stand alone, so use `true` the
first time a scenario changes one of them.

Surfnet copies each account from mainnet the first time it is read, while mainnet keeps trading.
Order books and the ActiveTraderBuffer name traders by their node in the GlobalTraderIndex, so the
three must come from the same moment. The first Phoenix template or tool that needs the index on a
surfnet loads it with the buffer and every market's book in one request. Splines, Trader accounts
and the PerpAssetMap are still copied when first needed, which can leave a collateral or a price
slightly behind mainnet. A Trader account copied at another moment than the index can also
disagree with it on whether the trader is hot: `phoenix-trader-collateral` marks a listed trader's
cold account hot again, so every Phoenix program reads the trader from the index, and a hot trader
the index does not list is refused.

**Known limitation:** that does not help when your own code read Phoenix accounts on a fresh
surfnet before any Phoenix template or tool ran, or once Phoenix lists more than 98 markets and the
request splits. It shows up as the uncross-crank message, the refusal of a hot trader the
GlobalTraderIndex does not list, or a skipped override failing with `InvalidAccountData` in Hawkeye
before any program log. Reset the Phoenix accounts (`surfnet_resetAccount` on the Phoenix program
with `includeOwnedAccounts: true`) or restart the surfnet, and play again.

## Templates

| Template                              | What it does                                                                                           | Signer accounts              |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------ | ---------------------------- |
| `phoenix-market-move`                 | Moves a market's oracle readings and its makers' splines to a price; crossed orders stay crossed.      | oracle keys, makers          |
| `phoenix-market-uncross`              | Matches the resting orders a move left crossed with Phoenix's uncross crank, which anyone can send.    | none, a permissionless crank |
| `phoenix-oracle-price`                | Moves only a market's oracle readings, in ticks or by a percentage; the book and makers stay.          | oracle keys                  |
| `phoenix-trader-cancel-orders`        | Cancels a trader's resting orders on every market, taking it out of the index, or on chosen ones.      | trader                       |
| `phoenix-trader-collateral`           | Brings a trader's effective collateral to a USDC amount, its distance from liquidation.                | field write                  |
| `phoenix-market-risk-factors`         | Maintenance, backstop and high-risk factors.                                                           | risk authority               |
| `phoenix-market-cancel-risk-factor`   | The resting-order factor, the ceiling for the maintenance factor.                                      | risk authority               |
| `phoenix-market-max-liquidation-size` | The most base lots one liquidation may close.                                                          | risk authority               |
| `phoenix-market-open-interest-cap`    | The most open interest the market accepts.                                                             | risk authority               |
| `phoenix-market-funding`              | Funding interval, period and maximum rate.                                                             | market authority             |
| `phoenix-market-fees`                 | Default taker and maker fees.                                                                          | market authority             |
| `phoenix-market-status`               | Active, PostOnly, Paused or Closed.                                                                    | market authority             |
| `phoenix-exchange-status`             | The exchange-wide active, gated and maintenance flags.                                                 | root authority               |
| `phoenix-withdraw-limits`             | The withdrawal budget and its refill per slot.                                                         | root authority               |
| `phoenix-withdraw-parameters`         | Deposit cooldown and withdrawal and queueing fees.                                                     | risk authority               |
| `phoenix-trader-fees`                 | A trader's fee override multipliers.                                                                   | market authority             |
| `phoenix-trader-capabilities`         | Allows or blocks a trader's orders, risk-increasing trades, deposits and withdrawals.                  | risk authority               |
| `phoenix-stop-loss-trigger`           | Edits the two legs of a StopLosses account placed with `PlaceStopLoss`.                                | IDL write                    |
| `phoenix-permission-limits`           | A delegated permission's expiry and remaining actions.                                                 | IDL write                    |

These templates cover Phoenix's perpetual markets. Templates for spot trading are planned.

## Recipes

`phoenix-market-move` and `phoenix-market-uncross` go together, in the same slot and in this order,
and Studio's Phoenix state presets build them that way. Without `phoenix-market-uncross`, resting
orders on the other side of a moved market's new price stay crossed, and trades and liquidations
fill against them at their old prices.

`phoenix-trader-collateral` takes the effective collateral wanted, in USDC, and lands on it when it
plays: it reads Hawkeye's effective collateral then and moves the collateral field by the
difference, where Phoenix reads it. That is the Trader account or, for a hot trader, which
`list_phoenix_trader_positions` reports as `hot: true`, its record in the GlobalTraderIndex, which
Phoenix copies back to the account when the trader leaves the index. Effective collateral moves one
for one with that field, so pending funding, resting orders and changes since the scenario was
created do not change where it lands. The value can be negative.

`liquidate_via_market_order` refuses a trader that still has risk-increasing resting orders on any
market, and one whose effective collateral is already below zero. A keeper force-cancels those
orders first with `ForceCancelRiskIncreasing`, which Phoenix allows any signer while the trader is
liquidatable. A scenario can cancel them instead with `phoenix-trader-cancel-orders`, given the
trader's account in its `trader` value and no `symbol`, before `phoenix-trader-collateral` in the
same slot. The overrides of a slot play one after another, each on its own, so a refused cancel
leaves the collateral to play alone. `symbol` picks the markets: empty for every market, which
takes a hot trader out of the index, one symbol such as `SOL`, or several separated by commas such
as `SOL, BTC, ETH`. An unknown symbol refuses the whole override. Positions stay open, and a cold
trader has no orders to cancel.

Pick the target from the tool's `margin`, the trader with its orders canceled, by what the keeper
should do:

- **One liquidation:** between `backstopMargin` and `maintenanceMargin`. The keeper's first
  liquidation releases enough margin that the account is healthy again.
- **Every position:** between `highRiskMargin` and `backstopMargin`, with the keeper liquidating the
  smallest maintenance margin first. A trader with many positions can recover before its last
  one. Below `highRiskMargin` a liquidation can leave the trader underwater and fail, and Play
  logs a warning.
- **Watch it cross:** pass `moves` to the tool and pick the target between the backstop and
  maintenance margin of `marginAfterMoves`. Play `phoenix-trader-cancel-orders`, so the move cannot
  fill the trader's orders, and `phoenix-trader-collateral` with
  `margin.effectiveCollateral - marginAfterMoves.effectiveCollateral + target`, which must stay
  above `margin.maintenanceMargin` so the trader is healthy until the move. Then play those
  `phoenix-market-move` overrides, each followed by its `phoenix-market-uncross`, at a later slot,
  since a cancel freezes the mark for risk in its own slot.

A target is worked out from the margins when the scenario is created, and they move with the mark:
after another scenario moves the trader's markets, create it again.

The tool reports quote lots; divide them by `1000000` for USDC. It fails with the reason when it
cannot read the trader, and reports in `refusal` why either template would refuse it, such as a
spline market maker. Nothing stops a collateral above the current one, which the global vault does
not back; a "healthy now, liquidatable after a move" target can need one.

Keep `fetchBeforeUse` false on the template. A refused override is skipped and only logs
`Skipping override ...`, for example when Phoenix keeps a trader in its index after the cancel;
check the trader with the tool again after Play.

## Keeping markets usable

Oracle updates keep each market's readings fresh on mainnet, and Phoenix refuses a market whose
readings are older than its stale threshold. Nothing updates them in the local VM, so every
Phoenix override also raises every market's stale thresholds in the local PerpAssetMap. Prices
stay as they were.

This maintenance runs before template validation, including for the two generic IDL templates,
and writes the map only when its data changes. A refused template can therefore leave these
maintenance changes applied.

Every Phoenix override also moves readings older than the local clock up to its slot. An oracle
report folds the gap between book and oracle into the price, weighted by the slots since the last
reading, so on a map fetched long before the clock a deep move would leave no positive price.

A running surfnet's clock trails mainnet, so a PerpAssetMap fetched from the upstream datasource
records funding updates from after the local time. Phoenix refuses to update funding, and with it
every price, before the last update, so every Phoenix override also moves those funding
timestamps back to the local clock. Funding then accrues from the local time.

Makers on mainnet re-quote every few slots, and a spline region stops quoting a set number of slots
after its maker's last update (6 to 60 on SOL). Nobody re-quotes on a surfnet, so every Phoenix
override also loads every market's spline collection and sets each region's lifespan to Phoenix's
good-till-canceled value, writing only collections that change. The books keep their liquidity
while the surfnet's clock runs, so a keeper finds the same quotes long after Play.

## Finding accounts

| Account           | How to find it                                                                                                                                                    |
| ----------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Trader            | PDA `["trader", authority, [pda index, subaccount index]]`. The pda index is 0; subaccount 0 is the cross-margin account and 1 and up are isolated accounts.      |
| Market orderbook  | The `orderbook` of the market in `list_phoenix_markets`.                                                                                                          |
| StopLosses        | PDA `["stoploss", trader account, asset id as u64 little-endian]`, or `getProgramAccounts` with `dataSize: 328` and a memcmp of the trader account at offset 224. |
| PermissionAccount | PDA `["permission", authority, delegated key]`, or `getProgramAccounts` with `dataSize: 168` and a memcmp of the authority at offset 8.                           |
| PerpAssetMap      | `2nHGAaEw3D5dd4hVueaUNoygkQFmoeKqRQWnSPqSMFUC`: every market's prices and risk factors, which the market templates change.                                        |
| WithdrawQueue     | `3c3NTwpg7yW91FxijkHBXwVH1xUifun3Z8TC5eW5Si3K`: the exchange's withdrawal queue and limits, which the withdraw templates change.                                  |
| GlobalConfig      | `2zskx2iyCvb6Stg7RBZkt1f6MrF4dpYtMG3yMvKwqtUZ`: the exchange-wide status flags, which `phoenix-exchange-status` changes.                                          |

## MCP

- `list_phoenix_markets` returns every market with its symbol, orderbook, `markTicks`,
  `tickSize`, `baseLotDecimals` and risk factors, read from the local VM.
- `list_phoenix_trader_positions` returns the markets where a trader holds a position, with its
  side, size in base lots and maintenance margin, whether it is `hot`, and its `margin` as
  `phoenix-trader-cancel-orders` leaves it: a hot trader's orders canceled. Given `moves`, it also
  returns `marginAfterMoves`, with each market moved and uncrossed.
  Hawkeye's views run on a copy of the surfnet's state, with its markets kept usable the way Play
  keeps them, and write nothing back. The reads go through the surfnet, which loads any account
  it is missing.
- `create_scenario` accepts any template and refuses a market symbol `list_phoenix_markets` does
  not list.

## Troubleshooting

| Warning or error                                              | Meaning                                                                                                                           |
| ------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| `Account ... not found in SVM for override ...`               | The override's own account is not in the local VM. Load it with the listing tools, or set `fetchBeforeUse: true` on a standalone one. |
| `... accounts are missing locally and there is no datasource` | The template needs Phoenix accounts that are not in the local VM, and surfnet has no upstream datasource to fetch them from.      |
| `... is offline and missing locally`                          | The account is marked offline, so surfnet will not fetch it.                                                                      |
| `instruction N failed: ... logs: [...]`                       | Phoenix refused the template's instruction; the logs carry its reason, such as a factor out of range.                             |
| `... is in Phoenix's active trader index and ...`             | `phoenix-trader-cancel-orders` canceled the trader's orders and Phoenix still lists it, or it has none to cancel.                 |
| `... quotes splines on ... markets; ...`                      | The trader is a spline market maker, which `phoenix-trader-collateral` does not support.                                         |
| `... is below its high-risk margin ...`                       | The collateral target is below the trader's high-risk margin; a liquidation can leave it underwater and fail.                    |
| `the ... book stayed crossed after ... uncross cranks`        | `phoenix-market-uncross` left the book crossed after its cranks. Try a smaller move.                                              |
| `InvalidAccountData` before any program log                   | The order book and the trader index were copied from mainnet at different moments; see the known limitation.                      |
| `Cannot get mark price, staleness or validity check failed`   | No Phoenix scenario was played on this surfnet, so the oracle readings aged past the stale thresholds. Play any Phoenix scenario. |

## Tests against mainnet

```sh
cargo test -p surfpool-core --features integration-tests tests::phoenix
```

Set `SURFPOOL_TEST_RPC_URL` to use a private endpoint if the public one rate-limits. CI does not
build this feature, so run them before a push (about half a minute).

- Each run copies mainnet's exchange accounts and every book once.
- Each test runs Phoenix's mainnet programs on a fresh local copy and plays templates as Play does.
- Liquidations are real keeper transactions.
- Each test opens its own trader locally, with the positions and orders it needs.
