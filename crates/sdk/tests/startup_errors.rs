//! A surfnet that fails to start is reported by `Surfnet::start`, and nothing of it keeps
//! running: neither a runloop that failed before the surfnet was ready (whose failure must end
//! the startup wait), nor a ready surfnet whose startup funding then failed (which no caller
//! holds a handle to stop).
#![cfg(target_os = "linux")]

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use surfpool_sdk::{Surfnet, SurfnetBuilder};

/// This process's threads that run a surfnet's runloop (the SDK names them `surfnet-sdk`).
fn runloop_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
        .filter(|name| name.trim() == "surfnet-sdk")
        .count()
}

fn start_within(builder: SurfnetBuilder, limit: Duration) -> Result<Surfnet, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let started = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(builder.start())
            .map_err(|e| e.to_string());
        let _ = tx.send(started);
    });
    rx.recv_timeout(limit)
        .expect("Surfnet::start did not return: the failed startup was waited on without end")
}

fn assert_nothing_left_running(case: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while runloop_threads() > 0 {
        assert!(
            Instant::now() < deadline,
            "{case}: a surfnet that failed to start is still running"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_failed_startup_is_returned_and_leaves_nothing_running() {
    assert_eq!(runloop_threads(), 0);

    // The runloop fails before the surfnet is ready: its datasource refuses connections.
    let unreachable = Surfnet::builder()
        .offline(false)
        .remote_rpc_url("http://127.0.0.1:1");
    let failed = start_within(unreachable, Duration::from_secs(120));
    assert!(failed.is_err(), "the startup should fail");
    assert_nothing_left_running("runloop failure");

    // The surfnet becomes ready, then its startup funding fails (an airdrop below the
    // rent-exempt minimum is skipped, so the payer is never funded).
    let unfunded = Surfnet::builder().airdrop_sol(1);
    let failed = start_within(unfunded, Duration::from_secs(120));
    assert!(failed.is_err(), "the startup should fail");
    assert_nothing_left_running("funding failure");
}
