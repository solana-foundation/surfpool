//! Pins the delivery contract of the simnet events channel.
//!
//! The channel is bounded(1024) and the sender is a [`SimnetEventsTx`],
//! which owns the two delivery policies: `log` is lossy by contract
//! (telemetry may drop under backpressure), `emit` is lossless (lifecycle
//! events block until the reader drains).

use surfpool_types::SimnetEvent;

use crate::surfnet::svm::{SurfnetSvm, SurfnetSvmConfig};

/// Build a surfnet and drain whatever construction queued, so each test
/// starts from an empty buffer with the real channel.
fn empty_events_channel() -> (SurfnetSvm, crossbeam_channel::Receiver<SimnetEvent>) {
    let (svm, simnet_events_rx, _geyser_rx) =
        SurfnetSvm::new(SurfnetSvmConfig::default()).expect("surfnet should build");
    while simnet_events_rx.try_recv().is_ok() {}
    (svm, simnet_events_rx)
}

#[test]
fn log_keeps_at_most_the_buffer_capacity() {
    let (svm, simnet_events_rx) = empty_events_channel();

    for i in 0..1025 {
        svm.simnet_events_tx.info(format!("filler {i}"));
    }

    let received: Vec<SimnetEvent> = simnet_events_rx.try_iter().collect();
    assert_eq!(
        received.len(),
        1024,
        "the 1,025th log line is dropped, and dropping it returns immediately"
    );
}

#[test]
fn emit_delivers_the_abort_through_a_full_buffer() {
    let (svm, simnet_events_rx) = empty_events_channel();

    for i in 0..1024 {
        svm.simnet_events_tx.info(format!("noise {i}"));
    }

    // Before the sender newtype, this event went through the same silent
    // try_send as the noise and vanished. emit blocks until the reader
    // makes room, so it arrives.
    let tx = svm.simnet_events_tx.clone();
    let emitter = std::thread::spawn(move || {
        tx.emit(SimnetEvent::Aborted("out of lamports".to_string()));
    });

    let mut received = Vec::new();
    // 1024 noise lines plus the abort.
    for _ in 0..1025 {
        received.push(
            simnet_events_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("every event arrives once the reader drains"),
        );
    }
    emitter.join().unwrap();

    assert!(
        matches!(&received[1024], SimnetEvent::Aborted(msg) if msg == "out of lamports"),
        "the abort queued behind the noise instead of dropping"
    );
}

#[test]
fn a_log_burst_beyond_capacity_sheds_the_newest_lines() {
    let (svm, simnet_events_rx) = empty_events_channel();

    const BURST: usize = 4096;
    for i in 0..BURST {
        svm.simnet_events_tx.info(format!("{i}"));
    }

    let received: Vec<usize> = simnet_events_rx
        .try_iter()
        .filter_map(|e| match e {
            SimnetEvent::InfoLog(_, msg) => msg.parse().ok(),
            _ => None,
        })
        .collect();

    assert_eq!(
        received.len(),
        1024,
        "3072 of 4096 log lines were shed, the lossy contract log declares"
    );
    assert_eq!(
        received,
        (0..1024).collect::<Vec<_>>(),
        "the survivors are the oldest lines; a reader that falls behind \
         loses recent telemetry, never lifecycle events, which go through \
         emit"
    );
}
