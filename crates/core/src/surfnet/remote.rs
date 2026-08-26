use std::{collections::HashMap, str::FromStr, time::Duration};

use async_trait::async_trait;
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
    rpc_request::{RpcError, RpcRequest, TokenAccountsFilter},
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
use solana_rpc_client::{
    http_sender::HttpSender,
    rpc_sender::{RpcSender, RpcTransportStats},
};
use solana_rpc_client_api::client_error::{
    Error as ClientError, ErrorKind as ClientErrorKind, Result as ClientResult,
};
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, UiConfirmedBlock, UiTransactionEncoding,
};
use surfpool_types::sanitized_datasource_url;

use super::GetTransactionResult;
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    rpc::utils::{MAX_SUPPORTED_TRANSACTION_VERSION, is_method_not_supported_error},
    surfnet::{
        AccountSource, CoupledAccount, GetAccountResult, locker::is_supported_token_program,
    },
    types::{RemoteRpcResult, TokenAccount},
};

/// How long one call to the datasource gets, start to finish.
///
/// The HTTP client's timeout bounds a single attempt rather than a call:
/// solana's sender retries a 429 up to five times and honours `Retry-After`
/// for as much as 120 seconds each time, so a throttled datasource can hold a
/// call for ten minutes with no individual attempt ever timing out. The value
/// sits comfortably above that 30 second per-attempt timeout, since a deadline
/// at or below it would cut off attempts that were going to succeed.
const DATASOURCE_DEADLINE: Duration = Duration::from_secs(60);

fn sanitized_client_error(error: &ClientError, datasource_url: &str) -> String {
    let endpoint =
        sanitized_datasource_url(datasource_url).unwrap_or_else(|| "the datasource".to_string());

    match error.kind() {
        ClientErrorKind::Reqwest(error) => {
            if let Some(status) = error.status() {
                format!("datasource returned HTTP {status} from {endpoint}")
            } else if error.is_timeout() {
                format!("datasource request to {endpoint} timed out")
            } else if error.is_connect() {
                format!("failed to connect to {endpoint}")
            } else if error.is_decode() {
                format!("failed to decode the response from {endpoint}")
            } else {
                format!("datasource request to {endpoint} failed")
            }
        }
        ClientErrorKind::Middleware(_) => format!("datasource middleware failed for {endpoint}"),
        ClientErrorKind::Io(error) => {
            format!("datasource I/O error ({:?}) for {endpoint}", error.kind())
        }
        ClientErrorKind::SerdeJson(error) => format!(
            "invalid JSON response from {endpoint} at line {}, column {}",
            error.line(),
            error.column()
        ),
        ClientErrorKind::RpcError(error) => match error {
            RpcError::RpcRequestError(_) | RpcError::ForUser(_) => {
                format!("datasource RPC request failed for {endpoint}")
            }
            RpcError::RpcResponseError { code, .. } => {
                format!("datasource RPC response error {code} from {endpoint}")
            }
            RpcError::ParseError(_) => {
                format!("failed to parse the RPC response from {endpoint}")
            }
        },
        ClientErrorKind::SigningError(error) => error.to_string(),
        ClientErrorKind::TransactionError(error) => error.to_string(),
        ClientErrorKind::Custom(_) => format!("datasource client error for {endpoint}"),
    }
}

/// Bounds how long the sender it wraps may take, so a datasource that stops
/// answering surfaces as an error rather than as a surfnet that appears stuck.
struct DeadlineSender<S> {
    inner: S,
    deadline: Duration,
}

impl<S> DeadlineSender<S> {
    fn new(inner: S, deadline: Duration) -> Self {
        DeadlineSender { inner, deadline }
    }
}

#[async_trait]
impl<S: RpcSender + Send + Sync> RpcSender for DeadlineSender<S> {
    async fn send(
        &self,
        request: RpcRequest,
        params: serde_json::Value,
    ) -> ClientResult<serde_json::Value> {
        match tokio::time::timeout(self.deadline, self.inner.send(request, params)).await {
            Ok(response) => response,
            // Scheme and host only. This message reaches a client through
            // JSON-RPC error data, and a datasource URL carries credentials in
            // its query, its path, and its userinfo. A URL that will not parse
            // is named generically rather than printed raw.
            Err(_) => Err(ClientErrorKind::Custom(format!(
                "{:?} to {} did not answer within {:?}",
                request,
                sanitized_datasource_url(&self.inner.url())
                    .unwrap_or_else(|| "the datasource".to_string()),
                self.deadline
            ))
            .into()),
        }
    }

