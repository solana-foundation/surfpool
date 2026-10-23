//! One surfnet's lifecycle, shared by the CLI, the Rust and Node SDKs and MCP. Core opens the
//! instance (it selects the execution mode and checks the store before anything is seeded),
//! runs it, and shuts it down; frontends keep their presentation, threads, readiness reporting
//! and feature-specific configuration.
//!
//! A normal instance is built when it opens. A historical instance (`SurfnetSvmConfig::historical`)
//! opens its store only: SVM Replay's backend starts when the instance runs, cancellably, and
//! the surfnet is built on the attached fork; nothing local is seeded for it.
//!
//! Closing is `SimnetCommand::Terminate` on the commands channel given to [`SurfnetInstance::run`]:
//! the runloop stops the instance's servers (and its SVM Replay backend first) and shuts its state
//! down, and the storage, its namespace lease and the backend close with the last handle. However
//! `run` returns, a historical instance's backend process group ends with it.
//!
//! A persistent store is leased before it is opened: a historical writer exclusively, a normal
//! surfnet shared (normal surfnets coexist, but never with a historical writer). Sandbox and profiling copies live
//! inside an instance and share its resources; they are never instances of their own.

use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender};
use surfpool_types::{
    HistoricalSimnetConfig, SimnetCommand, SimnetEvent, SimnetEventsTx, SurfnetStartupStatus,
    SurfpoolConfig,
};

use crate::{
    error::{SurfpoolError, SurfpoolResult},
    runloops,
    storage::{Storage, StorageBackend, historical_lease::HistoricalStoreLease},
    surfnet::{
        BlockHeader, GeyserEvent,
        backend::historical::{self, HistoricalLaunch, HistoricalLog, HistoricalSession},
        locker::SurfnetSvmLocker,
        svm::{HISTORICAL_SLOT_CHECKPOINT, SurfnetChannels, SurfnetSvm, SurfnetSvmConfig},
    },
};

/// An opened surfnet, ready to run.
pub struct SurfnetInstance {
    opened: Opened,
    simnet_events_tx: SimnetEventsTx,
    startup_status_rx: tokio::sync::watch::Receiver<SurfnetStartupStatus>,
    core_started_rx: tokio::sync::watch::Receiver<bool>,
}

enum Opened {
    /// A normal surfnet, built when it opened.
    Local {
        locker: SurfnetSvmLocker,
        geyser_events_rx: Receiver<GeyserEvent>,
    },
    /// A historical surfnet: its store is open and checked; SVM Replay attaches when it runs.
    Historical(Box<PendingHistorical>),
}

struct PendingHistorical {
    storage_backend: StorageBackend,
    store_lease: Option<Arc<Mutex<HistoricalStoreLease>>>,
    channels: SurfnetChannels,
    geyser_events_rx: Receiver<GeyserEvent>,
    config: SurfnetSvmConfig,
    historical: HistoricalSimnetConfig,
    /// Whether the store already holds this fork (it reopens through SVM Replay's journal).
    stored_fork: bool,
}

/// A historical surfnet's SVM Replay backend, ended when its run returns, however it returns.
struct EndsWithRun(HistoricalSession);

impl Drop for EndsWithRun {
    fn drop(&mut self) {
        self.0.terminate();
    }
}

/// Whether a historical launch journals its committed operations into the store's journal.
fn journaled(historical: &HistoricalSimnetConfig) -> bool {
    historical.args.iter().any(|arg| arg == "--journal")
}

/// One store holds one chain: a historical fork's store reopens only as that fork, with its
/// journal, and a new fork never starts in a store that holds another chain. Checked before the
/// store is seeded. Returns whether the store holds a historical fork.
fn check_store(
    storage_backend: &StorageBackend,
    historical: Option<&HistoricalSimnetConfig>,
) -> SurfpoolResult<bool> {
    let slot_checkpoint: Box<dyn Storage<String, u64>> =
        storage_backend.open_store("slot_checkpoint")?;
    let blocks: Box<dyn Storage<u64, BlockHeader>> = storage_backend.open_store("blocks")?;
    let checkpoint = |key: &str| slot_checkpoint.get(&key.to_string());
    let stored_fork = checkpoint(HISTORICAL_SLOT_CHECKPOINT)?.is_some();
    let stored_chain = checkpoint("latest_slot")?.is_some() || blocks.count()? > 0;
    let refusal = match (historical, stored_fork, stored_chain) {
        (None, true, _) => {
            Some("the surfnet's store holds a historical fork; start it with its --after")
        }
        (Some(historical), true, _) if !journaled(historical) => Some(
            "the surfnet's store holds a historical fork, but no journal was configured for it",
        ),
        (Some(_), false, true) => Some(
            "the surfnet's store holds another chain; a historical fork needs its own --db or --surfnet-id",
        ),
        _ => None,
    };
    match refusal {
        Some(message) => Err(SurfpoolError::historical(
            "SESSION_INTEGRITY",
            message,
            None,
        )),
        None => Ok(stored_fork),
    }
}

