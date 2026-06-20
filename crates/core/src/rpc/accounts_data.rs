use jsonrpc_core::{BoxFuture, Result};
use jsonrpc_derive::rpc;
use solana_account_decoder::{
    UiAccount,
    parse_account_data::SplTokenAdditionalDataV2,
    parse_token::{TokenAccountType, UiTokenAmount, parse_token_v3},
};
use solana_client::{
    rpc_config::RpcAccountInfoConfig,
    rpc_response::{RpcBlockCommitment, RpcResponseContext},
};
use solana_clock::Slot;
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client_api::response::Response as RpcResponse;
use solana_runtime::commitment::BlockCommitmentArray;

use super::{RunloopContext, SurfnetRpcContext};
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    rpc::{State, utils::verify_pubkey},
    surfnet::locker::{SvmAccessContext, is_supported_token_program},
    types::{MintAccount, TokenAccount},
};

#[rpc]
pub trait AccountsData {
    type Metadata;

    /// Returns detailed information about an account given its public key.
    ///
    /// This method queries the blockchain for the account associated with the provided
    /// public key string. It can be used to inspect balances, ownership, and program-related metadata.
    ///
    /// ## Parameters
    /// - `pubkey_str`: A base-58 encoded string representing the account's public key.
    /// - `config`: Optional configuration that controls encoding, commitment level,
    ///   data slicing, and other response details.
    ///
    /// ## Returns
    /// A [`RpcResponse`] containing an optional [`UiAccount`] object if the account exists.
    /// If the account does not exist, the response will contain `null`.
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getAccountInfo",
    ///   "params": [
    ///     "9XQeWMPMPXwW1fzLEQeTTrfF5Eb9dj8Qs3tCPoMw3GiE",
    ///     {
    ///       "encoding": "jsonParsed",
    ///       "commitment": "finalized"
    ///     }
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
    ///       "slot": 12345678
    ///     },
    ///     "value": {
    ///       "lamports": 10000000,
    ///       "data": {
    ///         "program": "spl-token",
    ///         "parsed": { ... },
    ///         "space": 165
    ///       },
    ///       "owner": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    ///       "executable": false,
    ///       "rentEpoch": 203,
    ///       "space": 165
    ///     }
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// ## Errors
    /// - Returns an error if the public key is malformed or invalid
    /// - Returns an internal error if the ledger cannot be accessed
    ///
    /// ## See also
    /// - [`UiAccount`]: A readable structure representing on-chain accounts
    #[rpc(meta, name = "getAccountInfo")]
    fn get_account_info(
        &self,
        meta: Self::Metadata,
        pubkey_str: String,
        config: Option<RpcAccountInfoConfig>,
    ) -> BoxFuture<Result<RpcResponse<Option<UiAccount>>>>;

    /// Returns commitment levels for a given block (slot).
    ///
    /// This method provides insight into how many validators have voted for a specific block
    /// and with what level of lockout. This can be used to analyze consensus progress and
    /// determine finality confidence.
    ///
    /// ## Parameters
    /// - `block`: The target slot (block) to query.
    ///
    /// ## Returns
    /// A [`RpcBlockCommitment`] containing a [`BlockCommitmentArray`], which is an array of 32
    /// integers representing the number of votes at each lockout level for that block. Each index
    /// corresponds to a lockout level (i.e., confidence in finality).
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getBlockCommitment",
    ///   "params": [150000000]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "commitment": [0, 4, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    ///     "totalStake": 100000000
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// ## Errors
    /// - If the slot is not found in the current bank or has been purged, this call may return an error.
    /// - May fail if the RPC node is lagging behind or doesn't have voting history for the slot.
    ///
    /// ## See also
    /// - [`BlockCommitmentArray`]: An array representing votes by lockout level
    /// - [`RpcBlockCommitment`]: Wrapper struct for the full response
    #[rpc(meta, name = "getBlockCommitment")]
    fn get_block_commitment(
        &self,
        meta: Self::Metadata,
        block: Slot,
    ) -> Result<RpcBlockCommitment<BlockCommitmentArray>>;

