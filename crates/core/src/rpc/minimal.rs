use jsonrpc_core::{BoxFuture, Result};
use jsonrpc_derive::rpc;
use solana_client::{
    rpc_config::{
        RpcContextConfig, RpcGetVoteAccountsConfig, RpcLeaderScheduleConfig,
        RpcLeaderScheduleConfigWrapper,
    },
    rpc_custom_error::RpcCustomError,
    rpc_response::{
        RpcIdentity, RpcLeaderSchedule, RpcResponseContext, RpcSnapshotSlotInfo,
        RpcVoteAccountStatus,
    },
};
use solana_clock::Slot;
use solana_commitment_config::CommitmentLevel;
use solana_epoch_info::EpochInfo;
use solana_rpc_client_api::response::Response as RpcResponse;

use super::{RunloopContext, SurfnetRpcContext};
use crate::{
    SURFPOOL_IDENTITY_PUBKEY,
    rpc::{State, utils::verify_pubkey},
    surfnet::{FINALIZATION_SLOT_THRESHOLD, GetAccountResult, locker::SvmAccessContext},
};

const SURFPOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct SurfpoolRpcVersionInfo {
    /// The current version of surfpool
    pub surfnet_version: String,
    /// The current version of solana-core
    pub solana_core: String,
    /// first 4 bytes of the FeatureSet identifier
    pub feature_set: Option<u32>,
}

#[rpc]
pub trait Minimal {
    type Metadata;

    /// Returns the balance (in lamports) of the account at the provided public key.
    ///
    /// This endpoint queries the current or historical balance of an account, depending on the optional commitment level provided in the config.
    ///
    /// ## Parameters
    /// - `pubkey_str`: The base-58 encoded public key of the account to query.
    /// - `_config` *(optional)*: [`RpcContextConfig`] specifying commitment level and/or minimum context slot.
    ///
    /// ## Returns
    /// An [`RpcResponse<u64>`] where the value is the balance in lamports.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getBalance",
    ///   "params": [
    ///     "4Nd1mXUmh23rQk8VN7wM9hEnfxqrrB1yrn11eW9gMoVr"
    ///   ]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "context": {
    ///       "slot": 1085597
    ///     },
    ///     "value": 20392800
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - 1 SOL = 1,000,000,000 lamports.
    /// - Use commitment level in the config to specify whether the balance should be fetched from processed, confirmed, or finalized state.
    ///
    /// # See Also
    /// - `getAccountInfo`, `getTokenAccountBalance`
    #[rpc(meta, name = "getBalance")]
    fn get_balance(
        &self,
        meta: Self::Metadata,
        pubkey_str: String,
        config: Option<RpcContextConfig>,
    ) -> BoxFuture<Result<RpcResponse<u64>>>;

    /// Returns information about the current epoch.
    ///
    /// This endpoint provides epoch-related data such as the current epoch number, the total number of slots in the epoch,
    /// the current slot index within the epoch, and the absolute slot number.
    ///
    /// ## Parameters
    /// - `config` *(optional)*: [`RpcContextConfig`] for specifying commitment level and/or minimum context slot.
    ///
    /// ## Returns
    /// An [`EpochInfo`] struct containing information about the current epoch.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getEpochInfo"
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "epoch": 278,
    ///     "slotIndex": 423,
    ///     "slotsInEpoch": 432000,
    ///     "absoluteSlot": 124390823,
    ///     "blockHeight": 18962432,
    ///     "transactionCount": 981234523
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - The `slotIndex` is the current slot's position within the epoch.
    /// - `slotsInEpoch` may vary due to network adjustments (e.g., warm-up periods).
    /// - The `commitment` field in the config can influence how recent the returned data is.
    ///
    /// # See Also
    /// - `getEpochSchedule`, `getSlot`, `getBlockHeight`
    #[rpc(meta, name = "getEpochInfo")]
    fn get_epoch_info(
        &self,
        meta: Self::Metadata,
        config: Option<RpcContextConfig>,
    ) -> Result<EpochInfo>;

    /// Returns the genesis hash of the blockchain.
    ///
    /// The genesis hash is a unique identifier that represents the state of the blockchain at the genesis block (the very first block).
    /// This can be used to validate the integrity of the blockchain and ensure that a node is operating with the correct blockchain data.
    ///
    /// ## Parameters
    /// - None.
    ///
    /// ## Returns
    /// A `String` containing the base-58 encoded genesis hash.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getGenesisHash"
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": "5eymX3jrWXcKqD1tsB2BzAB6gX9LP2pLrpVG6KwBSoZJ"
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - The genesis hash is a critical identifier for validating the blockchain’s origin and initial state.
    /// - This endpoint does not require any parameters and provides a quick way to verify the genesis hash for blockchain verification or initial setup.
    ///
    /// # See Also
    /// - `getEpochInfo`, `getBlock`, `getClusterNodes`
    #[rpc(meta, name = "getGenesisHash")]
    fn get_genesis_hash(&self, meta: Self::Metadata) -> BoxFuture<Result<String>>;

