//! The startup lifecycle machine, its wire projection, and the model check
//! that holds both to `startup-lifecycle.md`.

use serde::{Deserialize, Serialize};

use crate::types::{GetSurfnetInfoResponse, RunbookExecutionStatusReport};

impl GetSurfnetInfoResponse {
    /// Runbook ID of the synthetic startup entry in `runbook_executions`.
    ///
    /// The value is part of the legacy Anchor wire contract and must not
    /// change; the constant's name carries no such constraint.
    pub const STARTUP_COMPAT_RUNBOOK_ID: &'static str = "surfpool-startup";

    /// Builds a response with startup state projected into the legacy
    /// `runbook_executions` representation.
    ///
    /// While startup is in progress, the projection contains one incomplete
    /// synthetic runbook execution. A failed startup is represented as a
    /// completed execution carrying the failure messages. Once startup is
    /// ready, the synthetic entry is omitted.
    ///
    /// `started_at` is the surfnet startup time in Unix seconds. It must
    /// remain stable across calls: legacy clients diff `runbook_executions`
    /// between polls, and a churning timestamp reads as a new execution on
    /// every poll.
    pub fn with_startup(
        mut runbook_executions: Vec<RunbookExecutionStatusReport>,
        startup: SurfnetStartupStatus,
        started_at: u32,
    ) -> Self {
        // Legacy Anchor clients (versions that predate the explicit startup
        // field) infer readiness by waiting until every `runbook_executions`
        // entry is complete. They do not inspect `errors`, and their polling
        // loop has no timeout.
        //
        // Project startup into that protocol as one synthetic execution:
        // - in progress: incomplete;
        // - failed: complete, with the failure messages;
        // - ready: omitted.
        //
        // A failed entry must be complete. Leaving it incomplete would park
        // legacy clients in that loop forever; completing it lets them
        // proceed and encounter the recorded failure.
        //
        // The machine does not retain the failure instant, so the failed
        // entry reuses `started_at` for `completed_at`. The contract
        // requires only a non-null completion time, and a stable value
        // keeps the entry identical between polls.
        let compat = |completed_at, errors| RunbookExecutionStatusReport {
            started_at,
            completed_at,
            runbook_id: Self::STARTUP_COMPAT_RUNBOOK_ID.into(),
            errors,
        };
        match startup.phase() {
            SurfnetStartupPhase::Ready => {}
            SurfnetStartupPhase::Failed => {
                runbook_executions.push(compat(Some(started_at), Some(startup.failure_messages())))
            }
            SurfnetStartupPhase::Planning
            | SurfnetStartupPhase::Initializing
            | SurfnetStartupPhase::Deploying => runbook_executions.push(compat(None, None)),
        }
        Self {
            runbook_executions,
            startup,
        }
    }
}

/// Public readiness lifecycle for a surfnet. `Ready` here means the sealed
/// startup plan completed: clones hydrated, deployment runbooks succeeded.
/// Not to be confused with [`SimnetEvent::Ready`](crate::types::SimnetEvent),
/// which fires when core startup completes (RPC bound) and can precede this
/// by the entire clone-and-deploy window.
///
/// Each variant's meaning is defined by the phase projection table in
/// [`SurfnetStartupStatus`]'s documentation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SurfnetStartupPhase {
    #[default]
    Planning,
    Initializing,
    Deploying,
    Ready,
    Failed,
}

/// A unit of required startup work, declared by the sealed plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SurfnetStartupTask {
    /// Hydrate the accounts declared for cloning from the datasource.
    RemoteAccounts,
    /// Execute the deployment runbooks.
    Deployment,
}

/// A task's position in its lifecycle. The transitions between these
/// states are the task lifecycle table in [`SurfnetStartupStatus`]'s
/// documentation; `Succeeded` and `Failed` are terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SurfnetStartupTaskState {
    Pending,
    Running,
    Succeeded,
    Failed,
}

