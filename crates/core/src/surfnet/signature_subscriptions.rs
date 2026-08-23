//! Signature subscriptions as predicates over a recorded, monotone
//! per-signature lifecycle.
//!
//! A subscription is the predicate "complete me when this signature's
//! stage first satisfies my target level". Registration and stage
//! transitions mutate the same entry, so callers that hold the SVM
//! writer lock get atomicity for free: a subscriber either observes the
//! stage a transition recorded, or its waiter is present when the
//! transition drains. Delivery is a `oneshot` completed at the
//! transition (or a tick), so the notification variant and slot are
//! computed in exactly one place.
//!
//! The registry stores only what the persistent transaction store does
//! not: `Received` and `Failed` stages, and `Executed` stages that
//! still have waiters attached. Entries are removed as soon as they
//! carry neither, so the map is bounded by in-flight work rather than
//! transaction history.

use std::collections::HashMap;

use solana_clock::{MAX_PROCESSING_AGE, Slot};
use solana_commitment_config::CommitmentLevel;
use solana_signature::Signature;
use solana_transaction_error::TransactionError;
use tokio::sync::oneshot;

use super::{FINALIZATION_SLOT_THRESHOLD, SignatureSubscriptionType};

/// The recorded lifecycle stage of a signature.
///
/// Stages are ordered `Received < Failed < Executed` and a recorded
/// stage never regresses. `Failed` marks a transaction rejected before
/// commitment (simulation failure): it satisfies the processed level
/// with its error and never advances with the clock, since the
/// transaction will not confirm. `Executed` covers committed
/// transactions, reverted ones included; its commitment level is
/// derived from clock distance at evaluation time.
#[derive(Debug, Clone, PartialEq)]
pub enum TxStage {
    Received {
        slot: Slot,
    },
    Failed {
        slot: Slot,
        err: TransactionError,
    },
    Executed {
        slot: Slot,
        err: Option<TransactionError>,
    },
}

impl TxStage {
    fn slot(&self) -> Slot {
        match self {
            TxStage::Received { slot }
            | TxStage::Failed { slot, .. }
            | TxStage::Executed { slot, .. } => *slot,
        }
    }

    fn order(&self) -> u8 {
        match self {
            TxStage::Received { .. } => 0,
            TxStage::Failed { .. } => 1,
            TxStage::Executed { .. } => 2,
        }
    }
}

/// The payload a signature subscription resolves to.
#[derive(Debug, Clone, PartialEq)]
pub enum SignatureNotification {
    Received {
        slot: Slot,
    },
    Processed {
        slot: Slot,
        err: Option<TransactionError>,
    },
}

/// The outcome of an atomic subscribe: the notification now, or a
/// receiver completed by a later transition or tick.
pub enum SignatureSubscribeOutcome {
    Now(SignatureNotification),
    /// A waiter was installed. `known_locally` reports whether any
    /// stage for the signature was on record at registration time, so
    /// callers can reserve remote lookups for signatures this surfnet
    /// has never seen.
    Wait {
        rx: oneshot::Receiver<SignatureNotification>,
        known_locally: bool,
    },
}

struct Waiter {
    target: SignatureSubscriptionType,
    tx: oneshot::Sender<SignatureNotification>,
}

#[derive(Default)]
struct SignatureLifecycle {
    stage: Option<TxStage>,
    waiters: Vec<Waiter>,
}

impl SignatureLifecycle {
    /// Whether the entry still carries information: a waiter to
    /// complete, or a stage the persistent store cannot answer for
    /// (`Received` and `Failed` are never stored; `Executed` is).
    fn is_live(&self) -> bool {
        !self.waiters.is_empty()
            || matches!(
                self.stage,
                Some(TxStage::Received { .. } | TxStage::Failed { .. })
            )
    }
}

/// The per-signature subscription registry. One instance lives on
/// `SurfnetSvm`, so every method here runs under the SVM writer lock.
#[derive(Default)]
pub struct SignatureSubscriptions {
    lifecycles: HashMap<Signature, SignatureLifecycle>,
}

