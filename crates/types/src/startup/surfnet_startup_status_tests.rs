use super::*;

#[test]
fn an_unsealed_empty_plan_is_not_ready() {
    let status = SurfnetStartupStatus::default();
    assert_eq!(status.phase(), SurfnetStartupPhase::Planning);
    assert!(!status.is_ready());
}

#[test]
fn a_sealed_empty_plan_is_ready() {
    let mut status = SurfnetStartupStatus::default();
    status.seal_plan(vec![]).unwrap();
    assert!(status.is_ready());
}

#[test]
fn required_tasks_enforce_initialization_then_runbook_execution() {
    let mut status = SurfnetStartupStatus::default();
    status
        .seal_plan(vec![
            SurfnetStartupTask::RemoteAccounts,
            SurfnetStartupTask::RunbookExecutions,
        ])
        .unwrap();
    assert_eq!(status.phase(), SurfnetStartupPhase::CloningRemoteAccounts);

    status
        .start_task(SurfnetStartupTask::RemoteAccounts)
        .unwrap();
    status
        .complete_task(SurfnetStartupTask::RemoteAccounts)
        .unwrap();
    assert_eq!(status.phase(), SurfnetStartupPhase::ExecutingRunbooks);

    status
        .start_task(SurfnetStartupTask::RunbookExecutions)
        .unwrap();
    status
        .complete_task(SurfnetStartupTask::RunbookExecutions)
        .unwrap();
    assert!(status.is_ready());
}

#[test]
fn failed_tasks_are_terminal() {
    let mut status = SurfnetStartupStatus::default();
    status
        .seal_plan(vec![SurfnetStartupTask::RemoteAccounts])
        .unwrap();
    status
        .start_task(SurfnetStartupTask::RemoteAccounts)
        .unwrap();
    status
        .fail_task(SurfnetStartupTask::RemoteAccounts, "datasource unavailable")
        .unwrap();

    assert_eq!(status.phase(), SurfnetStartupPhase::Failed);
    assert_eq!(status.error(), Some("datasource unavailable"));
    assert_eq!(
        status
            .complete_task(SurfnetStartupTask::RemoteAccounts)
            .unwrap_err()
            .kind(),
        StartupErrorKind::AlreadyTerminal {
            phase: SurfnetStartupPhase::Failed
        }
    );
}

/// Each rejection's derived kind names why, so a test that stops
/// holding is one that changed which rule fired rather than one that
/// merely still refuses.
#[test]
fn illegal_transitions_are_rejected() {
    let mut status = SurfnetStartupStatus::default();
    assert_eq!(
        status
            .start_task(SurfnetStartupTask::RemoteAccounts)
            .unwrap_err()
            .kind(),
        StartupErrorKind::NotSealed
    );

    status
        .seal_plan(vec![SurfnetStartupTask::RemoteAccounts])
        .unwrap();
    assert_eq!(
        status
            .complete_task(SurfnetStartupTask::RemoteAccounts)
            .unwrap_err()
            .kind(),
        StartupErrorKind::TaskState {
            task: SurfnetStartupTask::RemoteAccounts,
            attempted: StartupTaskTransition::Complete,
            from: SurfnetStartupTaskState::Pending
        }
    );
    assert_eq!(
        status
            .seal_plan(vec![SurfnetStartupTask::RunbookExecutions])
            .unwrap_err()
            .kind(),
        StartupErrorKind::AlreadySealed {
            phase: SurfnetStartupPhase::CloningRemoteAccounts
        }
    );
}

/// A phase name this build does not know is skipped, so version skew
/// in the projection cannot break parsing of the authoritative fields.
#[test]
fn an_unknown_phase_name_does_not_break_parsing() {
    let json = r#"{"phase":"restarting","planSealed":false,"tasks":[],"error":null}"#;
    let status: SurfnetStartupStatus = serde_json::from_str(json).unwrap();
    assert_eq!(status.phase(), SurfnetStartupPhase::Planning);
}

