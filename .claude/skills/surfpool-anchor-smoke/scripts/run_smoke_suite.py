#!/usr/bin/env python3

from __future__ import annotations

import argparse
import datetime as dt
import os
import queue
import re
import shlex
import signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

TEST_CMD_RE = re.compile(r"^\s*-\s+cmd:\s*(.+?)\s*$")
CD_PREFIX_RE = re.compile(r"^cd\s+([^&]+?)\s+&&\s*(.+)$")
ANCHOR_WORD_RE = re.compile(r"(?<![\w./-])anchor(?![\w./-])")
ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
COMMAND_LITERAL_RE = re.compile(r'Command::new\(\s*"([^"\n]*surfpool[^"\n]*)"\s*\)')
FINDING_PATTERNS = [
    re.compile(r"panicked at", re.IGNORECASE),
    re.compile(r"\bAnchorError\b"),
    re.compile(r"\bAssertionError\b"),
    re.compile(r"Raw transaction .* failed \("),
    re.compile(r"Timeout of .* exceeded", re.IGNORECASE),
    re.compile(r"Unable to ", re.IGNORECASE),
    re.compile(r"\bException\b"),
    re.compile(r"^\s*Error:\s", re.IGNORECASE),
]
FINDING_EXCLUDE_PATTERNS = [
    re.compile(r"^error Command failed with exit code"),
    re.compile(r"^info Visit https://yarnpkg"),
    re.compile(r"^error Command failed with signal"),
]
MOCHA_PASSING_RE = re.compile(r"^\s*(\d+)\s+passing\s+\(([^)]+)\)\s*$")
MOCHA_FAILING_RE = re.compile(r"^\s*(\d+)\s+failing\s*$")
MOCHA_PENDING_RE = re.compile(r"^\s*(\d+)\s+pending\s*$")
DEFAULT_IDLE_TIMEOUT = 300
DEFAULT_TEST_TIMEOUT = 900

# Suites we skip by default because they poison the rest of the run.
# tests/anchor-cli-idl/test.sh launches `solana-test-validator --reset ... &`
# bound to localhost:8899. When `anchor test --skip-deploy --skip-local-validator`
# fails (which it does under our setup), `set -euo pipefail` exits before the
# trailing `kill $(jobs -p)`, leaving solana-test-validator alive. Surfpool spawns
# from later suites then either fail to bind 8899 or the Anchor client connects
# to whatever is already listening — every subsequent transaction returns
# "This program may not be used for executing instructions". The runner's
# process-group SIGTERM does not reliably reap the orphaned validator.
DEFAULT_SKIP_PATTERNS: list[str] = [
    "tests/anchor-cli-idl",
]


@dataclass
class StepResult:
    name: str
    original_command: str
    executed_command: str
    log_path: Path
    return_code: int
    status: str
    findings: list[str]
    tail_excerpt: list[str]
    duration_s: float = 0.0
    timeout_reason: str | None = None
    mocha_passing: int | None = None
    mocha_failing: int | None = None
    mocha_pending: int | None = None
    mocha_duration: str | None = None
    warnings: list[str] = field(default_factory=list)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build local Surfpool and Anchor, patch Anchor to use the local Surfpool install, run Anchor reusable tests serially, and write a markdown report.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument("--anchor-dir", default=None, help="Anchor repo root. Prompts when omitted.")
    parser.add_argument("--surfpool-dir", default=None, help="Surfpool repo root. Prompts when omitted.")
    parser.add_argument(
        "--workflow",
        default=".github/workflows/reusable-tests.yaml",
        help="Path to the reusable test workflow, relative to --anchor-dir unless absolute.",
    )
    parser.add_argument(
        "--report-root",
        default=None,
        help="Directory for timestamped logs and the markdown report. Defaults to <anchor-dir>/target/surfpool-anchor-smoke.",
    )
    parser.add_argument(
        "--local-surfpool-bin",
        default="~/.cargo/bin/surfpool",
        help="Surfpool binary path to inject into Anchor's start_surfpool_validator command.",
    )
    parser.add_argument(
        "--match",
        action="append",
        default=[],
        help="Only run commands whose original workflow command contains this text. Pass more than once to keep multiple subsets.",
    )
    parser.add_argument("--max-tests", type=int, default=None)
    parser.add_argument(
        "--skip",
        action="append",
        default=[],
        help="Drop commands whose original workflow text contains this term. Stacks with the built-in skip list.",
    )
    parser.add_argument(
        "--no-default-skips",
        action="store_true",
        help="Disable the built-in skip list (currently: tests/anchor-cli-idl, which leaks solana-test-validator into later suites).",
    )
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--list-tests", action="store_true")
    parser.add_argument("--skip-anchor-patch", action="store_true")
    parser.add_argument("--skip-surfpool-build", action="store_true")
    parser.add_argument("--skip-anchor-build", action="store_true")
    parser.add_argument("--skip-link-setup", action="store_true")
    parser.add_argument(
        "--test-timeout",
        type=int,
        default=DEFAULT_TEST_TIMEOUT,
        help="Kill any single test that runs longer than this many seconds. Use 0 to disable.",
    )
    parser.add_argument(
        "--idle-timeout",
        type=int,
        default=DEFAULT_IDLE_TIMEOUT,
        help="Kill any test that produces no output for this many seconds. Use 0 to disable.",
    )
    return parser.parse_args()


