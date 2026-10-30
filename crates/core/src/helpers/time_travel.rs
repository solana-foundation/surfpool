use solana_clock::{Clock, Slot};
use solana_epoch_info::EpochInfo;
use solana_epoch_schedule::EpochSchedule;

use crate::types::{TimeTravelConfig, TimeTravelError};

/// The Clock sysvar at `slot` and `unix_timestamp`, filled in the way Agave's `Bank::update_clock`
/// does: the epoch's start time carries over from `previous` while the epoch stays the same, and
/// resets to `unix_timestamp` otherwise (a new epoch, or a warp with no previous Clock).
pub fn clock_for_slot(
    slot: Slot,
    unix_timestamp: i64,
    previous: Option<&Clock>,
    epoch_schedule: &EpochSchedule,
) -> Clock {
    let epoch = epoch_schedule.get_epoch(slot);
    let epoch_start_timestamp = match previous {
        Some(previous) if previous.epoch == epoch => previous.epoch_start_timestamp,
        _ => unix_timestamp,
    };
    Clock {
        slot,
        epoch_start_timestamp,
        epoch,
        leader_schedule_epoch: epoch_schedule.get_leader_schedule_epoch(slot),
        unix_timestamp,
    }
}

/// Time travel math functions for calculating clock updates based on different time travel configurations.
///
/// This module contains pure functions that calculate the new clock state when time traveling
/// in a Solana network. These functions are extracted from the RPC implementation to make
/// them testable and reusable.
///
/// Calculates the new clock state when traveling to an absolute timestamp.
///
/// # Arguments
/// * `timestamp_target` - The target UNIX timestamp in milliseconds
/// * `current_updated_at` - The current timestamp in milliseconds
/// * `slot_time` - The slot time in milliseconds
/// * `epoch_info` - The current epoch information
/// * `epoch_schedule` - The installed epoch schedule
///
/// # Returns
/// A `Result` containing either a new `Clock` object representing the target time state,
/// or a `TimeTravelError` if the operation is invalid
pub fn calculate_absolute_timestamp_clock(
    timestamp_target: u64,
    current_updated_at: u64,
    slot_time: u64,
    epoch_info: &EpochInfo,
    epoch_schedule: &EpochSchedule,
) -> Result<Clock, TimeTravelError> {
    // Ensure the timestamp is in the future
    if timestamp_target < current_updated_at {
        return Err(TimeTravelError::PastTimestamp {
            target: timestamp_target,
            current: current_updated_at,
        });
    }

    // Prevent division by zero
    if slot_time == 0 {
        return Err(TimeTravelError::ZeroSlotTime);
    }
    if epoch_info.slots_in_epoch == 0 {
        return Err(TimeTravelError::ZeroSlotsInEpoch);
    }

    let slot = epoch_info.absolute_slot + (timestamp_target - current_updated_at) / slot_time;

    // timestamp_target is in milliseconds, we need to convert it to seconds
    let timestamp_target_seconds = timestamp_target / 1000;

    Ok(clock_for_slot(
        slot,
        timestamp_target_seconds as i64,
        None,
        epoch_schedule,
    ))
}

/// Calculates the new clock state when traveling to an absolute slot.
///
/// # Arguments
/// * `new_absolute_slot` - The target absolute slot number
/// * `current_absolute_slot` - The current absolute slot number
/// * `current_updated_at` - The current timestamp in milliseconds
/// * `slot_time` - The slot time in milliseconds
/// * `epoch_schedule` - The installed epoch schedule
///
/// # Returns
/// A `Result` containing either a new `Clock` object representing the target slot state,
/// or a `TimeTravelError` if the operation is invalid
pub fn calculate_absolute_slot_clock(
    new_absolute_slot: u64,
    current_absolute_slot: u64,
    current_updated_at: u64,
    slot_time: u64,
    epoch_schedule: &EpochSchedule,
) -> Result<Clock, TimeTravelError> {
    // Ensure the slot is in the future
    if new_absolute_slot < current_absolute_slot {
        return Err(TimeTravelError::PastSlot {
            target: new_absolute_slot,
            current: current_absolute_slot,
        });
    }

    let time_jump_in_absolute_slots = new_absolute_slot - current_absolute_slot;
    let time_jump_in_ms = time_jump_in_absolute_slots.saturating_mul(slot_time);
    let timestamp_target = current_updated_at.saturating_add(time_jump_in_ms);

    // timestamp_target is in milliseconds, we need to convert it to seconds
    let timestamp_target_seconds = timestamp_target / 1000;

    Ok(clock_for_slot(
        new_absolute_slot,
        timestamp_target_seconds as i64,
        None,
        epoch_schedule,
    ))
}

