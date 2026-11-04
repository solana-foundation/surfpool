use std::{collections::BTreeSet, fmt::Write, str::FromStr};

use ansi_term::Colour;
use serde::Serialize;
use serde_json::{Value, json};
use solana_pubkey::Pubkey;
use surfpool_core::solana_rpc_client::{
    api::request::RpcRequest, nonblocking::rpc_client::RpcClient,
};

use crate::cli::CuCommand;

/// Native and SPL programs, named without asking the RPC for an IDL.
const KNOWN_PROGRAMS: &[(&str, &str)] = &[
    ("11111111111111111111111111111111", "System"),
    (
        "ComputeBudget111111111111111111111111111111",
        "Compute Budget",
    ),
    (
        "AddressLookupTab1e1111111111111111111111111",
        "Address Lookup Table",
    ),
    (
        "BPFLoaderUpgradeab1e11111111111111111111111",
        "BPF Upgradeable Loader",
    ),
    ("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", "SPL Token"),
    ("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb", "Token-2022"),
    (
        "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
        "Associated Token",
    ),
    ("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr", "Memo"),
];

/// A transaction's compute units, broken down by the program invocations its logs record.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionProfile {
    pub signature: String,
    pub slot: u64,
    pub err: Value,
    pub compute_units_consumed: Option<u64>,
    /// The runtime cut the logs short, so compute units after the cut are unknown.
    pub logs_truncated: bool,
    pub invocations: Vec<Invocation>,
}

/// One program invocation: a top-level instruction or a CPI.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Invocation {
    pub program_id: String,
    pub name: Option<String>,
    /// Compute units consumed, CPIs included. Builtin programs log none, and neither does an
    /// invocation after the logs are cut.
    pub consumed: Option<u64>,
    pub error: Option<String>,
    /// The logs end before this invocation does.
    pub unfinished: bool,
    pub invocations: Vec<Invocation>,
}

impl Invocation {
    fn label(&self) -> String {
        printable(self.name.as_deref().unwrap_or(&self.program_id))
    }

    /// Compute units the logs put on this invocation: what it consumed or, when the logs end
    /// before its `consumed` line, what its CPIs did.
    fn attributed(&self) -> u64 {
        self.consumed
            .unwrap_or_else(|| self.invocations.iter().map(Invocation::attributed).sum())
    }

    /// Compute units spent in this program itself, outside the CPIs it made.
    fn self_consumed(&self) -> u64 {
        let in_cpis: u64 = self.invocations.iter().map(Invocation::attributed).sum();
        self.consumed
            .map_or(0, |consumed| consumed.saturating_sub(in_cpis))
    }
}

/// Program names and errors come from the RPC: control characters in them could rewrite the
/// terminal, so they print as U+FFFD.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

/// Builds the invocation tree from the runtime's `Program <id> invoke [n]`, `Program <id>
/// consumed <n> of <m> compute units`, `Program <id> success` and `Program <id> failed: <err>`
/// lines. Returns the top-level invocations and whether the logs were truncated.
pub fn parse_invocations(logs: &[String]) -> (Vec<Invocation>, bool) {
    let mut top_level = vec![];
    let mut open: Vec<Invocation> = vec![];
    let mut truncated = false;

    for line in logs {
        if line == "Log truncated" {
            truncated = true;
            break;
        }
        // A program's own output is `Program log: ...`, `Program data: ...` and so on, never a
        // program id, so it can't pass for a runtime line.
        let Some((program_id, event)) = line
            .strip_prefix("Program ")
            .and_then(|rest| rest.split_once(' '))
            .filter(|(program_id, _)| Pubkey::from_str(program_id).is_ok())
        else {
            continue;
        };

        if event.starts_with("invoke [") {
            open.push(invocation(program_id));
        } else if let Some(units) = event.strip_prefix("consumed ") {
            if let Some(invocation) = open.last_mut() {
                invocation.consumed = units.split(' ').next().and_then(|n| n.parse().ok());
            }
        } else if (event == "success" || event.starts_with("failed: "))
            && let Some(mut invocation) = open.pop()
        {
            invocation.error = event.strip_prefix("failed: ").map(str::to_string);
            attach(invocation, &mut open, &mut top_level);
        }
    }

    while let Some(mut invocation) = open.pop() {
        invocation.unfinished = true;
        attach(invocation, &mut open, &mut top_level);
    }
    (top_level, truncated)
}