/// The unsafe direction of the sealed branch: an error with no failed
/// task would otherwise deserialize to Ready, manufacturing readiness
/// from a malformed response.
#[test]
fn a_sealed_error_with_no_failed_task_is_refused() {
    let json =
        r#"{"phase":"failed","planSealed":true,"tasks":[],"error":"datasource unavailable"}"#;
    assert!(serde_json::from_str::<SurfnetStartupStatus>(json).is_err());
}

/// A failed task that arrives without its reason stays reportable: the
/// compat entry's errors list must never be empty for a failure.
#[test]
fn a_failed_task_without_a_reason_gains_a_placeholder() {
    let json = r#"{"planSealed":true,"tasks":[{"task":"remoteAccounts","state":"failed","error":null}],"error":null}"#;
    let status: SurfnetStartupStatus = serde_json::from_str(json).unwrap();
    assert_eq!(status.phase(), SurfnetStartupPhase::Failed);
    assert!(status.error().is_some());
    assert!(!status.failure_messages().is_empty());
}

/// Wire task tables deduplicate first-wins, as sealing does. A
/// duplicate kind would otherwise wedge the plan: `find` only ever
/// reaches the first entry, so no transition could move the second.
#[test]
fn duplicate_task_kinds_from_the_wire_collapse() {
    let json = r#"{"planSealed":true,"tasks":[{"task":"remoteAccounts","state":"succeeded","error":null},{"task":"remoteAccounts","state":"pending","error":null}],"error":null}"#;
    let status: SurfnetStartupStatus = serde_json::from_str(json).unwrap();
    assert_eq!(status.tasks().len(), 1);
    assert!(status.is_ready());
}

/// Terminal states dominate classification: a refusal because startup
/// already finished says so, whatever else is true of the state, so
/// `AlreadyTerminal` is a complete "stop retrying" discriminator.
#[test]
fn refusals_from_terminal_states_classify_as_terminal() {
    let mut ready = SurfnetStartupStatus::default();
    ready.seal_plan(vec![]).unwrap();
    assert_eq!(
        ready.seal_plan(vec![]).unwrap_err().kind(),
        StartupErrorKind::AlreadyTerminal {
            phase: SurfnetStartupPhase::Ready
        }
    );

    let mut failed = SurfnetStartupStatus::default();
    failed
        .seal_plan(vec![SurfnetStartupTask::RemoteAccounts])
        .unwrap();
    failed
        .fail_task(SurfnetStartupTask::RemoteAccounts, "boom")
        .unwrap();
    assert_eq!(
        failed.fail_planning("boom").unwrap_err().kind(),
        StartupErrorKind::AlreadyTerminal {
            phase: SurfnetStartupPhase::Failed
        }
    );
}

/// The reason-enum era blamed a seal after a planning failure on
/// "already sealed", about a plan that never sealed. The pair keeps
/// the evidence, and the derived kind classifies it truthfully.
#[test]
fn a_seal_after_planning_failure_is_terminal_not_already_sealed() {
    let mut status = SurfnetStartupStatus::default();
    status.fail_planning("boom").unwrap();

    let error = status.seal_plan(vec![]).unwrap_err();
    assert_eq!(
        error.kind(),
        StartupErrorKind::AlreadyTerminal {
            phase: SurfnetStartupPhase::Failed
        }
    );
    assert_eq!(
        error.attempted,
        StartupTransition::SealPlan { tasks: vec![] }
    );
    assert!(matches!(
        error.from,
        SurfnetStartupStatus::PlanningFailed { .. }
    ));
}

