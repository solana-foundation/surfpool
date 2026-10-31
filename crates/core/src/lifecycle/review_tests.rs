use std::{
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender};
use surfpool_types::{HistoricalSimnetConfig, SimnetCommand, SurfpoolConfig};

use super::SurfnetInstance;
use crate::surfnet::svm::SurfnetSvmConfig;

struct StalledInstance {
    commands: Sender<SimnetCommand>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
    pid: u32,
    _directory: tempfile::TempDir,
}

impl StalledInstance {
    fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let waiting = directory.path().join("request");
        let pins = serde_json::json!({"schema":"svm-historical-backend/v1", "kind":"ready", "pins": {
            "genesisHash":"5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d", "slot":123,
            "parentSlot":122,"blockHeight":42,"transactionIndex":0,
            "executionBlockhash":"CxvhuTN6ym1wmcchbrqySxr7Qpfs5Zj9qLi7zB18cyEz"}});
        let script = format!(
            "echo $$ > {}; printf '%s\\n' '{}'; read request; touch {}; while [ ! -e {} ]; do sleep 0.02; done",
            pid_file.display(),
            pins,
            waiting.display(),
            directory.path().join("release").display()
        );
        let (instance, _events) = SurfnetInstance::open(
            None,
            SurfnetSvmConfig {
                historical: Some(HistoricalSimnetConfig {
                    program: "/bin/sh".into(),
                    args: vec!["-c".into(), script, "sh".into()],
                    envs: vec![],
                }),
                ..SurfnetSvmConfig::default()
            },
        )
        .unwrap();
        let (commands, rx) = crossbeam_channel::unbounded();
        let tx = commands.clone();
        let (finished, done) = crossbeam_channel::bounded(1);
        let thread = std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(instance.run(SurfpoolConfig::default(), tx, rx));
            if let Err(error) = result {
                eprintln!("stopped attach: {error}");
            }
            finished.send(()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !waiting.exists() {
            assert!(
                Instant::now() < deadline,
                "backend did not receive its first request"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        Self {
            commands,
            done,
            thread: Some(thread),
            pid,
            _directory: directory,
        }
    }
}

impl Drop for StalledInstance {
    fn drop(&mut self) {
        if let Err(error) = self.commands.send(SimnetCommand::Terminate(None)) {
            eprintln!("instance already stopped during cleanup: {error}");
        }
        // Release the deterministic IPC peer even when testing the unfixed implementation.
        std::fs::write(self._directory.path().join("release"), b"").unwrap();
        self.thread.take().unwrap().join().unwrap();
        assert_eq!(unsafe { libc::kill(self.pid as libc::pid_t, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}

#[test]
fn review_terminate_interrupts_attach_without_stopping_another_instance() {
    let first = StalledInstance::start();
    let second = StalledInstance::start();
    first.commands.send(SimnetCommand::Terminate(None)).unwrap();
    let ended = first.done.recv_timeout(Duration::from_secs(5)).is_ok();
    let independent = unsafe { libc::kill(second.pid as libc::pid_t, 0) } == 0
        && !second.thread.as_ref().unwrap().is_finished();
    drop((first, second));
    assert!(
        ended,
        "Terminate was blocked behind an in-flight historical request"
    );
    assert!(
        independent,
        "stopping one instance must not stop another backend"
    );
}
