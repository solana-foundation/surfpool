use std::{fs, str::FromStr};

use serde::Serialize;
use solana_clock::Slot;
use solana_commitment_config::CommitmentConfig;
use solana_epoch_info::EpochInfo;
use solana_epoch_schedule::EpochSchedule;
use solana_signature::Signature;
use surfpool_core::surfnet::{
    locker::SurfnetSvmLocker,
    remote::{RemoteTransaction, SurfnetRemoteClient},
    svm::{SurfnetSvm, SurfnetSvmConfig},
};
use surfpool_types::{
    RpcProfileResultConfig, UiAccountChange, UiAccountEncoding, UiAccountProfileState,
    UiKeyedProfileResult, UiProfileResult, UuidOrSignature, sanitized_datasource_url,
};

use super::{Context, ProfileCommand};

/// A public RPC node cannot serve account state as it was at a past slot.
const STATE_CAVEAT: &str = "Accounts were read from the datasource at its current slot, address \
     lookup tables included. A different account state can produce a completely different result \
     than the remote's, and a lookup table deactivated since the transaction cannot be resolved \
     at this slot at all.";

/// What `--output` writes. The profile's has uuid `key` from the local surfnet generated, so the
/// signature, slot and block time of the remote transaction are recorded around it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileOutput<'a> {
    signature: String,
    /// The remote slot the transaction landed in, and the slot it re-executed at.
    slot: Slot,
    /// Unix timestamp, in seconds, the remote reports for that slot.
    block_time: Option<i64>,
    profile: &'a UiKeyedProfileResult,
}

/// Re-executes a remote transaction against a fresh surfnet initialized at its original slot.
pub async fn handle_profile_command(cmd: ProfileCommand, _ctx: &Context) -> Result<(), String> {
    let signature = Signature::from_str(cmd.signature.trim())
        .map_err(|e| format!("Invalid transaction signature '{}': {}", cmd.signature, e))?;

    let rpc_url = cmd.datasource_rpc_url();
    let remote_client = SurfnetRemoteClient::new(&rpc_url);
    let displayed_rpc_url =
        sanitized_datasource_url(&rpc_url).unwrap_or_else(|| "the datasource".to_string());

    println!("Fetching {} from {}", signature, displayed_rpc_url);
    let RemoteTransaction {
        transaction,
        slot,
        block_time,
        error: remote_error,
        compute_units_consumed: remote_compute_units,
    } = remote_client
        .get_versioned_transaction(signature)
        .await
        .map_err(|e| e.to_string())?;

    let epoch_schedule = remote_client.get_epoch_schedule().await.map_err(|e| {
        format!(
            "Failed to fetch the epoch schedule from {}: {}",
            displayed_rpc_url, e
        )
    })?;
    let epoch_info = epoch_info_at_slot(slot, &epoch_schedule);

    println!(
        "Re-executing at slot {} (epoch {}){}",
        slot,
        epoch_info.epoch,
        block_time
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|t| format!(", {}", t.format("%Y-%m-%d %H:%M:%S UTC")))
            .unwrap_or_default()
    );

    // The transaction keeps the remote's blockhash. Rewriting it would break the signatures,
    // so the check is skipped.
    let (mut surfnet_svm, _simnet_events_rx, _geyser_events_rx) =
        SurfnetSvm::new(SurfnetSvmConfig {
            surfnet_id: "profile".to_string(),
            skip_blockhash_check: true,
            ..SurfnetSvmConfig::default()
        })
        .map_err(|e| format!("Failed to initialize the local surfnet: {}", e))?;

    let clock_pinned_to_block_time = match block_time {
        Some(block_time) => {
            surfnet_svm.initialize_at_block_time(epoch_info, epoch_schedule, block_time)
        }
        None => {
            surfnet_svm.initialize(epoch_info, epoch_schedule);
            false
        }
    };
    if !clock_pinned_to_block_time {
        println!(
            "{}",
            yellow!(
                "The datasource reports no usable block time for this slot: the clock falls back to the current time."
            )
        );
    }

    let svm_locker = SurfnetSvmLocker::new(surfnet_svm);
    // The same commitment the transaction was fetched at. A transaction that just landed can
    // touch accounts that only exist at confirmed commitment.
    let remote_ctx = Some((remote_client, CommitmentConfig::confirmed()));

    let uuid = svm_locker
        .profile_transaction(&remote_ctx, transaction, None)
        .await
        .map_err(|e| format!("Failed to profile transaction {}: {}", signature, e))?
        .inner;

    // Base64 rather than the default `jsonParsed`. A parsed view of a token or stake account
    // cannot be turned back into the bytes it came from, and `--output` promises the full state.
    let config = RpcProfileResultConfig {
        encoding: Some(UiAccountEncoding::Base64),
        ..RpcProfileResultConfig::default()
    };
    let profile = svm_locker
        .get_profile_result(UuidOrSignature::Uuid(uuid), &config)
        .map_err(|e| format!("Failed to read back the profile of {}: {}", signature, e))?
        .ok_or_else(|| format!("No profile was recorded for {}", signature))?;

    print_profile(&profile, remote_error.as_deref(), remote_compute_units);

    if let Some(output) = &cmd.output {
        let json = serde_json::to_string_pretty(&ProfileOutput {
            signature: signature.to_string(),
            slot,
            block_time,
            profile: &profile,
        })
        .map_err(|e| format!("Failed to serialize the profile: {}", e))?;
        fs::write(output, json)
            .map_err(|e| format!("Failed to write the profile to {}: {}", output, e))?;
        println!("\nFull profile written to {}", output);
    }

    Ok(())
}

