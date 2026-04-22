use jsonrpc_core::Result;
use jsonrpc_derive::rpc;
use solana_client::{
    rpc_config::{RpcBlockProductionConfig, RpcContextConfig},
    rpc_custom_error::RpcCustomError,
    rpc_response::{
        RpcBlockProduction, RpcInflationGovernor, RpcInflationRate, RpcResponseContext,
    },
};
use solana_clock::Slot;
use solana_commitment_config::CommitmentConfig;
use solana_epoch_schedule::EpochSchedule;
use solana_rpc_client_api::response::Response as RpcResponse;

use super::{RunloopContext, State};
use crate::SURFPOOL_IDENTITY_PUBKEY;

#[rpc]
pub trait BankData {
    type Metadata;

    /// Returns the minimum balance required for rent exemption based on the given data length.
    ///
    /// This RPC method calculates the minimum balance required for an account to be exempt from
    /// rent charges. It uses the data length of the account to determine the balance. The result
    /// can help users manage their accounts by ensuring they have enough balance to cover rent
    /// exemption, preventing accounts from being purged by the system.
    ///
    /// ## Parameters
    /// - `data_len`: The length (in bytes) of the account data. This is used to determine the
    ///   minimum balance required for rent exemption.
    /// - `commitment`: (Optional) A `CommitmentConfig` that allows specifying the level of
    ///   commitment for querying. If not provided, the default commitment level will be used.
    ///
    /// ## Returns
    /// - `Result<u64>`: The method returns the minimum balance required for rent exemption
    ///   as a `u64`. If successful, it will be wrapped in `Ok`, otherwise an error will be
    ///   returned.
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getMinimumBalanceForRentExemption",
    ///   "params": [128]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": 2039280,
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - This method is commonly used to determine the required balance when creating new accounts
    ///   or performing account setup operations that need rent exemption.
    /// - The `commitment` parameter allows users to specify the level of assurance they want
    ///   regarding the state of the ledger. For example, using `Confirmed` or `Finalized` ensures
    ///   that the state is more reliable.
    ///
    /// ## Errors
    /// - If there is an issue with the `data_len` or `commitment` parameter (e.g., invalid data),
    ///   an error will be returned.
    #[rpc(meta, name = "getMinimumBalanceForRentExemption")]
    fn get_minimum_balance_for_rent_exemption(
        &self,
        meta: Self::Metadata,
        data_len: usize,
        commitment: Option<CommitmentConfig>,
    ) -> Result<u64>;

    /// Retrieves the inflation governor settings for the network.
    ///
    /// This RPC method returns the current inflation governor configuration, which controls the
    /// inflation rate of the network. The inflation governor is responsible for adjusting the
    /// inflation rate over time, with parameters like the initial and terminal inflation rates,
    /// the taper rate, the foundation amount, and the foundation term.
    ///
    /// ## Parameters
    /// - `commitment`: (Optional) A `CommitmentConfig` that specifies the commitment level for
    ///   querying the inflation governor settings. If not provided, the default commitment level
    ///   is used. Valid commitment levels include `Processed`, `Confirmed`, or `Finalized`.
    ///
    /// ## Returns
    /// - `Result<RpcInflationGovernor>`: The method returns an `RpcInflationGovernor` struct that
    ///   contains the inflation parameters if successful. Otherwise, an error will be returned.
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getInflationGovernor",
    ///   "params": []
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "initial": 0.15,
    ///     "terminal": 0.05,
    ///     "taper": 0.9,
    ///     "foundation": 0.02,
    ///     "foundation_term": 5.0
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - The inflation governor defines how inflation changes over time, ensuring the network's
    ///   growth remains stable and sustainable.
    /// - The `commitment` parameter allows users to define how strongly they want to ensure the
    ///   inflation data is confirmed or finalized when queried. For example, using `Confirmed` or
    ///   `Finalized` ensures a more reliable inflation state.
    ///
    /// ## Errors
    /// - If there is an issue with the `commitment` parameter or an internal error occurs,
    ///   an error will be returned.
    #[rpc(meta, name = "getInflationGovernor")]
    fn get_inflation_governor(
        &self,
        meta: Self::Metadata,
        commitment: Option<CommitmentConfig>,
    ) -> Result<RpcInflationGovernor>;