    /// Returns account information for multiple public keys in a single call.
    ///
    /// This method allows batching of account lookups for improved performance and fewer
    /// network roundtrips. It returns a list of `UiAccount` values in the same order as
    /// the provided public keys.
    ///
    /// ## Parameters
    /// - `pubkey_strs`: A list of base-58 encoded public key strings representing accounts to query.
    /// - `config`: Optional configuration to control encoding, commitment level, data slicing, etc.
    ///
    /// ## Returns
    /// A [`RpcResponse`] wrapping a vector of optional [`UiAccount`] objects.
    /// Each element in the response corresponds to the public key at the same index in the request.
    /// If an account is not found, the corresponding entry will be `null`.
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getMultipleAccounts",
    ///   "params": [
    ///     [
    ///       "9XQeWMPMPXwW1fzLEQeTTrfF5Eb9dj8Qs3tCPoMw3GiE",
    ///       "3nN8SBQ2HqTDNnaCzryrSv4YHd4d6GpVCEyDhKMPxN4o"
    ///     ],
    ///     {
    ///       "encoding": "jsonParsed",
    ///       "commitment": "confirmed"
    ///     }
    ///   ]
    /// }
    /// ```
    ///
    /// ## Example Response
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "result": {
    ///     "context": { "slot": 12345678 },
    ///     "value": [
    ///       {
    ///         "lamports": 10000000,
    ///         "data": {
    ///           "program": "spl-token",
    ///           "parsed": { ... },
    ///           "space": 165
    ///         },
    ///         "owner": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    ///         "executable": false,
    ///         "rentEpoch": 203,
    ///         "space": 165
    ///       },
    ///       null
    ///     ]
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// ## Errors
    /// - If any public key is malformed or invalid, the entire call may fail.
    /// - Returns an internal error if the ledger cannot be accessed or some accounts are purged.
    ///
    /// ## See also
    /// - [`UiAccount`]: Human-readable representation of an account
    /// - [`get_account_info`]: Use when querying a single account
    #[rpc(meta, name = "getMultipleAccounts")]
    fn get_multiple_accounts(
        &self,
        meta: Self::Metadata,
        pubkey_strs: Vec<String>,
        config: Option<RpcAccountInfoConfig>,
    ) -> BoxFuture<Result<RpcResponse<Vec<Option<UiAccount>>>>>;

    /// Returns the balance of a token account, given its public key.
    ///
    /// This method fetches the token balance of an account, including its amount and
    /// user-friendly information (like the UI amount in human-readable format). It is useful
    /// for token-related applications, such as checking balances in wallets or exchanges.
    ///
    /// ## Parameters
    /// - `pubkey_str`: The base-58 encoded string of the public key of the token account.
    /// - `commitment`: Optional commitment configuration to specify the desired confirmation level of the query.
    ///
    /// ## Returns
    /// A [`RpcResponse`] containing the token balance in a [`UiTokenAmount`] struct.
    /// If the account doesn't hold any tokens or is invalid, the response will contain `null`.
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getTokenAccountBalance",
    ///   "params": [
    ///     "3nN8SBQ2HqTDNnaCzryrSv4YHd4d6GpVCEyDhKMPxN4o",
    ///     {
    ///       "commitment": "confirmed"
    ///     }
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
    ///       "slot": 12345678
    ///     },
    ///     "value": {
    ///       "uiAmount": 100.0,
    ///       "decimals": 6,
    ///       "amount": "100000000",
    ///       "uiAmountString": "100.000000"
    ///     }
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// ## Errors
    /// - If the provided public key is invalid or does not exist.
    /// - If the account is not a valid token account or does not hold any tokens.
    ///
    /// ## See also
    /// - [`UiTokenAmount`]: Represents the token balance in user-friendly format.
    #[rpc(meta, name = "getTokenAccountBalance")]
    fn get_token_account_balance(
        &self,
        meta: Self::Metadata,
        pubkey_str: String,
        commitment: Option<CommitmentConfig>,
    ) -> BoxFuture<Result<RpcResponse<Option<UiTokenAmount>>>>;

    /// Returns the total supply of a token, given its mint address.
    ///
    /// This method provides the total circulating supply of a specific token, including the raw
    /// amount and human-readable UI-formatted values. It can be useful for tracking token issuance
    /// and verifying the supply of a token on-chain.
    ///
    /// ## Parameters
    /// - `mint_str`: The base-58 encoded string of the mint address for the token.
    /// - `commitment`: Optional commitment configuration to specify the desired confirmation level of the query.
    ///
    /// ## Returns
    /// A [`RpcResponse`] containing the total token supply in a [`UiTokenAmount`] struct.
    /// If the token does not exist or is invalid, the response will return an error.
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getTokenSupply",
    ///   "params": [
    ///     "So11111111111111111111111111111111111111112",
    ///     {
    ///       "commitment": "confirmed"
    ///     }
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
    ///       "slot": 12345678
    ///     },
    ///     "value": {
    ///       "uiAmount": 1000000000.0,
    ///       "decimals": 6,
    ///       "amount": "1000000000000000",
    ///       "uiAmountString": "1000000000.000000"
    ///     }
    ///   },
    ///   "id": 1
    /// }
    /// ```
    ///
    /// ## Errors
    /// - If the mint address is invalid or does not correspond to a token.
    /// - If the token supply cannot be fetched due to network issues or node synchronization problems.
    ///
    /// ## See also
    /// - [`UiTokenAmount`]: Represents the token balance or supply in a user-friendly format.
    #[rpc(meta, name = "getTokenSupply")]
    fn get_token_supply(
        &self,
        meta: Self::Metadata,
        mint_str: String,
        commitment: Option<CommitmentConfig>,
    ) -> BoxFuture<Result<RpcResponse<UiTokenAmount>>>;
}

#[derive(Clone)]
pub struct SurfpoolAccountsDataRpc;
impl AccountsData for SurfpoolAccountsDataRpc {
    type Metadata = Option<RunloopContext>;

    fn get_account_info(
        &self,
        meta: Self::Metadata,
        pubkey_str: String,
        config: Option<RpcAccountInfoConfig>,
    ) -> BoxFuture<Result<RpcResponse<Option<UiAccount>>>> {
        let config = config.unwrap_or_default();
        let pubkey = match verify_pubkey(&pubkey_str) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };
        #[cfg(feature = "prometheus")]
        let rpc_start = std::time::Instant::now();

        let SurfnetRpcContext {
            svm_locker,
            remote_ctx,
        } = match meta.get_rpc_context(config.commitment.unwrap_or_default()) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        Box::pin(async move {
            let SvmAccessContext {
                slot,
                inner: account_update,
                ..
            } = svm_locker.get_account(&remote_ctx, &pubkey, None).await?;

            #[cfg(feature = "prometheus")]
            if let Some(m) = crate::telemetry::metrics() {
                m.record_rpc_request("getAccountInfo", rpc_start.elapsed().as_millis() as u64);
            }
            svm_locker.write_account_update(account_update.clone());

            let ui_account = if let Some(((pubkey, account), token_data)) =
                account_update.map_account_with_token_data()
            {
                Some(
                    svm_locker
                        .account_to_rpc_keyed_account(
                            &pubkey,
                            &account,
                            &config,
                            token_data.map(|(mint, _)| mint),
                        )
                        .account,
                )
            } else {
                None
            };

            Ok(RpcResponse {
                context: RpcResponseContext::new(slot),
                value: ui_account,
            })
        })
    }

