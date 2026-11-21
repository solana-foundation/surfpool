## How to use

After compiling the `hello-geyser` plugin:
```console
cd examples/hello-geyser
cargo build --release
cd ../..
```

On Linux, set `libpath` in `examples/hello-geyser/hello-geyser.json` to
`target/release/libhello_geyser.so`. The included `.dylib` path is for macOS.

Start Surfpool with the plugin:
```console
surfpool start -g examples/hello-geyser/hello-geyser.json
```

## Generating account updates in a test

With the plugin running, use a signed transfer to exercise the account callback.
Run this in a second terminal with the Solana CLI installed. Both keypairs are
temporary and all RPC calls target your local Surfnet.

```sh
wallets=$(mktemp -d)
solana-keygen new --silent --no-bip39-passphrase --outfile "$wallets/payer.json"
solana-keygen new --silent --no-bip39-passphrase --outfile "$wallets/recipient.json"
recipient=$(solana-keygen pubkey "$wallets/recipient.json")
rpc=http://127.0.0.1:8899

solana --url "$rpc" --keypair "$wallets/payer.json" airdrop 2
solana --url "$rpc" --keypair "$wallets/payer.json" transfer "$recipient" 1 --allow-unfunded-recipient
```

The airdrop funds the payer and emits a synthetic *transaction* notification.
It does not emit a Geyser *account* update for the payer. The transfer does:
look for `lamports: 1000000000` in the plugin output for the newly funded
recipient. There will also be an update for the payer.

Similarly, `surfnet_setAccount` and `surfnet_setTokenAccount` can prepare test
state without emitting a Geyser account update for the changed account. Submit
a transaction after setting up that state when testing an account stream.