    /// Retrieves the current inflation rate for the network.
    ///
    /// This RPC method returns the current inflation rate, including the breakdown of inflation
    /// allocated to different entities such as validators and the foundation, along with the current
    /// epoch during which the rate applies.
    ///
    /// ## Parameters
    /// - No parameters are required for this method.
    ///
    /// ## Returns
    /// - `Result<RpcInflationRate>`: The method returns an `RpcInflationRate` struct that contains
    ///   the total inflation rate, the validator portion, the foundation portion, and the epoch
    ///   during which this inflation rate applies.
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getInflationRate",
    ///   "params": []
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "total": 0.10,
    ///     "validator": 0.07,
    ///     "foundation": 0.03,
    ///     "epoch": 1500
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - The total inflation rate is distributed among validators and the foundation based on
    ///   the configuration defined in the inflation governor.
    /// - The epoch field indicates the current epoch number during which this inflation rate applies.
    ///   An epoch is a period during which the network operates under certain parameters.
    /// - Inflation rates can change over time depending on network conditions and governance decisions.
    ///
    /// ## Errors
    /// - If there is an internal error, or if the RPC request is malformed, an error will be returned.
    #[rpc(meta, name = "getInflationRate")]
    fn get_inflation_rate(&self, meta: Self::Metadata) -> Result<RpcInflationRate>;

    /// Retrieves the epoch schedule for the network.
    ///
    /// This RPC method returns the configuration for the network's epoch schedule, including
    /// details on the number of slots per epoch, leader schedule offsets, and epoch warmup.
    ///
    /// ## Parameters
    /// - No parameters are required for this method.
    ///
    /// ## Returns
    /// - `Result<EpochSchedule>`: The method returns an `EpochSchedule` struct, which contains
    ///   information about the slots per epoch, leader schedule offsets, warmup state, and the
    ///   first epoch after the warmup period.
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getEpochSchedule",
    ///   "params": []
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "slotsPerEpoch": 432000,
    ///     "leaderScheduleSlotOffset": 500,
    ///     "warmup": true,
    ///     "firstNormalEpoch": 8,
    ///     "firstNormalSlot": 1073741824
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - The `slots_per_epoch` defines the maximum number of slots in each epoch, which determines
    ///   the number of time slots available for network validators to produce blocks.
    /// - The `leader_schedule_slot_offset` specifies how many slots before an epoch’s start the leader
    ///   schedule calculation begins for that epoch.
    /// - The `warmup` field indicates whether the epochs start short and grow over time.
    /// - The `first_normal_epoch` marks the first epoch after the warmup period.
    /// - The `first_normal_slot` gives the first slot after the warmup period in terms of the number of slots
    ///   from the start of the network.
    ///
    /// ## Errors
    /// - If the RPC request is malformed, or if there is an internal error, an error will be returned.
    #[rpc(meta, name = "getEpochSchedule")]
    fn get_epoch_schedule(&self, meta: Self::Metadata) -> Result<EpochSchedule>;

    /// Retrieves the leader of the current slot.
    ///
    /// This RPC method returns the leader for the current slot in the Solana network. The leader is responsible
    /// for producing blocks for the current slot. The leader is selected based on the Solana consensus mechanism.
    ///
    /// ## Parameters
    /// - `config`: An optional configuration for the request, which can include:
    ///     - `commitment`: A commitment level that defines how "final" the data must be.
    ///     - `min_context_slot`: An optional parameter to specify a minimum slot for the request.
    ///
    /// ## Returns
    /// - `Result<String>`: The method returns a `String` representing the public key of the leader for the current slot.
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getSlotLeader",
    ///   "params": []
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": "3HgA9r8H9z5Pb2L6Pt5Yq1QoFwgr6YwdKKUh9n2ANp5U"
    /// }
    /// ```
    ///
    /// # Notes
    /// - The leader for a given slot is selected based on the Solana network's consensus mechanism, and this method
    ///   allows you to query the current leader.
    #[rpc(meta, name = "getSlotLeader")]
    fn get_slot_leader(
        &self,
        meta: Self::Metadata,
        config: Option<RpcContextConfig>,
    ) -> Result<String>;

