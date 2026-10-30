use std::sync::{Arc, Barrier};

use surfpool_types::{HistoricalSessionOptions, HistoricalSimnetConfig};

use crate::{
    storage::StorageBackend,
    surfnet::{
        backend::historical::{HistoricalLaunch, launch},
        locker::SurfnetSvmLocker,
        svm::{SurfnetChannels, SurfnetSvm, SurfnetSvmConfig},
    },
};

#[test]
#[ignore = "needs a staged SVM Replay backend and cached history (M59_* variables)"]
fn historical_ticks_are_atomic_with_each_other_and_time_travel() {
    let var = |name| std::env::var(name).unwrap();
    let config = HistoricalSimnetConfig::session(HistoricalSessionOptions {
        after: var("M59_AFTER"),
        program: Some(var("M59_SVM_REPLAY_BIN")),
        data_dir: Some(var("M59_HISTORICAL_DATA_DIR").into()),
        config: Some(var("M59_HISTORICAL_CONFIG").into()),
        ..Default::default()
    })
    .unwrap();
    let session = launch(
        &HistoricalLaunch {
            program: config.program.into(),
            args: config.args.into_iter().map(Into::into).collect(),
            envs: config
                .envs
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
        },
        Arc::new(|line| eprintln!("{line}")),
        || false,
    )
    .unwrap();
    let (channels, _events, _geyser) = SurfnetChannels::new();
    let svm = SurfnetSvm::build_with(
        StorageBackend::open(&None, "clock-test").unwrap(),
        None,
        channels,
        SurfnetSvmConfig {
            slot_time: 1_000,
            ..Default::default()
        },
        Some(session.attach().unwrap()),
    )
    .unwrap();
    let locker = SurfnetSvmLocker::new(svm);
    let before = locker.with_svm_reader(|svm| (svm.get_latest_absolute_slot(), svm.updated_at));
    let barrier = Barrier::new(8);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    locker.advance_historical_tick().unwrap();
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    });
    assert_eq!(
        locker.with_svm_reader(|svm| (svm.get_latest_absolute_slot(), svm.updated_at)),
        (before.0 + 8, before.1 + 8_000)
    );
    let target = before.1 + 60_000;
    let barrier = Barrier::new(8);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|index| {
                scope.spawn({
                    let barrier = &barrier;
                    let locker = &locker;
                    move || {
                        barrier.wait();
                        if index == 0 {
                            locker.advance_historical(1, Some(target), false).unwrap();
                        } else {
                            locker.advance_historical_tick().unwrap();
                        }
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    });
    assert!(locker.with_svm_reader(|svm| svm.updated_at) >= target);
    session.terminate();
}
