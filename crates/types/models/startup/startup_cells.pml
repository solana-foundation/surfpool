/* GENERATED: the startup spec tables as Promela cells.
 * Source: crates/types/src/startup/spec.rs (PLAN_TABLE, TASK_TABLE).
 * Regenerate: cargo surfpool-update-startup-pml. Do not edit.
 *
 * State encodings follow the spec's list order, so Promela's
 * zero-initialized globals start in the first-listed state
 * (Unsealed, Pending), which is the machine's default.
 */

#define NTASKS 2
#define KIND_RemoteAccounts 0
#define KIND_RunbookExecutions 1

#define PLAN_Unsealed 0
#define PLAN_Sealed 1
#define PLAN_PlanningFailed 2

#define TASK_Pending 0
#define TASK_Running 1
#define TASK_Succeeded 2
#define TASK_Failed 3

byte plan_state;
byte task_state[NTASKS];

inline apply_plan_StartupPlanSealed() {
    if
    :: plan_state == PLAN_Unsealed -> plan_state = PLAN_Sealed
    :: plan_state == PLAN_Sealed -> skip /* Refuse */
    :: plan_state == PLAN_PlanningFailed -> skip /* Refuse */
    fi
}

inline apply_plan_StartupFailed() {
    if
    :: plan_state == PLAN_Unsealed -> plan_state = PLAN_PlanningFailed
    :: plan_state == PLAN_Sealed -> skip /* Refuse */
    :: plan_state == PLAN_PlanningFailed -> skip /* Refuse */
    fi
}

inline apply_task_StartupTaskStarted(i) {
    if
    :: task_state[i] == TASK_Pending -> task_state[i] = TASK_Running
    :: task_state[i] == TASK_Running -> skip /* Refuse */
    :: task_state[i] == TASK_Succeeded -> skip /* Refuse */
    :: task_state[i] == TASK_Failed -> skip /* Refuse */
    fi
}

inline apply_task_StartupTaskSucceeded(i) {
    if
    :: task_state[i] == TASK_Pending -> skip /* Refuse */
    :: task_state[i] == TASK_Running -> task_state[i] = TASK_Succeeded
    :: task_state[i] == TASK_Succeeded -> skip /* Refuse */
    :: task_state[i] == TASK_Failed -> skip /* Refuse */
    fi
}

inline apply_task_StartupTaskFailed(i) {
    if
    :: task_state[i] == TASK_Pending -> task_state[i] = TASK_Failed
    :: task_state[i] == TASK_Running -> task_state[i] = TASK_Failed
    :: task_state[i] == TASK_Succeeded -> skip /* Refuse */
    :: task_state[i] == TASK_Failed -> skip /* Refuse */
    fi
}

