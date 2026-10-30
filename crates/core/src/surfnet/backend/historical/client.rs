//! Historical mode (`surfpool start --after`): SVM Replay owns the historical state and
//! executes every transaction, clock change and account change; Surfpool keeps its RPC,
//! indexes, subscriptions, cheatcodes and workflows on top. This module is only the client
//! for SVM Replay's private backend process (`svm-replay session --backend stdio`): one JSON
//! request line, one JSON reply line. It holds no historical data or runtime rules itself.

use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Write},
    num::NonZeroU64,
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use crossbeam_channel::{Receiver, RecvTimeoutError, unbounded};
use serde_json::{Value, json};
use solana_account::{Account, AccountSharedData};
use solana_hash::Hash;
use solana_message::{
    compiled_instruction::CompiledInstruction,
    inner_instruction::{InnerInstruction, InnerInstructionsList},
};
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_context::transaction::TransactionReturnData;
use solana_transaction_error::TransactionError;

use crate::{
    error::{SurfpoolError, SurfpoolResult},
    surfnet::backend::{
        FailedTransactionMetadata, SimulatedTransactionInfo, TransactionMetadata, TransactionResult,
    },
};

/// Upper bound for one backend request. Requests that need historical data the cache lacks
/// wait for the provider; the backend's own budget normally ends them first.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(900);
/// Upper bound for preparing the fork, matching the backend's longest session budget.
const READY_TIMEOUT: Duration = Duration::from_secs(3600);
/// One account image per read: the largest (10 MiB) image stays far below the backend's
/// 64 MiB reply frame.
const ACCOUNTS_PER_READ: usize = 1;

/// How to start SVM Replay's backend; the CLI builds it from `--after` and provider options.
#[derive(Clone)]
pub struct HistoricalLaunch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Environment for the backend (for example a credential); never logged.
    pub envs: Vec<(OsString, OsString)>,
}

impl From<&surfpool_types::HistoricalSimnetConfig> for HistoricalLaunch {
    fn from(config: &surfpool_types::HistoricalSimnetConfig) -> Self {
        Self {
            program: PathBuf::from(&config.program),
            args: config.args.iter().map(OsString::from).collect(),
            envs: config
                .envs
                .iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value)))
                .collect(),
        }
    }
}

/// Where the backend's stderr and any cleanup failure are reported.
pub type HistoricalLog = Arc<dyn Fn(String) + Send + Sync>;

/// A typed backend failure as a Surfpool error: the backend's code stays first in the
/// message and in `data`, with its details (for example the provider that failed).
pub fn backend_error(error: &Value) -> SurfpoolError {
    SurfpoolError::historical(
        error["code"].as_str().unwrap_or("HISTORICAL_BACKEND"),
        error["message"]
            .as_str()
            .unwrap_or("historical backend error"),
        error.get("details").cloned(),
    )
}

fn protocol(message: impl std::fmt::Display) -> SurfpoolError {
    SurfpoolError::historical("HISTORICAL_BACKEND", &message.to_string(), None)
}

fn terminated() -> SurfpoolError {
    SurfpoolError::historical(
        "HISTORICAL_TERMINATED",
        "the historical backend was shut down with its surfnet",
        None,
    )
}

/// The child process; dropping it stops and reaps the process, logging what fails.
struct Process {
    child: Child,
    stdin: ChildStdin,
    replies: Receiver<Result<String, String>>,
    /// Set after a timeout, protocol error or exit; every later request fails with it.
    broken: Option<SurfpoolError>,
    /// Whether the backend (the group leader) was reaped. Every signal to its group and every
    /// reap happens under this guard, which is never held across I/O, so no kill can reach a
    /// process group id after its leader was reaped and the id became reusable.
    reaped: Lifecycle,
    log: HistoricalLog,
}

type Lifecycle = Arc<Mutex<bool>>;

fn lifecycle(guard: &Lifecycle) -> std::sync::MutexGuard<'_, bool> {
    guard
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// SIGKILL to the backend's process group; the caller holds the lifecycle guard and the
/// leader is not reaped.
fn kill_group(pid: u32, log: &HistoricalLog, reason: &str) {
    #[cfg(unix)]
    if unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            log(format!(
                "historical backend {pid}: group kill failed while {reason}: {error}"
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = (pid, log, reason);
}

/// Whether the backend exited, without reaping it: an exited, unreaped leader keeps its pid
/// and so its process group id reserved.
#[cfg(unix)]
fn exited_unreaped(pid: u32) -> std::io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { info.si_pid() } != 0)
}

impl Process {
    /// Stops the backend and everything it started: its process group (its own, see
    /// `launch`) is killed, which also ends stopped or orphaned workers, then it is reaped.
    /// A backend that already exited gracefully reaped its own workers.
    fn stop(&mut self, reason: &str) {
        let mut reaped = lifecycle(&self.reaped);
        if *reaped {
            return;
        }
        let pid = self.child.id();
        kill_group(pid, &self.log, reason);
        #[cfg(not(unix))]
        if let Err(e) = self.child.kill() {
            (self.log)(format!(
                "historical backend {pid}: kill failed while {reason}: {e}"
            ));
        }
        match self.child.wait() {
            Ok(_) => *reaped = true,
            Err(e) => (self.log)(format!(
                "historical backend {pid}: reap failed while {reason}: {e}"
            )),
        }
    }