    /// Retrieves the leaders for a specified range of slots.
    ///
    /// This RPC method returns the leaders for a specified range of slots in the Solana network. You can
    /// specify the `start_slot` from which the leaders should be queried and limit the number of results
    /// with the `limit` parameter. The leaders are responsible for producing blocks in the respective slots.
    ///
    /// ## Parameters
    /// - `start_slot`: The starting slot number for which the leaders should be queried.
    /// - `limit`: The number of slots (starting from `start_slot`) for which the leaders should be retrieved.
    ///
    /// ## Returns
    /// - `Result<Vec<String>>`: A vector of `String` values representing the public keys of the leaders for
    ///   the specified slot range.
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getSlotLeaders",
    ///   "params": [1000, 5]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": [
    ///     "3HgA9r8H9z5Pb2L6Pt5Yq1QoFwgr6YwdKKUh9n2ANp5U",
    ///     "BBh1FwXts8EZY6rPZ5kS2ygq99wYjFd5K5daRjc7eF9X",
    ///     "4XYo7yP5J2J8sLNSW3wGYPk3mdS1rbZUy4oFCp7wH1DN",
    ///     "8v1Cp6sHZh8XfGWS7sHZczH3v9NxdgMbo3g91Sh88dcJ",
    ///     "N6bPqwEoD9StS4AnzE27rHyz47tPcsZQjvW9w8p2NhF7"
    ///   ]
    /// }
    /// ```
    ///
    /// # Notes
    /// - The leaders are returned in the order corresponding to the slots queried, starting from `start_slot`
    ///   and continuing for `limit` slots.
    /// - This method provides an efficient way to get multiple leaders for a range of slots, useful for tracking
    ///   leaders over time or for scheduling purposes in decentralized applications.
    #[rpc(meta, name = "getSlotLeaders")]
    fn get_slot_leaders(
        &self,
        meta: Self::Metadata,
        start_slot: Slot,
        limit: u64,
    ) -> Result<Vec<String>>;

    /// Retrieves block production information for the specified validator identity or range of slots.
    ///
    /// This RPC method returns block production details for a given validator identity or a range of slots
    /// within a certain epoch. If no `identity` is provided, the method returns block production data for all
    /// validators. If a `range` is provided, it will return block production information for the slots within
    /// the specified range.
    ///
    /// ## Parameters
    /// - `config`: An optional configuration object that can include:
    ///     - `identity`: The base-58 encoded public key of a validator to query for block production data. If `None`, results for all validators will be returned.
    ///     - `range`: A range of slots for which block production information is needed. The range will default to the current epoch if `None`.
    ///     - `commitment`: The commitment level (optional) to use when querying for the block production data.
    ///
    /// ## Returns
    /// - `Result<RpcResponse<RpcBlockProduction>>`: The result contains a response object with block production data, including the number of leader slots and blocks produced by each validator.
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getBlockProduction",
    ///   "params": [{
    ///     "identity": "3HgA9r8H9z5Pb2L6Pt5Yq1QoFwgr6YwdKKUh9n2ANp5U",
    ///     "range": {
    ///       "firstSlot": 1000,
    ///       "lastSlot": 1050
    ///     }
    ///   }]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "byIdentity": {
    ///       "3HgA9r8H9z5Pb2L6Pt5Yq1QoFwgr6YwdKKUh9n2ANp5U": [10, 8],
    ///       "BBh1FwXts8EZY6rPZ5kS2ygq99wYjFd5K5daRjc7eF9X": [5, 4]
    ///     },
    ///     "range": {
    ///       "firstSlot": 1000,
    ///       "lastSlot": 1050
    ///     }
    ///   }
    /// }
    /// ```
    ///
    /// # Notes
    /// - The response contains a map of validator identities to a tuple of two values:
    ///     - The first value is the number of leader slots.
    ///     - The second value is the number of blocks produced by that validator in the queried range.
    /// - The `range` object specifies the range of slots that the block production information applies to, with `first_slot` being the starting slot and `last_slot` being the optional ending slot.
    ///
    /// ## Example Response Interpretation
    /// - In the example response, the identity `3HgA9r8H9z5Pb2L6Pt5Yq1QoFwgr6YwdKKUh9n2ANp5U` produced 10 leader slots and 8 blocks between slots 1000 and 1050.
    /// - Similarly, `BBh1FwXts8EZY6rPZ5kS2ygq99wYjFd5K5daRjc7eF9X` produced 5 leader slots and 4 blocks in the same slot range.
    #[rpc(meta, name = "getBlockProduction")]
    fn get_block_production(
        &self,
        meta: Self::Metadata,
        config: Option<RpcBlockProductionConfig>,
    ) -> Result<RpcResponse<RpcBlockProduction>>;
}

