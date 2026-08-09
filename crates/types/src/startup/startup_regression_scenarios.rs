//! Recorded histories: the event sequences that hurt us (or nearly did),
//! each pinned with what a client must observe along the way.
//!
//! The reachability sweep proves every state safe in the abstract; this
//! module keeps the concrete traces from real incidents, named and in one
//! place, so a regression reproduces an incident by name rather than by
//! archaeology. To record a new incident, append a [`Scenario`]: its
//! [`Provenance`] on the release timeline, the transitions that reproduce
//! it, and a [`Step::Observe`] at each point where a client's conclusion
//! mattered.
//!
//! The histories are versioned along two axes. Each scenario carries its
//! release provenance (which release exhibited it, which change closed
//! it). Each observation pins the conclusion of every client protocol
//! generation, because "what a client sees" depends on which protocol the
//! client speaks; a new generation adds its reader to [`Step::Observe`]
//! and every recorded history then pins it too.

use super::*;

/// Any fixed instant; the observations care what an entry says, not
/// when it started.
const STARTED_AT: u32 = 1_753_000_000;

/// One step of a recorded history: drive the machine, or pin what a
/// client observes at this point.
enum Step {
    Apply(StartupTransition),
    /// The observation, once per client protocol generation:
    ///
    /// - `phase` is what a current client reads from the `startup`
    ///   field.
    /// - `anchor_proceeds` is what a polling client (one that ignores the
    ///   field) concludes from the projected `runbook_executions`; it
    ///   proceeds when every entry is complete.
    Observe {
        phase: SurfnetStartupPhase,
        anchor_proceeds: bool,
    },
}

/// Where a history sits on the release timeline.
enum Provenance {
    /// A shipped release exhibited this history.
    Incident {
        /// The release line that exhibited it.
        observed_in: &'static str,
        /// The change that closed it.
        fixed_by: &'static str,
    },
    /// A rule the design must keep; no release has broken it.
    DesignRule,
}

impl Provenance {
    fn describe(&self) -> String {
        match self {
            Provenance::Incident {
                observed_in,
                fixed_by,
            } => format!("incident: observed in {observed_in}, fixed by {fixed_by}"),
            Provenance::DesignRule => "design rule".to_string(),
        }
    }
}

/// A named incident trace.
struct Scenario {
    name: &'static str,
    provenance: Provenance,
    /// The narrative: what happened, and why the observations below
    /// are the ones that matter.
    context: &'static str,
    steps: Vec<Step>,
}

