# HumidiFi

A proprietary market maker (PMM), not an AMM. Three templates control the fair value a market
quotes around, the spread it quotes at, and the vault inventory it pays from. Surfpool keeps the
quote current on its own once a scenario overrides a market.

HumidiFi publishes no IDL. Every market word it reads is stored XORed with a fixed per-offset key,
so the templates write through raw offsets with `u64_xor` encodings; the values you pass stay
plaintext. Program `9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp`, schema-8 market accounts
(1728 bytes, version word 8 at offset 1720).

## Template index

| Template                 | Overrides                                                 |
| ------------------------ | --------------------------------------------------------- |
| `humidifi-price`         | the fair value HumidiFi quotes around                     |
| `humidifi-spread`        | the spread HumidiFi adds to each side of its fair value   |
| `humidifi-vault-balance` | how many tokens HumidiFi has available to pay out a swap  |

## Number formats

| You'll see            | It means                                         | Example                                     |
| --------------------- | ------------------------------------------------ | ------------------------------------------- |
| `fair_value`          | quote atoms per base atom × 2^48, decimal string | SOL/USDC at 208 → `"58546795155816"`        |
| `spread`              | thousandths of a basis point on each side        | `25000` = 25 bps on both sides              |
| `tier_*_spread`       | basis points × 2^48, decimal string              | `"2814749767106560"` = 10 bps               |
| vault `amount`        | that mint's smallest unit                        | `1000000` = 1 USDC                          |

## Keep-alive (Surfpool handles it)

