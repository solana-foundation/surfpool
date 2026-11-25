# Pump / PumpSwap state preparation

Declarative state-preparation templates for the pump.fun ecosystem. A pump.fun coin's
life spans two on-chain programs, so the integration covers both:

1. **Pump** (bonding curve launchpad) — new coins trade on a constant-product curve over
   synthetic (virtual) reserves until the curve is bought out (`complete = true`).
2. **PumpSwap** (`pump-amm`, pump.fun's AMM) — completed curves migrate here; reserves
   live in the pool's token accounts, not in the Pool account itself.

Both programs publish Anchor IDLs, so the templates use the standard IDL override path
(no raw-offset layout needed).

## Program identity (verified 2026-11-19)

|                    | Pump                                           | PumpSwap                                       |
| ------------------ | ---------------------------------------------- | ---------------------------------------------- |
| Program ID         | `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`  | `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`  |
| ProgramData        | `B5MvUwXdiW1NMM6QFFD3ssPKBujD4zMohncbM73Z2BQu` | `6naEzKeUuFh1Jeeu51NXQgr5qkXgXtc9WKNct4xynVJc` |
| Last deployed slot | 469811025                                      | 469811067                                      |

A later deployment slot than the one above means the program was upgraded and this
integration must be revisited (layouts, formulas, fee wiring).

## The bundled IDLs

An IDL write decodes the whole account type, so on an account shorter than its type it
fails, and the surfnet skips the whole override with a warning. The bundled `BondingCurve`
type is 166 bytes. A census of `BondingCurve` accounts on 2026-11-19 found 2,586,858 curves of
166 or 256 bytes, which take writes, and at least 13 million curves of 49 to 151 bytes, which
do not, such as the 150-byte Fartcoin curve `TBHe5tJnuT4CQbHorJ1uVdfUoaYGPKgfCpiv2jgesVN`. At
least 9,457,515 of them are 125 to 151 bytes long and took writes with the 125-byte type of the
on-chain IDL. The bundled `Global` type is 1,088 bytes, and the live account is 1,096 bytes. For `Pool` and
`GlobalConfig`, see [`../pump-amm/README.md`](../pump-amm/README.md).

Fees: every buy/sell passes the fee program's `FeeConfig` (market-cap fee tiers) as a
required account under `pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ`; the flat
basis-point fields on `Global` / `GlobalConfig` are legacy.

## Templates

| Template                    | Account        | Address                                                                   | Use for                                                                |
| --------------------------- | -------------- | ------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| `pump-bonding-curve-custom` | `BondingCurve` | PDA `["bonding-curve", mint]`                                             | any coin's curve, selected by mint                                     |
| `pump-global`               | `Global`       | PDA `["global"]` → `4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf`         | fee/init parameters for curves created after the override              |
| `pump-amm-pool-state`       | `Pool`         | caller-provided pubkey                                                    | any pool by raw address (only path for non-canonical / non-WSOL pools) |
| `pump-amm-canonical-pool`   | `Pool`         | PDA `["pool", u16le(0), PDA(pump, ["pool-authority", mint]), mint, WSOL]` | the canonical WSOL-quoted pool of a migrated coin, selected by mint    |
| `pump-amm-global-config`    | `GlobalConfig` | PDA `["global_config"]` → `ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw`  | pool fee/disable flags                                                 |

Notes:

- The mint catalogs offer only verified tokens whose address ends in `pump`
  (976 mints in the bundled catalog; `address_suffix: pump` in both `overrides.yaml`
  files). Coins outside the verified catalog — including freshly launched ones — are
  still reachable through the graduation MCP tool and its Studio preset, which validate
  live on-chain state instead of the catalog, or through the raw REST payload, which
  performs no catalog validation.
- `pump-amm-canonical-pool` covers WSOL-quoted canonical migrations only; any other
  pool goes through `pump-amm-pool-state` with its address.
- Always set `fetchBeforeUse: true` so non-overridden fields keep their live values.

## Field reference

What each overridable field means and what overriding it lets you model. Keep the
curve-lifetime invariants when you touch reserves (`virtual − real` = 279.9T tokens and
30 SOL of quote with today's mainnet `Global` defaults).

### `BondingCurve`

| Field                    | Meaning                                                                       | Override it to                                                            |
| ------------------------ | ----------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| `virtual_token_reserves` | Synthetic token reserves in the price formula (raw, 6 decimals)               | reprice the curve - spot = `virtual_quote / virtual_token`                |
| `virtual_quote_reserves` | Synthetic quote reserves (lamports for SOL-quoted coins)                      | reprice the curve                                                         |
| `real_token_reserves`    | Tokens the curve still holds; completion is when this reaches 0               | set how close to graduation the curve sits (a small value = one buy away) |
| `real_quote_reserves`    | Quote the curve actually holds                                                | model accumulated quote                                                   |
| `complete`               | True once bought out; a completed curve rejects buy/sell and can only migrate | flip `true` to model a graduated curve, `false` to reopen trading         |
| `creator`                | Coin creator that accrues creator fees via the creator vault                  | point creator fees at a key you control                                   |
| `token_mint`             | Selects which coin's curve (a PDA seed, not a stored field)                   | choose the target coin                                                    |

### `Global` (singleton, `["global"]`)

| Field                                                                                                                                                   | Meaning                                                          | Override it to                                                                                  |
| ------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `fee_basis_points`, `creator_fee_basis_points`                                                                                                          | Legacy flat fees; live trades read the fee program's `FeeConfig` | rarely useful (legacy)                                                                          |
| `initial_virtual_token_reserves`, `initial_virtual_sol_reserves`, `initial_virtual_quote_reserves`, `initial_real_token_reserves`, `token_total_supply` | Seed values a **new** curve is created with                      | change the launch parameters of curves created after the override (existing curves keep theirs) |
| `enable_migrate`                                                                                                                                        | Gates the `migrate` instruction                                  | set `true` to let a completed curve migrate to PumpSwap                                         |
| `pool_migration_fee`                                                                                                                                    | Fee charged when a curve migrates                                | model migration cost                                                                            |
| `withdraw_authority`                                                                                                                                    | Authority the `migrate` / withdraw path checks                   | set to a key you control to drive a real `migrate` transaction on a fork                        |

## Worked example: reset a curve to a fresh state

Create it in the Studio editor (Pump tile → _Override Bonding Curve (Custom)_), which
fills the envelope automatically, or POST the full REST `Scenario` shape below to
`/v1/scenarios` — every envelope field is required by the endpoint, and the `account`
derivation is copied verbatim from the template. Then press Play:

```json
{
  "id": "d2f8a1c4-7b3e-4e9a-8c5d-0f6b2a9e4d71",
  "name": "fresh pump curve",
  "description": "reset a live coin's curve to launch state",
  "tags": ["pump"],
  "overrides": [
    {
      "id": "curve-reset-0",
      "templateId": "pump-bonding-curve-custom",
      "label": "reset bonding curve to launch state",
      "enabled": true,
      "scenarioRelativeSlot": 0,
      "fetchBeforeUse": true,
      "account": {
        "pda": {
          "programId": "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",
          "seeds": [
            { "string": "bonding-curve" },
            { "propertyRef": "token_mint" }
          ]
        }
      },
      "values": {
        "token_mint": "<a pump.fun mint from the catalog>",
        "virtual_token_reserves": 1073000000000000,
        "virtual_quote_reserves": 30000000000,
        "real_token_reserves": 793100000000000,
        "real_quote_reserves": 0,
        "complete": false
      }
    }
  ]
}
```

Expected result: the coin's `BondingCurve` PDA holds exactly these values while every
other byte (token_total_supply, creator, the extend_account tail) stays live. Verify by
re-opening the override in the Studio field editor, or via `getAccountInfo`: u64 LE at
offsets 8 (virtual tokens), 16 (virtual quote), 24 (real tokens), 32 (real quote), bool
at 48 (complete). The surfpool log must contain no `skipping override` line. The curve must
be at least 166 bytes, the size of the bundled `BondingCurve` type (see
[The bundled IDLs](#the-bundled-idls)).

Variant — a semantically valid _completed_ curve (rejects buys/sells with
`BondingCurveComplete`): `complete: true` requires `real_token_reserves: 0`; keep the
curve-lifetime invariants (`virtual − real` = 279.9T tokens / 30 SOL of quote, verified
against live mainnet data).

## Prepare a graduation

The `create_pump_graduation_scenario` MCP tool builds the whole preparation from Surfnet
state, with no field math required from the caller. Accounts already present locally are
authoritative; Surfnet fetches only missing accounts from mainnet. The same tool powers the
_Pump Graduation_ preset card in Studio.

```json
{
  "tokenMint": "<SOL-quoted pump.fun mint still on its bonding curve>"
}
```

The coin must have a **Token-2022 mint**, a SOL-quoted incomplete curve, and no canonical
PumpSwap pool yet. Eligibility failures are returned in the MCP tool result. The preset
does not cover coins with a classic SPL-Token mint or a non-SOL quote mint — their curve
can still be overridden field by field with `pump-bonding-curve-custom`, but this
graduation flow is Token-2022 and SOL-quote only. A successful tool call
returns the Studio editor URL, where the stored scenario contains three overrides:

1. the curve one buy away from completion (`real_token_reserves` = the finishing buy,
   sized so the buy also clears the migration fee),
2. the curve vault topped up to `migration reserve + finishing buy` — the reserve is
   what `migrate_v2` moves into the pool, so draining the vault to match
   `real_token_reserves` would make migration fail with `ZeroBaseAmount`,
3. `Global.enable_migrate = true`.

Press Play, then drive it like a user would: a real `buy_v2` of the curve override's
`real_token_reserves` completes the curve, and a real `migrate_v2` creates the canonical
WSOL pool with the reserve as its base liquidity.

## Shock a migrated pool's price

Use the existing `pump-amm-canonical-pool` template through the standard scenario API or
the generic `create_scenario` MCP tool. The Studio preset follows the same path: it loads
the registered template, supplies the mint and reserve value, and stores a normal scenario.

```json
{
  "templateId": "pump-amm-canonical-pool",
  "values": {
    "base_mint": "<WSOL-paired migrated pump.fun mint>",
    "virtual_quote_reserves": 15000000000000
  },
  "scenarioRelativeSlot": 1,
  "fetchBeforeUse": true
}
```

`virtual_quote_reserves` is added to the quote vault balance when the AMM quotes, so raising
it makes the same sell return more quote without touching any token balance. The template
derives the canonical WSOL pool from `base_mint`. The value replaces the pool's current
value V, which is not 0 on every pool; to scale the effective quote reserves by k, write
k * (Q + V) - Q, where Q is the quote vault balance. If the derived pool is absent or shorter
than the bundled 287-byte `Pool` type, the surfnet logs a warning and leaves the pool
unchanged. After a successful Play, the identical sell transaction simulates with a higher
quote-token output than before.

To move the balances themselves, override a vault with the `spl-token-account-balance`
template instead. The vault addresses are in the pool's `pool_base_token_account` and
`pool_quote_token_account` fields.

## Verification

- Address identity: `cargo test -p surfpool-core --lib pump` — template PDAs pinned to
  externally documented addresses (pump-public-docs).
- MCP surface: `cargo test -p surfpool-mcp surfpool::tests` — compact template listing,
  catalog scoping, and generic scenario validation.
- Integration: `cargo test -p surfpool-core --features integration-tests pump` needs a
  network connection (`SURFPOOL_TEST_RPC_URL` overrides the public mainnet endpoint).
  It round-trips live mainnet curve, pool, and config accounts through the bundled
  IDLs to catch layout drift after a program upgrade, proves overrides touch only
  their target bytes on real account data, pins which live curves and pools are too short
  for the bundled types and are left unchanged, and exercises the graduation builder's
  validation against live state. The lifecycle test discovers a fresh, still-trading
  Token-2022 coin from the pump program's recent transactions (no fixed coin stays
  incomplete; the fork freezes its state on first read), prepares it through the
  production builder and materializer, then executes real `buy_v2` and `migrate_v2`
  against the live programs. The price-shock test raises `virtual_quote_reserves` on the
  established live canonical pool, compares a baseline and a shocked PumpSwap `sell`
  simulation and requires the quote-token output to rise, then executes the sell for
  real. Trading against a freshly migrated (cashback-era) pool uses a different fee wiring
  and is an open follow-up.