    fn get_transport_stats(&self) -> RpcTransportStats {
        self.inner.get_transport_stats()
    }

    fn url(&self) -> String {
        self.inner.url()
    }
}

/// The RPC client a surfnet reaches its datasource through: an HTTP transport
/// wrapped in a [`DeadlineSender`], assembled behind one constructor so that
/// layers added later (tracing, recording, a retry policy) land here without
/// touching a public signature.
struct SurfpoolRpcClient {
    client: RpcClient,
}

impl SurfpoolRpcClient {
    fn new<U: ToString>(remote_rpc_url: U) -> Self {
        let sender = DeadlineSender::new(
            HttpSender::new(remote_rpc_url.to_string()),
            DATASOURCE_DEADLINE,
        );
        let client = RpcClient::new_sender(
            sender,
            RpcClientConfig::with_commitment(CommitmentConfig::default()),
        );
        SurfpoolRpcClient { client }
    }

    /// A variant that accepts invalid TLS certificates, for datasources
    /// behind self-signed certs.
    fn new_unsafe<U: ToString>(remote_rpc_url: U) -> Option<Self> {
        use reqwest;

        // Construction can fail after a fork (the daemonize path), so a
        // failure logs and surfaces as None rather than a panic.
        let client = match reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .tls_built_in_root_certs(false)
            .tls_built_in_webpki_certs(false)
            .timeout(std::time::Duration::from_secs(30))
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                error!("unable to initialize datasource client: {}", e);
                return None;
            }
        };
        let sender = DeadlineSender::new(
            HttpSender::new_with_client(remote_rpc_url, client),
            DATASOURCE_DEADLINE,
        );
        let client = RpcClient::new_sender(sender, RpcClientConfig::default());
        Some(SurfpoolRpcClient { client })
    }
}

