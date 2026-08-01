# Issue 715 readiness-race reproducer

This harness checks whether Surfpool makes an Anchor-compatible readiness
promise before accounts declared with `[[test.validator.clone]]` have been
installed.

It starts a local mock Solana RPC that deliberately delays
`getMultipleAccounts`, starts the local Surfpool binary, and mirrors Anchor's
readiness algorithm:

1. Wait for `getLatestBlockhash`.
2. Wait until every execution returned by `surfnet_getSurfnetInfo` is complete.
3. Immediately request the configured clone from Surfpool.

When the clone is missing at readiness, the harness also waits for it to appear.
This distinguishes the readiness race from a broken fixture or a revision that
does not support configured cloning.

The harness returns:

- `0` when the clone is present at readiness ("good" for `git bisect`);
- `1` when readiness precedes the clone ("bad");
- `125` when the revision or environment cannot be tested.

Run it from the Surfpool repository:

```sh
tests/issue-715/bisect.sh
```

With the startup state machine fix applied, the current implementation is
expected to return `0`.

For a bisect, copy the harness outside the worktree first so it remains
available when Git checks out revisions that predate the test:

```sh
cp -R tests/issue-715 /tmp/surfpool-issue-715
git bisect start
git bisect bad
git bisect good <known-good-revision>
git bisect run /tmp/surfpool-issue-715/bisect.sh
```

The default ports and timing can be overridden with:

- `SURFPOOL_ISSUE_715_RPC_PORT`
- `SURFPOOL_ISSUE_715_WS_PORT`
- `SURFPOOL_ISSUE_715_REMOTE_PORT`
- `SURFPOOL_ISSUE_715_CLONE_DELAY_MS`
- `SURFPOOL_ISSUE_715_TIMEOUT_MS`
- `SURFPOOL_ISSUE_715_BUILD_PROFILE` (`debug` or `release`)

## Two scripts

`bisect.sh` tests one revision and returns `git bisect run` exit codes: `0`
good, `1` bad, `125` untestable. `compare.sh` builds two revisions and prints a
verdict for each, to show the race before the fix and its absence after.