def expand_path(value: str) -> Path:
    return Path(os.path.expanduser(value)).resolve()


def ensure_exists(path: Path, description: str) -> None:
    if not path.exists():
        raise SystemExit(f"{description} does not exist: {path}")


def resolve_directory(cli_value: str | None, env_name: str, prompt: str, description: str) -> Path:
    candidate = cli_value or os.environ.get(env_name)

    while True:
        if candidate:
            path = expand_path(candidate)
            if path.exists():
                return path
            if not sys.stdin.isatty():
                raise SystemExit(f"{description} does not exist: {path}")
            print(f"{description} does not exist: {path}", file=sys.stderr)

        if not sys.stdin.isatty():
            raise SystemExit(f"Missing {description}. Pass it with the CLI flag or set {env_name}.")

        candidate = input(f"{prompt}: ").strip()
        if not candidate:
            raise SystemExit(f"Missing {description}.")


def extract_test_commands(workflow_path: Path) -> list[str]:
    commands: list[str] = []
    for line in workflow_path.read_text(encoding="utf-8").splitlines():
        match = TEST_CMD_RE.match(line)
        if match:
            commands.append(match.group(1).strip())
    if not commands:
        raise SystemExit(f"No test commands found in {workflow_path}")
    return commands


def filter_commands(
    commands: list[str],
    match_terms: list[str],
    skip_terms: list[str],
    max_tests: int | None,
) -> tuple[list[str], list[str]]:
    selected = commands
    if match_terms:
        lowered = [term.lower() for term in match_terms]
        selected = [cmd for cmd in commands if any(term in cmd.lower() for term in lowered)]
    skipped: list[str] = []
    if skip_terms:
        lowered_skips = [term.lower() for term in skip_terms]
        kept: list[str] = []
        for cmd in selected:
            if any(term in cmd.lower() for term in lowered_skips):
                skipped.append(cmd)
            else:
                kept.append(cmd)
        selected = kept
    if max_tests is not None:
        selected = selected[:max_tests]
    return selected, skipped


def rewrite_anchor_command(command: str, anchor_dir: Path) -> str:
    match = CD_PREFIX_RE.match(command.strip())
    if not match:
        return command

    relative_dir = match.group(1).strip()
    remainder = match.group(2).strip()
    command_dir = (anchor_dir / relative_dir).resolve()
    anchor_bin = (anchor_dir / "target" / "release" / "anchor").resolve()
    relative_anchor = os.path.relpath(anchor_bin, start=command_dir)
    rewritten = ANCHOR_WORD_RE.sub(shlex.quote(relative_anchor), remainder)
    return f"cd {relative_dir} && {rewritten}"


def detect_suite_name(command: str) -> str:
    match = CD_PREFIX_RE.match(command.strip())
    if match:
        return match.group(1).strip()
    return command.strip()


def strip_ansi(value: str) -> str:
    return ANSI_ESCAPE_RE.sub("", value)


def summarize_log(log_path: Path) -> tuple[list[str], list[str]]:
    lines = strip_ansi(log_path.read_text(encoding="utf-8", errors="replace")).splitlines()
    findings: list[str] = []
    seen: set[str] = set()

    for line in reversed(lines):
        stripped = line.strip()
        if not stripped:
            continue
        if any(pattern.search(stripped) for pattern in FINDING_EXCLUDE_PATTERNS):
            continue
        if any(pattern.search(stripped) for pattern in FINDING_PATTERNS):
            if stripped not in seen:
                findings.append(stripped)
                seen.add(stripped)
        if len(findings) >= 5:
            break

    if not findings:
        for line in reversed(lines):
            stripped = line.strip()
            if stripped:
                findings.append(stripped)
            if len(findings) >= 3:
                break

    tail = [line for line in lines[-12:] if line.strip()]
    return list(reversed(findings)), tail