    /// Returns the health status of the blockchain node.
    ///
    /// This method checks the health of the node and returns a status indicating whether the node is in a healthy state
    /// or if it is experiencing any issues such as being out of sync with the network or encountering any failures.
    ///
    /// ## Parameters
    /// - None.
    ///
    /// ## Returns
    /// A `String` indicating the health status of the node:
    /// - `"ok"`: The node is healthy and synchronized with the network.
    /// - `"failed"`: The node is not healthy or is experiencing issues.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getHealth"
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": "ok",
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - The `"ok"` response means that the node is fully operational and synchronized with the blockchain.
    /// - The `"failed"` response indicates that the node is either out of sync, has encountered an error, or is not functioning properly.
    /// - This is typically used to monitor the health of the node in production environments.
    ///
    /// # See Also
    /// - `getGenesisHash`, `getEpochInfo`, `getBlock`
    #[rpc(meta, name = "getHealth")]
    fn get_health(&self, meta: Self::Metadata) -> Result<String>;

    /// Returns the identity (public key) of the node.
    ///
    /// This method retrieves the current identity of the node, which is represented by a public key.
    /// The identity is used to uniquely identify the node on the network.
    ///
    /// ## Parameters
    /// - None.
    ///
    /// ## Returns
    /// A `RpcIdentity` object containing the identity of the node:
    /// - `identity`: The base-58 encoded public key of the node's identity.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getIdentity"
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "identity": "Base58EncodedPublicKeyHere"
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - The identity returned is a base-58 encoded public key representing the current node.
    /// - This identity is often used for network identification and security.
    ///
    /// # See Also
    /// - `getGenesisHash`, `getHealth`, `getBlock`
    #[rpc(meta, name = "getIdentity")]
    fn get_identity(&self, meta: Self::Metadata) -> Result<RpcIdentity>;

    /// Returns the current slot of the ledger.
    ///
    /// This method retrieves the current slot number in the blockchain, which represents a point in the ledger's history.
    /// Slots are used to organize and validate the timing of transactions in the network.
    ///
    /// ## Parameters
    /// - `config` (optional): Configuration options for the request, such as commitment level or context slot. Defaults to `None`.
    ///
    /// ## Returns
    /// A `Slot` value representing the current slot of the ledger.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getSlot"
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": 12345678,
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - The slot represents the position in the ledger. It increments over time as new blocks are produced.
    ///
    /// # See Also
    /// - `getBlock`, `getEpochInfo`, `getGenesisHash`
    #[rpc(meta, name = "getSlot")]
    fn get_slot(&self, meta: Self::Metadata, config: Option<RpcContextConfig>) -> Result<Slot>;

    /// Returns the current block height.
    ///
    /// This method retrieves the height of the most recent block in the ledger, which is an indicator of how many blocks have been added to the blockchain. The block height is the number of blocks that have been produced since the genesis block.
    ///
    /// ## Parameters
    /// - `config` (optional): Configuration options for the request, such as commitment level or context slot. Defaults to `None`.
    ///
    /// ## Returns
    /// A `u64` representing the current block height of the ledger.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getBlockHeight"
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": 12345678,
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - The block height reflects the number of blocks produced in the ledger, starting from the genesis block. It is incremented each time a new block is added.
    ///
    /// # See Also
    /// - `getSlot`, `getEpochInfo`, `getGenesisHash`
    #[rpc(meta, name = "getBlockHeight")]
    fn get_block_height(
        &self,
        meta: Self::Metadata,
        config: Option<RpcContextConfig>,
    ) -> Result<u64>;

    /// Returns information about the highest snapshot slot.
    ///
    /// This method retrieves information about the most recent snapshot slot, which refers to the slot in the blockchain where the most recent snapshot has been taken. A snapshot is a point-in-time capture of the state of the ledger, allowing for quicker validation of the state without processing every transaction.
    ///
    /// ## Parameters
    /// - `meta`: Metadata passed with the request, such as the client’s request context.
    ///
    /// ## Returns
    /// A `RpcSnapshotSlotInfo` containing information about the highest snapshot slot.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getHighestSnapshotSlot"
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "slot": 987654,
    ///     "root": "A9B7F1A4D1D55D0635B905E5AB6341C5D9F7F4D2A1160C53B5647B1E3259BB24"
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - The snapshot slot represents the most recent snapshot in the blockchain and is used for more efficient state validation and recovery.
    /// - The result also includes the root, which is the blockhash at the snapshot point.
    ///
    /// # See Also
    /// - `getBlock`, `getSnapshotInfo`
    #[rpc(meta, name = "getHighestSnapshotSlot")]
    fn get_highest_snapshot_slot(&self, meta: Self::Metadata) -> Result<RpcSnapshotSlotInfo>;

