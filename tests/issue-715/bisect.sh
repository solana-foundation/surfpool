#!/usr/bin/env bash
set -u

# Exit codes follow `git bisect run` conventions:
#   0   good: the clone exists when Anchor's readiness check completes
#   1   bad:  the readiness race was reproduced
#   125 skip: this revision or local environment could not run the test

fixture_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 125

rpc_port="${SURFPOOL_ISSUE_715_RPC_PORT:-18899}"
ws_port="${SURFPOOL_ISSUE_715_WS_PORT:-18900}"
remote_port="${SURFPOOL_ISSUE_715_REMOTE_PORT:-18898}"
clone_delay_ms="${SURFPOOL_ISSUE_715_CLONE_DELAY_MS:-8000}"
timeout_ms="${SURFPOOL_ISSUE_715_TIMEOUT_MS:-30000}"
build_profile="${SURFPOOL_ISSUE_715_BUILD_PROFILE:-debug}"
clone_address="AqH29mZfQFgRpfwaPoTMWSKJ5kqauoc1FwVBRksZyQrt"

case "$build_profile" in
  debug)
    cargo_profile_args=()
    surfpool_bin="$repo_root/target/debug/surfpool"
    ;;
  release)
    cargo_profile_args=(--release)
    surfpool_bin="$repo_root/target/release/surfpool"
    ;;
  *)
    echo "SKIP: SURFPOOL_ISSUE_715_BUILD_PROFILE must be debug or release" >&2
    exit 125
    ;;
esac

run_dir="$(mktemp -d "${TMPDIR:-/tmp}/surfpool-issue-715.XXXXXX")" || exit 125
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
    if ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid" 2>/dev/null || true
      return
    fi
    sleep 0.1
  done

  kill -TERM "$pid" 2>/dev/null || true
  for _ in {1..10}; do
    if ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid" 2>/dev/null || true
      return
    fi
    sleep 0.1
  done

  kill -KILL "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

cleanup() {
  stop_process "$surfpool_pid"
  stop_process "$mock_pid"
  rm -rf "$run_dir"
}
trap cleanup EXIT INT TERM

if ! command -v node >/dev/null 2>&1; then
  echo "SKIP: Node.js 18 or newer is required" >&2
  exit 125
fi

if ! cp "$fixture_dir/Anchor.toml" "$run_dir/Anchor.toml"; then
  echo "SKIP: unable to prepare the Anchor fixture" >&2
  exit 125
fi

# Keep the fixture's datasource URL aligned when callers override the default
# remote port.
if [[ "$remote_port" != "18898" ]]; then
  sed -i.bak "s/127\\.0\\.0\\.1:18898/127.0.0.1:${remote_port}/" "$run_dir/Anchor.toml"
fi

echo "Building Surfpool at $(git rev-parse --short HEAD) ($build_profile)"
if ! NO_DNA=1 cargo build --bin surfpool "${cargo_profile_args[@]}"; then
  echo "SKIP: Surfpool did not build at this revision" >&2
  exit 125
fi

node "$fixture_dir/delayed-rpc.mjs" "$remote_port" "$clone_delay_ms" \
  >"$run_dir/mock-rpc.log" 2>&1 &
mock_pid=$!

for _ in {1..50}; do
  if ! kill -0 "$mock_pid" 2>/dev/null; then
    echo "SKIP: delayed RPC exited during startup" >&2
    sed -n '1,160p' "$run_dir/mock-rpc.log" >&2
    exit 125
  fi
  if grep -q "delayed RPC listening" "$run_dir/mock-rpc.log"; then
    break
  fi
  sleep 0.1
done

if ! grep -q "delayed RPC listening" "$run_dir/mock-rpc.log"; then
  echo "SKIP: delayed RPC did not start" >&2
  sed -n '1,160p' "$run_dir/mock-rpc.log" >&2
  exit 125
fi

(
  cd "$run_dir" || exit 125
  NO_DNA=1 "$surfpool_bin" start \
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

node "$fixture_dir/assert-anchor-ready.mjs" \
  "http://127.0.0.1:${rpc_port}" \
  "$clone_address" \
  "$timeout_ms"
result=$?

if [[ "$result" -eq 125 ]] || ! kill -0 "$surfpool_pid" 2>/dev/null; then
  echo "Surfpool log:" >&2
  sed -n '1,240p' "$run_dir/surfpool.log" >&2
  echo "Delayed RPC log:" >&2
  sed -n '1,160p' "$run_dir/mock-rpc.log" >&2
  exit 125
fi

exit "$result"