    /// Waits for a graceful exit after shutdown before `Drop` stops it.
    fn exited_within(&mut self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            {
                let mut reaped = lifecycle(&self.reaped);
                if *reaped {
                    return true;
                }
                match self.child.try_wait() {
                    Ok(Some(_)) => {
                        *reaped = true;
                        return true;
                    }
                    Ok(None) if std::time::Instant::now() < deadline => {}
                    Ok(None) => return false,
                    Err(e) => {
                        let pid = self.child.id();
                        (self.log)(format!(
                            "historical backend {pid}: status check failed: {e}"
                        ));
                        return false;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop("closing");
    }
}

/// The backend process. One request runs at a time; the process ends with the last handle.
pub struct HistoricalBackend {
    process: Mutex<Process>,
    next_id: AtomicU64,
    pins: Value,
    log: HistoricalLog,
    /// The backend's pid, which is also its process group: termination needs no lock, so it
    /// also ends a request that is still waiting on a stopped or wedged backend.
    pid: u32,
    terminated: AtomicBool,
    reaped: Lifecycle,
    /// How many operations SVM Replay's journal holds, as its latest reply said; none
    /// without a journal.
    journaled: Option<AtomicU64>,
    /// The newest store revision among the journal's records when the backend started.
    journal_revision: Option<u64>,
    /// The store revision the surfnet is writing (0: none), sent with each operation.
    revision: AtomicU64,
}

/// Every running backend, so a surfnet that is shutting down can end them even while a
/// request holds one.
static BACKENDS: Mutex<Vec<std::sync::Weak<HistoricalBackend>>> = Mutex::new(Vec::new());

/// Ends every running historical backend (see [`HistoricalBackend::terminate`]).
pub fn terminate_all() {
    let backends: Vec<_> = match BACKENDS.lock() {
        Ok(mut backends) => {
            backends.retain(|backend| backend.strong_count() > 0);
            backends
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .collect()
        }
        Err(poisoned) => poisoned
            .into_inner()
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect(),
    };
    for backend in backends {
        backend.terminate();
    }
}

impl HistoricalBackend {
    /// Starts the backend and waits for its ready line, until `cancelled` (polled while the
    /// fork is prepared) turns true. A failure before ready stops and reaps the process and
    /// returns the backend's own typed error when it printed one.
    pub fn launch(
        launch: &HistoricalLaunch,
        log: HistoricalLog,
        mut cancelled: impl FnMut() -> bool,
    ) -> SurfpoolResult<Arc<Self>> {
        let mut command = Command::new(&launch.program);
        command
            .args(&launch.args)
            .envs(launch.envs.iter().map(|(name, value)| (name, value)))
            // The backend exits with this process (it binds itself to the parent).
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own process group: a terminal Ctrl-C reaches Surfpool, which shuts it down.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn().map_err(|e| {
            protocol(format!(
                "cannot start {}: {e}",
                launch.program.to_string_lossy()
            ))
        })?;
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
            let pid = child.id();
            if let Err(e) = child.kill().and_then(|()| child.wait().map(drop)) {
                log(format!(
                    "historical backend {pid}: stop after missing pipe failed: {e}"
                ));
            }
            return Err(protocol("historical backend pipes missing"));
        };
        let (sender, replies) = unbounded();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                let message = match reader.read_line(&mut line) {
                    Ok(0) => Err("the historical backend exited".to_string()),
                    Ok(_) => Ok(line),
                    Err(e) => Err(format!("read historical backend reply: {e}")),
                };
                let last = message.is_err();
                if sender.send(message).is_err() || last {
                    return;
                }
            }
        });
        let stderr_log = log.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) => stderr_log(line),
                    Err(e) => {
                        stderr_log(format!("historical backend stderr unreadable: {e}"));
                        return;
                    }
                }
            }
        });
        // From here on, every early return drops `process`, which stops and reaps the child.
        let mut process = Process {
            child,
            stdin,
            replies,
            broken: None,
            reaped: Arc::new(Mutex::new(false)),
            log: log.clone(),
        };
        let deadline = std::time::Instant::now() + READY_TIMEOUT;
        let first = loop {
            match process.replies.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(line)) => break line,
                Ok(Err(message)) => {
                    // Workers may outlive an early exit: stop the whole group first.
                    process.stop("handling an exit before ready");
                    return Err(protocol(format!("{message} before it was ready")));
                }
                Err(RecvTimeoutError::Timeout) if cancelled() => {
                    return Err(SurfpoolError::historical(
                        "HISTORICAL_CANCELLED",
                        "startup was cancelled while the historical fork was being prepared",
                        None,
                    ));
                }
                Err(RecvTimeoutError::Timeout) if std::time::Instant::now() < deadline => {}
                Err(_) => return Err(protocol("the historical backend did not become ready")),
            }
        };
        let first: Value = serde_json::from_str(&first)
            .map_err(|e| protocol(format!("invalid first backend line: {e}")))?;
        if first["schema"] == "svm-historical-backend/v1" && first["kind"] == "ready" {
            let pins = first
                .get("pins")
                .filter(|pins| pins.is_object())
                .cloned()
                .ok_or_else(|| protocol("ready line has no pins"))?;
            // A journaled fork's ready line says how many operations SVM Replay replayed, and
            // the newest store revision they belong to.
            let malformed = || protocol("ready line has a malformed journal");
            let (journaled, journal_revision) = match first.get("journal") {
                None => (None, None),
                Some(journal) => (
                    Some(AtomicU64::new(
                        journal["records"].as_u64().ok_or_else(malformed)?,
                    )),
                    serde_json::from_value::<Option<u64>>(journal["revision"].clone())
                        .map_err(|_| malformed())?,
                ),
            };
            let pid = process.child.id();
            let reaped = process.reaped.clone();
            let backend = Arc::new(Self {
                process: Mutex::new(process),
                next_id: AtomicU64::new(1),
                pins,
                log,
                pid,
                terminated: AtomicBool::new(false),
                reaped,
                journaled,
                journal_revision,
                revision: AtomicU64::new(0),
            });
            match BACKENDS.lock() {
                Ok(mut backends) => backends.push(Arc::downgrade(&backend)),
                Err(poisoned) => poisoned.into_inner().push(Arc::downgrade(&backend)),
            }
            return Ok(backend);
        }
        // The backend failed before it was ready and printed its typed error instead.
        match first.get("error").filter(|error| error.is_object()) {
            Some(error) => Err(backend_error(error)),
            None => Err(protocol(format!("unexpected first backend line: {first}"))),
        }
    }

    /// The fork's pins: slot, transaction index, runtime, worker and source identities.
    pub fn pins(&self) -> &Value {
        &self.pins
    }

    /// How many operations SVM Replay's journal holds; none without a journal.
    pub fn journaled(&self) -> Option<u64> {
        self.journaled
            .as_ref()
            .map(|journaled| journaled.load(Ordering::Acquire))
    }

    /// The newest store revision among the journal's records when the backend started.
    pub fn journal_revision(&self) -> Option<u64> {
        self.journal_revision
    }

    /// Sends `revision` with each following operation (none: outside the store's writes).
    pub fn set_revision(&self, revision: Option<NonZeroU64>) {
        let revision = revision.map_or(0, NonZeroU64::get);
        self.revision.store(revision, Ordering::Release);
    }

    /// Fails once the backend process has exited or its stream broke.
    pub fn check(&self) -> SurfpoolResult<()> {
        let mut process = self
            .process
            .lock()
            .map_err(|_| protocol("historical backend lock poisoned"))?;
        if let Some(error) = &process.broken {
            return Err(error.clone());
        }
        let pid = process.child.id();
        let guard = process.reaped.clone();
        let mut reaped = lifecycle(&guard);
        #[cfg(unix)]
        let exited = exited_unreaped(pid);
        #[cfg(not(unix))]
        let exited = process.child.try_wait().map(|status| status.is_some());
        match exited {
            Ok(false) => Ok(()),
            Ok(true) => {
                // Its workers may outlive a crashed backend: end the group it led while the
                // unreaped leader still reserves its id, then reap it.
                kill_group(pid, &self.log, "handling an exit");
                if let Err(e) = process.child.wait() {
                    (self.log)(format!("historical backend {pid}: reap failed: {e}"));
                } else {
                    *reaped = true;
                }
                drop(reaped);
                self.terminated.store(true, Ordering::Release);
                let error = protocol("the historical backend exited");
                process.broken = Some(error.clone());
                Err(error)
            }
            Err(e) => Err(protocol(format!("historical backend status unknown: {e}"))),
        }
    }

    /// Ends the backend now: shut down gracefully when it is idle, otherwise (a request is
    /// waiting on it) kill its process group, which fails that request at once. Later
    /// requests fail with `HISTORICAL_TERMINATED`.
    pub fn terminate(&self) {
        if self.terminated.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut process) = self.process.try_lock() {
            if process.broken.is_none() {
                let line = format!("{}\n", json!({"id": 0, "op": "shutdown"}));
                match process
                    .stdin
                    .write_all(line.as_bytes())
                    .and_then(|()| process.stdin.flush())
                {
                    Ok(()) => {
                        if let Err(e) = process.replies.recv_timeout(Duration::from_secs(2)) {
                            (self.log)(format!(
                                "historical backend {}: shutdown unanswered: {e}",
                                self.pid
                            ));
                        }
                        if !process.exited_within(Duration::from_secs(2)) {
                            (self.log)(format!(
                                "historical backend {}: still running after shutdown; stopping it",
                                self.pid
                            ));
                        }
                    }
                    Err(e) => (self.log)(format!(
                        "historical backend {}: shutdown not sent: {e}",
                        self.pid
                    )),
                }
            }
            process.stop("terminating");
            process.broken = Some(terminated());
            return;
        }
        // A request holds the process: kill its group without that lock, under the lifecycle
        // guard, so the leader cannot be reaped (and its id reused) in between.
        let reaped = lifecycle(&self.reaped);
        if !*reaped {
            kill_group(self.pid, &self.log, "terminating during a request");
            (self.log)(format!(
                "historical backend {}: killed while a request was waiting on it",
                self.pid
            ));
        }
    }

    fn request(&self, branch: u64, op: Value) -> SurfpoolResult<Value> {
        if self.terminated.load(Ordering::Acquire) {
            return Err(terminated());
        }
        let mut process = self
            .process
            .lock()
            .map_err(|_| protocol("historical backend lock poisoned"))?;
        if let Some(error) = &process.broken {
            return Err(error.clone());
        }
        if self.terminated.load(Ordering::Acquire) {
            return Err(terminated());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // An operation inside a store revision is journaled with it.
        let mut request = json!({"id": id, "branch": branch, "op": op});
        if let Some(revision) = NonZeroU64::new(self.revision.load(Ordering::Acquire)) {
            request["revision"] = json!(revision.get());
        }
        let reply = exchange(&mut process, id, request);
        let result = reply.and_then(|reply| {
            if let Some(journaled) = &self.journaled {
                let count = reply["journaled"]
                    .as_u64()
                    .ok_or_else(|| protocol(format!("reply {id} lacks the journal's count")))?;
                journaled.store(count, Ordering::Release);
            }
            match (reply.get("result"), reply.get("error")) {
                (Some(result), None) => Ok(Ok(result.clone())),
                (None, Some(error)) if error.is_object() => Ok(Err(backend_error(error))),
                _ => Err(protocol(format!(
                    "reply {id} must hold exactly one of result and error"
                ))),
            }
        });
        match result {
            Ok(answer) => answer,
            Err(error) => {
                // A lost, mismatched or malformed reply leaves the stream unusable.
                self.terminated.store(true, Ordering::Release);
                process.stop("handling a protocol failure");
                process.broken = Some(error.clone());
                Err(error)
            }
        }
    }
}

