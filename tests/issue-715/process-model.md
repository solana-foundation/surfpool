# Issue 715 startup process model

**Why can a client trust that `Ready` really means ready?**

That is the only question this document answers. The event-model documents
([event-model-simple.md](./event-model-simple.md),
[event-model.md](./event-model.md)) describe the logical model; this one shows
why the physical arrangement of processes, threads and channels enforces it.

State the guarantee, show what a client observes, prove the guarantee, then
reveal the machinery. The appendices carry the broken ordering, the per-task
state machines, and the thread map, for readers who want them.

## The invariant

A client may observe `Ready` if and only if the startup plan has been sealed
and every required startup task has completed successfully.

```mermaid
flowchart TB
    seal["Plan sealed"]
    clones["clones"]
    deploy["deployment"]
    ready(["Ready"]):::good

    seal --> clones
    seal --> deploy
    clones --> ready
    deploy --> ready

    classDef good fill:#dcfce7,stroke:#15803d,color:#14532d,stroke-width:3px;
```

## What a client observes

The score axis is repurposed (my apologies to UX practitioners): it rates the
quality of what the client observes at that step, from 1 (a wrong answer) to 7
(truthful and final).

```mermaid
journey
    title Happy path: ready means ready
    section Startup
      Start surfpool: 5: Anchor
      Poll, see pending startup entry: 5: Anchor, Surfpool
      Hydration and deployment finish: 6: Surfpool, Remote
      Poll, observe Ready: 7: Anchor, Surfpool
    section Tests
      Request the clone, present: 7: Anchor, Surfpool
      Tests run against real state: 7: Anchor
```

```mermaid
journey
    title Unhappy path: failure is loud
    section Startup
      Start surfpool: 5: Anchor
      A required task fails: 2: Surfpool
      Entry completed with errors: 3: Anchor, Surfpool
      Headless surfnet aborts: 3: Surfpool
    section Tests
      Startup fails visibly, tests never begin: 4: Anchor
```

The unhappy path scoring a 4 at the end is deliberate: tests never beginning is
the correct outcome of a failed startup, and distinctly better than a confident
wrong answer.

## Proof: why `Ready` cannot happen early

Every edge below is a happens-before guarantee. If there is a path from A to B,
B cannot occur before A. The label names the primitive that provides it.

```mermaid
flowchart TB
    seal["plan sealed"]:::cut
    okC["clones succeeded"]:::cut
    okD["deployment succeeded"]:::cut
    ready["phase = Ready"]:::cut
    obs["client poll answers ready"]

    seal -- "mpsc FIFO" --> okC
    seal -- "mpsc FIFO" --> okD
    okC -- "join" --> ready
    okD -- "join" --> ready
    ready -- "read-guard projection" --> obs

    classDef cut fill:#dcfce7,stroke:#15803d,color:#14532d,stroke-width:2px
```

Every path to the client passes through the seal and through both task
successes. That green cut is total: no path reaches a client answer without
crossing it.

The intermediate steps (accounts arriving, the batch installing, tasks being
dispatched) are implementation, not proof. They appear in the appendices.

Five primitives carry those edges, each with a local and mechanical guarantee:

1. **The seal is a fence.** `SealStartupPlan` is a synchronous round trip, and
   every task command is sent after it on the same FIFO mpsc channel. The
   runloop can never observe work for an unsealed plan; the ordering argument
   is one channel's delivery order.
2. **One writer, no interleaving.** Only the runloop mutates startup state, so
   transitions serialize by construction, and the transition table in
   [state-table.md](./state-table.md) is checkable by exhaustive enumeration.
   There is no concurrent schedule to miss.
3. **Hydration is atomic.** The clone batch installs under a single write
   guard, so `Ready` can never expose a partially installed clone set.
4. **Readiness is level-triggered.** The watch channel publishes a value, not
   an event, so late subscribers read the current phase instead of hoping they
   subscribed before an edge.
5. **Clients see projections, never internals.** Both the `startup` field and
   the compatibility entry are computed from the lifecycle under a read guard
   at response time, which is what lets unmodified legacy Anchor benefit.

## One possible execution

A single schedule consistent with the graph above. Others exist; the ordering
constraints are what matter, not this particular interleaving.

