# PumpSwap (pump-amm)

The AMM a pump.fun coin trades on after its bonding curve completes and migrates. For the
bonding-curve side and the full lifecycle, see [`../pump/README.md`](../pump/README.md).

Program: `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`.

## The bundled IDL

The bundled `Pool` type is 287 bytes. An IDL write decodes the whole type, so on a shorter
pool it fails, and the surfnet skips the whole override with a warning. A census of all
1,610,872 `Pool` accounts on 2026-11-18 found 1,334,394 pools of 287, 300 or 301 bytes, which
take writes, and 276,478 shorter pools, which do not:

| Pool size (bytes) | 211    | 243    | 244    | 245     | 261   | 270 | 271    |
| ----------------- | ------ | ------ | ------ | ------- | ----- | --- | ------ |
| Pools             | 37,410 | 74,316 | 40,594 | 101,128 | 9,549 | 806 | 12,675 |

124,158 of them, the pools of 245 to 271 bytes, took writes with the 245-byte type of the
on-chain IDL. The bundled `GlobalConfig` type is 949 bytes, the size of the live account.

## Templates

| Template                  | Account        | Selected by   | Use for                                                                           |
| ------------------------- | -------------- | ------------- | --------------------------------------------------------------------------------- |
| `pump-amm-pool-state`     | `Pool`         | pool address  | any pool, including non-canonical or non-WSOL ones                                |
| `pump-amm-canonical-pool` | `Pool`         | coin mint     | the canonical WSOL pool of a migrated coin, derived so you don't need its address |
| `pump-amm-global-config`  | `GlobalConfig` | — (singleton) | pool fees and disable flags                                                       |

## Field reference

What each overridable field means and what overriding it lets you model.

### `Pool`

| Field                    | Meaning                                                           | Override it to                                                         |
| ------------------------ | ----------------------------------------------------------------- | ---------------------------------------------------------------------- |
| `lp_supply`              | Total LP token supply before user burns and lock-ups              | model LP state                                                         |
| `coin_creator`           | Pubkey accruing the coin-creator fee for this pool                | point creator fees at a key you control                                |
| `virtual_quote_reserves` | Signed quote amount added to the quote vault balance when quoting | shift the effective quote (reprice) without touching any vault balance |

`virtual_quote_reserves` is a signed `i128` at `Pool` bytes 245 to 260. It is not 0 on every
pool: a census of all 1,610,872 `Pool` accounts on 2026-11-18 found 129,698 of the 1,357,424
pools of 261 bytes or more with a non-zero value (89,035 positive, 40,663 negative), such as
-769230 on `GseMAnNDvntR5uFePZ51yZBXzNSn7GdFPkfHwfr6d77J`. A write replaces the current value.

The other price-setting reserves live in the pool's token accounts (`pool_base_token_account`
/ `pool_quote_token_account`), not the `Pool` account - move those with the spl-token
template.

### `GlobalConfig` (singleton, `["global_config"]`)

| Field                                                                               | Meaning                                                            | Override it to                                    |
| ----------------------------------------------------------------------------------- | ------------------------------------------------------------------ | ------------------------------------------------- |
| `lp_fee_basis_points`, `protocol_fee_basis_points`, `coin_creator_fee_basis_points` | Legacy flat fees; live trades read the fee program's `FeeConfig`   | legacy - won't change what a swap charges         |
| `disable_flags`                                                                     | Bitmask disabling individual instructions (0 = everything enabled) | disable specific instructions to test error paths |

## Pricing

PumpSwap is a constant-product AMM. The reserves that set the price live in the pool's two
token accounts, not in the `Pool` account. Effective quote reserves are the quote vault
balance Q plus `Pool.virtual_quote_reserves` V.

Two ways to move the price:

- override `virtual_quote_reserves` on the pool — shifts the effective quote without
  touching any balance. To scale the effective quote reserves by k, write k * (Q + V) - Q;
- override the vault balances with the `spl-token-account-balance` template — the vault
  addresses are in the `Pool` account's `pool_base_token_account` /
  `pool_quote_token_account` fields.

For the canonical WSOL pool of a migrated coin, the Studio preset loads the existing
`pump-amm-canonical-pool` template and stores a standard scenario. AI clients use the same
template through the generic `create_scenario` MCP tool. See the complete example in
[`../pump/README.md`](../pump/README.md).

## Notes

- The canonical template only works for coins that migrated to PumpSwap (roughly March 2025
  onward). Coins that graduated earlier went to Raydium and have no canonical pool — use
  `pump-amm-pool-state` with the pool address for those.
- Fees on a live trade come from the external fee program's `FeeConfig`, not from the
  basis-point fields on `GlobalConfig` (those are legacy). Overriding them here won't change
  what a swap charges.
- Always set `fetchBeforeUse: true` so the fields you don't override keep their live values.