fn exchange(process: &mut Process, id: u64, request: Value) -> SurfpoolResult<Value> {
    let mut line = serde_json::to_vec(&request).map_err(protocol)?;
    line.push(b'\n');
    process
        .stdin
        .write_all(&line)
        .and_then(|()| process.stdin.flush())
        .map_err(|e| protocol(format!("send to historical backend: {e}")))?;
    let reply = match process.replies.recv_timeout(REQUEST_TIMEOUT) {
        Ok(Ok(line)) => serde_json::from_str::<Value>(&line)
            .map_err(|e| protocol(format!("invalid historical backend reply: {e}")))?,
        Ok(Err(message)) => return Err(protocol(message)),
        Err(RecvTimeoutError::Timeout) => {
            return Err(protocol(format!(
                "no reply within {}s",
                REQUEST_TIMEOUT.as_secs()
            )));
        }
        Err(RecvTimeoutError::Disconnected) => {
            return Err(protocol("the historical backend exited"));
        }
    };
    if reply.get("id") != Some(&json!(id)) {
        return Err(protocol(format!(
            "reply {} answered request {id}",
            reply["id"]
        )));
    }
    Ok(reply)
}

impl Drop for HistoricalBackend {
    fn drop(&mut self) {
        let log = self.log.clone();
        let process = match self.process.get_mut() {
            Ok(process) => process,
            Err(poisoned) => {
                log("historical backend lock poisoned; stopping the process".into());
                poisoned.into_inner()
            }
        };
        if process.broken.is_none() {
            let line = format!("{}\n", json!({"id": 0, "op": "shutdown"}));
            match process
                .stdin
                .write_all(line.as_bytes())
                .and_then(|()| process.stdin.flush())
            {
                Ok(()) => match process.replies.recv_timeout(Duration::from_secs(10)) {
                    Ok(Ok(_)) => {}
                    Ok(Err(message)) => log(format!("historical backend shutdown: {message}")),
                    Err(e) => log(format!("historical backend shutdown unanswered: {e}")),
                },
                Err(e) => log(format!("historical backend shutdown not sent: {e}")),
            }
            if !process.exited_within(Duration::from_secs(10)) {
                log("historical backend did not exit after shutdown; stopping it".into());
            }
        }
        // `Process::drop` stops and reaps whatever is still running.
    }
}