    fn get_multiple_accounts(
        &self,
        meta: Self::Metadata,
        pubkeys_str: Vec<String>,
        config: Option<RpcAccountInfoConfig>,
    ) -> BoxFuture<Result<RpcResponse<Vec<Option<UiAccount>>>>> {
        let config = config.unwrap_or_default();
        let pubkeys = match pubkeys_str
            .iter()
            .map(|s| verify_pubkey(s))
            .collect::<SurfpoolResult<Vec<_>>>()
        {
            Ok(p) => p,
            Err(e) => return e.into(),
        };

        let SurfnetRpcContext {
            svm_locker,
            remote_ctx,
        } = match meta.get_rpc_context(config.commitment.unwrap_or_default()) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        #[cfg(feature = "prometheus")]
        let rpc_start = std::time::Instant::now();

        Box::pin(async move {
            let SvmAccessContext {
                slot,
                inner: account_updates,
                ..
            } = svm_locker
                .get_multiple_accounts(&remote_ctx, &pubkeys, None)
                .await?;

            #[cfg(feature = "prometheus")]
            if let Some(m) = crate::telemetry::metrics() {
                m.record_rpc_request(
                    "getMultipleAccounts",
                    rpc_start.elapsed().as_millis() as u64,
                );
            }

            svm_locker.write_multiple_account_updates(&account_updates);

            // Convert account updates to UI accounts, order is already preserved by get_multiple_accounts
            let mut ui_accounts = vec![];
            for account_update in account_updates.into_iter() {
                if let Some(((pubkey, account), token_data)) =
                    account_update.map_account_with_token_data()
                {
                    ui_accounts.push(Some(
                        svm_locker
                            .account_to_rpc_keyed_account(
                                &pubkey,
                                &account,
                                &config,
                                token_data.map(|(mint, _)| mint),
                            )
                            .account,
                    ));
                } else {
                    ui_accounts.push(None);
                }
            }

            Ok(RpcResponse {
                context: RpcResponseContext::new(slot),
                value: ui_accounts,
            })
        })
    }

    fn get_block_commitment(
        &self,
        meta: Self::Metadata,
        block: Slot,
    ) -> Result<RpcBlockCommitment<BlockCommitmentArray>> {
        // get the info we need and free up lock before validation
        let (current_slot, block_exists) = meta
            .with_svm_reader(|svm_reader| {
                svm_reader
                    .blocks
                    .contains_key(&block)
                    .map_err(SurfpoolError::from)
                    .map(|exists| (svm_reader.get_latest_absolute_slot(), exists))
            })
            .map_err(Into::<jsonrpc_core::Error>::into)??;

        // block is valid if it exists in our block history or it's not too far in the future
        if !block_exists && block > current_slot {
            return Err(jsonrpc_core::Error::invalid_params(format!(
                "Block {} not found",
                block
            )));
        }

        let commitment_array = [0u64; 32];

        Ok(RpcBlockCommitment {
            commitment: Some(commitment_array),
            total_stake: 0,
        })
    }

    // SPL Token-specific RPC endpoints
    // See https://github.com/solana-labs/solana-program-library/releases/tag/token-v2.0.0 for
    // program details

    fn get_token_account_balance(
        &self,
        meta: Self::Metadata,
        pubkey_str: String,
        commitment: Option<CommitmentConfig>,
    ) -> BoxFuture<Result<RpcResponse<Option<UiTokenAmount>>>> {
        let pubkey = match verify_pubkey(&pubkey_str) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        let SurfnetRpcContext {
            svm_locker,
            remote_ctx,
        } = match meta.get_rpc_context(commitment.unwrap_or_default()) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        Box::pin(async move {
            let token_account_result = svm_locker
                .get_account(&remote_ctx, &pubkey, None)
                .await?
                .inner;

            svm_locker.write_account_update(token_account_result.clone());

            let token_account = token_account_result.map_account()?;

            let (mint_pubkey, _amount) = if is_supported_token_program(&token_account.owner) {
                let unpacked_token_account = TokenAccount::unpack(&token_account.data)?;
                (
                    unpacked_token_account.mint(),
                    unpacked_token_account.amount(),
                )
            } else {
                return Err(SurfpoolError::invalid_account_data(
                    pubkey,
                    "Account is not owned by Token or Token-2022 program",
                    None::<String>,
                )
                .into());
            };

            let SvmAccessContext {
                slot,
                inner: mint_account_result,
                ..
            } = svm_locker
                .get_account(&remote_ctx, &mint_pubkey, None)
                .await?;

            svm_locker.write_account_update(mint_account_result.clone());

            let mint_account = mint_account_result.map_account()?;

            let token_decimals = if is_supported_token_program(&mint_account.owner) {
                let unpacked_mint_account = MintAccount::unpack(&mint_account.data)?;
                unpacked_mint_account.decimals()
            } else {
                return Err(SurfpoolError::invalid_account_data(
                    mint_pubkey,
                    "Mint account is not owned by Token or Token-2022 program",
                    None::<String>,
                )
                .into());
            };

            Ok(RpcResponse {
                context: RpcResponseContext::new(slot),
                value: {
                    parse_token_v3(
                        &token_account.data,
                        Some(&SplTokenAdditionalDataV2 {
                            decimals: token_decimals,
                            ..Default::default()
                        }),
                    )
                    .ok()
                    .and_then(|t| match t {
                        TokenAccountType::Account(account) => Some(account.token_amount),
                        _ => None,
                    })
                },
            })
        })
    }