/// A cloned registry is empty. SVM clones exist for profiling and
/// bundle sandboxes, which must not complete live subscribers'
/// waiters; one-shot delivery cannot be shared between two SVMs.
impl Clone for SignatureSubscriptions {
    fn clone(&self) -> Self {
        Self::default()
    }
}

fn commitment_rank(level: CommitmentLevel) -> u8 {
    match level {
        CommitmentLevel::Processed => 1,
        CommitmentLevel::Confirmed => 2,
        CommitmentLevel::Finalized => 3,
    }
}

fn executed_rank(executed_slot: Slot, current_slot: Slot) -> u8 {
    if current_slot >= executed_slot.saturating_add(FINALIZATION_SLOT_THRESHOLD) {
        3
    } else if current_slot > executed_slot {
        2
    } else {
        1
    }
}

/// The notification `target` resolves to at `stage`, or `None` while
/// the stage does not yet satisfy the target. The single home of both
/// the satisfaction rule and the payload rule.
pub(crate) fn try_notification(
    target: &SignatureSubscriptionType,
    stage: &TxStage,
    current_slot: Slot,
) -> Option<SignatureNotification> {
    match (target, stage) {
        (SignatureSubscriptionType::Received, stage) => {
            Some(SignatureNotification::Received { slot: stage.slot() })
        }
        (SignatureSubscriptionType::Commitment(level), TxStage::Executed { slot, err }) => {
            (executed_rank(*slot, current_slot) >= commitment_rank(*level)).then(|| {
                SignatureNotification::Processed {
                    slot: *slot,
                    err: err.clone(),
                }
            })
        }
        (SignatureSubscriptionType::Commitment(level), TxStage::Failed { slot, err }) => {
            (*level == CommitmentLevel::Processed).then(|| SignatureNotification::Processed {
                slot: *slot,
                err: Some(err.clone()),
            })
        }
        (SignatureSubscriptionType::Commitment(_), TxStage::Received { .. }) => None,
    }
}

impl SignatureSubscriptions {
    /// Atomically resolves `target` against the signature's stage or
    /// registers a waiter. `known_stage` carries what the caller read
    /// from the persistent transaction store, so a waiter registered
    /// against an already-executed transaction is seeded with the stage
    /// later ticks advance.
    pub fn subscribe(
        &mut self,
        signature: &Signature,
        target: SignatureSubscriptionType,
        known_stage: Option<TxStage>,
        current_slot: Slot,
    ) -> SignatureSubscribeOutcome {
        let lifecycle = self.lifecycles.entry(*signature).or_default();
        if let Some(stage) = known_stage {
            Self::advance(lifecycle, stage);
        }
        let outcome = match &lifecycle.stage {
            Some(stage) => match try_notification(&target, stage, current_slot) {
                Some(notification) => SignatureSubscribeOutcome::Now(notification),
                None => Self::wait(lifecycle, target),
            },
            None => Self::wait(lifecycle, target),
        };
        if !lifecycle.is_live() {
            self.lifecycles.remove(signature);
        }
        outcome
    }

    /// Records a stage transition and completes the waiters it
    /// satisfies. A stage never regresses: a `Received` recorded after
    /// `Executed` is ignored.
    pub fn record(&mut self, signature: &Signature, stage: TxStage, current_slot: Slot) {
        let lifecycle = self.lifecycles.entry(*signature).or_default();
        Self::advance(lifecycle, stage);
        Self::drain(lifecycle, current_slot);
        if !lifecycle.is_live() {
            self.lifecycles.remove(signature);
        }
    }

