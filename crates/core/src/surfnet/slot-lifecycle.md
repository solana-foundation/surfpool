A slot is announced (`CreatedBank`) before any of its block data is
emitted, advances through `Processed`, `Confirmed`, and `Rooted` in that
order and at most once each, or dies (`Dead`): a clock warp abandons the
open slot it leaves behind, and a backward warp kills every slot the new
timeline rewrites. Block production, the startup task, the warp
handlers, and a network reset all drive this one transition relation
instead of each emitting statuses by hand.

## The per-slot table

Rows are the recorded stage of one slot, columns the per-slot events; a
cell says what is emitted and where the slot goes. This table is
generated from the spec's `PER_SLOT` constant
(`slot_lifecycle/spec.rs`), which the exhaustive sweeps hold the
machine to, so what you read here is what runs.

<!-- BEGIN GENERATED: per-slot-table -->
| State | announce | produce | confirm | root |
|---|---|---|---|---|
| `(absent)` | emits CreatedBank; -> Announced | emits CreatedBank, Processed; -> Processed | ignored | ignored |
| `Announced` | ignored | emits Processed; -> Processed | ignored | ignored |
| `Processed` | ignored | ignored | emits Confirmed; -> Confirmed | ignored |
| `Confirmed` | ignored | ignored | ignored | emits Rooted; -> forgotten |
<!-- END GENERATED: per-slot-table -->

## The machine, as edges

<!-- BEGIN GENERATED: diagram -->
```text
(absent)    --announce--> Announced  emits CreatedBank
(absent)    --produce--> Processed  emits CreatedBank, Processed
Announced   --produce--> Processed  emits Processed
Processed   --confirm--> Confirmed  emits Confirmed
Confirmed   --root--> (forgotten) emits Rooted
Announced   --warp away--> (forgotten) emits Dead
(any stage)  --warp back--> (forgotten) emits Dead, every rewritten slot
```
<!-- END GENERATED: diagram -->

## Warp, rooting, and clear

Warps, rooting, and a reset are set-level operations, deliberately
kept out of the table:

- A forward warp from the open slot `f` to `t` kills the abandoned
  slot (`f` was announced and never produced, so it is emitted `Dead`
  and forgotten) and announces `t` through the table's own announce
  cell, so a warp landing on a slot already on record announces
  nothing.
- A backward warp is a reorg: every slot at or past `t` dies (`Dead`,
  in slot order), whatever its stage, and `t` is then re-announced.
  The new timeline replays the killed slots, so their statuses appear
  again; at-most-once holds per bank, and a reorg makes a new bank.
- A backward warp may land at or below the root line (time travel to
  the current epoch does exactly this). Rooted slots left the registry
  when they rooted, so they die without a `Dead`; the landing emits a
  `CreatedBank` at or below anything a consumer saw `Rooted`, with no
  recorded parent when the registry holds nothing below it. That
  announce is the discontinuity signal: a consumer treats a
  `CreatedBank` for a slot it saw rooted as a timeline replacement,
  dropping its state for every slot at or above it.
- Rooting is a threshold, not a single slot: when finality reaches
  slot `r`, every confirmed slot at or below `r` roots, in slot order,
  each through the table's root cell. The registry decides which slots
  are due from its own record, so a history with gaps (a warp) roots
  exactly what it confirmed.
- A reset forgets every slot; the caller announces the new open slot.

## What the table cannot hold

Two obligations order this machine's emissions against other streams,
and live in code order rather than in cells:

- Data before confirmation: `confirm_current_block` emits a slot's block
  data (`BlockMeta`, `Entry`) before driving `produce` and `confirm`,
  so a consumer that flushes on `Confirmed` never loses data.
- Startup before traffic: the startup task announces the open slot and
  sends `EndOfStartup` before the RPC listeners bind, so nothing
  external can emit block data for a slot a plugin is not tracking.

Interleavings (who runs between which writer sections) are out of
scope for this document; the sweeps in
`slot_lifecycle/reachability_tests.rs` cover every reachable state and
event of the sequential machine.

## Limits

Two histories fall outside the registry's record:

- A restart in persistent mode starts an empty registry. Slots
  confirmed by an earlier process get no further statuses in either
  stream: nothing roots them, and nothing replays them.
- A network reset erases the world: every live slot is forgotten with
  no terminal status, and the new genesis is announced. This is the
  one path that drops a live slot without a `Rooted` or a `Dead`; a
  warp, by contrast, kills what it abandons.

Warps split the guarantees in two. Within a bank, the per-slot
guarantees are unconditional: announced before data, data before
confirmation, statuses in order and at most once. Across timelines,
rooted-is-final and slot monotonicity hold only until the operator
warps across them: time travel is a cheatcode, and the operator who
calls it suspends exactly those two guarantees, at one announced
boundary.

## Maintenance

State a rule change in `PER_SLOT` (or the warp/clear arms) first, then
change the machine; the sweep names the first disagreement. Then run
`cargo surfpool-update-slot-spec` to regenerate the blocks above, and
review that diff as the observable change. The prose here is authored:
revise it when a rule changes meaning.
