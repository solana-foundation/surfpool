use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, bounded, select, unbounded};
use surfpool_types::{SimnetCommand, SimnetEventsTx};

use crate::surfnet::backend::HistoricalSession;

/// Stop this instance outside the command loop, including while attach or an RPC holds it.
pub(super) struct HistoricalCommands {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
    events: SimnetEventsTx,
}

impl HistoricalCommands {
    pub(super) fn start(
        session: HistoricalSession,
        source: Receiver<SimnetCommand>,
        events: SimnetEventsTx,
    ) -> std::io::Result<(Self, Receiver<SimnetCommand>)> {
        let (stop, stopped) = bounded::<()>(0);
        let (forward, commands) = unbounded();
        let errors = events.clone();
        let thread = std::thread::Builder::new()
            .name("historical-commands".into())
            .spawn(move || {
                loop {
                    select! {
                        recv(stopped) -> _ => break,
                        recv(source) -> command => {
                            let Ok(command) = command else { break };
                            if matches!(command, SimnetCommand::Terminate(_)) {
                                session.terminate();
                            }
                            if let Err(error) = forward.send(command) {
                                errors.error(format!("Historical command loop closed: {error}"));
                                break;
                            }
                        }
                    }
                }
            })?;
        Ok((
            Self {
                stop: Some(stop),
                thread: Some(thread),
                events,
            },
            commands,
        ))
    }
}

impl Drop for HistoricalCommands {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            self.events.error("Historical command forwarder panicked");
        }
    }
}
