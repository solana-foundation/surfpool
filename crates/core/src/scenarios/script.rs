//! Scenario scripts: scenarios that react.
//!
//! A [`surfpool_types::Scenario`]'s overrides are fixed when it is registered. Its `scripts` are
//! not: each is Lua source with an `on_slot(ctx)` function, called once per slot after the slot's
//! block is confirmed and before the slot's scheduled overrides are applied. It reads the chain as
//! the block left it and returns nothing: it acts through `ctx`. Scripts are registered with
//! their scenario, through `surfnet_registerScenario`.
//!
//! ```lua
//! -- Globals persist between slots: this is the script's state.
//! seen = seen or 0
//!
//! function on_slot(ctx)
//!   local wallet = ctx.account("9xQe...")           -- nil, or { lamports, owner, data, executable }
//!   if wallet and wallet.lamports ~= seen then
//!     seen = wallet.lamports
//!     ctx.override {                                 -- any override template, applied this slot
//!       template = "phoenix-market-move",
//!       values = { symbol = "SOL", target_ticks = "1234" },
//!     }
//!   end
//! end
//! ```
//!
//! `ctx` holds:
//!
//! | Field | What |
//! | --- | --- |
//! | `ctx.slot`, `ctx.elapsed` | The slot that just opened, and slots since the script's base slot |
//! | `ctx.account(pubkey)` | A local account, `data` as a Lua string (read it with `string.unpack`) |
//! | `ctx.override{ template, values, account?, label?, fetch? }` | Queue an override for this slot |
//! | `ctx.set_account(pubkey, { lamports?, data?, owner? })` | Write an account's fields |
//! | `ctx.random()` | A float in `[0, 1)` from the script's seed |
//! | `ctx.log(message)` | A line in surfnet's log |
//! | `ctx.phoenix.markets()` | Phoenix Eternal markets: `symbol`, `mark_ticks`, `tick_size`, `base_lot_decimals` |
//! | `ctx.phoenix.bbo(symbol)` | `{ bid_ticks, ask_ticks, mark_ticks, index_ticks }` |
//! | `ctx.phoenix.position(trader, symbol)` | The trader's signed base lots in the market |
//! | `ctx.phoenix.quote(symbol, "buy" or "sell", lots, trader)` | What a market order of `lots` would fill for now, nothing traded: `{ filled, filled_lots, quote_lots, avg_ticks }`. `trader` is any trader account |
//!
//! A script is active over an optional interval, in slots counted from its scenario's base slot
//! as the overrides' are: from `active_from` (default 1) up to but not
//! including `active_until` (default: until unregistered), every `every` slots (default 1). Past
//! its interval it is removed. A script registered with an id in use replaces that script.
//!
//! Scripts due at a slot run in the order they were registered. An account write lands as soon
//! as its script returns, so the next script reads it; queued overrides join the slot's scheduled
//! ones, after them, and are applied once every script has run.
//!
//! The VM has `table`, `string`, `math` and `utf8` only: no `os`, `io`, `require`, `load`, and no
//! `math.random`. So that a session replays the same, `pairs` visits keys in sorted order
//! (numbers, then strings; other key types are an error) and `next` is gone: Lua's own order
//! changes from run to run. A call that runs past the script's instruction limit, or a VM past
//! [`MEMORY_LIMIT_BYTES`], is stopped, and what the call queued is dropped. A script whose calls
//! fail [`MAX_CONSECUTIVE_FAILURES`] times in a row is disabled.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use mlua::{HookTriggers, Lua, LuaOptions, LuaSerdeExt, StdLib, Table, Value, VmState};
use solana_account::Account;
use solana_clock::Slot;
use solana_pubkey::Pubkey;
use surfpool_types::{AccountAddress, OverrideInstance, ScenarioScript};

use crate::{
    error::{SurfpoolError, SurfpoolResult},
    scenarios::{
        protocols::phoenix_eternal::v1::{
            market::phoenix_markets,
            state_builder::{Exchange, PHOENIX_GLOBAL_CONFIG, Sandbox, local_account},
            trader::{AssetView, BboView, asset_view, bbo_view, order_quote},
        },
        template_registry,
    },
    surfnet::{surfnet_lite_svm::reject_managed_sysvar, svm::SurfnetSvm},
};

/// The instruction limit of a script that names none.
pub const DEFAULT_INSTRUCTION_LIMIT: u64 = 10_000_000;
/// How often the VM checks the limit, in instructions.
const HOOK_EVERY: u32 = 1_000;
/// The most memory one script's VM may hold.
pub const MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
/// A script that fails this many calls in a row is disabled.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 20;

/// What a script asked for in one slot.
pub enum Action {
    Override(Box<OverrideInstance>),
    SetAccount(Pubkey, Account),
}

/// One script's call for one slot.
pub struct ScriptRun {
    pub script_id: String,
    pub name: String,
    pub actions: Vec<Action>,
    pub logs: Vec<String>,
    pub error: Option<String>,
    /// Set when this failure disabled the script.
    pub disabled: bool,
}

struct Loaded {
    script: ScenarioScript,
    lua: Lua,
    registered_at: Slot,
    used: Arc<AtomicU64>,
    rng: u64,
    consecutive_failures: u32,
    disabled: bool,
}

/// The scripts registered on a surfnet.
#[derive(Default)]
pub struct ScenarioScripts {
    scripts: Vec<Loaded>,
}

/// A copy of a surfnet runs no scripts: a Lua VM's state cannot be copied, and two surfnets
/// running one script would each act on what the other never saw.
impl Clone for ScenarioScripts {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl ScenarioScripts {
    pub fn is_empty(&self) -> bool {
        self.scripts.is_empty()
    }

    pub fn clear(&mut self) {
        self.scripts.clear();
    }

    /// Loads `script` and replaces any script with its id. Fails when the source does not compile
    /// or defines no `on_slot` function.
    pub fn register(&mut self, script: ScenarioScript, slot: Slot) -> SurfpoolResult<()> {
        let limit = script
            .instruction_limit
            .unwrap_or(DEFAULT_INSTRUCTION_LIMIT);
        let used = Arc::new(AtomicU64::new(0));
        let lua = new_vm(limit, used.clone()).map_err(script_error)?;
        lua.load(&script.source)
            .set_name(format!("={}", script.name))
            .exec()
            .map_err(script_error)?;
        if !matches!(
            lua.globals()
                .get::<Value>("on_slot")
                .map_err(script_error)?,
            Value::Function(_)
        ) {
            return Err(SurfpoolError::internal(format!(
                "scenario script {} defines no on_slot function",
                script.name
            )));
        }
        self.scripts.retain(|loaded| loaded.script.id != script.id);
        self.scripts.push(Loaded {
            rng: script.seed.unwrap_or(0),
            script,
            lua,
            registered_at: slot,
            used,
            consecutive_failures: 0,
            disabled: false,
        });
        Ok(())
    }

    /// Removes the script with `id`. Returns whether there was one.
    pub fn unregister(&mut self, id: &str) -> bool {
        let before = self.scripts.len();
        self.scripts.retain(|loaded| loaded.script.id != id);
        self.scripts.len() != before
    }