    fn get_token_supply(
        &self,
        meta: Self::Metadata,
        mint_str: String,
        commitment: Option<CommitmentConfig>,
    ) -> BoxFuture<Result<RpcResponse<UiTokenAmount>>> {
        let mint_pubkey = match verify_pubkey(&mint_str) {
            Ok(pubkey) => pubkey,
            Err(e) => return e.into(),
        };

        let SurfnetRpcContext {
            svm_locker,
            remote_ctx,
        } = match meta.get_rpc_context(commitment.unwrap_or_default()) {
            Ok(res) => res,
            Err(e) => return e.into(),
        };

        Box::pin(async move {
            let SvmAccessContext {
                slot,
                inner: mint_account_result,
                ..
            } = svm_locker
                .get_account(&remote_ctx, &mint_pubkey, None)
                .await?;

            svm_locker.write_account_update(mint_account_result.clone());

            let mint_account = mint_account_result.map_account()?;

            if !is_supported_token_program(&mint_account.owner) {
                return Err(SurfpoolError::invalid_account_data(
                    mint_pubkey,
                    "Account is not a token mint account",
                    None::<String>,
                )
                .into());
            }

            let mint_data = MintAccount::unpack(&mint_account.data)?;

            Ok(RpcResponse {
                context: RpcResponseContext::new(slot),
                value: {
                    parse_token_v3(
                        &mint_account.data,
                        Some(&SplTokenAdditionalDataV2 {
                            decimals: mint_data.decimals(),
                            ..Default::default()
                        }),
                    )
                    .ok()
                    .and_then(|t| match t {
                        TokenAccountType::Mint(mint) => {
                            let supply_u64 = mint.supply.parse::<u64>().unwrap_or(0);
                            let ui_amount = if supply_u64 == 0 {
                                Some(0.0)
                            } else {
                                let divisor = 10_u64.pow(mint.decimals as u32);
                                Some(supply_u64 as f64 / divisor as f64)
                            };

                            Some(UiTokenAmount {
                                amount: mint.supply.clone(),
                                decimals: mint.decimals,
                                ui_amount,
                                ui_amount_string: mint.supply,
                            })
                        }
                        _ => None,
                    })
                    .ok_or_else(|| {
                        SurfpoolError::invalid_account_data(
                            mint_pubkey,
                            "Failed to parse token mint account",
                            None::<String>,
                        )
                    })?
                },
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use solana_account::Account;
    use solana_keypair::Keypair;
    use solana_program_option::COption;
    use solana_program_pack::Pack;
    use solana_pubkey::Pubkey;
    use solana_signer::Signer;
    use solana_system_interface::instruction::create_account;
    use solana_transaction::Transaction;
    use spl_associated_token_account_interface::{
        address::get_associated_token_address_with_program_id,
        instruction::create_associated_token_account,
    };
    use spl_token_2022_interface::instruction::{initialize_mint2, mint_to, transfer_checked};
    use spl_token_interface::state::{Account as TokenAccount, AccountState, Mint};

    use super::*;
    use crate::{
        surfnet::{GetAccountResult, remote::SurfnetRemoteClient},
        tests::helpers::TestSetup,
        types::SyntheticBlockhash,
    };

    #[ignore = "connection-required"]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_token_account_balance() {
        let setup = TestSetup::new(SurfpoolAccountsDataRpc);

        let mint_pk = Pubkey::new_unique();

        let minimum_rent = setup.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .minimum_balance_for_rent_exemption(Mint::LEN)
        });

        let mut data = [0; Mint::LEN];

        let default = Mint {
            decimals: 6,
            supply: 1000000000000000,
            is_initialized: true,
            ..Default::default()
        };
        default.pack_into_slice(&mut data);

        let mint_account = Account {
            lamports: minimum_rent,
            owner: spl_token_interface::ID,
            executable: false,
            rent_epoch: 0,
            data: data.to_vec(),
        };

        setup
            .context
            .svm_locker
            .write_account_update(GetAccountResult::FoundAccount(mint_pk, mint_account, true));

        let token_account_pk = Pubkey::new_unique();

        let minimum_rent = setup.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .minimum_balance_for_rent_exemption(TokenAccount::LEN)
        });

        let mut data = [0; TokenAccount::LEN];

        let default = TokenAccount {
            mint: mint_pk,
            owner: spl_token_interface::ID,
            state: AccountState::Initialized,
            amount: 100 * 1000000,
            ..Default::default()
        };
        default.pack_into_slice(&mut data);

        let token_account = Account {
            lamports: minimum_rent,
            owner: spl_token_interface::ID,
            executable: false,
            rent_epoch: 0,
            data: data.to_vec(),
        };

        setup
            .context
            .svm_locker
            .write_account_update(GetAccountResult::FoundAccount(
                token_account_pk,
                token_account,
                true,
            ));

        let res = setup
            .rpc
            .get_token_account_balance(Some(setup.context), token_account_pk.to_string(), None)
            .await
            .unwrap();

        assert_eq!(
            res.value.unwrap(),
            UiTokenAmount {
                amount: String::from("100000000"),
                decimals: 6,
                ui_amount: Some(100.0),
                ui_amount_string: String::from("100")
            }
        );
    }

    #[test]
    fn test_get_block_commitment_past_slot() {
        let setup = TestSetup::new(SurfpoolAccountsDataRpc);
        let current_slot = setup.context.svm_locker.get_latest_absolute_slot();
        let past_slot = if current_slot > 10 {
            current_slot - 10
        } else {
            0
        };

        let result = setup
            .rpc
            .get_block_commitment(Some(setup.context), past_slot)
            .unwrap();

        // Should return commitment data for past slot
        assert!(result.commitment.is_some());
        assert_eq!(result.total_stake, 0);
    }

