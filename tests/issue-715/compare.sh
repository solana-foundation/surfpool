#!/usr/bin/env bash
set -u

# Runs the readiness-race reproducer against two revisions and prints a
# verdict for each, to demonstrate the race before the fix and its absence
# after.
#
#   usage: tests/issue-715/compare.sh [before-rev] [after-rev]
#   default: main HEAD
#
# Per revision the reproducer is: a mock Solana RPC that delays
# getMultipleAccounts, the surfpool binary built at that revision and pointed
# at it, and assert-anchor-ready.mjs mirroring Anchor's readiness algorithm
# against the result. That checker exits 0 when the clone is present at
# readiness, 1 when readiness precedes it, and 125 when the revision or the
# environment could not be tested.
#
# This script's own exit status is about the demonstration rather than either
# run: 0 when the before revision reproduced the race and the after revision
# did not, 1 when that expectation was not met, and 125 when a run could not
# be evaluated.
#
# Ports and timings can be moved out of the way of a surfnet already running
# locally: SURFPOOL_ISSUE_715_RPC_PORT, _WS_PORT, _REMOTE_PORT,
# _CLONE_DELAY_MS, _TIMEOUT_MS, and _BUILD_PROFILE (debug or release).

harness_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(git -C "$harness_dir" rev-parse --show-toplevel 2>/dev/null)" || {
  echo "SKIP: not inside a git repository" >&2
  exit 125
}

before_rev="${1:-main}"
after_rev="${2:-HEAD}"

rpc_port="${SURFPOOL_ISSUE_715_RPC_PORT:-18899}"
ws_port="${SURFPOOL_ISSUE_715_WS_PORT:-18900}"
remote_port="${SURFPOOL_ISSUE_715_REMOTE_PORT:-18898}"
clone_delay_ms="${SURFPOOL_ISSUE_715_CLONE_DELAY_MS:-8000}"
timeout_ms="${SURFPOOL_ISSUE_715_TIMEOUT_MS:-30000}"
build_profile="${SURFPOOL_ISSUE_715_BUILD_PROFILE:-debug}"
clone_address="AqH29mZfQFgRpfwaPoTMWSKJ5kqauoc1FwVBRksZyQrt"

case "$build_profile" in
  debug) cargo_profile_args=() ;;
  release) cargo_profile_args=(--release) ;;
  *)
    echo "SKIP: SURFPOOL_ISSUE_715_BUILD_PROFILE must be debug or release" >&2
    exit 125
    ;;
esac

command -v node >/dev/null 2>&1 || {
  echo "SKIP: Node.js 18 or newer is required" >&2
  exit 125
}

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/surfpool-issue-715.XXXXXX")" || exit 125
worktrees=()
mock_pid=""
surfpool_pid=""

stop_process() {
  local pid="$1"
  if [[ -z "$pid" ]] || ! kill -0 "$pid" 2>/dev/null; then
    return
  fi

  # Surfpool installs an interrupt handler, while some historical revisions do
  # not exit on SIGTERM. Give SIGINT a short grace period before escalating.
  kill -INT "$pid" 2>/dev/null || true
  for _ in {1..20}; do
    kill -0 "$pid" 2>/dev/null || { wait "$pid" 2>/dev/null || true; return; }
    sleep 0.1
  done

  kill -TERM "$pid" 2>/dev/null || true
  for _ in {1..10}; do
    kill -0 "$pid" 2>/dev/null || { wait "$pid" 2>/dev/null || true; return; }
    sleep 0.1
  done

  kill -KILL "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

cleanup() {
  stop_process "$surfpool_pid"
  stop_process "$mock_pid"
  for worktree in "${worktrees[@]:-}"; do
    [[ -n "$worktree" ]] || continue
    git -C "$repo_root" worktree remove --force "$worktree" >/dev/null 2>&1 || true
  done
  rm -rf "$work_dir"
}
trap cleanup EXIT INT TERM

describe() {
  git -C "$repo_root" log -1 --format='%h %s' "$1" 2>/dev/null
}

verdict() {
  case "$1" in
    0) echo "GOOD (clone installed before readiness)" ;;
    1) echo "BAD (readiness preceded the clone)" ;;
    125) echo "SKIP (revision or environment could not be tested)" ;;
    *) echo "UNKNOWN (exit $1)" ;;
  esac
}