/// Calculates the new clock state when traveling to an absolute epoch.
///
/// # Arguments
/// * `new_epoch` - The target epoch number
/// * `current_epoch` - The current epoch number
/// * `current_absolute_slot` - The current absolute slot number
/// * `current_updated_at` - The current timestamp in milliseconds
/// * `slot_time` - The slot time in milliseconds
/// * `epoch_schedule` - The installed epoch schedule
///
/// # Returns
/// A `Result` containing either a new `Clock` object representing the target epoch state,
/// or a `TimeTravelError` if the operation is invalid
pub fn calculate_absolute_epoch_clock(
    new_epoch: u64,
    current_epoch: u64,
    current_absolute_slot: u64,
    current_updated_at: u64,
    slot_time: u64,
    epoch_schedule: &EpochSchedule,
) -> Result<Clock, TimeTravelError> {
    // Ensure the epoch is in the future
    if new_epoch < current_epoch {
        return Err(TimeTravelError::PastEpoch {
            target: new_epoch,
            current: current_epoch,
        });
    }

    // The first slot of the epoch, unless the chain is already past it: time travel never rewinds.
    let new_absolute_slot = epoch_schedule
        .get_first_slot_in_epoch(new_epoch)
        .max(current_absolute_slot);
    let time_jump_in_absolute_slots = new_absolute_slot.saturating_sub(current_absolute_slot);
    let time_jump_in_ms = time_jump_in_absolute_slots * slot_time;
    let timestamp_target = current_updated_at + time_jump_in_ms;

    // timestamp_target is in milliseconds, we need to convert it to seconds
    let timestamp_target_seconds = timestamp_target / 1000;

    Ok(clock_for_slot(
        new_absolute_slot,
        timestamp_target_seconds as i64,
        None,
        epoch_schedule,
    ))
}

/// Calculates the new clock state based on a time travel configuration.
///
/// # Arguments
/// * `config` - The time travel configuration
/// * `current_updated_at` - The current timestamp in milliseconds
/// * `slot_time` - The slot time in milliseconds
/// * `epoch_info` - The current epoch information
/// * `epoch_schedule` - The installed epoch schedule
///
/// # Returns
/// A `Result` containing either a new `Clock` object representing the target state,
/// or a `TimeTravelError` if the operation is invalid
pub fn calculate_time_travel_clock(
    config: &TimeTravelConfig,
    current_updated_at: u64,
    slot_time: u64,
    epoch_info: &EpochInfo,
    epoch_schedule: &EpochSchedule,
) -> Result<Clock, TimeTravelError> {
    match config {
        TimeTravelConfig::AbsoluteTimestamp(timestamp_target) => {
            calculate_absolute_timestamp_clock(
                *timestamp_target,
                current_updated_at,
                slot_time,
                epoch_info,
                epoch_schedule,
            )
        }
        TimeTravelConfig::AbsoluteSlot(new_absolute_slot) => calculate_absolute_slot_clock(
            *new_absolute_slot,
            epoch_info.absolute_slot,
            current_updated_at,
            slot_time,
            epoch_schedule,
        ),
        TimeTravelConfig::AbsoluteSlotAt { slot, timestamp } => calculate_absolute_slot_clock(
            *slot,
            epoch_info.absolute_slot,
            current_updated_at,
            slot_time,
            epoch_schedule,
        )
        .map(|clock| {
            let timestamp_seconds = (*timestamp / 1000) as i64;
            Clock {
                epoch_start_timestamp: timestamp_seconds,
                unix_timestamp: timestamp_seconds,
                ..clock
            }
        }),
        TimeTravelConfig::AbsoluteEpoch(new_epoch) => calculate_absolute_epoch_clock(
            *new_epoch,
            epoch_info.epoch,
            epoch_info.absolute_slot,
            current_updated_at,
            slot_time,
            epoch_schedule,
        ),
    }
}