fn scenarios() -> Vec<Scenario> {
    use Step::*;
    use SurfnetStartupTask::*;

    vec![
        Scenario {
            name: "issue-715: readiness observed while a declared clone was outstanding",
            provenance: Provenance::Incident {
                observed_in: "the v1.5 release line",
                fixed_by: "#733",
            },
            context: "Anchor polled during the clone window, read an empty \
                      runbook list as complete (all([]) == true), and ran \
                      tests against accounts that did not exist yet.",
            steps: vec![
                // The window before sealing: the required work is not
                // known yet, and that must already read as "not ready".
                Observe {
                    phase: SurfnetStartupPhase::Planning,
                    anchor_proceeds: false,
                },
                Apply(StartupTransition::SealPlan {
                    tasks: vec![RemoteAccounts],
                }),
                // The original race window: sealed, clone outstanding.
                Observe {
                    phase: SurfnetStartupPhase::CloningRemoteAccounts,
                    anchor_proceeds: false,
                },
                Apply(StartupTransition::StartTask {
                    task: RemoteAccounts,
                }),
                Observe {
                    phase: SurfnetStartupPhase::CloningRemoteAccounts,
                    anchor_proceeds: false,
                },
                Apply(StartupTransition::CompleteTask {
                    task: RemoteAccounts,
                }),
                Observe {
                    phase: SurfnetStartupPhase::Ready,
                    anchor_proceeds: true,
                },
            ],
        },
        Scenario {
            name: "runbooks finishing first must not open the window",
            provenance: Provenance::DesignRule,
            context: "Hydration and runbook execution run concurrently; the fast \
                      track completing must not read as startup complete \
                      while the slow track still owns declared accounts.",
            steps: vec![
                Apply(StartupTransition::SealPlan {
                    tasks: vec![RemoteAccounts, RunbookExecutions],
                }),
                Apply(StartupTransition::StartTask {
                    task: RunbookExecutions,
                }),
                Apply(StartupTransition::CompleteTask {
                    task: RunbookExecutions,
                }),
                Observe {
                    phase: SurfnetStartupPhase::CloningRemoteAccounts,
                    anchor_proceeds: false,
                },
                Apply(StartupTransition::StartTask {
                    task: RemoteAccounts,
                }),
                Apply(StartupTransition::CompleteTask {
                    task: RemoteAccounts,
                }),
                Observe {
                    phase: SurfnetStartupPhase::Ready,
                    anchor_proceeds: true,
                },
            ],
        },
        Scenario {
            name: "a planning failure must release pollers",
            provenance: Provenance::DesignRule,
            context: "Legacy Anchor's readiness loop has no timeout; a \
                      failure that left the compat entry pending would \
                      park it forever, so failure completes the entry and \
                      the client proceeds into the recorded error.",
            steps: vec![
                Apply(StartupTransition::FailPlanning {
                    error: "could not detect framework".to_string(),
                }),
                Observe {
                    phase: SurfnetStartupPhase::Failed,
                    anchor_proceeds: true,
                },
            ],
        },
        Scenario {
            name: "a hydration failure must release pollers",
            provenance: Provenance::DesignRule,
            context: "Same rule as a planning failure, reached through a \
                      sealed plan: the failed task carries the reason and \
                      the compat entry completes with it.",
            steps: vec![
                Apply(StartupTransition::SealPlan {
                    tasks: vec![RemoteAccounts, RunbookExecutions],
                }),
                Apply(StartupTransition::StartTask {
                    task: RemoteAccounts,
                }),
                Apply(StartupTransition::FailTask {
                    task: RemoteAccounts,
                    error: "datasource unavailable".to_string(),
                }),
                Observe {
                    phase: SurfnetStartupPhase::Failed,
                    anchor_proceeds: true,
                },
            ],
        },
        Scenario {
            name: "an empty sealed plan is ready immediately",
            provenance: Provenance::DesignRule,
            context: "Sealing closes the world: before it, an empty task \
                      list means \"not yet known\"; after it, \"nothing to \
                      do\". Embedded surfnets rely on the second reading.",
            steps: vec![
                Observe {
                    phase: SurfnetStartupPhase::Planning,
                    anchor_proceeds: false,
                },
                Apply(StartupTransition::SealPlan { tasks: vec![] }),
                Observe {
                    phase: SurfnetStartupPhase::Ready,
                    anchor_proceeds: true,
                },
            ],
        },
    ]
}

/// Drives every recorded history through a fresh machine and holds each
/// observation. A failure names the scenario and the step, so the
/// regression report reads as the incident it reproduces.
#[test]
fn recorded_histories_observe_what_they_recorded() {
    for scenario in scenarios() {
        let mut status = SurfnetStartupStatus::default();
        for (index, step) in scenario.steps.iter().enumerate() {
            match step {
                Step::Apply(transition) => {
                    status.apply(transition.clone()).unwrap_or_else(|error| {
                        panic!(
                            "{} [{}]: step {index} was refused: {error}\n({})",
                            scenario.name,
                            scenario.provenance.describe(),
                            scenario.context
                        )
                    });
                }
                Step::Observe {
                    phase,
                    anchor_proceeds,
                } => {
                    assert_eq!(
                        status.phase(),
                        *phase,
                        "{} [{}]: step {index}: phase\n({})",
                        scenario.name,
                        scenario.provenance.describe(),
                        scenario.context
                    );
                    let response =
                        GetSurfnetInfoResponse::with_startup(vec![], status.clone(), STARTED_AT);
                    let proceeds = response
                        .runbook_executions
                        .iter()
                        .all(|execution| execution.completed_at.is_some());
                    assert_eq!(
                        proceeds,
                        *anchor_proceeds,
                        "{} [{}]: step {index}: Anchor's conclusion\n({})",
                        scenario.name,
                        scenario.provenance.describe(),
                        scenario.context
                    );
                }
            }
        }
    }
}