/// A transaction fetched from a remote, with what the remote reports about running it
#[derive(Debug)]
pub struct RemoteTransaction {
    pub transaction: VersionedTransaction,
    pub slot: Slot,
    pub block_time: Option<i64>,
    pub error: Option<String>,
    pub compute_units_consumed: Option<u64>,
}

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
        SurfnetRemoteClient {
            client: SurfpoolRpcClient::new(remote_rpc_url).client,
        }
    }

    pub fn new_unsafe<U: ToString>(remote_rpc_url: U) -> Option<Self> {
        SurfpoolRpcClient::new_unsafe(remote_rpc_url).map(|rpc_client| SurfnetRemoteClient {
            client: rpc_client.client,
        })
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

                        result = Some(GetAccountResult::FoundCoupledAccount(
                            (*pubkey, account.clone()),
                            CoupledAccount::Mint(token_account.mint(), mint.value),
                            AccountSource::Remote,
                        ));
                    };
                } else if account.executable {
                    let program_data_address = get_program_data_address(pubkey);

                    let program_data = self
                        .client
                        .get_account_with_commitment(&program_data_address, commitment_config)
                        .await
                        .map_err(|e| SurfpoolError::get_account(*pubkey, e))?;

                    result = Some(GetAccountResult::FoundCoupledAccount(
                        (*pubkey, account.clone()),
                        CoupledAccount::ProgramData(program_data_address, program_data.value),
                        AccountSource::Remote,
                    ));
                }

                result.unwrap_or(GetAccountResult::FoundAccount(
                    *pubkey,
                    account,
                    AccountSource::Remote,
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
                                AccountSource::Remote,
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
                            AccountSource::Remote,
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
                        GetAccountResult::FoundCoupledAccount(
                            (account_buffer[index].0, account_buffer[index].1.clone()),
                            CoupledAccount::Mint(account_buffer[index].2, remote_account.clone()),
                            AccountSource::Remote,
                        ),
                    );
                } else {
                    results_map.insert(
                        account_buffer[index].0,
                        GetAccountResult::FoundCoupledAccount(
                            (account_buffer[index].0, account_buffer[index].1.clone()),
                            CoupledAccount::ProgramData(
                                account_buffer[index].2,
                                remote_account.clone(),
                            ),
                            AccountSource::Remote,
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
            .try_get_transaction(signature, config, latest_absolute_slot)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                error!("{e}");
                GetTransactionResult::None(signature)
            }
        }
    }

    pub(crate) async fn try_get_transaction(
        &self,
        signature: Signature,
        config: RpcTransactionConfig,
        latest_absolute_slot: u64,
    ) -> SurfpoolResult<GetTransactionResult> {
        let transaction = self
            .client
            .send::<Option<EncodedConfirmedTransactionWithStatusMeta>>(
                RpcRequest::GetTransaction,
                json!([signature.to_string(), config]),
            )
            .await
            .map_err(|error| {
                SurfpoolError::get_transaction(
                    signature,
                    sanitized_client_error(&error, &self.client.url()),
                )
            })?;

        Ok(match transaction {
            Some(tx) => {
                GetTransactionResult::found_transaction(signature, tx, latest_absolute_slot)
            }
            None => GetTransactionResult::None(signature),
        })
    }

    /// Gets a confirmed transaction from the remote, with its slot and block time.
    ///
    /// A `null` result means the transaction is missing. Any other failure is returned as an
    /// error.
    pub async fn get_versioned_transaction(
        &self,
        signature: Signature,
    ) -> SurfpoolResult<RemoteTransaction> {
        let config = RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::Base64),
            commitment: Some(CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(MAX_SUPPORTED_TRANSACTION_VERSION),
        };

        // Clean up the client error first. The raw one can contain the secret in the
        // datasource URL, and this message is shown to the user.
        let encoded = self
            .client
            .send::<Option<EncodedConfirmedTransactionWithStatusMeta>>(
                RpcRequest::GetTransaction,
                json!([signature.to_string(), config]),
            )
            .await
            .map_err(|error| {
                SurfpoolError::get_transaction(
                    signature,
                    sanitized_client_error(&error, &self.client.url()),
                )
            })?;
        let encoded = encoded.ok_or_else(|| SurfpoolError::transaction_not_found(signature))?;

        let slot = encoded.slot;
        let block_time = encoded.block_time;
        let meta = encoded.transaction.meta;
        let error = meta
            .as_ref()
            .and_then(|meta| meta.err.as_ref().map(|e| e.to_string()));
        let compute_units_consumed =
            meta.and_then(|meta| Option::<u64>::from(meta.compute_units_consumed));
        let transaction = encoded.transaction.transaction.decode().ok_or_else(|| {
            SurfpoolError::internal(format!(
                "Transaction {} could not be decoded from the datasource response",
                signature
            ))
        })?;

        Ok(RemoteTransaction {
            transaction,
            slot,
            block_time,
            error,
            compute_units_consumed,
        })
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use solana_client::rpc_request::RpcResponseErrorData;

    use super::*;
    use crate::surfnet::{locker::SurfnetSvmLocker, svm::SurfnetSvm};

    struct ReturnsNull {
        requests: Arc<Mutex<Vec<(RpcRequest, serde_json::Value)>>>,
    }

    #[async_trait]
    impl RpcSender for ReturnsNull {
        async fn send(
            &self,
            request: RpcRequest,
            params: serde_json::Value,
        ) -> ClientResult<serde_json::Value> {
            self.requests
                .lock()
                .expect("request recorder mutex should not be poisoned")
                .push((request, params));
            Ok(serde_json::Value::Null)
        }

        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats::default()
        }

        fn url(&self) -> String {
            "http://returns-null.example".to_string()
        }
    }

    struct ReturnsError;

    #[async_trait]
    impl RpcSender for ReturnsError {
        async fn send(
            &self,
            _request: RpcRequest,
            _params: serde_json::Value,
        ) -> ClientResult<serde_json::Value> {
            Err(ClientErrorKind::Custom("provider unavailable".to_string()).into())
        }

        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats::default()
        }

        fn url(&self) -> String {
            "http://returns-error.example".to_string()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_remote_transaction_remains_none_through_the_locker() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let client = SurfnetRemoteClient {
            client: RpcClient::new_sender(
                ReturnsNull {
                    requests: Arc::clone(&requests),
                },
                RpcClientConfig::default(),
            ),
        };
        let signature = Signature::new_unique();
        let config = RpcTransactionConfig {
            encoding: Some(solana_transaction_status::UiTransactionEncoding::Base64),
            commitment: Some(CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(0),
        };
        let (svm, _, _) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let result = locker
            .get_transaction(&Some(client), &signature, config)
            .await
            .expect("a null getTransaction result is not a provider failure");

        assert!(matches!(result, GetTransactionResult::None(found) if found == signature));
        let requests = requests
            .lock()
            .expect("request recorder mutex should not be poisoned");
        assert_eq!(
            requests.as_slice(),
            &[(
                RpcRequest::GetTransaction,
                json!([signature.to_string(), config])
            )]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_remote_transaction_provider_failure_reaches_the_locker_caller() {
        let client = SurfnetRemoteClient {
            client: RpcClient::new_sender(ReturnsError, RpcClientConfig::default()),
        };
        let signature = Signature::new_unique();
        let (svm, _, _) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);

        let error = match locker
            .get_transaction(&Some(client), &signature, RpcTransactionConfig::default())
            .await
        {
            Ok(_) => panic!("a provider failure must not be reported as a missing transaction"),
            Err(error) => error,
        };
        let message = error.to_string();

        assert!(message.contains(&signature.to_string()));
        assert!(message.contains("datasource client error"));
        assert!(message.contains("http://returns-error.example"));
        assert!(!message.contains("provider unavailable"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_remote_transaction_failure_does_not_disclose_datasource_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test server should bind");
        let address = listener
            .local_addr()
            .expect("test server should have a local address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("test server should accept");
            let mut request = [0u8; 4096];
            stream
                .readable()
                .await
                .expect("request stream should become readable");
            stream
                .try_read(&mut request)
                .expect("test server should read the request");
            let response = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n";
            stream
                .writable()
                .await
                .expect("response stream should become writable");
            stream
                .try_write(response)
                .expect("test server should write the response");
        });
        let datasource =
            format!("http://user:SUPERSECRET@{address}/private-path?api-key=SUPERSECRET");
        let client = SurfnetRemoteClient::new(&datasource);
        let signature = Signature::new_unique();

        let error = match client
            .try_get_transaction(signature, RpcTransactionConfig::default(), 0)
            .await
        {
            Ok(_) => panic!("an HTTP failure should reach the caller"),
            Err(error) => error.to_string(),
        };
        server.await.expect("test server should finish");

        assert!(error.contains("401 Unauthorized"));
        assert!(error.contains("http://127.0.0.1"));
        assert!(!error.contains("SUPERSECRET"));
        assert!(!error.contains("private-path"));
        assert!(!error.contains("api-key"));
    }

    #[test]
    fn provider_reflections_do_not_disclose_datasource_credentials() {
        let datasource =
            "https://user:SUPERSECRET@rpc.example.com/private/SUPERSECRET?api-key=SUPERSECRET";

        for fragment in [
            "user:SUPERSECRET",
            "/private/SUPERSECRET",
            "api-key=SUPERSECRET",
        ] {
            let errors = [
                (
                    ClientErrorKind::RpcError(RpcError::RpcResponseError {
                        code: -32000,
                        message: fragment.to_string(),
                        data: RpcResponseErrorData::Empty,
                    }),
                    Some("-32000"),
                ),
                (ClientErrorKind::Custom(fragment.to_string()), None),
            ];

            for (error, expected_code) in errors {
                let error = ClientError::from(error);
                let message = sanitized_client_error(&error, datasource);

                assert!(!message.contains("SUPERSECRET"));
                assert!(!message.contains("private"));
                assert!(!message.contains("api-key"));
                assert!(message.contains("https://rpc.example.com"));
                if let Some(code) = expected_code {
                    assert!(message.contains(code));
                }
            }
        }
    }

    /// A call that never completes, whether because the endpoint went quiet
    /// or because its retry policy never gave control back. The deadline does
    /// not need to know which.
    struct NeverAnswers(String);

    #[async_trait]
    impl RpcSender for NeverAnswers {
        async fn send(
            &self,
            _request: RpcRequest,
            _params: serde_json::Value,
        ) -> ClientResult<serde_json::Value> {
            std::future::pending().await
        }

        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats::default()
        }

        fn url(&self) -> String {
            self.0.clone()
        }
    }

    /// A datasource URL is a credential: the key can sit in the query, the
    /// path, or the userinfo, and this message reaches a client through
    /// JSON-RPC error data. The failure has to name the host without carrying
    /// the secret along with it.
    #[tokio::test]
    async fn a_timeout_does_not_disclose_the_datasource_credentials() {
        let secrets = [
            "https://rpc.example.com/?api-key=SUPERSECRET",
            "https://rpc.example.com/SUPERSECRET",
            "https://user:SUPERSECRET@rpc.example.com",
        ];

        for url in secrets {
            let sender =
                DeadlineSender::new(NeverAnswers(url.to_string()), Duration::from_millis(50));

            let message = sender
                .send(RpcRequest::GetSlot, serde_json::Value::Null)
                .await
                .expect_err("a datasource that never answers should not succeed")
                .to_string();

            assert!(
                !message.contains("SUPERSECRET"),
                "the failure disclosed the datasource credential: {message}"
            );
            assert!(
                message.contains("rpc.example.com"),
                "the failure should still name the host: {message}"
            );
        }
    }

    /// Answers every request with one fixed value.
    struct Answers(serde_json::Value);

    #[async_trait]
    impl RpcSender for Answers {
        async fn send(
            &self,
            _request: RpcRequest,
            _params: serde_json::Value,
        ) -> ClientResult<serde_json::Value> {
            Ok(self.0.clone())
        }

        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats::default()
        }

        fn url(&self) -> String {
            "http://test.example".to_string()
        }
    }

    /// Answers every request with one fixed value, and keeps the params it was called with.
    struct RecordsParams {
        answer: serde_json::Value,
        params: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    }

    #[async_trait]
    impl RpcSender for RecordsParams {
        async fn send(
            &self,
            _request: RpcRequest,
            params: serde_json::Value,
        ) -> ClientResult<serde_json::Value> {
            *self.params.lock().unwrap() = Some(params);
            Ok(self.answer.clone())
        }

        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats::default()
        }

        fn url(&self) -> String {
            "http://test.example".to_string()
        }
    }

    /// Fails every request, like a datasource cannot be reached.
    struct Fails(String);

    #[async_trait]
    impl RpcSender for Fails {
        async fn send(
            &self,
            _request: RpcRequest,
            _params: serde_json::Value,
        ) -> ClientResult<serde_json::Value> {
            Err(ClientErrorKind::Custom(self.0.clone()).into())
        }

        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats::default()
        }

        fn url(&self) -> String {
            "http://test.example".to_string()
        }
    }

    fn client_from(sender: impl RpcSender + Send + Sync + 'static) -> SurfnetRemoteClient {
        SurfnetRemoteClient {
            client: RpcClient::new_sender(sender, RpcClientConfig::default()),
        }
    }

    #[tokio::test]
    async fn a_null_result_is_a_missing_transaction() {
        let client = client_from(Answers(serde_json::Value::Null));

        let message = client
            .get_versioned_transaction(Signature::default())
            .await
            .expect_err("a datasource holding no such transaction should not succeed")
            .to_string();

        assert!(
            message.contains("not found"),
            "a null result should read as a missing transaction: {message}"
        );
    }

    /// A transaction is finalized about half a minute after it is confirmed. Asking for a
    /// finalized one would return `null` for a transaction the caller just sent, which looks
    /// like the transaction does not exist.
    #[tokio::test]
    async fn a_transaction_is_fetched_at_confirmed_commitment() {
        let params = std::sync::Arc::new(std::sync::Mutex::new(None));
        let client = client_from(RecordsParams {
            answer: serde_json::Value::Null,
            params: params.clone(),
        });

        let _ = client.get_versioned_transaction(Signature::default()).await;

        let params = params.lock().unwrap().clone().expect("a request was sent");
        assert_eq!(
            params[1]["commitment"], "confirmed",
            "the transaction should be fetched at confirmed commitment, got {params}"
        );
    }

    /// The caller has to know the difference between a datasource that turned the request
    /// away and a transaction that does not exist. The first one is fixed by using another
    /// endpoint, the second one is not. The reason has to reach the caller without the raw
    /// client error, which can contain the secret in the datasource URL.
    #[tokio::test]
    async fn a_datasource_failure_is_not_a_missing_transaction() {
        let client = client_from(Fails("429 Too Many Requests".to_string()));

        let message = client
            .get_versioned_transaction(Signature::default())
            .await
            .expect_err("a datasource that refuses the request should not succeed")
            .to_string();

        assert!(
            message.contains("datasource client error"),
            "the failure should say the datasource refused: {message}"
        );
        assert!(
            message.contains("http://test.example"),
            "the failure should still name the endpoint: {message}"
        );
        assert!(
            !message.contains("not found"),
            "a refused request should not read as a missing transaction: {message}"
        );
        assert!(
            !message.contains("429"),
            "the raw client error should not be passed through: {message}"
        );
    }

    #[tokio::test]
    async fn a_datasource_that_never_answers_is_an_error_rather_than_a_wait() {
        let sender = DeadlineSender::new(
            NeverAnswers("http://never.example".to_string()),
            Duration::from_millis(50),
        );

        let error = sender
            .send(RpcRequest::GetSlot, serde_json::Value::Null)
            .await
            .expect_err("a datasource that never answers should not succeed");

        let message = error.to_string();
        assert!(
            message.contains("did not answer"),
            "the failure should say what happened: {message}"
        );
        assert!(
            message.contains("http://never.example"),
            "the failure should identify the datasource: {message}"
        );
        assert!(
            message.contains("GetSlot"),
            "the failure should identify the request: {message}"
        );
    }
}
