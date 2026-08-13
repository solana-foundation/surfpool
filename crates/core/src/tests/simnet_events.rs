//! Pins the delivery behavior of the simnet events channel.
//!
//! The channel is `crossbeam_channel::bounded(1024)` (`surfnet/svm.rs`), and
//! core sends on it with `let _ = tx.try_send(...)` at dozens of sites. On a
//! bounded channel `try_send` fails when the buffer is full, so under
//! backpressure those sites drop events silently, with no distinction
//! between a log line and an `Aborted`. These tests demonstrate the drop and
//! measure it, so a later change to the sending policy has a pinned before.

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
fn the_events_channel_refuses_the_1025th_event() {
    let (svm, _rx) = empty_events_channel();

    let mut accepted = 0u32;
    while svm
        .simnet_events_tx
        .try_send(SimnetEvent::info(format!("filler {accepted}")))
        .is_ok()
    {
        accepted += 1;
    }

    assert_eq!(accepted, 1024, "the buffer holds exactly its bound");
}

#[test]
fn a_full_buffer_drops_an_abort_silently() {
    let (svm, simnet_events_rx) = empty_events_channel();

    for i in 0..1024 {
        svm.simnet_events_tx
            .try_send(SimnetEvent::info(format!("noise {i}")))
            .expect("the first 1024 sends are accepted");
    }

    // The idiom used at the core call sites, verbatim: the Result vanishes,
    // and with it the abort.
    let _ = svm
        .simnet_events_tx
        .try_send(SimnetEvent::Aborted("out of lamports".to_string()));

    let received: Vec<SimnetEvent> = simnet_events_rx.try_iter().collect();
    assert_eq!(received.len(), 1024);
    assert!(
        !received
            .iter()
            .any(|e| matches!(e, SimnetEvent::Aborted(_))),
        "the abort was dropped; 1024 log lines were kept instead"
    );
}

#[test]
fn a_burst_beyond_capacity_loses_the_newest_events() {
    let (svm, simnet_events_rx) = empty_events_channel();

    const BURST: usize = 4096;
    for i in 0..BURST {
        let _ = svm
            .simnet_events_tx
            .try_send(SimnetEvent::info(format!("{i}")));
    }

    let received: Vec<usize> = simnet_events_rx
        .try_iter()
        .filter_map(|e| match e {
            SimnetEvent::InfoLog(_, msg) => msg.parse().ok(),
            _ => None,
        })
        .collect();

    assert_eq!(received.len(), 1024, "3072 of 4096 events were dropped");
    assert_eq!(
        received,
        (0..1024).collect::<Vec<_>>(),
        "the survivors are the oldest events; everything after the buffer \
         filled was lost, so under backpressure the freshest information is \
         what disappears"
    );
}
