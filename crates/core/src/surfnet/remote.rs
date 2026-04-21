use std::{collections::HashMap, str::FromStr};

use serde_json::json;
use solana_account::Account;
use solana_account_decoder::UiAccount;
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_client::{GetConfirmedSignaturesForAddress2Config, RpcClientConfig},
    rpc_config::{
        RpcAccountInfoConfig, RpcBlockConfig, RpcLargestAccountsConfig, RpcProgramAccountsConfig,
        RpcSignaturesForAddressConfig, RpcTokenAccountsFilter, RpcTransactionConfig,
    },
    rpc_filter::RpcFilterType,
    rpc_request::{RpcRequest, TokenAccountsFilter},
    rpc_response::{
        RpcAccountBalance, RpcConfirmedTransactionStatusWithSignature, RpcKeyedAccount, RpcResult,
        RpcTokenAccountBalance,
    },
};
use solana_clock::Slot;
use solana_commitment_config::CommitmentConfig;
use solana_epoch_info::EpochInfo;
use solana_epoch_schedule::EpochSchedule;
use solana_hash::Hash;
use solana_loader_v3_interface::get_program_data_address;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction_status::UiConfirmedBlock;

use super::GetTransactionResult;
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    rpc::utils::is_method_not_supported_error,
    surfnet::{GetAccountResult, locker::is_supported_token_program},
    types::{RemoteRpcResult, TokenAccount},
};

pub struct SurfnetRemoteClient {
    pub client: RpcClient,
}
impl Clone for SurfnetRemoteClient {
    fn clone(&self) -> Self {
        let remote_rpc_url = self.client.url();
        SurfnetRemoteClient::new_unsafe(remote_rpc_url)
            .expect("unable to clone SurfnetRemoteClient")
    }
}

pub trait SomeRemoteCtx {
    fn get_remote_ctx<T>(&self, input: T) -> Option<(SurfnetRemoteClient, T)>;
}

impl SomeRemoteCtx for Option<SurfnetRemoteClient> {
    fn get_remote_ctx<T>(&self, input: T) -> Option<(SurfnetRemoteClient, T)> {
        self.as_ref()
            .map(|remote_rpc_client| (remote_rpc_client.clone(), input))
    }
}

impl SurfnetRemoteClient {
    pub fn new<U: ToString>(remote_rpc_url: U) -> Self {
        let client = RpcClient::new(remote_rpc_url.to_string());
        SurfnetRemoteClient { client }
    }

