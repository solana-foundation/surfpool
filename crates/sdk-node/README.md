# `@solana/surfpool`

Node.js bindings for the Surfpool SDK, built with `napi-rs`.

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
