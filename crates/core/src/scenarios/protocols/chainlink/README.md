# Chainlink

Surfpool bundles the IDL of Chainlink's Solana Store program
(`HEvSKofvBgfaexv23kMabbYqxasxU3mQ4ibBMEmJWHny`) and one template, `chainlink-data-feed`, so a
scenario can move the latest round of any Chainlink data feed: its answer, slot and timestamp.
Programs that read the feed then see that round. The template only prepares state.

Chainlink Data Streams has no counterpart here. Its reports are verified inside the consuming
transaction and stored, if at all, in the consumer's own accounts (Kamino's Scope prices, for
example), so those accounts are overridden through the consumer's templates.

## The bundled IDL

A feed account is the `Transmissions` header followed by its rounds. Chainlink's published IDL
describes only the header, so the answer is out of its reach. Chainlink's
Solana SDK, `chainlink_solana` 2.0.8 (`v2::read_feed_v2`), reads a feed as that header padded to
192 bytes plus exactly one 48-byte `Transmission`, and rejects any feed whose `live_length` is
not 1. The bundled `v1/idl.json` describes the same layout:

1. The program's on-chain IDL account `7FQupg5eV1vLDs7eFcSGDFDgskKUCb9hEGv1JbVA4BPo` (legacy
   format, sha256 `bf4dbb4df4245331997cf00c0f4f7fc40ef13a3f4d14dfcc7ab8587f7c6e20af`), converted
   with `anchor idl convert --program-id HEvSKofvBgfaexv23kMabbYqxasxU3mQ4ibBMEmJWHny` (Anchor CLI
   0.31.1, output sha256 `e62c5f885b8fa219008a378f7d778acfb95e416fed0dbe7825143ed70b91ec90`).
2. Two additions taken from the SDK: the `Transmission` type, and two fields at the end of
   `Transmissions`, `_header_padding` (40 bytes, up to `HEADER_SIZE` = 192) and `live`.

Nothing else differs from the converter's output.

## Tests

`registry.rs` pins the round's offsets without a network. The live tests read the SOL / USD feed:
one checks that only the round's bytes change, the other runs the deployed Store program's
`query` and checks that it returns the round the template wrote:

```
cargo test -p surfpool-core --features integration-tests chainlink
```