```mermaid
sequenceDiagram
    participant anchor as Anchor CLI
    participant rpcsrv as RPC servers
    participant cli as CLI orchestration
    participant runloop as Runloop
    participant deployer as Deployment executor
    participant remote as mainnet / devnet

    anchor->>rpcsrv: getSurfnetInfo (polling starts immediately)
    rpcsrv-->>anchor: Planning, one pending surfpool-startup entry

    rect rgb(220, 252, 231)
        Note over cli,runloop: the seal, the one synchronous hop
        cli->>runloop: SealStartupPlan([RemoteAccounts, Deployment], reply_tx)
        runloop-->>cli: Ok, phase = Initializing
    end

    cli->>runloop: StartStartupTask + FetchRemoteAccounts
    cli->>deployer: spawn runbook futures

    runloop->>remote: getMultipleAccounts (HTTPS)
    remote-->>runloop: account batch
    Note over runloop: install batch under one write guard,<br/>CompleteStartupTask(RemoteAccounts, Ok)

    deployer->>rpcsrv: deployment transactions (loopback)
    deployer->>runloop: CompleteStartupTask(Deployment, Ok)
    Note over runloop: the join: phase = Ready

    anchor->>rpcsrv: getSurfnetInfo
    rpcsrv-->>anchor: Ready, no pending entries
    anchor->>rpcsrv: getAccountInfo(clone)
    rpcsrv-->>anchor: account present
```

The RPC servers answer throughout. Deployment runbooks need a live endpoint,
and their transactions loop back through the same servers clients use. What the
fix changed is what those servers say, never when they exist.

## Implementation

Where each piece lives:

| Piece                                                      | Location                           |
|------------------------------------------------------------|------------------------------------|
| Startup machine (`SurfnetStartupStatus`)                   | `crates/types/src/startup.rs`      |
| Planner, seal, watchdog                                    | `crates/cli/src/cli/simnet/mod.rs` |
| Command handling, remote fetch, atomic install             | `crates/core/src/runloops/mod.rs`  |
| Watch channel (`subscribe_startup_status`)                 | `crates/core/src/surfnet/svm.rs`   |
| Compat projection (`GetSurfnetInfoResponse::with_startup`) | `crates/types/src/startup.rs`      |

Three processes participate: the remote RPC that clones are fetched from, the
surfpool process, and the clients. SDK and MCP embedders run the core in their
own process instead, sealing an empty plan at construction, so the same
lifecycle applies with no tasks to wait for.

## Appendix A: the broken ordering

What the reported bug looked like, kept for comparison.

```mermaid
journey
    title Broken: readiness outruns the work
    section Startup
      Start surfpool: 5: Anchor
      Poll getSurfnetInfo: 5: Anchor, Surfpool
      See empty list, assume ready: 2: Anchor
    section Tests
      Request the clone: 1: Anchor, Surfpool
      Clone missing, tests fail: 1: Anchor
      Accounts arrive too late: 2: Surfpool, Remote
```

In the ordering notation, the bug was a missing edge.

```mermaid
flowchart TB
    core["core startup, RPC bound"]
    info["SurfnetInfo shows<br/>runbookExecutions = []"]
    obs["client poll answers ready"]:::bad
    inspect["inspect Anchor.toml"]
    fetch["fetch dispatched<br/>(fire and forget)"]
    inst["clones installed"]

    core --> info --> obs
    core --> inspect --> fetch -. "HTTPS" .-> inst
    inst -. "MISSING: nothing ordered<br/>installation before observation" .-> obs

    classDef bad fill:#fee2e2,stroke:#dc2626,color:#7f1d1d,stroke-width:2px
```

"Client poll answers ready" was reachable without passing through "clones
installed" at all. Everything in the fix exists to add ordering edges until the
cut is total.

## Appendix B: state machines for the synchronization tasks

### Per-task lifecycle

Each task named in the sealed plan runs this machine independently. The
submitter sends `StartStartupTask` before dispatching the work; the worker
reports the outcome through `CompleteStartupTask`, which maps a `Result` onto
the terminal states.

```mermaid
stateDiagram-v2
    [*] --> Pending : named in the sealed plan
    Pending --> Running : StartStartupTask
    Running --> Succeeded : CompleteStartupTask(Ok)
    Running --> Failed : CompleteStartupTask(Err)
    Succeeded --> [*]
    Failed --> [*]
```

