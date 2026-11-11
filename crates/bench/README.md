# surfpool-bench

Throughput benchmarks for surfpool that go through the public JSON-RPC
interface. The tool launches a real `surfpool` binary with `surfpool start
--offline`, seeds state with the same RPC methods and cheatcodes a user would
call, and measures what a client observes. Nothing links against the surfnet.

CI runs it on every pull request and fails the `Benchmarks` check when a gated
metric regresses more than 15% against the base branch, measured on the same
machine in the same job. See [CI](#ci) for how that is wired.

## Scenarios

Each scenario starts a fresh `surfpool` process, runs a 2 second warm-up, then
measures for 10 seconds. The gated metric is the one that fails CI; the others
are reported to help diagnose a regression. A scenario aborts, and the run
exits with an error, if any request fails or any transaction fails or never
confirms, so a misbehaving run cannot pass as a slow one.

| Scenario | Load | Gated metric | Also reported |
|---|---|---|---|
| `send_tx_default` | 16 payers send system transfers to fresh recipients as fast as `sendTransaction` accepts them, preflight on, instruction profiling on (the defaults). A confirmer polls `getSignatureStatuses` until each transaction is confirmed. | `tps_confirmed`: confirmed transactions per second from the first send to the last confirmation | send rate, send latency p50/p99, send-to-confirm latency p50 |
| `send_tx_no_profiling` | Same load with `--disable-instruction-profiling`. Separates a regression in the SVM or ingestion path from one in profiling. | `tps_confirmed` | same as above |
| `reads` | 16 clients mix `getAccountInfo` (50%), `getBalance` (25%) and `getMultipleAccounts` of 20 keys (25%) over 2,000 accounts seeded with `surfnet_setAccount`. | `rps`: requests per second | p50 latency per method |
| `gpa` | 4 clients call `getProgramAccounts` with base64 encoding over a program that owns 3,000 accounts of 128 bytes. | `rps` | latency p50/p99 |

Every count, concurrency and duration is a flag; run `surfpool-bench run
--help` for the list. Two result files can only be compared when they were
measured with the same flags.

## Running locally

Build release binaries of surfpool and the bench tool. Debug builds of surfpool
are more than ten times slower on the transaction path and are not meaningful.

```sh
cargo build --release -p surfpool-cli -p surfpool-bench
```

Measure one binary:

```sh
target/release/surfpool-bench run \
  --surfpool-bin target/release/surfpool \
  --out results.json --rounds 3
```

Compare two binaries the way CI does. Rounds are interleaved and the side that
goes first alternates, so slow drift on the machine does not favour one side.
The command writes `base.json`, `head.json` and `summary.md` into the output
directory and exits 1 if a gated metric regressed past the threshold or was
measured on only one side.

```sh
git worktree add /tmp/surfpool-base main
(cd /tmp/surfpool-base && cargo build --release -p surfpool-cli)

target/release/surfpool-bench ab \
  --base-bin /tmp/surfpool-base/target/release/surfpool \
  --head-bin target/release/surfpool \
  --rounds 3 --threshold 0.15 --out-dir bench-results
```

Compare result files produced earlier, for example two artifacts downloaded
from CI. Several files per side are pooled, so three single-round files equal
one three-round file:

```sh
target/release/surfpool-bench compare --base base.json --head head.json
```

For a quick check that everything works, shrink the load:

```sh
target/release/surfpool-bench run --surfpool-bin target/release/surfpool \
  --out /tmp/smoke.json --duration-secs 2 --warmup-secs 1 \
  --read-accounts 200 --gpa-accounts 200
```

## Result files

`run` and `ab` write JSON with a `schema_version`, the surfpool version, the
commit when the caller passed one, the host, the load configuration, and one
entry per round holding every sample:

```json
{
  "scenario": "send_tx_default",
  "metric": "tps_confirmed",
  "unit": "tx/s",
  "value": 7421.3,
  "direction": "higher_is_better",
  "gate": true
}
```

`compare` takes the median of each metric across all rounds on each side. A
gated metric regresses when the head median is worse than the base median by
more than the threshold. Improvements never fail. The Markdown report shows the
median and the range over rounds for both sides, so noise is visible next to
the verdict.

`export` converts result files to the `customBiggerIsBetter` format of
[github-action-benchmark](https://github.com/benchmark-action/github-action-benchmark)
for the trend history. Only higher-is-better metrics are exported because that
format carries one direction per file.

## CI

`.github/workflows/benchmark.yml` has two jobs.

**Pull requests.** The job checks out the PR merged into its base branch,
builds `surfpool` from that and from the base branch tip into one shared cargo
target directory, builds `surfpool-bench` from the PR, and runs `ab` with three
interleaved rounds. The Markdown report goes to the job summary and the JSON
files are uploaded as the `benchmark-results` artifact. The job fails when a
gated metric regresses more than `BENCH_THRESHOLD` (15%). Pull requests from
forks are skipped, because the job is meant to run on a self-hosted machine and
must not execute untrusted code there.

**Pushes to main.** The job benchmarks the new tip with three rounds, uploads
the result, and appends the gated metrics to the `bench/` directory of the
`gh-pages` branch with github-action-benchmark, which renders trend charts at
`https://solana-foundation.github.io/surfpool/bench/`. The branch is created
on first use. This history is informational; only the pull request job gates.

A new push to a pull request cancels the bench already running for it.

### Runner

The jobs run on the runner named by the `BENCH_RUNS_ON` repository variable,
a JSON array of labels. While the variable is unset they fall back to
GitHub-hosted `ubuntu-latest`, which works but is noisy and slow. To move them
to the dedicated bare-metal machine:

1. Register a self-hosted runner on the repository with the labels
   `self-hosted`, `linux`, `x64`, `surfpool-bench`. Configure it to run one job
   at a time; the benchmark assumes it has the machine to itself.
2. Install `rustup` (the pinned toolchain in `rust-toolchain` installs itself on
   first use), `git`, `jq`, a C toolchain, and the build dependencies the main
   workflow installs: `libudev-dev clang libclang-dev`.
3. Set the repository variable `BENCH_RUNS_ON` to
   `["self-hosted","linux","x64","surfpool-bench"]`.

The cargo target directory lives next to the checkout
(`../surfpool-bench-target`) so it survives the clean step of
`actions/checkout` and keeps release builds incremental across jobs.

### Tuning the gate

The threshold is `BENCH_THRESHOLD` in the workflow. The ranges in the summary
table show the round-to-round spread on the machine; if gated metrics routinely
spread by more than a few percent, raise the rounds (`BENCH_ROUNDS`) or the
duration (`BENCH_DURATION_SECS`) before tightening the threshold. Both can be
overridden for one run from the workflow's manual trigger.
