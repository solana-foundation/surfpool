# Phoenix Eternal state preparation

This integration prepares deterministic Phoenix Eternal account state. Bots remain responsible
for submitting trades, arbitrage, and liquidation transactions.

## Templates

| Template                            | Account it writes                                                          | Fields                                            | Studio                       |
| ----------------------------------- | -------------------------------------------------------------------------- | ------------------------------------------------- | ---------------------------- |
| `phoenix-trader-collateral-stress`  | A Trader, and its GlobalTraderIndex record while the fork's index lists it | `traderState.quoteLotCollateral`                  | Phoenix state dialog, editor |
| `phoenix-direct-mark-risk-shock`    | The PerpAssetMap (fixed address)                                           | `symbol`, `target_ticks`                          | Phoenix state dialog, editor |
| `phoenix-maintenance-margin-stress` | The PerpAssetMap (fixed address)                                           | `symbol`, `maintenance_risk_factor_bps`           | Phoenix state dialog, editor |
| `phoenix-market-fees`               | One market's orderbook                                                     | `defaultTakerFeeMicro`, `defaultMakerFeeMicro`    | Editor                       |
| `phoenix-withdraw-limits`           | The WithdrawQueue (fixed address)                                          | Budget, refill and fee fields                     | Editor                       |
| `phoenix-trader-capabilities`       | One cold Trader                                                            | `traderState.flags`                               | Editor                       |
| `phoenix-stop-loss-trigger`         | One StopLosses account                                                     | Trigger and execution prices of both slots        | Editor                       |
| `phoenix-permission-limits`         | One PermissionAccount                                                      | `expiresAtTimestamp`, `numSignerActionsRemaining` | Editor                       |