    pub fn len(&self) -> usize {
        self.scripts.len()
    }

    /// Removes the scripts whose interval is over at `slot`.
    pub fn retire(&mut self, slot: Slot) {
        self.scripts.retain(|loaded| !loaded.expired(slot));
    }

    /// Calls `on_slot` of the script at `index`, in registration order, when it is due at `slot`.
    /// The caller applies the run before the next call, so each script reads what the ones
    /// before it wrote.
    pub fn run_at(&mut self, index: usize, svm: &SurfnetSvm, slot: Slot) -> Option<ScriptRun> {
        let loaded = self.scripts.get_mut(index)?;
        loaded.due(slot).then(|| loaded.run(svm, slot))
    }
}

impl Loaded {
    /// The script's first active slot.
    fn first_slot(&self) -> Slot {
        self.registered_at
            .saturating_add(self.script.active_from.unwrap_or(1))
    }

    /// Whether the script's interval is over at `slot`.
    fn expired(&self, slot: Slot) -> bool {
        self.script
            .active_until
            .is_some_and(|until| slot >= self.registered_at.saturating_add(until))
    }

    /// Whether the script is called at `slot`.
    fn due(&self, slot: Slot) -> bool {
        let first = self.first_slot();
        !self.disabled
            && slot >= first
            && !self.expired(slot)
            && (slot - first).is_multiple_of(self.script.every.unwrap_or(1).max(1))
    }

    fn run(&mut self, svm: &SurfnetSvm, slot: Slot) -> ScriptRun {
        self.used.store(0, Ordering::Relaxed);
        let actions = RefCell::new(Vec::new());
        let logs = RefCell::new(Vec::new());
        let rng = Cell::new(self.rng);
        let elapsed = slot.saturating_sub(self.registered_at);
        let result = call_on_slot(&self.lua, svm, slot, elapsed, &actions, &logs, &rng);
        self.rng = rng.get();
        let mut run = ScriptRun {
            script_id: self.script.id.clone(),
            name: self.script.name.clone(),
            actions: actions.into_inner(),
            logs: logs.into_inner(),
            error: None,
            disabled: false,
        };
        match result {
            Ok(()) => self.consecutive_failures = 0,
            Err(e) => {
                // A failed call applies nothing: half a slot's writes would leave the market in a
                // state the script never meant.
                run.actions.clear();
                run.error = Some(e.to_string());
                self.consecutive_failures += 1;
                if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    self.disabled = true;
                    run.disabled = true;
                }
            }
        }
        run
    }
}

fn script_error(e: mlua::Error) -> SurfpoolError {
    SurfpoolError::internal(format!("scenario script: {e}"))
}

fn lua_error(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::RuntimeError(e.to_string())
}

/// A VM with the pure libraries only and an instruction budget per call.
fn new_vm(limit: u64, used: Arc<AtomicU64>) -> mlua::Result<Lua> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
        LuaOptions::default(),
    )?;
    lua.set_memory_limit(MEMORY_LIMIT_BYTES)?;
    // Before `load` goes: the prelude is ours.
    lua.load(SORTED_PAIRS).set_name("=prelude").exec()?;
    let globals = lua.globals();
    for name in [
        "dofile",
        "loadfile",
        "load",
        "loadstring",
        "require",
        "collectgarbage",
        "print",
    ] {
        globals.set(name, Value::Nil)?;
    }
    let math: Table = globals.get("math")?;
    math.set("random", Value::Nil)?;
    math.set("randomseed", Value::Nil)?;
    let string: Table = globals.get("string")?;
    string.set("dump", Value::Nil)?;
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(HOOK_EVERY),
        move |_lua, _debug| {
            let total = used.fetch_add(u64::from(HOOK_EVERY), Ordering::Relaxed);
            if total >= limit {
                Err(lua_error(format!(
                    "the call ran past its limit of {limit} instructions"
                )))
            } else {
                Ok(VmState::Continue)
            }
        },
    );
    Ok(lua)
}

/// Lua visits a table's keys in an order that depends on a hash seed drawn at startup, so the
/// same script could act in a different order on a replay. This `pairs` sorts the keys first.
const SORTED_PAIRS: &str = r#"
local rawnext, sort, type, error = next, table.sort, type, error
local function before(a, b)
  local ta, tb = type(a), type(b)
  if ta ~= tb then return ta < tb end
  return a < b
end
function pairs(t)
  local keys, n = {}, 0
  for k in rawnext, t do
    local tk = type(k)
    if tk ~= "number" and tk ~= "string" then
      error("pairs: a " .. tk .. " key has no order that is the same on every run", 2)
    end
    n = n + 1
    keys[n] = k
  end
  sort(keys, before)
  local i = 0
  return function()
    i = i + 1
    local k = keys[i]
    if k ~= nil then return k, t[k] end
  end
end
next = nil
"#;

fn parse_pubkey(text: &str) -> mlua::Result<Pubkey> {
    Pubkey::from_str(text).map_err(|_| lua_error(format!("{text} is not a public key")))
}

/// A u64 as a Lua integer, or a float past `i64::MAX`.
fn lua_u64(value: u64) -> Value {
    match i64::try_from(value) {
        Ok(value) => Value::Integer(value),
        Err(_) => Value::Number(value as f64),
    }
}

