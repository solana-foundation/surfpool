# Kamino

Surfpool bundles IDLs and override templates for **six Kamino programs**, so a scenario can put a
Kamino market into whatever state you need before your code runs against it.

This is a how-to. For how scenarios work in general see the [scenarios README](../../README.md)
every field's own purpose and units are on the template itself, visible in Studio and via
`get_override_templates`.

## Two rules that decide whether an override sticks

**1. Override inputs, not results.** Kamino stores settings someone chose (`liquidation_threshold_pct`)
and values it computed from them (`market_price_sf`, the Obligation's `*_value_sf`). Before a
liquidation it runs `refresh_reserve` and `refresh_obligation`, which recompute every computed value.
So overriding a computed value is discarded moments later.

| Want to change | Override this | Not this |
|---|---|---|
| A price | `kamino-scope-price` | `liquidity.market_price_sf` |
| Position health | `kamino-reserve-config` → `liquidation_threshold_pct` | `kamino-obligation-health` |

**2. Add `"persist": true`** only to inputs your scenario never writes - prices, risk config,
caps. Never to state your transactions mutate (reserve liquidity, obligation or vault balances):
re-applying reverts their writes each slot, so a swap leaves no trace and the arbitrage it measures
is not real.

## Number formats

| You'll see | It means | Example |
|---|---|---|
| `_sf` | value x 2^60 | $1.00 → `1152921504606846976` |
| `_bps` | basis points | `100` = 1% |
| `_pct` | whole percent | `74` = 74% |
| Scope `value` / `exp` | `value / 10^exp` | `$0.15` → value `15000000`, exp `8` |
| Farm stake, `reward_per_share_scaled` | value x 2^18 | |
| Token amounts | the mint's smallest unit | 1 USDC → `1000000` |

## Finding the Scope entry for a token

Every reserve names its price source. Read the reserve's
`config.token_info.scope_configuration`:

- `price_feed` - which Scope account to override
- `price_chain` - which entry in it (65535 means unused). If two entries are listed, the price is
  the **first multiplied by the second** - that's how a token quoted in SOL is priced.

Verified 2026-08-11:

| Scope account | Entries |
|---|---|
| `3t4JZcueEzTbVP6kLxXrL3VpWx45jDer4eqysweBchNH` | SOL 3, USDC 13, PYUSD 148, cbBTC 175 |
| `3NJYftD5sjVfxSnUdZ1wVML8f3aC6mp1CXCL6L7TnU8C` | SOL 0, JLP 416, POPCAT 492 |

---

# Recipes

## Make a position liquidatable

Two independent levers where either works, both together is safest.

```json
{
  "templateId": "kamino-scope-price",
  "scenarioRelativeSlot": 0, "enabled": true,
  "fetchBeforeUse": true, "persist": true,
  "account": { "pubkey": "3NJYftD5sjVfxSnUdZ1wVML8f3aC6mp1CXCL6L7TnU8C" },
  "values": { "prices.492.price.value": 2124828, "prices.492.price.exp": 8 }
}
```

```
kamino-reserve-config  on the collateral reserve
  config.liquidation_threshold_pct: 29     # was 40
```

**Why:** halving the collateral's price halves what Kamino thinks it is worth. Lowering the
threshold shrinks the borrow limit. Both survive `refresh_obligation`. See
[`examples/kamino-liquidation-arbitrage.json`](../../examples/kamino-liquidation-arbitrage.json)
for a complete, tested scenario.

## Turn a liquidation into an arbitrage

Crash the price in Scope but leave the DEX pools at their real price - the gap between them is the
profit. Add depth so the exit does not slip:

```
whirlpool-popcat-sol   liquidity: 5000000000000000     # sell the seized collateral
whirlpool-sol-usdc     liquidity: 50000000000000000    # route back to the debt token
```

## Age a loan instantly

```
kamino-reserve-state
  liquidity.cumulative_borrow_rate_bsf.value.0: <raise it>
```

**Why:** Kamino derives what a borrower owes from the ratio between this index and the borrower's
snapshot of it. Raising it accrues interest without waiting.

## Force a reserve to run dry

```
kamino-reserve-state    liquidity.total_available_amount: 0
kamino-reserve-limits   withdraw_queue.next_withdrawable_ticket_sequence_number: 7
kamino-lending-market-risk  withdraw_ticket_issuance_enabled: 1
```

**Why:** an empty reserve defers withdrawals into a queue. The market-level switch must be on or the
feature never activates. Build the ticket itself with `kamino-withdraw-ticket`.

## Block an action to test the rejection

```
kamino-reserve-limits       config.borrow_limit: 0          # no new borrows here
kamino-reserve-status       config.status: 1                # reserve obsolete
kamino-lending-market-risk  emergency_mode: 1               # market-wide wind-down
kamino-liquidity-strategy-guards  withdraw_blocked: 1       # strategy exit blocked
kamino-swap-global-config   flash_take_order_blocked: 1     # no flash fills
```

## Build a position from scratch

```
kamino-obligation-positions
  deposits.0.deposit_reserve:  <collateral reserve>
  deposits.0.deposited_amount: 10000000000
  borrows.0.borrow_reserve:    <debt reserve>
  borrows.0.borrowed_amount_sf: <amount x 2^60>
  has_debt: 1
```

**Why:** element paths let you set one slot. Supplying a whole array needs all 8 (deposits) or 5
(borrows) entries complete, padding included.

## Give a farm user claimable rewards

Fastest - an already-accrued balance, tests only the claim path:

```
kamino-farms-user-rewards   rewards_issued_unclaimed.0: 500000000
                            last_claim_ts.0: 0
```

Realistic - let the program compute the accrual:

```
kamino-farms-reward-accumulator   reward_infos.0.reward_per_share_scaled: <raise>
```

**Why:** claimable is `active_stake_scaled x reward_per_share_scaled - rewards_tally_scaled`.
Raising the farm's side and leaving the user's tally alone creates the gap they can claim.

## Simulate elapsed time

Every reward and fee mechanism accrues from a timestamp. Move it into the past and the next
accrual covers a longer period - no clock advancing needed.

```
kamino-farms-reward-emissions      reward_infos.0.last_issuance_ts
kamino-vault-fees                  last_fee_charge_timestamp
kamino-vault-rewards               reward_info.last_issuance_ts
kamino-liquidity-strategy-rewards  kamino_rewards.0.last_issuance_ts
```

## Make an Earn vault look profitable, or fail

```
# earned yield: assets up, shares unchanged
kamino-vault-state   token_available: 1000000000

# clean share-price assertion: no fees
kamino-vault-fees    performance_fee_bps: 0
                     management_fee_bps: 0

# withdrawal failure: all weight in one reserve, then starve it
kamino-vault-allocation  vault_allocation_strategy.0.target_allocation_weight: 100
kamino-reserve-state     liquidity.total_available_amount: 0
```

## Partially fill a limit order

```
kamino-swap-order
  initial_input_amount:   1000000000
  remaining_input_amount:  500000000    # half filled
  expected_output_amount: 100000000     # cheap for the taker
  tip_amount: <raise to attract a filler>
```

---

# Troubleshooting

| Rejection | Fix |
|---|---|
| Price rejected as stale | Set `prices.N.last_updated_slot` / `unix_timestamp` to now, or raise `config.token_info.max_age_price_seconds` on `kamino-reserve-oracle` |
| Price rejected for TWAP divergence | Move the matching entry with `kamino-scope-twap`, or raise `max_twap_divergence_bps` |
| Your override silently did nothing | The field name does not exist in the IDL - surfpool logs a `warn!` and drops the whole override. Check the log |
| `exceeds what a JSON number can hold exactly` | Pass large `u128`/`i128` values as decimal strings, e.g. `"1152921504606846976000"`. Plain JSON numbers are fine below 2^53 |
| `Account with discriminator ... not found in IDL` | The account is not Anchor-based (e.g. Raydium AMM v4). It cannot be overridden through the IDL path |
| `Failed to resolve account address` | The `pubkey` is not valid base58 |
| Override reverted after a transaction touched the account | Add `"persist": true` - but only if that field is an input, not state the transaction is meant to change |
| A value the program recomputes will not stay put | Pin the input it reads instead: Scope price over a Reserve's cached price, `liquidation_threshold_pct` over the Obligation's health fields |

---

# Template index

**Kamino Lend** &middot; `KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD`

| Template | Overrides |
|---|---|
| `kamino-reserve-state` |  Kamino Reserve liquidity, accrued fees and cached price |
| `kamino-reserve-config` |  Kamino Reserve LTV, liquidation thresholds and bonuses |
| `kamino-reserve-status` |  Kamino Reserve status and usage restrictions |
| `kamino-reserve-limits` |  Kamino Reserve caps and the withdrawal queue |
| `kamino-reserve-fees` |  Kamino Reserve origination, flash-loan and protocol fees |
| `kamino-reserve-interest-rate` |  the Kamino Reserve borrow-rate curve |
| `kamino-reserve-oracle` |  which oracle a Kamino Reserve reads, and its staleness guards |
| `kamino-reserve-rewards` |  Kamino Reserve reward emissions |
| `kamino-reserve-debt-term` |  Kamino Reserve fixed-term debt settings |
| `kamino-withdraw-ticket` |  a Kamino queued-withdrawal ticket |
| `kamino-reserve-main-sol` |  the SOL reserve of Kamino's Main Market |
| `kamino-reserve-main-usdc` |  the USDC reserve of Kamino's Main Market |
| `kamino-obligation-health` |  Kamino Obligation health metrics |
| `kamino-obligation-positions` |  the deposits and borrows of a Kamino Obligation |
| `kamino-obligation-orders` |  Kamino Obligation stop-loss and take-profit orders |
| `kamino-lending-market-risk` |  Kamino market-wide switches and liquidation limits |
| `kamino-lending-market-elevation-groups` |  Kamino e-mode elevation groups |

**Scope oracle** &middot; `HFn8GnPADiny6XqUoWE8uRPPxb29ikn4yTuPa9MF2fWJ`

| Template | Overrides |
|---|---|
| `kamino-scope-price` |  a price in Kamino's Scope oracle |
| `kamino-scope-price-source` |  where a Scope index reads its price from |
| `kamino-scope-twap` |  a Kamino Scope TWAP entry |

**Farms** &middot; `FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr`

| Template | Overrides |
|---|---|
| `kamino-farms-reward-emissions` |  a Kamino farm's reward schedule and budget |
| `kamino-farms-reward-accumulator` |  a Kamino farm's reward accumulator and staked totals |
| `kamino-farms-user-rewards` |  one user's farm stake and reward balances |
| `kamino-farms-farm-config` |  Kamino farm caps, lockups and cooldowns |
| `kamino-farms-global-config` |  the Kamino Farms treasury fee |

**Swap (LIMO)** &middot; `LiMoM9rMhrdYrfzUCxQppvxCSG1FcrUK9G8uLq4A1GF`

| Template | Overrides |
|---|---|
| `kamino-swap-order` |  a Kamino limit order's amounts and fill progress |
| `kamino-swap-global-config` |  Kamino limit order global switches and fees |

**Earn vaults** &middot; `KvauGMspG5k6rtzrqqn7WNn3oZdyKqLKwK2XWQ8FLjd`

| Template | Overrides |
|---|---|
| `kamino-vault-state` |  Kamino Earn vault balances and deposit limits |
| `kamino-vault-fees` |  Kamino Earn vault performance, management and exit fees |
| `kamino-vault-allocation` |  how a Kamino Earn vault spreads deposits across reserves |
| `kamino-vault-rewards` |  Kamino Earn vault reward emissions |
| `kamino-vault-reserve-whitelist` |  a Kamino Earn vault reserve whitelist entry |

**Liquidity** &middot; `6LtLpnUFNByNXLyCoK9wA2MykKAmQNZKBdY8s47dehDc`

| Template | Overrides |
|---|---|
| `kamino-liquidity-strategy-balances` |  a Kamino Liquidity strategy's holdings and shares |
| `kamino-liquidity-strategy-rewards` |  Kamino Liquidity strategy reward balances |
| `kamino-liquidity-strategy-guards` |  Kamino Liquidity strategy caps and slippage guards |
| `kamino-liquidity-strategy-fees` |  the Kamino Liquidity strategy's cut of fees and rewards |