struct Branch {
    backend: Arc<HistoricalBackend>,
    id: AtomicU64,
    /// Forks close their branch when the last handle drops; the main branch never does.
    owned: AtomicBool,
}

impl Drop for Branch {
    fn drop(&mut self) {
        if self.owned.load(Ordering::Acquire) {
            let id = self.id.load(Ordering::Acquire);
            if let Err(error) = self.backend.request(id, json!("close")) {
                (self.backend.log)(format!("historical branch {id}: close failed: {error}"));
            }
        }
    }
}

/// A handle to one isolated branch of the historical session. Cloning shares the branch;
/// `fork` copies it.
#[derive(Clone)]
pub struct HistoricalVm {
    branch: Arc<Branch>,
}

impl HistoricalVm {
    pub fn main(backend: Arc<HistoricalBackend>) -> Self {
        Self {
            branch: Arc::new(Branch {
                backend,
                id: AtomicU64::new(0),
                owned: AtomicBool::new(false),
            }),
        }
    }

    pub fn backend(&self) -> &Arc<HistoricalBackend> {
        &self.branch.backend
    }

    /// The backend branch this handle currently names.
    pub fn branch_id(&self) -> u64 {
        self.branch.id.load(Ordering::Acquire)
    }

    fn request(&self, op: Value) -> SurfpoolResult<Value> {
        self.branch.backend.request(self.branch_id(), op)
    }

    fn command(&self, method: &str, params: Option<Value>) -> SurfpoolResult<Value> {
        let command = match params {
            Some(params) => json!({"method": method, "params": params}),
            None => json!({"method": method}),
        };
        self.request(json!({"command": command}))
    }

    /// An isolated copy of this branch's current state, closed when its last handle drops.
    pub fn fork(&self) -> SurfpoolResult<Self> {
        let reply = self.request(json!("fork"))?;
        let id = reply["branch"]
            .as_u64()
            .ok_or_else(|| protocol("fork reply has no branch"))?;
        Ok(Self {
            branch: Arc::new(Branch {
                backend: self.branch.backend.clone(),
                id: AtomicU64::new(id),
                owned: AtomicBool::new(true),
            }),
        })
    }

    /// Makes `target` continue as this fork of it. The backend refuses when `target`
    /// changed after the fork. Afterwards `target` names (and keeps its ownership of) the
    /// promoted branch, and this handle no longer closes it.
    pub fn commit_into(&self, target: &Self) -> SurfpoolResult<()> {
        let id = self.branch_id();
        self.request(json!({"commit": {"into": target.branch_id()}}))?;
        target.branch.id.store(id, Ordering::Release);
        self.branch.owned.store(false, Ordering::Release);
        Ok(())
    }

    pub fn state(&self) -> SurfpoolResult<Value> {
        self.command("state", None)
    }

    pub fn accounts(&self, pubkeys: &[Pubkey]) -> SurfpoolResult<Vec<Option<Account>>> {
        let mut accounts = Vec::with_capacity(pubkeys.len());
        for chunk in pubkeys.chunks(ACCOUNTS_PER_READ) {
            let keys: Vec<String> = chunk.iter().map(Pubkey::to_string).collect();
            let reply = self.command("accounts", Some(json!({"pubkeys": keys})))?;
            let images = reply
                .as_array()
                .filter(|images| images.len() == chunk.len())
                .ok_or_else(|| protocol("account reply does not match the request"))?;
            for image in images {
                accounts.push(account(image)?);
            }
        }
        Ok(accounts)
    }