    /// Returns the total number of transactions processed by the blockchain.
    ///
    /// This method retrieves the number of transactions that have been processed in the blockchain up to the current point. It provides a snapshot of the transaction throughput and can be useful for monitoring and performance analysis.
    ///
    /// ## Parameters
    /// - `meta`: Metadata passed with the request, such as the client’s request context.
    /// - `config`: Optional configuration for the request, such as commitment settings or minimum context slot.
    ///
    /// ## Returns
    /// A `u64` representing the total transaction count.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getTransactionCount"
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": 1234567890,
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - This method gives a cumulative count of all transactions in the blockchain from the start of the network.
    ///
    /// # See Also
    /// - `getBlockHeight`, `getEpochInfo`
    #[rpc(meta, name = "getTransactionCount")]
    fn get_transaction_count(
        &self,
        meta: Self::Metadata,
        config: Option<RpcContextConfig>,
    ) -> Result<u64>;

    /// Returns the current version of the server or application.
    ///
    /// This method retrieves the version information for the server or application. It provides details such as the version number and additional metadata that can help with compatibility checks or updates.
    ///
    /// ## Parameters
    /// - `meta`: Metadata passed with the request, such as the client’s request context.
    ///
    /// ## Returns
    /// A `SurfpoolRpcVersionInfo` object containing the version details.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getVersion"
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "surfnet_version": "1.2.3",
    ///     "solana_core": "1.9.0",
    ///     "feature_set": 12345
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - The version information typically includes the version number of `surfpool`, the version of `solana-core`, and a `feature_set` identifier (first 4 bytes).
    /// - The `feature_set` field may not always be present, depending on whether a feature set identifier is available.
    ///
    /// # See Also
    /// - `getHealth`, `getIdentity`
    #[rpc(meta, name = "getVersion")]
    fn get_version(&self, meta: Self::Metadata) -> Result<SurfpoolRpcVersionInfo>;

    /// Returns vote account information.
    ///
    /// This method retrieves the current status of vote accounts, including information about the validator’s vote account and whether it is delinquent. The response includes vote account details such as the stake, commission, vote history, and more.
    ///
    /// ## Parameters
    /// - `meta`: Metadata passed with the request, such as the client’s request context.
    /// - `config`: Optional configuration parameters, such as specific vote account addresses or commitment settings.
    ///
    /// ## Returns
    /// A `RpcVoteAccountStatus` object containing details about the current and delinquent vote accounts.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getVoteAccounts",
    ///   "params": [{}]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "current": [
    ///       {
    ///         "votePubkey": "votePubkeyBase58",
    ///         "nodePubkey": "nodePubkeyBase58",
    ///         "activatedStake": 1000000,
    ///         "commission": 5,
    ///         "epochVoteAccount": true,
    ///         "epochCredits": [[1, 1000, 900], [2, 1100, 1000]],
    ///         "lastVote": 1000,
    ///         "rootSlot": 1200
    ///       }
    ///     ],
    ///     "delinquent": [
    ///       {
    ///         "votePubkey": "delinquentVotePubkeyBase58",
    ///         "nodePubkey": "delinquentNodePubkeyBase58",
    ///         "activatedStake": 0,
    ///         "commission": 10,
    ///         "epochVoteAccount": false,
    ///         "epochCredits": [[1, 500, 400]],
    ///         "lastVote": 0,
    ///         "rootSlot": 0
    ///       }
    ///     ]
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - The `current` field contains details about vote accounts that are active and have current stake.
    /// - The `delinquent` field contains details about vote accounts that have become delinquent due to inactivity or other issues.
    /// - The `epochCredits` field contains historical voting data.
    ///
    /// # See Also
    /// - `getHealth`, `getIdentity`, `getVersion`
    #[rpc(meta, name = "getVoteAccounts")]
    fn get_vote_accounts(
        &self,
        meta: Self::Metadata,
        config: Option<RpcGetVoteAccountsConfig>,
    ) -> Result<RpcVoteAccountStatus>;

