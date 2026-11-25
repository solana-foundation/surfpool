# Pull request security signals

The `PR security` workflow highlights potentially sensitive changes at first
glance and reports dependency and payload risks. All findings are advisory:
matching a rule does not fail CI or block merging. A green check does not mean
a change is safe or API-compatible.

`.github/CODEOWNERS` automatically requests **@MicaiahReid** for sensitive paths,
including CI configuration, dependency manifests and lockfiles, build/install/
release scripts, public Rust and Node interfaces, RPC and HTTP handlers, signing
and transaction execution, storage, filesystem/process code, test assets,
minified files, and common binary formats. GitHub uses the CODEOWNERS file on
the PR's base branch, so ownership changes take effect after they merge.

## What reviewers see

Every PR gets two checks under **PR security**, visible in the PR's checks list
and **Checks** tab:

- **Sensitive changes** reports changed paths and their indicators in the Actions
  run summary, accessible through **Details**. Indicators include code ownership,
  sensitive API primitives, binary changes, and payload signals. Findings appear
  in the summary and job logs with file paths, line numbers where applicable,
  and reasons. Payload values are not printed. Findings leave the check green.
- **Dependency review** uses GitHub's dependency graph to report newly introduced
  known vulnerabilities, including development dependencies, as warnings. Results
  appear in the Actions summary and logs. It runs with `warn-only: true`.
  Coverage depends on GitHub's ecosystem support and graph ingestion; the
  sensitive-path summary flags manifests and locks independently.

Tool and checkout steps use `continue-on-error`. If a step fails, the workflow
adds an **analysis incomplete** warning and summary instead of treating missing
results as a clean scan. Platform-level failures, such as a runner being lost
or the entire job timing out, can still interrupt a workflow.

When a PR is ready for review, changes matching CODEOWNERS also request the
owner in the PR's **Reviewers** area. Required code-owner approval is a separate
GitHub review requirement, unaffected by CI being advisory. No PR comments or
labels are posted by this workflow.

## Deterministic indicators

The **Sensitive changes** job inspects committed changes against their merge base:

- Changed, added, or deleted lockfiles without a corresponding manifest change
  are reported. The root Cargo lock accepts the workspace manifest or a member
  listed in `.github/security/workspace-manifests.json`; excluded examples have
  independent locks. Update that trusted list when workspace membership changes.
  Node locks are paired with the sibling `package.json`. Python and Go lockfile
  names are also recognized. Lockfile-only transitive updates, including bot
  updates, produce findings without failing CI.
- Added text, including fixtures, is scanned for lines over 2,000 characters,
  encoded-looking tokens of at least 256 characters, tokens of at least 80
  characters with Shannon entropy of at least 4.5 bits per character, runs of
  eight hex/Unicode escapes, and selected dynamic execution/decoding primitives.
  Known lockfile formats are exempt from payload-string rules because integrity
  hashes are expected; ownership and dependency reporting still apply.
- Added sensitive API primitives are highlighted. Those outside the base
  CODEOWNERS policy also produce an ownership-coverage finding. Changes to
  ownership take effect after merging; no separate ownership PR is required
  by this advisory check.
- Binary blobs, symlinks, submodules, and new or removed executable files are
  highlighted, including unfamiliar filenames. Missing base-policy ownership
  is reported. Blobs over 10 MiB are reported as unscanned. Git blobs are read
  without executing filters or following symlinks.

Rust detection is textual, not behavioral. An added reference to `reqwest`,
`ureq`, `hyper`, or selected standard-library networking APIs is highlighted.
The checker does not establish whether a URL is malicious or whether a call runs
on startup. Calls through an existing client, an alias, or a wrapper can escape
API-token detection; a matching owned path still produces a path indicator.

These heuristics cannot detect all malicious code or interface-breaking changes.
Public interface paths stay owned regardless of token matches. False positives
need human triage. CODEOWNERS matching supports basename globs and literal rooted
paths/directories; unsupported syntax produces an incomplete-analysis report.

## Execution and local checks

The workflow runs on pull requests, including drafts, with a read-only
`GITHUB_TOKEN`, no configured secrets, and no persisted checkout credentials.
It fetches PR objects without checking out the PR tree or installing packages.
The checker, CODEOWNERS, and workspace manifest list come from the base revision.
During initial adoption only, when the base has no checker, the workflow loads
the proposed checker and policy files. That first PR needs full manual review.
First-time contributor approval applies to this workflow and existing build/test
workflows.

Both scripts use Node.js built-ins with no package installation. Run locally
with Node.js 22+ and both Git revisions available:

```sh
node --test .github/security/check-pr.test.mjs
node .github/security/check-pr.mjs <full-base-sha> <full-head-sha>
```

Outside Actions, the checker prints its summary to the terminal. Findings return
exit code zero. Unexpected execution errors return nonzero locally; CI catches
those step failures and reports incomplete analysis.

## GitHub settings

Keep these advisory checks optional in branch protection and rulesets. Existing
required PR review remains separate. CODEOWNERS adds automatic review requests
and, where code-owner review is required, specifies whose approval is needed.
It does not request an additional approval after the listed owner has approved.

Under Settings → Actions → General, use **Read repository contents and packages
permissions**, disable **Allow GitHub Actions to create and approve pull
requests**, and select **Require approval for first-time contributors** (not just
contributors new to GitHub). Keep write-token and secret access for fork runs
disabled. Existing job-scoped `contents: write` for publishing benchmark history/
releases and `id-token: write` for trusted publishing are deliberate exceptions.

Repository settings cannot be enforced by CODEOWNERS or workflow YAML alone.
The local admin gap report records the inspected settings and rollout steps.
GitHub documents [code-owner enforcement](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/about-code-owners),
[workflow approval settings](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/enabling-features-for-your-repository/managing-github-actions-settings-for-a-repository),
and the [dependency review action](https://github.com/actions/dependency-review-action).