#[cfg(test)]
mod tests {
    use solana_epoch_info::EpochInfo;

    use super::*;

    fn schedule() -> EpochSchedule {
        EpochSchedule::without_warmup()
    }

    fn create_test_epoch_info(epoch: u64, slot_index: u64, absolute_slot: u64) -> EpochInfo {
        EpochInfo {
            epoch,
            slot_index,
            slots_in_epoch: 432_000,
            absolute_slot,
            block_height: 0,
            transaction_count: Some(0),
        }
    }

    #[test]
    fn test_calculate_absolute_timestamp_clock_basic() {
        let epoch_info = create_test_epoch_info(1, 1000, 433_000);
        let current_time = 1_000_000_000; // 1 billion ms
        let slot_time = 400; // 400ms per slot
        let target_time = current_time + 1_000_000; // 1 second later

        let clock = calculate_absolute_timestamp_clock(
            target_time,
            current_time,
            slot_time,
            &epoch_info,
            &schedule(),
        )
        .unwrap();

        assert_eq!(clock.unix_timestamp, target_time as i64 / 1_000);
        assert_eq!(clock.epoch_start_timestamp, target_time as i64 / 1_000);
        assert_eq!(clock.epoch, 1); // Should stay in same epoch
        assert_eq!(clock.slot, 433_000 + (1_000_000 / 400)); // Should advance by time difference
    }

    #[test]
    fn test_calculate_absolute_timestamp_clock_epoch_transition() {
        let epoch_info = create_test_epoch_info(1, 431_000, 863_000); // Near end of epoch 1
        let current_time = 1_000_000_000;
        let slot_time = 400;
        let target_time = current_time + 10_000_000; // 10 seconds later

        let clock = calculate_absolute_timestamp_clock(
            target_time,
            current_time,
            slot_time,
            &epoch_info,
            &schedule(),
        )
        .unwrap();

        assert_eq!(clock.unix_timestamp, target_time as i64 / 1000);
        assert_eq!(clock.epoch_start_timestamp, target_time as i64 / 1000);
        // With 10 seconds and 400ms slot time, we advance 25,000 slots, from slot 863,000 to
        // 888,000, past the first slot of epoch 2 (864,000)
        assert_eq!(clock.slot, 888_000);
        assert_eq!(clock.epoch, 2);
    }

