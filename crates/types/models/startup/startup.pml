/*
 * The startup machine's concurrent grammar, checked against the spec
 * tables it includes. Three pieces interact:
 *
 *     Sealer ----seal/fail----> plan_state     (PLAN_TABLE cells)
 *     Runner(i) --task events--> task_state[i] (TASK_TABLE cells)
 *     projection macros ------> ready/failed   (expected_phase rules)
 *
 * The exhaustive sweep in surfnet_startup_reachability_tests.rs holds
 * the Rust machine to the spec tables, one sequential transition at a
 * time. Production is not sequential: the plan tasks run concurrently,
 * so a reader can interleave with a sealer and two task runners in any
 * order. This model asks the questions the sweep cannot: it drives the
 * generated table cells (startup_cells.pml, rendered from PLAN_TABLE
 * and TASK_TABLE by cargo surfpool-update-startup-pml) from concurrent
 * workers and checks what a reader can observe across schedules. The
 * cells file is held byte-for-byte to the tables by a test, so the
 * model's transition relation cannot drift from the spec.
 *
 * What this model asks
 * --------------------
 * 1. Can any schedule show a reader readiness and later take it back?
 *    No: ready_stable holds. Once every declared task has succeeded,
 *    the terminal rows of TASK_TABLE refuse every further task event
 *    and PLAN_TABLE refuses a second seal, so readiness is final.
 * 2. Does every schedule end with startup over (Ready or Failed)?
 *    Yes: the end assert and the completes claim hold. Every worker
 *    drives its machine to a terminal row or is gated off after
 *    startup is over.
 * 3. Do these verdicts actually depend on the generated cells?
 *    Yes: -DSKIPSTART sends one runner straight to completion without
 *    starting, the Pending row of apply_task_StartupTaskSucceeded
 *    refuses it, the task rests Pending, and the end assert fires.
 * 4. What does the machine's not-over gate protect? -DUNGATED removes
 *    it and every verdict still holds: the terminal table rows keep
 *    the projection stable on their own. The gate protects the error
 *    contract (a caller driving a finished startup is refused, not
 *    ignored), which is the sweep's territory, not this model's.
 *
 * How the model represents the code
 * ---------------------------------
 * Under study: crates/types/src/startup/spec.rs (PLAN_TABLE,
 * TASK_TABLE, expected_phase) and the machine's call grammar in
 * SurfnetStartupStatus (seal_plan, start_task, complete_task,
 * fail_task).
 *
 * Sealer is the planning phase: exactly one action, a seal carrying
 * one of the four payload subsets or a planning failure. The machine's
 * seal_plan installs the task table in one locker call, so payload
 * installation and the plan transition sit in one atomic block; only
 * the Sealer writes plan_state, so the seal cannot be refused and the
 * payload cannot install against a refused seal.
 *
 * Runner(i) is task i's driver (hydration, runbook execution): wait
 * for planning to end, then fail early or start and then succeed or
 * fail, each event applied through the generated cell for it. Every
 * gate-and-apply is one atomic block, standing in for the machine's
 * single write lock around each transition.
 *
 * Concessions: NTASKS is unrolled to 2 in the projection macros (the
 * Rust iterates); a refused event is a silent skip (the machine
 * returns a typed error; the sweep owns that contract); duplicate
 * payload entries are not modeled (declared is a set, matching the
 * machine's dedup); the projection models only ready/failed, the
 * CloningRemoteAccounts/ExecutingRunbooks split is the document's
 * territory and no property here mentions it.
 *
 * Properties
 * ----------
 * end assert: after all three workers exit, IS_OVER.
 *   Read: startup finishes on every schedule; no task can rest
 *   non-terminal while startup claims to continue.
 * ltl ready_stable: [] (IS_READY -> [] IS_READY)
 *   Read: a client that observed readiness never observes its absence
 *   afterward. Safety; checked without fairness.
 * ltl completes: <> IS_OVER
 *   Read: some suffix of every fair schedule reaches Ready or Failed.
 *   Weak fairness (-f) rules out the schedule where an enabled worker
 *   never steps, which no real scheduler produces.
 *
 * Configurations
 * --------------
 * (default)    the machine as shipped: all payloads, all failure
 *              choices; clean.
 * -DUNGATED    removes the not-over gate from every task event; still
 *              clean (question 4 above).
 * -DSKIPSTART  runner KIND_RemoteAccounts completes without starting;
 *              the cells refuse it and the end assert fires (the
 *              witness that the verdicts run through the cells).
 * -DWEDGE      runners wait for a sealed plan instead of the end of
 *              planning; the planning-failure path never seals, the
 *              runners block forever, and pan reports the invalid end
 *              state (the wedge witness: Spin catches a worker that
 *              can block forever, not only wrong states).
 * -DLTL        declares the two ltl claims; select one with -ltl.
 *
 * Promela notation
 * ----------------
 * ::   one option of an if/do; any executable option may be chosen
 * atomic { }  the enclosed steps execute without interleaving
 * (expr)     a statement that blocks until expr is true
 * ltl name { } a named temporal claim, selected with -ltl name
 *
 * Expected (Spin 6.5.2)
 * ---------------------
 * spin -search startup.pml
 *   -> errors: 0 (603 states)
 *      every schedule ends startup over
 * spin -DUNGATED -search startup.pml
 *   -> errors: 0
 *      the terminal rows alone keep the projection stable
 * spin -DSKIPSTART -search startup.pml
 *   -> assertion violated (the IS_OVER macro, expanded), errors: 1
 *      a completion that skips its start is refused and startup
 *      never finishes; pan prints the assert with every #define
 *      resolved, so the Makefile greps plan_state, a symbol the
 *      expansion keeps
 * spin -DWEDGE -search startup.pml
 *   -> invalid end state (at depth 3), errors: 1
 *      the trail replay shows both runners parked on the mutated
 *      wait with plan_state = PLAN_PlanningFailed and Done parked on
 *      its join at done_workers = 1
 * spin -DLTL -search -a -ltl ready_stable startup.pml
 *   -> errors: 0
 *      readiness once observed is never observed absent
 * spin -DLTL -search -a -f -ltl completes startup.pml
 *   -> errors: 0
 *      every fair schedule reaches Ready or Failed
 */