def parse_mocha_summary(log_path: Path) -> tuple[int | None, int | None, int | None, str | None]:
    """Return (passing, failing, pending, last_duration). Last mocha summary wins for multi-suite runs."""
    try:
        lines = strip_ansi(log_path.read_text(encoding="utf-8", errors="replace")).splitlines()
    except OSError:
        return None, None, None, None

    passing: int | None = None
    failing: int | None = None
    pending: int | None = None
    duration: str | None = None

    for line in lines:
        match = MOCHA_PASSING_RE.match(line)
        if match:
            passing = (passing or 0) + int(match.group(1))
            duration = match.group(2).strip()
            continue
        match = MOCHA_FAILING_RE.match(line)
        if match:
            failing = (failing or 0) + int(match.group(1))
            continue
        match = MOCHA_PENDING_RE.match(line)
        if match:
            pending = (pending or 0) + int(match.group(1))

    return passing, failing, pending, duration


def format_duration(seconds: float) -> str:
    if seconds < 1:
        return f"{int(seconds * 1000)}ms"
    if seconds < 60:
        return f"{seconds:.1f}s"
    minutes, secs = divmod(int(seconds), 60)
    if minutes < 60:
        return f"{minutes}m{secs:02d}s"
    hours, minutes = divmod(minutes, 60)
    return f"{hours}h{minutes:02d}m"


def run_logged(
    command: str,
    cwd: Path,
    env: dict[str, str],
    log_path: Path,
    dry_run: bool,
    test_timeout: int = 0,
    idle_timeout: int = 0,
) -> tuple[int, str | None, float]:
    """Run a shell command in its own process group with optional timeouts.

    Returns (return_code, timeout_reason, duration_seconds). When a timeout fires, the entire
    process group is SIGTERMed (then SIGKILLed after 5s) so any leaked grandchildren — e.g.
    background validators that reparented to init — don't stall the runner.
    """
    print(f"$ (cd {cwd} && {command})")
    log_path.parent.mkdir(parents=True, exist_ok=True)
    start = time.monotonic()

    with log_path.open("w", encoding="utf-8") as handle:
        handle.write(f"$ (cd {cwd} && {command})\n\n")

        if dry_run:
            handle.write("[dry-run] command not executed\n")
            return 0, None, 0.0

        process = subprocess.Popen(
            ["/bin/bash", "-lc", command],
            cwd=str(cwd),
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
            start_new_session=True,
        )

        assert process.stdout is not None
        line_queue: "queue.Queue[str | None]" = queue.Queue()

        def reader() -> None:
            try:
                for line in process.stdout:  # type: ignore[union-attr]
                    line_queue.put(line)
            finally:
                line_queue.put(None)

        reader_thread = threading.Thread(target=reader, daemon=True)
        reader_thread.start()

        last_output = start
        timeout_reason: str | None = None

        while True:
            now = time.monotonic()
            if test_timeout and now - start >= test_timeout:
                timeout_reason = f"test timeout after {int(now - start)}s (limit {test_timeout}s)"
                break
            if idle_timeout and now - last_output >= idle_timeout:
                timeout_reason = f"idle timeout after {int(now - last_output)}s with no output (limit {idle_timeout}s)"
                break

            poll = 1.0
            if test_timeout:
                poll = min(poll, max(0.1, start + test_timeout - now))
            if idle_timeout:
                poll = min(poll, max(0.1, last_output + idle_timeout - now))

            try:
                item = line_queue.get(timeout=poll)
            except queue.Empty:
                continue

            if item is None:
                break

            sys.stdout.write(item)
            handle.write(item)
            last_output = time.monotonic()

        if timeout_reason is not None:
            kill_line = f"\n[runner] killed: {timeout_reason}\n"
            sys.stdout.write(kill_line)
            handle.write(kill_line)
            _terminate_process_group(process)
            while True:
                try:
                    item = line_queue.get(timeout=0.5)
                except queue.Empty:
                    break
                if item is None:
                    break
                sys.stdout.write(item)
                handle.write(item)

        return_code = process.wait()
        reader_thread.join(timeout=1.0)
        duration = time.monotonic() - start
        return return_code, timeout_reason, duration