fn epoch_info_at_slot(slot: u64, epoch_schedule: &EpochSchedule) -> EpochInfo {
    let (epoch, slot_index) = epoch_schedule.get_epoch_and_slot_index(slot);
    EpochInfo {
        epoch,
        slot_index,
        slots_in_epoch: epoch_schedule.get_slots_in_epoch(epoch),
        absolute_slot: slot,
        // The slot, not the block height at that slot. The two differ whenever slots are skipped,
        // but nothing in this path reads the field.
        block_height: slot,
        transaction_count: None,
    }
}

fn print_profile(
    profile: &UiKeyedProfileResult,
    remote_error: Option<&str>,
    remote_compute_units: Option<u64>,
) {
    let result = &profile.transaction_profile;

    let local_error = result.error_message.as_deref();

    println!();
    if outcome_diverges(result, remote_error) {
        println!(
            "{}\n",
            yellow!(
                "This run reached a different outcome than the remote. What follows describes \
                 this run, not the transaction as the chain ran it."
            )
        );
        println!("Status         {}", describe_status(local_error));
        println!("Remote status  {}", describe_status(remote_error));
    } else {
        println!(
            "Status         {}  {}",
            describe_status(local_error),
            black!("(matches the remote)")
        );
    }

    let cost_note = match remote_compute_units {
        None => String::new(),
        Some(units) if cost_diverges(result, remote_compute_units) => {
            format!("  {}", yellow!(format!("(remote: {})", units)))
        }
        Some(_) => format!("  {}", black!("(matches the remote)")),
    };
    println!(
        "Compute units  {}{}",
        result.compute_units_consumed, cost_note
    );

    if let Some(logs) = &result.log_messages
        && !logs.is_empty()
    {
        println!("\nLogs");
        for log in logs {
            println!("  {}", log);
        }
    }

    let changes = result
        .account_states
        .iter()
        .filter_map(|(pubkey, state)| describe_account_change(state).map(|change| (pubkey, change)))
        .collect::<Vec<_>>();
    if !changes.is_empty() {
        println!("\nAccount changes");
        for (pubkey, change) in changes {
            println!("  {}  {}", pubkey, change);
        }
    }

    println!("\n{}", yellow!(STATE_CAVEAT));
}

fn describe_status(error: Option<&str>) -> String {
    match error {
        None => green!("SUCCESS"),
        Some(error) => format!("{} ({})", red!("FAILED"), error),
    }
}

/// Whether this run and the remote reached a different outcome.
///
/// Compares the error messages rather than just success against failure, because both sides
/// can fail for different reasons.
fn outcome_diverges(result: &UiProfileResult, remote_error: Option<&str>) -> bool {
    result.error_message.as_deref() != remote_error
}

/// Whether this run cost a different number of compute units than the remote reports.
///
/// A different cost is normal, because the accounts have moved on since the transaction landed.
/// The caller prints the remote number next to this one instead of warning.
fn cost_diverges(result: &UiProfileResult, remote_compute_units: Option<u64>) -> bool {
    remote_compute_units.is_some_and(|units| units != result.compute_units_consumed)
}