/// One entry in the sealed task table: a task, its state, and its error
/// when the state is `Failed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SurfnetStartupTaskStatus {
    pub task: SurfnetStartupTask,
    pub state: SurfnetStartupTaskState,
    pub error: Option<String>,
}

/// The move a caller tried to make on a task, which a rejection names so the
/// failure says what was attempted rather than only what state blocked it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupTaskTransition {
    Start,
    Complete,
    Fail,
}

impl std::fmt::Display for StartupTaskTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start => write!(f, "start"),
            Self::Complete => write!(f, "complete"),
            Self::Fail => write!(f, "fail"),
        }
    }
}

/// The transition alphabet: every move a caller can attempt on the machine.
/// The named methods on [`SurfnetStartupStatus`] are wrappers that build one
/// of these and pass it through [`SurfnetStartupStatus::apply`], so a refusal
/// can carry exactly what was attempted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartupTransition {
    /// See [`SurfnetStartupStatus::seal_plan`].
    SealPlan { tasks: Vec<SurfnetStartupTask> },
    /// See [`SurfnetStartupStatus::fail_planning`].
    FailPlanning { error: String },
    /// See [`SurfnetStartupStatus::start_task`].
    StartTask { task: SurfnetStartupTask },
    /// See [`SurfnetStartupStatus::complete_task`].
    CompleteTask { task: SurfnetStartupTask },
    /// See [`SurfnetStartupStatus::fail_task`].
    FailTask {
        task: SurfnetStartupTask,
        error: String,
    },
}

/// A refused transition: what was attempted, and the state it was refused
/// from.
///
/// The pair is the whole story; it classifies nothing and so cannot
/// misclassify. [`StartupError::kind`] derives the reason from the pair in
/// one place, for callers and messages that want a name for what went
/// wrong. The fields are private so the only constructor is the machine's
/// refusal in [`SurfnetStartupStatus::apply`]; a value of this type is
/// therefore always a refusal that actually happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupError {
    attempted: StartupTransition,
    from: SurfnetStartupStatus,
}

/// Why the startup machine refused a transition, derived from the
/// (transition, state) pair by [`StartupError::kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupErrorKind {
    /// The plan is already sealed, and sealing happens once.
    AlreadySealed { phase: SurfnetStartupPhase },
    /// Nothing can move until the plan is sealed.
    NotSealed,
    /// Startup has already finished, in one direction or the other.
    AlreadyTerminal { phase: SurfnetStartupPhase },
    /// The task is not one the sealed plan declared.
    TaskNotPlanned { task: SurfnetStartupTask },
    /// The task cannot make that move from the state it is in.
    TaskState {
        task: SurfnetStartupTask,
        attempted: StartupTaskTransition,
        from: SurfnetStartupTaskState,
    },
    /// Planning has already finished, so it cannot fail now.
    NotPlanning { phase: SurfnetStartupPhase },
}

impl StartupError {
    /// What was attempted.
    pub fn attempted(&self) -> &StartupTransition {
        &self.attempted
    }

    /// The state the transition was refused from.
    pub fn refused_from(&self) -> &SurfnetStartupStatus {
        &self.from
    }