The phase is derived from this table of task states plus the sealed flag; no
code sets the phase directly. The derivation rules are in
[state-table.md](./state-table.md), enforced by the exhaustive model check in
`surfnet_startup_reachability_tests`.

### Startup watchdog (headless only)

Legacy Anchor's readiness loop can perceive exactly two things: a completed
execution list and process death. So a headless surfnet whose startup failed
must die rather than serve forever. The TUI spawns no watchdog: an interactive
session displays the failure and stays alive.

```mermaid
stateDiagram-v2
    [*] --> Watching : spawn (headless only)
    Watching : watch channel wait_for(phase is terminal)
    Watching --> QuietExit : phase == Ready
    Watching --> Aborting : phase == Failed
    Aborting : send Aborted event,<br/>then Terminate command<br/>(graceful shutdown, WAL checkpoint)
    QuietExit --> [*]
    Aborting --> [*]
```

N.B. the watch channel is level-triggered: `wait_for` evaluates the current
value before waiting. A watchdog that subscribes after the machine already
reached a terminal phase still fires; there is no edge to miss.

### Legacy compatibility projection

A pure function of the phase, not a machine: every `getSurfnetInfo` response
recomputes it from the current lifecycle, so it can never lag or race.

| Lifecycle phase                   | Projected into `runbookExecutions`                                |
|-----------------------------------|-------------------------------------------------------------------|
| Planning, Initializing, Deploying | one pending `surfpool-startup` entry (`completedAt` null)         |
| Ready                             | no entry                                                          |
| Failed                            | the entry, completed, with `errors` carrying the failure messages |

The Failed row is completed rather than pending because legacy Anchor's loop
has no timeout and never reads `errors`: a forever-pending entry would starve
it, while a completed one lets it proceed and fail visibly. The entry's
`startedAt` is the surfnet's start time, stable across polls, because clients
diff the execution list between polls and a fresh timestamp per response read
as a new execution every time.

## Appendix C: processes, threads and channels

Subgraphs are process boundaries. Solid edges carry requests or commands,
dashed edges carry replies, events or observations, and the label names the
primitive.

```mermaid
flowchart TB
    subgraph remote["Remote RPC process (mainnet / devnet / testnet)"]
        datasource["JSON-RPC endpoint"]:::external
    end

    subgraph surfpool["surfpool process"]
        direction TB
        cli["CLI orchestration<br/>startup planner"]:::thread
        runloop["Block-production runloop<br/>single consumer; the only writer<br/>of startup machine state"]:::thread
        deployer["Deployment executor thread"]:::thread
        watchdog["Startup watchdog thread<br/>headless only"]:::thread
        tui["TUI / log frontend"]:::thread
        locker["SVM locker<br/>RwLock over SurfnetSvm,<br/>owns the startup status<br/>watch sender"]:::state
        rpcsrv["RPC servers<br/>HTTP and WS"]:::thread
    end

    subgraph clients["Client processes"]
        anchor["Anchor CLI"]:::external
    end

    cli -- "SimnetCommand<br/>(mpsc, FIFO)" --> runloop
    runloop -. "seal outcome<br/>(bounded reply channel,<br/>5s timeout)" .-> cli
    deployer -- "CompleteStartupTask<br/>(same mpsc)" --> runloop
    runloop -- "mutations under<br/>one write guard" --> locker
    rpcsrv -- "reads under<br/>read guard" --> locker
    locker -. "startup status<br/>(tokio watch channel)" .-> watchdog
    runloop -. "SimnetEvent<br/>(mpsc)" .-> tui
    deployer -- "deployment transactions<br/>(loopback JSON-RPC)" --> rpcsrv
    runloop -- "getMultipleAccounts<br/>(HTTPS JSON-RPC)" --> datasource
    anchor -- "getSurfnetInfo poll,<br/>getAccountInfo<br/>(HTTP JSON-RPC)" --> rpcsrv

    classDef external fill:#eceff1,stroke:#546e7a,color:#263238;
    classDef thread fill:#f3e8ff,stroke:#9333ea,color:#581c87;
    classDef state fill:#dcfce7,stroke:#16a34a,color:#14532d;
```

Two edges cross a process boundary, and both only emit requests: the runloop
pulls clone accounts from the remote, and clients pull the read model from
surfpool. Neither can push state into the lifecycle. All the pushing happens
inside the surfpool process, on channels with exactly one consumer.