#[derive(Clone)]
pub struct SurfpoolBankDataRpc;
impl BankData for SurfpoolBankDataRpc {
    type Metadata = Option<RunloopContext>;

    fn get_minimum_balance_for_rent_exemption(
        &self,
        meta: Self::Metadata,
        data_len: usize,
        _commitment: Option<CommitmentConfig>,
    ) -> Result<u64> {
        meta.with_svm_reader(move |svm_reader| {
            svm_reader
                .inner
                .minimum_balance_for_rent_exemption(data_len)
        })
        .map_err(Into::into)
    }

    fn get_inflation_governor(
        &self,
        meta: Self::Metadata,
        _commitment: Option<CommitmentConfig>,
    ) -> Result<RpcInflationGovernor> {
        meta.with_svm_reader(|svm_reader| svm_reader.inflation.into())
            .map_err(Into::into)
    }

    fn get_inflation_rate(&self, meta: Self::Metadata) -> Result<RpcInflationRate> {
        meta.with_svm_reader(|svm_reader| -> RpcInflationRate {
            let inflation_activation_slot = svm_reader
                .blocks
                .keys()
                .unwrap_or_default()
                .into_iter()
                .min()
                .unwrap_or_default();
            let epoch_schedule = svm_reader.inner.get_sysvar::<EpochSchedule>();
            let inflation_start_slot = epoch_schedule.get_first_slot_in_epoch(
                epoch_schedule
                    .get_epoch(inflation_activation_slot)
                    .saturating_sub(1),
            );
            let epoch = svm_reader.latest_epoch_info().epoch;
            let num_slots = epoch_schedule.get_first_slot_in_epoch(epoch) - inflation_start_slot;

            let inflation = svm_reader.inflation;
            let slots_per_year = svm_reader.genesis_config.slots_per_year();

            let slot_in_year = num_slots as f64 / slots_per_year;

            RpcInflationRate {
                total: inflation.total(slot_in_year),
                validator: inflation.validator(slot_in_year),
                foundation: inflation.foundation(slot_in_year),
                epoch,
            }
        })
        .map_err(Into::into)
    }

    fn get_epoch_schedule(&self, meta: Self::Metadata) -> Result<EpochSchedule> {
        meta.with_svm_reader(move |svm_reader| svm_reader.inner.get_sysvar::<EpochSchedule>())
            .map_err(Into::into)
    }

    fn get_slot_leader(
        &self,
        meta: Self::Metadata,
        config: Option<RpcContextConfig>,
    ) -> Result<String> {
        let svm_locker = meta.get_svm_locker()?;
        let config = config.unwrap_or_default();

        let committed_slot =
            svm_locker.get_slot_for_commitment(&config.commitment.unwrap_or_default());

        // validate minContextSlot if provided
        if let Some(min_context_slot) = config.min_context_slot {
            if committed_slot < min_context_slot {
                return Err(RpcCustomError::MinContextSlotNotReached {
                    context_slot: min_context_slot,
                }
                .into());
            }
        }

        Ok(SURFPOOL_IDENTITY_PUBKEY.to_string())
    }