fn attach(invocation: Invocation, open: &mut [Invocation], top_level: &mut Vec<Invocation>) {
    match open.last_mut() {
        Some(caller) => caller.invocations.push(invocation),
        None => top_level.push(invocation),
    }
}

fn invocation(program_id: &str) -> Invocation {
    Invocation {
        program_id: program_id.to_string(),
        name: KNOWN_PROGRAMS
            .iter()
            .find(|(id, _)| *id == program_id)
            .map(|(_, name)| name.to_string()),
        ..Default::default()
    }
}

/// Builds the invocation tree from the message's instructions and the CPIs
/// `meta.innerInstructions` records under them, nested by `stackHeight`. Unlike the logs, these
/// aren't truncated. `None` when the RPC doesn't record them, or not with their stack heights.
fn instruction_tree(transaction: &Value) -> Option<Vec<Invocation>> {
    let message = &transaction["transaction"]["message"];
    let meta = &transaction["meta"];
    // Instructions index the static account keys, then those loaded from lookup tables.
    let keys = message["accountKeys"]
        .as_array()?
        .iter()
        .chain(
            meta["loadedAddresses"]["writable"]
                .as_array()
                .into_iter()
                .flatten(),
        )
        .chain(
            meta["loadedAddresses"]["readonly"]
                .as_array()
                .into_iter()
                .flatten(),
        )
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()?;
    let program = |instruction: &Value| {
        let index = instruction["programIdIndex"].as_u64()?;
        keys.get(index as usize)
            .map(|program_id| invocation(program_id))
    };

    let mut top_level = message["instructions"]
        .as_array()?
        .iter()
        .map(program)
        .collect::<Option<Vec<_>>>()?;
    for inner in meta["innerInstructions"].as_array()? {
        let index = inner["index"].as_u64()? as usize;
        let mut open = vec![std::mem::take(top_level.get_mut(index)?)];
        for instruction in inner["instructions"].as_array()? {
            // The top-level instruction is at height 1, its CPIs at 2, and so on.
            let height = instruction["stackHeight"].as_u64()? as usize;
            while open.len() >= height {
                let done = open.pop()?;
                open.last_mut()?.invocations.push(done);
            }
            open.push(program(instruction)?);
        }
        while open.len() > 1 {
            let done = open.pop()?;
            open.last_mut()?.invocations.push(done);
        }
        top_level[index] = open.pop()?;
    }

    // The instructions after the one that failed never ran.
    if let Some(failed) = meta["err"]["InstructionError"][0].as_u64() {
        top_level.truncate(failed as usize + 1);
    }
    Some(top_level)
}

/// Copies the compute units, errors and cut-off frames the logs record onto the instruction
/// tree. Both list invocations in the order they ran, so the n-th `invoke` line is the n-th
/// instruction. One the logs don't open, a precompile's or one after the cut, gets nothing.
fn take_compute_units_from_logs(tree: &mut [Invocation], mut logged: Vec<Invocation>) {
    let mut frames = vec![];
    for_each_invocation(&mut logged, &mut |frame| {
        frames.push((
            std::mem::take(&mut frame.program_id),
            frame.consumed,
            frame.error.take(),
            frame.unfinished,
        ));
    });
    let mut frames = frames.into_iter().peekable();
    for_each_invocation(tree, &mut |invocation| {
        if let Some((_, consumed, error, unfinished)) =
            frames.next_if(|(program_id, ..)| *program_id == invocation.program_id)
        {
            invocation.consumed = consumed;
            invocation.error = error;
            invocation.unfinished = unfinished;
        }
    });
}

fn for_each_invocation(invocations: &mut [Invocation], f: &mut impl FnMut(&mut Invocation)) {
    for invocation in invocations {
        f(invocation);
        for_each_invocation(&mut invocation.invocations, f);
    }
}