def _terminate_process_group(process: subprocess.Popen) -> None:
    try:
        pgid = os.getpgid(process.pid)
    except ProcessLookupError:
        return

    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(pgid, sig)
        except (ProcessLookupError, PermissionError):
            return
        try:
            process.wait(timeout=5)
            return
        except subprocess.TimeoutExpired:
            continue


def find_function_body(contents: str, function_name: str) -> tuple[int, int]:
    function_match = re.search(rf"\bfn\s+{re.escape(function_name)}\s*\(", contents)
    if not function_match:
        raise ValueError(f"Could not find function {function_name} in cli/src/lib.rs")

    brace_start = contents.find("{", function_match.end())
    if brace_start == -1:
        raise ValueError(f"Could not locate the body of {function_name} in cli/src/lib.rs")

    depth = 0
    for index in range(brace_start, len(contents)):
        char = contents[index]
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return brace_start + 1, index

    raise ValueError(f"Could not parse the body of {function_name} in cli/src/lib.rs")


def find_surfpool_command_literal(contents: str) -> tuple[int, int, str, int]:
    body_start, body_end = find_function_body(contents, "start_surfpool_validator")
    body = contents[body_start:body_end]
    matches = list(COMMAND_LITERAL_RE.finditer(body))

    if not matches:
        raise ValueError(
            "Could not find Command::new(...surfpool...) inside start_surfpool_validator in cli/src/lib.rs"
        )
    if len(matches) > 1:
        raise ValueError(
            "Found multiple Command::new(...surfpool...) matches inside start_surfpool_validator in cli/src/lib.rs"
        )

    match = matches[0]
    literal_start = body_start + match.start(1)
    literal_end = body_start + match.end(1)
    line_number = contents.count("\n", 0, literal_start) + 1
    return literal_start, literal_end, match.group(1), line_number


def patch_anchor_cli(anchor_dir: Path, local_surfpool_bin: Path, log_path: Path, dry_run: bool) -> StepResult:
    cli_file = anchor_dir / "cli" / "src" / "lib.rs"
    ensure_exists(cli_file, "Anchor CLI source file")
    contents = cli_file.read_text(encoding="utf-8")
    literal_start, literal_end, current_literal, line_number = find_surfpool_command_literal(contents)
    target_literal = str(local_surfpool_bin)

    log_path.parent.mkdir(parents=True, exist_ok=True)
    with log_path.open("w", encoding="utf-8") as handle:
        handle.write("$ patch Anchor Surfpool command\n\n")
        handle.write(f"file: {cli_file}\n")
        handle.write(f"line: {line_number}\n")
        handle.write(f"current: {current_literal}\n")
        handle.write(f"target: {target_literal}\n")

        if current_literal == target_literal:
            handle.write("status: already-configured\n")
        elif dry_run:
            handle.write("status: would-patch\n")
        else:
            updated = contents[:literal_start] + target_literal + contents[literal_end:]
            cli_file.write_text(updated, encoding="utf-8")
            handle.write("status: patched\n")

    return make_step_result(
        name="patch-anchor-cli-surfpool-command",
        original_command="patch cli/src/lib.rs start_surfpool_validator Command::new(...surfpool...)",
        executed_command=f"set Command::new literal to {target_literal}",
        log_path=log_path,
        return_code=0,
        dry_run=dry_run,
    )


def make_step_result(
    name: str,
    original_command: str,
    executed_command: str,
    log_path: Path,
    return_code: int,
    dry_run: bool,
    duration_s: float = 0.0,
    timeout_reason: str | None = None,
    parse_mocha: bool = False,
) -> StepResult:
    findings, tail_excerpt = summarize_log(log_path)
    if dry_run:
        status = "dry-run"
    elif timeout_reason:
        status = "failed"
    elif return_code == 0:
        status = "passed"
    else:
        status = "failed"

    passing: int | None = None
    failing: int | None = None
    pending: int | None = None
    mocha_duration: str | None = None
    warnings: list[str] = []

    if parse_mocha and not dry_run:
        passing, failing, pending, mocha_duration = parse_mocha_summary(log_path)
        if status == "passed" and passing == 0 and not failing:
            warnings.append("mocha reported 0 passing tests despite exit 0 — no tests discovered?")
        if timeout_reason:
            warnings.append(timeout_reason)

    return StepResult(
        name=name,
        original_command=original_command,
        executed_command=executed_command,
        log_path=log_path,
        return_code=return_code,
        status=status,
        findings=findings,
        tail_excerpt=tail_excerpt,
        duration_s=duration_s,
        timeout_reason=timeout_reason,
        mocha_passing=passing,
        mocha_failing=failing,
        mocha_pending=pending,
        mocha_duration=mocha_duration,
        warnings=warnings,
    )