    pub fn account(&self, pubkey: &Pubkey) -> SurfpoolResult<Option<Account>> {
        Ok(self.accounts(std::slice::from_ref(pubkey))?.pop().flatten())
    }

    /// Finalized history strictly before the fork's position, read by SVM Replay (see
    /// `historical_archive`): the same for every branch.
    pub fn archive(&self, query: Value) -> SurfpoolResult<Value> {
        self.request(json!({"archive": query}))
    }

    /// Every account this branch has loaded, with `None` for a proven absence: the fork's
    /// own inventory kept by SVM Replay, not every account that existed at the slot.
    pub fn known_accounts(&self) -> SurfpoolResult<Vec<(Pubkey, Option<Account>)>> {
        let reply = self.request(json!("knownAccounts"))?;
        reply
            .as_array()
            .ok_or_else(|| protocol("known accounts reply is not a list"))?
            .iter()
            .map(|entry| {
                let pubkey = text(entry, "pubkey")?
                    .parse()
                    .map_err(|e| protocol(format!("known account key: {e}")))?;
                Ok((pubkey, account(field(entry, "account")?)?))
            })
            .collect()
    }

    /// What this branch already holds for `pubkey`, acquiring nothing: `None` when the
    /// account was never loaded (its historical image is unknown, not absent).
    pub fn peek(&self, pubkey: &Pubkey) -> SurfpoolResult<Option<Option<Account>>> {
        Ok(self
            .peek_accounts(std::slice::from_ref(pubkey))?
            .pop()
            .flatten())
    }

    /// `peek` for each of `pubkeys`, at most 100 per backend request.
    pub fn peek_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> SurfpoolResult<Vec<Option<Option<Account>>>> {
        let mut peeked = Vec::with_capacity(pubkeys.len());
        for chunk in pubkeys.chunks(100) {
            let keys: Vec<String> = chunk.iter().map(Pubkey::to_string).collect();
            let reply = self.request(json!({"peek": {"pubkeys": keys}}))?;
            let entries = reply
                .as_array()
                .filter(|entries| entries.len() == chunk.len())
                .ok_or_else(|| protocol("peek reply does not match the request"))?;
            for entry in entries {
                peeked.push(match field(entry, "loaded")?.as_bool() {
                    Some(false) => None,
                    Some(true) => Some(account(field(entry, "account")?)?),
                    None => return Err(protocol("peek loaded flag is not a boolean")),
                });
            }
        }
        Ok(peeked)
    }

    /// Replaces an account; a zero-lamport account is an explicit absence.
    pub fn set_account(&self, pubkey: &Pubkey, account: &Account) -> SurfpoolResult<()> {
        self.set_accounts(&[(*pubkey, account.clone())])
    }

    /// Replaces accounts atomically: SVM Replay validates them together (a program with
    /// its program data) and applies all or none.
    pub fn set_accounts(&self, accounts: &[(Pubkey, Account)]) -> SurfpoolResult<()> {
        let entries: Vec<Value> = accounts
            .iter()
            .map(|(pubkey, account)| {
                if account.lamports == 0 {
                    json!({"pubkey": pubkey.to_string(), "lamports": "0"})
                } else {
                    json!({"pubkey": pubkey.to_string(), "lamports": account.lamports.to_string(),
                        "owner": account.owner.to_string(), "executable": account.executable,
                        "rentEpoch": account.rent_epoch.to_string(),
                        "dataBase64": BASE64.encode(&account.data)})
                }
            })
            .collect();
        self.command("overrideAccounts", Some(json!({"entries": entries})))?;
        Ok(())
    }

    /// Executes on this branch through the backend's admission (signatures, blockhash age,
    /// durable nonces, duplicates) and commits; Surfpool runs its preflight beforehand.
    pub fn send(&self, tx: &VersionedTransaction) -> SurfpoolResult<TransactionResult> {
        let reply = self.command(
            "rpcSend",
            Some(json!({"transactionBase64": wire(tx)?, "skipPreflight": true})),
        )?;
        let output = field(&reply, "output")?;
        let meta = metadata(output, tx)?;
        match transaction_error(output)? {
            None => Ok(Ok(meta)),
            Some(err) => Ok(Err(FailedTransactionMetadata { err, meta })),
        }
    }

    /// Executes and commits without RPC admission or signature checks, as Surfpool's local
    /// LiteSVM does for instruction-prefix profiling on a disposable fork.
    pub fn send_unverified(&self, tx: &VersionedTransaction) -> SurfpoolResult<TransactionResult> {
        let output = self.command(
            "send",
            Some(json!({"transactionBase64": wire(tx)?, "sigVerify": false})),
        )?;
        let meta = metadata(&output, tx)?;
        match transaction_error(&output)? {
            None => Ok(Ok(meta)),
            Some(err) => Ok(Err(FailedTransactionMetadata { err, meta })),
        }
    }

    /// Checks lifetime without signatures before executing on a disposable bundle branch.
    pub fn send_simulated(&self, tx: &VersionedTransaction) -> SurfpoolResult<TransactionResult> {
        let reply = self.command(
            "rpcSimulate",
            Some(json!({"transactionBase64": wire(tx)?, "sigVerify": false,
                "replaceRecentBlockhash": false})),
        )?;
        let output = field(&reply, "output")?;
        if let Some(err) = transaction_error(output)? {
            return Ok(Err(FailedTransactionMetadata {
                err,
                meta: metadata(output, tx)?,
            }));
        }
        self.send_unverified(tx)
    }

