# Surfnet startup lifecycle

Startup is modeled as two state machines:

- the **plan lifecycle**, which determines whether startup work has been
  defined;
- the **task lifecycle**, which tracks each declared startup task.

`startup.phase` is a projection of those machines. It is derived from the
current state and is never stored.

## Plan lifecycle

The startup plan defines the required work. Once sealed, its task set is fixed.

<!-- BEGIN GENERATED: plan-lifecycle -->
| State                                      | Event                                                   | New state                                                                                      |
|--------------------------------------------|---------------------------------------------------------|------------------------------------------------------------------------------------------------|
| [Unsealed][SurfnetStartupStatus::Planning] | [StartupPlanSealed(tasks)][StartupTransition::SealPlan] | [Sealed][SurfnetStartupStatus::Sealed], every task [Pending][SurfnetStartupTaskState::Pending] |
| [Unsealed][SurfnetStartupStatus::Planning] | [StartupFailed][StartupTransition::FailPlanning]        | [PlanningFailed][SurfnetStartupStatus::PlanningFailed]                                         |
<!-- END GENERATED: plan-lifecycle -->

Properties:

- A plan may be sealed at most once.
- Repeated task kinds in a sealed payload collapse to one entry.
- An empty sealed plan is immediately ready.
- An unsealed plan cannot become ready.

## Task lifecycle

Each task declared by the sealed plan follows this lifecycle.

<!-- BEGIN GENERATED: task-lifecycle -->
| State                                                                                    | Event                                                   | New state                                       |
|------------------------------------------------------------------------------------------|---------------------------------------------------------|-------------------------------------------------|
| [Pending][SurfnetStartupTaskState::Pending]                                              | [StartupTaskStarted][StartupTransition::StartTask]      | [Running][SurfnetStartupTaskState::Running]     |
| [Running][SurfnetStartupTaskState::Running]                                              | [StartupTaskSucceeded][StartupTransition::CompleteTask] | [Succeeded][SurfnetStartupTaskState::Succeeded] |
| [Pending][SurfnetStartupTaskState::Pending], [Running][SurfnetStartupTaskState::Running] | [StartupTaskFailed][StartupTransition::FailTask]        | [Failed][SurfnetStartupTaskState::Failed]       |
<!-- END GENERATED: task-lifecycle -->

Properties:

- `Succeeded` and `Failed` are terminal.
- A task may fail before it starts.
- A task may not succeed before it starts.
- Task events are accepted only for tasks declared in the sealed plan.
- Any transition not listed above is rejected, leaving the state unchanged.

## Phase projection

`startup.phase` summarizes the current startup state.

<!-- BEGIN GENERATED: projection -->
| State                            | Phase                                               | Meaning                                                                                 |
|----------------------------------|-----------------------------------------------------|-----------------------------------------------------------------------------------------|
| `Planning`                       | [`planning`][SurfnetStartupPhase::Planning]         | The required task set is still being computed.                                          |
| `PlanningFailed { error }`       | [`failed`][SurfnetStartupPhase::Failed]             | Planning failed before a plan was sealed.                                               |
| `Sealed`, any task `Failed`      | [`failed`][SurfnetStartupPhase::Failed]             | A required startup task failed.                                                         |
| `Sealed`, every task `Succeeded` | [`ready`][SurfnetStartupPhase::Ready]               | All required work has completed. An empty sealed plan reaches this state immediately.   |
| `Sealed`, clones outstanding     | [`initializing`][SurfnetStartupPhase::Initializing] | Account hydration is still in progress.                                                 |
| `Sealed`, deployment outstanding | [`deploying`][SurfnetStartupPhase::Deploying]       | Hydration has completed (or was unnecessary); deployment is the remaining startup work. |
<!-- END GENERATED: projection -->

Multiple states may project to the same phase. For example, clone tasks that
are `Pending` and `Running` both project to `initializing`.

Likewise, `failed` has two distinct causes:

- planning failed before the plan was sealed;
- a startup task failed after sealing.

The wire format exposes both the projected phase and the underlying state
(`phase`, `planSealed`, `tasks`, and `error`), allowing clients to present
either a high-level summary or detailed task progress without inconsistency.

## Phase transitions

The phase graph below is derived by exhaustively exploring every reachable
state.