def make_failed_step_result(
    name: str,
    original_command: str,
    executed_command: str,
    log_path: Path,
    message: str,
) -> StepResult:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_path.write_text(f"{message}\n", encoding="utf-8")
    findings, tail_excerpt = summarize_log(log_path)
    return StepResult(
        name=name,
        original_command=original_command,
        executed_command=executed_command,
        log_path=log_path,
        return_code=1,
        status="failed",
        findings=findings,
        tail_excerpt=tail_excerpt,
    )


def render_report(
    report_path: Path,
    started_at: dt.datetime,
    anchor_dir: Path,
    surfpool_dir: Path,
    workflow_path: Path,
    selected_commands: list[str],
    setup_results: list[StepResult],
    test_results: list[StepResult],
    run_note: str | None = None,
    skipped_commands: list[str] | None = None,
) -> None:
    passed = sum(result.status == "passed" for result in test_results)
    failed = sum(result.status == "failed" for result in test_results)
    setup_failed = next((result for result in setup_results if result.status == "failed"), None)
    any_dry_run = any(result.status == "dry-run" for result in [*setup_results, *test_results])

    if run_note:
        overall_status = "interrupted"
    else:
        overall_status = "setup-failed" if setup_failed else ("dry-run" if any_dry_run else ("failed" if failed else "passed"))

    lines = [
        "# Surfpool Anchor Smoke Report",
        "",
        f"- Started: `{started_at.isoformat()}`",
        f"- Status: `{overall_status}`",
        f"- Anchor dir: `{anchor_dir}`",
        f"- Surfpool dir: `{surfpool_dir}`",
        f"- Workflow source: `{workflow_path}`",
        f"- Anchor binary: `{anchor_dir / 'target' / 'release' / 'anchor'}`",
        f"- Tests selected: `{len(selected_commands)}`",
        f"- Tests passed: `{passed}`",
        f"- Tests failed: `{failed}`",
        "",
    ]

    if run_note:
        lines.extend(["## Run Note", "", f"- {run_note}", ""])

    if skipped_commands:
        lines.extend(["## Skipped", ""])
        for cmd in skipped_commands:
            lines.append(f"- `{cmd}`")
        lines.append("")

    lines.extend(["## Setup", ""])

    for result in setup_results:
        lines.extend(
            [
                f"### {result.name}",
                f"- Status: `{result.status}`",
                f"- Exit code: `{result.return_code}`",
                f"- Log: `{result.log_path}`",
                "",
            ]
        )

    if setup_failed:
        lines.extend(
            [
                "## Setup Failure Findings",
                "",
                *[f"- {finding}" for finding in setup_failed.findings],
                "",
                "```text",
                *setup_failed.tail_excerpt,
                "```",
                "",
            ]
        )

    if test_results:
        lines.extend(
            [
                "## Results",
                "",
                "| # | Test | Status | Exit | Passing | Failing | Duration | Notes |",
                "| - | ---- | ------ | ---- | ------- | ------- | -------- | ----- |",
            ]
        )
        for index, result in enumerate(test_results, start=1):
            passing = "-" if result.mocha_passing is None else str(result.mocha_passing)
            failing = "-" if result.mocha_failing is None else str(result.mocha_failing)
            duration = format_duration(result.duration_s) if result.duration_s else "-"
            notes_parts: list[str] = []
            if result.timeout_reason:
                notes_parts.append(f"**timeout** ({result.timeout_reason})")
            notes_parts.extend(result.warnings)
            notes = "<br>".join(notes_parts) if notes_parts else ""
            lines.append(
                f"| {index} | `{result.name}` | {result.status} | `{result.return_code}` | {passing} | {failing} | {duration} | {notes} |"
            )
        lines.append("")

    lines.extend(["## Failed Tests", ""])

    failed_results = [result for result in test_results if result.status == "failed"]
    if not failed_results:
        lines.append("No test failures recorded.")
        lines.append("")
    else:
        for result in failed_results:
            lines.extend(
                [
                    f"### {result.name}",
                    f"- Exit code: `{result.return_code}`",
                    f"- Original command: `{result.original_command}`",
                    f"- Executed command: `{result.executed_command}`",
                    f"- Log: `{result.log_path}`",
                ]
            )
            if result.timeout_reason:
                lines.append(f"- Timeout: `{result.timeout_reason}`")
            if result.duration_s:
                lines.append(f"- Duration: `{format_duration(result.duration_s)}`")
            lines.append("- Findings:")
            lines.extend(f"  - {finding}" for finding in result.findings)
            lines.extend(["", "```text", *result.tail_excerpt, "```", ""])

    report_path.write_text("\n".join(lines), encoding="utf-8")