    #[test]
    fn test_calculate_absolute_timestamp_clock_past() {
        let epoch_info = create_test_epoch_info(1, 1000, 433_000);
        let current_time = 1_000_000_000;
        let slot_time = 400;
        let target_time = current_time - 1_000_000; // 1 second earlier

        let result = calculate_absolute_timestamp_clock(
            target_time,
            current_time,
            slot_time,
            &epoch_info,
            &schedule(),
        );
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), TimeTravelError::PastTimestamp { target, current } if target == target_time && current == current_time)
        );
    }

    #[test]
    fn test_calculate_absolute_slot_clock_basic() {
        let epoch_info = create_test_epoch_info(1, 1000, 433_000);
        let current_time = 1_000_000_000;
        let slot_time = 400;
        let target_slot = 500_000;

        let clock = calculate_absolute_slot_clock(
            target_slot,
            epoch_info.absolute_slot,
            current_time,
            slot_time,
            &schedule(),
        )
        .unwrap();

        assert_eq!(clock.slot, target_slot);
        assert_eq!(clock.epoch, target_slot / epoch_info.slots_in_epoch);
        assert_eq!(
            clock.unix_timestamp,
            (current_time + (target_slot - epoch_info.absolute_slot) * slot_time) as i64 / 1_000
        );
    }

    #[test]
    fn test_calculate_absolute_slot_clock_epoch_boundary() {
        let epoch_info = create_test_epoch_info(1, 431_999, 863_999); // Last slot of epoch 1
        let current_time = 1_000_000_000;
        let slot_time = 400;
        let target_slot = 864_000; // First slot of epoch 2

        let clock = calculate_absolute_slot_clock(
            target_slot,
            epoch_info.absolute_slot,
            current_time,
            slot_time,
            &schedule(),
        )
        .unwrap();

        assert_eq!(clock.slot, 864_000); // First slot of new epoch
        assert_eq!(clock.epoch, 2); // New epoch
        assert_eq!(
            clock.unix_timestamp,
            (current_time + slot_time) as i64 / 1_000
        );
    }

    #[test]
    fn test_calculate_absolute_slot_clock_past() {
        let epoch_info = create_test_epoch_info(1, 1000, 433_000);
        let current_time = 1_000_000_000;
        let slot_time = 400;
        let target_slot = 432_000; // Earlier slot

        let result = calculate_absolute_slot_clock(
            target_slot,
            epoch_info.absolute_slot,
            current_time,
            slot_time,
            &schedule(),
        );
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), TimeTravelError::PastSlot { target, current } if target == target_slot && current == epoch_info.absolute_slot)
        );
    }

    #[test]
    fn test_calculate_absolute_epoch_clock_basic() {
        let epoch_info = create_test_epoch_info(1, 1000, 433_000);
        let current_time = 1_000_000_000;
        let slot_time = 400;
        let target_epoch = 5;

        let clock = calculate_absolute_epoch_clock(
            target_epoch,
            epoch_info.epoch,
            epoch_info.absolute_slot,
            current_time,
            slot_time,
            &schedule(),
        )
        .unwrap();

        assert_eq!(clock.slot, target_epoch * epoch_info.slots_in_epoch); // First slot of the epoch
        assert_eq!(clock.epoch, target_epoch);
        assert_eq!(
            clock.unix_timestamp,
            (current_time
                + (target_epoch * epoch_info.slots_in_epoch - epoch_info.absolute_slot) * slot_time)
                as i64
                / 1_000
        );
    }

    #[test]
    fn test_calculate_absolute_epoch_clock_same_epoch() {
        let epoch_info = create_test_epoch_info(1, 1000, 433_000);
        let current_time = 1_000_000_000;
        let slot_time = 400;
        let target_epoch = 1; // Same epoch

        let clock = calculate_absolute_epoch_clock(
            target_epoch,
            epoch_info.epoch,
            epoch_info.absolute_slot,
            current_time,
            slot_time,
            &schedule(),
        )
        .unwrap();

        assert_eq!(clock.slot, 433_000); // Already past the epoch's first slot: stay, don't rewind
        assert_eq!(clock.epoch, 1);
        // When staying in the same epoch, no time should advance
        assert_eq!(clock.unix_timestamp, current_time as i64 / 1_000);
    }

    #[test]
    fn test_calculate_absolute_epoch_clock_past() {
        let epoch_info = create_test_epoch_info(5, 1000, 2_161_000);
        let current_time = 1_000_000_000;
        let slot_time = 400;
        let target_epoch = 1; // Earlier epoch

        let result = calculate_absolute_epoch_clock(
            target_epoch,
            epoch_info.epoch,
            epoch_info.absolute_slot,
            current_time,
            slot_time,
            &schedule(),
        );
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            TimeTravelError::PastEpoch {
                target: 1,
                current: 5
            }
        ));
    }

    #[test]
    fn test_calculate_time_travel_clock_absolute_timestamp() {
        let epoch_info = create_test_epoch_info(1, 1000, 433_000);
        let current_time = 1_000_000_000;
        let slot_time = 400;
        let config = TimeTravelConfig::AbsoluteTimestamp(current_time + 1_000_000);

        let clock =
            calculate_time_travel_clock(&config, current_time, slot_time, &epoch_info, &schedule())
                .unwrap();

        assert_eq!(
            clock.unix_timestamp,
            (current_time + 1_000_000) as i64 / 1_000
        );
        assert_eq!(clock.epoch, 1);
    }

    #[test]
    fn test_calculate_time_travel_clock_absolute_slot() {
        let epoch_info = create_test_epoch_info(1, 1000, 433_000);
        let current_time = 1_000_000_000;
        let slot_time = 400;
        let config = TimeTravelConfig::AbsoluteSlot(500_000);

        let clock =
            calculate_time_travel_clock(&config, current_time, slot_time, &epoch_info, &schedule())
                .unwrap();

        assert_eq!(clock.slot, 500_000);
        assert_eq!(clock.epoch, 500_000 / epoch_info.slots_in_epoch);
    }

    #[test]
    fn test_calculate_time_travel_clock_absolute_epoch() {
        let epoch_info = create_test_epoch_info(1, 1000, 433_000);
        let current_time = 1_000_000_000;
        let slot_time = 400;
        let config = TimeTravelConfig::AbsoluteEpoch(5);

        let clock =
            calculate_time_travel_clock(&config, current_time, slot_time, &epoch_info, &schedule())
                .unwrap();

        assert_eq!(clock.slot, 5 * 432_000); // First slot of epoch 5
        assert_eq!(clock.epoch, 5);
    }

    #[test]
    fn test_edge_case_zero_slot_time() {
        let epoch_info = create_test_epoch_info(1, 1000, 433_000);
        let current_time = 1_000_000_000;
        let slot_time = 0; // Edge case: zero slot time
        let target_time = current_time + 1_000_000;

        // This should return an error due to division by zero
        let result = calculate_absolute_timestamp_clock(
            target_time,
            current_time,
            slot_time,
            &epoch_info,
            &schedule(),
        );
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TimeTravelError::ZeroSlotTime));
    }

    #[test]
    fn test_edge_case_large_time_jump() {
        let epoch_info = create_test_epoch_info(1, 1000, 433_000);
        let current_time = 1_000_000_000;
        let slot_time = 400;
        let target_time = current_time + 1_000_000_000_000; // Very large jump

        let clock = calculate_absolute_timestamp_clock(
            target_time,
            current_time,
            slot_time,
            &epoch_info,
            &schedule(),
        )
        .unwrap();

        assert_eq!(clock.unix_timestamp, target_time as i64 / 1_000);
        assert!(clock.epoch > 1); // Should advance many epochs

        // A slot whose extrapolated time overflows still lands at the time asked for.
        let config = TimeTravelConfig::AbsoluteSlotAt {
            slot: u64::MAX,
            timestamp: 5_000,
        };
        let clock =
            calculate_time_travel_clock(&config, current_time, slot_time, &epoch_info, &schedule())
                .unwrap();
        assert_eq!((clock.slot, clock.unix_timestamp), (u64::MAX, 5));
    }

    #[test]
    fn test_edge_case_exact_epoch_boundary() {
        let epoch_info = create_test_epoch_info(1, 431_999, 863_999); // Last slot of epoch 1
        let current_time = 1_000_000_000;
        let slot_time = 400;
        let target_time = current_time + slot_time; // Exactly one slot later

        let clock = calculate_absolute_timestamp_clock(
            target_time,
            current_time,
            slot_time,
            &epoch_info,
            &schedule(),
        )
        .unwrap();

        assert_eq!(clock.slot, 864_000); // One slot past 863_999, the first slot of epoch 2
        assert_eq!(clock.epoch, 2);
    }

    /// Under a warmup schedule the first epochs are 32, 64 and 128 slots long, and every kind of
    /// time travel follows the schedule rather than a fixed epoch length, leader schedule epoch
    /// included.
    #[test]
    fn test_time_travel_follows_a_warmup_schedule() {
        let schedule = EpochSchedule::custom(432_000, 432_000, true);
        let epoch_info = create_test_epoch_info(0, 0, 0);
        let current_time = 1_000_000_000;
        let slot_time = 400;
        let travel = |config| {
            let clock = calculate_time_travel_clock(
                &config,
                current_time,
                slot_time,
                &epoch_info,
                &schedule,
            )
            .unwrap();
            (clock.slot, clock.epoch, clock.leader_schedule_epoch)
        };

        assert_eq!(
            [
                travel(TimeTravelConfig::AbsoluteEpoch(3)),
                travel(TimeTravelConfig::AbsoluteSlot(100)),
                travel(TimeTravelConfig::AbsoluteTimestamp(
                    current_time + 100 * slot_time
                )),
            ],
            [(224, 3, 4), (100, 2, 3), (100, 2, 3)]
        );
    }
}
