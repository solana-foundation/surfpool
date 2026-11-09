# SolFi V2

A proprietary market maker (PMM), not an AMM. Four templates control its external price, spread by
trade size, swap pressure and vault inventory. Surfpool keeps the oracle current on its own while a
scenario is registered.

SolFi does not derive its mid from vault ratios. Its external oracle supplies the price, market
splines widen around that price, and the vaults provide inventory and settlement capacity.

## Template index

| Template              | Overrides                                                              |
| --------------------- | ---------------------------------------------------------------------- |
| `solfi-price`         | the price SolFi uses for swaps                                         |
| `solfi-size-impact`   | the extra cost SolFi adds when buying or selling, at each trade size   |
| `solfi-swap-pressure` | the count of large swaps in a row that makes SolFi widen, and its reset |
| `solfi-vault-balance` | how many tokens SolFi has available for swaps                          |

## Number formats

| You'll see                            | It means                                               | Example                                              |
| ------------------------------------- | ------------------------------------------------------ | ---------------------------------------------------- |
| `price_coefficient`, `price_exponent` | coefficient × 10^exponent, adjusted for token decimals | WSOL/USDC $50 at exponent -10 → `500000000`          |
| directional curve `y`                 | scaled by the oracle's widening scale                  | WSOL/USDC `10000` → 1% with a neutral age multiplier |
| `max_widening`                        | tenths of a ppm                                        | `100000` = 1%, `10000` = 0.1%                        |
| size-curve `x`                        | quote-token smallest units                             | `1000000000` = 1,000 USDC                            |
| vault `amount`                        | that mint's smallest unit                              | `1000000` = 1 USDC                                   |

Price conversion is:

```text
human_price = coefficient × 10^exponent × 10^(base_decimals - quote_decimals)
```

The exponent is stored independently from the coefficient. Set both fields together so the intended
human price does not depend on the exponent already stored in the selected oracle account.

## Picking a market

Market templates target the market account, the price template its embedded oracle, and the vault
template one of its SPL-token vaults. Market and vault templates also take that market's oracle,
market bytes 24..56, in their `oracle` input, which is never written: it names the oracle keep-alive
keeps current. The backend templates do not
embed a market catalog. Use the template's LLM guidance to discover a market from the SolFi program,
verify the pair through both vault mints, and select the corresponding target account. Empty markets
are valid SolFi state, but their presence on chain is not a promise that a swap can currently settle.

Do not reuse an oracle or vault merely because the token pair looks similar. Use
`fetchBeforeUse: true` so the selected account is loaded into the local VM from the upstream
datasource before its bytes are changed. Fetch an account only on its first override. A later fetch
replaces the full local account and can erase earlier overrides on its other fields.

## Keep-alive (Surfpool handles it)