<!-- BEGIN GENERATED: observed -->
| Phase                                             | Accepts                                                                                                                                                       | Can lead to                                                                                                                                                                |
|---------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| [Planning][SurfnetStartupPhase::Planning]         | [StartupFailed][StartupTransition::FailPlanning], [StartupPlanSealed][StartupTransition::SealPlan]                                                            | [Deploying][SurfnetStartupPhase::Deploying], [Failed][SurfnetStartupPhase::Failed], [Initializing][SurfnetStartupPhase::Initializing], [Ready][SurfnetStartupPhase::Ready] |
| [Initializing][SurfnetStartupPhase::Initializing] | [StartupTaskFailed][StartupTransition::FailTask], [StartupTaskStarted][StartupTransition::StartTask], [StartupTaskSucceeded][StartupTransition::CompleteTask] | [Deploying][SurfnetStartupPhase::Deploying], [Failed][SurfnetStartupPhase::Failed], [Initializing][SurfnetStartupPhase::Initializing], [Ready][SurfnetStartupPhase::Ready] |
| [Deploying][SurfnetStartupPhase::Deploying]       | [StartupTaskFailed][StartupTransition::FailTask], [StartupTaskStarted][StartupTransition::StartTask], [StartupTaskSucceeded][StartupTransition::CompleteTask] | [Deploying][SurfnetStartupPhase::Deploying], [Failed][SurfnetStartupPhase::Failed], [Ready][SurfnetStartupPhase::Ready]                                                    |
| [Ready][SurfnetStartupPhase::Ready]               | nothing                                                                                                                                                       | terminal                                                                                                                                                                   |
| [Failed][SurfnetStartupPhase::Failed]             | nothing                                                                                                                                                       | terminal                                                                                                                                                                   |

41 reachable states, 533 attempted transitions, 63 accepted.
<!-- END GENERATED: observed -->

A phase does not necessarily change when the underlying state changes. For
example, multiple clone tasks may advance while the phase remains
`initializing`.

`initializing` may also transition directly to `ready`: hydration and
deployment execute concurrently, so deployment may already have completed when
the final clone task succeeds.

## Implementation notes

- The implementation exposes task operations only after a plan has been
  sealed.
- `startup.phase` is computed on demand and is never stored.
- Exhaustive model checking verifies the phase projection against an
  independently written specification (the `spec` module beside the machine)
  and verifies that the compatibility projection presented to clients agrees
  with it.
- Every table in this document is generated and compared byte-for-byte: the
  lifecycle and projection tables render from the `spec` module, the phase
  transition table from the model-checking sweep. `cargo
  surfpool-update-startup-spec` regenerates them all; the surrounding prose
  is authored.

<!-- BEGIN GENERATED: links -->
[StartupTransition::CompleteTask]: StartupTransition::CompleteTask
[StartupTransition::FailPlanning]: StartupTransition::FailPlanning
[StartupTransition::FailTask]: StartupTransition::FailTask
[StartupTransition::SealPlan]: StartupTransition::SealPlan
[StartupTransition::StartTask]: StartupTransition::StartTask
[SurfnetStartupPhase::Deploying]: SurfnetStartupPhase::Deploying
[SurfnetStartupPhase::Failed]: SurfnetStartupPhase::Failed
[SurfnetStartupPhase::Initializing]: SurfnetStartupPhase::Initializing
[SurfnetStartupPhase::Planning]: SurfnetStartupPhase::Planning
[SurfnetStartupPhase::Ready]: SurfnetStartupPhase::Ready
[SurfnetStartupStatus::Planning]: SurfnetStartupStatus::Planning
[SurfnetStartupStatus::PlanningFailed]: SurfnetStartupStatus::PlanningFailed
[SurfnetStartupStatus::Sealed]: SurfnetStartupStatus::Sealed
[SurfnetStartupTaskState::Failed]: SurfnetStartupTaskState::Failed
[SurfnetStartupTaskState::Pending]: SurfnetStartupTaskState::Pending
[SurfnetStartupTaskState::Running]: SurfnetStartupTaskState::Running
[SurfnetStartupTaskState::Succeeded]: SurfnetStartupTaskState::Succeeded
<!-- END GENERATED: links -->