    /// Simulates on this branch; `accounts` get their post-execution images.
    pub fn simulate(
        &self,
        tx: &VersionedTransaction,
        sig_verify: bool,
        accounts: &[Pubkey],
    ) -> SurfpoolResult<Result<SimulatedTransactionInfo, FailedTransactionMetadata>> {
        let keys: Vec<String> = accounts.iter().map(Pubkey::to_string).collect();
        let reply = self.request(json!({"simulate": {"transactionBase64": wire(tx)?,
            "sigVerify": sig_verify, "replaceRecentBlockhash": false, "accounts": keys}}))?;
        let output = field(field(&reply, "simulation")?, "output")?;
        let meta = metadata(output, tx)?;
        if let Some(err) = transaction_error(output)? {
            return Ok(Err(FailedTransactionMetadata { err, meta }));
        }
        let images = field(&reply, "postAccounts")?
            .as_array()
            .filter(|images| images.len() == accounts.len())
            .ok_or_else(|| protocol("simulation post accounts do not match the request"))?;
        let mut post_accounts = Vec::with_capacity(images.len());
        for (pubkey, image) in accounts.iter().zip(images) {
            // A proven absence after execution is an empty account, as LiteSVM reports it.
            let account = account(image)?.unwrap_or_default();
            post_accounts.push((*pubkey, AccountSharedData::from(account)));
        }
        Ok(Ok(SimulatedTransactionInfo {
            meta,
            post_accounts,
        }))
    }

    /// The fee for a serialized message, or none when its recent blockhash is not (or no
    /// longer) valid on this fork.
    pub fn fee_for_message(&self, message: &[u8]) -> SurfpoolResult<Option<u64>> {
        let reply = self.command(
            "feeForMessage",
            Some(json!({"messageBase64": BASE64.encode(message)})),
        )?;
        match reply {
            Value::Null => Ok(None),
            reply => reply
                .as_u64()
                .map(Some)
                .ok_or_else(|| protocol(format!("fee reply is not a number: {reply}"))),
        }
    }

    /// Whether a blockhash is accepted for processing on this fork now. SVM Replay decides:
    /// an archived blockhash's age comes from the boundary block or its parent headers.
    pub fn blockhash_valid(&self, blockhash: &Hash) -> SurfpoolResult<bool> {
        let reply = self.command(
            "blockhashValid",
            Some(json!({"blockhash": blockhash.to_string()})),
        )?;
        reply.as_bool().ok_or_else(|| {
            protocol(format!(
                "blockhash validity reply is not a boolean: {reply}"
            ))
        })
    }

    pub fn rent(&self, data_len: usize) -> SurfpoolResult<u64> {
        let reply = self.command("rent", Some(json!({"dataLen": data_len})))?;
        reply
            .as_u64()
            .ok_or_else(|| protocol(format!("rent reply is not a number: {reply}")))
    }

    /// The branch's latest blockhash and its last valid block height.
    pub fn latest_blockhash(&self) -> SurfpoolResult<(Hash, u64)> {
        let reply = self.command("latestBlockhash", None)?;
        let hash = reply["blockhash"]
            .as_str()
            .and_then(|hash| hash.parse().ok())
            .ok_or_else(|| protocol("latest blockhash missing"))?;
        let height = reply["lastValidBlockHeight"]
            .as_u64()
            .ok_or_else(|| protocol("last valid block height missing"))?;
        Ok((hash, height))
    }

    pub fn set_balance(&self, pubkey: &Pubkey, lamports: u64) -> SurfpoolResult<()> {
        self.command(
            "setBalance",
            Some(json!({"pubkey": pubkey.to_string(), "lamports": lamports.to_string()})),
        )?;
        Ok(())
    }

    /// Local slots inside the boundary's epoch; the clock's time moves only when given.
    /// `synthetic_epoch` opts in to a synthetic test warp into later epochs: SVM Replay moves
    /// the Clock there and keeps runtime, features, stakes and rewards at the boundary.
    pub fn advance(
        &self,
        slots: u64,
        unix_timestamp: Option<i64>,
        synthetic_epoch: bool,
    ) -> SurfpoolResult<Value> {
        let mut params = json!({"slots": slots, "unixTimestamp": unix_timestamp});
        if synthetic_epoch {
            params["syntheticEpoch"] = json!(true);
        }
        self.command("advance", Some(params))
    }

    /// Back to the historical boundary state.
    pub fn reset(&self) -> SurfpoolResult<Value> {
        self.command("reset", None)
    }

    /// Puts accounts back to their boundary images, discarding local edits on this branch.
    pub fn restore(&self, pubkeys: &[Pubkey]) -> SurfpoolResult<()> {
        let keys: Vec<String> = pubkeys.iter().map(Pubkey::to_string).collect();
        self.request(json!({"restore": {"pubkeys": keys}}))?;
        Ok(())
    }

    pub fn revision(&self) -> SurfpoolResult<u64> {
        let reply = self.request(json!("revision"))?;
        reply["revision"]
            .as_u64()
            .ok_or_else(|| protocol("revision missing"))
    }
}

fn wire(tx: &VersionedTransaction) -> SurfpoolResult<String> {
    wincode::serialize(tx)
        .map(|bytes| BASE64.encode(bytes))
        .map_err(|e| protocol(format!("encode transaction: {e}")))
}

fn field<'a>(value: &'a Value, name: &str) -> SurfpoolResult<&'a Value> {
    value
        .get(name)
        .ok_or_else(|| protocol(format!("backend reply lacks {name}")))
}

fn text<'a>(value: &'a Value, name: &str) -> SurfpoolResult<&'a str> {
    field(value, name)?
        .as_str()
        .ok_or_else(|| protocol(format!("{name} is not text")))
}