On mainnet the maker republishes each market's quote, and HumidiFi fills only while the Clock's slot
is at most the market's staleness limit past the quote's slot. The limits stored in market accounts
are a few slots (`2` on SOL/USDC). Nobody runs the maker against a surfnet, so Surfpool republishes
the quote every slot ([keep-alive](../../README.md#keep-alive)): it writes the current slot to the
quote's slot (offset 616) and leaves the limit as stored. There is nothing to schedule.

The price and spread templates keep the market they write quoting. `humidifi-vault-balance` takes
the vault's market as its `market` input and keeps that market quoting; the input is not written to
the vault. Keep-alive starts in the slot the override plays and runs until the network is reset.

Price conversion is:

```text
fair_value = floor(price × 2^48 / 10^(base_decimals - quote_decimals))
```

where `price` is quote tokens per base token. SOL/USDC has 9/6 decimals, so 208 USDC per SOL is
`floor(208 × 2^48 / 10^3) = 58546795155816`.

### Layout

| Offset | Encoding                             | Field               | Unit                             | Template                 |
| ------ | ------------------------------------ | ------------------- | -------------------------------- | ------------------------ |
| 8      | stored bytes                         | layout tag          | `[44,90,19,124,56,111,47,150]`   | read-only                |
| 384    | pubkey, four masked words            | quote mint          | n/a                              | read-only                |
| 416    | pubkey, four masked words            | base mint           | n/a                              | read-only                |
| 448    | pubkey, four masked words            | quote vault         | n/a                              | read-only                |
| 480    | pubkey, four masked words            | base vault          | n/a                              | read-only                |
| 576    | `u64_xor` `0xb957ed15dc877426`       | fair value          | quote atoms per base atom × 2^48 | `humidifi-price`         |
| 608    | `u64_xor` `0x6e9de2b30b19f1ea`       | max staleness       | slots                            | read-only                |
| 616    | `u64_xor` `0x6e9de2b30b19f1ea`       | last update slot    | slot                             | keep-alive               |
| 176    | `u64_xor` `0x40f849d0005707ba`       | first tier spread   | bps × 2^48                       | `humidifi-spread`        |
| 256    | `u64_xor` `0x40f249da005d07b4`       | second tier spread  | bps × 2^48                       | `humidifi-spread`        |
| 336    | `u64_xor` `0x40e449cc004b07ae`       | third tier spread   | bps × 2^48                       | `humidifi-spread`        |
| 800    | `u64_xor` `0x504156a22548f8dd`       | spread              | thousandths of a bp              | `humidifi-spread`        |
| 1720   | `u64`, plaintext                     | schema version      | version                          | read-only                |
| 64     | `u64` (SPL token account)            | vault amount        | mint's smallest unit             | `humidifi-vault-balance` |

The four pubkey words use the keys `fb5ce87aae443c38`, `04a2178451bac3c7`, `04a1178751b9c3c6` and
`04a0178651b8c3c5`. Surfpool does not unmask them for callers; see below.

## Picking a market

The templates have no default account, and the backend embeds no market catalog. Studio offers a
short list of featured markets and also accepts a market or vault address directly. API callers must
provide the market account in the override; `humidifi-vault-balance` takes one of that market's
vaults instead, with the market as its `market` input. The price and spread templates target the
1728-byte market account, and the vault template targets one of its SPL-token vaults.

Studio includes four featured markets:

| Choice       | Best suited for                                                       |
| ------------ | --------------------------------------------------------------------- |
| SOL / USDC   | SOL price shocks, stale quotes and liquidity tests                    |
| PENGU / USDC | Altcoin price and inventory stress tests; the deepest market          |
| WET / USDC   | A second altcoin market with classic SPL-token vaults                 |
| PUMP / USDC  | Volatile-token price tests; its base vault is a Token-2022 account    |

HumidiFi stores each market's mints and vaults masked, and a search by mint needs those masked
bytes, so start from a market address. When only a pair is named, each template's LLM guidance uses
the market Studio lists for it or asks for an address. Every vault is a token account owned by its
market, so `getTokenAccountsByOwner` with the market and a token program id lists the market's mints
without unmasking, which also works when only an address is given. The stablecoin is the quote and
the only other mint is the base; tokens sent to a market can add more mints, and the guidance then
asks for the pair. Some markets also own dust accounts; the guidance keeps the account holding the
most tokens and asks for the vault when every one is nearly empty.

Before it prepares any override, the guidance reads those token accounts and says the market looks
stopped when even the largest account of a pair mint is nearly empty.

Use `fetchBeforeUse: true` so the selected account is loaded into the local VM from the upstream
datasource before its bytes are changed. A later override that builds on an earlier one in the same
scenario uses `false`, or the re-fetch undoes the earlier write.

## Two rules that prevent misleading scenarios

**1. Name the market on a vault override.** Keep-alive republishes a market only once a scenario
overrides it. A scenario that only lowers a vault keeps its market quoting through the vault
template's `market` input; without it the market's quote goes stale within its limit and swaps reject
with `Custom(0xfaded)`.

**2. Do not repeatedly reset transaction-owned inventory.** Price and spread are configuration
inputs. A vault balance is state that swaps modify. Reapplying a vault override after every swap can
undo the swap and manufacture or erase inventory.

## Scenario ideas

### SOL price shock

1. Choose **SOL / USDC** in `humidifi-price`.
2. Set `fair_value` from the formula, for example `"58546795155816"` for 208 USDC per SOL.
3. Compare a swap before and after. Keep a run with the original price as a control.

### Maker stops quoting

1. Leave a market out of the scenario: no override names it, not even as a vault's `market`.
2. Nothing republishes its quote, so within its staleness limit every swap on that market reverts
   with `Custom(0xfaded)`. Confirm a router falls back to another venue.

### Maker cannot pay one side

1. In `humidifi-vault-balance`, choose the base vault to block users buying the base token, or the
   quote vault to block users selling it.
2. Lower the balance to `0`, or to a thin amount to watch the quote shrink first.
3. Apply it once, then confirm the affected direction fails and the opposite direction still fills.

# Recipes

## Set the fair value

```text
template: humidifi-price
fair_value: "58546795155816"      # SOL/USDC at 208 with 9/6 decimals
```

Raising `fair_value` makes the base token more expensive, so a quote-to-base swap returns
proportionally less base. A buy fills within 1% of the price-implied output at the stored and at
a doubled fair value. Pass the value as a string; it can
exceed what a JSON number holds exactly.

## Set a fixed spread

```text
template: humidifi-spread
spread: 25000                     # 25 bps on each side
tier_1_spread: "0"
tier_2_spread: "0"
tier_3_spread: "0"
```

HumidiFi widens each side by S basis points: a buy pays `fair × (1 + S/10000)` and a sell
receives `fair / (1 + S/10000)`. S is `spread / 1000` plus the spread of the size tier the trade
falls in; the market sets its tiers by quote notional. The maker republishes `spread` with every
quote upstream, while the local VM keeps the value it fetched until an override changes it. With
the tiers at 0, every trade size prices at the set spread on both sides, apart from about 1 bp of
inventory skew. A Jupiter swap in the local VM also pays HumidiFi's fallback spread (see
Troubleshooting).

## How the quote ages

HumidiFi fills while `clock_slot - last_update_slot <= max_staleness_slots` and rejects with
`Custom(0xfaded)` (1027565) one slot later; the boundary is inclusive. Keep-alive writes the current
slot every slot, so a market a scenario overrides is always 0 slots old.

## Drain inventory

```text
template: humidifi-vault-balance
account: <vault from getTokenAccountsByOwner>
amount:  0
```

The base vault pays quote-to-base swaps, the quote vault pays base-to-quote swaps. Lowering the
input-side vault leaves that direction unchanged. A thin payout vault still settles but quotes much
less, because HumidiFi prices against its inventory: at 0.5% of the stored base inventory a buy
returns roughly a thousandth of its output. This is not an AMM reserve formula; use
`humidifi-price` to move the mid.

# Troubleshooting

| Symptom                                                   | Fix                                                                                  |
| --------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| `Custom(1027565)` (`0xfaded`) after lowering a vault      | The vault override has no `market`, so nothing keeps that market quoting             |
| `Custom(1027565)` (`0xfaded`) on a market you did not override | Expected: nothing republishes a market no override names                        |
| `Custom(49)` after lowering a vault                       | The payout vault is empty; HumidiFi refuses the swap itself                          |
| Token program `Custom(1)` (InsufficientFunds)             | The payout vault is empty on a market that leaves the refusal to the token transfer  |
| `Custom(310)` (`0x136`) in both directions                | Both vaults are empty                                                                |
| A Jupiter swap in the local VM prices about 100 bps wider | Jupiter's swap carries a 16-slot window of upstream slots, and the local VM's clock is outside it, so HumidiFi adds the fallback spread stored in the shared account `Fnf3RYYN4uHEzUnCNAbeN4LrezyAbSJZ6Digr2ActVXC` at offset 536 (100 bps upstream). It applies to every template on every market; compare with a control run |
| Output collapses after lowering a vault                   | The payout vault is thin; HumidiFi prices against inventory                          |
| Price is off by a power of ten                            | Use both mints' decimals from the market's token accounts in the formula             |
| A vault override has no effect                            | It targets a dust account; use the market-owned account holding the most tokens      |
| A vault balance returns after a swap                      | Remove the later vault reset; it is undoing transaction-owned state                  |