    /// Classifies the refusal.
    ///
    /// The classification exists here and nowhere else, so it cannot drift
    /// between call sites, and the pair remains available as evidence when
    /// the name is not enough. Startup being over dominates every other
    /// explanation, for plan and task moves alike, so `AlreadyTerminal` is
    /// a complete "stop retrying" discriminator.
    pub fn kind(&self) -> StartupErrorKind {
        let phase = self.from.phase();
        if matches!(
            phase,
            SurfnetStartupPhase::Ready | SurfnetStartupPhase::Failed
        ) {
            return StartupErrorKind::AlreadyTerminal { phase };
        }
        match (&self.attempted, &self.from) {
            (
                StartupTransition::SealPlan { .. } | StartupTransition::FailPlanning { .. },
                SurfnetStartupStatus::Planning,
            ) => {
                unreachable!("a legal transition was never refused: {self:?}")
            }
            (StartupTransition::SealPlan { .. }, SurfnetStartupStatus::Sealed(_)) => {
                StartupErrorKind::AlreadySealed { phase }
            }
            (StartupTransition::FailPlanning { .. }, SurfnetStartupStatus::Sealed(_)) => {
                StartupErrorKind::NotPlanning { phase }
            }
            (_, SurfnetStartupStatus::Planning) => StartupErrorKind::NotSealed,
            // PlanningFailed's phase is Failed, so the terminal check above
            // owns every refusal from it.
            (_, SurfnetStartupStatus::PlanningFailed { .. }) => {
                unreachable!("a planning failure is terminal: {self:?}")
            }
            (StartupTransition::StartTask { task }, SurfnetStartupStatus::Sealed(_)) => {
                self.task_kind(*task, StartupTaskTransition::Start)
            }
            (StartupTransition::CompleteTask { task }, SurfnetStartupStatus::Sealed(_)) => {
                self.task_kind(*task, StartupTaskTransition::Complete)
            }
            (StartupTransition::FailTask { task, .. }, SurfnetStartupStatus::Sealed(_)) => {
                self.task_kind(*task, StartupTaskTransition::Fail)
            }
        }
    }

    /// A task move refused on a non-terminal sealed plan: an undeclared
    /// task, or a state that does not admit the move.
    fn task_kind(
        &self,
        task: SurfnetStartupTask,
        attempted: StartupTaskTransition,
    ) -> StartupErrorKind {
        match self.from.tasks().iter().find(|status| status.task == task) {
            None => StartupErrorKind::TaskNotPlanned { task },
            Some(status) => StartupErrorKind::TaskState {
                task,
                attempted,
                from: status.state,
            },
        }
    }
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.kind())
    }
}

impl std::error::Error for StartupError {}

impl std::fmt::Display for StartupErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadySealed { phase } => write!(
                f,
                "startup plan can only be sealed once while planning (phase: {phase:?})"
            ),
            Self::NotSealed => write!(f, "startup plan has not been sealed"),
            Self::AlreadyTerminal { phase } => {
                write!(f, "startup is already terminal ({phase:?})")
            }
            Self::TaskNotPlanned { task } => {
                write!(f, "startup task {task:?} is not part of the sealed plan")
            }
            Self::TaskState {
                task,
                attempted,
                from,
            } => write!(f, "startup task {task:?} cannot {attempted} from {from:?}"),
            Self::NotPlanning { phase } => {
                write!(f, "startup planning cannot fail from phase {phase:?}")
            }
        }
    }
}

/// Lifecycle read model for surfnet startup: seal a plan, drive its tasks,
/// and read the derived phase back.
///
/// A refused transition returns [`StartupError`] and leaves the state
/// untouched, so a caller can publish the status after every attempt
/// without checking which attempts landed.
///
/// The representation carries the issue-715 invariant structurally: the
/// phase is a projection of the variant, and only a sealed plan has a task
/// table to derive `Ready` from, so an unsealed status cannot represent
/// readiness at all. The wire shape is unchanged: a manual `Serialize`
/// impl projects the same flat `{ phase, planSealed, tasks, error }`
/// object the struct form produced.
///
/// The lifecycle in full, from the spec beside this file. The reachability
/// tests hold the machine to it, and the include anchors the document, so
/// renaming it breaks the build rather than leaving a dead reference:
///
/// ---
///
#[doc = include_str!("startup-lifecycle.md")]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SurfnetStartupStatus {
    /// Unsealed: the required task set is not known yet. Never ready.
    #[default]
    Planning,
    /// Planning failed before a plan was sealed. Terminal.
    PlanningFailed { error: String },
    /// The task set is closed; the phase derives from the task states. An
    /// empty sealed plan derives `Ready` immediately.
    Sealed(SealedStartupPlan),
}