fn bytes(value: &Value, name: &str) -> SurfpoolResult<Vec<u8>> {
    BASE64
        .decode(text(value, name)?)
        .map_err(|e| protocol(format!("{name}: {e}")))
}

/// An account image from the backend; `null` is a proven absence at this state.
fn account(image: &Value) -> SurfpoolResult<Option<Account>> {
    if image.is_null() {
        return Ok(None);
    }
    let number = |name: &str| {
        text(image, name)?
            .parse::<u64>()
            .map_err(|e| protocol(format!("account image {name}: {e}")))
    };
    Ok(Some(Account {
        lamports: number("lamports")?,
        owner: text(image, "owner")?
            .parse()
            .map_err(|e| protocol(format!("account image owner: {e}")))?,
        executable: field(image, "executable")?
            .as_bool()
            .ok_or_else(|| protocol("account image executable is not a boolean"))?,
        rent_epoch: number("rentEpoch")?,
        data: bytes(image, "dataBase64")?,
    }))
}

/// `null` is the native schema's successful execution; a missing field is a protocol error.
fn transaction_error(output: &Value) -> SurfpoolResult<Option<TransactionError>> {
    match field(output, "normalizedError")? {
        Value::Null => Ok(None),
        Value::String(error) => serde_json::from_str(error)
            .map(Some)
            .map_err(|e| protocol(format!("transaction error {error}: {e}"))),
        _ => Err(protocol("transaction error is neither null nor text")),
    }
}

