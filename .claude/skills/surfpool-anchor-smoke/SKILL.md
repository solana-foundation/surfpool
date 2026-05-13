---
name: surfpool-anchor-smoke
description: Smoke test a local Surfpool build against Anchor's reusable test matrix. Use when an agent needs to build Surfpool from source, rebuild the Anchor CLI, point Anchor at the locally installed Surfpool binary, run the enabled entries from `.github/workflows/reusable-tests.yaml` one at a time with Anchor's local release build, and produce a failure report. Prompt for the Surfpool and Anchor repo roots when they are not already explicit in the task.
---

# Surfpool Anchor Smoke

Use this folder as a reusable playbook for running Anchor's reusable smoke matrix against a local Surfpool build.

## Inputs

- Ask for both repo roots unless they are already explicit:
  - Surfpool repo root
  - Anchor repo root
- If the agent cannot do interactive prompts, pass them explicitly:

```bash
python3 .claude/skills/surfpool-anchor-smoke/scripts/run_smoke_suite.py \
  --surfpool-dir <path-to-surfpool-repo> \
  --anchor-dir <path-to-anchor-repo>
```

- The runner also accepts `SURFPOOL_DIR` and `ANCHOR_DIR` environment variables.

## Workflow

1. Install Surfpool locally with `cargo surfpool-install-dev` in the Surfpool repo.
2. Locate the Surfpool process spawn inside the Anchor repo's `cli/src/lib.rs` by searching the `start_surfpool_validator` function body for `Command::new(...surfpool...)`.
3. Update that command to use the local cargo-installed Surfpool binary, usually `~/.cargo/bin/surfpool`.
4. Build the Anchor CLI with `cd cli && cargo build --release` in the Anchor repo.
5. Reproduce the local TS `yarn link` setup from the Anchor repo's `.github/actions/setup-ts/action.yaml`.
6. Parse the enabled `- cmd:` entries from the Anchor repo's `.github/workflows/reusable-tests.yaml` and drop entries in the runner's built-in skip list (currently `tests/anchor-cli-idl`).
7. Run those tests serially with Anchor's repo-local release binary. Rewrite direct `anchor ...` invocations to `target/release/anchor` relative to each test directory, and prepend `<anchor-dir>/target/release` to `PATH` so shell scripts also pick up the local build.
8. Write per-step logs and a Markdown report that highlights failed suites, hangs, and setup problems.

## Runner

Run from the Surfpool repo root:

```bash
python3 .claude/skills/surfpool-anchor-smoke/scripts/run_smoke_suite.py
```

Useful flags:

- `--match <text>`: run only commands whose original workflow command contains the text. Pass more than once to keep multiple subsets.
- `--skip <text>`: drop commands whose original workflow command contains the text. Stacks with the built-in skip list (`tests/anchor-cli-idl`).
- `--no-default-skips`: disable the built-in skip list. Only use this if you want to debug the skipped suite in isolation; it will poison subsequent suites.
- `--max-tests <n>`: stop after the first `n` selected tests.
- `--dry-run`: print and report the planned commands without executing them.
- `--list-tests`: print the extracted workflow commands and exit.
- `--local-surfpool-bin <path>`: override the path used when patching Anchor's Surfpool spawn command.
- `--skip-anchor-patch`: skip the automatic Anchor CLI patch step.
- `--skip-surfpool-build`, `--skip-anchor-build`, `--skip-link-setup`: reuse prior setup artifacts while iterating.
- `--test-timeout <seconds>` (default 900): hard ceiling per test. When hit, the whole process group is SIGTERMed then SIGKILLed.
- `--idle-timeout <seconds>` (default 300): kill a test that has produced no output for this long. Catches mocha hangs waiting on never-fired signature/logs subscriptions.

Example:

```bash
python3 .claude/skills/surfpool-anchor-smoke/scripts/run_smoke_suite.py \
  --surfpool-dir <path-to-surfpool-repo> \
  --anchor-dir <path-to-anchor-repo> \
  --match tests/sysvars \
  --match tests/errors
```

## Guardrails

- Do not hardcode user-specific repo paths in the skill body or in the runner defaults.
- Do not hardcode a line number in `cli/src/lib.rs`; always find the Surfpool command call by search.
- Patch only the located `Command::new(...surfpool...)` string literal inside `start_surfpool_validator`.
- Do not use the Anchor repo's `setup-tests.sh` for this workflow. It installs a debug `anchor` into `~/.cargo/bin`, while this smoke test is supposed to exercise `target/release/anchor`.
- Keep user worktree changes unless they directly block the smoke run.
- If setup fails before the test phase starts, stop and report that setup failure rather than guessing about test outcomes.
- `tests/anchor-cli-idl/test.sh` is skipped by default. It spawns `solana-test-validator --reset ... &` bound to `localhost:8899` and only kills it via a trailing `kill $(jobs -p)` that never runs because `set -euo pipefail` exits earlier when the in-script `anchor test` fails. The orphaned validator survives the runner's process-group SIGTERM on darwin, and every subsequent Surfpool-spawned suite then routes traffic to it — manifesting as `Transaction simulation failed: This program may not be used for executing instructions` (no programs deployed) and `RPC response error -32601: Method not found` (missing Surfpool extension RPCs). Keep it skipped unless you are debugging that suite in isolation.
- Other test scripts may also background their own validators. If a similar pattern shows up later, add it to `DEFAULT_SKIP_PATTERNS` in `run_smoke_suite.py` rather than leaving it to poison the run.

## Resource

`scripts/run_smoke_suite.py`

- prompts for repo roots when needed
- patches Anchor's Surfpool spawn command by search, not by line number
- executes setup and serial test runs
- rewrites top-level `anchor` invocations to the repo-local release binary
- preserves per-step and per-test logs
- produces a Markdown failure report