    /// Returns the leader schedule for the given configuration or slot.
    ///
    /// This method retrieves the leader schedule for the given slot or configuration, providing a map of validator identities to slot indices within a given range.
    ///
    /// ## Parameters
    /// - `meta`: Metadata passed with the request, such as the client’s request context.
    /// - `options`: Optional configuration wrapper, which can either be a specific slot or a full configuration.
    /// - `config`: Optional configuration containing the validator identity and commitment level.
    ///
    /// ## Returns
    /// An `Option<RpcLeaderSchedule>` containing a map of leader identities (base-58 encoded pubkeys) to slot indices, relative to the first epoch slot.
    ///
    /// ## Example Request
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getLeaderSchedule",
    ///   "params": [{}]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "votePubkey1": [0, 2, 4],
    ///     "votePubkey2": [1, 3, 5]
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// # Notes
    /// - The returned map contains validator identities as keys (base-58 encoded strings), with slot indices as values.
    ///
    /// # See Also
    /// - `getSlot`, `getBlockHeight`, `getEpochInfo`
    #[rpc(meta, name = "getLeaderSchedule")]
    fn get_leader_schedule(
        &self,
        meta: Self::Metadata,
        options: Option<RpcLeaderScheduleConfigWrapper>,
        config: Option<RpcLeaderScheduleConfig>,
    ) -> Result<Option<RpcLeaderSchedule>>;
}

#[derive(Clone)]
pub struct SurfpoolMinimalRpc;
impl Minimal for SurfpoolMinimalRpc {
    type Metadata = Option<RunloopContext>;

    fn get_balance(
        &self,
        meta: Self::Metadata,
        pubkey_str: String,
        config: Option<RpcContextConfig>,
    ) -> BoxFuture<Result<RpcResponse<u64>>> {
        let pubkey = match verify_pubkey(&pubkey_str) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        let config = config.unwrap_or_default();
        let commitment_config = config.commitment.unwrap_or_default();
        let min_ctx_slot = config.min_context_slot;

        let SurfnetRpcContext {
            svm_locker,
            remote_ctx,
        } = match meta.get_rpc_context(commitment_config) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        Box::pin(async move {
            #[cfg(feature = "prometheus")]
            let rpc_start = std::time::Instant::now();

            let SvmAccessContext {
                slot,
                inner: account_update,
                ..
            } = svm_locker.get_account(&remote_ctx, &pubkey, None).await?;

            if let Some(min_slot) = min_ctx_slot
                && slot < min_slot
            {
                return Err(RpcCustomError::MinContextSlotNotReached {
                    context_slot: min_slot,
                }
                .into());
            }

            let balance = match &account_update {
                GetAccountResult::FoundAccount(_, account, _)
                | GetAccountResult::FoundProgramAccount((_, account), _)
                | GetAccountResult::FoundTokenAccount((_, account), _) => account.lamports,
                GetAccountResult::None(_) => 0,
            };

            svm_locker.write_account_update(account_update);

            #[cfg(feature = "prometheus")]
            if let Some(m) = crate::telemetry::metrics() {
                m.record_rpc_request("getBalance", rpc_start.elapsed().as_millis() as u64);
            }

            Ok(RpcResponse {
                context: RpcResponseContext::new(slot),
                value: balance,
            })
        })
    }

    fn get_epoch_info(
        &self,
        meta: Self::Metadata,
        _config: Option<RpcContextConfig>,
    ) -> Result<EpochInfo> {
        meta.with_svm_reader(|svm_reader| svm_reader.latest_epoch_info.clone())
            .map_err(Into::into)
    }

    fn get_genesis_hash(&self, meta: Self::Metadata) -> BoxFuture<Result<String>> {
        let SurfnetRpcContext {
            svm_locker,
            remote_ctx,
        } = match meta.get_rpc_context(()) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        Box::pin(async move {
            Ok(svm_locker
                .get_genesis_hash(&remote_ctx.map(|(client, _)| client))
                .await?
                .inner
                .to_string())
        })
    }

    fn get_health(&self, _meta: Self::Metadata) -> Result<String> {
        // todo: we could check the time from the state clock and compare
        Ok("ok".to_string())
    }

    fn get_identity(&self, _meta: Self::Metadata) -> Result<RpcIdentity> {
        Ok(RpcIdentity {
            identity: SURFPOOL_IDENTITY_PUBKEY.to_string(),
        })
    }

    fn get_slot(&self, meta: Self::Metadata, config: Option<RpcContextConfig>) -> Result<Slot> {
        let config = config.unwrap_or_default();
        let latest_absolute_slot = meta
            .with_svm_reader(|svm_reader| svm_reader.get_latest_absolute_slot())
            .map_err(Into::<jsonrpc_core::Error>::into)?;
        let slot = match config.commitment.unwrap_or_default().commitment {
            CommitmentLevel::Processed => latest_absolute_slot,
            CommitmentLevel::Confirmed => latest_absolute_slot - 1,
            CommitmentLevel::Finalized => latest_absolute_slot - FINALIZATION_SLOT_THRESHOLD,
        };

        if let Some(min_context_slot) = config.min_context_slot {
            if slot < min_context_slot {
                return Err(RpcCustomError::MinContextSlotNotReached {
                    context_slot: min_context_slot,
                }
                .into());
            }
        }

        Ok(slot)
    }