/// Drives one task through every (state, event) pair via the machine
/// and compares each outcome with the spec's task lifecycle. This is
/// what makes the generated task-lifecycle table a statement about the
/// machine rather than a declaration beside it.
#[test]
fn the_task_lifecycle_matches_the_spec() {
    use SurfnetStartupTaskState::*;
    let task = SurfnetStartupTask::RemoteAccounts;

    for from in spec::TASK_STATES {
        for event in spec::TaskEvent::ALL {
            let mut status = SurfnetStartupStatus::default();
            status.seal_plan(vec![task]).unwrap();
            match from {
                Pending => {}
                Running => status.start_task(task).unwrap(),
                Succeeded => {
                    status.start_task(task).unwrap();
                    status.complete_task(task).unwrap();
                }
                Failed => status.fail_task(task, "boom").unwrap(),
            }

            let result = match event {
                spec::TaskEvent::Started => status.start_task(task),
                spec::TaskEvent::Succeeded => status.complete_task(task),
                spec::TaskEvent::Failed => status.fail_task(task, "boom"),
            };

            match spec::task_transition(from, event) {
                Some(next) => {
                    result.unwrap_or_else(|error| {
                        panic!("{:?} from {from:?} should land: {error}", event.name())
                    });
                    assert_eq!(status.tasks()[0].state, next);
                }
                None => assert!(
                    result.is_err(),
                    "{:?} from {from:?} should be refused",
                    event.name()
                ),
            }
        }
    }
}

/// Drives the plan through every (state, event) pair via the machine
/// and compares each outcome with the spec's plan lifecycle, including
/// the seal's postcondition: every declared task starts Pending.
#[test]
fn the_plan_lifecycle_matches_the_spec() {
    use spec::{PlanEvent, PlanState};
    let task = SurfnetStartupTask::RemoteAccounts;

    for from in spec::PLAN_STATES {
        for event in PlanEvent::ALL {
            let mut status = SurfnetStartupStatus::default();
            match from {
                PlanState::Unsealed => {}
                PlanState::Sealed => status.seal_plan(vec![task]).unwrap(),
                PlanState::PlanningFailed => status.fail_planning("boom").unwrap(),
            }

            let result = match event {
                PlanEvent::Sealed => status.seal_plan(vec![task]),
                PlanEvent::Failed => status.fail_planning("boom"),
            };

            match spec::plan_transition(from, event) {
                Some(next) => {
                    result.unwrap_or_else(|error| {
                        panic!("{:?} from {from:?} should land: {error}", event.name())
                    });
                    assert_eq!(spec::plan_state_of(&status), next);
                    if next == PlanState::Sealed {
                        assert!(
                            status
                                .tasks()
                                .iter()
                                .all(|entry| entry.state == SurfnetStartupTaskState::Pending),
                            "sealing must leave every declared task Pending: {status:?}"
                        );
                    }
                }
                None => {
                    assert!(
                        result.is_err(),
                        "{:?} from {from:?} should be refused",
                        event.name()
                    );
                    assert_eq!(
                        spec::plan_state_of(&status),
                        from,
                        "a refused plan event must not move the plan"
                    );
                }
            }
        }
    }
}

// The flat wire shape is contract: clients read `planSealed` and
// `phase` as plain fields (the sdk and mcp integration tests pin the
// same shape end to end). The representation is an enum, so the manual
// Serialize impl must keep projecting the object the struct form
// produced.
#[test]
fn serializes_to_the_flat_wire_shape() {
    let mut sealed = SurfnetStartupStatus::default();
    sealed
        .seal_plan(vec![SurfnetStartupTask::RemoteAccounts])
        .unwrap();
    let json = serde_json::to_value(&sealed).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "phase": "cloningRemoteAccounts",
            "planSealed": true,
            "tasks": [
                { "task": "remoteAccounts", "state": "pending", "error": null }
            ],
            "error": null,
        })
    );
}

#[test]
fn deserializes_from_the_shape_it_serializes() {
    let mut sealed_failed = SurfnetStartupStatus::default();
    sealed_failed
        .seal_plan(vec![SurfnetStartupTask::RemoteAccounts])
        .unwrap();
    sealed_failed
        .start_task(SurfnetStartupTask::RemoteAccounts)
        .unwrap();
    sealed_failed
        .fail_task(SurfnetStartupTask::RemoteAccounts, "boom")
        .unwrap();

    let mut planning_failed = SurfnetStartupStatus::default();
    planning_failed.fail_planning("boom").unwrap();

    let mut ready = SurfnetStartupStatus::default();
    ready.seal_plan(vec![]).unwrap();

    for status in [
        SurfnetStartupStatus::default(),
        planning_failed,
        sealed_failed,
        ready,
    ] {
        let json = serde_json::to_string(&status).unwrap();
        let back: SurfnetStartupStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status, "round trip changed the status: {json}");
    }
}