def main() -> int:
    args = parse_args()
    started_at = dt.datetime.now().astimezone()

    anchor_dir = resolve_directory(args.anchor_dir, "ANCHOR_DIR", "Enter the Anchor repo root", "Anchor repo")
    workflow_path = expand_path(args.workflow) if os.path.isabs(args.workflow) else (anchor_dir / args.workflow).resolve()
    ensure_exists(workflow_path, "Reusable test workflow")

    all_commands = extract_test_commands(workflow_path)
    skip_terms = list(args.skip)
    if not args.no_default_skips:
        skip_terms = list(DEFAULT_SKIP_PATTERNS) + skip_terms
    selected_commands, skipped_commands = filter_commands(
        all_commands, args.match, skip_terms, args.max_tests
    )
    if skipped_commands:
        print(f"Skipping {len(skipped_commands)} command(s) due to skip filters:")
        for cmd in skipped_commands:
            print(f"  - {cmd}")
    if not selected_commands:
        raise SystemExit("No workflow commands matched the current filters.")

    if args.list_tests:
        for index, command in enumerate(selected_commands, start=1):
            print(f"{index:02d}. {rewrite_anchor_command(command, anchor_dir)}")
        return 0

    surfpool_dir = resolve_directory(args.surfpool_dir, "SURFPOOL_DIR", "Enter the Surfpool repo root", "Surfpool repo")
    local_surfpool_bin = expand_path(args.local_surfpool_bin)
    report_root = expand_path(args.report_root) if args.report_root else (anchor_dir / "target" / "surfpool-anchor-smoke").resolve()
    run_dir = report_root / started_at.strftime("%Y%m%d-%H%M%S")
    logs_dir = run_dir / "logs"
    report_path = run_dir / "report.md"

    env = os.environ.copy()
    anchor_release_dir = str((anchor_dir / "target" / "release").resolve())
    env["PATH"] = f"{anchor_release_dir}:{env.get('PATH', '')}"

    setup_results: list[StepResult] = []
    test_results: list[StepResult] = []
    run_dir.mkdir(parents=True, exist_ok=True)

    if not args.skip_anchor_patch:
        patch_log = logs_dir / "patch-anchor-cli-surfpool-command.log"
        try:
            patch_result = patch_anchor_cli(anchor_dir, local_surfpool_bin, patch_log, args.dry_run)
        except Exception as exc:
            patch_result = make_failed_step_result(
                name="patch-anchor-cli-surfpool-command",
                original_command="patch cli/src/lib.rs start_surfpool_validator Command::new(...surfpool...)",
                executed_command=f"set Command::new literal to {local_surfpool_bin}",
                log_path=patch_log,
                message=str(exc),
            )

        setup_results.append(patch_result)
        if patch_result.status == "failed":
            render_report(report_path, started_at, anchor_dir, surfpool_dir, workflow_path, selected_commands, setup_results, test_results, skipped_commands=skipped_commands)
            print(f"Report written to {report_path}")
            return 1

    setup_plan: list[tuple[str, str, Path]] = []
    if not args.skip_surfpool_build:
        setup_plan.append(("build-surfpool", "cargo surfpool-install-dev", surfpool_dir))
    if not args.skip_anchor_build:
        setup_plan.append(("build-anchor-cli", "cargo build --release", anchor_dir / "cli"))
    if not args.skip_link_setup:
        setup_plan.extend(
            [
                ("link-borsh", "cd ts/packages/borsh && yarn --frozen-lockfile && yarn build && yarn link --force", anchor_dir),
                ("link-anchor-errors", "cd ts/packages/anchor-errors && yarn --frozen-lockfile && yarn build && yarn link --force", anchor_dir),
                ("link-anchor", "cd ts/packages/anchor && yarn --frozen-lockfile && yarn build:node && yarn link", anchor_dir),
                (
                    "link-spl-associated-token-account",
                    "cd ts/packages/spl-associated-token-account && yarn --frozen-lockfile && yarn build:node && yarn link",
                    anchor_dir,
                ),
                ("link-spl-token", "cd ts/packages/spl-token && yarn --frozen-lockfile && yarn build:node && yarn link", anchor_dir),
                ("link-tutorial", "cd examples/tutorial && yarn link @anchor-lang/core @anchor-lang/borsh && yarn --frozen-lockfile", anchor_dir),
                (
                    "link-tests",
                    "cd tests && yarn link @anchor-lang/core @anchor-lang/borsh @anchor-lang/spl-associated-token-account @anchor-lang/spl-token && yarn --frozen-lockfile",
                    anchor_dir,
                ),
            ]
        )

    total = len(selected_commands)

    def flush_report() -> None:
        render_report(report_path, started_at, anchor_dir, surfpool_dir, workflow_path, selected_commands, setup_results, test_results, skipped_commands=skipped_commands)

    try:
        for step_name, command, cwd in setup_plan:
            log_path = logs_dir / f"{step_name}.log"
            return_code, timeout_reason, duration_s = run_logged(command, cwd, env, log_path, args.dry_run)
            result = make_step_result(
                step_name,
                command,
                command,
                log_path,
                return_code,
                args.dry_run,
                duration_s=duration_s,
                timeout_reason=timeout_reason,
            )
            setup_results.append(result)
            flush_report()
            if result.status == "failed":
                print(f"Report written to {report_path}")
                return 1

        if not args.dry_run:
            anchor_binary = anchor_dir / "target" / "release" / "anchor"
            if not anchor_binary.exists():
                setup_results.append(
                    StepResult(
                        name="verify-anchor-binary",
                        original_command="verify target/release/anchor exists",
                        executed_command="verify target/release/anchor exists",
                        log_path=logs_dir / "build-anchor-cli.log",
                        return_code=1,
                        status="failed",
                        findings=[f"Missing expected anchor binary: {anchor_binary}"],
                        tail_excerpt=[f"Missing expected anchor binary: {anchor_binary}"],
                    )
                )
                flush_report()
                print(f"Report written to {report_path}")
                return 1

        for index, command in enumerate(selected_commands, start=1):
            suite_name = detect_suite_name(command)
            rewritten_command = rewrite_anchor_command(command, anchor_dir)
            log_name = f"test-{index:02d}-{suite_name.replace('/', '_')}.log"
            log_path = logs_dir / log_name

            print(f"::test-start:: {index:02d}/{total} {suite_name}", flush=True)
            return_code, timeout_reason, duration_s = run_logged(
                rewritten_command,
                anchor_dir,
                env,
                log_path,
                args.dry_run,
                test_timeout=args.test_timeout,
                idle_timeout=args.idle_timeout,
            )
            result = make_step_result(
                name=suite_name,
                original_command=command,
                executed_command=rewritten_command,
                log_path=log_path,
                return_code=return_code,
                dry_run=args.dry_run,
                duration_s=duration_s,
                timeout_reason=timeout_reason,
                parse_mocha=True,
            )
            test_results.append(result)
            passing = "-" if result.mocha_passing is None else str(result.mocha_passing)
            failing = "-" if result.mocha_failing is None else str(result.mocha_failing)
            duration = format_duration(duration_s) if duration_s else "-"
            timeout_note = f" timeout={timeout_reason}" if timeout_reason else ""
            print(
                f"::test-end:: {index:02d}/{total} {suite_name} status={result.status} rc={return_code} "
                f"passing={passing} failing={failing} duration={duration}{timeout_note}",
                flush=True,
            )
            flush_report()

    except KeyboardInterrupt:
        render_report(
            report_path,
            started_at,
            anchor_dir,
            surfpool_dir,
            workflow_path,
            selected_commands,
            setup_results,
            test_results,
            run_note="Run interrupted before the full test matrix completed.",
            skipped_commands=skipped_commands,
        )
        print(f"Report written to {report_path}")
        return 130

    render_report(report_path, started_at, anchor_dir, surfpool_dir, workflow_path, selected_commands, setup_results, test_results, skipped_commands=skipped_commands)
    print(f"Report written to {report_path}")

    if any(result.status == "failed" for result in test_results):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