impl TransactionProfile {
    /// Reads a `getTransaction` result.
    pub fn from_rpc(signature: &str, transaction: &Value) -> Result<Self, String> {
        if transaction.is_null() {
            return Err(format!("transaction {signature} not found"));
        }
        let meta = &transaction["meta"];
        let logs: Vec<String> = serde_json::from_value(meta["logMessages"].clone())
            .map_err(|_| format!("the RPC returned no log messages for {signature}"))?;
        let (logged, logs_truncated) = parse_invocations(&logs);
        let invocations = match instruction_tree(transaction) {
            Some(mut tree) => {
                take_compute_units_from_logs(&mut tree, logged);
                tree
            }
            None => logged,
        };
        Ok(Self {
            signature: signature.to_string(),
            slot: transaction["slot"].as_u64().unwrap_or_default(),
            err: meta["err"].clone(),
            compute_units_consumed: meta["computeUnitsConsumed"].as_u64(),
            logs_truncated,
            invocations,
        })
    }

    /// Compute units no `consumed` line charges to a top-level invocation: builtin programs',
    /// those after a log cut, and those a failed transaction's runtime drains after the last
    /// `consumed` line.
    fn unattributed(&self) -> u64 {
        let attributed: u64 = self.invocations.iter().map(Invocation::attributed).sum();
        self.compute_units_consumed
            .unwrap_or(0)
            .saturating_sub(attributed)
    }

    pub fn render(&self, color: bool) -> String {
        let paint = |colour: Colour, text: &str| {
            if color {
                colour.paint(text).to_string()
            } else {
                text.to_string()
            }
        };

        let mut out = format!("{}\n", self.signature);
        let total = self
            .compute_units_consumed
            .map(|units| format!(" · {} CU", thousands(units)))
            .unwrap_or_default();
        if self.err.is_null() {
            let status = paint(Colour::Green, "✓ succeeded");
            let _ = writeln!(out, "{status} in slot {}{total}\n", thousands(self.slot));
        } else {
            let status = paint(Colour::Red, "✗ failed");
            let _ = writeln!(
                out,
                "{status} in slot {}{total} · {}\n",
                thousands(self.slot),
                self.err
            );
        }

        let mut rows = vec![];
        push_rows(
            &self.invocations,
            "",
            self.compute_units_consumed,
            &mut rows,
        );
        let tree_width = rows.iter().map(|r| r.tree.chars().count()).max();
        let units_width = rows.iter().map(|r| r.units.chars().count()).max();
        for row in &rows {
            let line = format!(
                "{:<tree_width$}  {:>units_width$}  {:>6}",
                row.tree,
                row.units,
                row.share,
                tree_width = tree_width.unwrap_or_default(),
                units_width = units_width.unwrap_or_default(),
            );
            let note = match (&row.error, row.unfinished) {
                (Some(error), _) => format!("  {}", paint(Colour::Red, &format!("✗ {error}"))),
                (None, true) => format!("  {}", paint(Colour::Yellow, "… logs end here")),
                (None, false) => String::new(),
            };
            let _ = writeln!(out, "{}{note}", line.trim_end());
        }

        if self.logs_truncated {
            let _ = writeln!(
                out,
                "\n{}",
                paint(
                    Colour::Yellow,
                    "The runtime truncated these logs: compute units after the cut are unknown."
                )
            );
        }
        let unattributed = self.unattributed();
        if unattributed > 0 {
            let _ = writeln!(
                out,
                "\n{} CU unattributed: no program's logs account for them.",
                thousands(unattributed)
            );
        }
        out
    }

    /// Collapsed stacks, one `caller;callee <self compute units>` line per invocation, for
    /// speedscope.app, inferno-flamegraph or flamegraph.pl. The weights add up to the
    /// transaction's compute units.
    pub fn folded(&self) -> String {
        fn walk(invocations: &[Invocation], stack: &str, out: &mut String) {
            for invocation in invocations {
                // `;` separates frames, so a name can't contain one.
                let frame = format!("{stack}{}", invocation.label().replace(';', ":"));
                let own = invocation.self_consumed();
                if own > 0 {
                    let _ = writeln!(out, "{frame} {own}");
                }
                walk(&invocation.invocations, &format!("{frame};"), out);
            }
        }

        let mut out = String::new();
        walk(&self.invocations, "", &mut out);
        let unattributed = self.unattributed();
        if unattributed > 0 {
            let _ = writeln!(out, "unattributed {unattributed}");
        }
        out
    }
}

