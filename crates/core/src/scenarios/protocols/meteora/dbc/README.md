# Meteora Dynamic Bonding Curve (DBC) state preparation

Declarative state-preparation templates for Meteora's Dynamic Bonding Curve, the launchpad program
behind most non-pump.fun token launches on Solana. A token launched through DBC trades on a
virtual pool (a price curve over the pool's own `base_reserve` / `quote_reserve`) until the quote
raised reaches the threshold set in the pool's `PoolConfig`; the pool then graduates, stops
trading, and its liquidity migrates to a Meteora DAMM pool.

The program publishes an Anchor IDL, so the templates use the standard IDL override path.

## Program identity (verified 2026-11-04)

|                    | Dynamic Bonding Curve                                                                          |
| ------------------ | ---------------------------------------------------------------------------------------------- |
| Program ID         | `dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN`                                                  |
| ProgramData        | `HUfnSSiJxgspQm6C1rkqv6L3XgVtn7AESApgCQpCXCYh`                                                 |
| Last deployed slot | 457502600                                                                                      |
| Bundled IDL        | the program's on-chain IDL account, fetched 2026-10-28 (`metadata.version` 0.1.10), reformatted |

A later deployment slot than the one above means the program was upgraded and this integration
must be revisited (layouts, thresholds, error codes).

## Templates

| Template                    | Account       | Address                | Use for                                                              |
| --------------------------- | ------------- | ---------------------- | -------------------------------------------------------------------- |
| `meteora-dbc-pool-custom`   | `VirtualPool` | caller-provided pubkey | a launch pool's reserves, price and migration state                  |
| `meteora-dbc-config-custom` | `PoolConfig`  | caller-provided pubkey | the graduation thresholds shared by every pool created from a config |

Both templates take the account's address directly. The pool address is a PDA of
`["pool", config, max(base_mint, quote_mint), min(base_mint, quote_mint)]` where max and min are by
byte order, which the seed grammar cannot express; the pool is also simply the `pool` account of
the swap instruction you are testing, and its `config` field names the PoolConfig.

Always set `fetchBeforeUse: true` so non-overridden fields keep their live values. Values are JSON
numbers; the two `u128` fields (`sqrt_price`, `migration_sqrt_price`) exceed 2^53 and go in as
decimal strings.

## Field reference

### `VirtualPool`

| Field                | Meaning                                                              | Override it to                                                                   |
| -------------------- | -------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| `quote_reserve`      | quote raised so far (raw units)                                      | place the pool anywhere on its curve, up to and past graduation                  |
| `base_reserve`       | launched tokens the curve still holds (raw units)                    | keep consistent with `quote_reserve` when you move the pool along the curve      |
| `sqrt_price`         | current Q64.64 square-root price (`u128`)                            | make a transaction's quote stale                                                 |
| `is_migrated`        | 1 once liquidity moved to the DAMM pool                              | model the post-migration state                                                   |
| `migration_progress` | 0 PreBondingCurve, 1 PostBondingCurve, 2 LockedVesting, 3 CreatedPool | model each stage between graduation and the DAMM pool                            |
| `activation_point`   | slot or timestamp (per the config's `activation_type`) trading opens | test a client against a pool that has not opened yet                             |

### `PoolConfig`

| Field                      | Meaning                                              | Override it to                                                      |
| -------------------------- | ---------------------------------------------------- | ------------------------------------------------------------------- |
| `migration_quote_threshold` | quote reserve at which pools graduate               | graduate a live pool with one small buy on the surfnet              |
| `migration_base_threshold` | base tokens the pool must hold to seed the DAMM pool | keep the migration path valid after moving the quote threshold      |
| `migration_sqrt_price`     | Q64.64 price at which the curve ends                 | keep the curve's end consistent with the thresholds                 |

## Failure modes these reproduce

Measured over the program's mainnet traffic (TxWhy failure index, 3.6 million DBC transactions
since 2026-09-21, 24% of them failed), the three errors below account for nearly every real-user
DBC failure that is not a plain balance shortfall.

| Scenario              | Override                                                                                   | What the program does                                                                                                  |
| --------------------- | ------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------- |
| pool already graduated | `quote_reserve` = config's `migration_quote_threshold` (plus `is_migrated: 1`, `migration_progress: 3`) | every swap fails with `PoolIsCompleted` (6013) before any token moves                                        |
| pool nearly full      | `sqrt_price` just under the config's `migration_sqrt_price`, `quote_reserve` just under the threshold | an exact-in buy larger than the room fails with `InsufficientLiquidity` (6033); partial-fill `swap2` takes the room |
| stale quote           | `sqrt_price` up (under `migration_sqrt_price`) and `quote_reserve` up (under the threshold), as if a buy landed | a swap whose `minimum_amount_out` came from the old price fails with `ExceededSlippage` (6002)                |
| graduate it yourself  | `meteora-dbc-config-custom`: `migration_quote_threshold` just above the live `quote_reserve` | the next buy of at least the difference completes the curve; the following swap fails with `PoolIsCompleted`            |

Each row was played on a surfnet (surfpool 1.7.0, mainnet fork) against a live pool by writing the
same fields the template writes, with a real 0.05 SOL buy built by Meteora's SDK as the probe:
graduated and nearly full refused as stated; moving `quote_reserve` alone to just under the
threshold left the buy valid, which is why the nearly-full row moves `sqrt_price`.

Notes:

- The swap math reads `sqrt_price` and the config's curve segments; `quote_reserve` is bookkeeping
  that only the graduation check (`quote_reserve >= migration_quote_threshold`) reads. Move both
  together, or the pool reads as graduated while its price says otherwise.
- The pool template writes the pool account only. The base and quote vaults (token accounts the
  pool names) keep their live balances, so a raised `quote_reserve` is not backed by quote in the
  vault. Swap failures are refused before any transfer, so that is fine for them; to play a full
  migration, also set the vault balances with the SPL token template, and keep
  `migration_base_threshold` within what the base vault holds plus fees and vesting
  (`InsufficientLiquidityForMigration`, 6029).
- A config is shared by every pool a launchpad partner created from it; overriding it moves the
  graduation point of all of them on the surfnet.