# Starts the mock datasource and a surfnet against it, runs the checker, and
# returns its verdict. The caller owns the binary; this owns the processes.
run_reproducer() {
  local binary="$1" label="$2"
  local run_dir="$work_dir/run-$label"
  mkdir -p "$run_dir" || return 125

  cp "$harness_dir/Anchor.toml" "$run_dir/Anchor.toml" || return 125
  if [[ "$remote_port" != "18898" ]]; then
    sed -i.bak "s/127\\.0\\.0\\.1:18898/127.0.0.1:${remote_port}/" "$run_dir/Anchor.toml"
  fi

  node "$harness_dir/delayed-rpc.mjs" "$remote_port" "$clone_delay_ms" \
    >"$run_dir/mock-rpc.log" 2>&1 &
  mock_pid=$!

  for _ in {1..50}; do
    kill -0 "$mock_pid" 2>/dev/null || break
    grep -q "delayed RPC listening" "$run_dir/mock-rpc.log" && break
    sleep 0.1
  done
  if ! grep -q "delayed RPC listening" "$run_dir/mock-rpc.log"; then
    echo "SKIP: delayed RPC did not start" >&2
    sed -n '1,160p' "$run_dir/mock-rpc.log" >&2
    return 125
  fi

  (
    cd "$run_dir" || exit 125
    NO_DNA=1 "$binary" start \
      --offline \
      --rpc-url "http://127.0.0.1:${remote_port}" \
      --port "$rpc_port" \
      --ws-port "$ws_port" \
      --no-tui \
      --no-studio \
      --disable-instruction-profiling \
      --max-profiles 1 \
      --log-level debug \
      --block-production-mode transaction \
      --legacy-anchor-compatibility \
      --yes
  ) >"$run_dir/surfpool.log" 2>&1 &
  surfpool_pid=$!

  node "$harness_dir/assert-anchor-ready.mjs" \
    "http://127.0.0.1:${rpc_port}" \
    "$clone_address" \
    "$timeout_ms"
  local result=$?

  if [[ "$result" -eq 125 ]] || ! kill -0 "$surfpool_pid" 2>/dev/null; then
    echo "Surfpool log:" >&2
    sed -n '1,240p' "$run_dir/surfpool.log" >&2
    echo "Delayed RPC log:" >&2
    sed -n '1,160p' "$run_dir/mock-rpc.log" >&2
    result=125
  fi

  stop_process "$surfpool_pid"; surfpool_pid=""
  stop_process "$mock_pid"; mock_pid=""
  return $result
}

run_rev() {
  local rev="$1" label="$2"
  local worktree="$work_dir/wt-$label"

  if ! git -C "$repo_root" worktree add --detach "$worktree" "$rev" \
      >"$work_dir/$label-worktree.log" 2>&1; then
    echo "SKIP: could not check out $rev" >&2
    sed -n '1,40p' "$work_dir/$label-worktree.log" >&2
    return 125
  fi
  worktrees+=("$worktree")

  # Build into the primary target directory rather than the worktree's own, so
  # each revision rebuilds the few workspace crates that differ instead of the
  # whole dependency graph.
  echo "  building $rev ($build_profile)"
  if ! CARGO_TARGET_DIR="$repo_root/target" NO_DNA=1 cargo build \
      --manifest-path "$worktree/Cargo.toml" \
      --bin surfpool "${cargo_profile_args[@]}" \
      >"$work_dir/$label-build.log" 2>&1; then
    echo "SKIP: surfpool did not build at $rev" >&2
    tail -n 25 "$work_dir/$label-build.log" >&2
    return 125
  fi

  run_reproducer "$repo_root/target/$build_profile/surfpool" "$label"
}

echo "before: $(describe "$before_rev")"
run_rev "$before_rev" before
before_code=$?
echo "  -> $(verdict "$before_code")"
echo

echo "after:  $(describe "$after_rev")"
run_rev "$after_rev" after
after_code=$?
echo "  -> $(verdict "$after_code")"
echo

if [[ "$before_code" -eq 125 || "$after_code" -eq 125 ]]; then
  echo "INCONCLUSIVE: at least one revision could not be tested"
  exit 125
fi

if [[ "$before_code" -eq 1 && "$after_code" -eq 0 ]]; then
  echo "DEMONSTRATED: $before_rev reproduces the race, $after_rev does not"
  exit 0
fi

echo "UNEXPECTED: wanted BAD before and GOOD after, got $(verdict "$before_code") and $(verdict "$after_code")"
exit 1
