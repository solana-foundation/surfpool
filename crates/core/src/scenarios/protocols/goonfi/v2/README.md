# GoonFi V2

A proprietary market maker (PMM), not an AMM. Four templates control its external quote, the
reference band that guards the quote, the penalty for larger trades, and vault inventory. Surfpool
keeps the quote current on its own once a scenario overrides a market.

GoonFi does not derive its price from vault ratios. Each market reads a 32-byte oracle owned by a
companion publisher program, checks that quote against a reference band stored in the market, and
settles from its two token vaults.

Programs: market program `goonuddtQRrWqqn5nFyczVKaie28f3kDkHWkHtURSLE`, oracle publisher
`dijkbkCAKfFTCxQg3u1pg82gVU1jJGHBBRcteD11mBu`.

## Template index

| Template                | Overrides                                          |
| ----------------------- | -------------------------------------------------- |
| `goonfi-price`          | the bid and ask GoonFi quotes                      |
| `goonfi-reference-band` | the band that must contain the quote for a fill    |
| `goonfi-size-impact`    | how much worse larger trades fill, per direction   |
| `goonfi-vault-balance`  | how many tokens GoonFi has available for swaps     |

## Number formats

| You'll see                   | It means                                        | Example                          |
| ---------------------------- | ----------------------------------------------- | -------------------------------- |
| bid, ask, reference prices   | quote per base × 10^6, whatever the mint decimals | SOL at 99.74 USDC → `99740000` |
| size penalties               | millionths of the price                         | `10000` = 1%                     |
| vault `amount`               | that mint's smallest unit                       | `1000000` = 1 USDC               |

Price conversion is:

```text
price_x1e6 = human_price × 10^6
```

### Layout

| Account     | Owner            | Size | Offset | Field                    | Encoding | Written by              |
| ----------- | ---------------- | ---- | ------ | ------------------------ | -------- | ----------------------- |
| MarketState | market program   | 2048 | 0      | tag `30bc2f353458329a`   | bytes    | read only               |
| MarketState | market program   | 2048 | 40     | activity slot            | u64      | read only               |
| MarketState | market program   | 2048 | 80     | base mint                | pubkey   | read only               |
| MarketState | market program   | 2048 | 112    | quote mint               | pubkey   | read only               |
| MarketState | market program   | 2048 | 144    | base vault               | pubkey   | read only               |
| MarketState | market program   | 2048 | 176    | quote vault              | pubkey   | read only               |
| MarketState | market program   | 2048 | 208    | oracle                   | pubkey   | read only               |
| MarketState | market program   | 2048 | 240    | `sell_penalty_0..9`      | 10 × u64 | `goonfi-size-impact`    |
| MarketState | market program   | 2048 | 320    | `buy_penalty_0..9`       | 10 × u64 | `goonfi-size-impact`    |
| MarketState | market program   | 2048 | 1472   | sale size points (base atoms) | 10 × u64 | read only          |
| MarketState | market program   | 2048 | 1552   | purchase size points (quote atoms) | 10 × u64 | read only     |
| MarketState | market program   | 2048 | 1712   | `reference_price_a_x1e6` | u64      | `goonfi-reference-band` |
| MarketState | market program   | 2048 | 1720   | `reference_price_b_x1e6` | u64      | `goonfi-reference-band` |
| PriceOracle | oracle publisher | 32   | 0      | `bid_price_x1e6`         | u64      | `goonfi-price`          |
| PriceOracle | oracle publisher | 32   | 8      | `ask_price_x1e6`         | u64      | `goonfi-price`          |
| PriceOracle | oracle publisher | 32   | 16     | last update slot         | u32      | keep-alive              |
| PriceOracle | oracle publisher | 32   | 20     | decay multiplier         | u32      | read only               |
| PriceOracle | oracle publisher | 32   | 24     | publish timestamp (ms)   | u64      | read only, not consulted |
| TokenAccount | token program   | 165  | 64     | `amount`                 | u64      | `goonfi-vault-balance`  |

## Picking a market

The templates have no default account, and the backend embeds no market catalog. Studio offers a
short list of featured markets and also accepts a market, oracle or vault address directly; API and
MCP callers put the account in the override. The band and size templates target the market account, the
price template targets the oracle the market points at (bytes 208..240), and the vault template
targets one of the market's SPL-token vaults (base at 144..176, quote at 176..208). Market and vault
templates also take that oracle in their `oracle` input, which is never written: it names the oracle
keep-alive keeps current.

Studio includes four featured markets:

