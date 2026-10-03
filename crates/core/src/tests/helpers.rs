#![allow(dead_code)]
use std::net::TcpListener;

use crossbeam_channel::{Receiver, Sender};
use solana_clock::Clock;
use solana_epoch_info::EpochInfo;
use solana_transaction::versioned::VersionedTransaction;
use surfpool_types::{CheatcodeConfig, RpcConfig, SimnetCommand, SimnetEvent};

use crate::{
    rpc::RunloopContext,
    surfnet::{PluginCommand, locker::SurfnetSvmLocker, svm::SurfnetSvm},
};

pub fn get_free_port() -> Result<u16, String> {
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| format!("Failed to bind to port 0: {}", e))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("failed to parse address: {}", e))?
        .port();
    drop(listener);
    Ok(port)
}

/// Minimal JSON-RPC stand-in that answers every request with one canned `result` body, so
/// the remote-fetch branches can be exercised without a network.
pub async fn canned_rpc(result_json: impl Into<String>) -> String {
    let result_json = result_json.into();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind canned rpc");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let result_json = result_json.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 16 * 1024];
                let _ = stream.read(&mut buf).await;
                let body = format!(r#"{{"jsonrpc":"2.0","result":{result_json},"id":1}}"#);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });

    format!("http://{addr}")
}

#[derive(Clone)]
pub struct TestSetup<T>
where
    T: Clone,
{
    pub context: RunloopContext,
    pub rpc: T,
}

impl<T> TestSetup<T>
where
    T: Clone,
{
    pub fn new(rpc: T) -> Self {
        Self::new_with_events(rpc).0
    }

    /// The same setup, keeping the event receiver, for tests that read what
    /// the surfnet emitted rather than only what it returned.
    pub fn new_with_events(rpc: T) -> (Self, Receiver<SimnetEvent>) {
        let (simnet_commands_tx, _rx) = crossbeam_channel::unbounded();

        let (mut surfnet_svm, simnet_events_rx, _) = SurfnetSvm::default();
        let clock = Clock {
            slot: 123,
            epoch_start_timestamp: 123,
            epoch: 1,
            leader_schedule_epoch: 1,
            unix_timestamp: 123,
        };
        surfnet_svm.inner.set_sysvar::<Clock>(&clock);
        surfnet_svm.latest_epoch_info = EpochInfo {
            epoch: clock.epoch,
            slot_index: clock.slot,
            slots_in_epoch: 100,
            absolute_slot: clock.slot,
            block_height: 42,
            transaction_count: Some(2),
        };
        surfnet_svm.transactions_processed = 69;

        let (plugin_commands_tx, _plugin_commands_rx) =
            crossbeam_channel::unbounded::<PluginCommand>();
        let setup = TestSetup {
            context: RunloopContext {
                simnet_commands_tx: simnet_commands_tx.clone(),
                id: None,
                svm_locker: SurfnetSvmLocker::new(surfnet_svm),
                remote_rpc_client: None,
                rpc_config: RpcConfig::default(),
                cheatcode_config: CheatcodeConfig::new(),
                plugin_commands_tx,
            },
            rpc,
        };
        (setup, simnet_events_rx)
    }

    pub fn new_with_epoch_info(rpc: T, epoch_info: EpochInfo) -> Self {
        let setup = TestSetup::new(rpc);
        setup
            .context
            .svm_locker
            .0
            .blocking_write()
            .latest_epoch_info = epoch_info;
        setup
    }

    pub fn new_with_mempool(rpc: T, simnet_commands_tx: Sender<SimnetCommand>) -> Self {
        let mut setup = TestSetup::new(rpc);
        setup.context.simnet_commands_tx = simnet_commands_tx;
        setup
    }

    /// Runs serialized VM mutations as the production runloop would and
    /// deliberately discards every other command.
    pub fn new_with_serial_vm_executor(rpc: T) -> Self {
        Self::new_with_serial_vm_executor_and_handler(rpc, |_| true)
    }

    pub fn new_with_serial_vm_executor_and_mempool(rpc: T) -> (Self, Receiver<SimnetCommand>) {
        let (mempool_tx, mempool_rx) = crossbeam_channel::unbounded();
        let setup = Self::new_with_serial_vm_executor_and_handler(rpc, move |command| {
            mempool_tx.send(command).is_ok()
        });

        (setup, mempool_rx)
    }

    fn new_with_serial_vm_executor_and_handler(
        rpc: T,
        mut handle_non_mutation: impl FnMut(SimnetCommand) -> bool + Send + 'static,
    ) -> Self {
        let (simnet_commands_tx, simnet_commands_rx) = crossbeam_channel::unbounded();
        let setup = Self::new_with_mempool(rpc, simnet_commands_tx);

        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("serial VM mutation test runtime should start");

            while let Ok(command) = simnet_commands_rx.recv() {
                match command {
                    SimnetCommand::ProcessSerialVmMutation(task) => {
                        runtime.block_on(task.run());
                    }
                    command => {
                        if !handle_non_mutation(command) {
                            break;
                        }
                    }
                }
            }
        });

        setup
    }

    pub async fn without_blockhash(self) -> Self {
        let mut state_writer = self.context.svm_locker.0.write().await;
        state_writer.skip_blockhash_check = true;
        let svm = state_writer.inner.clone();
        let svm = svm.with_blockhash_check(false);
        state_writer.inner = svm;
        drop(state_writer);
        self
    }

    pub async fn process_txs(&mut self, txs: Vec<VersionedTransaction>) {
        let (status_tx, _rx) = crossbeam_channel::unbounded();
        for tx in txs {
            let _ = self
                .context
                .svm_locker
                .process_transaction(&None, tx.clone(), status_tx.clone(), true, false)
                .await
                .unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use surfpool_types::{BlockProductionMode, SerialVmMutationResult, SerialVmMutationTask};

    use super::*;

    #[test]
    fn serial_vm_executor_discards_non_mutation_commands_without_stopping() {
        let setup = TestSetup::new_with_serial_vm_executor(());
        setup
            .context
            .simnet_commands_tx
            .send(SimnetCommand::UpdateBlockProductionMode(
                BlockProductionMode::Manual,
            ))
            .expect("discarded command should be accepted");

        let (completed_tx, completed_rx) = crossbeam_channel::bounded(1);
        setup
            .context
            .simnet_commands_tx
            .send(SimnetCommand::ProcessSerialVmMutation(
                SerialVmMutationTask::new(move || {
                    Box::pin(async move {
                        completed_tx
                            .send(())
                            .expect("test mutation completion receiver should remain available");
                        SerialVmMutationResult::NoBlock
                    })
                }),
            ))
            .expect("serial mutation should be accepted after a discarded command");

        completed_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("serial mutation executor should remain alive");
    }
}