On mainnet SolFi's maker republishes each market's oracle every couple of slots. Nobody runs it
against a surfnet, so while a scenario with a SolFi override is registered Surfpool republishes the
oracle every slot ([keep-alive](../../README.md#keep-alive)): the publication slot (byte 16), the
publication time in milliseconds (byte 24) and a validity horizon of 200 slots (byte 40). The price
stays as set. There is nothing to schedule:

- `solfi-price` keeps the oracle it writes current.
- `solfi-size-impact`, `solfi-swap-pressure` and `solfi-vault-balance` keep the oracle in their
  `oracle` input current. Without it, nothing keeps that oracle current, and an expired one rejects
  swaps with error 23.

Keep-alive starts in the slot the override plays and runs until the network is reset. After
`surfnet_timeTravel` it republishes the oracle for the slot the jump lands on.

## Three rules that prevent misleading scenarios

**1. Give market and vault overrides their oracle.** A price or spread can be encoded correctly and
still never reach the quote if the oracle has expired. Set `oracle` on every market and vault
override, or override the price, so keep-alive keeps the oracle current. If a later upstream fetch of
the oracle is required, reapply `solfi-price` after it in that slot.

**2. Do not repeatedly reset transaction-owned inventory.** Price and spline settings are
configuration inputs. A vault balance is state that swaps modify. Reapplying a vault override after
every swap can undo the swap and manufacture or erase inventory.

**3. Swap pressure builds up only within a slot.** SolFi counts large swaps in a row in each direction
and widens that direction as the count grows: on WSOL/USDC ten 1 SOL sales in a row cost about 95 bps
more than the first. The maker's republication restarts the count, and keep-alive republishes every
slot, so swaps in a later slot price like the first, as on mainnet. To measure several large swaps
within one slot from a clean count, apply `solfi-swap-pressure` with the count at `0` first. See
"Reset swap pressure" below.

## Scenario ideas

### PMM risk-off during a SOL crash

This scenario models a maker that remains available for small trades but protects itself after SOL
falls: it marks SOL at $50, pays 1% less when buying SOL, and limits its USDC payout inventory to 25
USDC.

1. On **WSOL / USDC**, use `solfi-price` with exponent `-10` and coefficient `500000000`.
2. Use `solfi-size-impact` with all eight buy-base knots at `1000`, all eight sell-base knots at `10000`, age multiplier
   `1000`, additional widening `0`, and maximum widening `100000`.
3. Use `solfi-vault-balance` on the WSOL/USDC **USDC vault** with amount `25000000`. Apply this
   transaction-owned balance once before the swaps being tested.

The deployed-program integration test confirms the complete four-template scenario: a 0.1 SOL sale
still fills near 4.95 USDC, the opposite direction is not widened, a 1 SOL control sale fills with
the original vault, and the same 1 SOL sale fails with error 18 after the 25 USDC limit is applied.

### SOL price shock

Use this to test whether a router, trading strategy or lending flow reacts correctly when SolFi's SOL
price moves suddenly.

1. Choose **WSOL / USDC** in `solfi-price`.
2. Set both price fields to the new price. For example, exponent `-10` and coefficient `500000000`
   means $50 per SOL.
3. Compare a swap before and after the price change. Also keep a run with the original price as a
   control.

### USDT depeg

Use this to model USDT trading below or above one dollar and observe route selection or collateral
valuation.

1. Choose **USDT / USDC** in `solfi-price`.
2. At exponent `-10`, use coefficient `9500000000` for $0.95 or `10500000000` for $1.05.
3. Compare both swap directions so the test proves the new price is applied reciprocally.

### Maker becomes cautious in one direction

Use this to test what happens when SolFi still trades but strongly discourages users from buying one
asset from it.

1. Choose the market in `solfi-size-impact`.
2. Raise all eight **Buy-base** knots to make buying WSOL or USDT more expensive, or all eight
   **Sell-base** knots to make selling it more expensive.
3. Keep the other direction low, set the age multiplier to `1000`, additional widening to `0`, and
   set the maximum high enough to allow the requested spread.
4. Set the market's oracle in `oracle`, then compare equal-notional swaps in both directions.

### Large orders receive a worse price

Use this to test order splitting and whether a router moves a large trade to another venue.

1. Choose a market in `solfi-size-impact`.
2. Give the first size knots small values and later knots progressively larger values.
3. Set the age multiplier to `1000` and additional widening to `0` so only trade size is changing the
   result.
4. Run small, medium and large swaps. Require at least one strict deterioration

### SolFi cannot fill one side of a swap

Use this to test fallback routing and transaction failure handling when the maker runs out of the
token it must pay.

1. In `solfi-vault-balance`, choose the WSOL or USDT vault to block users buying the base asset, or
   choose the USDC vault to block users selling the base asset.
2. Lower the balance enough that the requested swap cannot be paid.
3. Do not schedule repeated vault resets unless restoring inventory after every transaction is
   explicitly part of the test.
4. Confirm the affected swap fails with SolFi error 18 and that the opposite direction or an
   alternative venue still works.

# Recipes

## Set the PMM price

```text
template: solfi-price
price_exponent:    -10
price_coefficient: 500000000      # WSOL/USDC at $50 with 9/6 decimals
```

Set both fields. Doubling the coefficient doubles base-to-quote output and halves quote-to-base
output, subject to spread and rounding. The price-looking word in the market account is not the
authoritative input, but changing the external oracle is what reprices a fill.

Keep-alive keeps the oracle current for the whole scenario. Fetching the upstream oracle replaces the
full local account and erases the earlier price override. If a later fetch is required, reapply
`solfi-price` after it in that slot.
Repricing only SolFi while leaving another venue unchanged creates a real cross-venue dislocation
suitable for router, arbitrage and liquidation-path testing. Always include an undislocated control leg.

## How the quote ages

An expired SolFi oracle rejects the transaction with error 23. Valid is not constant either: SolFi
widens the quote as the oracle ages within its validity window. On WSOL/USDC, measured against the
deployed program:

| Slots since the last publication | 0 | 5 | 10 | 25 | 50 | 199 | 201 |
| -------------------------------- | - | - | - | - | - | - | - |
| Fill compared with a fresh quote | same | −3.6 bps | −10 bps | −21 bps | −34 bps | −44 bps | rejected (error 23) |

USDT/USDC does not drift; the amount depends on each market's age multiplier curve. Keep-alive
republishes every slot, so a scenario's oracle stays at age 0. An oracle no registered scenario keeps
current ages as in the table; set `age_multiplier_curve_y: 1000` with `solfi-size-impact` to switch
the age term off.

## Quote a constant directional spread

`solfi-size-impact` sets the widening at each of eight trade sizes; giving all eight the same value
makes it constant. Default WSOL/USDC example, one percent in both directions:

```text
template: solfi-size-impact
quote_to_base_y_0 ... quote_to_base_y_7: 10000     # all eight the same
base_to_quote_y_0 ... base_to_quote_y_7: 10000
age_multiplier_curve_y:      1000
additional_widening_curve_y: 0
max_widening:                100000
```

The `quote_to_base` values make buying the base asset more expensive, the `base_to_quote` values
selling it. Set only one direction for a risk-off scenario, or both for a symmetric spread.

The directional value is market-specific because the oracle contributes a scale:

```text
final_widening = directional_y × oracle_scale / 1000
output ≈ oracle_mid × (1 - final_widening / 10000000)
```

The verified WSOL/USDC oracle scale is `10000`, so `directional_y: 10000` reaches the 1% clamp. The
verified USDT/USDC scale is `1000`, so its corresponding value is `100000`. Do not copy the same
directional value across markets without reading the oracle scale.

Set the age multiplier to `1000` and additional widening to `0` when you need the configured spread
to be deterministic. Set `max_widening` at or above the intended result or the clamp will flatten it.

## Make large trades progressively worse

Use `solfi-size-impact`. Each direction has eight `y` properties, one for each `x` breakpoint stored
in the selected market account. Set all eight values for the side being modeled and use
non-decreasing values for ordinary liquidity deterioration.

```text
template: solfi-size-impact
quote_to_base_y_0: 1000
quote_to_base_y_1: 1000
quote_to_base_y_2: 10000
quote_to_base_y_3: 20000
quote_to_base_y_4: 40000
quote_to_base_y_5: 60000
quote_to_base_y_6: 80000
quote_to_base_y_7: 100000
age_multiplier_curve_y:      1000
additional_widening_curve_y: 0
max_widening:                100000
```

The template intentionally preserves the `x` positions stored in the selected market account. The
`x` axis is raw quote-token notional:

- quote-to-base uses the raw quote input
- base-to-quote converts the base input to quote notional at the oracle price before lookup.

SolFi linearly interpolates between adjacent points and uses the nearest endpoint outside the
configured range. The same oracle scale and maximum clamp described under "Quote a constant
directional spread" apply.

## Put the maker into directional risk-off mode

Use `solfi-size-impact`, with flat values for one constant penalty or rising values for a penalty
that grows with size:

```text
# Maker does not want to sell more base
quote_to_base_y_0 ... _7: <wide>
base_to_quote_y_0 ... _7: <tight>
```

Reverse the two values when the maker does not want to buy more base. The deployed-program tests
require the targeted response to dominate any cross-effect rather than assuming bit-identical output
on the other side.

## Reset swap pressure

SolFi counts consecutive large swaps in each direction. A swap counts by its value, not its token
amount: on WSOL/USDC from about 100 USDC. A swap the other way does not reset the count.
The count widens that direction twice over: the `additional_widening_curve_y` term grows with it, and
from the ninth swap the program multiplies the trade-size widening by about ten, capped by
`max_widening`.

```text
template: solfi-swap-pressure
consecutive_buys:  0
consecutive_sells: 0
```

A count of `0` is a complete reset: the next swaps price as if none had happened. A swap that sees the
oracle published later than at the previous swap also starts a new count, and keep-alive republishes
every slot, so the count builds up only across large swaps within one slot. The counts are state that
swaps change, like a vault balance - do not reapply them after every swap unless restoring them is the
point of the test.

## Reduce inventory or make a direction unfillable

```text
template: solfi-vault-balance
account: <actual market vault>
amount:  <smaller raw token amount>
```

The base vault pays quote-to-base swaps, the quote vault pays base-to-quote swaps. Reducing the
payout vault far enough makes that direction reject with SolFi error 18. Vaults also enter nonlinear
inventory policy, so changing the input-side vault can move a quote even though it is not paying out.

This is not an AMM reserve-price formula. Use `solfi-price` to change the mid. Do not repeatedly
reset a vault unless restoring the same inventory after every transaction is deliberately the scenario.

# Troubleshooting

| Symptom                                                        | Fix                                                                                        |
| -------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| Price or spread override writes correctly but the swap rejects | The oracle expired. Set `oracle` on market and vault overrides. Expiry is checked first    |
| `Custom(23)`                                                   | The oracle validity horizon is behind the executing slot                                   |
| `Custom(18)` after lowering a vault                            | The payout vault cannot settle the requested output                                        |
| Constant spread is smaller than requested                      | Account for `oracle_scale`, neutralize the age/additional curves, and raise `max_widening` |
| Size impact appears at the wrong base amount                   | Breakpoints are quote notional. Base input is converted at the oracle price first          |
| One direction widened instead of the other                     | Quote-to-base is buying base. Base-to-quote is selling base                                |
| A vault balance returns after a swap                           | Remove the later vault reset; it is undoing transaction-owned state                        |
| Fills get worse over repeated swaps in one direction in a slot | Swap pressure: apply `solfi-swap-pressure` with that direction's count at `0`              |
| Fills get worse over a few slots with nothing else changing    | Nothing keeps the oracle current. Set `oracle` on the override, or the age multiplier to 1000 |