    /// Re-evaluates every waiter against the clock. Called once per
    /// produced block: this is where `Executed` stages cross the
    /// confirmed and finalized boundaries. Also the sweep: waiters
    /// whose receiver was dropped are removed, and `Received` and
    /// `Failed` stages older than the blockhash validity window are
    /// forgotten.
    pub fn tick(&mut self, current_slot: Slot) {
        self.lifecycles.retain(|_, lifecycle| {
            Self::drain(lifecycle, current_slot);
            // 150 slots, per MAX_PROCESSING_AGE: past that, a Received
            // transaction's blockhash can no longer be current and a
            // Failed one's error no longer describes anything a client
            // could still act on. (A durable-nonce transaction can
            // outlive the window; its late subscriber falls back to the
            // waiter path, an accepted residual.)
            if let Some(TxStage::Received { slot } | TxStage::Failed { slot, .. }) =
                &lifecycle.stage
                && current_slot >= slot.saturating_add(MAX_PROCESSING_AGE as u64)
            {
                lifecycle.stage = None;
            }
            lifecycle.is_live()
        });
    }

    fn advance(lifecycle: &mut SignatureLifecycle, stage: TxStage) {
        let advances = lifecycle
            .stage
            .as_ref()
            .is_none_or(|current| stage.order() > current.order());
        if advances {
            lifecycle.stage = Some(stage);
        }
    }

    fn wait(
        lifecycle: &mut SignatureLifecycle,
        target: SignatureSubscriptionType,
    ) -> SignatureSubscribeOutcome {
        let (tx, rx) = oneshot::channel();
        // Executed only: a Received or Failed stage cannot answer a
        // commitment target locally, and the pre-machine flow consulted
        // the remote for exactly those signatures. Counting them here
        // would suppress the remote lookup for a signature stuck at
        // Received locally that exists remotely.
        let known_locally = matches!(lifecycle.stage, Some(TxStage::Executed { .. }));
        lifecycle.waiters.push(Waiter { target, tx });
        SignatureSubscribeOutcome::Wait { rx, known_locally }
    }

