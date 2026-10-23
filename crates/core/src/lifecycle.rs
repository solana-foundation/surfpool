//! One surfnet's lifecycle, shared by the CLI, the Rust and Node SDKs and MCP. Core opens the
//! instance (its storage and execution backend), runs it, and shuts it down; frontends keep
//! their presentation, threads, readiness reporting and feature-specific configuration.
//!
//! Closing is `SimnetCommand::Terminate` on the commands channel given to [`SurfnetInstance::run`]:
//! the runloop stops the instance's servers and shuts its state down, and the storage and backend
//! close with the last handle. Sandbox and profiling copies live inside an instance and share its
//! resources; they are never instances of their own.

use crossbeam_channel::{Receiver, Sender};
use surfpool_types::{SimnetCommand, SimnetEvent, SurfpoolConfig};

use crate::{
    error::SurfpoolResult,
    runloops,
    surfnet::{
        GeyserEvent,
        locker::SurfnetSvmLocker,
        svm::{SurfnetSvm, SurfnetSvmConfig},
    },
};

/// An opened surfnet, ready to run.
pub struct SurfnetInstance {
    locker: SurfnetSvmLocker,
    geyser_events_rx: Receiver<GeyserEvent>,
}

impl SurfnetInstance {
    /// The startup factory: opens the storage `database_url` names (in memory when `None`) and
    /// the normal (local) execution backend. Returns the instance and the events through which
    /// the frontend observes its readiness, errors and shutdown.
    pub fn open(
        database_url: Option<&str>,
        config: SurfnetSvmConfig,
    ) -> SurfpoolResult<(Self, Receiver<SimnetEvent>)> {
        let (svm, simnet_events_rx, geyser_events_rx) =
            SurfnetSvm::new_with_db(database_url, config)?;
        Ok((
            Self {
                locker: SurfnetSvmLocker::new(svm),
                geyser_events_rx,
            },
            simnet_events_rx,
        ))
    }

    /// A handle to this instance's state. Handles share the instance; they never own a
    /// lifecycle of their own.
    pub fn locker(&self) -> &SurfnetSvmLocker {
        &self.locker
    }

    /// Runs the instance until `SimnetCommand::Terminate` or a fatal error (the frontend chooses
    /// the thread and runtime).
    pub async fn run(
        self,
        config: SurfpoolConfig,
        simnet_commands_tx: Sender<SimnetCommand>,
        simnet_commands_rx: Receiver<SimnetCommand>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        runloops::start_local_surfnet_runloop(
            self.locker,
            config,
            simnet_commands_tx,
            simnet_commands_rx,
            self.geyser_events_rx,
        )
        .await
    }
}