/// What the transaction did to one account, or [None] if it left it unchanged.
fn describe_account_change(state: &UiAccountProfileState) -> Option<String> {
    match state {
        UiAccountProfileState::Readonly => None,
        UiAccountProfileState::Writable(change) => match change {
            UiAccountChange::Unchanged(_) => None,
            UiAccountChange::Create(after) => Some(format!("created, {} lamports", after.lamports)),
            UiAccountChange::Delete(before) => {
                Some(format!("deleted, was {} lamports", before.lamports))
            }
            UiAccountChange::Update(before, after) => {
                let mut changes = Vec::new();
                if before.lamports != after.lamports {
                    changes.push(format!(
                        "{} -> {} lamports",
                        before.lamports, after.lamports
                    ));
                }
                if before.data != after.data {
                    changes.push("data changed".to_string());
                }
                if before.owner != after.owner {
                    changes.push(format!("owner {} -> {}", before.owner, after.owner));
                }
                if before.executable != after.executable {
                    changes.push(format!(
                        "executable {} -> {}",
                        before.executable, after.executable
                    ));
                }
                if before.rent_epoch != after.rent_epoch {
                    changes.push(format!(
                        "rent epoch {} -> {}",
                        before.rent_epoch, after.rent_epoch
                    ));
                }
                // `Update` compares the raw accounts, but the encoding used here can hide
                // the difference.
                if changes.is_empty() {
                    changes.push("changed".to_string());
                }
                Some(changes.join(", "))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_result(error: Option<&str>, compute_units_consumed: u64) -> UiProfileResult {
        UiProfileResult {
            account_states: Default::default(),
            compute_units_consumed,
            log_messages: None,
            error_message: error.map(str::to_string),
        }
    }

    /// The case a user is most likely to read as a bug in the tool: the explorer shows the
    /// transaction succeeding, and it fails here.
    #[test]
    fn a_transaction_that_only_fails_here_diverges() {
        let result = profile_result(Some("custom program error: 0x0"), 300);

        assert!(outcome_diverges(&result, None));
    }

    #[test]
    fn a_transaction_that_only_fails_on_the_remote_diverges() {
        let result = profile_result(None, 300);

        assert!(outcome_diverges(&result, Some("custom program error: 0x0")));
    }

    /// Both sides fail, so nothing looks wrong, but the reason shown is one the chain never
    /// hit. Checking only whether each side failed would miss this.
    #[test]
    fn a_failure_for_another_reason_than_the_remote_diverges() {
        let result = profile_result(Some("custom program error: 0x1"), 4500);

        assert!(outcome_diverges(
            &result,
            Some("Transaction results in an account (1) with insufficient funds for rent")
        ));
    }

    #[test]
    fn a_run_that_matches_the_remote_diverges_in_neither_outcome_nor_cost() {
        let result = profile_result(Some("custom program error: 0x6f"), 7487);

        assert!(!outcome_diverges(
            &result,
            Some("custom program error: 0x6f")
        ));
        assert!(!cost_diverges(&result, Some(7487)));
    }

    /// The run cost a different amount against today's accounts, but it reached the same
    /// outcome as the chain, so it must not warn that it does not match.
    #[test]
    fn a_different_price_is_a_cost_gap_and_not_an_outcome_gap() {
        let result = profile_result(None, 683001);

        assert!(cost_diverges(&result, Some(438566)));
        assert!(!outcome_diverges(&result, None));
    }

    /// A datasource that reports no compute units leaves nothing to compare.
    #[test]
    fn an_unknown_remote_cost_is_not_a_gap() {
        let result = profile_result(None, 7487);

        assert!(!cost_diverges(&result, None));
    }

    #[test]
    fn the_epoch_of_a_slot_holds_that_slot() {
        let epoch_schedule = EpochSchedule::default();
        let slot = 441_671_636;

        let epoch_info = epoch_info_at_slot(slot, &epoch_schedule);

        assert_eq!(epoch_info.absolute_slot, slot);
        assert_eq!(
            epoch_schedule.get_first_slot_in_epoch(epoch_info.epoch) + epoch_info.slot_index,
            slot,
            "the epoch and the index within it should add back up to the slot"
        );
    }
}
