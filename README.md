<div align="center">
  <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/solana-foundation/surfpool/main/doc/assets/surfpool-github-hero-dark.png">
      <source media="(prefers-color-scheme: light)" srcset="https://raw.githubusercontent.com/solana-foundation/surfpool/main/doc/assets/surfpool-github-hero-light.png">
      <img alt="Surfpool is the best place to train before surfing Solana" style="max-width: 60%;">
  </picture>
</div>

## 🌊 Overview

Surfpool is your drop-in replacement for `solana-test-validator`, designed for builders who want to work with real mainnet state — without downloading the entire chain.

But Surfpool goes further: it introduces Infrastructure as Code for Solana, empowering developers to define, deploy, and operate both on-chain and off-chain infrastructure declaratively and reproducibly.

It’s built local-first and offline-ready, so you can spin up networks on your laptop — and then promote the exact same setup to the cloud when it’s showtime.

## Surfpool in action: 101 Series

<a href="https://www.youtube.com/playlist?list=PL0FMgRjJMRzO1FdunpMS-aUS4GNkgyr3T">
  <picture>
    <source srcset="https://raw.githubusercontent.com/solana-foundation/surfpool/main/doc/assets/youtube.png">
    <img alt="Surfpool 101 series" style="max-width: 100%;">
  </picture>
</a>

## 💡 Key Features

### 🪄 Drop-in replacement for solana-test-validator

Spin up local networks that mirror mainnet state instantly — no 2 TB snapshots, no heavy setup (yes, even runs on a Raspberry Pi).
Surfpool has been battle-tested by hundreds of developers with existing Solana tools — including `solana-cli`, `anchor`, and `Kit` — so you can plug it into your workflow without changing a thing.

### 🧩 IDL-to-SQL

Transform your on-chain IDL into a fully queryable SQL schema.
Surfpool’s IDL-to-SQL engine bridges programs and databases — automatically generating tables and syncing chain data to local SQLite/Postgres for instant indexing and analytics.

### 🛡️ Infrastructure as Code (IaC) for Web3

Define your stack once — then deploy and tweak it thousands of times before mainnet, with minimal friction.
Inspired by Terraform, Surfpool’s IaC makes your setup reproducible by design:
your local environment is optimized for speed and feedback, while production is optimized for safety and scales gracefully.

### 🎮 Cheatcodes for Builders

Simulate, debug, and replay transactions — all without touching mainnet.
Includes Stream Oracles, Universal Faucet, Transaction Inspector, and Time Travel for fast, fearless experimentation.

### ☁️ Surfpool Studio → Surfpool Cloud

Surfpool Studio is your local dashboard to visualize, inspect, and manage your networks in real time.
Surfpool Cloud extends that same experience to the cloud — letting you index mainnet data and run large-scale simulations with the same developer experience. It’s serverless, backend-as-a-service, and built for analytics at scale.

### 🧪 Surfpool Scenarios

With Surfpool Scenarios you can curate slot-by-slot account states for key accounts, mixing live mainnet data with overridden account states.
This allows you to stress test your protocol in key real-world situations and to reproduce any chain-state conditions. See the [Scenarios Docs](./crates/core/src/scenarios/README.md) for more details.

## Installation

Surfpool installer:

```console
curl -sL https://run.surfpool.run/ | bash
```

Install from source:

```console
# Clone repo
git clone https://github.com/solana-foundation/surfpool.git

# Set repo as current directory
cd surfpool

# Build
cargo surfpool-install
```

Surfpool can also be used through our public [docker image](https://hub.docker.com/r/surfpool/surfpool):

```console
docker run surfpool/surfpool --version
```

To run Surfpool from Docker with the default RPC, WebSocket, and Studio ports exposed on your host:

```console
docker run --rm -p 8899:8899 -p 8900:8900 -p 18488:18488 surfpool/surfpool
```

The image binds to `0.0.0.0` inside the container, but advertises `127.0.0.1` by default so local Docker workflows continue to get usable URLs. When the container needs to be reachable from another machine, override `SURFPOOL_PUBLIC_HOST`:

```console
docker run --rm \
  -p 8899:8899 -p 8900:8900 -p 18488:18488 \
  -e SURFPOOL_PUBLIC_HOST=staging.example.com \
  surfpool/surfpool
```

For fully custom externally visible URLs, such as TLS termination or remapped ports, override `SURFPOOL_PUBLIC_RPC_URL`, `SURFPOOL_PUBLIC_WS_URL`, and `SURFPOOL_PUBLIC_STUDIO_URL`.

Verify installation:

```console
surfpool --version
```

## Usage

Start a local Solana network with:

```console
surfpool start
```

If inside an Anchor project, Surfpool will:

- Automatically generate infrastructure as code (similar to Terraform).

- Deploy your Solana programs to the local network.

- Provide a clean, structured environment to iterate safely.

The command:

```console
surfpool start --help
```

Is documenting all the options available.

## Crypto Infrastructure as Code: A New Standard in Web3

Infrastructure as code (IaC) transforms how teams deploy and operate Solana programs:

- Declarative & Reproducible – Clearly defines environments, making deployments consistent.

- Auditable – Security teams can review not just the code of your Solana programs, but the way you will be deploying and operating your protocol.

- Seamless Transition to Mainnet – Test with the exact infrastructure that will go live.

With Surfpool, every developer learns to deploy Solana programs the right way—scalable, secure, and production-ready from day one.

## 🤖 MCP

- Surfpool is getting agentic friendly, thanks to a built-in MCP. We'll be adding more tools over time, the first use case we're covering is "Start a local network with 10 users loaded with SOL, USDC, JUP and TRUMP tokens" (#130 - @BretasArthur1, @lgalabru)
- To get started, make `surfpool` available globally by opening the command palette (Cmd/Ctrl + Shift + P) and selecting > Cursor Settings > MCP > Add new global MCP server:

```json
{
  "mcpServers": {
    "surfpool": {
      "command": "surfpool",
      "args": ["mcp"]
    }
  }
}
```

## Architecture & How to Contribute

Surfpool is built on the low-level solana-svm API, utilizing the excellent LiteSVM wrapper. This approach provides greater flexibility and significantly faster boot times, ensuring a smooth developer experience.

We are actively developing Surfpool and welcome contributions from the community. If you'd like to get involved, here’s how:

- Join the [DevEx Telegram channel](https://t.me/SolanaDevEx)

- Explore and contribute to open issues: [GitHub Issues](https://github.com/solana-foundation/surfpool/issues?q=is%3Aissue%20state%3Aopen%20label%3A%22help%20wanted%22)
- Get releases updates via [X](https://x.com/txtx_sol) or [Telegram Channel](https://t.me/surfpool)

Your contributions help shape the future of Surfpool, making it an essential tool for Solana developers worldwide.