impl SurfnetInstance {
    /// The startup factory: opens the storage `database_url` names (in memory when `None`),
    /// checks that it holds this surfnet's chain, and, for a normal surfnet, builds it on the
    /// local backend. A historical surfnet's backend attaches in [`Self::run`]. Returns the
    /// instance and the events through which the frontend observes its readiness, errors and
    /// shutdown.
    pub fn open(
        database_url: Option<&str>,
        config: SurfnetSvmConfig,
    ) -> SurfpoolResult<(Self, Receiver<SimnetEvent>)> {
        let (channels, simnet_events_rx, geyser_events_rx) = SurfnetChannels::new();
        let simnet_events_tx = channels.simnet_events_tx.clone();
        let startup_status_rx = channels.startup_status_watch_tx.subscribe();
        let core_started_rx = channels.core_started_watch_tx.subscribe();
        let store_lease = match config.historical {
            Some(_) => HistoricalStoreLease::acquire(database_url, &config.surfnet_id),
            None => HistoricalStoreLease::normal(database_url, &config.surfnet_id),
        }
        .map_err(SurfpoolError::store_lease)?
        .map(|lease| Arc::new(Mutex::new(lease)));
        let storage_backend = StorageBackend::open(&database_url, &config.surfnet_id)?;
        let stored_fork = check_store(&storage_backend, config.historical.as_ref())?;
        let opened = match config.historical.clone() {
            None => Opened::Local {
                locker: SurfnetSvmLocker::new(SurfnetSvm::build_with(
                    storage_backend,
                    store_lease,
                    channels,
                    config,
                    None,
                )?),
                geyser_events_rx,
            },
            Some(historical) => Opened::Historical(Box::new(PendingHistorical {
                storage_backend,
                store_lease,
                channels,
                geyser_events_rx,
                config,
                historical,
                stored_fork,
            })),
        };
        Ok((
            Self {
                opened,
                simnet_events_tx,
                startup_status_rx,
                core_started_rx,
            },
            simnet_events_rx,
        ))
    }

    /// A handle to this instance's state. Handles share the instance; they never own a
    /// lifecycle of their own. A historical instance has no state before it runs: its surfnet
    /// is built on the fork SVM Replay attaches in [`Self::run`].
    pub fn locker(&self) -> Option<&SurfnetSvmLocker> {
        match &self.opened {
            Opened::Local { locker, .. } => Some(locker),
            Opened::Historical(_) => None,
        }
    }

    /// The instance's event sender, for frontend reports before and during the run.
    pub fn simnet_events_tx(&self) -> SimnetEventsTx {
        self.simnet_events_tx.clone()
    }

    /// Startup-status transitions since the instance opened (none is missed).
    pub fn subscribe_startup_status(&self) -> tokio::sync::watch::Receiver<SurfnetStartupStatus> {
        self.startup_status_rx.clone()
    }

    /// Turns true once the core runloop serves; a historical fork's preparation can take long.
    pub fn subscribe_core_started(&self) -> tokio::sync::watch::Receiver<bool> {
        self.core_started_rx.clone()
    }

    /// Whether SVM Replay executes this surfnet.
    pub fn is_historical(&self) -> bool {
        matches!(self.opened, Opened::Historical(_))
    }