// The wire `phase` is derived output, so deserialization rebuilds the
// variant from `planSealed` and the task table and ignores the phase a
// response claims. The safe direction matters: a malformed unsealed
// response must never manufacture readiness on the client side.
#[test]
fn deserialization_never_manufactures_readiness_from_an_unsealed_status() {
    let json = r#"{"phase":"ready","planSealed":false,"tasks":[],"error":null}"#;
    let status: SurfnetStartupStatus = serde_json::from_str(json).unwrap();
    assert_eq!(status.phase(), SurfnetStartupPhase::Planning);
    assert!(!status.is_ready());
}

const STARTED_AT: u32 = 1_753_000_000;

#[test]
fn legacy_anchor_sees_startup_as_pending_until_ready() {
    let planning =
        GetSurfnetInfoResponse::with_startup(vec![], SurfnetStartupStatus::default(), STARTED_AT);
    assert_eq!(planning.runbook_executions.len(), 1);
    assert_eq!(
        planning.runbook_executions[0].runbook_id,
        GetSurfnetInfoResponse::STARTUP_COMPAT_RUNBOOK_ID
    );
    assert!(planning.runbook_executions[0].completed_at.is_none());

    let mut ready = SurfnetStartupStatus::default();
    ready.seal_plan(vec![]).unwrap();
    let ready_response = GetSurfnetInfoResponse::with_startup(vec![], ready, STARTED_AT);
    assert!(ready_response.runbook_executions.is_empty());
}

// The compat entry must be identical from poll to poll: clients diff
// runbook_executions between responses, and a churning timestamp made
// the synthetic entry read as a new execution every 500ms.
#[test]
fn compat_entry_is_stable_across_polls() {
    let mut failed = SurfnetStartupStatus::default();
    failed.fail_planning("boom").unwrap();

    for status in [SurfnetStartupStatus::default(), failed] {
        let first = GetSurfnetInfoResponse::with_startup(vec![], status.clone(), STARTED_AT);
        let second = GetSurfnetInfoResponse::with_startup(vec![], status, STARTED_AT);
        assert_eq!(first.runbook_executions, second.runbook_executions);
        assert_eq!(first.runbook_executions[0].started_at, STARTED_AT);
    }
}

// A pending compat entry on Failed would starve legacy Anchor's readiness
// loop, which has no timeout; the entry must complete, with the reason
// recorded in `errors`.
#[test]
fn legacy_anchor_sees_startup_failure_as_completed_with_errors() {
    let mut failed = SurfnetStartupStatus::default();
    failed
        .seal_plan(vec![SurfnetStartupTask::RemoteAccounts])
        .unwrap();
    failed
        .start_task(SurfnetStartupTask::RemoteAccounts)
        .unwrap();
    failed
        .fail_task(SurfnetStartupTask::RemoteAccounts, "datasource unavailable")
        .unwrap();

    let response = GetSurfnetInfoResponse::with_startup(vec![], failed, STARTED_AT);
    assert_eq!(response.runbook_executions.len(), 1);
    let compat = &response.runbook_executions[0];
    assert_eq!(
        compat.runbook_id,
        GetSurfnetInfoResponse::STARTUP_COMPAT_RUNBOOK_ID
    );
    assert_eq!(compat.completed_at, Some(STARTED_AT));
    assert_eq!(
        compat.errors,
        Some(vec!["datasource unavailable".to_string()])
    );
}

// fail_planning has no task to carry the error, so the machine-level
// error must reach the compat entry on its own.
#[test]
fn legacy_anchor_sees_planning_failure_as_completed_with_errors() {
    let mut failed = SurfnetStartupStatus::default();
    failed.fail_planning("could not detect framework").unwrap();

    let response = GetSurfnetInfoResponse::with_startup(vec![], failed, STARTED_AT);
    assert_eq!(response.runbook_executions.len(), 1);
    let compat = &response.runbook_executions[0];
    assert_eq!(compat.completed_at, Some(STARTED_AT));
    assert_eq!(
        compat.errors,
        Some(vec!["could not detect framework".to_string()])
    );
}