/// The runtime's transaction metadata as LiteSVM reports it: one inner-instruction list per
/// top-level instruction, from the runtime trace (never inferred from logs).
fn metadata(output: &Value, tx: &VersionedTransaction) -> SurfpoolResult<TransactionMetadata> {
    let signature = tx
        .signatures
        .first()
        .copied()
        .ok_or_else(|| protocol("transaction has no signature"))?;
    let logs = field(output, "logs")?
        .as_array()
        .ok_or_else(|| protocol("logs are not a list"))?
        .iter()
        .map(|log| log.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| protocol("a log line is not text"))?;
    let compute_units_consumed = field(output, "computeUnits")?
        .as_u64()
        .ok_or_else(|| protocol("compute units are not a number"))?;
    // The native schema reports no fee (null) for a transaction rejected before charging.
    let fee = match field(output, "fee")? {
        Value::Null => 0,
        Value::String(fee) => fee
            .parse()
            .map_err(|e| protocol(format!("fee {fee}: {e}")))?,
        _ => return Err(protocol("fee is neither null nor text")),
    };
    let return_data = match field(output, "returnData")? {
        Value::Null => TransactionReturnData::default(),
        data => TransactionReturnData {
            program_id: text(data, "programId")?
                .parse()
                .map_err(|e| protocol(format!("return data program: {e}")))?,
            data: bytes(data, "dataBase64")?,
        },
    };
    let mut inner_instructions: InnerInstructionsList =
        vec![Vec::new(); tx.message.instructions().len()];
    let groups = field(field(output, "instructionTrace")?, "groups")?
        .as_array()
        .ok_or_else(|| protocol("instruction trace groups are not a list"))?;
    for group in groups {
        let index = field(group, "index")?
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| protocol("trace group index is not a number"))?;
        let list = inner_instructions
            .get_mut(index)
            .ok_or_else(|| protocol("trace group index out of range"))?;
        let entries = field(group, "instructions")?
            .as_array()
            .ok_or_else(|| protocol("trace group instructions are not a list"))?;
        for entry in entries {
            let small = |name: &str| {
                field(entry, name)?
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .ok_or_else(|| protocol(format!("trace {name} is not a byte")))
            };
            let accounts = field(entry, "accounts")?
                .as_array()
                .ok_or_else(|| protocol("trace accounts are not a list"))?
                .iter()
                .map(|index| index.as_u64().and_then(|index| u8::try_from(index).ok()))
                .collect::<Option<Vec<u8>>>()
                .ok_or_else(|| protocol("trace account index is not a byte"))?;
            list.push(InnerInstruction {
                instruction: CompiledInstruction {
                    program_id_index: small("programIdIndex")?,
                    accounts,
                    data: bytes(entry, "dataBase64")?,
                },
                stack_height: small("stackHeight")?,
            });
        }
    }
    Ok(TransactionMetadata {
        signature,
        logs,
        inner_instructions,
        compute_units_consumed,
        return_data,
        fee,
        loaded_accounts_data_size: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_errors_keep_their_code_and_provider_details() {
        let error = backend_error(
            &json!({"code": "SOURCE_TRANSPORT", "message": "provider unreachable",
            "details": {"provider": "alchemy"}}),
        );
        // Logs show the code and message; RPC clients still get the details.
        assert_eq!(error.to_string(), "SOURCE_TRANSPORT: provider unreachable");
        let rpc: jsonrpc_core::Error = error.into();
        assert!(rpc.message.starts_with("SOURCE_TRANSPORT: "));
        assert_eq!(rpc.data.unwrap()["details"]["provider"], json!("alchemy"));
    }

    #[test]
    fn account_images_keep_u64s_and_a_null_image_is_an_absence() {
        let image = json!({"pubkey": "x", "lamports": "18446744073709551615",
            "owner": "11111111111111111111111111111111", "executable": false,
            "rentEpoch": "18446744073709551615", "dataBase64": "AQID"});
        let account = account(&image).unwrap().unwrap();
        assert_eq!((account.lamports, account.rent_epoch), (u64::MAX, u64::MAX));
        assert_eq!(account.data, vec![1, 2, 3]);
        assert_eq!(super::account(&Value::Null).unwrap(), None);
        assert!(super::account(&json!({"lamports": "1"})).is_err());
    }

    fn two_instruction_transaction() -> VersionedTransaction {
        use solana_keypair::Keypair;
        use solana_message::{Message, VersionedMessage};
        use solana_signer::Signer;
        let payer = Keypair::new();
        let ix = solana_system_interface::instruction::transfer(
            &payer.pubkey(),
            &Pubkey::new_unique(),
            1,
        );
        let message = Message::new(&[ix.clone(), ix], Some(&payer.pubkey()));
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&payer]).unwrap()
    }

    #[test]
    fn v1_uses_the_canonical_wire_format_not_legacy_bincode() {
        let tx = VersionedTransaction {
            signatures: vec![Default::default()],
            message: solana_message::VersionedMessage::V1(solana_message::v1::Message {
                header: solana_message::MessageHeader {
                    num_required_signatures: 1,
                    ..Default::default()
                },
                account_keys: vec![Pubkey::new_unique()],
                ..Default::default()
            }),
        };
        let bytes = BASE64.decode(wire(&tx).unwrap()).unwrap();
        assert_eq!(bytes, wincode::serialize(&tx).unwrap());
        assert_eq!(bytes[0], solana_message::v1::V1_PREFIX);
        assert_eq!(
            wincode::deserialize::<VersionedTransaction>(&bytes).unwrap(),
            tx
        );
    }

    #[test]
    fn legacy_and_v0_wire_bytes_remain_identical() {
        use solana_keypair::Keypair;
        use solana_message::{Message, VersionedMessage};
        use solana_signer::Signer;
        let payer = Keypair::new();
        let ix = solana_system_interface::instruction::transfer(
            &payer.pubkey(),
            &Pubkey::new_unique(),
            1,
        );
        let hash = Hash::new_unique();
        let legacy = Message::new_with_blockhash(&[ix.clone()], Some(&payer.pubkey()), &hash);
        let v0 =
            solana_message::v0::Message::try_compile(&payer.pubkey(), &[ix], &[], hash).unwrap();
        for message in [VersionedMessage::Legacy(legacy), VersionedMessage::V0(v0)] {
            let tx = VersionedTransaction::try_new(message, &[&payer]).unwrap();
            let bytes = BASE64.decode(wire(&tx).unwrap()).unwrap();
            assert_eq!(bytes, bincode::serialize(&tx).unwrap());
            assert_eq!(
                wincode::deserialize::<VersionedTransaction>(&bytes).unwrap(),
                tx
            );
        }
    }

    fn output() -> Value {
        json!({"logs": ["a"], "computeUnits": 300, "fee": "5000",
            "returnData": {"programId": "11111111111111111111111111111111", "dataBase64": "BQ=="},
            "normalizedError": "{\"InstructionError\":[1,{\"Custom\":7}]}",
            "instructionTrace": {"groups": [{"index": 1, "instructions": [
                {"path": [1, 0], "stackHeight": 2, "programIdIndex": 2, "accounts": [0, 1], "dataBase64": "AA=="}]}]}})
    }

    #[test]
    fn runtime_traces_become_one_inner_list_per_top_level_instruction() {
        let tx = two_instruction_transaction();
        let meta = metadata(&output(), &tx).unwrap();
        assert_eq!(meta.inner_instructions.len(), 2);
        assert!(meta.inner_instructions[0].is_empty());
        assert_eq!(meta.inner_instructions[1][0].stack_height, 2);
        assert_eq!((meta.fee, meta.compute_units_consumed), (5000, 300));
        assert_eq!(meta.loaded_accounts_data_size, None);
        assert_eq!(meta.return_data.data, vec![5]);
        assert_eq!(
            transaction_error(&output()).unwrap(),
            Some(TransactionError::InstructionError(
                1,
                solana_instruction::error::InstructionError::Custom(7)
            ))
        );
    }

    #[test]
    fn historical_metadata_never_fabricates_a_local_vm_measurement() {
        let tx = two_instruction_transaction();
        for error in [Value::Null, output()["normalizedError"].clone()] {
            let mut result = output();
            result["normalizedError"] = error;
            let receipt = metadata(&result, &tx).unwrap();
            assert_eq!(receipt.loaded_accounts_data_size, None);
            assert!(serde_json::to_value(receipt).unwrap()["loaded_accounts_data_size"].is_null());
        }
    }

    #[test]
    fn missing_or_malformed_metadata_is_a_protocol_error_not_a_default() {
        let tx = two_instruction_transaction();
        for (path, value) in [
            ("/normalizedError", None),
            ("/fee", None),
            ("/returnData", None),
            ("/returnData/dataBase64", None),
            ("/instructionTrace", None),
            ("/instructionTrace/groups/0/instructions", None),
            ("/instructionTrace/groups/0/instructions/0/dataBase64", None),
            ("/instructionTrace/groups/0/index", Some(json!(9))),
            ("/logs", Some(json!("a"))),
        ] {
            let mut broken = output();
            let (parent, key) = path.rsplit_once('/').unwrap();
            let target = broken
                .pointer_mut(if parent.is_empty() { "" } else { parent })
                .unwrap();
            match value {
                None => {
                    target.as_object_mut().unwrap().remove(key);
                }
                Some(value) => target[key] = value,
            }
            let failed = metadata(&broken, &tx).is_err() || transaction_error(&broken).is_err();
            assert!(failed, "{path} must not default");
        }
        // Explicit nulls are the native schema's "none" values.
        let mut rejected = output();
        rejected["fee"] = Value::Null;
        rejected["returnData"] = Value::Null;
        rejected["normalizedError"] = Value::Null;
        let meta = metadata(&rejected, &tx).unwrap();
        assert_eq!(meta.fee, 0);
        assert_eq!(transaction_error(&rejected).unwrap(), None);
    }
}