fn splitmix64(state: &Cell<u64>) -> u64 {
    let mut z = state.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
    state.set(z);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn exchange(svm: &SurfnetSvm) -> mlua::Result<Exchange> {
    Exchange::from_config(&local_account(svm, &PHOENIX_GLOBAL_CONFIG).map_err(lua_error)?)
        .map_err(lua_error)
}

fn market(
    svm: &SurfnetSvm,
    exchange: &Exchange,
    symbol: &str,
) -> mlua::Result<super::protocols::phoenix_eternal::v1::market::Market> {
    let map = local_account(svm, &exchange.perp_asset_map).map_err(lua_error)?;
    phoenix_markets(exchange.perp_asset_map, &map)
        .map_err(lua_error)?
        .into_iter()
        .find(|market| market.symbol == symbol)
        .ok_or_else(|| lua_error(format!("no Phoenix market {symbol}")))
}

fn call_on_slot(
    lua: &Lua,
    svm: &SurfnetSvm,
    slot: Slot,
    elapsed: u64,
    actions: &RefCell<Vec<Action>>,
    logs: &RefCell<Vec<String>>,
    rng: &Cell<u64>,
) -> mlua::Result<()> {
    // Phoenix's views run on a copy of the VM. Copying it is most of what a read costs, and
    // views write nothing, so one copy serves every read of the call.
    let views: RefCell<Option<Sandbox>> = RefCell::new(None);
    let view = |instruction: &solana_instruction::Instruction| -> mlua::Result<Vec<u8>> {
        views
            .borrow_mut()
            .get_or_insert_with(|| Sandbox::new(svm))
            .view(instruction)
            .map_err(lua_error)
    };
    lua.scope(|scope| {
        let ctx = lua.create_table()?;
        ctx.set("slot", lua_u64(slot))?;
        ctx.set("elapsed", lua_u64(elapsed))?;

        ctx.set(
            "account",
            scope.create_function(|lua, pubkey: String| {
                let pubkey = parse_pubkey(&pubkey)?;
                let Some(account) = svm.get_account(&pubkey).map_err(lua_error)? else {
                    return Ok(Value::Nil);
                };
                let table = lua.create_table()?;
                table.set("lamports", lua_u64(account.lamports))?;
                table.set("owner", account.owner.to_string())?;
                table.set("data", lua.create_string(&account.data)?)?;
                table.set("executable", account.executable)?;
                Ok(Value::Table(table))
            })?,
        )?;

        ctx.set(
            "override",
            scope.create_function(|lua, spec: Table| {
                let template_id = spec
                    .get::<Option<String>>("template")?
                    .ok_or_else(|| lua_error("override: `template` is required"))?;
                // As a registered scenario's overrides: a template the registry does not hold
                // still applies, through the IDL of the account's owner.
                let carried = template_registry()
                    .get(&template_id)
                    .map(|template| &template.address);
                let account = match (spec.get::<Option<String>>("account")?, carried) {
                    (Some(pubkey), _) => AccountAddress::Pubkey(parse_pubkey(&pubkey)?.to_string()),
                    (None, Some(AccountAddress::Pubkey(pubkey))) if !pubkey.is_empty() => {
                        AccountAddress::Pubkey(pubkey.clone())
                    }
                    (None, _) => {
                        return Err(lua_error(format!(
                            "override: {template_id} carries no address, give `account`"
                        )));
                    }
                };
                let values: HashMap<String, serde_json::Value> =
                    match spec.get::<Option<Value>>("values")? {
                        Some(values) => lua.from_value(values)?,
                        None => HashMap::new(),
                    };
                let mut instance = OverrideInstance::new(template_id, 0, account);
                instance.values = values;
                instance.label = spec.get::<Option<String>>("label")?;
                instance.fetch_before_use = spec.get::<Option<bool>>("fetch")?.unwrap_or(false);
                actions
                    .borrow_mut()
                    .push(Action::Override(Box::new(instance)));
                Ok(())
            })?,
        )?;

        ctx.set(
            "set_account",
            scope.create_function(|_, (pubkey, fields): (String, Table)| {
                let pubkey = parse_pubkey(&pubkey)?;
                // Surfnet writes these itself every slot, as `surfnet_setAccount` refuses too.
                reject_managed_sysvar(&pubkey).map_err(lua_error)?;
                let mut account = svm
                    .get_account(&pubkey)
                    .map_err(lua_error)?
                    .unwrap_or_default();
                if let Some(lamports) = fields.get::<Option<u64>>("lamports")? {
                    account.lamports = lamports;
                }
                if let Some(data) = fields.get::<Option<mlua::String>>("data")? {
                    account.data = data.as_bytes().to_vec();
                }
                if let Some(owner) = fields.get::<Option<String>>("owner")? {
                    account.owner = parse_pubkey(&owner)?;
                }
                actions
                    .borrow_mut()
                    .push(Action::SetAccount(pubkey, account));
                Ok(())
            })?,
        )?;

        ctx.set(
            "random",
            scope.create_function(|_, ()| {
                // 53 bits: every value is an exact double in [0, 1).
                Ok((splitmix64(rng) >> 11) as f64 / (1u64 << 53) as f64)
            })?,
        )?;

        ctx.set(
            "log",
            scope.create_function(|lua, message: Value| {
                let text = match lua.coerce_string(message)? {
                    Some(text) => text.to_string_lossy(),
                    None => "nil".to_string(),
                };
                logs.borrow_mut().push(text);
                Ok(())
            })?,
        )?;

        let phoenix = lua.create_table()?;
        phoenix.set(
            "markets",
            scope.create_function(|lua, ()| {
                let exchange = exchange(svm)?;
                let map = local_account(svm, &exchange.perp_asset_map).map_err(lua_error)?;
                let markets = lua.create_table()?;
                for market in phoenix_markets(exchange.perp_asset_map, &map).map_err(lua_error)? {
                    let entry = lua.create_table()?;
                    entry.set("symbol", market.symbol)?;
                    entry.set("mark_ticks", lua_u64(market.mark_ticks))?;
                    entry.set("tick_size", lua_u64(market.tick_size))?;
                    entry.set("base_lot_decimals", market.base_lot_decimals)?;
                    entry.set("orderbook", market.orderbook.to_string())?;
                    markets.push(entry)?;
                }
                Ok(markets)
            })?,
        )?;
        phoenix.set(
            "bbo",
            scope.create_function(|lua, symbol: String| {
                let exchange = exchange(svm)?;
                let market = market(svm, &exchange, &symbol)?;
                let view = BboView::from_return_data(&view(&bbo_view(&exchange, &market))?)
                    .map_err(lua_error)?;
                let table = lua.create_table()?;
                table.set("bid_ticks", lua_u64(view.best_bid_ticks))?;
                table.set("ask_ticks", lua_u64(view.best_ask_ticks))?;
                table.set("mark_ticks", lua_u64(view.mark_price_ticks))?;
                table.set("index_ticks", lua_u64(view.index_price_ticks))?;
                Ok(table)
            })?,
        )?;
        phoenix.set(
            "position",
            scope.create_function(|_, (trader, symbol): (String, String)| {
                let trader = parse_pubkey(&trader)?;
                let exchange = exchange(svm)?;
                let market = market(svm, &exchange, &symbol)?;
                let data =
                    view(&asset_view(&exchange, &trader, market.asset_id).map_err(lua_error)?)?;
                Ok(AssetView::from_return_data(&data)
                    .map_err(lua_error)?
                    .base_lots)
            })?,
        )?;
        phoenix.set(
            "quote",
            scope.create_function(
                |lua, (symbol, side, lots, trader): (String, String, u64, String)| {
                    let buy = match side.as_str() {
                        "buy" => true,
                        "sell" => false,
                        other => {
                            return Err(lua_error(format!(
                                "quote: side is buy or sell, not {other}"
                            )));
                        }
                    };
                    let trader = parse_pubkey(&trader)?;
                    let exchange = exchange(svm)?;
                    let market = market(svm, &exchange, &symbol)?;
                    let mut guard = views.borrow_mut();
                    let sandbox = guard.get_or_insert_with(|| Sandbox::new(svm));
                    let quote = order_quote(sandbox, &exchange, &market, &trader, buy, lots)
                        .map_err(lua_error)?;
                    let table = lua.create_table()?;
                    table.set("filled_lots", lua_u64(quote.filled_base_lots))?;
                    table.set("quote_lots", lua_u64(quote.filled_quote_lots))?;
                    table.set(
                        "filled",
                        quote.outcome == 0 && lots > 0 && quote.filled_base_lots == lots,
                    )?;
                    // The average fill, in ticks, when anything filled.
                    if quote.filled_base_lots > 0 && market.tick_size > 0 {
                        table.set(
                            "avg_ticks",
                            quote.filled_quote_lots as f64
                                / quote.filled_base_lots as f64
                                / market.tick_size as f64,
                        )?;
                    }
                    Ok(table)
                },
            )?,
        )?;
        ctx.set("phoenix", phoenix)?;

        lua.globals()
            .get::<mlua::Function>("on_slot")?
            .call::<()>(ctx)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(source: &str) -> ScenarioScript {
        ScenarioScript {
            id: "test".into(),
            name: "test".into(),
            source: source.into(),
            seed: Some(7),
            instruction_limit: None,
            active_from: None,
            active_until: None,
            every: None,
        }
    }

    fn svm() -> SurfnetSvm {
        SurfnetSvm::default().0
    }

    impl ScenarioScripts {
        /// Every due script's run at `slot`, none of them applied.
        fn run_slot(&mut self, svm: &SurfnetSvm, slot: Slot) -> Vec<ScriptRun> {
            self.retire(slot);
            (0..self.len())
                .filter_map(|index| self.run_at(index, svm, slot))
                .collect()
        }
    }

    fn run(scripts: &mut ScenarioScripts, svm: &SurfnetSvm, slot: Slot) -> ScriptRun {
        scripts.run_slot(svm, slot).pop().expect("one script")
    }

    #[test]
    fn a_script_without_on_slot_is_refused() {
        let mut scripts = ScenarioScripts::default();
        assert!(scripts.register(script("x = 1"), 0).is_err());
        assert!(scripts.register(script("function on_slot("), 0).is_err());
        assert!(scripts.is_empty());
    }

    #[test]
    fn globals_persist_and_the_script_reads_what_the_chain_holds() {
        let mut svm = svm();
        let watched = Pubkey::new_unique();
        let mirror = Pubkey::new_unique();
        let source = format!(
            r#"
            calls = 0
            function on_slot(ctx)
              calls = calls + 1
              local account = ctx.account("{watched}")
              local lamports = account and account.lamports or 0
              ctx.set_account("{mirror}", {{ lamports = 2 * lamports + calls }})
              ctx.log("slot " .. ctx.slot .. " elapsed " .. ctx.elapsed)
            end
            "#
        );
        let mut scripts = ScenarioScripts::default();
        scripts.register(script(&source), 10).unwrap();

        let first = run(&mut scripts, &svm, 11);
        assert_eq!(first.error, None);
        assert_eq!(first.logs, vec!["slot 11 elapsed 1"]);
        assert!(matches!(
            &first.actions[..],
            [Action::SetAccount(pubkey, account)] if *pubkey == mirror && account.lamports == 1
        ));

        // The script reacts to a write made after it was registered.
        svm.set_account(
            &watched,
            Account {
                lamports: 500,
                ..Account::default()
            },
        )
        .unwrap();
        let second = run(&mut scripts, &svm, 12);
        assert!(matches!(
            &second.actions[..],
            [Action::SetAccount(_, account)] if account.lamports == 1_002
        ));
    }

    #[test]
    fn an_override_names_a_known_template_and_takes_its_address() {
        let mut scripts = ScenarioScripts::default();
        scripts
            .register(
                script(
                    r#"
                    function on_slot(ctx)
                      ctx.override {
                        template = "phoenix-market-move",
                        values = { symbol = "SOL", target_ticks = "1234" },
                        label = "impact",
                      }
                    end
                    "#,
                ),
                0,
            )
            .unwrap();
        let run_ = run(&mut scripts, &svm(), 1);
        assert_eq!(run_.error, None);
        let [Action::Override(instance)] = &run_.actions[..] else {
            panic!("one override");
        };
        assert_eq!(instance.template_id, "phoenix-market-move");
        assert_eq!(instance.values["target_ticks"], "1234");
        assert_eq!(instance.label.as_deref(), Some("impact"));
        assert!(matches!(&instance.account, AccountAddress::Pubkey(pubkey) if !pubkey.is_empty()));

        scripts
            .register(
                script(r#"function on_slot(ctx) ctx.override { template = "nope" } end"#),
                0,
            )
            .unwrap();
        let unknown = run(&mut scripts, &svm(), 1);
        assert!(unknown.error.unwrap().contains("nope carries no address"));
    }

    #[test]
    fn a_script_runs_over_its_interval_only() {
        let mut scripts = ScenarioScripts::default();
        let mut windowed = script("function on_slot(ctx) ctx.log(ctx.elapsed) end");
        windowed.active_from = Some(3);
        windowed.active_until = Some(9);
        windowed.every = Some(2);
        scripts.register(windowed, 100).unwrap();
        let svm = svm();
        let called: Vec<String> = (100..115)
            .flat_map(|slot| scripts.run_slot(&svm, slot))
            .flat_map(|run| run.logs)
            .collect();
        assert_eq!(called, vec!["3", "5", "7"]);
        // Past its interval the script is gone.
        assert!(scripts.is_empty());
    }

    #[test]
    fn scripts_register_with_their_scenario_or_not_at_all() {
        let mut svm = svm();
        let mut good = script("function on_slot(ctx) end");
        good.id = "good".into();
        let mut broken = script("function on_slot(");
        broken.id = "broken".into();

        let mut scenario = surfpool_types::Scenario::new("s".into(), "s".into());
        scenario.scripts = vec![good.clone(), broken];
        assert!(svm.register_scenario(scenario, None).is_err());
        assert!(svm.scenario_scripts.is_empty());

        let mut scenario = surfpool_types::Scenario::new("s".into(), "s".into());
        scenario.scripts = vec![good];
        svm.register_scenario(scenario, Some(40)).unwrap();
        assert_eq!(svm.scenario_scripts.scripts[0].registered_at, 40);

        // A scenario file without `scripts` still loads, and one without scripts writes none.
        let plain: surfpool_types::Scenario = serde_json::from_str(
            r#"{"id":"a","name":"a","description":"","overrides":[],"tags":[]}"#,
        )
        .unwrap();
        assert!(plain.scripts.is_empty());
        assert!(!serde_json::to_string(&plain).unwrap().contains("scripts"));
    }

    #[test]
    fn a_runaway_call_is_stopped_and_applies_nothing() {
        let mut scripts = ScenarioScripts::default();
        let mut runaway = script(
            r#"
            function on_slot(ctx)
              ctx.set_account("11111111111111111111111111111112", { lamports = 1 })
              while true do end
            end
            "#,
        );
        runaway.instruction_limit = Some(50_000);
        scripts.register(runaway, 0).unwrap();
        let svm = svm();
        let run_ = run(&mut scripts, &svm, 1);
        assert!(run_.error.unwrap().contains("limit of 50000 instructions"));
        assert!(run_.actions.is_empty());

        // The budget is per call, and repeated failures disable the script.
        for _ in 1..MAX_CONSECUTIVE_FAILURES - 1 {
            assert!(!run(&mut scripts, &svm, 1).disabled);
        }
        assert!(run(&mut scripts, &svm, 1).disabled);
        assert!(scripts.run_slot(&svm, 1).is_empty());
    }

    #[test]
    fn the_vm_has_no_way_out_and_its_randomness_is_seeded() {
        let source = r#"
            function on_slot(ctx)
              assert(os == nil and io == nil and require == nil and load == nil)
              assert(debug == nil and package == nil and dofile == nil)
              assert(math.random == nil and string.dump == nil)
              ctx.log(string.format("%.17g %.17g", ctx.random(), ctx.random()))
            end
        "#;
        let draws = |seed| {
            let mut scripts = ScenarioScripts::default();
            let mut script = script(source);
            script.seed = Some(seed);
            scripts.register(script, 0).unwrap();
            let svm = svm();
            let first = run(&mut scripts, &svm, 1);
            assert_eq!(first.error, None);
            let second = run(&mut scripts, &svm, 2);
            (first.logs, second.logs)
        };
        let (first, second) = draws(7);
        assert_ne!(first, second);
        assert_eq!(draws(7), (first.clone(), second));
        assert_ne!(draws(8).0, first);
    }

    // ---- The VM ----

    /// One script's logs over `slots` calls on a fresh surfnet.
    fn logs_of(source: &str, slots: std::ops::Range<Slot>) -> Vec<String> {
        let mut scripts = ScenarioScripts::default();
        scripts.register(script(source), 0).unwrap();
        let svm = svm();
        slots
            .flat_map(|slot| {
                let run = run(&mut scripts, &svm, slot);
                assert_eq!(run.error, None);
                run.logs
            })
            .collect()
    }

    /// One call's error.
    fn error_of(source: &str) -> String {
        let mut scripts = ScenarioScripts::default();
        scripts.register(script(source), 0).unwrap();
        let run = run(&mut scripts, &svm(), 1);
        assert!(run.actions.is_empty(), "a failed call applies nothing");
        run.error.expect("the call fails")
    }

    #[test]
    fn pairs_visits_keys_in_sorted_order_on_every_run() {
        let source = r#"
            function on_slot(ctx)
              local t = { zeta = 1, alpha = 2, [10] = "a", [2] = "b", mid = 3, [1.5] = "c" }
              for i = 1, 200 do t["k" .. i] = i end
              local seen = {}
              for k, v in pairs(t) do seen[#seen + 1] = tostring(k) end
              ctx.log(table.concat(seen, ","))
            end
        "#;
        let first = logs_of(source, 1..2);
        let keys: Vec<&str> = first[0].split(',').collect();
        assert_eq!(&keys[..4], ["1.5", "2", "10", "alpha"]);
        assert_eq!(keys.len(), 206);
        let strings = &keys[3..];
        assert!(strings.windows(2).all(|pair| pair[0] < pair[1]));
        // Each VM draws its own hash seed; the order does not follow it.
        for _ in 0..8 {
            assert_eq!(logs_of(source, 1..2), first);
        }
    }

    #[test]
    fn pairs_refuses_keys_without_an_order_and_next_is_gone() {
        assert!(
            error_of("function on_slot(ctx) for k in pairs({ [true] = 1 }) do end end")
                .contains("a boolean key has no order")
        );
        assert!(
            error_of("function on_slot(ctx) for k in pairs({ [{}] = 1 }) do end end")
                .contains("a table key has no order")
        );
        logs_of(
            r#"function on_slot(ctx)
                 assert(next == nil)
                 local n = 0
                 for i, v in ipairs({ "a", "b", "c" }) do n = n + i end
                 assert(n == 6)
                 for k, v in pairs({}) do error("empty") end
               end"#,
            1..2,
        );
    }

    #[test]
    fn a_vm_past_its_memory_limit_fails_the_call_and_keeps_running() {
        let mut scripts = ScenarioScripts::default();
        scripts
            .register(
                script(
                    r#"
                    calls = 0
                    function on_slot(ctx)
                      calls = calls + 1
                      ctx.set_account("11111111111111111111111111111112", { lamports = calls })
                      if calls == 2 then hog = string.rep("x", 200 * 1024 * 1024) end
                    end
                    "#,
                ),
                0,
            )
            .unwrap();
        let svm = svm();
        assert_eq!(run(&mut scripts, &svm, 1).error, None);
        let hog = run(&mut scripts, &svm, 2);
        assert!(
            hog.error.as_deref().unwrap().contains("memory"),
            "{:?}",
            hog.error
        );
        assert!(hog.actions.is_empty());
        // The VM survives: globals are as the failed call left them.
        let after = run(&mut scripts, &svm, 3);
        assert_eq!(after.error, None);
        assert!(matches!(
            &after.actions[..],
            [Action::SetAccount(_, account)] if account.lamports == 3
        ));
    }

    #[test]
    fn the_instruction_budget_is_per_call_and_per_script() {
        let busy =
            "function on_slot(ctx) local n = 0 for i = 1, 20000 do n = n + i end ctx.log(n) end";
        let mut scripts = ScenarioScripts::default();
        let mut tight = script(busy);
        tight.id = "tight".into();
        tight.instruction_limit = Some(10_000);
        let mut roomy = script(busy);
        roomy.id = "roomy".into();
        roomy.instruction_limit = Some(1_000_000);
        scripts.register(tight, 0).unwrap();
        scripts.register(roomy, 0).unwrap();
        let svm = svm();
        // Many calls each under the limit never add up to it.
        for slot in 1..30 {
            let runs = scripts.run_slot(&svm, slot);
            if slot < MAX_CONSECUTIVE_FAILURES as u64 {
                assert!(runs[0].error.as_deref().unwrap().contains("limit of 10000"));
            }
            let roomy = runs.last().unwrap();
            assert_eq!(roomy.error, None);
            assert_eq!(roomy.logs, vec!["200010000"]);
        }
    }

    #[test]
    fn a_failure_counts_only_while_it_repeats() {
        // Fails on every slot but each tenth: never 20 in a row.
        let mut scripts = ScenarioScripts::default();
        scripts
            .register(
                script(
                    r#"function on_slot(ctx)
                         if ctx.slot % 10 ~= 0 then error("boom at " .. ctx.slot) end
                         ctx.log("ok")
                       end"#,
                ),
                0,
            )
            .unwrap();
        let svm = svm();
        let mut ok = 0;
        for slot in 1..=100 {
            let run = run(&mut scripts, &svm, slot);
            assert!(!run.disabled);
            match run.error {
                None => ok += 1,
                Some(error) => assert!(error.contains(&format!("boom at {slot}")), "{error}"),
            }
        }
        assert_eq!(ok, 10);
    }

    #[test]
    fn registering_an_id_again_replaces_the_script_and_revives_a_disabled_one() {
        let mut scripts = ScenarioScripts::default();
        let svm = svm();
        scripts
            .register(
                script("n = 0 function on_slot(ctx) n = n + 1 ctx.log('old ' .. n) end"),
                0,
            )
            .unwrap();
        assert_eq!(run(&mut scripts, &svm, 1).logs, vec!["old 1"]);
        scripts
            .register(
                script("n = n or 100 function on_slot(ctx) n = n + 1 ctx.log('new ' .. n) end"),
                1,
            )
            .unwrap();
        assert_eq!(scripts.len(), 1);
        // A fresh VM: the old script's globals are gone.
        assert_eq!(run(&mut scripts, &svm, 2).logs, vec!["new 101"]);

        scripts
            .register(script("function on_slot(ctx) error('always') end"), 2)
            .unwrap();
        for slot in 3..3 + MAX_CONSECUTIVE_FAILURES as u64 {
            run(&mut scripts, &svm, slot);
        }
        assert!(scripts.run_slot(&svm, 50).is_empty(), "disabled");
        assert_eq!(scripts.len(), 1, "a disabled script stays registered");
        scripts
            .register(script("function on_slot(ctx) ctx.log('back') end"), 50)
            .unwrap();
        assert_eq!(run(&mut scripts, &svm, 51).logs, vec!["back"]);

        // A replacement that does not compile leaves the script in place.
        assert!(scripts.register(script("function on_slot("), 51).is_err());
        assert_eq!(run(&mut scripts, &svm, 52).logs, vec!["back"]);
    }

    #[test]
    fn the_interval_holds_through_its_edges_and_through_skipped_slots() {
        let calls = |from, until, every, base: Slot, slots: Vec<Slot>| -> Vec<String> {
            let mut scripts = ScenarioScripts::default();
            let mut windowed = script("function on_slot(ctx) ctx.log(ctx.slot) end");
            (windowed.active_from, windowed.active_until, windowed.every) = (from, until, every);
            scripts.register(windowed, base).unwrap();
            let svm = svm();
            slots
                .into_iter()
                .flat_map(|slot| scripts.run_slot(&svm, slot))
                .flat_map(|run| run.logs)
                .collect()
        };
        let all = || (0..12).collect::<Vec<Slot>>();
        // No interval: from the slot after the base, every slot.
        assert_eq!(
            calls(None, None, None, 5, all()),
            ["6", "7", "8", "9", "10", "11"]
        );
        // From the base itself.
        assert_eq!(calls(Some(0), Some(3), None, 5, all()), ["5", "6", "7"]);
        // `every` of 0 is every slot.
        assert_eq!(calls(Some(1), Some(4), Some(0), 5, all()), ["6", "7", "8"]);
        // An empty interval never runs.
        assert!(calls(Some(4), Some(4), None, 5, all()).is_empty());
        assert!(calls(Some(6), Some(2), None, 5, all()).is_empty());
        // A base in the future: nothing before it.
        assert_eq!(calls(None, None, Some(2), 9, all()), ["10"]);
        // A time travel skips slots: `every` keeps its phase, and a script whose interval was
        // jumped over is never called.
        assert_eq!(
            calls(Some(1), None, Some(3), 0, vec![1, 2, 3, 50, 51, 52, 53]),
            ["1", "52"]
        );
        assert!(calls(Some(1), Some(10), None, 0, vec![400, 401]).is_empty());
        // Slots near the top of the range do not overflow.
        assert!(
            calls(
                Some(u64::MAX),
                Some(u64::MAX),
                Some(u64::MAX),
                u64::MAX - 1,
                all()
            )
            .is_empty()
        );
    }

    #[test]
    fn account_fields_cross_into_lua_and_back_unchanged() {
        let mut svm = svm();
        let source_account = Pubkey::new_unique();
        let copy = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        // Every byte value, zeros included: Lua strings are byte strings.
        let data: Vec<u8> = (0..=255u8).chain([0, 0, 255, 0]).collect();
        svm.set_account(
            &source_account,
            Account {
                lamports: u64::MAX,
                data: data.clone(),
                owner,
                ..Account::default()
            },
        )
        .unwrap();
        let source = format!(
            r#"
            function on_slot(ctx)
              local a = ctx.account("{source_account}")
              assert(#a.data == 260 and a.owner == "{owner}" and a.executable == false)
              -- Past the signed 64-bit range a count arrives as a float.
              assert(math.type(a.lamports) == "float" and a.lamports == 2^64)
              assert(string.unpack("<I4", a.data, 1) == 0x03020100)
              assert(ctx.account("{copy}") == nil)
              ctx.set_account("{copy}", {{ lamports = 7, data = a.data, owner = a.owner }})
              ctx.set_account("{copy}", {{ data = string.pack("<I8", 1234567890123) .. a.data }})
            end
            "#
        );
        let mut scripts = ScenarioScripts::default();
        scripts.register(script(&source), 0).unwrap();
        let run = run(&mut scripts, &svm, 1);
        assert_eq!(run.error, None);
        let [
            Action::SetAccount(first, whole),
            Action::SetAccount(_, packed),
        ] = &run.actions[..]
        else {
            panic!("two writes");
        };
        assert_eq!(*first, copy);
        assert_eq!(
            (whole.lamports, &whole.data, whole.owner),
            (7, &data, owner)
        );
        assert_eq!(packed.data[..8], 1_234_567_890_123u64.to_le_bytes());
        assert_eq!(packed.data[8..], data[..]);
    }

    #[test]
    fn bad_arguments_fail_the_call_with_a_message_that_names_them() {
        for (call, message) in [
            (
                r#"ctx.account("not-a-key")"#,
                "not-a-key is not a public key",
            ),
            (r#"ctx.account()"#, "bad argument"),
            (
                r#"ctx.set_account("SysvarC1ock11111111111111111111111111111111", { lamports = 1 })"#,
                "SysvarC1ock11111111111111111111111111111111",
            ),
            (
                r#"ctx.set_account("11111111111111111111111111111112", { lamports = -1 })"#,
                "u64",
            ),
            (
                r#"ctx.set_account("11111111111111111111111111111112", { owner = "x" })"#,
                "x is not a public key",
            ),
            (r#"ctx.override { values = {} }"#, "`template` is required"),
            (
                r#"ctx.override { template = "token-amount" }"#,
                "give `account`",
            ),
            (
                r#"ctx.override { template = "token-amount", account = "nope" }"#,
                "nope is not a public key",
            ),
            (r#"ctx.phoenix.bbo("SOL")"#, "missing locally"),
            (r#"error({ code = 1 })"#, ""),
            (r#"local x = nil; x.y = 1"#, "attempt to index"),
        ] {
            let error = error_of(&format!("function on_slot(ctx) {call} end"));
            assert!(error.contains(message), "{call}: {error}");
        }
    }

    #[test]
    fn log_takes_strings_and_numbers() {
        assert_eq!(
            logs_of(
                r#"function on_slot(ctx) ctx.log("a") ctx.log(12) ctx.log(1.5) ctx.log(nil) end"#,
                1..2
            ),
            ["a", "12", "1.5", "nil"]
        );
    }

    #[test]
    fn a_script_cannot_reach_another_scripts_state() {
        let mut scripts = ScenarioScripts::default();
        let mut writer = script("secret = 42 function on_slot(ctx) end");
        writer.id = "writer".into();
        let mut reader = script("function on_slot(ctx) ctx.log(tostring(secret)) end");
        reader.id = "reader".into();
        scripts.register(writer, 0).unwrap();
        scripts.register(reader, 0).unwrap();
        let runs = scripts.run_slot(&svm(), 1);
        assert_eq!(runs[1].logs, vec!["nil"]);
    }

    // ---- On a surfnet, through block confirmation ----

    use crossbeam_channel::Receiver;
    use solana_program_pack::Pack;
    use spl_token_interface::state::{Account as TokenAccount, AccountState};
    use surfpool_types::{Scenario, SimnetEvent};

    use crate::surfnet::locker::SurfnetSvmLocker;

    fn surfnet() -> (SurfnetSvmLocker, Receiver<SimnetEvent>) {
        let (svm, events_rx, _geyser_rx) = SurfnetSvm::default();
        (SurfnetSvmLocker::new(svm), events_rx)
    }

    fn named(id: &str, source: impl Into<String>) -> ScenarioScript {
        let mut script = script(&source.into());
        script.id = id.into();
        script.name = id.into();
        script
    }

    fn scenario_of(scripts: Vec<ScenarioScript>) -> Scenario {
        let mut scenario = Scenario::new("test".into(), "test".into());
        scenario.scripts = scripts;
        scenario
    }

    fn lamports(locker: &SurfnetSvmLocker, pubkey: &Pubkey) -> u64 {
        locker
            .with_svm_reader(|svm| svm.get_account(pubkey))
            .unwrap()
            .map_or(0, |account| account.lamports)
    }

    fn set_lamports(locker: &SurfnetSvmLocker, pubkey: &Pubkey, lamports: u64) {
        locker.with_svm_writer(|svm| {
            svm.set_account(
                pubkey,
                Account {
                    lamports,
                    ..Account::default()
                },
            )
            .unwrap()
        });
    }

    async fn confirm(locker: &SurfnetSvmLocker, blocks: usize) {
        for _ in 0..blocks {
            locker.confirm_current_block(&None).await.unwrap();
        }
    }

    /// The script lines and warnings the surfnet reported so far.
    fn reported(events_rx: &Receiver<SimnetEvent>) -> (Vec<String>, Vec<String>) {
        let (mut infos, mut warnings) = (Vec::new(), Vec::new());
        for event in events_rx.try_iter() {
            match event {
                SimnetEvent::InfoLog(_, line) if line.starts_with('[') => infos.push(line),
                SimnetEvent::WarnLog(_, line) => warnings.push(line),
                _ => {}
            }
        }
        (infos, warnings)
    }

    fn token_account(locker: &SurfnetSvmLocker, amount: u64) -> Pubkey {
        let address = Pubkey::new_unique();
        let mut data = vec![0; TokenAccount::LEN];
        TokenAccount {
            mint: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            amount,
            state: AccountState::Initialized,
            ..Default::default()
        }
        .pack_into_slice(&mut data);
        locker.with_svm_writer(|svm| {
            svm.set_account(
                &address,
                Account {
                    lamports: 2_039_280,
                    data,
                    owner: spl_token_interface::ID,
                    ..Account::default()
                },
            )
            .unwrap()
        });
        address
    }

    fn token_amount(locker: &SurfnetSvmLocker, address: &Pubkey) -> u64 {
        let account = locker
            .with_svm_reader(|svm| svm.get_account(address))
            .unwrap()
            .unwrap();
        TokenAccount::unpack(&account.data).unwrap().amount
    }

    fn set_token_amount(address: &Pubkey, relative_slot: Slot, amount: u64) -> OverrideInstance {
        OverrideInstance::new(
            "token-amount".to_string(),
            relative_slot,
            AccountAddress::Pubkey(address.to_string()),
        )
        .with_values(HashMap::from([(
            "amount".to_string(),
            serde_json::json!(amount),
        )]))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_block_calls_the_script_once_and_its_write_is_on_chain() {
        let (locker, events_rx) = surfnet();
        let watched = Pubkey::new_unique();
        let mirror = Pubkey::new_unique();
        let base = locker.get_latest_absolute_slot();
        locker
            .register_scenario(
                scenario_of(vec![named(
                    "mirror",
                    format!(
                        r#"calls = 0
                        function on_slot(ctx)
                          calls = calls + 1
                          local a = ctx.account("{watched}")
                          ctx.set_account("{mirror}", {{ lamports = (a and a.lamports or 0) + calls }})
                          ctx.log(ctx.slot .. " " .. ctx.elapsed)
                        end"#
                    ),
                )]),
                None,
            )
            .unwrap();
        // Registering calls nothing.
        assert_eq!(lamports(&locker, &mirror), 0);

        confirm(&locker, 1).await;
        assert_eq!(lamports(&locker, &mirror), 1);
        set_lamports(&locker, &watched, 1_000);
        confirm(&locker, 1).await;
        assert_eq!(lamports(&locker, &mirror), 1_002);
        confirm(&locker, 3).await;
        assert_eq!(lamports(&locker, &mirror), 1_005);

        let (lines, warnings) = reported(&events_rx);
        let expected: Vec<String> = (1..=5)
            .map(|n| format!("[mirror] {} {n}", base + n))
            .collect();
        assert_eq!(lines, expected);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scripts_run_in_registration_order_and_each_reads_the_writes_before_it() {
        let (locker, _events_rx) = surfnet();
        let (shared, total) = (Pubkey::new_unique(), Pubkey::new_unique());
        let add = |id: &str, n: u64| {
            named(
                id,
                format!(
                    r#"function on_slot(ctx)
                         local a = ctx.account("{shared}")
                         ctx.set_account("{shared}", {{ lamports = (a and a.lamports or 0) * 10 + {n} }})
                       end"#
                ),
            )
        };
        let sum = named(
            "sum",
            format!(
                r#"function on_slot(ctx)
                     ctx.set_account("{total}", {{ lamports = ctx.account("{shared}").lamports }})
                   end"#
            ),
        );
        locker
            .register_scenario(scenario_of(vec![add("a", 1), add("b", 2)]), None)
            .unwrap();
        // A second scenario's scripts come after the first's.
        locker
            .register_scenario(scenario_of(vec![add("c", 3), sum]), None)
            .unwrap();
        confirm(&locker, 1).await;
        assert_eq!(lamports(&locker, &shared), 123);
        assert_eq!(lamports(&locker, &total), 123);
        confirm(&locker, 1).await;
        assert_eq!(lamports(&locker, &total), 123_123);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_scripts_override_applies_after_the_slots_scheduled_one() {
        let (locker, _events_rx) = surfnet();
        let token = token_account(&locker, 10);
        let mut scenario = scenario_of(vec![named(
            "double",
            format!(
                r#"function on_slot(ctx)
                     -- A token account's amount is the u64 at byte 64.
                     local amount = string.unpack("<I8", ctx.account("{token}").data, 65)
                     ctx.override {{
                       template = "token-amount",
                       account = "{token}",
                       values = {{ amount = amount * 2 }},
                     }}
                   end"#
            ),
        )]);
        // Scheduled for the second slot: the script reads 20 there and queues 40, which
        // applies after the scheduled 500.
        scenario.add_override(set_token_amount(&token, 2, 500));
        locker.register_scenario(scenario, None).unwrap();

        confirm(&locker, 1).await;
        assert_eq!(token_amount(&locker, &token), 20);
        confirm(&locker, 1).await;
        assert_eq!(token_amount(&locker, &token), 40);
        // The next slot reads what both left.
        confirm(&locker, 1).await;
        assert_eq!(token_amount(&locker, &token), 80);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_override_that_cannot_apply_is_skipped_and_the_rest_of_the_slot_goes_on() {
        let (locker, events_rx) = surfnet();
        let token = token_account(&locker, 1);
        let missing = Pubkey::new_unique();
        let counter = Pubkey::new_unique();
        locker
            .register_scenario(
                scenario_of(vec![named(
                    "mixed",
                    format!(
                        r#"n = 0
                        function on_slot(ctx)
                          n = n + 1
                          ctx.override {{ template = "token-amount", account = "{missing}", values = {{ amount = 5 }} }}
                          ctx.override {{ template = "token-amount", account = "{token}", values = {{ amount = "not a number" }} }}
                          ctx.override {{ template = "token-amount", account = "{token}", values = {{ amount = 100 + n }} }}
                          ctx.set_account("{counter}", {{ lamports = n }})
                        end"#
                    ),
                )]),
                None,
            )
            .unwrap();
        confirm(&locker, 3).await;
        assert_eq!(token_amount(&locker, &token), 103);
        assert_eq!(lamports(&locker, &counter), 3);
        assert!(
            locker
                .with_svm_reader(|svm| svm.get_account(&missing))
                .unwrap()
                .is_none()
        );
        // The script itself did not fail: nothing disables it.
        let (_, warnings) = reported(&events_rx);
        assert!(
            warnings.iter().all(|line| !line.contains("failed at slot")),
            "{warnings:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_script_is_reported_and_stops_neither_the_slot_nor_the_others() {
        let (locker, events_rx) = surfnet();
        let token = token_account(&locker, 1);
        let alive = Pubkey::new_unique();
        let mut scenario = scenario_of(vec![
            named(
                "broken",
                format!(
                    r#"function on_slot(ctx)
                         ctx.set_account("{alive}", {{ lamports = 999 }})
                         error("no")
                       end"#
                ),
            ),
            named(
                "healthy",
                format!(
                    r#"n = 0
                    function on_slot(ctx)
                      n = n + 1
                      ctx.set_account("{alive}", {{ lamports = n }})
                    end"#
                ),
            ),
        ]);
        for slot in 1..=30 {
            scenario.add_override(set_token_amount(&token, slot, slot));
        }
        locker.register_scenario(scenario, None).unwrap();
        confirm(&locker, 30).await;

        // The scheduled overrides and the other script went on every slot, and the failed
        // calls wrote nothing.
        assert_eq!(token_amount(&locker, &token), 30);
        assert_eq!(lamports(&locker, &alive), 30);
        let (_, warnings) = reported(&events_rx);
        let failures: Vec<&String> = warnings
            .iter()
            .filter(|line| line.contains("Scenario script broken failed"))
            .collect();
        assert_eq!(failures.len(), MAX_CONSECUTIVE_FAILURES as usize);
        assert!(failures[0].contains("no"));
        assert!(failures.last().unwrap().contains("(script disabled)"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_scenario_with_a_base_slot_starts_its_scripts_there() {
        let (locker, _events_rx) = surfnet();
        let calls = Pubkey::new_unique();
        let base = locker.get_latest_absolute_slot() + 5;
        let mut windowed = named(
            "windowed",
            format!(
                r#"function on_slot(ctx)
                     local a = ctx.account("{calls}")
                     ctx.set_account("{calls}", {{ lamports = (a and a.lamports or 0) * 100 + ctx.elapsed }})
                   end"#
            ),
        );
        windowed.active_from = Some(2);
        windowed.active_until = Some(5);
        locker
            .register_scenario(scenario_of(vec![windowed]), Some(base))
            .unwrap();
        confirm(&locker, 20).await;
        // Called at base + 2, + 3 and + 4, then removed.
        assert_eq!(lamports(&locker, &calls), 20_304);
        assert!(locker.with_svm_reader(|svm| svm.scenario_scripts.is_empty()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reset_drops_the_scripts_and_a_copy_of_the_surfnet_runs_none() {
        let (locker, _events_rx) = surfnet();
        let counter = Pubkey::new_unique();
        locker
            .register_scenario(
                scenario_of(vec![named(
                    "count",
                    format!(
                        r#"n = 0
                        function on_slot(ctx) n = n + 1 ctx.set_account("{counter}", {{ lamports = n }}) end"#
                    ),
                )]),
                None,
            )
            .unwrap();
        confirm(&locker, 2).await;
        assert_eq!(lamports(&locker, &counter), 2);

        locker.with_svm_reader(|svm| {
            assert!(svm.clone_for_profiling().scenario_scripts.is_empty());
            assert!(svm.clone().scenario_scripts.is_empty());
            assert_eq!(svm.scenario_scripts.len(), 1);
        });

        locker.with_svm_writer(|svm| {
            let (epoch_info, epoch_schedule) = (
                svm.latest_epoch_info.clone(),
                svm.inner
                    .get_sysvar::<solana_epoch_schedule::EpochSchedule>(),
            );
            svm.reset_network(epoch_info, epoch_schedule).unwrap();
        });
        assert!(locker.with_svm_reader(|svm| svm.scenario_scripts.is_empty()));
        confirm(&locker, 2).await;
        assert_eq!(lamports(&locker, &counter), 0);
    }

    /// The same scenario on two fresh surfnets, driven by the same writes, ends in the same
    /// state and says the same things: seeded draws, sorted iteration, no clock but the slot.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_replays_the_same() {
        let watched = Pubkey::new_unique();
        let out: Vec<Pubkey> = (0..4).map(|_| Pubkey::new_unique()).collect();
        let targets = out
            .iter()
            .map(|pubkey| format!("\"{pubkey}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!(
            r#"
            book = {{}}
            function on_slot(ctx)
              local a = ctx.account("{watched}")
              local seen = a and a.lamports or 0
              -- A walk with seeded noise, pushed by what the chain shows.
              for i, target in ipairs({{ {targets} }}) do
                local level = (book[target] or 1000000) + math.floor((ctx.random() - 0.5) * 1000) + seen
                book[target] = level
              end
              local line = {{}}
              for target, level in pairs(book) do
                ctx.set_account(target, {{ lamports = level }})
                line[#line + 1] = level
              end
              ctx.log(table.concat(line, " "))
            end
            "#
        );
        let session = |seed: u64| {
            let source = source.clone();
            let out = out.clone();
            async move {
                let (locker, events_rx) = surfnet();
                let mut script = named("walk", source);
                script.seed = Some(seed);
                locker
                    .register_scenario(scenario_of(vec![script]), None)
                    .unwrap();
                for step in 0..40u64 {
                    if step % 7 == 3 {
                        set_lamports(&locker, &watched, step);
                    }
                    confirm(&locker, 1).await;
                }
                let state: Vec<u64> = out.iter().map(|pubkey| lamports(&locker, pubkey)).collect();
                let (lines, warnings) = reported(&events_rx);
                assert!(warnings.is_empty(), "{warnings:?}");
                // The slot differs between surfnets only if they start apart; these do not.
                (state, lines)
            }
        };
        let first = session(11).await;
        assert_eq!(first.1.len(), 40);
        assert_eq!(session(11).await, first);
        assert_eq!(session(11).await, first);
        assert_ne!(session(12).await.0, first.0);
    }
}