    fn drain(lifecycle: &mut SignatureLifecycle, current_slot: Slot) {
        let stage = lifecycle.stage.clone();
        let waiters = std::mem::take(&mut lifecycle.waiters);
        for waiter in waiters {
            if waiter.tx.is_closed() {
                continue;
            }
            let notification = stage
                .as_ref()
                .and_then(|stage| try_notification(&waiter.target, stage, current_slot));
            match notification {
                // A send failure means the receiver was dropped after
                // the is_closed check: the same outcome as a sweep.
                Some(notification) => {
                    let _ = waiter.tx.send(notification);
                }
                None => lifecycle.waiters.push(waiter),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(n: u8) -> Signature {
        Signature::from([n; 64])
    }

    fn processed() -> SignatureSubscriptionType {
        SignatureSubscriptionType::Commitment(CommitmentLevel::Processed)
    }

    fn confirmed() -> SignatureSubscriptionType {
        SignatureSubscriptionType::Commitment(CommitmentLevel::Confirmed)
    }

    fn finalized() -> SignatureSubscriptionType {
        SignatureSubscriptionType::Commitment(CommitmentLevel::Finalized)
    }

    fn expect_wait(outcome: SignatureSubscribeOutcome) -> oneshot::Receiver<SignatureNotification> {
        match outcome {
            SignatureSubscribeOutcome::Wait { rx, .. } => rx,
            SignatureSubscribeOutcome::Now(n) => panic!("expected Wait, got Now({n:?})"),
        }
    }

    fn expect_now(outcome: SignatureSubscribeOutcome) -> SignatureNotification {
        match outcome {
            SignatureSubscribeOutcome::Now(n) => n,
            SignatureSubscribeOutcome::Wait { .. } => panic!("expected Now, got Wait"),
        }
    }

    #[test]
    fn transition_completes_a_registered_waiter() {
        let mut subs = SignatureSubscriptions::default();
        let mut rx = expect_wait(subs.subscribe(&sig(1), processed(), None, 10));
        assert!(rx.try_recv().is_err());
        subs.record(
            &sig(1),
            TxStage::Executed {
                slot: 10,
                err: None,
            },
            10,
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            SignatureNotification::Processed {
                slot: 10,
                err: None
            }
        );
        assert!(subs.lifecycles.is_empty());
    }

    #[test]
    fn late_subscriber_is_satisfied_from_recorded_received_stage() {
        // The scan-before-store race of the old design: received fires,
        // then the subscriber arrives, then the transaction stores.
        // With the stage recorded, the late subscriber resolves now.
        let mut subs = SignatureSubscriptions::default();
        subs.record(&sig(1), TxStage::Received { slot: 7 }, 7);
        let n = expect_now(subs.subscribe(&sig(1), SignatureSubscriptionType::Received, None, 7));
        assert_eq!(n, SignatureNotification::Received { slot: 7 });
    }

    #[test]
    fn received_stage_does_not_satisfy_commitment_targets() {
        let mut subs = SignatureSubscriptions::default();
        subs.record(&sig(1), TxStage::Received { slot: 7 }, 7);
        let mut rx = expect_wait(subs.subscribe(&sig(1), processed(), None, 7));
        subs.record(&sig(1), TxStage::Executed { slot: 8, err: None }, 8);
        assert_eq!(
            rx.try_recv().unwrap(),
            SignatureNotification::Processed { slot: 8, err: None }
        );
    }

    #[test]
    fn received_target_resolves_from_executed_stage_with_received_variant() {
        let mut subs = SignatureSubscriptions::default();
        let n = expect_now(subs.subscribe(
            &sig(1),
            SignatureSubscriptionType::Received,
            Some(TxStage::Executed { slot: 5, err: None }),
            5,
        ));
        assert_eq!(n, SignatureNotification::Received { slot: 5 });
    }

    #[test]
    fn ticks_advance_executed_stages_across_commitment_boundaries() {
        let mut subs = SignatureSubscriptions::default();
        let mut confirmed_rx = expect_wait(subs.subscribe(&sig(1), confirmed(), None, 10));
        let mut finalized_rx = expect_wait(subs.subscribe(&sig(1), finalized(), None, 10));
        subs.record(
            &sig(1),
            TxStage::Executed {
                slot: 10,
                err: None,
            },
            10,
        );
        assert!(confirmed_rx.try_recv().is_err());

        subs.tick(11);
        assert_eq!(
            confirmed_rx.try_recv().unwrap(),
            SignatureNotification::Processed {
                slot: 10,
                err: None
            }
        );
        assert!(finalized_rx.try_recv().is_err());

        subs.tick(10 + FINALIZATION_SLOT_THRESHOLD);
        assert_eq!(
            finalized_rx.try_recv().unwrap(),
            SignatureNotification::Processed {
                slot: 10,
                err: None
            }
        );
        assert!(subs.lifecycles.is_empty());
    }

    #[test]
    fn known_stage_seeds_the_entry_so_ticks_can_finish_it() {
        // A finalized-target subscriber for a transaction the store
        // already holds: the waiter must carry the executed stage into
        // the registry, or no tick could ever complete it.
        let mut subs = SignatureSubscriptions::default();
        let mut rx = expect_wait(subs.subscribe(
            &sig(1),
            finalized(),
            Some(TxStage::Executed {
                slot: 10,
                err: None,
            }),
            11,
        ));
        subs.tick(10 + FINALIZATION_SLOT_THRESHOLD);
        assert_eq!(
            rx.try_recv().unwrap(),
            SignatureNotification::Processed {
                slot: 10,
                err: None
            }
        );
    }

    #[test]
    fn failed_stage_satisfies_processed_only_and_never_advances() {
        let mut subs = SignatureSubscriptions::default();
        let mut processed_rx = expect_wait(subs.subscribe(&sig(1), processed(), None, 10));
        let mut confirmed_rx = expect_wait(subs.subscribe(&sig(1), confirmed(), None, 10));
        let err = TransactionError::BlockhashNotFound;
        subs.record(
            &sig(1),
            TxStage::Failed {
                slot: 10,
                err: err.clone(),
            },
            10,
        );
        assert_eq!(
            processed_rx.try_recv().unwrap(),
            SignatureNotification::Processed {
                slot: 10,
                err: Some(err)
            }
        );
        assert!(confirmed_rx.try_recv().is_err());
        subs.tick(10 + FINALIZATION_SLOT_THRESHOLD);
        assert!(confirmed_rx.try_recv().is_err());
    }

    #[test]
    fn stages_never_regress() {
        let mut subs = SignatureSubscriptions::default();
        // The confirmed-target waiter keeps the entry live at Executed.
        let mut rx = expect_wait(subs.subscribe(&sig(1), confirmed(), None, 9));
        subs.record(&sig(1), TxStage::Executed { slot: 9, err: None }, 9);
        // A received recorded after execution (a duplicate submission)
        // must not regress the stage: were Received to win, this tick
        // could no longer confirm.
        subs.record(&sig(1), TxStage::Received { slot: 9 }, 9);
        subs.tick(10);
        assert_eq!(
            rx.try_recv().unwrap(),
            SignatureNotification::Processed { slot: 9, err: None }
        );
    }

    #[test]
    fn dropped_receivers_are_swept() {
        let mut subs = SignatureSubscriptions::default();
        let rx = expect_wait(subs.subscribe(&sig(1), confirmed(), None, 10));
        drop(rx);
        subs.tick(11);
        assert!(subs.lifecycles.is_empty());
    }

    #[test]
    fn stale_received_stages_are_forgotten() {
        let mut subs = SignatureSubscriptions::default();
        subs.record(&sig(1), TxStage::Received { slot: 5 }, 5);
        subs.tick(5 + MAX_PROCESSING_AGE as u64 - 1);
        assert_eq!(subs.lifecycles.len(), 1);
        subs.tick(5 + MAX_PROCESSING_AGE as u64);
        assert!(subs.lifecycles.is_empty());
    }

    #[test]
    fn late_subscriber_is_answered_from_a_retained_failed_stage() {
        // The persistent store never sees a simulation failure, so the
        // registry is the only place a late processed-target subscriber
        // can learn the error from.
        let mut subs = SignatureSubscriptions::default();
        let err = TransactionError::BlockhashNotFound;
        subs.record(
            &sig(1),
            TxStage::Failed {
                slot: 5,
                err: err.clone(),
            },
            5,
        );
        let n = expect_now(subs.subscribe(&sig(1), processed(), None, 6));
        assert_eq!(
            n,
            SignatureNotification::Processed {
                slot: 5,
                err: Some(err)
            }
        );
        subs.tick(5 + MAX_PROCESSING_AGE as u64);
        assert!(subs.lifecycles.is_empty());
    }

    #[test]
    fn only_executed_stages_count_as_known_locally() {
        // A Received-stuck signature cannot answer a commitment target
        // locally; reporting it as known would suppress the remote
        // lookup the pre-machine flow performed for it.
        let mut subs = SignatureSubscriptions::default();
        subs.record(&sig(1), TxStage::Received { slot: 5 }, 5);
        match subs.subscribe(&sig(1), processed(), None, 5) {
            SignatureSubscribeOutcome::Wait { known_locally, .. } => {
                assert!(!known_locally)
            }
            SignatureSubscribeOutcome::Now(n) => panic!("expected Wait, got Now({n:?})"),
        }
        match subs.subscribe(
            &sig(2),
            finalized(),
            Some(TxStage::Executed { slot: 5, err: None }),
            5,
        ) {
            SignatureSubscribeOutcome::Wait { known_locally, .. } => {
                assert!(known_locally)
            }
            SignatureSubscribeOutcome::Now(n) => panic!("expected Wait, got Now({n:?})"),
        }
    }

    #[test]
    fn immediate_satisfaction_registers_no_entry() {
        let mut subs = SignatureSubscriptions::default();
        let n = expect_now(subs.subscribe(
            &sig(1),
            processed(),
            Some(TxStage::Executed { slot: 4, err: None }),
            4,
        ));
        assert_eq!(n, SignatureNotification::Processed { slot: 4, err: None });
        assert!(subs.lifecycles.is_empty());
    }
}