    #[test]
    fn test_get_block_commitment_with_actual_block() {
        let setup = TestSetup::new(SurfpoolAccountsDataRpc);

        // create a block in the SVM's block history
        let test_slot = 12345;
        setup.context.svm_locker.with_svm_writer(|svm_writer| {
            use crate::surfnet::BlockHeader;

            svm_writer
                .blocks
                .store(
                    test_slot,
                    BlockHeader {
                        hash: SyntheticBlockhash::new(test_slot).to_string(),
                        previous_blockhash: SyntheticBlockhash::new(test_slot - 1).to_string(),
                        parent_slot: test_slot - 1,
                        block_time: chrono::Utc::now().timestamp_millis(),
                        block_height: test_slot,
                        signatures: vec![],
                    },
                )
                .unwrap();
        });

        let result = setup
            .rpc
            .get_block_commitment(Some(setup.context), test_slot)
            .unwrap();

        // should return commitment data for the existing block
        assert!(result.commitment.is_some());
        assert_eq!(result.total_stake, 0);
    }

    #[test]
    fn test_get_block_commitment_no_metadata() {
        let setup = TestSetup::new(SurfpoolAccountsDataRpc);

        let result = setup.rpc.get_block_commitment(None, 123);

        assert!(result.is_err());
        // This should fail because meta is None, triggering the SurfpoolError::missing_context() path
    }

