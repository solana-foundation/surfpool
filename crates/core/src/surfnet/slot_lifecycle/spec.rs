//! The spec: the slot table itself, as data the sweeps hold the
//! machine to and the module documentation renders.
//!
//! The principle, from the startup state machine (types/src/startup/
//! spec.rs): spec and implementation must be different encodings of the
//! same rules, because the sweep proves the machine agrees with this
//! module, and that proof is empty the moment the two share code. Here
//! the spec's encoding is [`PER_SLOT`], one row per (state, event)
//! cell, and the machine must never interpret it: the table belongs to
//! the spec side only.
//!
//! This machine carries the full treatment (table-as-data, exhaustive
//! sweeps, a generated document) because an external protocol depends
//! on it: geyser plugins consume the emission sequences, and the table
//! has enough cells, plus two set-level events, that eyeballing
//! totality stopped being credible. Smaller registries make do with a
//! named test per cell and hand-written rustdoc.
//!
//! Warp, root-through, and clear are not cells: they are set-level
//! operations over the whole registry, stated in
//! [`expected_emissions`] and [`expected_view`] directly, with the
//! warp's announce step and each root-through root routed through the
//! table's own rows so the two encodings cannot drift on what
//! announcing or rooting means.
//!
//! Maintenance procedure for changing a state, event, or transition:
//!
//! 1. State the new cell in [`PER_SLOT`] first (or the new rule in the
//!    warp/clear arms, in their vocabulary).
//! 2. Change the machine to satisfy it.
//! 3. `cargo test -p surfpool-core --lib slot_lifecycle` fails while
//!    the two disagree, naming the first state and event where they
//!    part.
//! 4. `cargo surfpool-update-slot-spec` regenerates the tables in
//!    `slot-lifecycle.md`; review that diff as the observable change.
//!    The prose around the tables is authored: revise it by hand when
//!    a rule changes meaning, and leave it alone otherwise.

use std::collections::BTreeMap;

use solana_clock::Slot;

use super::SlotStage;

/// The events the registry reacts to; the sweep drives every one from
/// every reachable state.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Event {
    /// The slot is announced (block production for N+1, startup and
    /// resets for the open slot, a warp for its destination).
    Announce(Slot),
    /// The slot's block was produced.
    Produce(Slot),
    /// The slot's block was confirmed.
    Confirm(Slot),
    /// The slot was rooted.
    Root(Slot),
    /// Finality reached `threshold`: every confirmed slot at or below
    /// it roots, in slot order, each through the table's root cell.
    RootThrough(Slot),
    /// The clock jumped from the open slot `from` to `to`.
    Warp { from: Slot, to: Slot },
    /// A network reset forgot every slot.
    Clear,
}

/// The per-slot events: the table's columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventKind {
    /// The announce column.
    Announce,
    /// The produce column.
    Produce,
    /// The confirm column.
    Confirm,
    /// The root column.
    Root,
}

/// A status in spec vocabulary, so comparisons read as the table does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// `SlotStatus::CreatedBank`: the slot is announced.
    Created,
    /// `SlotStatus::Processed`: the slot's block was produced.
    Processed,
    /// `SlotStatus::Confirmed`: the slot's block was confirmed.
    Confirmed,
    /// `SlotStatus::Rooted`: finalized; the slot leaves the registry.
    Rooted,
    /// `SlotStatus::Dead`: abandoned by a warp before it was produced.
    Dead,
}

/// A cell's successor: the slot keeps its stage, moves to another, or
/// leaves the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Next {
    /// The slot keeps its stage (an ignored event).
    Stay,
    /// The slot moves to this stage.
    To(SlotStage),
    /// The slot leaves the registry (a terminal status was emitted).
    Forgotten,
}

/// One cell of the per-slot table.
pub(crate) struct Row {
    /// The stage the slot is in; `None` is a slot not on record.
    pub(crate) state: Option<SlotStage>,
    /// The column: which per-slot event arrives.
    pub(crate) event: EventKind,
    /// The statuses the cell emits, in order.
    pub(crate) emits: &'static [Status],
    /// Where the slot goes.
    pub(crate) next: Next,
}

use EventKind as E;
use Next::{Forgotten, Stay, To};
use SlotStage::{Announced, Confirmed as SConfirmed, Processed as SProcessed};
use Status as S;

const fn row(
    state: Option<SlotStage>,
    event: EventKind,
    emits: &'static [Status],
    next: Next,
) -> Row {
    Row {
        state,
        event,
        emits,
        next,
    }
}

