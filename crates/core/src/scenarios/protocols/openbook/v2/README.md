# OpenBook v2

A central limit order book (CLOB) on Solana, program `opnb2LAfJYbRMAHHvqjCwQxanZn7ReEHp1k81EohpZb`.
Drift routes spot fills through it and Jupiter routes swaps through it, so its markets are worth
overriding when testing either integration.

Two accounts are covered: a `Market` (fees, expiry, deposits) and an `OpenOrdersAccount` (one
owner's balances and locked order lots on a market). The order book itself (`BookSide`) and the
fill queue (`EventHeap`) hold nested order trees and are not covered.

# Template index

| Template | Overrides |
|---|---|
| `openbook-v2-sol-usdc-market` | the SOL/USDC market |
| `openbook-v2-btc-usdc-market` | the BTC/USDC market |
| `openbook-v2-eth-usdc-market` | the ETH/USDC market |
| `openbook-v2-wif-usdc-market` | the WIF/USDC market |
| `openbook-v2-bonk-usdc-market` | the BONK/USDC market |
| `openbook-v2-market-custom` | any market by address, plus its oracle staleness window |
| `openbook-v2-open-orders` | an open orders account's free balances and locked lots |

## Picking a market

Anyone can create an OpenBook v2 market, so a pair has many: mainnet holds 18 SOL/USDC markets,
most of them empty. The five presets are the markets Drift's `OpenbookV2FulfillmentConfig`
accounts point at, which are the ones carrying real flow. For any other market use
`openbook-v2-market-custom` with the market's address.

None of the presets has an oracle, so oracle staleness only appears on the custom template. It has
no effect on a market whose `oracle_a` is empty.

## Number formats

| Field | Unit | Example |
|---|---|---|
| `maker_fee`, `taker_fee` | 10^-6 of the quote amount | `100` = 1 bps, `1000` = 10 bps |
| `time_expiry` | unix seconds | `0` = never expires, `-1` = expired now |
| `*_deposit_total`, `fees_available` | native units of the token | `1_000_000` = 1 USDC |
| `position.*_free_native` | native units of the token | `1_000_000_000` = 1 SOL |
| `position.*_lots` | lots of the market | see below |

A market's lot sizes convert lots to native units: base native = base lots × `base_lot_size`. The
presets' sizes:

| Market | Base decimals | `base_lot_size` | `quote_lot_size` |
|---|---|---|---|
| SOL/USDC | 9 | 100000 | 1 |
| BTC/USDC | 8 | 100 | 1 |
| ETH/USDC | 8 | 1000 | 1 |
| WIF/USDC | 6 | 10000 | 1 |
| BONK/USDC | 5 | 100000000 | 1 |

All five quote in USDC (6 decimals).

## Overrides are bookkeeping

The deposit totals and an owner's free balances record what the market's vaults owe. Overriding
them moves no tokens: `settle_funds` transfers out of the vaults, and fails if they hold less than
the override claims. To give an owner a balance they can withdraw, also raise the vault's token
balance (for example with `spl-token-account-balance`) and the market's deposit total.

Locked lots must agree with the owner's resting orders. Overriding them alone desynchronises the
account from the book.

# Recipes

## Expire a market

Set `time_expiry` to `-1`, or to a unix time a few minutes ahead of the surfnet clock. Once the
Clock passes it, deposits and new orders (maker and taker) are refused, and the market can only be
pruned, settled and closed. This is what a frontend or keeper has to handle when a market winds down.

## Change the fee schedule

Set `taker_fee` (always `>= 0`) and `maker_fee`. A negative `maker_fee` pays makers a rebate out
of the taker fee, so it must not exceed the taker fee in size.

# The bundled IDL

`idl.json` is the IDL the deployed program publishes on chain, in its Anchor IDL account
`6HsCgDXQq6JZ7VsgHzmB6hqpTh7ZZhqQRktDru9TXn9L` (the one [idl.solana.com](https://idl.solana.com)
serves for the program). That IDL is in the legacy (pre Anchor 0.30) format, which Surfpool cannot
read, so `idl.json` is it converted, with nothing added or removed:

```
anchor idl convert openbook_v2.json -p opnb2LAfJYbRMAHHvqjCwQxanZn7ReEHp1k81EohpZb -o idl.json
```

| | |
|---|---|
| Source | Anchor IDL account `6HsCgDXQq6JZ7VsgHzmB6hqpTh7ZZhqQRktDru9TXn9L`, read at slot 474646726 (2026-11-27) |
| Source SHA-256 | `d949b2bc4fc649ce4da4b2734f7625f02ac0497d08467556809059483fbf782f` (the decompressed JSON) |
| Converter | Anchor CLI 1.2.2 |
| `idl.json` SHA-256 | `2930b90ed8fad10ca09156f1b43d6c1ac82688c730d1666aa86c0262e79a4176` |

The legacy IDL has no program address or discriminators. `-p` sets the address, and the converter
derives each discriminator from the account's name, as Anchor does.

The IDL in OpenBook's repository is newer than the deployed program: it adds the `FillOrKill` order
type and the `WouldExecutePartially` error (6043) from
[openbook-v2#231](https://github.com/openbook-dex/openbook-v2/pull/231), which no program upgrade
has shipped. The `Market` and `OpenOrdersAccount` layouts are the same in both.

# Testing

`crates/core/src/tests/openbook/` fetches the preset markets and a live open orders account from
mainnet, checks that they round-trip through the bundled IDL unchanged, and checks that overrides
change only their target bytes. Its last test runs the deployed program: it takes an order on the
live SOL/USDC market, and refuses the same order as `MarketHasExpired` once `time_expiry` is
overridden to `-1`:

```
cargo test -p surfpool-core --features integration-tests openbook
```
