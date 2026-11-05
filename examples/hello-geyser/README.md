## How to use

After commpiling the `hello-geyser` plugin:
```console
cd examples/hello-geyser
cargo build --release
cd ../..
```

The plugin can be attached using the command:
```console
surfpool start -g examples/hello-geyser/hello-geyser.json
```

## Cheatcodes and Geyser notifications

Cheatcodes can change Surfnet state without producing the same Geyser events as
a transaction. The current behavior is:

| Operation | Geyser notification |
| --- | --- |
| `requestAirdrop` | Emits a synthetic transaction notification. The plugin must enable transaction notifications to receive it. |
| `surfnet_setAccount` | Updates the account in Surfnet, but does not emit a Geyser account update. |
| `surfnet_setTokenAccount` | Updates the token account in Surfnet, but does not emit a Geyser account update. |

The example plugin currently enables account notifications and disables
transaction notifications. As configured, it will not print the airdrop's
synthetic transaction callback. Enabling transaction notifications lets the
plugin observe that callback, but account-setting cheatcodes still do not
produce account update notifications. To exercise Geyser account updates, send
a transaction that changes the account.