#include "startup_cells.pml"

/* the sealed plan's task set; written once, inside the seal */
bool declared[NTASKS];
byte done_workers;

/* expected_phase, rules 1..3, as expressions a claim can read. Rule
 * order matters only to the mid-phase split, which no property here
 * uses. Unrolled at NTASKS = 2. */
#define ANY_FAILED (plan_state == PLAN_PlanningFailed \
    || (declared[0] && task_state[0] == TASK_Failed) \
    || (declared[1] && task_state[1] == TASK_Failed))
#define ALL_DONE ((!declared[0] || task_state[0] == TASK_Succeeded) \
    && (!declared[1] || task_state[1] == TASK_Succeeded))
#define IS_READY (plan_state == PLAN_Sealed && !ANY_FAILED && ALL_DONE)
#define IS_OVER (IS_READY || ANY_FAILED)

/* the machine refuses task events once startup is over; -DUNGATED
 * removes that gate to show what the terminal rows carry alone */
#ifdef UNGATED
#define GATED true
#else
#define GATED (!IS_OVER)
#endif

active proctype Sealer() {
    atomic {
        if
        :: apply_plan_StartupPlanSealed()          /* the empty plan */
        :: declared[KIND_RemoteAccounts] = true;
           apply_plan_StartupPlanSealed()
        :: declared[KIND_RunbookExecutions] = true;
           apply_plan_StartupPlanSealed()
        :: declared[KIND_RemoteAccounts] = true;
           declared[KIND_RunbookExecutions] = true;
           apply_plan_StartupPlanSealed()
        :: apply_plan_StartupFailed()
        fi
    };
    done_workers++
}

proctype Runner(byte i) {
#ifdef WEDGE
    /* the mutation: wait for a sealed plan instead of the end of
     * planning; a planning failure never seals, so this runner blocks
     * forever and the join in Done never completes */
    (plan_state == PLAN_Sealed);
#else
    (plan_state != PLAN_Unsealed);         /* planning ended */
#endif
    if
    :: declared[i] ->
#ifdef SKIPSTART
        if
        :: i == KIND_RemoteAccounts ->
            /* the mutation: complete without starting; the Pending row
             * of the succeeded cell refuses this */
            atomic {
                if
                :: GATED -> apply_task_StartupTaskSucceeded(i)
                :: !(GATED) -> skip
                fi
            };
            goto worked
        :: else -> skip
        fi;
#endif
        if
        :: atomic {                         /* fail before starting */
                if
                :: GATED -> apply_task_StartupTaskFailed(i)
                :: !(GATED) -> skip
                fi
            }
        :: atomic {
                if
                :: GATED -> apply_task_StartupTaskStarted(i)
                :: !(GATED) -> skip
                fi
            };
            if
            :: atomic {
                    if
                    :: GATED -> apply_task_StartupTaskSucceeded(i)
                    :: !(GATED) -> skip
                    fi
                }
            :: atomic {
                    if
                    :: GATED -> apply_task_StartupTaskFailed(i)
                    :: !(GATED) -> skip
                    fi
                }
            fi
        fi
    :: !declared[i] -> skip                       /* not in this plan */
    fi;
worked:
    done_workers++
}

init {
    atomic {
        run Runner(KIND_RemoteAccounts);
        run Runner(KIND_RunbookExecutions)
    }
}

/* the contract at rest: when every worker has exited, startup is over */
active proctype Done() {
    (done_workers == 3);
    assert(IS_OVER)
}

#ifdef LTL
ltl ready_stable { [] (IS_READY -> [] IS_READY) }
ltl completes { <> IS_OVER }
#endif
