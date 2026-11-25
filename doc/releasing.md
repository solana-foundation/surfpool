# Releasing Surfpool

Releases are cut by merging a pull request. There is no manual version bump.

## Trains

| Channel | When | Where |
| --- | --- | --- |
| **edge** | Every merge to `main` | GitHub prerelease [`edge`](https://github.com/solana-foundation/surfpool/releases/tag/edge) (rolling), `surfpool/surfpool:edge`, and npm `@solana/surfpool@edge` when `NPM_EDGE_ENABLED` is on |
| **stable** | When the release PR is merged | GitHub release `vX.Y.Z` (latest), Docker `X.Y.Z`/`X.Y`/`X`/`latest`, crates.io, npm `latest` |

Edge versions look like `1.7.1-edge.740`: the patch after the last release,
then the number of commits on `main`. They sort above the last release and below
the next one.

Install the edge CLI:

```console
curl -sL https://run.surfpool.run/ | VERSION=edge bash
```

## Commit messages

Pull request titles must follow [Conventional Commits](https://www.conventionalcommits.org/).
Squash merges use the title as the commit on `main`, and the `PR Title` check
enforces the format. The type decides the next version:

- `fix:` bumps the patch version and `feat:` bumps the minor version.
- `feat!:`, or a `BREAKING CHANGE:` footer, bumps the major version.
- `perf:` and `revert:` appear in the changelog; `chore:`, `ci:`, `docs:`, `refactor:`, `test:` and `build:` do not.

## Cutting a release

[release-please](https://github.com/googleapis/release-please) keeps a PR named
`chore(release): publish vX.Y.Z` open. Each merge to `main` updates its version
and `CHANGELOG.md`. To release:

1. Review the release PR. To reword a changelog entry, add a
   `BEGIN_COMMIT_OVERRIDE` … `END_COMMIT_OVERRIDE` block to the body of the
   merged PR it came from; the release PR picks it up on the next run.
2. Approve and merge it.

From the merge commit, `release_please.yml` tags `vX.Y.Z` and opens a draft
GitHub release, attaches the binaries, publishes the release, and pushes Docker
images. `release_crates.yaml` and `release_sdk_node_npm.yml` see the new version
on `main` and wait for approval in the `release` environment before publishing.

The npm workflow publishes a stable version only from the push that changed it.
If that run fails, re-run its failed jobs, or run **Publish SDK Node Package**
manually on the release tag (e.g. `v1.8.0`). Packages that already published
are skipped.

To force a specific version, add a `Release-As: X.Y.Z` footer to a commit on `main`.

## Files

- `release-please-config.json` and `.release-please-manifest.json` configure
  release-please; the manifest holds the last released version.
- `.github/scripts/set-version.js <version>` rewrites the version in every
  manifest and lockfile. CI uses it to stamp edge builds.
- `.github/scripts/edge-version.sh` prints the edge version for `HEAD`.
