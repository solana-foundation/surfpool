A slot is announced (`CreatedBank`) before any of its block data is
emitted, advances through `Processed`, `Confirmed`, and `Rooted` in that
order and at most once each, or dies (`Dead`) when a clock warp abandons
it before it is produced. Block production, the startup task, the warp
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
(any slot the new timeline rewrites)--> (forgotten), backward warps only
```
<!-- END GENERATED: diagram -->

## Warp and clear

A warp and a reset are set-level operations, deliberately kept out of
the table:

- A warp from the open slot `f` to `t` kills the abandoned slot (`f`
  was announced and never produced, so it is emitted `Dead` and
  forgotten), forgets every slot at or past `t` when the warp is
  backward (the new timeline rewrites them), and then announces `t`
  through the table's own announce cell, so a warp landing on a slot
  already on record announces nothing.
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

Interleavings (who runs between which writer sections) are checked in
the Promela models kept with the review notes, not here; the sweeps in
`slot_lifecycle/reachability_tests.rs` cover every reachable state and
event of the sequential machine.

## Maintenance

State a rule change in `PER_SLOT` (or the warp/clear arms) first, then
change the machine; the sweep names the first disagreement. Then run
`cargo surfpool-update-slot-spec` to regenerate the blocks above, and
review that diff as the observable change. The prose here is authored:
revise it when a rule changes meaning.