/// The per-slot table. The source is the readable artifact; the same
/// rows render into `slot-lifecycle.md` for cargo doc.
#[rustfmt::skip]
pub(crate) const PER_SLOT: &[Row] = &[
    //   state             event       emits                          next
    row( None,             E::Announce, &[S::Created],                To(Announced)   ),
    row( None,             E::Produce,  &[S::Created, S::Processed],  To(SProcessed)  ),
    row( None,             E::Confirm,  &[],                          Stay            ),
    row( None,             E::Root,     &[],                          Stay            ),
    row( Some(Announced),  E::Announce, &[],                          Stay            ), // announced at most once
    row( Some(Announced),  E::Produce,  &[S::Processed],              To(SProcessed)  ),
    row( Some(Announced),  E::Confirm,  &[],                          Stay            ), // no skipping
    row( Some(Announced),  E::Root,     &[],                          Stay            ),
    row( Some(SProcessed), E::Announce, &[],                          Stay            ),
    row( Some(SProcessed), E::Produce,  &[],                          Stay            ), // processed at most once
    row( Some(SProcessed), E::Confirm,  &[S::Confirmed],              To(SConfirmed)  ),
    row( Some(SProcessed), E::Root,     &[],                          Stay            ),
    row( Some(SConfirmed), E::Announce, &[],                          Stay            ),
    row( Some(SConfirmed), E::Produce,  &[],                          Stay            ),
    row( Some(SConfirmed), E::Confirm,  &[],                          Stay            ), // confirmed at most once
    row( Some(SConfirmed), E::Root,     &[S::Rooted],                 Forgotten       ),
];

/// The table's cell for a stage and event. Totality is a test, so the
/// lookup cannot miss.
fn cell(state: Option<SlotStage>, event: EventKind) -> &'static Row {
    PER_SLOT
        .iter()
        .find(|row| row.state == state && row.event == event)
        .expect("the_table_is_total guarantees every cell exists")
}

pub(crate) type View = BTreeMap<Slot, SlotStage>;

fn apply_cell(view: &mut View, slot: Slot, event: EventKind) -> Vec<(Slot, Status)> {
    let row = cell(view.get(&slot).copied(), event);
    match row.next {
        Stay => {}
        To(stage) => {
            view.insert(slot, stage);
        }
        Forgotten => {
            view.remove(&slot);
        }
    }
    row.emits.iter().map(|status| (slot, *status)).collect()
}

/// The emissions the spec requires for (view, event), in order.
pub(crate) fn expected_emissions(view: &View, event: &Event) -> Vec<(Slot, Status)> {
    let mut view = view.clone();
    match event {
        Event::Announce(slot) => apply_cell(&mut view, *slot, E::Announce),
        Event::Produce(slot) => apply_cell(&mut view, *slot, E::Produce),
        Event::Confirm(slot) => apply_cell(&mut view, *slot, E::Confirm),
        Event::Root(slot) => apply_cell(&mut view, *slot, E::Root),
        Event::RootThrough(threshold) => {
            let due: Vec<Slot> = view
                .iter()
                .filter(|(slot, stage)| **slot <= *threshold && **stage == SConfirmed)
                .map(|(slot, _)| *slot)
                .collect();
            due.into_iter()
                .flat_map(|slot| apply_cell(&mut view, slot, E::Root))
                .collect()
        }
        Event::Warp { from, to } => {
            // Set-level rules: a backward warp kills every slot the new
            // timeline rewrites (`Dead`, ascending; the abandoned open
            // slot is among them), a forward warp kills only the
            // abandoned open slot, and the destination is announced
            // through the table's own announce cell.
            let mut out = vec![];
            if to < from {
                let rewritten: Vec<Slot> = view.keys().copied().filter(|slot| slot >= to).collect();
                for slot in rewritten {
                    view.remove(&slot);
                    out.push((slot, S::Dead));
                }
            } else if from != to && view.get(from) == Some(&Announced) {
                view.remove(from);
                out.push((*from, S::Dead));
            }
            out.extend(apply_cell(&mut view, *to, E::Announce));
            out
        }
        Event::Clear => vec![],
    }
}

/// The registry the spec requires (view, event) to leave behind.
pub(crate) fn expected_view(view: &View, event: &Event) -> View {
    let mut next = view.clone();
    match event {
        Event::Announce(slot) => {
            apply_cell(&mut next, *slot, E::Announce);
        }
        Event::Produce(slot) => {
            apply_cell(&mut next, *slot, E::Produce);
        }
        Event::Confirm(slot) => {
            apply_cell(&mut next, *slot, E::Confirm);
        }
        Event::Root(slot) => {
            apply_cell(&mut next, *slot, E::Root);
        }
        Event::RootThrough(threshold) => {
            let due: Vec<Slot> = next
                .iter()
                .filter(|(slot, stage)| **slot <= *threshold && **stage == SConfirmed)
                .map(|(slot, _)| *slot)
                .collect();
            for slot in due {
                apply_cell(&mut next, slot, E::Root);
            }
        }
        Event::Warp { from, to } => {
            if to < from {
                next.retain(|slot, _| slot < to);
            } else if from != to && next.get(from) == Some(&Announced) {
                next.remove(from);
            }
            apply_cell(&mut next, *to, E::Announce);
        }
        Event::Clear => next.clear(),
    }
    next
}