    /// Runs the instance until `SimnetCommand::Terminate` or a fatal error (the frontend chooses
    /// the thread and runtime). A historical instance first starts SVM Replay's backend; a
    /// Terminate received meanwhile cancels that start, and other commands wait for the fork.
    pub async fn run(
        self,
        config: SurfpoolConfig,
        simnet_commands_tx: Sender<SimnetCommand>,
        simnet_commands_rx: Receiver<SimnetCommand>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (locker, geyser_events_rx, _backend) = match self.opened {
            Opened::Local {
                locker,
                geyser_events_rx,
            } => (locker, geyser_events_rx, None),
            Opened::Historical(pending) => {
                let PendingHistorical {
                    storage_backend,
                    store_lease,
                    channels,
                    geyser_events_rx,
                    config: svm_config,
                    historical,
                    stored_fork,
                } = *pending;
                let events = self.simnet_events_tx.clone();
                let log: HistoricalLog =
                    Arc::new(move |line| events.info(format!("svm-replay: {line}")));
                // Before the fork is ready only Terminate matters; other commands wait for it.
                let mut deferred = Vec::new();
                let mut terminated = false;
                // SVM Replay requires the journal of a store with history and refuses one for a
                // store without: a store and its journal never part ways.
                let mut launch = HistoricalLaunch::from(&historical);
                if journaled(&historical) {
                    launch.args.extend([
                        "--journal-require".into(),
                        if stored_fork { "existing" } else { "new" }.into(),
                    ]);
                }
                let session = historical::launch(&launch, log, || {
                    while let Ok(command) = simnet_commands_rx.try_recv() {
                        match command {
                            SimnetCommand::Terminate(_) => terminated = true,
                            command => deferred.push(command),
                        }
                    }
                    terminated
                });
                if terminated {
                    storage_backend.shutdown();
                    return Ok(());
                }
                for command in deferred {
                    let _ = simnet_commands_tx.send(command);
                }
                let session = session.inspect_err(|error| {
                    self.simnet_events_tx
                        .error(format!("Historical fork failed to start: {error}"))
                })?;
                // However this run ends, a failed attach included, the backend ends with it.
                let backend = EndsWithRun(session.clone());
                let svm = session
                    .attach()
                    .and_then(|fork| {
                        SurfnetSvm::build_with(
                            storage_backend,
                            store_lease,
                            channels,
                            svm_config,
                            Some(fork),
                        )
                    })
                    .inspect_err(|error| {
                        self.simnet_events_tx
                            .error(format!("Historical fork failed to start: {error}"))
                    })?;
                // The historical clock is in place: interfaces waiting for the fork can show it.
                self.simnet_events_tx
                    .system_clock_updated(svm.inner.clock());
                (SurfnetSvmLocker::new(svm), geyser_events_rx, Some(backend))
            }
        };
        runloops::start_local_surfnet_runloop(
            locker,
            config,
            simnet_commands_tx,
            simnet_commands_rx,
            geyser_events_rx,
        )
        .await
    }
}

/// Ends every running historical backend of this process (a frontend that is exiting).
pub fn terminate_historical_backends() {
    historical::terminate_all();
}

#[cfg(all(test, unix, feature = "sqlite"))]
mod tests {
    use std::{
        path::Path,
        time::{Duration, Instant},
    };

    use surfpool_types::{HistoricalSimnetConfig, SimnetCommand, SimnetConfig, SurfpoolConfig};

    use super::SurfnetInstance;
    use crate::{
        storage::historical_lease::HistoricalStoreLease,
        surfnet::svm::{SurfnetSvm, SurfnetSvmConfig},
    };