| Choice      | Best suited for                                                       |
| ----------- | --------------------------------------------------------------------- |
| SOL / USDC  | SOL price shocks, band moves and liquidity tests                      |
| HYPE / USDC | Altcoin price and inventory stress tests                              |
| JUP / USDC  | A second altcoin market with deep classic SPL-token vaults            |
| PUMP / USDC | Volatile-token price tests; its base vault is a Token-2022 account    |

For any other pair, each template's LLM guidance finds the market on the program: three calls filter
its accounts by the two mints and return each market with its oracle and both vaults. A pair can
have several markets; the guidance keeps the one whose base vault holds the most tokens, says so
when both its vaults are nearly empty, and names the direction that cannot fill when one is. A
market that stopped trading usually has near-empty vaults, a zero reference band and an oracle
millions of slots old.

Do not reuse an oracle or vault merely because the token pair looks similar. Use
`fetchBeforeUse: true` so the selected account is loaded into the local VM from the upstream
datasource before its bytes are changed.

## Keep-alive (Surfpool handles it)

On mainnet GoonFi's maker republishes each market's quote every slot or few. Nobody runs it against a
surfnet, so while a scenario with a GoonFi override is registered Surfpool republishes the oracle
every slot ([keep-alive](../../../README.md#keep-alive)): it stamps the update slot (byte 16) one slot
back, so the quote stays one slot old and every swap gets the best fill. The price stays as set. There is nothing to schedule:

- `goonfi-price` keeps the oracle it writes current.
- `goonfi-reference-band`, `goonfi-size-impact` and `goonfi-vault-balance` keep the oracle in their
  `oracle` input current. Without it, nothing keeps that oracle current, and a stale one rejects
  swaps with `0x15`.

Keep-alive starts in the slot the override plays and runs until the network is reset.

## Three rules that prevent misleading scenarios

**1. Move the band with the price.** A price can be encoded correctly and still never fill. The two
anchors have fixed roles: GoonFi rejects a sell whose bid sits above reference A, and a buy whose
ask sits below reference B, with error `0x24`, within a small per-market tolerance. Every
`goonfi-price` move needs `goonfi-reference-band` on the paired market, scaled by the same factor.
Each fill moves its own anchor: a sell sets A to its bid, a buy sets B to its ask. So a price move
made after swaps meets the anchors those swaps left: apply the band again in the same slot as the
price move.

**2. Do not repeatedly reset transaction-owned inventory.** Price and band are configuration
inputs. A vault balance is state that swaps modify. Reapplying a vault override
after every swap can undo the swap and manufacture or erase inventory.

**3. A later scenario meets an earlier band.** `fetchBeforeUse: true` reloads only the override's
own account. When an earlier scenario on the same surfnet moved a market's band, a later scenario
that reloads only the oracle still meets that band, and one direction rejects with `0x24`. Set the
band again in the later scenario.

## How quote age changes the fill

Keep-alive keeps a market a scenario overrides one slot old. A market no override names is not
republished, and ages like this. A quote fills best from its update slot to four slots after it.
From the fifth slot, each slot costs one decay step, whose size is set per market, until the quote rejects
with `0x15`. The maker can change a market's step; measured on the featured markets in November
2026:

| Slots since the update | SOL / USDC  | HYPE / USDC | JUP / USDC |
| ---------------------- | ----------- | ----------- | ---------- |
| 0 to 4                 | best fill   | best fill   | best fill  |
| 5                      | 0           | -30 bp      | 0          |
| 10                     | 0           | -180 bp     | 0          |
| 15                     | 0           | -330 bp     | 0          |
| 16                     | `0x15`      | -360 bp     | 0          |
| 20                     |             | -480 bp     | 0          |
| 21                     |             | `0x15`      | `0x15`     |

A swap in the same slot as the market's previous swap pays one decay step. From the next slot the
fill is normal again; repeated swaps leave nothing behind.

## How trade size changes the fill

Every fill pays a size penalty in millionths of the price (`10000` = 1%). Each direction has ten
size points, sales in base atoms and purchases in quote atoms, and a penalty for each. Between two
points the penalty is interpolated linearly; a trade smaller than the first point pays the first
point's penalty. `goonfi-size-impact` changes the penalties and leaves the market's size points as
they are. A penalty of `0` removes its point; a direction whose ten penalties are all `0` rejects
with `0xd`. Measured on the featured markets, compared with a 100 USDC trade:

| Trade        | SOL / USDC | HYPE / USDC | JUP / USDC |
| ------------ | ---------- | ----------- | ---------- |
| 1,000 USDC   | -0.4 bp    | -12 bp      | -1.2 bp    |
| 10,000 USDC  | -11 bp     | -65 bp      | -12 bp     |
| 100,000 USDC | -24 bp     | -155 bp     | vault too small |

## Scenario ideas

### SOL reprices by 50%

1. On **SOL / USDC**, use `goonfi-price` with bid and ask at 1.5 times the bid stored in its oracle.
2. Use `goonfi-reference-band` with both anchors at the same value.
3. Use `goonfi-vault-balance` on the **USDC vault** with half the expected sell output. Apply this
   transaction-owned balance once before the swaps being tested.

A sale of about 100 USDC worth of SOL fills at the new price with the original inventory, the same
sale fails with error `0x1` against the capped USDC vault, and a buy still fills from the SOL vault.

### Stablecoin depeg

1. Find the **USDT / USDC** market as [Picking a market](#picking-a-market) describes (Studio does
   not list it), then target its oracle in `goonfi-price` and the market in `goonfi-reference-band`.
2. Set bid, ask and both anchors to `950000` for $0.95 or `1050000` for $1.05.
3. Compare both swap directions against an unchanged control run.

### The maker refuses one direction

1. Choose a market in `goonfi-price` and leave the band untouched.
2. Raise the bid above reference A to block sells, or lower the ask below reference B to block buys.
3. Confirm the blocked side fails with `0x24` while the other side still fills.

### Large trades get much worse

1. Choose a market in `goonfi-size-impact`.
2. Double `sell_penalty_5` to `sell_penalty_9` and `buy_penalty_5` to `buy_penalty_9`; read the
   current values at market bytes 240 and 320 first.
3. Compare a 100 USDC trade, which is unchanged, with a 10,000 USDC trade, which fills worse.

### GoonFi cannot fill one side of a swap

1. In `goonfi-vault-balance`, choose the quote vault to block sells, or the base vault to block
   buys.
2. Lower the balance below the requested output.
3. Confirm the affected swap fails with `0x1` and the opposite direction still works.

# Recipes

## Set the PMM price

```text
template: goonfi-price
bid_price_x1e6: 150000000      # SOL at $150
ask_price_x1e6: 150000000

template: goonfi-reference-band
reference_price_a_x1e6: 150000000
reference_price_b_x1e6: 150000000
```

Set all four fields. A coupled 2x move doubles a sell's output and a coupled halving doubles a
buy's output, within 1%. Keep ask at or above bid.

## Make trade size not matter

```text
template: goonfi-size-impact
sell_penalty_0 ... sell_penalty_9: <one value, e.g. the market's sell_penalty_1>
buy_penalty_0  ... buy_penalty_9:  <one value, e.g. the market's buy_penalty_1>
```

With equal penalties, a 100 USDC trade and a 10,000 USDC trade fill at the same price, within a couple
of basis points. Set all twenty to `10000` to make every fill 1% worse.

## Reduce inventory or make a direction unfillable

```text
template: goonfi-vault-balance
account: <actual market vault>
amount:  <smaller raw token amount>
```

The base vault pays buys, the quote vault pays sells. A quote vault holding exactly a sell's output
still fills; one unit less rejects with `0x1`. Buys also price base inventory, so lowering the
base vault moves a buy's output slightly before it blocks it.

# Troubleshooting

| Symptom                                                   | Fix                                                                  |
| --------------------------------------------------------- | -------------------------------------------------------------------- |
| `Custom(36)` (`0x24`) after a price change                 | Move `goonfi-reference-band` by the same factor as the price         |
| `Custom(36)` (`0x24`) when a later scenario reloads the oracle | An earlier scenario's band is still set; apply `goonfi-reference-band` again |
| `Custom(21)` (`0x15`)                                      | Nothing republishes that oracle: override the price, or set `oracle` on the market and vault overrides |
| `Custom(38)` (`0x26`) on an ageing quote                   | A decayed fill fell about 5% below the fresh quote; set `oracle` as for `0x15` |
| `Custom(1)` (`0x1`) after lowering a vault                 | The token program refused the transfer: the payout vault holds less than the output |
| `Custom(9)` (`0x9`)                                        | A zero bid or ask; set both prices above 0                           |
| `Custom(13)` (`0xd`) on one direction after `goonfi-size-impact` | All ten penalties of that direction are 0; set at least one above 0 |
| A price move after swaps rejects one side with `0x24`      | The swaps moved the anchors; apply `goonfi-reference-band` again in the same slot |
| A fill is a few basis points worse than expected           | The swap shares a slot with the market's previous swap; swap one slot later |
| `Custom(15)` (`0xf`)                                       | The swap's minimum output is above the fill                          |
| A price override writes correctly but the fill is unchanged | The override targets another market's oracle; read it from 208..240 |
| A vault balance returns after a swap                       | Remove the later vault reset; it is undoing transaction-owned state  |
