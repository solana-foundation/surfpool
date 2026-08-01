# Issue 715 startup state transition table

Derived from the "Desired: a sealed startup plan governs readiness" model in
[event-model.md](./event-model.md), and matching the implementation in
`SurfnetStartupStatus` (crates/types/src/types.rs). The machine derives its
phase from the sealed task table, so several rows carry a guard: the same
event lands in different states depending on which tasks the sealed plan
requires and which have already succeeded.

Scope: phase-changing transitions only. Task starts (`StartStartupTask`,
Pending to Running) are accepted in Initializing and Deploying but never
change the phase, so they are listed separately below.

## Phase transitions

| State        | Transition event                  | Guard                        | New state    |
|--------------|-----------------------------------|------------------------------|--------------|
| Planning     | StartupPlanSealed                 | required = ∅                 | Ready        |
| Planning     | StartupPlanSealed                 | required includes clones     | Initializing |
| Planning     | StartupPlanSealed                 | required = {deployment} only | Deploying    |
| Planning     | StartupFailed                     | planning error               | Failed       |
|              |                                   |                              |              |
| Initializing | StartupTaskSucceeded (clones)     | deployment not yet succeeded | Deploying    |
| Initializing | StartupTaskSucceeded (clones)     | no deployment required       | Ready        |
| Initializing | StartupTaskFailed (any task)      |                              | Failed       |
|              |                                   |                              |              |
| Deploying    | StartupTaskSucceeded (deployment) |                              | Ready        |
| Deploying    | StartupTaskFailed (deployment)    |                              | Failed       |
|              |                                   |                              |              |
| Ready        | (none)                            | terminal                     |              |
| Failed       | (none)                            | terminal                     |              |

## Phase-preserving transitions

| State                   | Transition event                | Effect                             |
|-------------------------|---------------------------------|------------------------------------|
| Initializing            | StartupTaskStarted (clones)     | clones task Pending to Running     |
| Initializing, Deploying | StartupTaskStarted (deployment) | deployment task Pending to Running |

## Notes

- Event names follow the event model; the code spells them as commands:
  `SealStartupPlan`, `StartStartupTask`, `CompleteStartupTask` (which maps a
  task `Result` onto Succeeded or Failed), and `FailStartupPlanning`.
- The first row is the invariant the whole design hangs on, stated
  positively: a *sealed* empty plan is ready immediately, while an unsealed
  plan has no row that reaches Ready at all. Sealing closes the world:
  before it, an empty task list means "not yet known"; after, "nothing to
  do".
- Ready and Failed accept no further events; every command is rejected, and
  a rejected command leaves the state unchanged.
- This table is enforced mechanically: the exhaustive model check in
  `surfnet_startup_reachability_tests` (crates/types/src/types.rs) visits
  every reachable state and asserts the phase a spec oracle derives from
  these rules, so a divergence between this table and the code fails the
  test suite.