    /// A historical surfnet whose stand-in backend runs `script` instead of SVM Replay: what a
    /// failing or slow backend start looks like to the lifecycle.
    fn historical(script: &str) -> SurfnetSvmConfig {
        SurfnetSvmConfig {
            historical: Some(HistoricalSimnetConfig {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), script.into(), "sh".into()],
                envs: vec![],
            }),
            ..SurfnetSvmConfig::default()
        }
    }

    fn surfpool_config() -> SurfpoolConfig {
        SurfpoolConfig {
            simnets: vec![SimnetConfig::default()],
            ..SurfpoolConfig::default()
        }
    }

    fn run(instance: SurfnetInstance) -> Result<(), String> {
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(instance.run(surfpool_config(), commands_tx, commands_rx))
            .map_err(|e| e.to_string())
    }

    #[test]
    fn a_historical_instance_owns_its_namespace_until_it_ends() {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("surfnet.sqlite");
        let db = db.to_str().unwrap();
        let writer = SurfnetInstance::open(Some(db), historical("exit 3")).unwrap();
        for config in [historical("exit 3"), SurfnetSvmConfig::default()] {
            let refused = SurfnetInstance::open(Some(db), config).err().unwrap();
            assert!(refused.to_string().contains("SESSION_IN_USE"), "{refused}");
        }
        drop(writer);
        assert!(SurfnetInstance::open(Some(db), historical("exit 3")).is_ok());
    }

    #[test]
    fn normal_instances_share_their_namespace_and_keep_historical_writers_out() {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("surfnet.sqlite");
        let db = db.to_str().unwrap();
        let first = SurfnetInstance::open(Some(db), SurfnetSvmConfig::default()).unwrap();
        let second = SurfnetInstance::open(Some(db), SurfnetSvmConfig::default()).unwrap();
        let refused = SurfnetInstance::open(Some(db), historical("exit 3"))
            .err()
            .unwrap();
        assert!(refused.to_string().contains("SESSION_IN_USE"), "{refused}");
        drop((first, second));
        assert!(
            HistoricalStoreLease::acquire(Some(db), "default")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_failed_historical_start_releases_the_namespace() {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("surfnet.sqlite");
        let db = db.to_str().unwrap();
        let (instance, _events) = SurfnetInstance::open(Some(db), historical("exit 3")).unwrap();
        assert!(run(instance).is_err());
        assert!(
            HistoricalStoreLease::acquire(Some(db), "default")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_failed_bind_releases_the_namespace() {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("surfnet.sqlite");
        let db = db.to_str().unwrap();
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (instance, _events) =
            SurfnetInstance::open(Some(db), SurfnetSvmConfig::default()).unwrap();
        let config = SurfpoolConfig {
            simnets: vec![SimnetConfig {
                offline_mode: true,
                ..SimnetConfig::default()
            }],
            rpc: surfpool_types::RpcConfig {
                bind_host: "127.0.0.1".into(),
                bind_port: occupied.local_addr().unwrap().port(),
                ..surfpool_types::RpcConfig::default()
            },
            ..SurfpoolConfig::default()
        };
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let failed = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(instance.run(config, commands_tx, commands_rx));
        assert!(failed.is_err());
        assert!(
            HistoricalStoreLease::acquire(Some(db), "default")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_cancelled_historical_start_leaves_no_backend_and_releases_the_namespace() {
        let directory = tempfile::tempdir().unwrap();
        let db = directory
            .path()
            .join("surfnet.sqlite")
            .to_str()
            .unwrap()
            .to_string();
        let pid_file = directory.path().join("backend.pid");
        let script = format!("echo $$ > {}; exec sleep 600", pid_file.display());
        let (instance, _events) = SurfnetInstance::open(Some(&db), historical(&script)).unwrap();
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let terminate = commands_tx.clone();
        let running = std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(instance.run(surfpool_config(), commands_tx, commands_rx))
                .map_err(|e| e.to_string())
        });
        // The stand-in backend has started and will never be ready; then the surfnet is stopped.
        let deadline = Instant::now() + Duration::from_secs(60);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "the stand-in backend never started"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        terminate.send(SimnetCommand::Terminate(None)).unwrap();
        assert_eq!(running.join().unwrap(), Ok(()));
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "the backend outlived its cancelled start"
        );
        assert!(
            HistoricalStoreLease::acquire(Some(&db), "default")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn low_level_persistent_constructors_take_the_shared_lease_and_clones_share_it() {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("surfnet.sqlite");
        let db = db.to_str().unwrap();
        let writer = HistoricalStoreLease::acquire(Some(db), "default").unwrap();
        let refused = SurfnetSvm::new_with_db(Some(db), SurfnetSvmConfig::default())
            .err()
            .unwrap();
        assert!(refused.to_string().contains("SESSION_IN_USE"), "{refused}");
        drop(writer);
        let (svm, _events, _geyser) =
            SurfnetSvm::new_with_db(Some(db), SurfnetSvmConfig::default()).unwrap();
        let clone = svm.clone_for_profiling().unwrap();
        drop(svm);
        // The clone holds the instance's lease: it ends with the last handle.
        assert!(
            HistoricalStoreLease::acquire(Some(db), "default")
                .err()
                .unwrap()
                .starts_with("SESSION_IN_USE:")
        );
        drop(clone);
        assert!(
            HistoricalStoreLease::acquire(Some(db), "default")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn low_level_constructors_refuse_a_historical_config() {
        let refused = SurfnetSvm::new_with_db(None, historical("exit 3"))
            .err()
            .unwrap();
        assert!(
            refused
                .to_string()
                .contains("UNSUPPORTED_IN_HISTORICAL_MODE"),
            "{refused}"
        );
    }
}
