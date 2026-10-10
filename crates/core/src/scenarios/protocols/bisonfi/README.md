# BisonFi

A proprietary market maker (PMM), not an AMM. Four templates: price, depth, spread and freshness.

Because it is a market maker rather than a curve, it can be put into states no constant-product pool
can reach - quoting wide with deep inventory, or refusing to quote at all. Those are the scenarios
worth reaching for this protocol to test.

# Template index

| Template | Overrides |
|---|---|
| `bisonfi-fair-value` | the mid price BisonFi quotes around |
| `bisonfi-depth` | how far a trade moves BisonFi's price |
| `bisonfi-spread` | the spread BisonFi quotes around its mid |
| `bisonfi-freshness` | keeps the maker quoting in a slot, or models it going offline |

## Number formats

| You'll see | It means | Example |
|---|---|---|
| `fair_value` | price x 2^88, as a decimal **string** | $50 -> `"15474250491067253436239052800"` |
| `tick_offset` | 1/2,560,000 of the mid | `25600` = 1%, `2560` = 10 bps, `256` = 100 ppm |
| reserves | the mint's smallest unit | 1 USDC -> `1000000` |
| `last_update_slot` | a signed offset from the materialization slot | `0` = current slot, `-2` = two slots old |

`fair_value` exceeds what a JSON number holds exactly, so it must be quoted. To convert a spread:
`ticks = percent * 25600`.

## Picking a market

The templates intentionally have no default account. Callers must provide the pool account in the
override. The template guidance explains how to discover a pool when only the token pair is known.

Only version-3 pool accounts are supported. Direct API callers are responsible for supplying an
account that uses this layout.

# Recipes

## Set a price

```
template: bisonfi-fair-value
fair_value: "15474250491067253436239052800"    # $50 x 2^88, as a STRING
```

Set `fetchBeforeUse: true` only if this is the pool's first override, so the pool is loaded from the
upstream datasource before it is changed.

## Make large trades slip

```
template: bisonfi-depth
quote_reserve: <current / 10>     # makes SELLING the base asset expensive
base_reserve:  <current / 10>     # makes BUYING it expensive
```

The side the pool pays *out* of is the side that constrains the trade. Set both if the scenario does
not fix a direction.

**Reach for an order of magnitude.** The response is not linear - a trade worth a couple of percent of
a reserve barely notices that reserve being quartered.

**Lower, never raise.** These fields mirror the balances of the vaults, which this template does not
touch. Lowering is safe. Raising one above the vault's real balance makes the program compute a payout
the vault cannot cover, and the swap fails when it settles.

## Make a market unable to fill

The same template, taken further: set the payout reserve to `0` to make that direction return no fill
or fail with insufficient liquidity. Use `quote_reserve: 0` for a base-asset sale and
`base_reserve: 0` for a base-asset purchase. This is useful for testing how a router handles a venue
that cannot fill at all.

## Keep the venue quoting

**A pool loaded into the local VM eventually goes stale by itself.** The local VM does not republish
the mid. The exact allowed age is maker policy and can vary between markets or deployments. Once
stale, the price, depth and spread templates are silently ignored. Refresh the timestamp to keep the
venue alive for as long as your scenario needs.

```
template: bisonfi-freshness
last_update_slot: 0              # relative to the materialization slot
```

Refreshing resumes the price the venue already held - no new price is needed. The value is a signed
offset, not an absolute slot: `0` means "published in this slot". If the scenario spans enough slots
for the quote to become stale again, schedule another freshness override in each slot where the quote
must be usable with `fetchBeforeUse: false`. A later upstream fetch replaces the full pool account;
if one is required, reapply the scenario's price, depth and spread overrides after it in that slot.

An immediate scenario may not need this. A scenario that spans multiple slots should keep the quote
fresh explicitly rather than depending on the venue's current tolerance.

## Quote a wide spread

```
template: bisonfi-spread
working_levels.0.tick_offset:            -25600     # 1% below mid
configured_levels.0.tick_offset:         -25600
continuation_levels.0.tick_offset:       -25600
continuation_source_levels.0.tick_offset: -25600
```

**Set all four properties of a side, or all eight.** The `.0.` paths are the bid side, the `.4.` and
`.5.` paths the ask side. Setting only some of them produces a spread that varies with timing.

**Signs matter.** Bid offsets are negative and price SELLS of the base token. Ask offsets are positive
and price BUYS.

Do not use `0` to mean "no offset" - use a small magnitude instead.

## Reprice or widen mid-flight

Schedule two steps on the same field a couple of slots apart: the caller prices on one number and
executes against another. Works with `bisonfi-fair-value` (the mid moves) or `bisonfi-spread` (the
maker widens).

Both **revert**, caught by the caller's own minimum-output bound - the opposite symptom to a dark
maker, which succeeds with zero. Testing the pair is more informative than either alone: one failure
is detectable by a consumer and one is not.

## Arbitrage against an AMM

Move `fair_value` away from an AMM's price on the same pair and the two venues disagree by a real,
executable margin - both legs fit in one transaction. Two things to get right:

- **Use an exact-output swap on the AMM leg.** Instruction amounts are fixed when the transaction is
  built, so a leg that buys "whatever N USDC gets" cannot be followed by one that sells exactly that.
  Ask the AMM for a known quantity and pay whatever it costs.
- **Expect the undislocated round trip to lose money** - the taker pays a fee on both venues. The
  dislocation has to clear that before any profit appears, and a control run showing a profit at the
  true mid means you are measuring something other than a round trip.

# Troubleshooting

| Symptom | Fix |
|---|---|
| A price, depth or spread override had no effect and nothing errored | The quote is stale, and the freshness gate runs first. Refresh `last_update_slot` - see "Keep the venue quoting" |
| The pool quotes nothing at any size | Probably one of the dormant markets. Check how far `last_update_slot` is behind the chain |
| A spread override does nothing | You set some of a side's four properties but not all, or the trade is too small - very small trades do not consult the ladder. Try a percent or so of `base_reserve`, and try a few sizes |
| A stale market returns 0 instead of reverting | Not a bug: a stale venue returns zero and the transaction SUCCEEDS, and the swap's minimum-output bound is not enforced on that path |
| The quote becomes stale again later in the scenario | Schedule another freshness override with `fetchBeforeUse: false` in each slot where the quote must be usable |
| `Custom(60)` | A Token-2022 mint whose token accounts need extension data matching that mint |
| A large swap exhausts its compute budget | The default 200k compute budget may be too small; request about 1.4M units |
| A freshness override does not seem to age the pool | If your harness derives its clock from the pool's own `last_update_slot`, aging the account moves the clock with it. Apply the override after the clock is taken |