/// The flat wire form of [`SurfnetStartupStatus`]. The wire `phase` is a
/// projection the serializer computes; deserialization rebuilds the variant
/// from the authoritative fields (`planSealed`, `tasks`, `error`) and never
/// reads the phase a response claims, so a phase name this build does not
/// know is skipped rather than rejected and cannot break parsing.
///
/// A client must never manufacture readiness from a malformed response.
/// Unsealed statuses deserialize to planning (or a planning failure when
/// they carry an error) regardless of any claimed phase, and sealed ones
/// are sanitized toward the pessimistic reading: task kinds deduplicate
/// first-wins as sealing does, a `Failed` task missing its reason gains a
/// placeholder so the failure stays reportable, a stray error on a task
/// that has not failed is dropped, and a response whose top-level error
/// contradicts a task table with no failure is refused outright.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SurfnetStartupStatusWire {
    plan_sealed: bool,
    tasks: Vec<SurfnetStartupTaskStatus>,
    error: Option<String>,
}

impl Serialize for SurfnetStartupStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("SurfnetStartupStatus", 4)?;
        state.serialize_field("phase", &self.phase())?;
        state.serialize_field("planSealed", &self.plan_sealed())?;
        state.serialize_field("tasks", self.tasks())?;
        state.serialize_field("error", &self.error())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for SurfnetStartupStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = SurfnetStartupStatusWire::deserialize(deserializer)?;
        if !wire.plan_sealed {
            return Ok(if let Some(error) = wire.error {
                Self::PlanningFailed { error }
            } else {
                Self::Planning
            });
        }

        let mut tasks = wire.tasks;
        let mut kinds_seen: Vec<SurfnetStartupTask> = vec![];
        tasks.retain(|status| {
            if kinds_seen.contains(&status.task) {
                return false;
            }
            kinds_seen.push(status.task);
            true
        });
        for status in &mut tasks {
            if status.state == SurfnetStartupTaskState::Failed && status.error.is_none() {
                status.error = Some("failure reason not reported".to_string());
            }
            if status.state != SurfnetStartupTaskState::Failed {
                status.error = None;
            }
        }

        let any_failed = tasks
            .iter()
            .any(|status| status.state == SurfnetStartupTaskState::Failed);
        if wire.error.is_some() && !any_failed {
            return Err(serde::de::Error::custom(
                "a sealed startup status carries an error but no failed task",
            ));
        }
        Ok(Self::Sealed(SealedStartupPlan::from_task_statuses(tasks)))
    }
}

/// A startup plan whose task set is fixed.
///
/// The task operations live here rather than on [`SurfnetStartupStatus`], so
/// holding one of these is proof the plan was sealed. Every mutation routes
/// through [`SurfnetStartupStatus::apply`], which is the single place the
/// seal is checked; nothing downstream re-derives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedStartupPlan {
    tasks: Vec<SurfnetStartupTaskStatus>,
}

impl SealedStartupPlan {
    /// Seals `tasks`, dropping repeats: a task named twice is one obligation.
    fn new(tasks: Vec<SurfnetStartupTask>) -> Self {
        let mut statuses: Vec<SurfnetStartupTaskStatus> = vec![];
        for task in tasks {
            if statuses.iter().any(|status| status.task == task) {
                continue;
            }
            statuses.push(SurfnetStartupTaskStatus {
                task,
                state: SurfnetStartupTaskState::Pending,
                error: None,
            });
        }
        Self { tasks: statuses }
    }

    /// Rebuilds a plan from a wire payload, which may say anything. Callers
    /// deriving readiness from it get whatever the task states imply, which is
    /// the point: the phase is never taken on trust.
    fn from_task_statuses(tasks: Vec<SurfnetStartupTaskStatus>) -> Self {
        Self { tasks }
    }