fn stage_name(state: Option<SlotStage>) -> &'static str {
    match state {
        None => "(absent)",
        Some(Announced) => "Announced",
        Some(SProcessed) => "Processed",
        Some(SConfirmed) => "Confirmed",
    }
}

fn status_name(status: Status) -> &'static str {
    match status {
        S::Created => "CreatedBank",
        S::Processed => "Processed",
        S::Confirmed => "Confirmed",
        S::Rooted => "Rooted",
        S::Dead => "Dead",
    }
}

fn event_name(event: EventKind) -> &'static str {
    match event {
        E::Announce => "announce",
        E::Produce => "produce",
        E::Confirm => "confirm",
        E::Root => "root",
    }
}

fn cell_text(row: &Row) -> String {
    let emits = row
        .emits
        .iter()
        .map(|status| status_name(*status))
        .collect::<Vec<_>>()
        .join(", ");
    let next = match row.next {
        Stay => "no change".to_string(),
        To(stage) => format!("-> {}", stage_name(Some(stage))),
        Forgotten => "-> forgotten".to_string(),
    };
    if emits.is_empty() {
        "ignored".to_string()
    } else {
        format!("emits {emits}; {next}")
    }
}

/// The per-slot table rendered as markdown, one state per row and one
/// event per column, for `slot-lifecycle.md`.
pub(crate) fn render_per_slot_table() -> String {
    let states = [None, Some(Announced), Some(SProcessed), Some(SConfirmed)];
    let events = [E::Announce, E::Produce, E::Confirm, E::Root];
    let mut out = String::from("| State |");
    for event in events {
        out.push_str(&format!(" {} |", event_name(event)));
    }
    out.push_str("\n|---|---|---|---|---|\n");
    for state in states {
        out.push_str(&format!("| `{}` |", stage_name(state)));
        for event in events {
            out.push_str(&format!(" {} |", cell_text(cell(state, event))));
        }
        out.push('\n');
    }
    out
}

/// A stage as a mermaid state reference: `(absent)` is the start
/// pseudo-state, since a slot's record begins at its first event.
fn stage_ref(state: Option<SlotStage>) -> &'static str {
    match state {
        None => "[*]",
        Some(stage) => stage_name(Some(stage)),
    }
}

/// The machine's advancing edges rendered as a mermaid state diagram,
/// for `slot-lifecycle.md`. The two warp edges are the set-level
/// rules, spelled beside the table-driven ones; `(forgotten)` is the
/// end pseudo-state. The fence sits inside `BEGIN MERMAID` markers so
/// the render pipeline can pre-draw it for cargo doc.
pub(crate) fn render_diagram() -> String {
    let mut out =
        String::from("<!-- BEGIN MERMAID: machine-edges -->\n```mermaid\nstateDiagram-v2\n");
    for row in PER_SLOT {
        let target = match row.next {
            To(stage) if row.state != Some(stage) => stage_name(Some(stage)),
            Forgotten => "[*]",
            _ => continue,
        };
        out.push_str(&format!(
            "    {} --> {} : {} ({})\n",
            stage_ref(row.state),
            target,
            event_name(row.event),
            row.emits
                .iter()
                .map(|status| status_name(*status))
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    out.push_str("    Announced --> [*] : warp away (Dead)\n");
    out.push_str("    state \"any stage\" as any_stage\n");
    out.push_str("    any_stage --> [*] : warp back (Dead, every rewritten slot)\n");
    out.push_str("```\n<!-- END MERMAID: machine-edges -->\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_total() {
        let states = [None, Some(Announced), Some(SProcessed), Some(SConfirmed)];
        let events = [E::Announce, E::Produce, E::Confirm, E::Root];
        for state in states {
            for event in events {
                let count = PER_SLOT
                    .iter()
                    .filter(|row| row.state == state && row.event == event)
                    .count();
                assert_eq!(
                    count,
                    1,
                    "the cell ({}, {}) must appear exactly once",
                    stage_name(state),
                    event_name(event)
                );
            }
        }
        assert_eq!(PER_SLOT.len(), states.len() * events.len());
    }
}