    pub fn new_unsafe<U: ToString>(remote_rpc_url: U) -> Option<Self> {
        use reqwest;
        use solana_rpc_client::http_sender::HttpSender;

        // Retry HTTP client initialization to handle potential fork-related issues
        let client = match reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .tls_built_in_root_certs(false)
            .tls_built_in_webpki_certs(false)
            .timeout(std::time::Duration::from_secs(30))
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                error!(
                    "unable to initialize datasource client after retries: {}",
                    e
                );
                return None;
            }
        };
        let http_sender = HttpSender::new_with_client(remote_rpc_url, client);
        let client = RpcClient::new_sender(http_sender, RpcClientConfig::default());
        Some(SurfnetRemoteClient { client })
    }

    pub async fn get_epoch_info(&self) -> SurfpoolResult<EpochInfo> {
        self.client.get_epoch_info().await.map_err(Into::into)
    }

    pub async fn get_epoch_schedule(&self) -> SurfpoolResult<EpochSchedule> {
        self.client.get_epoch_schedule().await.map_err(Into::into)
    }

    pub async fn get_account(
        &self,
        pubkey: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> SurfpoolResult<GetAccountResult> {
        #[cfg(feature = "prometheus")]
        let fetch_start = std::time::Instant::now();

        let res = self
            .client
            .get_account_with_commitment(pubkey, commitment_config)
            .await
            .map_err(|e| SurfpoolError::get_account(*pubkey, e))?;

        let result = match res.value {
            Some(account) => {
                let mut result = None;
                if is_supported_token_program(&account.owner) {
                    if let Ok(token_account) = TokenAccount::unpack(&account.data) {
                        let mint = self
                            .client
                            .get_account_with_commitment(&token_account.mint(), commitment_config)
                            .await
                            .map_err(|e| SurfpoolError::get_account(*pubkey, e))?;

                        result = Some(GetAccountResult::FoundTokenAccount(
                            (*pubkey, account.clone()),
                            (token_account.mint(), mint.value),
                        ));
                    };
                } else if account.executable {
                    let program_data_address = get_program_data_address(pubkey);

                    let program_data = self
                        .client
                        .get_account_with_commitment(&program_data_address, commitment_config)
                        .await
                        .map_err(|e| SurfpoolError::get_account(*pubkey, e))?;

                    result = Some(GetAccountResult::FoundProgramAccount(
                        (*pubkey, account.clone()),
                        (program_data_address, program_data.value),
                    ));
                }

                result.unwrap_or(GetAccountResult::FoundAccount(
                    *pubkey, account,
                    // Mark this account as needing to be updated in the SVM, since we fetched it
                    true,
                ))
            }
            None => GetAccountResult::None(*pubkey),
        };
        #[cfg(feature = "prometheus")]
        if let Some(m) = crate::telemetry::metrics() {
            m.record_remote_fetch(fetch_start.elapsed().as_millis() as u64);
        }
        Ok(result)
    }

    pub async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
        commitment_config: CommitmentConfig,
    ) -> SurfpoolResult<Vec<GetAccountResult>> {
        #[cfg(feature = "prometheus")]
        let fetch_start = std::time::Instant::now();

        let remote_accounts = self
            .client
            .get_multiple_accounts(pubkeys)
            .await
            .map_err(SurfpoolError::get_multiple_accounts)?;
        debug!("Fetched {:?} accounts from remote", pubkeys);
        debug!(
            "Found accounts for pubkeys: {:#?}",
            remote_accounts
                .iter()
                .zip(pubkeys)
                .filter_map(|(account, pubkey)| if account.is_some() {
                    Some(pubkey)
                } else {
                    None
                })
                .collect::<Vec<&Pubkey>>()
        );
        let mut results_map: HashMap<Pubkey, GetAccountResult> = HashMap::new();
        let mut mint_accounts_src: Vec<(Pubkey, Account, Pubkey)> = vec![];
        let mut program_accounts_src: Vec<(Pubkey, Account, Pubkey)> = vec![];
        for (pubkey, remote_account) in pubkeys.iter().zip(remote_accounts) {
            if let Some(remote_account) = remote_account {
                if is_supported_token_program(&remote_account.owner) {
                    if let Ok(token_account) = TokenAccount::unpack(&remote_account.data) {
                        mint_accounts_src.push((*pubkey, remote_account, token_account.mint()));
                    } else {
                        results_map.insert(
                            *pubkey,
                            GetAccountResult::FoundAccount(
                                *pubkey,
                                remote_account,
                                // Mark this account as needing to be updated in the SVM, since we fetched it
                                true,
                            ),
                        );
                    }
                } else if remote_account.executable {
                    let program_data_address = get_program_data_address(pubkey);
                    program_accounts_src.push((*pubkey, remote_account, program_data_address));
                } else {
                    results_map.insert(
                        *pubkey,
                        GetAccountResult::FoundAccount(
                            *pubkey,
                            remote_account,
                            // Mark this account as needing to be updated in the SVM, since we fetched it
                            true,
                        ),
                    );
                }
            } else {
                results_map.insert(*pubkey, GetAccountResult::None(*pubkey));
            }
        }

        debug!(
            "Identified {} mint accounts and {} program accounts to fetch for remote accounts",
            mint_accounts_src.len(),
            program_accounts_src.len()
        );

        if !(mint_accounts_src.is_empty() && program_accounts_src.is_empty()) {
            let mint_acc_src_len = mint_accounts_src.len();
            let mut account_buffer = mint_accounts_src.clone();
            account_buffer.extend_from_slice(&program_accounts_src);

            let account_pubkeys: Vec<Pubkey> = account_buffer.iter().map(|p| p.2).collect();

            let binding_remote_accounts = self
                .client
                .get_multiple_accounts_with_commitment(&account_pubkeys, commitment_config)
                .await
                .map_err(SurfpoolError::get_multiple_accounts)?
                .value;

            debug!(
                "Fetched {} additional accounts from remote",
                binding_remote_accounts.len()
            );
            debug!(
                "Found additional accounts for pubkeys: {:#?}",
                binding_remote_accounts
                    .iter()
                    .zip(account_pubkeys)
                    .filter_map(|(account, pubkey)| if account.is_some() {
                        Some(pubkey)
                    } else {
                        None
                    })
                    .collect::<Vec<Pubkey>>()
            );

            for (index, remote_account) in binding_remote_accounts.iter().enumerate() {
                if index < mint_acc_src_len {
                    // mint accounts to be inserted
                    results_map.insert(
                        account_buffer[index].0,
                        GetAccountResult::FoundTokenAccount(
                            (account_buffer[index].0, account_buffer[index].1.clone()),
                            (account_buffer[index].2, remote_account.clone()),
                        ),
                    );
                } else {
                    results_map.insert(
                        account_buffer[index].0,
                        GetAccountResult::FoundProgramAccount(
                            (account_buffer[index].0, account_buffer[index].1.clone()),
                            (account_buffer[index].2, remote_account.clone()),
                        ),
                    );
                }
            }
        }
        #[cfg(feature = "prometheus")]
        if let Some(m) = crate::telemetry::metrics() {
            m.record_remote_fetch(fetch_start.elapsed().as_millis() as u64);
        }
        Ok(pubkeys
            .iter()
            .map(|pk| {
                results_map
                    .remove(pk)
                    .unwrap_or(GetAccountResult::None(*pk))
            })
            .collect())
    }

    pub async fn get_transaction(
        &self,
        signature: Signature,
        config: RpcTransactionConfig,
        latest_absolute_slot: u64,
    ) -> GetTransactionResult {
        match self
            .client
            .get_transaction_with_config(&signature, config)
            .await
        {
            Ok(tx) => GetTransactionResult::found_transaction(signature, tx, latest_absolute_slot),
            Err(_) => GetTransactionResult::None(signature),
        }
    }

    pub async fn get_token_accounts_by_owner(
        &self,
        owner: Pubkey,
        filter: &TokenAccountsFilter,
        config: &RpcAccountInfoConfig,
    ) -> SurfpoolResult<Vec<RpcKeyedAccount>> {
        let token_account_filter = match filter {
            TokenAccountsFilter::Mint(mint) => RpcTokenAccountsFilter::Mint(mint.to_string()),
            TokenAccountsFilter::ProgramId(program_id) => {
                RpcTokenAccountsFilter::ProgramId(program_id.to_string())
            }
        };

        // the RPC client's default implementation of get_token_accounts_by_owner doesn't allow providing the config,
        // so we need to use the send method directly
        let res: RpcResult<Vec<RpcKeyedAccount>> = self
            .client
            .send(
                RpcRequest::GetTokenAccountsByOwner,
                json!([owner.to_string(), token_account_filter, config]),
            )
            .await;
        res.map_err(|e| SurfpoolError::get_token_accounts(owner, filter, e))
            .map(|res| res.value)
    }

    pub async fn get_token_largest_accounts(
        &self,
        mint: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> SurfpoolResult<Vec<RpcTokenAccountBalance>> {
        self.client
            .get_token_largest_accounts_with_commitment(mint, commitment_config)
            .await
            .map(|response| response.value)
            .map_err(|e| SurfpoolError::get_token_largest_accounts(*mint, e))
    }

    pub async fn get_token_accounts_by_delegate(
        &self,
        delegate: Pubkey,
        filter: &TokenAccountsFilter,
        config: &RpcAccountInfoConfig,
    ) -> SurfpoolResult<Vec<RpcKeyedAccount>> {
        // validate that the program is supported if using ProgramId filter
        if let TokenAccountsFilter::ProgramId(program_id) = &filter {
            if !is_supported_token_program(program_id) {
                return Err(SurfpoolError::unsupported_token_program(*program_id));
            }
        }

        let token_account_filter = match &filter {
            TokenAccountsFilter::Mint(mint) => RpcTokenAccountsFilter::Mint(mint.to_string()),
            TokenAccountsFilter::ProgramId(program_id) => {
                RpcTokenAccountsFilter::ProgramId(program_id.to_string())
            }
        };

        let res: RpcResult<Vec<RpcKeyedAccount>> = self
            .client
            .send(
                RpcRequest::GetTokenAccountsByDelegate,
                json!([delegate.to_string(), token_account_filter, config]),
            )
            .await;

        res.map_err(|e| SurfpoolError::get_token_accounts_by_delegate_error(delegate, filter, e))
            .map(|res| res.value)
    }

    pub async fn get_program_accounts(
        &self,
        program_id: &Pubkey,
        account_config: RpcAccountInfoConfig,
        filters: Option<Vec<RpcFilterType>>,
    ) -> SurfpoolResult<RemoteRpcResult<Vec<(Pubkey, UiAccount)>>> {
        handle_remote_rpc(|| async {
            self.client
                .get_program_ui_accounts_with_config(
                    program_id,
                    RpcProgramAccountsConfig {
                        filters,
                        with_context: Some(false),
                        account_config,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| SurfpoolError::get_program_accounts(*program_id, e))
        })
        .await
    }

    pub async fn get_largest_accounts(
        &self,
        config: Option<RpcLargestAccountsConfig>,
    ) -> SurfpoolResult<RemoteRpcResult<Vec<RpcAccountBalance>>> {
        handle_remote_rpc(|| async {
            self.client
                .get_largest_accounts_with_config(config.unwrap_or_default())
                .await
                .map(|res| res.value)
                .map_err(SurfpoolError::get_largest_accounts)
        })
        .await
    }

    pub async fn get_genesis_hash(&self) -> SurfpoolResult<Hash> {
        self.client.get_genesis_hash().await.map_err(Into::into)
    }

    pub async fn get_signatures_for_address(
        &self,
        pubkey: &Pubkey,
        config: Option<&RpcSignaturesForAddressConfig>,
    ) -> SurfpoolResult<Vec<RpcConfirmedTransactionStatusWithSignature>> {
        let c = match config {
            Some(c) => GetConfirmedSignaturesForAddress2Config {
                before: c
                    .before
                    .as_deref()
                    .and_then(|s| Signature::from_str(&s).ok()),
                commitment: c.commitment,
                limit: c.limit,
                until: c
                    .until
                    .as_deref()
                    .and_then(|s| Signature::from_str(&s).ok()),
            },
            _ => GetConfirmedSignaturesForAddress2Config::default(),
        };
        self.client
            .get_signatures_for_address_with_config(pubkey, c)
            .await
            .map_err(SurfpoolError::get_signatures_for_address)
    }

    pub async fn get_block(
        &self,
        slot: &Slot,
        config: RpcBlockConfig,
    ) -> SurfpoolResult<UiConfirmedBlock> {
        self.client
            .get_block_with_config(*slot, config)
            .await
            .map_err(|e| SurfpoolError::get_block(e, *slot))
    }
}

/// Handles remote RPC calls, returning a `RemoteRpcResult` indicating whether the method was supported.
/// If the method is not supported, it returns `RemoteRpcResult::MethodNotSupported`.
/// If the method is supported, it returns `RemoteRpcResult::Ok(T)`.
/// If the method is supported but returns an error, it returns `Err(E)`.
pub async fn handle_remote_rpc<T, E, F, Fut>(fut: F) -> Result<RemoteRpcResult<T>, E>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    match fut().await {
        Ok(val) => Ok(RemoteRpcResult::Ok(val)),
        Err(e) if is_method_not_supported_error(&e) => Ok(RemoteRpcResult::MethodNotSupported),
        Err(e) => Err(e),
    }
}
