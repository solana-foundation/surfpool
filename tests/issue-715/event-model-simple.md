# Issue 715 startup event model (simplified)

This simplified analysis treats some states as a black box in order to explain the gist of the problem. Each pipeline of commands, processors, and events from the full diagram is folded into a single sub-machine state, so the only structure left visible is the ordering problem itself.

>Legend:
>
>- Composite state: a sub-machine hiding a whole command/event pipeline
>- Red: state that is observable too early, or a failure
>- Green: correctly gated public state

## Broken: readiness precedes project initialization

```mermaid
stateDiagram-v2
    [*] --> Starting

    state "Starting (two concurrent tracks, no join)" as Starting {
        [*] --> RpcUp
        RpcUp : core startup, RPC servers bound
        RpcUp --> LooksReady : SurfnetInfo has no runbooks,<br/>all([]) == true
        --
        [*] --> ProjectInit
        ProjectInit : inspect Anchor.toml,<br/>fetch clones from remote RPC,<br/>install (slow, fire-and-forget)
    }

    LooksReady --> TestsRunning : Anchor polls, observes ready
    TestsRunning --> CloneMissing : GetAccountInfo(clone)<br/>while ProjectInit is still running

    note right of Starting
        The lower track is still running when the
        upper track becomes publicly observable.
        Nothing joins them.
    end note

    classDef bad fill:#fee2e2,stroke:#dc2626,color:#7f1d1d,stroke-width:2px
    class LooksReady,CloneMissing bad
```

The full flowchart collapses to one fact: `Starting` contains two concurrent tracks and no join. `LooksReady` is reachable while `ProjectInit` is still running, because clone hydration is never represented in `SurfnetInfo` and Anchor's all-complete check over an empty runbook list incorrectly reports true.

## Desired: a sealed plan gates a single exit

"Sealed" here (and in the code: `seal_plan`, `planSealed`) means the required task set has been computed and can no longer change. Sealing closes the world: before the plan is sealed, an empty task list means "not yet known"; after, it means "nothing to do". That is why a sealed empty plan is legitimately ready while an unsealed plan never is.

```mermaid
stateDiagram-v2
    [*] --> Planning
    Planning : inspect project configuration,<br/>seal plan with required tasks<br/>(clones, deployment)
    Planning --> Initializing : StartupPlanSealed

    state "Initializing (plan tasks run concurrently)" as Initializing {
        [*] --> Hydrating
        Hydrating : fetch and install clones<br/>(atomic commit)
        Hydrating --> [*]
        --
        [*] --> Deploying
        Deploying : RPC serving for initialization,<br/>run deployment runbooks
        Deploying --> [*]
    }

    Initializing --> Ready : every required task succeeded
    Initializing --> Failed : any task errored

    Ready : first publicly observable state
    Failed : Anchor fails startup, tests never begin

    classDef good fill:#dcfce7,stroke:#15803d,color:#14532d,stroke-width:2px
    classDef bad fill:#fee2e2,stroke:#dc2626,color:#7f1d1d,stroke-width:2px
    class Ready good
    class Failed bad
```

The same sub-machines exist as in the broken model; what changed is the topology around them:

- `Planning` has exactly one exit, and it goes through `StartupPlanSealed`.
  An unsealed plan can never reach `Ready`, even when its task set is empty.
- The concurrent tracks now live *inside* `Initializing`, so leaving that
  state is a join: `Ready` requires every required task to have succeeded.
- RPC comes up inside `Initializing` (deployment runbooks need a live
  endpoint), but public readiness is the `Ready` state; external clients
  observe the lifecycle read model and wait for it.

- Compatibility: every in-flight phase is projected as one pending
  `surfpool-startup` entry in `runbookExecutions`, so Anchor versions
  that only inspect that list observe pending work until the lifecycle
  reaches `Ready`. On `Failed` the entry is projected as completed with
  `errors` populated instead: legacy Anchor's readiness loop has no
  timeout, so a forever-pending entry would starve it.