    fn get_slot_leaders(
        &self,
        meta: Self::Metadata,
        start_slot: Slot,
        limit: u64,
    ) -> Result<Vec<String>> {
        if limit > 5000 {
            return Err(jsonrpc_core::Error {
                code: jsonrpc_core::ErrorCode::InvalidParams,
                message: "Limit must be less than 5000".to_string(),
                data: None,
            });
        }

        let svm_locker = meta.get_svm_locker()?;
        let epoch_info = svm_locker.get_epoch_info();

        let first_slot_in_epoch = epoch_info
            .absolute_slot
            .saturating_sub(epoch_info.slot_index);
        let last_slot_in_epoch = first_slot_in_epoch + epoch_info.slots_in_epoch.saturating_sub(1);
        if start_slot > last_slot_in_epoch || (start_slot + limit) > last_slot_in_epoch {
            return Err(jsonrpc_core::Error {
                code: jsonrpc_core::ErrorCode::InvalidParams,
                message: format!(
                    "Invalid slot range: leader schedule for epoch {} is unavailable",
                    epoch_info.epoch
                ),
                data: None,
            });
        }

        Ok(vec![SURFPOOL_IDENTITY_PUBKEY.to_string()])
    }

    fn get_block_production(
        &self,
        meta: Self::Metadata,
        config: Option<RpcBlockProductionConfig>,
    ) -> Result<RpcResponse<RpcBlockProduction>> {
        meta.with_svm_reader(|svm_reader| {
            let current_slot = svm_reader.get_latest_absolute_slot();
            let epoch_info = &svm_reader.latest_epoch_info;

            let (first_slot, last_slot) = if let Some(ref config) = config {
                if let Some(ref range) = config.range {
                    (range.first_slot, range.last_slot.unwrap_or(current_slot))
                } else {
                    let epoch_start_slot = epoch_info.absolute_slot - epoch_info.slot_index;
                    (epoch_start_slot, current_slot)
                }
            } else {
                let epoch_start_slot = epoch_info.absolute_slot - epoch_info.slot_index;
                (epoch_start_slot, current_slot)
            };

            RpcResponse {
                context: RpcResponseContext::new(current_slot),
                value: RpcBlockProduction {
                    // Empty HashMap - no validator block production data in simulation
                    by_identity: std::collections::HashMap::new(),
                    range: solana_client::rpc_response::RpcBlockProductionRange {
                        first_slot,
                        last_slot,
                    },
                },
            }
        })
        .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use solana_client::rpc_config::RpcBlockProductionConfigRange;
    use solana_commitment_config::CommitmentLevel;
    use solana_inflation::Inflation;

    use super::*;
    use crate::tests::helpers::TestSetup;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_epoch_schedule() {
        let setup = TestSetup::new(SurfpoolBankDataRpc);
        let res = setup.rpc.get_epoch_schedule(Some(setup.context)).unwrap();

        assert_eq!(res, EpochSchedule::without_warmup());
    }

    #[test]
    fn test_get_block_production() {
        let setup = TestSetup::new(SurfpoolBankDataRpc);

        // test with no config
        let result = setup
            .rpc
            .get_block_production(Some(setup.context.clone()), None)
            .unwrap();

        // verify empty results (simulation mode)
        assert!(
            result.value.by_identity.is_empty(),
            "Should have no validators in simulation"
        );
        assert!(
            result.value.range.first_slot <= result.value.range.last_slot,
            "Valid slot range"
        );

        // test with custom range
        let config = Some(RpcBlockProductionConfig {
            identity: None,
            range: Some(RpcBlockProductionConfigRange {
                first_slot: 100,
                last_slot: Some(200),
            }),
            commitment: None,
        });

        let result2 = setup
            .rpc
            .get_block_production(Some(setup.context), config)
            .unwrap();

        assert_eq!(result2.value.range.first_slot, 100);
        assert_eq!(result2.value.range.last_slot, 200);
        assert!(result2.value.by_identity.is_empty());
    }

    #[test]
    fn test_get_slot_leaders() {
        let setup = TestSetup::new(SurfpoolBankDataRpc);

        // test with valid parameters
        let result = setup
            .rpc
            .get_slot_leaders(Some(setup.context.clone()), 0, 10)
            .unwrap();

        assert_eq!(
            result[0],
            SURFPOOL_IDENTITY_PUBKEY.to_string(),
            "Should only return one leader - itself"
        );

        // test with invalid limit
        let err = setup
            .rpc
            .get_slot_leaders(Some(setup.context.clone()), 0, 6000)
            .unwrap_err();

        assert_eq!(
            err.code,
            jsonrpc_core::ErrorCode::InvalidParams,
            "Should return InvalidParams error for limit > 5000"
        );

        let latest_slot = setup.context.svm_locker.get_latest_absolute_slot();

        // test with start_slot >= latest_slot
        let err = setup
            .rpc
            .get_slot_leaders(Some(setup.context), latest_slot + 100, 10)
            .unwrap_err();

        assert_eq!(
            err.code,
            jsonrpc_core::ErrorCode::InvalidParams,
            "Should return InvalidParams error for start_slot >= latest_slot"
        );
    }

    #[test]
    fn test_get_inflation_rate() {
        let setup = TestSetup::new(SurfpoolBankDataRpc);
        let result = setup.rpc.get_inflation_rate(Some(setup.context));
        assert!(result.is_ok())
    }

    #[test]
    fn test_get_inflation_governor() {
        let setup = TestSetup::new(SurfpoolBankDataRpc);

        let result = setup
            .rpc
            .get_inflation_governor(Some(setup.context), None)
            .unwrap();

        assert_eq!(result, Inflation::default().into());
    }

    #[test]
    fn test_get_minimum_balance_for_rent_exemption() {
        let setup = TestSetup::new(SurfpoolBankDataRpc);
        let rent = setup
            .rpc
            .get_minimum_balance_for_rent_exemption(Some(setup.context), 0, None)
            .unwrap();

        assert_eq!(rent, 890880)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_slot_leader_basic() {
        let setup = TestSetup::new(SurfpoolBankDataRpc);

        let result = setup.rpc.get_slot_leader(Some(setup.context.clone()), None);

        match result {
            Ok(identity) => {
                assert_eq!(identity, SURFPOOL_IDENTITY_PUBKEY.to_string());
                println!("✅ Basic test passed");
            }
            Err(e) => {
                panic!("❌ Test failed: {:?}", e);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_slot_leader_with_config() {
        let setup = TestSetup::new(SurfpoolBankDataRpc);

        let config = RpcContextConfig {
            commitment: Some(CommitmentConfig {
                commitment: CommitmentLevel::Processed,
            }),
            min_context_slot: None,
        };

        let result = setup
            .rpc
            .get_slot_leader(Some(setup.context.clone()), Some(config));

        match result {
            Ok(identity) => {
                assert_eq!(identity, SURFPOOL_IDENTITY_PUBKEY.to_string());
                println!("✅ Config test passed");
            }
            Err(e) => {
                panic!("❌ Test failed: {:?}", e);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_slot_leader_min_context_slot_error() {
        let setup = TestSetup::new(SurfpoolBankDataRpc);

        let config = RpcContextConfig {
            commitment: Some(CommitmentConfig {
                commitment: CommitmentLevel::Finalized,
            }),
            min_context_slot: Some(999999), // high number that should fail
        };

        let result = setup
            .rpc
            .get_slot_leader(Some(setup.context.clone()), Some(config));

        assert!(result.is_err());
        println!("✅ MinContextSlot error test passed");
    }
}
