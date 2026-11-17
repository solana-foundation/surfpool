# Surfpool Scenarios
This module contains pre-defined scenarios that can be executed within the Surfpool environment. 
These scenarios allow you to create a time-sequence of account states.
As the scenario is executed, each step, which is associated with a subsequent slot in the surfnet, will override the surfnet accounts db with the account states defined in that step.
This powerful tool allows for testing and simulation of various account states over time, enabling developers to observe how different scenarios will impact their protocol.


## Scenario Components
There are a few components that allow a scenario to function.

### Program IDL
For Surfpool to understand how to serialize and deserialize account data, it needs to have access to the program's IDL. 
Protocols that are natively supported by Surfpool will have their IDLs included by default.

**Currently supported protocols:**
- **Pyth v2** - Price oracle with 4 price feed templates (SOL/USD, BTC/USD, ETH/BTC, ETH/USD)
- **Jupiter v6** - DEX aggregator with TokenLedger manipulation template
- **Switchboard On-Demand** - On-demand oracle with QuoteAccount override template
- **Kamino** – Lending (v1.23.0), Scope oracle, Farms, Swap/LIMO, Earn vaults and Liquidity, across six programs. See [protocols/kamino/README.md](./protocols/kamino/README.md)
- **Drift v2** - Perp and spot markets, user state, and global state
- **Pump v1** - Bonding curve launchpad with curve reserve and global config override templates
- **Phoenix Eternal** - Perpetuals venue with trader collateral, mark price, maintenance margin, market fee, withdraw limit, trader capability, stop-loss trigger and delegated permission templates. See [protocols/phoenix-eternal/README.md](./protocols/phoenix-eternal/README.md)
- **PumpSwap v1** - Constant-product AMM with pool state and global config override templates, including canonical pool derivation for migrated pump.fun coins