    #[test]
    fn test_get_block_commitment_future_slot_error() {
        let setup = TestSetup::new(SurfpoolAccountsDataRpc);
        let current_slot = setup.context.svm_locker.get_latest_absolute_slot();
        let future_slot = current_slot + 1000;

        let result = setup
            .rpc
            .get_block_commitment(Some(setup.context), future_slot);

        // Should return an error for future slots
        assert!(result.is_err());

        let error = result.unwrap_err();
        assert_eq!(error.code, jsonrpc_core::ErrorCode::InvalidParams);
        assert!(error.message.contains("Block") && error.message.contains("not found"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_token_supply_with_real_mint() {
        let setup = TestSetup::new(SurfpoolAccountsDataRpc);

        let mint_pubkey = Pubkey::new_unique();

        // Create mint account data
        let mut mint_data = [0u8; Mint::LEN];
        let mint = Mint {
            mint_authority: COption::Some(Pubkey::new_unique()),
            supply: 1_000_000_000_000,
            decimals: 6,
            is_initialized: true,
            freeze_authority: COption::None,
        };
        Mint::pack(mint, &mut mint_data).unwrap();

        let mint_account = Account {
            lamports: setup.context.svm_locker.with_svm_reader(|svm_reader| {
                svm_reader
                    .inner
                    .minimum_balance_for_rent_exemption(Mint::LEN)
            }),
            data: mint_data.to_vec(),
            owner: spl_token_interface::id(),
            executable: false,
            rent_epoch: 0,
        };

        setup.context.svm_locker.with_svm_writer(|svm_writer| {
            svm_writer
                .set_account(&mint_pubkey, mint_account.clone())
                .unwrap();
        });

        let res = setup
            .rpc
            .get_token_supply(
                Some(setup.context),
                mint_pubkey.to_string(),
                Some(CommitmentConfig::confirmed()),
            )
            .await
            .unwrap();

        assert_eq!(res.value.amount, "1000000000000");
        assert_eq!(res.value.decimals, 6);
        assert_eq!(res.value.ui_amount_string, "1000000000000");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_invalid_pubkey_format() {
        let setup = TestSetup::new(SurfpoolAccountsDataRpc);

        // test various invalid pubkey formats
        let invalid_pubkeys = vec![
            "",
            "invalid",
            "123",
            "not-a-valid-base58-string!@#$",
            "11111111111111111111111111111111111111111111111111111111111111111",
            "invalid-base58-characters-ö",
        ];

        for invalid_pubkey in invalid_pubkeys {
            let res = setup
                .rpc
                .get_token_supply(
                    Some(setup.context.clone()),
                    invalid_pubkey.to_string(),
                    Some(CommitmentConfig::confirmed()),
                )
                .await;

            assert!(
                res.is_err(),
                "Should fail for invalid pubkey: '{}'",
                invalid_pubkey
            );

            let error_msg = res.unwrap_err().to_string();
            assert!(
                error_msg.contains("Invalid") || error_msg.contains("invalid"),
                "Error should mention invalidity for '{}': {}",
                invalid_pubkey,
                error_msg
            );
        }

        println!("✅ All invalid pubkey formats correctly rejected");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_nonexistent_account() {
        let setup = TestSetup::new(SurfpoolAccountsDataRpc);

        // valid pubkey format but nonexistent account
        let nonexistent_mint = Pubkey::new_unique();

        let res = setup
            .rpc
            .get_token_supply(
                Some(setup.context),
                nonexistent_mint.to_string(),
                Some(CommitmentConfig::confirmed()),
            )
            .await;

        assert!(res.is_err(), "Should fail for nonexistent account");

        let error_msg = res.unwrap_err().to_string();
        assert!(
            error_msg.contains("not found") || error_msg.contains("account"),
            "Error should mention account not found: {}",
            error_msg
        );

        println!("✅ Nonexistent account correctly rejected: {}", error_msg);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_invalid_mint_data() {
        let setup = TestSetup::new(SurfpoolAccountsDataRpc);

        let fake_mint = Pubkey::new_unique();

        setup.context.svm_locker.with_svm_writer(|svm_writer| {
            // create an account owned by SPL Token but with invalid data
            let invalid_mint_account = Account {
                lamports: 1000000,
                data: vec![0xFF; 50], // invalid mint data (random bytes)
                owner: spl_token_interface::id(),
                executable: false,
                rent_epoch: 0,
            };

            svm_writer
                .set_account(&fake_mint, invalid_mint_account)
                .unwrap();
        });

        let res = setup
            .rpc
            .get_token_supply(
                Some(setup.context),
                fake_mint.to_string(),
                Some(CommitmentConfig::confirmed()),
            )
            .await;

        assert!(
            res.is_err(),
            "Should fail for account with invalid mint data"
        );

        let error_msg = res.unwrap_err().to_string();
        assert!(
            error_msg.eq("Parse error: Failed to unpack mint account"),
            "Incorrect error received: {}",
            error_msg
        );

        println!("✅ Invalid mint data correctly rejected: {}", error_msg);
    }

    #[ignore = "requires-network"]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_remote_rpc_failure() {
        // test with invalid remote RPC URL
        let bad_remote_client =
            SurfnetRemoteClient::new("https://invalid-url-that-doesnt-exist.com");
        let mut setup = TestSetup::new(SurfpoolAccountsDataRpc);
        setup.context.remote_rpc_client = Some(bad_remote_client);

        let nonexistent_mint = Pubkey::new_unique();

        let res = setup
            .rpc
            .get_token_supply(
                Some(setup.context),
                nonexistent_mint.to_string(),
                Some(CommitmentConfig::confirmed()),
            )
            .await;

        assert!(res.is_err(), "Should fail when remote RPC is unreachable");

        let error_msg = res.unwrap_err().to_string();
        println!("✅ Remote RPC failure handled: {}", error_msg);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_transfer_token() {
        // Create connection to local validator
        let client = TestSetup::new(SurfpoolAccountsDataRpc);
        let recent_blockhash = client
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        // Generate a new keypair for the fee payer
        let fee_payer = Keypair::new();

        // Generate a second keypair for the token recipient
        let recipient = Keypair::new();

        // Airdrop 1 SOL to fee payer
        client
            .context
            .svm_locker
            .airdrop(&fee_payer.pubkey(), 1_000_000_000)
            .unwrap()
            .unwrap();

        // Airdrop 1 SOL to recipient for rent exemption
        client
            .context
            .svm_locker
            .airdrop(&recipient.pubkey(), 1_000_000_000)
            .unwrap()
            .unwrap();

        // Generate keypair to use as address of mint
        let mint = Keypair::new();

        // Get default mint account size (in bytes), no extensions enabled
        let mint_space = Mint::LEN;
        let mint_rent = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .minimum_balance_for_rent_exemption(mint_space)
        });

        // Instruction to create new account for mint (token 2022 program)
        let create_account_instruction = create_account(
            &fee_payer.pubkey(),             // payer
            &mint.pubkey(),                  // new account (mint)
            mint_rent,                       // lamports
            mint_space as u64,               // space
            &spl_token_2022_interface::id(), // program id
        );

        // Instruction to initialize mint account data
        let initialize_mint_instruction = initialize_mint2(
            &spl_token_2022_interface::id(),
            &mint.pubkey(),            // mint
            &fee_payer.pubkey(),       // mint authority
            Some(&fee_payer.pubkey()), // freeze authority
            2,                         // decimals
        )
        .unwrap();

        // Calculate the associated token account address for fee_payer
        let source_token_address = get_associated_token_address_with_program_id(
            &fee_payer.pubkey(),             // owner
            &mint.pubkey(),                  // mint
            &spl_token_2022_interface::id(), // program_id
        );

        // Instruction to create associated token account for fee_payer
        let create_source_ata_instruction = create_associated_token_account(
            &fee_payer.pubkey(),             // funding address
            &fee_payer.pubkey(),             // wallet address
            &mint.pubkey(),                  // mint address
            &spl_token_2022_interface::id(), // program id
        );

        // Calculate the associated token account address for recipient
        let destination_token_address = get_associated_token_address_with_program_id(
            &recipient.pubkey(),             // owner
            &mint.pubkey(),                  // mint
            &spl_token_2022_interface::id(), // program_id
        );

        // Instruction to create associated token account for recipient
        let create_destination_ata_instruction = create_associated_token_account(
            &fee_payer.pubkey(),             // funding address
            &recipient.pubkey(),             // wallet address
            &mint.pubkey(),                  // mint address
            &spl_token_2022_interface::id(), // program id
        );

        // Amount of tokens to mint (100 tokens with 2 decimal places)
        let amount = 100_00;

        // Create mint_to instruction to mint tokens to the source token account
        let mint_to_instruction = mint_to(
            &spl_token_2022_interface::id(),
            &mint.pubkey(),         // mint
            &source_token_address,  // destination
            &fee_payer.pubkey(),    // authority
            &[&fee_payer.pubkey()], // signer
            amount,                 // amount
        )
        .unwrap();

        // Create transaction and add instructions
        let transaction = Transaction::new_signed_with_payer(
            &[
                create_account_instruction,
                initialize_mint_instruction,
                create_source_ata_instruction,
                create_destination_ata_instruction,
                mint_to_instruction,
            ],
            Some(&fee_payer.pubkey()),
            &[&fee_payer, &mint],
            recent_blockhash,
        );

        let (status_tx, _status_rx) = crossbeam_channel::unbounded();
        client
            .context
            .svm_locker
            .process_transaction(&None, transaction.into(), status_tx.clone(), false, true)
            .await
            .unwrap();

        println!("Mint Address: {}", mint.pubkey());
        println!("Recipient Address: {}", recipient.pubkey());
        println!("Source Token Account Address: {}", source_token_address);
        println!(
            "Destination Token Account Address: {}",
            destination_token_address
        );
        println!("Minted {} tokens to the source token account", amount);

        // Get the latest blockhash for the transfer transaction
        let recent_blockhash = client
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        // Amount of tokens to transfer (0.50 tokens with 2 decimals)
        let transfer_amount = 50;

        // Create transfer_checked instruction to send tokens from source to destination
        let transfer_instruction = transfer_checked(
            &spl_token_2022_interface::id(), // program id
            &source_token_address,           // source
            &mint.pubkey(),                  // mint
            &destination_token_address,      // destination
            &fee_payer.pubkey(),             // owner of source
            &[&fee_payer.pubkey()],          // signers
            transfer_amount,                 // amount
            2,                               // decimals
        )
        .unwrap();

        // Create transaction for transferring tokens
        let transaction = Transaction::new_signed_with_payer(
            &[transfer_instruction],
            Some(&fee_payer.pubkey()),
            &[&fee_payer],
            recent_blockhash,
        );

        client
            .context
            .svm_locker
            .process_transaction(&None, transaction.clone().into(), status_tx, true, true)
            .await
            .unwrap();

        println!("Successfully transferred 0.50 tokens from sender to recipient");

        let source_balance = client
            .rpc
            .get_token_account_balance(
                Some(client.context.clone()),
                source_token_address.to_string(),
                Some(CommitmentConfig::confirmed()),
            )
            .await
            .unwrap();

        let destination_balance = client
            .rpc
            .get_token_account_balance(
                Some(client.context.clone()),
                destination_token_address.to_string(),
                Some(CommitmentConfig::confirmed()),
            )
            .await
            .unwrap();

        println!(
            "Source Token Account Balance: {} tokens ({})",
            source_balance.value.as_ref().unwrap().ui_amount.unwrap(),
            source_balance.value.as_ref().unwrap().amount
        );
        println!(
            "Destination Token Account Balance: {} tokens ({})",
            destination_balance
                .value
                .as_ref()
                .unwrap()
                .ui_amount
                .unwrap(),
            destination_balance.value.as_ref().unwrap().amount
        );

        assert_eq!(source_balance.value.unwrap().amount, "9950");
        assert_eq!(destination_balance.value.unwrap().amount, "50");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_account_info() {
        // Create connection to local validator
        let client = TestSetup::new(SurfpoolAccountsDataRpc);
        let recent_blockhash = client
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        // Generate a new keypair for the fee payer
        let fee_payer = Keypair::new();

        // Generate a second keypair for the token recipient
        let recipient = Keypair::new();

        // Airdrop 1 SOL to fee payer
        client
            .context
            .svm_locker
            .airdrop(&fee_payer.pubkey(), 1_000_000_000)
            .unwrap()
            .unwrap();

        // Airdrop 1 SOL to recipient for rent exemption
        client
            .context
            .svm_locker
            .airdrop(&recipient.pubkey(), 1_000_000_000)
            .unwrap()
            .unwrap();

        // Generate keypair to use as address of mint
        let mint = Keypair::new();

        // Get default mint account size (in bytes), no extensions enabled
        let mint_space = Mint::LEN;
        let mint_rent = client.context.svm_locker.with_svm_reader(|svm_reader| {
            svm_reader
                .inner
                .minimum_balance_for_rent_exemption(mint_space)
        });

        // Instruction to create new account for mint (token 2022 program)
        let create_account_instruction = create_account(
            &fee_payer.pubkey(),             // payer
            &mint.pubkey(),                  // new account (mint)
            mint_rent,                       // lamports
            mint_space as u64,               // space
            &spl_token_2022_interface::id(), // program id
        );

        // Instruction to initialize mint account data
        let initialize_mint_instruction = initialize_mint2(
            &spl_token_2022_interface::id(),
            &mint.pubkey(),            // mint
            &fee_payer.pubkey(),       // mint authority
            Some(&fee_payer.pubkey()), // freeze authority
            2,                         // decimals
        )
        .unwrap();

        // Calculate the associated token account address for fee_payer
        let source_token_address = get_associated_token_address_with_program_id(
            &fee_payer.pubkey(),             // owner
            &mint.pubkey(),                  // mint
            &spl_token_2022_interface::id(), // program_id
        );

        // Instruction to create associated token account for fee_payer
        let create_source_ata_instruction = create_associated_token_account(
            &fee_payer.pubkey(),             // funding address
            &fee_payer.pubkey(),             // wallet address
            &mint.pubkey(),                  // mint address
            &spl_token_2022_interface::id(), // program id
        );

        // Calculate the associated token account address for recipient
        let destination_token_address = get_associated_token_address_with_program_id(
            &recipient.pubkey(),             // owner
            &mint.pubkey(),                  // mint
            &spl_token_2022_interface::id(), // program_id
        );

        // Instruction to create associated token account for recipient
        let create_destination_ata_instruction = create_associated_token_account(
            &fee_payer.pubkey(),             // funding address
            &recipient.pubkey(),             // wallet address
            &mint.pubkey(),                  // mint address
            &spl_token_2022_interface::id(), // program id
        );

        // Amount of tokens to mint (100 tokens with 2 decimal places)
        let amount = 100_00;

        // Create mint_to instruction to mint tokens to the source token account
        let mint_to_instruction = mint_to(
            &spl_token_2022_interface::id(),
            &mint.pubkey(),         // mint
            &source_token_address,  // destination
            &fee_payer.pubkey(),    // authority
            &[&fee_payer.pubkey()], // signer
            amount,                 // amount
        )
        .unwrap();

        // Create transaction and add instructions
        let transaction = Transaction::new_signed_with_payer(
            &[
                create_account_instruction,
                initialize_mint_instruction,
                create_source_ata_instruction,
                create_destination_ata_instruction,
                mint_to_instruction,
            ],
            Some(&fee_payer.pubkey()),
            &[&fee_payer, &mint],
            recent_blockhash,
        );

        let (status_tx, _status_rx) = crossbeam_channel::unbounded();
        // Send and confirm transaction
        client
            .context
            .svm_locker
            .process_transaction(&None, transaction.clone().into(), status_tx, true, true)
            .await
            .unwrap();

        println!("Mint Address: {}", mint.pubkey());
        println!("Recipient Address: {}", recipient.pubkey());
        println!("Source Token Account Address: {}", source_token_address);
        println!(
            "Destination Token Account Address: {}",
            destination_token_address
        );
        println!("Minted {} tokens to the source token account", amount);

        // Get the latest blockhash for the transfer transaction
        let recent_blockhash = client
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        // Amount of tokens to transfer (0.50 tokens with 2 decimals)
        let transfer_amount = 50;

        // Create transfer_checked instruction to send tokens from source to destination
        let transfer_instruction = transfer_checked(
            &spl_token_2022_interface::id(), // program id
            &source_token_address,           // source
            &mint.pubkey(),                  // mint
            &destination_token_address,      // destination
            &fee_payer.pubkey(),             // owner of source
            &[&fee_payer.pubkey()],          // signers
            transfer_amount,                 // amount
            2,                               // decimals
        )
        .unwrap();

        // Create transaction for transferring tokens
        let transaction = Transaction::new_signed_with_payer(
            &[transfer_instruction],
            Some(&fee_payer.pubkey()),
            &[&fee_payer],
            recent_blockhash,
        );
        let (status_tx, _status_rx) = crossbeam_channel::unbounded();
        // Send and confirm transaction
        client
            .context
            .svm_locker
            .process_transaction(&None, transaction.clone().into(), status_tx, true, true)
            .await
            .unwrap();

        println!(
            "Successfully transferred 0.50 tokens from {} to {}",
            source_token_address, destination_token_address
        );

        let source_account_info = client
            .rpc
            .get_account_info(
                Some(client.context.clone()),
                source_token_address.to_string(),
                Some(RpcAccountInfoConfig {
                    encoding: Some(solana_account_decoder::UiAccountEncoding::JsonParsed),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();

        let destination_account_info = client
            .rpc
            .get_account_info(
                Some(client.context.clone()),
                destination_token_address.to_string(),
                Some(RpcAccountInfoConfig {
                    encoding: Some(solana_account_decoder::UiAccountEncoding::JsonParsed),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();

        println!("Source Account Info: {:?}", source_account_info);
        println!("Destination Account Info: {:?}", destination_account_info);

        let source_account = source_account_info.value.unwrap();
        if let solana_account_decoder::UiAccountData::Json(parsed) = source_account.data {
            let amount = parsed.parsed["info"]["tokenAmount"]["amount"]
                .as_str()
                .unwrap();
            assert_eq!(amount, "9950");
        } else {
            panic!("source account data was not in json parsed format");
        }

        let destination_account = destination_account_info.value.unwrap();
        if let solana_account_decoder::UiAccountData::Json(parsed) = destination_account.data {
            let amount = parsed.parsed["info"]["tokenAmount"]["amount"]
                .as_str()
                .unwrap();
            assert_eq!(amount, "50");
        } else {
            panic!("destination account data was not in json parsed format");
        }
    }

    #[ignore = "requires-network"]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_multiple_accounts_with_remote_preserves_order() {
        // This test checks that order is preserved when mixing local and remote accounts
        let mut setup = TestSetup::new(SurfpoolAccountsDataRpc);

        // Add a remote client to trigger get_multiple_accounts_with_remote_fallback path
        let remote_client = SurfnetRemoteClient::new("https://api.mainnet-beta.solana.com");
        setup.context.remote_rpc_client = Some(remote_client);

        // Create three accounts with different lamport amounts
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();
        let pk3 = Pubkey::new_unique();

        println!("{}", pk1);
        println!("{}", pk2);
        println!("{}", pk3);

        let account1 = Account {
            lamports: 1_000_000,
            data: vec![],
            owner: solana_pubkey::Pubkey::default(),
            executable: false,
            rent_epoch: 0,
        };

        let account3 = Account {
            lamports: 3_000_000,
            data: vec![],
            owner: solana_pubkey::Pubkey::default(),
            executable: false,
            rent_epoch: 0,
        };

        // Store only account1 and account3 locally (account2 will need remote fetch)
        setup
            .context
            .svm_locker
            .write_account_update(GetAccountResult::FoundAccount(pk1, account1, true));
        setup
            .context
            .svm_locker
            .write_account_update(GetAccountResult::FoundAccount(pk3, account3, true));

        // Request accounts in order: [pk1, pk2, pk3]
        // pk1 and pk3 are local, pk2 is missing (will try remote fetch and fail)
        let pubkeys_str = vec![pk1.to_string(), pk2.to_string(), pk3.to_string()];

        let response = setup
            .rpc
            .get_multiple_accounts(
                Some(setup.context),
                pubkeys_str,
                Some(RpcAccountInfoConfig::default()),
            )
            .await
            .unwrap();

        // Verify we got 3 results
        assert_eq!(response.value.len(), 3);

        println!("{:?}", response);

        // First account should be account1 with 1M lamports
        assert!(response.value[0].is_some());
        assert_eq!(
            response.value[0].as_ref().unwrap().lamports,
            1_000_000,
            "First element should be account1"
        );

        // Second account should be None (pk2 doesn't exist locally or remotely)
        assert!(
            response.value[1].is_none(),
            "Second element should be None for missing pk2"
        );

        // Third account should be account3 with 3M lamports
        assert!(response.value[2].is_some());
        assert_eq!(
            response.value[2].as_ref().unwrap().lamports,
            3_000_000,
            "Third element should be account3"
        );

        println!("✅ Account order preserved with remote: [1M lamports, None, 3M lamports]");
    }
}
