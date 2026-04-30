# `surfpool-sdk`

Node.js bindings for the Surfpool SDK, built with `napi-rs`.

## Usage

```ts
import { Surfnet } from "surfpool-sdk";

const surfnet = Surfnet.start();
console.log(surfnet.rpcUrl); // http://127.0.0.1:xxxxx

// ... run tests / interact with the local validator ...

// Graceful shutdown: closes HTTP + WebSocket RPC servers and frees ports.
surfnet.stop();
```

`stop()` is idempotent and synchronous; it blocks briefly while servers close.
Wire it into test teardown (e.g. `afterAll`) to avoid `connection reset` /
`broken pipe` warnings caused by the OS yanking sockets at process exit.

## Development

```bash
npm ci
npm run build
npm test
```

## Publishing

The npm package is released from `crates/sdk-node` using prebuilt native artifacts for:

- `x86_64-apple-darwin`
- `aarch64-apple-darwin`
- `x86_64-unknown-linux-gnu`

The GitHub Actions release workflow builds those artifacts first, assembles the per-platform npm package directories, and then publishes each package with npm trusted publishing over GitHub OIDC.