For custom protocols, an IDL can be registered at runtime using the [`surfnet_registerIdl`](https://docs.surfpool.run/rpc/cheatcodes#surfnet-registeridl) RPC cheatcode.

### Scenario Registration
Scenarios can be registered at runtime using the [`surfnet_registerScenario`](https://docs.surfpool.run/rpc/cheatcodes#surfnet-registerscenario) RPC cheatcode. 
This cheatcode takes in a scenario definition in JSON format, which includes the scenario name, description, and a list of overrides to apply to accounts.
Each override contains a map of the field in the account to override (as indexed in the IDL), and the value to apply for that key.

Field keys use dot notation. Segments address struct fields by name and array elements by
zero-based index, so `liquidity.total_available_amount`, `deposits.0.deposited_amount` and
`config.borrow_rate_curve.points.3.borrow_rate_bps` are all valid. Supplying a composite value (a
whole struct or array) also works, but it must be **complete** - every field of every element,
padding included - because the account is re-encoded with Borsh. An out-of-range index or a
non-numeric segment on an array is a hard error, never a silent write elsewhere.

An override is applied once. Its resulting account state remains in later slots until a transaction
or another override writes that account again.

### Scripts: scenarios that react

An override is fixed when its scenario is registered. A scenario can also carry `scripts`: Lua
5.4 source whose `on_slot(ctx)` function runs once per slot, after the slot's block is confirmed
and before the slot's overrides are applied. A script reads the chain as the block left it and
queues overrides or account writes, so a scenario can answer what transactions did: a price that
moves with a taker's flow, depth that comes back at its own pace.

```json
{
  "id": "impact", "name": "Taker impact", "description": "", "overrides": [], "tags": [],
  "scripts": [{
    "id": "impact",
    "name": "impact",
    "source": "function on_slot(ctx) ... end",
    "activeFrom": 1,
    "activeUntil": 400
  }]
}
```

```lua
local TRADER, SYMBOL, TICKS_PER_LOT = "<trader account>", "SOL", 0.036
base, seen, impact = nil, 0, 0          -- globals persist between slots

function on_slot(ctx)
  base = base or ctx.phoenix.bbo(SYMBOL).index_ticks
  local position = ctx.phoenix.position(TRADER, SYMBOL)
  local flow = position - seen
  seen = position
  if flow == 0 then return end
  impact = impact + TICKS_PER_LOT * flow
  ctx.override {
    template = "phoenix-market-move",
    values = { symbol = SYMBOL, target_ticks = tostring(math.floor(base + impact + 0.5)) },
  }
  ctx.override { template = "phoenix-market-uncross", values = { symbol = SYMBOL } }
end
```

- **Interval.** `activeFrom` (default 1), `activeUntil` (default: never) and `every` (default 1)
  are slots counted from the scenario's base slot, as `scenarioRelativeSlot` is. Past its
  interval a script is removed. Register with the clock paused, or give the base `slot`, when the
  first slot matters: a slot that passes between reading the base and registering is one the
  script was not there for.
- **`ctx`.** `slot`, `elapsed`, `account(pubkey)`, `override{ template, values, account?, label?,
  fetch? }`, `set_account(pubkey, { lamports?, data?, owner? })`, `random()`, `log(message)`, and
  `phoenix.markets()`, `phoenix.bbo(symbol)`, `phoenix.position(trader, symbol)`,
  `phoenix.quote(symbol, side, lots, trader)`. The table in `script.rs` describes each.
- **Order.** Scripts due at a slot run in the order they were registered. An account write lands
  when its script returns; queued overrides apply after the slot's scheduled ones.
- **Writes.** A second `set_account` on an account in one call patches the first: `lamports`,
  then `data`, writes both. A script's write reaches account subscribers and Geyser plugins as a
  scheduled override's does.
- **Limits.** No `os`, `io`, `require` or `load`; `ctx.random()` is seeded; `pairs` visits keys in
  sorted order, so a session replays the same. A call past its instruction limit (10 million by
  default) or a VM past 64 MiB is stopped and applies nothing; 20 failed calls in a row disable
  the script. A call queues at most 1,024 writes and overrides, and 64 MiB of logs, account data
  and override values: one more is refused, an error the script can catch.
- **All or nothing.** A scenario is refused whole: when a script does not load or an override's
  slot overflows, nothing of it is registered, and scripts already running under the same ids
  go on with their state.
- **Not kept.** Scripts are not part of a snapshot, a network reset drops them, and a copy of a
  surfnet (a profiling sandbox) runs none.

### Kamino integration tests

Byte-level Kamino coverage lives in `crates/core/src/tests/kamino/`. Those tests fetch the real
accounts from mainnet, so they need a network connection and are compiled only behind a feature:

```
cargo test -p surfpool-core --features integration-tests kamino
```

Set `SURFPOOL_TEST_RPC_URL` to use a private endpoint instead of the public one. The default test
run needs no network.

### Programs with no IDL

Programs that publish no usable IDL can describe their account bytes directly in an override
collection:

```yaml
raw_layout: true

templates:
  - id: example-market-price
    properties:
      - path: price
        offset: 32
        encoding: u128
```

Raw-layout overrides use the same scenario API as IDL-backed templates. Values are encoded as
little-endian integers and can be written once or repeatedly at a fixed stride. Every write is
bounds-checked against the account data available at runtime, while the selected account is the
caller's responsibility. This permits compatible accounts with additional trailing data and layouts
that remain valid across program upgrades. Large integers that exceed `u64` should be supplied as
decimal strings so JSON parsing cannot lose precision.

Each collection uses exactly one write model: an IDL-backed collection cannot set
`raw_layout: true`, and an IDL-less collection must set it. Raw-layout collections are currently
compiled into Surfpool's built-in template registry; there is no runtime raw-layout registration
endpoint.

### Override Templates
Directly using the `surfnet_registerScenario` endpoint requires building out a map of account keys that are specific to the schema of the account that is being written to.
This is a cumbersome process in most cases.
Override templates that are registered in the Surfpool repo will automatically be available in Surfpool Studio's drag-and-drop UI for creating scenarios.
This UI will automatically create the `surfnet_registerScenario` payload for you, eliminating the complexity from the process.

## Creating Native Scenario Support for a Protocol
Having a protocol being natively supported by Surfpool Scenarios will allow users to easily create scenarios to override account states for that protocol.
All accounts in a surfnet's account db that are owned by the protocol will include automatic IDL-parsing, and the protocol's account overrides will be available in Surfpool Studio's drag-and-drop UI.

The following steps can be followed to natively support a protocol:
 1. Create a folder for the protocol in the `crates/core/src/scenarios/protocols` folder. For example, `crates/core/src/scenarios/protocols/pyth/v2`.
 2. Add a file called `idl.json` containing the protocol's anchor IDL to the folder. For example, see [Pyth's IDL](./protocols/pyth/v2/idl.json) or [Jupiter's IDL](./protocols/jupiter/v6/idl.json)
 3. Add a file called `overrides.yaml` to the folder. For example, see [Pyth's Override File](./protocols/pyth/v2/overrides.yaml) or [Jupiter's Override File](./protocols/jupiter/v6/overrides.yaml).
    1. This file is what will populate Surfpool Studio's UI with the protocol details. The Pyth override file linked above produces the following in the UI:
   ![Pyth Overrides](../../../..//doc/assets/pyth-overrides.png)
 4. Update the [registry.rs](./registry.rs) file. A small amount of code has to be written to wire together the template registry, the `overrides.yaml`, and the `idl.json`. See the `load_pyth_overrides` and `load_jupiter_overrides` functions for examples.

If any part of these instructions are beyond your skill level or availability, but you'd like to see a specific protocol supported, feel free to [Open an Issue to Support a new Protocol](https://github.com/solana-foundation/surfpool/issues/new?template=native-scenario-support-for-protocol.md)!
Opening an issue to signal interest is a big help.
If you're able to find an IDL for the protocol, even better!
