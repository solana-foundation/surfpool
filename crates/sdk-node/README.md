# `@solana/surfpool`

Node.js bindings for the Surfpool SDK, built with `napi-rs`.

## Usage

```ts
import { Surfnet } from "@solana/surfpool";

const surfnet = Surfnet.start();
console.log(surfnet.rpcUrl); // http://127.0.0.1:xxxxx

// ... run tests / interact with the local validator ...

// Graceful shutdown: closes HTTP + WebSocket RPC servers and frees ports.
surfnet.stop();
```

`stop()` is idempotent and synchronous; it blocks briefly while servers close.
Wire it into test teardown (e.g. `afterAll`) to avoid `connection reset` /
`broken pipe` warnings caused by the OS yanking sockets at process exit.

## Kit plugin (`@solana/surfpool/kit`)

The `@solana/surfpool/kit` entry provides an [`@solana/kit`](https://github.com/anza-xyz/kit)
plugin that is a drop-in replacement for `solanaLocalRpc()` or `litesvm()`,
plus a typed RPC client for every `surfnet_*` cheatcode.

The kit dependencies are optional peers — install them alongside this package:

```bash
npm install --save-dev @solana/surfpool @solana/kit @solana/kit-plugin-rpc @solana/kit-plugin-signer
```

**Embedded mode** boots an in-process Surfnet, installs a pre-funded `payer`,
the full local RPC stack (`rpc`, `rpcSubscriptions`, `airdrop`,
`sendTransactions`, …), the native handle as `client.surfnet`, and a typed
cheatcodes RPC as `client.cheatcodes`:

```ts
import { createClient } from "@solana/kit";
import { surfpool } from "@solana/surfpool/kit";

const client = await createClient().use(surfpool());

await client.cheatcodes.timeTravel({ absoluteSlot: 1_000_000 }).send();
client.surfnet.fundSol(client.payer.address, 1_000_000_000);
const slot = await client.rpc.getSlot().send();

client.surfnet.stop();
```

The embedded client is `Disposable`: disposing it stops the Surfnet, so you can
use `using` instead of calling `stop()` yourself and a recreated client boots a
fresh instance.

```ts
using client = await createClient().use(surfpool());
// Surfnet stops automatically when `client` goes out of scope.
```

Surfnet startup options go under the `surfnet` key; everything else is
forwarded to the standard local RPC plugin:

```ts
const client = await createClient().use(
  surfpool({ surfnet: { offline: true }, skipPreflight: true }),
);
```

**Attach mode** connects to an already-running Surfpool (e.g. `surfpool start`)
instead of booting one — no native module is loaded and the client must
already have a `payer`:

```ts
import { createClient } from "@solana/kit";
import { payer } from "@solana/kit-plugin-signer";
import { surfpool } from "@solana/surfpool/kit";

const client = await createClient()
  .use(payer(myPayer))
  .use(surfpool({ rpcUrl: "http://127.0.0.1:8899" }));
```

That payer is usually unfunded on the running Surfnet. `airdropAddresses`
credits each listed address or signer while the client is composed, so no
separate cheatcode call is needed before sending a transaction:

```ts
const client = await createClient()
  .use(payer(myPayer))
  .use(
    surfpool({
      airdropAddresses: [myPayer, someRecipient],
      airdropAmount: 5_000_000_000n, // lamports, defaults to 10 SOL
      rpcUrl: "http://127.0.0.1:8899",
    }),
  );
```

Funding is additive, like a real airdrop: `airdropAmount` is added to whatever
the address already holds, and only the lamport balance is written, so existing
account data and owner survive. A failure to fund throws, naming the address.
The option works in embedded mode too, alongside the pre-funded payer.

For one-off use without a client, `createSurfnetCheatcodesRpc(url)` returns a
standalone `Rpc<SurfnetCheatcodesApi>`, and `surfnetCheatcodes()` installs
`client.cheatcodes` on any existing client.

Note on integers: cheatcode responses arrive with `bigint` integers (the
cheatcodes transport parses all JSON integers as `bigint` so u64 values like
`rentEpoch` survive above 2^53); request payloads accept `number | bigint`.

## Development

```bash
npm ci
npm run build
npm test
```

The TypeScript types for the `surfnet_*` cheatcodes under
`surfpool-sdk/kit/generated/` are generated from the Rust wire types in
`crates/types` (ts-rs, behind the `ts-bindings` feature) and committed. After
changing those Rust types, refresh them with:

```bash
npm run generate:kit-types
```

CI regenerates the bindings and fails on any diff.

## Publishing

The npm package is released from `crates/sdk-node` using prebuilt native artifacts for:

- `x86_64-apple-darwin`
- `aarch64-apple-darwin`
- `x86_64-unknown-linux-gnu`

The GitHub Actions release workflow builds those artifacts first, assembles the per-platform npm package directories, and then publishes each package with npm trusted publishing over GitHub OIDC.