    pub fn tasks(&self) -> &[SurfnetStartupTaskStatus] {
        &self.tasks
    }

    // Phase derivation encodes task ordering: a non-succeeded RemoteAccounts
    // pins the phase at Initializing, and Deploying is the residual case.
    // Adding a task variant requires deciding where it sits in this ordering.
    fn phase(&self) -> SurfnetStartupPhase {
        if self
            .tasks
            .iter()
            .any(|status| status.state == SurfnetStartupTaskState::Failed)
        {
            SurfnetStartupPhase::Failed
        } else if self
            .tasks
            .iter()
            .all(|status| status.state == SurfnetStartupTaskState::Succeeded)
        {
            SurfnetStartupPhase::Ready
        } else if self.tasks.iter().any(|status| {
            status.task == SurfnetStartupTask::RemoteAccounts
                && status.state != SurfnetStartupTaskState::Succeeded
        }) {
            SurfnetStartupPhase::Initializing
        } else {
            SurfnetStartupPhase::Deploying
        }
    }

    fn error(&self) -> Option<&str> {
        self.tasks
            .iter()
            .find(|status| status.state == SurfnetStartupTaskState::Failed)
            .and_then(|status| status.error.as_deref())
    }

    fn failure_messages(&self) -> Vec<String> {
        self.tasks
            .iter()
            .filter_map(|status| status.error.clone())
            .collect()
    }

    /// A terminal plan accepts no further transitions. Sealing is not checked
    /// here: this type only exists sealed.
    fn active(&self) -> bool {
        !matches!(
            self.phase(),
            SurfnetStartupPhase::Ready | SurfnetStartupPhase::Failed
        )
    }

    // The task moves return whether they were accepted and mutate only on
    // acceptance. They classify nothing: a refusal's reason is derived by
    // `StartupError::kind` from the (transition, state) pair.
    fn start_task(&mut self, task: SurfnetStartupTask) -> bool {
        if !self.active() {
            return false;
        }
        let Some(status) = self.tasks.iter_mut().find(|status| status.task == task) else {
            return false;
        };
        if status.state != SurfnetStartupTaskState::Pending {
            return false;
        }
        status.state = SurfnetStartupTaskState::Running;
        true
    }

    fn complete_task(&mut self, task: SurfnetStartupTask) -> bool {
        if !self.active() {
            return false;
        }
        let Some(status) = self.tasks.iter_mut().find(|status| status.task == task) else {
            return false;
        };
        if status.state != SurfnetStartupTaskState::Running {
            return false;
        }
        status.state = SurfnetStartupTaskState::Succeeded;
        true
    }

    fn fail_task(&mut self, task: SurfnetStartupTask, error: String) -> bool {
        if !self.active() {
            return false;
        }
        let Some(status) = self.tasks.iter_mut().find(|status| status.task == task) else {
            return false;
        };
        if !matches!(
            status.state,
            SurfnetStartupTaskState::Pending | SurfnetStartupTaskState::Running
        ) {
            return false;
        }
        status.state = SurfnetStartupTaskState::Failed;
        status.error = Some(error);
        true
    }
}

impl SurfnetStartupStatus {
    /// The five-valued summary a client reads back as `startup.phase`,
    /// derived from the current state on every call and never stored.
    pub fn phase(&self) -> SurfnetStartupPhase {
        match self {
            Self::Planning => SurfnetStartupPhase::Planning,
            Self::PlanningFailed { .. } => SurfnetStartupPhase::Failed,
            Self::Sealed(plan) => plan.phase(),
        }
    }

    pub fn plan_sealed(&self) -> bool {
        matches!(self, Self::Sealed(_))
    }

    /// The sealed task table; empty until the plan is sealed.
    pub fn tasks(&self) -> &[SurfnetStartupTaskStatus] {
        match self {
            Self::Sealed(plan) => plan.tasks(),
            _ => &[],
        }
    }