    fn get_block_height(
        &self,
        meta: Self::Metadata,
        config: Option<RpcContextConfig>,
    ) -> Result<u64> {
        let config = config.unwrap_or_default();

        if let Some(target_slot) = config.min_context_slot {
            let block_exists =
                meta.with_svm_reader(|svm_reader| svm_reader.blocks.contains_key(&target_slot))??;

            if !block_exists {
                return Err(jsonrpc_core::Error::invalid_params(format!(
                    "Block not found for slot: {}",
                    target_slot
                )));
            }
        }

        meta.with_svm_reader(|svm_reader| {
            if let Some(target_slot) = config.min_context_slot {
                if let Some(block_header) = svm_reader.blocks.get(&target_slot)? {
                    return Ok(block_header.block_height);
                }
            }

            // default behavior: return the latest block height with commitment adjustments
            let latest_block_height = svm_reader.latest_epoch_info.block_height;

            let block_height = match config.commitment.unwrap_or_default().commitment {
                CommitmentLevel::Processed => latest_block_height,
                CommitmentLevel::Confirmed => latest_block_height.saturating_sub(1),
                CommitmentLevel::Finalized => {
                    latest_block_height.saturating_sub(FINALIZATION_SLOT_THRESHOLD)
                }
            };
            Ok::<u64, jsonrpc_core::Error>(block_height)
        })?
        .map_err(Into::into)
    }

    fn get_highest_snapshot_slot(&self, _meta: Self::Metadata) -> Result<RpcSnapshotSlotInfo> {
        Err(jsonrpc_core::Error {
            code: jsonrpc_core::ErrorCode::ServerError(-32008),
            message: "No snapshot".into(),
            data: None,
        })
    }

    fn get_transaction_count(
        &self,
        meta: Self::Metadata,
        _config: Option<RpcContextConfig>,
    ) -> Result<u64> {
        meta.with_svm_reader(|svm_reader| svm_reader.transactions_processed)
            .map_err(Into::into)
    }

    fn get_version(&self, _: Self::Metadata) -> Result<SurfpoolRpcVersionInfo> {
        let version = solana_version::Version::default();

        Ok(SurfpoolRpcVersionInfo {
            surfnet_version: SURFPOOL_VERSION.to_string(),
            solana_core: version.to_string(),
            feature_set: Some(version.feature_set()),
        })
    }

    fn get_vote_accounts(
        &self,
        _meta: Self::Metadata,
        config: Option<RpcGetVoteAccountsConfig>,
    ) -> Result<RpcVoteAccountStatus> {
        // validate inputs if provided
        if let Some(config) = config {
            // validate vote_pubkey if provided
            if let Some(vote_pubkey_str) = config.vote_pubkey {
                verify_pubkey(&vote_pubkey_str)?;
            }
        }

        // Return empty vote accounts
        Ok(RpcVoteAccountStatus {
            current: vec![],
            delinquent: vec![],
        })
    }

    fn get_leader_schedule(
        &self,
        meta: Self::Metadata,
        options: Option<RpcLeaderScheduleConfigWrapper>,
        config: Option<RpcLeaderScheduleConfig>,
    ) -> Result<Option<RpcLeaderSchedule>> {
        let (slot, maybe_config) = options.map(|options| options.unzip()).unwrap_or_default();
        let config = maybe_config.or(config).unwrap_or_default();

        if let Some(ref identity) = config.identity {
            let _ = verify_pubkey(identity)?;
        }

        let svm_locker = meta.get_svm_locker()?;
        let epoch_info = svm_locker.get_epoch_info();

        let slot = slot.unwrap_or(epoch_info.absolute_slot);

        let first_slot_in_epoch = epoch_info
            .absolute_slot
            .saturating_sub(epoch_info.slot_index);
        let last_slot_in_epoch = first_slot_in_epoch + epoch_info.slots_in_epoch.saturating_sub(1);

        if slot < first_slot_in_epoch || slot > last_slot_in_epoch {
            return Ok(None);
        }

        // return empty leader schedule if epoch found and everything is valid
        Ok(Some(std::collections::HashMap::new()))
    }
}

#[cfg(test)]
mod tests {
    use jsonrpc_core::ErrorCode;
    use solana_client::rpc_config::RpcContextConfig;
    use solana_commitment_config::CommitmentConfig;
    use solana_epoch_info::EpochInfo;
    use solana_genesis_config::GenesisConfig;
    use solana_pubkey::Pubkey;