struct Row {
    tree: String,
    units: String,
    share: String,
    error: Option<String>,
    unfinished: bool,
}

/// One row per invocation, depth first. `total` is the transaction's compute units, given for
/// the top level only, where each invocation's share of it is shown.
fn push_rows(invocations: &[Invocation], indent: &str, total: Option<u64>, rows: &mut Vec<Row>) {
    for (index, invocation) in invocations.iter().enumerate() {
        let last = index + 1 == invocations.len();
        let share = match (invocation.consumed, total) {
            (Some(units), Some(total)) if total > 0 => {
                format!("{:.1}%", units as f64 * 100.0 / total as f64)
            }
            _ => String::new(),
        };
        rows.push(Row {
            tree: format!(
                "{indent}{}{}",
                if last { "└─ " } else { "├─ " },
                invocation.label()
            ),
            units: invocation
                .consumed
                .map(|units| format!("{} CU", thousands(units)))
                .unwrap_or_else(|| "—".to_string()),
            share,
            error: invocation.error.as_deref().map(printable),
            unfinished: invocation.unfinished,
        });
        push_rows(
            &invocation.invocations,
            &format!("{indent}{}", if last { "   " } else { "│  " }),
            None,
            rows,
        );
    }
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Names programs the built-in list doesn't know from the IDLs registered with the surfnet.
/// Best effort: a program without an IDL keeps its address, and so does every program on an RPC
/// that isn't a surfnet.
async fn name_programs_from_idls(client: &RpcClient, invocations: &mut [Invocation]) {
    let mut unnamed = BTreeSet::new();
    for_each_invocation(invocations, &mut |invocation| {
        if invocation.name.is_none() {
            unnamed.insert(invocation.program_id.clone());
        }
    });

    let mut names = vec![];
    for program_id in unnamed {
        let request = RpcRequest::Custom {
            method: "surfnet_getActiveIdl",
        };
        // Stop at the first error, such as a "method not found" from an RPC that isn't a
        // surfnet, rather than asking once per program.
        let Ok(idl) = client.send::<Value>(request, json!([program_id])).await else {
            break;
        };
        if let Some(name) = idl["value"]["metadata"]["name"].as_str() {
            names.push((program_id, name.to_string()));
        }
    }

    for_each_invocation(invocations, &mut |invocation| {
        if let Some((_, name)) = names.iter().find(|(id, _)| *id == invocation.program_id) {
            invocation.name = Some(name.clone());
        }
    });
}

pub async fn handle_cu_command(cmd: CuCommand) -> Result<(), String> {
    let rpc_url = cmd.get_rpc_url();
    let client = RpcClient::new(rpc_url.clone());
    let transaction = client
        .send::<Value>(
            RpcRequest::GetTransaction,
            json!([
                cmd.signature,
                {
                    "encoding": "json",
                    "maxSupportedTransactionVersion": 1,
                    "commitment": "confirmed",
                }
            ]),
        )
        .await
        .map_err(|e| format!("failed to read {} from {rpc_url}: {e}", cmd.signature))?;
    let mut profile = TransactionProfile::from_rpc(&cmd.signature, &transaction)?;
    name_programs_from_idls(&client, &mut profile.invocations).await;

    if let Some(path) = &cmd.folded {
        std::fs::write(path, profile.folded())
            .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    }

    if cmd.json {
        let json = serde_json::to_string_pretty(&profile)
            .map_err(|e| format!("failed to serialize the profile: {e}"))?;
        println!("{json}");
    } else {
        print!("{}", profile.render(atty::is(atty::Stream::Stdout)));
        if let Some(path) = &cmd.folded {
            println!(
                "\nWrote a flame graph to {}: drop it on https://www.speedscope.app",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATA: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
    const TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    const SYSTEM: &str = "11111111111111111111111111111111";
    const UNKNOWN: &str = "L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95";
    const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";
    const ED25519: &str = "Ed25519SigVerify111111111111111111111111111";

    fn logs(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    /// The opening of mainnet transaction 2Qk4Jev…UdQi (slot 463,407,496): two compute budget
    /// instructions, then an associated token account creation with its four CPIs.
    fn create_associated_token_account_logs() -> Vec<String> {
        logs(&[
            "Program ComputeBudget111111111111111111111111111111 invoke [1]",
            "Program ComputeBudget111111111111111111111111111111 success",
            "Program ComputeBudget111111111111111111111111111111 invoke [1]",
            "Program ComputeBudget111111111111111111111111111111 success",
            "Program ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL invoke [1]",
            "Program log: CreateIdempotent",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [2]",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA consumed 179 of 189849 compute units",
            "Program return: TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA pQAAAAAAAAA=",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA success",
            "Program 11111111111111111111111111111111 invoke [2]",
            "Program 11111111111111111111111111111111 success",
            "Program log: Initialize the associated token account",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [2]",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA consumed 37 of 184760 compute units",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA success",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [2]",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA consumed 229 of 182299 compute units",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA success",
            "Program ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL consumed 17913 of 199700 compute units",
            "Program ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL success",
        ])
    }

    /// A `getTransaction` result for those logs: 17,913 CU in the associated token program,
    /// 150 in each compute budget instruction.
    fn create_associated_token_account() -> Value {
        json!({
            "slot": 463407496,
            "transaction": {
                "message": {
                    "accountKeys": [UNKNOWN, COMPUTE_BUDGET, ATA, TOKEN, SYSTEM],
                    "instructions": [
                        {"programIdIndex": 1},
                        {"programIdIndex": 1},
                        {"programIdIndex": 2},
                    ],
                },
            },
            "meta": {
                "err": null,
                "computeUnitsConsumed": 18213,
                "logMessages": create_associated_token_account_logs(),
                "innerInstructions": [{
                    "index": 2,
                    "instructions": [
                        {"programIdIndex": 3, "stackHeight": 2},
                        {"programIdIndex": 4, "stackHeight": 2},
                        {"programIdIndex": 3, "stackHeight": 2},
                        {"programIdIndex": 3, "stackHeight": 2},
                    ],
                }],
            },
        })
    }

    #[test]
    fn nested_invocations_become_a_tree_with_self_compute_units() {
        let (top_level, truncated) = parse_invocations(&create_associated_token_account_logs());

        assert!(!truncated);
        assert_eq!(top_level.len(), 3);
        let ata = &top_level[2];
        assert_eq!((ata.program_id.as_str(), ata.consumed), (ATA, Some(17_913)));
        assert_eq!(ata.name.as_deref(), Some("Associated Token"));
        let cpis = ata
            .invocations
            .iter()
            .map(|cpi| (cpi.program_id.as_str(), cpi.consumed))
            .collect::<Vec<_>>();
        assert_eq!(
            cpis,
            [
                (TOKEN, Some(179)),
                (SYSTEM, None),
                (TOKEN, Some(37)),
                (TOKEN, Some(229)),
            ]
        );
        assert_eq!(ata.self_consumed(), 17_913 - 179 - 37 - 229);
    }

    #[test]
    fn a_builtin_without_a_consumed_line_shows_no_cost_and_charges_its_caller() {
        let profile =
            TransactionProfile::from_rpc("sig", &create_associated_token_account()).unwrap();

        let system = &profile.invocations[2].invocations[1];
        assert_eq!((system.consumed, system.self_consumed()), (None, 0));
        // The compute budget instructions log no cost: their 300 CU are reported apart.
        assert_eq!(profile.unattributed(), 300);
    }

    #[test]
    fn a_failed_invocation_carries_its_error_and_its_caller_fails_with_it() {
        let (top_level, _) = parse_invocations(&logs(&[
            "Program L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95 invoke [1]",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [2]",
            "Program log: Error: insufficient funds",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA consumed 1418 of 213427 compute units",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA failed: custom program error: 0x1",
            "Program L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95 consumed 3819 of 215828 compute units",
            "Program L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95 failed: custom program error: 0x1",
        ]));

        let caller = &top_level[0];
        assert_eq!(caller.error.as_deref(), Some("custom program error: 0x1"));
        assert_eq!(
            caller.invocations[0].error.as_deref(),
            Some("custom program error: 0x1")
        );
        assert!(!caller.unfinished);
    }

    #[test]
    fn truncated_logs_mark_the_tree_and_its_open_invocations() {
        let (top_level, truncated) = parse_invocations(&logs(&[
            "Program L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95 invoke [1]",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [2]",
            "Log truncated",
        ]));

        assert!(truncated);
        assert_eq!(top_level.len(), 1, "the open CPI closes inside its caller");
        assert!(top_level[0].unfinished);
        assert!(top_level[0].invocations[0].unfinished);
    }

    #[test]
    fn truncated_logs_keep_every_call_the_metadata_records() {
        // An Ed25519 precompile, which logs nothing, then a program whose logs are cut inside it.
        // Its CPIs go to programs loaded from a lookup table.
        let profile = TransactionProfile::from_rpc(
            "sig",
            &json!({
                "slot": 1,
                "transaction": {
                    "message": {
                        "accountKeys": [UNKNOWN, ED25519],
                        "instructions": [{"programIdIndex": 1}, {"programIdIndex": 0}],
                    },
                },
                "meta": {
                    "err": null,
                    "computeUnitsConsumed": 5000,
                    "loadedAddresses": {"writable": [], "readonly": [TOKEN, SYSTEM]},
                    "innerInstructions": [{
                        "index": 1,
                        "instructions": [
                            {"programIdIndex": 2, "stackHeight": 2},
                            {"programIdIndex": 3, "stackHeight": 3},
                            {"programIdIndex": 2, "stackHeight": 2},
                        ],
                    }],
                    "logMessages": [
                        "Program L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95 invoke [1]",
                        "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [2]",
                        "Program 11111111111111111111111111111111 invoke [3]",
                        "Program 11111111111111111111111111111111 success",
                        "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA consumed 300 of 199000 compute units",
                        "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA success",
                        "Log truncated",
                    ],
                },
            }),
        )
        .unwrap();

        assert!(profile.logs_truncated);
        let [precompile, caller] = &profile.invocations[..] else {
            panic!("{:?}", profile.invocations);
        };
        assert_eq!(
            (precompile.program_id.as_str(), precompile.consumed),
            (ED25519, None)
        );
        assert_eq!(
            (caller.program_id.as_str(), caller.consumed),
            (UNKNOWN, None)
        );
        assert!(caller.unfinished);
        let cpis = caller
            .invocations
            .iter()
            .map(|cpi| (cpi.program_id.as_str(), cpi.consumed))
            .collect::<Vec<_>>();
        assert_eq!(
            cpis,
            [(TOKEN, Some(300)), (TOKEN, None)],
            "the CPI after the cut stays"
        );
        assert_eq!(caller.invocations[0].invocations[0].program_id, SYSTEM);
        assert_eq!(profile.unattributed(), 4700);
    }

    #[test]
    fn instructions_after_the_failed_one_are_left_out() {
        let profile = TransactionProfile::from_rpc(
            "sig",
            &json!({
                "slot": 1,
                "transaction": {
                    "message": {
                        "accountKeys": [UNKNOWN, TOKEN],
                        "instructions": [{"programIdIndex": 0}, {"programIdIndex": 1}],
                    },
                },
                "meta": {
                    "err": {"InstructionError": [0, {"Custom": 1}]},
                    "computeUnitsConsumed": 100,
                    "innerInstructions": [],
                    "logMessages": [
                        "Program L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95 invoke [1]",
                        "Program L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95 consumed 100 of 200000 compute units",
                        "Program L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95 failed: custom program error: 0x1",
                    ],
                },
            }),
        )
        .unwrap();

        let [failed] = &profile.invocations[..] else {
            panic!("{:?}", profile.invocations);
        };
        assert_eq!(failed.error.as_deref(), Some("custom program error: 0x1"));
    }

    #[test]
    fn a_programs_own_log_lines_cannot_pass_for_the_runtimes() {
        let (top_level, _) = parse_invocations(&logs(&[
            "Program L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95 invoke [1]",
            "Program log: success",
            "Program log: consumed 1 of 2 compute units",
            "Program data: invoke [2]",
            "Program L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95 consumed 701 of 215828 compute units",
            "Program L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95 success",
        ]));

        assert_eq!(top_level.len(), 1);
        assert_eq!(top_level[0].program_id, UNKNOWN);
        assert_eq!(top_level[0].consumed, Some(701));
        assert!(top_level[0].invocations.is_empty());
    }

    #[test]
    fn a_cpi_under_a_caller_cut_from_the_logs_is_counted_once() {
        let profile = TransactionProfile::from_rpc(
            "sig",
            &json!({
                "slot": 1,
                "meta": {
                    "err": null,
                    "computeUnitsConsumed": 1000,
                    "logMessages": [
                        "Program L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95 invoke [1]",
                        "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [2]",
                        "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA consumed 200 of 199000 compute units",
                        "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA success",
                        "Log truncated",
                    ],
                },
            }),
        )
        .unwrap();

        assert_eq!(profile.unattributed(), 800);
        assert_eq!(
            profile.folded(),
            format!("{UNKNOWN};SPL Token 200\nunattributed 800\n")
        );
    }

    #[test]
    fn names_and_errors_from_the_rpc_cannot_steer_the_terminal_or_the_flame_graph() {
        let profile = TransactionProfile {
            signature: "sig".to_string(),
            slot: 1,
            err: json!({"InstructionError": [0, {"Custom": 1}]}),
            compute_units_consumed: Some(100),
            logs_truncated: false,
            invocations: vec![Invocation {
                program_id: UNKNOWN.to_string(),
                name: Some("evil\u{1b}[2J;caller".to_string()),
                consumed: Some(100),
                error: Some("custom\u{1b}]0;title\u{7}\nerror".to_string()),
                ..Default::default()
            }],
        };

        let rendered = profile.render(false);
        assert!(!rendered.contains(['\u{1b}', '\u{7}']), "{rendered:?}");
        assert!(rendered.contains("evil\u{FFFD}[2J;caller"));
        assert_eq!(profile.folded(), "evil\u{FFFD}[2J:caller 100\n");
    }

    #[test]
    fn folded_stacks_add_up_to_the_transactions_compute_units() {
        let profile =
            TransactionProfile::from_rpc("sig", &create_associated_token_account()).unwrap();

        let folded = profile.folded();
        assert_eq!(
            folded,
            "Associated Token 17468\n\
             Associated Token;SPL Token 179\n\
             Associated Token;SPL Token 37\n\
             Associated Token;SPL Token 229\n\
             unattributed 300\n"
        );
        let total: u64 = folded
            .lines()
            .map(|line| line.rsplit_once(' ').unwrap().1.parse::<u64>().unwrap())
            .sum();
        assert_eq!(Some(total), profile.compute_units_consumed);
    }

    #[test]
    fn the_tree_shows_each_invocations_compute_units_and_share() {
        let profile =
            TransactionProfile::from_rpc("sig", &create_associated_token_account()).unwrap();

        let expected = [
            "sig",
            "✓ succeeded in slot 463,407,496 · 18,213 CU",
            "",
            "├─ Compute Budget            —",
            "├─ Compute Budget            —",
            "└─ Associated Token  17,913 CU   98.4%",
            "   ├─ SPL Token         179 CU",
            "   ├─ System                 —",
            "   ├─ SPL Token          37 CU",
            "   └─ SPL Token         229 CU",
            "",
            "300 CU unattributed: no program's logs account for them.",
            "",
        ];
        assert_eq!(profile.render(false), expected.join("\n"));
    }

    #[test]
    fn a_missing_transaction_or_log_is_reported() {
        assert_eq!(
            TransactionProfile::from_rpc("sig", &Value::Null).unwrap_err(),
            "transaction sig not found"
        );
        assert_eq!(
            TransactionProfile::from_rpc("sig", &json!({"slot": 1, "meta": {"err": null}}))
                .unwrap_err(),
            "the RPC returned no log messages for sig"
        );
    }

    #[test]
    fn thousands_are_separated() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(463_407_496), "463,407,496");
    }
}