    /// The machine-level failure, when the phase is `Failed`: the planning
    /// error, or the failed task's error (at most one task can fail; the
    /// first failure is terminal).
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Planning => None,
            Self::PlanningFailed { error } => Some(error),
            Self::Sealed(plan) => plan.error(),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.phase() == SurfnetStartupPhase::Ready
    }

    /// Failure messages for presentation: each failed task's error; a
    /// planning failure has no task, so its error stands alone.
    pub fn failure_messages(&self) -> Vec<String> {
        match self {
            Self::Planning => vec![],
            Self::PlanningFailed { error } => vec![error.clone()],
            Self::Sealed(plan) => plan.failure_messages(),
        }
    }

    /// The one door: every transition, named wrapper or not, goes through
    /// here. One place decides acceptance, one place upholds "a refusal
    /// mutates nothing", and one place builds the refusal pair.
    pub fn apply(&mut self, transition: StartupTransition) -> Result<(), StartupError> {
        let accepted = match &transition {
            StartupTransition::SealPlan { tasks } => {
                if matches!(self, Self::Planning) {
                    *self = Self::Sealed(SealedStartupPlan::new(tasks.clone()));
                    true
                } else {
                    false
                }
            }
            StartupTransition::FailPlanning { error } => {
                if matches!(self, Self::Planning) {
                    *self = Self::PlanningFailed {
                        error: error.clone(),
                    };
                    true
                } else {
                    false
                }
            }
            StartupTransition::StartTask { task } => match self {
                Self::Sealed(plan) => plan.start_task(*task),
                _ => false,
            },
            StartupTransition::CompleteTask { task } => match self {
                Self::Sealed(plan) => plan.complete_task(*task),
                _ => false,
            },
            StartupTransition::FailTask { task, error } => match self {
                Self::Sealed(plan) => plan.fail_task(*task, error.clone()),
                _ => false,
            },
        };
        if accepted {
            Ok(())
        } else {
            Err(StartupError {
                attempted: transition,
                from: self.clone(),
            })
        }
    }

    /// Fixes the required task set, converting "not yet known" into "this
    /// is the complete list".
    ///
    /// A plan seals once; a later seal is refused. Repeated task kinds
    /// collapse to one entry, and sealing an empty set is ready
    /// immediately.
    pub fn seal_plan(&mut self, tasks: Vec<SurfnetStartupTask>) -> Result<(), StartupError> {
        self.apply(StartupTransition::SealPlan { tasks })
    }

    /// Moves `task` from `Pending` to `Running`.
    pub fn start_task(&mut self, task: SurfnetStartupTask) -> Result<(), StartupError> {
        self.apply(StartupTransition::StartTask { task })
    }

    /// Moves `task` from `Running` to `Succeeded`.
    pub fn complete_task(&mut self, task: SurfnetStartupTask) -> Result<(), StartupError> {
        self.apply(StartupTransition::CompleteTask { task })
    }

    /// Moves `task` to `Failed` from `Pending` or `Running`, recording why.
    /// Failing from `Pending` is deliberate: work can fail before it
    /// begins, but it cannot finish before it begins.
    pub fn fail_task(
        &mut self,
        task: SurfnetStartupTask,
        error: impl Into<String>,
    ) -> Result<(), StartupError> {
        self.apply(StartupTransition::FailTask {
            task,
            error: error.into(),
        })
    }

    /// Records that planning died before a plan was sealed. Terminal, and
    /// refused once a plan exists.
    pub fn fail_planning(&mut self, error: impl Into<String>) -> Result<(), StartupError> {
        self.apply(StartupTransition::FailPlanning {
            error: error.into(),
        })
    }
}

#[cfg(test)]
mod spec;

#[cfg(test)]
mod surfnet_startup_status_tests;

#[cfg(test)]
mod surfnet_startup_reachability_tests;