    use super::*;
    use crate::{tests::helpers::TestSetup, types::SyntheticBlockhash};

    #[test]
    fn test_get_block_height_processed_commitment() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);

        let config = RpcContextConfig {
            commitment: Some(CommitmentConfig::processed()),
            min_context_slot: None,
        };

        let expected_height = setup
            .rpc
            .get_block_height(Some(setup.context.clone()), Some(config));
        assert!(expected_height.is_ok());

        let latest_epoch_block_height = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_epoch_info.block_height);

        assert_eq!(expected_height.unwrap(), latest_epoch_block_height);
    }

    #[test]
    fn test_get_block_height_confirmed_commitment() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);

        let config = RpcContextConfig {
            commitment: Some(CommitmentConfig::confirmed()),
            min_context_slot: None,
        };

        let expected_height = setup
            .rpc
            .get_block_height(Some(setup.context.clone()), Some(config));
        assert!(expected_height.is_ok());

        let latest_epoch_block_height = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_epoch_info.block_height);

        assert_eq!(expected_height.unwrap(), latest_epoch_block_height - 1);
    }

    #[test]
    fn test_get_block_height_with_min_context_slot() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);

        // create blocks at specific slots with known block heights
        let test_cases = vec![(100, 50), (200, 150), (300, 275)];

        {
            let mut svm_writer = setup.context.svm_locker.0.blocking_write();
            for (slot, block_height) in &test_cases {
                svm_writer
                    .blocks
                    .store(
                        *slot,
                        crate::surfnet::BlockHeader {
                            hash: SyntheticBlockhash::new(*slot).to_string(),
                            previous_blockhash: SyntheticBlockhash::new(slot - 1).to_string(),
                            block_time: chrono::Utc::now().timestamp_millis(),
                            block_height: *block_height,
                            parent_slot: slot - 1,
                            signatures: Vec::new(),
                        },
                    )
                    .unwrap();
            }
        }

        for (slot, expected_height) in test_cases {
            let config = RpcContextConfig {
                commitment: None,
                min_context_slot: Some(slot),
            };

            let result = setup
                .rpc
                .get_block_height(Some(setup.context.clone()), Some(config));
            assert!(
                result.is_ok(),
                "failed to get block height for slot {}",
                slot
            );
            assert_eq!(
                result.unwrap(),
                expected_height,
                "Wrong block height for slot {}",
                slot
            );
        }
    }

    #[test]
    fn test_get_block_height_error_case_slot_not_found() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);

        {
            let mut svm_writer = setup.context.svm_locker.0.blocking_write();
            svm_writer
                .blocks
                .store(
                    100,
                    crate::surfnet::BlockHeader {
                        hash: SyntheticBlockhash::new(100).to_string(),
                        previous_blockhash: SyntheticBlockhash::new(99).to_string(),
                        block_time: chrono::Utc::now().timestamp_millis(),
                        block_height: 50,
                        parent_slot: 99,
                        signatures: Vec::new(),
                    },
                )
                .unwrap();
        }

        // slot that definitely doesn't exist
        let nonexistent_slot = 999;
        let config = RpcContextConfig {
            commitment: None,
            min_context_slot: Some(nonexistent_slot),
        };

        let result = setup
            .rpc
            .get_block_height(Some(setup.context), Some(config));

        assert!(
            result.is_err(),
            "Expected error for nonexistent slot {}",
            nonexistent_slot
        );

        let error = result.unwrap_err();

        assert_eq!(error.code, jsonrpc_core::types::ErrorCode::InvalidParams);
        assert!(
            error.message.contains("Block not found for slot"),
            "Error message should mention block not found, got: {}",
            error.message
        );
        assert!(
            error.message.contains(&nonexistent_slot.to_string()),
            "Error message should include the slot number, got: {}",
            error.message
        );
    }

    #[test]
    fn test_get_health() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);
        let result = setup.rpc.get_health(Some(setup.context));
        assert_eq!(result.unwrap(), "ok");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_balance() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);

        let airdrop_amount = 5 * 1_000_000_000u64;
        let to_airdrop_pubkey = Pubkey::new_unique();

        setup
            .context
            .svm_locker
            .airdrop(&to_airdrop_pubkey, airdrop_amount)
            .unwrap()
            .unwrap();

        let pass_if_correct_config_result = setup
            .rpc
            .get_balance(
                Some(setup.context.clone()),
                to_airdrop_pubkey.to_string(),
                None,
            )
            .await;

        assert!(
            pass_if_correct_config_result.is_ok(),
            "Expected the operation to pass"
        );

        assert_eq!(
            pass_if_correct_config_result.unwrap().value,
            airdrop_amount,
            "Invalid returned lamports for the account"
        );

        let wrong_min_slot = setup.context.svm_locker.get_latest_absolute_slot() + 100;

        let fail_if_latest_slot_lt_min_ctx_slot_result = setup
            .rpc
            .get_balance(
                Some(setup.context.clone()),
                Pubkey::new_unique().to_string(),
                Some(RpcContextConfig {
                    commitment: None,
                    min_context_slot: Some(wrong_min_slot),
                }),
            )
            .await;

        let expected_err: Result<()> = Result::Err(
            RpcCustomError::MinContextSlotNotReached {
                context_slot: wrong_min_slot,
            }
            .into(),
        );

        assert!(
            fail_if_latest_slot_lt_min_ctx_slot_result.is_err(),
            "Expected get_balance rpc method to fail when latest_absolute_slot < min_context_slot"
        );

        assert_eq!(
            fail_if_latest_slot_lt_min_ctx_slot_result.err().unwrap(),
            expected_err.err().unwrap()
        );
    }

    #[test]
    fn test_get_transaction_count() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);
        let transactions_processed = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.transactions_processed);
        let result = setup.rpc.get_transaction_count(Some(setup.context), None);
        assert_eq!(result.unwrap(), transactions_processed);
    }

    #[test]
    fn test_get_epoch_info() {
        let info = EpochInfo {
            epoch: 1,
            slot_index: 1,
            slots_in_epoch: 1,
            absolute_slot: 1,
            block_height: 1,
            transaction_count: Some(1),
        };
        let setup = TestSetup::new_with_epoch_info(SurfpoolMinimalRpc, info.clone());
        let result = setup.rpc.get_epoch_info(Some(setup.context), None).unwrap();
        assert_eq!(result, info);
    }

    #[test]
    fn test_get_slot() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);
        let result = setup.rpc.get_slot(Some(setup.context), None).unwrap();
        assert_eq!(result, 92);
    }

    #[test]
    fn test_get_highest_snapshot_slot_returns_error() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);

        let result = setup.rpc.get_highest_snapshot_slot(Some(setup.context));

        assert!(
            result.is_err(),
            "Expected get_highest_snapshot_slot to return an error"
        );

        if let Err(error) = result {
            assert_eq!(
                error.code,
                jsonrpc_core::ErrorCode::ServerError(-32008),
                "Expected error code -32008, got {:?}",
                error.code
            );
            assert_eq!(
                error.message, "No snapshot",
                "Expected error message 'No snapshot', got '{}'",
                error.message
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_genesis_hash() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);

        let genesis_hash = setup
            .rpc
            .get_genesis_hash(Some(setup.context))
            .await
            .unwrap();

        assert_eq!(genesis_hash, GenesisConfig::default().hash().to_string())
    }

    #[test]
    fn test_get_identity() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);
        let result = setup.rpc.get_identity(Some(setup.context)).unwrap();
        assert_eq!(result.identity, SURFPOOL_IDENTITY_PUBKEY.to_string());
    }

    #[test]
    fn test_get_leader_schedule_valid_cases() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);

        setup.context.svm_locker.with_svm_writer(|svm_writer| {
            svm_writer.latest_epoch_info = EpochInfo {
                epoch: 100,
                slot_index: 50,
                slots_in_epoch: 432000,
                absolute_slot: 43200050,
                block_height: 43200050,
                transaction_count: None,
            };
        });

        // test 1: Valid slot within current epoch, no identity
        let result = setup
            .rpc
            .get_leader_schedule(
                Some(setup.context.clone()),
                Some(RpcLeaderScheduleConfigWrapper::SlotOnly(Some(43200025))), // Within epoch
                None,
            )
            .unwrap();

        assert!(result.is_some());
        assert!(result.unwrap().is_empty()); // Should return empty HashMap

        // test 2: No slot provided (should use current slot), no identity
        let result = setup
            .rpc
            .get_leader_schedule(Some(setup.context.clone()), None, None)
            .unwrap();

        assert!(result.is_some());
        assert!(result.unwrap().is_empty());

        // test 3: Valid slot with valid identity
        let valid_pubkey = Pubkey::new_unique();
        let result = setup
            .rpc
            .get_leader_schedule(
                Some(setup.context.clone()),
                None,
                Some(RpcLeaderScheduleConfig {
                    identity: Some(valid_pubkey.to_string()),
                    commitment: None,
                }),
            )
            .unwrap();

        assert!(result.is_some());
        assert!(result.unwrap().is_empty());

        // test 4: Boundary cases - first slot in epoch
        let first_slot_in_epoch = 43200050 - 50;
        let result = setup
            .rpc
            .get_leader_schedule(
                Some(setup.context.clone()),
                Some(RpcLeaderScheduleConfigWrapper::SlotOnly(Some(
                    first_slot_in_epoch,
                ))),
                None,
            )
            .unwrap();

        assert!(result.is_some());
        assert!(result.unwrap().is_empty());

        // test 5: Boundary cases - last slot in epoch
        let last_slot_in_epoch = first_slot_in_epoch + 432000 - 1;
        let result = setup
            .rpc
            .get_leader_schedule(
                Some(setup.context.clone()),
                Some(RpcLeaderScheduleConfigWrapper::SlotOnly(Some(
                    last_slot_in_epoch,
                ))),
                None,
            )
            .unwrap();

        assert!(result.is_some());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_get_leader_schedule_invalid_cases() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);

        setup.context.svm_locker.with_svm_writer(|svm_writer| {
            svm_writer.latest_epoch_info = EpochInfo {
                epoch: 100,
                slot_index: 50,
                slots_in_epoch: 432000,
                absolute_slot: 43200050,
                block_height: 43200050,
                transaction_count: None,
            };
        });

        let first_slot_in_epoch = 43200050 - 50;
        let last_slot_in_epoch = first_slot_in_epoch + 432000 - 1;

        // test 1: Slot before current epoch (should return None)
        let result = setup
            .rpc
            .get_leader_schedule(
                Some(setup.context.clone()),
                Some(RpcLeaderScheduleConfigWrapper::SlotOnly(Some(
                    first_slot_in_epoch - 1,
                ))),
                None,
            )
            .unwrap();

        assert!(result.is_none());

        // test 2: Slot after current epoch (should return None)
        let result = setup
            .rpc
            .get_leader_schedule(
                Some(setup.context.clone()),
                Some(RpcLeaderScheduleConfigWrapper::SlotOnly(Some(
                    last_slot_in_epoch + 1,
                ))),
                None,
            )
            .unwrap();

        assert!(result.is_none());

        // test 3: Way outside epoch range (should return None)
        let result = setup
            .rpc
            .get_leader_schedule(
                Some(setup.context.clone()),
                Some(RpcLeaderScheduleConfigWrapper::SlotOnly(Some(1000000))), // Very old slot
                None,
            )
            .unwrap();

        assert!(result.is_none());

        // test 4: Invalid identity pubkey (should return Error)
        let result = setup.rpc.get_leader_schedule(
            Some(setup.context.clone()),
            None,
            Some(RpcLeaderScheduleConfig {
                identity: Some("invalid_pubkey_string".to_string()),
                commitment: None,
            }),
        );

        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidParams);

        // test 5: Empty identity string (should return Error)
        let result = setup.rpc.get_leader_schedule(
            Some(setup.context.clone()),
            None,
            Some(RpcLeaderScheduleConfig {
                identity: Some("".to_string()),
                commitment: None,
            }),
        );

        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidParams);

        // test 6: No metadata (should return Error from get_svm_locker)
        let result = setup.rpc.get_leader_schedule(None, None, None);

        assert!(result.is_err());
    }

    #[test]
    fn test_get_vote_accounts_valid_config_returns_empty() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);

        // test with valid configuration including all optional parameters
        let config = RpcGetVoteAccountsConfig {
            vote_pubkey: Some("11111111111111111111111111111112".to_string()),
            commitment: Some(CommitmentConfig::processed()),
            keep_unstaked_delinquents: Some(true),
            delinquent_slot_distance: Some(100),
        };

        let result = setup
            .rpc
            .get_vote_accounts(Some(setup.context.clone()), Some(config));

        // should succeed with valid inputs
        assert!(result.is_ok());

        let vote_accounts = result.unwrap();

        // should return empty current and delinquent arrays
        assert_eq!(vote_accounts.current.len(), 0);
        assert_eq!(vote_accounts.delinquent.len(), 0);
    }

    #[test]
    fn test_get_vote_accounts_invalid_pubkey_returns_error() {
        let setup = TestSetup::new(SurfpoolMinimalRpc);

        // test with invalid vote pubkey that's not valid base58
        let config = RpcGetVoteAccountsConfig {
            vote_pubkey: Some("invalid_pubkey_not_base58".to_string()),
            commitment: Some(CommitmentConfig::finalized()),
            keep_unstaked_delinquents: Some(false),
            delinquent_slot_distance: Some(50),
        };

        let result = setup
            .rpc
            .get_vote_accounts(Some(setup.context.clone()), Some(config));

        // should fail due to invalid vote pubkey
        assert!(result.is_err());

        let error = result.unwrap_err();

        // should be invalid params error
        assert_eq!(error.code, jsonrpc_core::ErrorCode::InvalidParams);
    }
}