A liquidation cascade is two overrides in one scenario: collateral stress at slot 0, then a mark
shock at slot 1. See [Recipe: liquidation cascade](#recipe-liquidation-cascade).

Set `fetchBeforeUse: true` on every override, so the live account is forked before its bytes are
changed. Use `false` only for a later override that builds on state an earlier override of the
same scenario prepared.

## Number formats

| Value                                          | Unit                                                                                                                             | Example                           |
| ---------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- | --------------------------------- |
| Mark and stop-loss prices                      | Phoenix ticks. USD per base unit = `markTicks * tickSize * 10^(baseLotDecimals - 6)`, with all three from `list_phoenix_markets` | BTC was 86883 ticks on 2026-10-02 |
| Collateral, withdraw budgets and withdraw fees | Quote lots of PhUSD, which has 6 decimals                                                                                        | `1000000` = 1 PhUSD               |
| Risk factors                                   | Basis points of the initial margin                                                                                               | `5000` = 50%                      |
| Trading fees                                   | Millionths of the filled notional                                                                                                | `350` = 0.035%                    |
| Capability bits                                | Bit flags                                                                                                                        | `62` = active cold trader         |
| Permission expiry                              | Unix seconds                                                                                                                     | `0` = never expires               |

The collateral, mark and maintenance templates take their numbers as decimal strings, so values
outside JavaScript's safe integer range stay exact. The other five templates edit their account
through the IDL like the Kamino templates, so their values are JSON numbers.

## Hot and cold traders

Phoenix keeps an active ("hot") trader's TraderState in a 16-byte record in the
GlobalTraderIndex, and its positions in the ActiveTraderBuffer. A cold trader's state lives in its
own Trader account. On mainnet the HOT bit (value 1) of the Trader's `traderState.flags` and the
index agree, and traders join and leave the hot set all the time: 17 of 208 hot traders left it
within a day when we measured on 2026-10-02.

A fork loads the GlobalTraderIndex once, so it can disagree with a freshly fetched Trader. Hawkeye
and the program read a trader from its index record whenever the fork's index lists it,
whatever the HOT bit says. The templates follow the program:

- A trader the fork's index lists gets its collateral written to its record and to its account.
  Other TraderState fields are refused for it, since only collateral is mirrored into the record.
- A trader flagged hot that the fork's index does not list turned hot after the fork loaded the
  index. Phoenix rejects every transaction for it on that fork with
  `TradersViewError::TraderNotFound`, so the collateral override is refused. Restart the fork to
  load the current index.

## Template guides

### Collateral stress

| Field                            | Meaning                                                                                                       |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| `traderState.quoteLotCollateral` | Exact signed collateral in quote lots, as a decimal string, at least `-4611686018427387904` (`i64::MIN / 2`). |

- The account is the Trader PDA `["trader", authority, [pda index, subaccount index]]`, usually
  `[0, 0]` for the main account.
- Collateral can only be lowered; raising it needs a real deposit into the global vault. With
  fetchBeforeUse the ceiling is the live amount on mainnet, otherwise the amount the fork reads.
  The exception is a trader that left the hot set after the fork loaded the index: the fork's
  index still lists it, so its record there is the ceiling either way. A target above the
  ceiling is skipped at Play with a warning.
- The target is the trader's quote-lot collateral, not just its deposits. Phoenix's effective
  collateral adds the discounted unrealized PnL, unsettled funding and discounted spot
  collateral, so lowering collateral by N quote lots lowers effective collateral by N. To reach a margin tier, take `collateral` and
  `effective_collateral` from Hawkeye's `view_margin` and set
  `target = collateral - effective_collateral + wanted_effective`.

Phoenix ranks a trader by its effective collateral against the margins `view_margin` returns:

| Effective collateral is below    | Tier                 |
| -------------------------------- | -------------------- |
| The high-risk margin, or below 0 | HighRisk             |
| The backstop requirement         | BackstopLiquidatable |
| The maintenance margin           | Liquidatable         |
| The cancel margin                | Cancellable          |
| The at-risk margin               | AtRisk               |
| None of them                     | Safe                 |

### Direct mark shock

| Field          | Meaning                                                         |
| -------------- | --------------------------------------------------------------- |
| `symbol`       | Exact market symbol from `list_phoenix_markets`, such as `BTC`. |
| `target_ticks` | Exact mark in ticks, as a decimal string, from 1 to 4294967295. |

- It sets the market's mark ticks and stamps the mark with the current slot. It also sets the
  five spot and five perp oracle samples and the spot component slot to the same ticks and slot.
- The orderbook and spline do not move, so trades still fill at the book's real prices, and a
  mark Phoenix rebuilds after a trade can move back toward the book. `list_phoenix_markets`
  shows the mark the fork holds.
- The target is exact ticks, not a percentage. Compute a relative move from `markTicks` just
  before Play: a target created as +10% from 86651 ticks was +9.76% once BTC had moved to 86840.

### Maintenance margin stress

| Field                         | Meaning                                                                                                                                        |
| ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| `symbol`                      | Exact market symbol from `list_phoenix_markets`.                                                                                               |
| `maintenance_risk_factor_bps` | Maintenance factor in bps of the initial margin, as a decimal string. It must be above the market's `backstopRiskFactorBps` and at most 10000. |

- The maintenance margin scales linearly with the factor. All 92 markets used 5000 with a
  backstop of 2000, so 10000 doubles the requirement, the most it can rise.
- A factor at or below the backstop factor is refused: it would leave no Liquidatable tier.

### Market fees

| Field                  | Meaning                                                                     |
| ---------------------- | --------------------------------------------------------------------------- |
| `defaultTakerFeeMicro` | Taker fee in millionths of the filled notional.                             |
| `defaultMakerFeeMicro` | Maker fee in millionths; negative is a rebate. Keep it below the taker fee. |

- The account is the market's orderbook: resolve the market with `list_phoenix_markets` and use
  its `orderbook`. The markets we checked charged 350 (taker) and 50 (maker) on 2026-10-02.
- Per-trader fee overrides, prices and orders do not change.

### Withdraw limits

| Field                                           | Meaning                                                                                                         |
| ----------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `withdrawThrottle.maxBudget.inner`              | Most quote lots withdrawable before requests queue. Live: `2000000000000` (2,000,000 PhUSD).                    |
| `withdrawThrottle.remainingBudget.inner`        | Quote lots still withdrawable now; keep it at or below the maximum. `0` sends every withdrawal to the queue.    |
| `withdrawThrottle.replenishAmountPerSlot.inner` | Quote lots added back each slot. Live: `450000000` (450 PhUSD). Set `0` to keep the budget empty while testing. |
| `withdrawalFee.inner`                           | Quote lots charged per withdrawal. Live: `500000` (0.5 PhUSD).                                                  |
| `enqueueingFee.inner`                           | Quote lots charged when a withdrawal is queued. Live: `500000` (0.5 PhUSD).                                     |

- The account is the singleton WithdrawQueue `3c3NTwpg7yW91FxijkHBXwVH1xUifun3Z8TC5eW5Si3K`, which
  the template carries.
- A withdrawal above the remaining budget is queued and paid later by the permissionless
  `ConsumeWithdrawQueue` crank. Collateral and already queued requests do not change.

### Trader capabilities

| Field               | Meaning                                                                                          |
| ------------------- | ------------------------------------------------------------------------------------------------ |
| `traderState.flags` | Capability bits: 1 hot, 2 place limit, 4 place market, 8 increase risk, 16 deposit, 32 withdraw. |

- Mainnet traders use `63` (hot), `62` (active cold) and `6` (freshly registered, frozen).
  `54` is reduce-only and `6` freezes a trader.
- Never change the hot bit (1): the index would not list the trader, and Phoenix would reject
  its transactions. Limit orders need a hot trader, so a cold trader places market orders only.
- Only cold traders are supported. A trader the fork's index lists keeps these bits in its
  index record, so Play skips the override with a warning.

### Stop-loss trigger

| Field                               | Meaning                                                          |
| ----------------------------------- | ---------------------------------------------------------------- |
| `stopLosses.0.triggerPrice.inner`   | Ticks at or above which slot 0 (the greater-than trigger) fires. |
| `stopLosses.0.executionPrice.inner` | Worst ticks slot 0's order may fill at once it fires.            |
| `stopLosses.1.triggerPrice.inner`   | Ticks at or below which slot 1 (the less-than trigger) fires.    |
| `stopLosses.1.executionPrice.inner` | Worst ticks slot 1's order may fill at once it fires.            |

- It arms or disarms a stop loss the trader already placed, for example to test a keeper's
  `ExecuteStopLoss` next to the current mark. It does not place, resize or execute one.
- Each slot has a flags byte (account byte 49 for slot 0, 129 for slot 1): bit 0 active, bit 2
  buy, otherwise sell. Only an active slot fires, so edit only active slots.
- A sell's execution price is at or below its trigger, a buy's at or above it.
- Do not touch `positionSequenceNumber`: it must keep matching the live position.
- The account is the PDA `["stoploss", trader account, asset id as u64 little-endian]`. To find
  one, call `getProgramAccounts` on the Phoenix program with `dataSize: 328` and a memcmp of the
  trader account at offset 224. The asset id is the u32 at offset 256.

### Delegated permission

| Field                       | Meaning                                                                                                 |
| --------------------------- | ------------------------------------------------------------------------------------------------------- |
| `expiresAtTimestamp`        | Unix seconds after which the permission stops working. `0` never expires; any past time expires it now. |
| `numSignerActionsRemaining` | Actions the delegate may still sign; `0` leaves none.                                                   |

- It tests a bot that trades under a delegated permission when the permission runs out. The
  actions the permission grants do not change.
- The account is the PDA `["permission", authority, delegated key]`. To find one, call
  `getProgramAccounts` on the Phoenix program with `dataSize: 168` and a memcmp of the authority
  at offset 8 or of the delegated key at offset 40.

## Recipe: liquidation cascade

1. Pick a trader whose `view_margin` shows a position and `is_liquidatable` 0.
2. At slot 0, use `phoenix-trader-collateral-stress` with
   `target = collateral - effective_collateral + maintenance_margin / 2`. Effective collateral
   then sits at half the maintenance margin, so the trader is Liquidatable.
3. At slot 1, use `phoenix-direct-mark-risk-shock` on the trader's market, moving the mark
   against the position.
4. Prepare the fork for transactions (next section), then send the liquidation.

## Before sending transactions

Views such as Hawkeye work on any fork. Phoenix transactions need two accounts the fork does not
copy by itself. After every surfnet start:

1. Copy the mainnet `SysvarLastRestartS1ot1111111111111111111111` account with
   `surfnet_setAccount`. Surfnet starts this sysvar at 0, and Phoenix only trades while it equals
   the restart slot its GlobalConfig acknowledged (246464040 on mainnet).
2. Create `GdxfTLSsdSY37G6fZoYtdGDSfgFnbT2EmRpuePZxWShS`, the Phoenix log authority, as an empty
   system account with `surfnet_setAccount`. It does not exist on mainnet, and without it
   `sendTransaction` waits 30 s for it.

## Use from Studio

1. Start an online Surfpool fork and open Studio.
2. For collateral, mark or maintenance stress, open **Scenario presets**, choose
   **Phoenix state** and pick the **State goal**: Liquidation-risk collateral, Direct mark-price
   adjustment or Maintenance margin stress.
   - Collateral takes the Trader account and a target in USD or quote lots.
   - The mark takes a market and a target as a percent change, a USD price or ticks.
   - The maintenance factor takes a market and a target as a percent or bps.
   - The market field searches the live list by symbol or orderbook address.
3. For the other templates, open the scenario editor, pick **Phoenix Eternal** and the template,
   and set the account and fields described above.
4. Inspect the generated override, then press **Play** to activate it.
5. Send the bot, trade, arbitrage, or liquidation transaction you want to evaluate to the local
   Surfnet RPC, normally `http://127.0.0.1:8899`.

## Use through MCP

| Tool                                 | Required parameters          |
| ------------------------------------ | ---------------------------- |
| `create_phoenix_collateral_scenario` | `trader`, `targetQuoteLots`  |
| `list_phoenix_markets`               | None; optional `surfnetPort` |
| `create_scenario`                    | Any of the templates above   |

- `list_phoenix_markets` returns every listed market with its `symbol`, `orderbook`,
  `markTicks`, `tickSize`, `baseLotDecimals`, `maintenanceRiskFactorBps` and
  `backstopRiskFactorBps`. It reads the fork's PerpAssetMap, so `markTicks` is the fork's value,
  which a market template refreshes when it plays with fetchBeforeUse.
- `create_phoenix_collateral_scenario` reads the Trader from the surfnet Studio plays scenarios
  on and returns a Studio editor URL.
- `create_scenario` refuses a market symbol `list_phoenix_markets` does not list, and suggests
  the listed spelling when only the case differs, such as `SOL` for `sol`. If the market list
  cannot be read, the scenario is created without this check.

## Troubleshooting

| Error or observation                                                                    | Meaning                                                                                                                                                                             |
| --------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Phoenix PerpAssetMap ... was not found` or `Phoenix dependency ... is missing locally` | Neither the local fork nor its datasource holds the Phoenix account graph. Start Surfpool against a datasource that carries the deployment.                                         |
| `Hot Phoenix Trader has no reachable GlobalTraderIndex entry`                           | The trader turned hot after the fork loaded the index; Phoenix rejects its transactions too. Restart the fork.                                                                      |
| `Cannot get mark price, staleness or validity check failed`                             | The fork's PerpAssetMap aged since it was loaded. Market templates refresh it with `fetchBeforeUse: true`; for collateral-only scenarios add a market override or restart the fork. |
| `Global configuration must be active` when a transaction is sent after Play             | The LastRestartSlot sysvar is still 0. See [Before sending transactions](#before-sending-transactions).                                                                             |
| `sendTransaction` waits 30 s and fails with `Failed to fetch accounts from remote`      | The Phoenix log authority is missing. See [Before sending transactions](#before-sending-transactions).                                                                              |

## Verification against mainnet

```sh
cargo test -p surfpool-core --features integration-tests tests::phoenix
```

Set `SURFPOOL_TEST_RPC_URL` to use a private endpoint if the public one rate-limits. Each run
reads the deployed Phoenix Eternal and Hawkeye bytecode once, so a program upgrade is picked up
on the next run.
