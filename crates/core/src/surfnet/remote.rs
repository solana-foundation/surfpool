use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

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
use solana_clock::{Clock, Slot};
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
use solana_sysvar_id::SysvarId;
use solana_transaction_status::{EncodedConfirmedTransactionWithStatusMeta, UiConfirmedBlock};
use surfpool_types::sanitized_datasource_url;

use super::GetTransactionResult;
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    rpc::utils::is_method_not_supported_error,
    surfnet::{
        AccountSource, CoupledAccount, GetAccountResult, locker::is_supported_token_program,
    },
    types::{RemoteRpcResult, TokenAccount},
};

/// How long one call to the datasource gets, start to finish.
///
/// Without this outer deadline, the HTTP timeout applies per attempt,
/// so Solana retry/backoff handling can keep a datasource call alive
/// for up to ten minutes.
const DATASOURCE_DEADLINE: Duration = Duration::from_secs(60);
const DATASOURCE_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DATASOURCE_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_HISTORICAL_PAGES: usize = 1024;

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

/// The datasource's answer for a `Mint` filter it cannot resolve: JSON-RPC `-32602` with
/// `could not find mint`. Any other `-32602` (bad encoding, bad program filter) is a request
/// error the caller must see.
fn is_unknown_mint(filter: &TokenAccountsFilter, error: &ClientError) -> bool {
    matches!(filter, TokenAccountsFilter::Mint(_))
        && matches!(
            error.kind(),
            ClientErrorKind::RpcError(RpcError::RpcResponseError { code: -32602, message, .. })
                if message.contains("could not find mint")
        )
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

/// Pins account hydration and bounds remote history to one slot.
/// Queries without a historical source fail rather than mixing current state into the fork.
struct ForkSender<S> {
    inner: S,
    slot: Option<Slot>,
    signature_cursors: Mutex<HashMap<Pubkey, (String, Slot)>>,
}

impl<S: RpcSender + Send + Sync> ForkSender<S> {
    async fn transaction_slot(&self, signature: &str) -> ClientResult<Slot> {
        Signature::from_str(signature)
            .map_err(|_| ClientErrorKind::Custom("Invalid signature cursor".into()))?;
        let response = self.inner.send(RpcRequest::GetTransaction,
            json!([signature, {"encoding": "base64", "commitment": "finalized", "maxSupportedTransactionVersion": 0}])).await?;
        response["slot"].as_u64().ok_or_else(|| {
            ClientErrorKind::Custom("Cannot resolve historical signature cursor".into()).into()
        })
    }

    async fn signatures(
        &self,
        params: serde_json::Value,
        slot: Slot,
    ) -> ClientResult<serde_json::Value> {
        let address = params[0]
            .as_str()
            .ok_or_else(|| ClientErrorKind::Custom("Invalid address".into()))?;
        let address_key = Pubkey::from_str(address)
            .map_err(|_| ClientErrorKind::Custom("Invalid address".into()))?;
        let mut config: RpcSignaturesForAddressConfig = if params[1].is_null() {
            Default::default()
        } else {
            serde_json::from_value(params[1].clone())?
        };
        let limit = config.limit.unwrap_or(1000);
        if !(1..=1000).contains(&limit) || config.min_context_slot.is_some_and(|min| min > slot) {
            return Err(ClientErrorKind::Custom(
                "Invalid limit or minimum context slot for historical signatures".into(),
            )
            .into());
        }
        let mut cursor_slot = u64::MAX;
        let mut until_slot = None;
        for (cursor, is_before) in [(&config.before, true), (&config.until, false)] {
            if let Some(cursor) = cursor {
                let resolved_slot = self.transaction_slot(cursor).await?;
                if resolved_slot > slot {
                    return Err(ClientErrorKind::Custom(
                        "Signature cursor is after the fork slot".into(),
                    )
                    .into());
                }
                if is_before {
                    cursor_slot = resolved_slot;
                } else {
                    until_slot = Some(resolved_slot);
                }
            }
        }
        if config.before.is_none()
            && let Some((cursor, cached_slot)) =
                self.signature_cursors.lock().unwrap().get(&address_key)
        {
            config.before = Some(cursor.clone());
            cursor_slot = *cached_slot;
        }
        config.commitment = Some(CommitmentConfig::finalized());
        config.min_context_slot = None;
        config.limit = Some(1000);
        let mut seen = HashSet::new();
        if let Some(before) = &config.before {
            seen.insert(before.clone());
        }
        let mut result = Vec::new();
        for _ in 0..MAX_HISTORICAL_PAGES {
            let response = self
                .inner
                .send(
                    RpcRequest::GetSignaturesForAddress,
                    json!([address, config]),
                )
                .await?;
            let page: Vec<RpcConfirmedTransactionStatusWithSignature> =
                serde_json::from_value(response)?;
            if page.is_empty() {
                return Ok(serde_json::to_value(result)?);
            }
            if page.len() > 1000 {
                return Err(ClientErrorKind::Custom("Oversized signature page".into()).into());
            }
            for entry in &page {
                if entry.slot > cursor_slot
                    || until_slot.is_some_and(|until| entry.slot < until)
                    || config.until.as_ref() == Some(&entry.signature)
                    || Signature::from_str(&entry.signature).is_err()
                    || !seen.insert(entry.signature.clone())
                {
                    return Err(ClientErrorKind::Custom(
                        "Invalid or repeating historical signature page".into(),
                    )
                    .into());
                }
                cursor_slot = entry.slot;
            }
            // Binary search the descending page for the first signature at or before S.
            let cutoff = page.partition_point(|entry| entry.slot > slot);
            if cutoff > 0 {
                let mut cursors = self.signature_cursors.lock().unwrap();
                if cursors.len() >= 1024 && !cursors.contains_key(&address_key) {
                    cursors.clear();
                }
                let boundary = &page[cutoff - 1];
                cursors.insert(address_key, (boundary.signature.clone(), boundary.slot));
            }
            config.before = page.last().map(|entry| entry.signature.clone());
            result.extend(page.into_iter().skip(cutoff).take(limit - result.len()));
            if result.len() == limit {
                return Ok(serde_json::to_value(result)?);
            }
        }
        Err(ClientErrorKind::Custom(
            "Historical signature pagination exceeded its page budget".into(),
        )
        .into())
    }

    async fn token_accounts_by_owner(
        &self,
        params: serde_json::Value,
        slot: Slot,
    ) -> ClientResult<serde_json::Value> {
        let mut config = params[2].clone();
        if config.is_null() {
            config = json!({});
        }
        if !config.is_object() {
            return Err(ClientErrorKind::Custom("Invalid account config".into()).into());
        }
        if config["minContextSlot"]
            .as_u64()
            .is_some_and(|min| min > slot)
        {
            return Err(
                ClientErrorKind::Custom("Minimum context slot is after the fork".into()).into(),
            );
        }
        let mut cursor = serde_json::Value::Null;
        let mut cursors = HashSet::new();
        let mut keys = HashSet::new();
        let mut result = Vec::new();
        for _ in 0..MAX_HISTORICAL_PAGES {
            let mut archive_config = json!({"slot": slot, "pageLimit": 1000});
            if !cursor.is_null() {
                archive_config["pageKey"] = cursor;
            }
            let response = self
                .inner
                .send(
                    RpcRequest::Custom {
                        method: "getTokenAccountsByOwnerAtSlot",
                    },
                    json!([params[0], params[1], archive_config]),
                )
                .await?;
            if response["context"]["slot"].as_u64() != Some(slot) {
                return Err(ClientErrorKind::Custom(
                    "Token account page is not at the fork slot".into(),
                )
                .into());
            }
            let page = response["value"]
                .as_array()
                .ok_or_else(|| ClientErrorKind::Custom("Invalid token account page".into()))?;
            if page.len() > 1000 {
                return Err(ClientErrorKind::Custom("Oversized token account page".into()).into());
            }
            for entry in page {
                let key = entry["pubkey"].as_str().ok_or_else(|| {
                    ClientErrorKind::Custom("Missing token account pubkey".into())
                })?;
                if Pubkey::from_str(key).is_err() || !keys.insert(key.to_owned()) {
                    return Err(ClientErrorKind::Custom(
                        "Invalid or repeating token account".into(),
                    )
                    .into());
                }
            }
            // Discovery returns jsonParsed only. Read bytes at S to preserve the requested encoding/dataSlice.
            for chunk in page.chunks(100) {
                let accounts = jsonrpc_core::futures::future::try_join_all(
                    chunk
                        .iter()
                        .map(|entry| self.account(entry["pubkey"].clone(), config.clone(), slot)),
                )
                .await?;
                for (entry, account) in chunk.iter().zip(accounts) {
                    if account["value"].is_null() {
                        return Err(ClientErrorKind::Custom(
                            "Historical token account disappeared during hydration".into(),
                        )
                        .into());
                    }
                    result.push(json!({"pubkey": entry["pubkey"], "account": account["value"]}));
                }
            }
            cursor = response
                .get("pageKey")
                .cloned()
                .ok_or_else(|| ClientErrorKind::Custom("Missing token pagination cursor".into()))?;
            if cursor.is_null() {
                return Ok(json!({"context": {"slot": slot}, "value": result}));
            }
            let next = cursor
                .as_str()
                .filter(|next| !next.is_empty())
                .ok_or_else(|| ClientErrorKind::Custom("Invalid token pagination cursor".into()))?;
            if !cursors.insert(next.to_owned()) {
                return Err(
                    ClientErrorKind::Custom("Repeating token pagination cursor".into()).into(),
                );
            }
        }
        Err(
            ClientErrorKind::Custom("Historical token pagination exceeded its page budget".into())
                .into(),
        )
    }
    async fn account(
        &self,
        pubkey: serde_json::Value,
        mut config: serde_json::Value,
        slot: Slot,
    ) -> ClientResult<serde_json::Value> {
        if config.is_null() {
            config = json!({});
        }
        let config = config
            .as_object_mut()
            .ok_or_else(|| ClientErrorKind::Custom("Invalid account config".into()))?;
        if config
            .get("minContextSlot")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|minimum| minimum > slot)
        {
            return Err(
                ClientErrorKind::Custom("Minimum context slot is after the fork".into()).into(),
            );
        }
        config.remove("minContextSlot");
        config.insert("slot".into(), json!(slot));
        config.insert("commitment".into(), json!("finalized"));
        let response = self
            .inner
            .send(RpcRequest::GetAccountInfo, json!([pubkey, config]))
            .await?;
        if response["context"]["slot"].as_u64() != Some(slot) || response.get("value").is_none() {
            return Err(ClientErrorKind::Custom(format!("Datasource did not return account state at fork slot {slot}; an account archive RPC is required")).into());
        }
        Ok(response)
    }
}

#[async_trait]
impl<S: RpcSender + Send + Sync> RpcSender for ForkSender<S> {
    async fn send(
        &self,
        request: RpcRequest,
        mut params: serde_json::Value,
    ) -> ClientResult<serde_json::Value> {
        let Some(slot) = self.slot else {
            return self.inner.send(request, params).await;
        };
        if matches!(request, RpcRequest::GetBlock | RpcRequest::GetTransaction) {
            let args = params
                .as_array_mut()
                .filter(|args| (1..=2).contains(&args.len()))
                .ok_or_else(|| {
                    ClientErrorKind::Custom("Invalid historical request parameters".into())
                })?;
            if args.len() == 1 {
                args.push(json!({}));
            }
            if args[1].is_null() {
                args[1] = json!({});
            }
            let config = args[1].as_object_mut().ok_or_else(|| {
                ClientErrorKind::Custom("Invalid historical request config".into())
            })?;
            config.insert("commitment".into(), json!("finalized"));
        }
        match request {
            RpcRequest::GetSignaturesForAddress => self.signatures(params, slot).await,
            RpcRequest::GetTokenAccountsByOwner => self.token_accounts_by_owner(params, slot).await,
            RpcRequest::GetBlocks | RpcRequest::GetBlocksWithLimit => {
                let start = params[0]
                    .as_u64()
                    .ok_or_else(|| ClientErrorKind::Custom("Invalid block start slot".into()))?;
                if start > slot {
                    return Ok(json!([]));
                }
                let (end, limit, query) = if request == RpcRequest::GetBlocks {
                    let end = if params[1].is_null() || params[1].is_object() {
                        slot
                    } else {
                        params[1]
                            .as_u64()
                            .ok_or_else(|| {
                                ClientErrorKind::Custom("Invalid block end slot".into())
                            })?
                            .min(slot)
                    };
                    if end < start {
                        return Ok(json!([]));
                    }
                    (
                        end,
                        500_001,
                        json!([start, end, {"commitment": "finalized"}]),
                    )
                } else {
                    let limit = params[1]
                        .as_u64()
                        .filter(|limit| *limit <= 500_000)
                        .ok_or_else(|| ClientErrorKind::Custom("Invalid block limit".into()))?
                        as usize;
                    (
                        u64::MAX,
                        limit,
                        json!([start, limit, {"commitment": "finalized"}]),
                    )
                };
                let blocks: Vec<Slot> =
                    serde_json::from_value(self.inner.send(request, query).await?)?;
                if blocks.len() > limit
                    || blocks.iter().any(|block| *block < start || *block > end)
                    || blocks.windows(2).any(|pair| pair[0] >= pair[1])
                {
                    return Err(
                        ClientErrorKind::Custom("Invalid historical block list".into()).into(),
                    );
                }
                Ok(serde_json::to_value(
                    blocks
                        .into_iter()
                        .take_while(|block| *block <= slot)
                        .collect::<Vec<_>>(),
                )?)
            }
            RpcRequest::GetAccountInfo => {
                self.account(params[0].clone(), params[1].clone(), slot)
                    .await
            }
            RpcRequest::GetMultipleAccounts => {
                let pubkeys = params[0]
                    .as_array()
                    .ok_or_else(|| ClientErrorKind::Custom("Invalid account list".into()))?;
                // Archive providers may only support historical getAccountInfo.
                let responses = jsonrpc_core::futures::future::try_join_all(
                    pubkeys
                        .iter()
                        .map(|pubkey| self.account(pubkey.clone(), params[1].clone(), slot)),
                )
                .await?;
                let values: Vec<_> = responses
                    .into_iter()
                    .map(|response| response["value"].clone())
                    .collect();
                Ok(json!({"context": {"slot": slot}, "value": values}))
            }
            RpcRequest::GetBlock | RpcRequest::GetBlockTime => {
                let requested_slot = params[0]
                    .as_u64()
                    .ok_or_else(|| ClientErrorKind::Custom("Invalid block slot".into()))?;
                if requested_slot > slot {
                    return Err(ClientErrorKind::Custom(format!(
                        "Block {requested_slot} is after fork slot {slot}"
                    ))
                    .into());
                }
                self.inner.send(request, params).await
            }
            RpcRequest::GetTransaction => {
                let response = self.inner.send(request, params).await?;
                if !response.is_null() {
                    let transaction_slot = response["slot"].as_u64().ok_or_else(|| {
                        ClientErrorKind::Custom("Transaction slot unavailable".into())
                    })?;
                    if transaction_slot > slot {
                        return Err(ClientErrorKind::Custom(format!(
                            "Transaction is after fork slot {slot}"
                        ))
                        .into());
                    }
                }
                Ok(response)
            }
            RpcRequest::GetVersion | RpcRequest::GetGenesisHash | RpcRequest::GetEpochSchedule => {
                self.inner.send(request, params).await
            }
            // "Unavailable" errors trigger callers' fallback to local-only results.
            _ => Err(ClientErrorKind::Custom(format!(
                "Cannot answer {request} at fork slot {slot}"
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
    fn try_new<U: ToString>(
        remote_rpc_url: U,
        fork_slot: Option<Slot>,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .default_headers(HttpSender::default_headers())
            .timeout(DATASOURCE_HTTP_TIMEOUT)
            .pool_idle_timeout(DATASOURCE_POOL_IDLE_TIMEOUT)
            .build()?;
        let sender = DeadlineSender::new(
            ForkSender {
                inner: HttpSender::new_with_client(remote_rpc_url, client),
                slot: fork_slot,
                signature_cursors: Mutex::default(),
            },
            DATASOURCE_DEADLINE,
        );
        let client = RpcClient::new_sender(
            sender,
            RpcClientConfig::with_commitment(CommitmentConfig::default()),
        );
        Ok(SurfpoolRpcClient { client })
    }
}

#[derive(Clone)]
pub struct SurfnetRemoteClient {
    pub fork_slot: Option<Slot>,
    pub client: Arc<RpcClient>,
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
        Self::try_new(remote_rpc_url).expect("unable to initialize datasource client")
    }

    pub fn try_new<U: ToString>(remote_rpc_url: U) -> Result<Self, reqwest::Error> {
        Self::try_new_at_slot(remote_rpc_url, None)
    }

    pub fn try_new_at_slot<U: ToString>(
        remote_rpc_url: U,
        fork_slot: Option<Slot>,
    ) -> Result<Self, reqwest::Error> {
        SurfpoolRpcClient::try_new(remote_rpc_url, fork_slot).map(|rpc_client| {
            SurfnetRemoteClient {
                fork_slot,
                client: Arc::new(rpc_client.client),
            }
        })
    }

    pub async fn get_epoch_info(&self) -> SurfpoolResult<EpochInfo> {
        let Some(slot) = self.fork_slot else {
            return self.client.get_epoch_info().await.map_err(Into::into);
        };
        let schedule = self.get_epoch_schedule().await?;
        let block = self
            .get_block(
                &slot,
                RpcBlockConfig {
                    commitment: Some(CommitmentConfig::finalized()),
                    transaction_details: Some(solana_transaction_status::TransactionDetails::None),
                    rewards: Some(false),
                    max_supported_transaction_version: Some(0),
                    ..Default::default()
                },
            )
            .await?;
        let (epoch, slot_index) = schedule.get_epoch_and_slot_index(slot);
        Ok(EpochInfo {
            absolute_slot: slot,
            block_height: block
                .block_height
                .ok_or_else(|| SurfpoolError::internal("Fork block height unavailable"))?,
            epoch,
            slot_index,
            slots_in_epoch: schedule.get_slots_in_epoch(epoch),
            transaction_count: None,
        })
    }

    pub async fn get_fork_clock(&self) -> SurfpoolResult<Option<Clock>> {
        let Some(slot) = self.fork_slot else {
            return Ok(None);
        };
        let account = self.client.get_account(&Clock::id()).await?;
        let clock: Clock = bincode::deserialize(&account.data)
            .map_err(|e| SurfpoolError::internal(e.to_string()))?;
        if clock.slot != slot || clock.unix_timestamp < 0 {
            return Err(SurfpoolError::internal(
                "Archive returned an invalid fork Clock",
            ));
        }
        Ok(Some(clock))
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
            .get_multiple_accounts_with_commitment(pubkeys, commitment_config)
            .await
            .map_err(SurfpoolError::get_multiple_accounts)?
            .value;
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
        match res {
            Ok(res) => Ok(res.value),
            // A mint that exists only on this surfnet is `could not find mint` upstream. That is
            // a definite "no remote accounts", not a failed lookup, and must not discard the
            // local accounts the caller merges with. Historical walks propagate errors since
            // an error may come from a later page or account hydration.
            Err(e) if self.fork_slot.is_none() && is_unknown_mint(filter, &e) => {
                log::debug!(
                    "datasource does not know the mint in getTokenAccountsByOwner for {owner}; \
                     answering from local accounts only"
                );
                Ok(vec![])
            }
            Err(e) => Err(SurfpoolError::get_token_accounts(owner, filter, e)),
        }
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
        if self.fork_slot.is_some() {
            return self
                .client
                .send(
                    RpcRequest::GetSignaturesForAddress,
                    json!([pubkey.to_string(), config]),
                )
                .await
                .map_err(SurfpoolError::get_signatures_for_address);
        }
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

    pub async fn get_block_time(&self, slot: Slot) -> SurfpoolResult<Option<i64>> {
        self.client
            .send(RpcRequest::GetBlockTime, json!([slot]))
            .await
            .map_err(Into::into)
    }

    /// Historical lookup failures must not become incomplete block lists.
    pub async fn get_blocks(&self, start: Slot, end: Option<Slot>) -> SurfpoolResult<Vec<Slot>> {
        match self.client.get_blocks(start, end).await {
            Err(_) if self.fork_slot.is_none() => Ok(vec![]), // Preserve the live datasource fallback.
            result => result.map_err(Into::into),
        }
    }

    pub async fn get_blocks_with_limit(
        &self,
        start: Slot,
        limit: usize,
    ) -> SurfpoolResult<Vec<Slot>> {
        self.client
            .send(RpcRequest::GetBlocksWithLimit, json!([start, limit]))
            .await
            .map_err(Into::into)
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

    const FORK_SLOT: u64 = 1_000_000;

    struct ArchiveRpc {
        slot: u64,
        program: Pubkey,
    }

    #[async_trait]
    impl RpcSender for ArchiveRpc {
        async fn send(
            &self,
            request: RpcRequest,
            params: serde_json::Value,
        ) -> ClientResult<serde_json::Value> {
            use base64::{Engine, engine::general_purpose::STANDARD};
            Ok(match request {
                RpcRequest::GetAccountInfo => {
                    assert_eq!(params[1]["slot"], FORK_SLOT);
                    assert_eq!(params[1]["commitment"], "finalized");
                    assert!(params[1].get("minContextSlot").is_none());
                    let data = if params[0] == Clock::id().to_string() {
                        bincode::serialize(&Clock {
                            slot: FORK_SLOT,
                            epoch: 2,
                            unix_timestamp: 1_600_000_000,
                            ..Clock::default()
                        })
                        .unwrap()
                    } else {
                        vec![1, 2, 3]
                    };
                    let value = if params[0] == "missing" {
                        serde_json::Value::Null
                    } else {
                        json!({"lamports": 1_000_000, "owner": Pubkey::default().to_string(),
                            "data": [STANDARD.encode(data), "base64"], "rentEpoch": 0,
                            "executable": params[0] == self.program.to_string()})
                    };
                    json!({"context": {"slot": self.slot}, "value": value})
                }
                RpcRequest::GetSignaturesForAddress => {
                    if params[1]["before"].as_str().is_some() {
                        json!([])
                    } else {
                        json!([signature_row(99, FORK_SLOT - 1)])
                    }
                }
                RpcRequest::GetTransaction => json!({"slot": self.slot}),
                RpcRequest::GetEpochSchedule => {
                    serde_json::to_value(EpochSchedule::without_warmup()).unwrap()
                }
                RpcRequest::GetGenesisHash => json!(Hash::default().to_string()),
                RpcRequest::GetVersion => json!({"solana-core": "4.2.0", "feature-set": 0}),
                RpcRequest::GetBlock => {
                    assert_eq!(params[0], FORK_SLOT);
                    json!({"blockhash": Hash::default().to_string(), "previousBlockhash": Hash::default().to_string(),
                        "parentSlot": FORK_SLOT - 1, "blockHeight": 900_000, "blockTime": 1_600_000_000})
                }
                _ => panic!("unexpected archive request: {request}"),
            })
        }
        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats::default()
        }
        fn url(&self) -> String {
            "http://archive.example".into()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fork_slot_pins_accounts_dependencies_and_startup() {
        use solana_keypair::Keypair;
        use solana_signer::Signer;
        use solana_transaction::Transaction;

        let program = Pubkey::new_unique();
        let client = SurfnetRemoteClient {
            fork_slot: Some(FORK_SLOT),
            client: RpcClient::new_sender(
                ForkSender {
                    inner: ArchiveRpc {
                        slot: FORK_SLOT,
                        program,
                    },
                    slot: Some(FORK_SLOT),
                    signature_cursors: Mutex::default(),
                },
                RpcClientConfig::default(),
            )
            .into(),
        };
        let program_data = get_program_data_address(&program);
        for result in [
            client
                .get_account(&program, CommitmentConfig::processed())
                .await
                .unwrap(),
            client
                .get_multiple_accounts(&[program], CommitmentConfig::confirmed())
                .await
                .unwrap()
                .remove(0),
        ] {
            assert!(
                matches!(result, GetAccountResult::FoundCoupledAccount(_, CoupledAccount::ProgramData(key, Some(_)), _) if key == program_data)
            );
        }

        let missing: serde_json::Value = client.client.send(RpcRequest::GetMultipleAccounts,
            json!([["missing", program.to_string()], {"encoding": "base64", "minContextSlot": FORK_SLOT}])).await.unwrap();
        assert!(missing["value"][0].is_null());
        assert_eq!(missing["value"][1]["executable"], true);

        let (svm, _, _) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);
        let remote = Some(client);
        locker.initialize(&remote).await.unwrap();
        for reset in [false, true] {
            if reset {
                locker.reset_network(&remote).await.unwrap();
            }
            assert_eq!(locker.get_epoch_info().absolute_slot, FORK_SLOT);
            assert_eq!(locker.get_epoch_info().block_height, 900_000);
            locker.with_svm_reader(|svm| {
                assert_eq!(svm.inner.get_sysvar::<Clock>().slot, FORK_SLOT);
                assert_eq!(
                    svm.inner.get_sysvar::<Clock>().unix_timestamp,
                    1_600_000_000
                );
                assert_eq!(
                    svm.calculate_block_time_for_slot(FORK_SLOT),
                    1_600_000_000_000
                );
            });
        }

        // Advancing and transacting locally must not advance lazy remote reads.
        locker
            .with_svm_writer(|svm| svm.confirm_current_block())
            .unwrap();
        assert_eq!(locker.get_latest_absolute_slot(), FORK_SLOT + 1);
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        locker
            .airdrop(&payer.pubkey(), 10_000_000)
            .unwrap()
            .unwrap();
        locker.airdrop(&recipient, 1_000_000).unwrap().unwrap();
        let tx = Transaction::new_signed_with_payer(
            &[solana_system_interface::instruction::transfer(
                &payer.pubkey(),
                &recipient,
                1_000_000,
            )],
            Some(&payer.pubkey()),
            &[&payer],
            locker.latest_absolute_blockhash(),
        );
        let signature = tx.signatures[0];
        let context = remote
            .clone()
            .map(|client| (client, CommitmentConfig::processed()));
        let (status_tx, _) = crossbeam_channel::unbounded();
        locker
            .process_transaction(&context, tx.into(), status_tx, true, true)
            .await
            .unwrap();
        let balance = locker
            .get_account(&context, &recipient, None)
            .await
            .unwrap()
            .inner
            .map_account()
            .unwrap()
            .lamports;
        assert_eq!(balance, 2_000_000);
        assert!(
            !locker
                .get_transaction(&remote, &signature, RpcTransactionConfig::default())
                .await
                .unwrap()
                .is_none()
        );
        let signatures = locker
            .get_signatures_for_address(
                &remote.clone().map(|client| (client, ())),
                &recipient,
                Some(&RpcSignaturesForAddressConfig {
                    min_context_slot: Some(FORK_SLOT + 1),
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .inner;
        assert!(
            signatures
                .iter()
                .any(|row| row.signature == signature.to_string())
        );
        assert_eq!(signatures.last().unwrap().slot, FORK_SLOT - 1);
        // ArchiveRpc asserts that this newly fetched account still requests FORK_SLOT.
        let account = locker
            .get_account(&context, &Pubkey::new_unique(), None)
            .await
            .unwrap();
        assert_eq!(account.inner.map_account().unwrap().lamports, 1_000_000);
    }

    #[tokio::test]
    async fn fork_slot_rejects_latest_state_and_unsupported_queries() {
        let sender = ForkSender {
            inner: ArchiveRpc {
                slot: FORK_SLOT + 1,
                program: Pubkey::new_unique(),
            },
            slot: Some(FORK_SLOT),
            signature_cursors: Mutex::default(),
        };
        for (method, params) in [
            (RpcRequest::GetBlock, json!([FORK_SLOT + 1, {}])),
            (RpcRequest::GetBlockTime, json!([FORK_SLOT + 1])),
            (RpcRequest::GetTransaction, json!(["signature", {}])),
            (
                RpcRequest::GetAccountInfo,
                json!(["missing", {"minContextSlot": FORK_SLOT + 1}]),
            ),
        ] {
            assert!(sender.send(method, params).await.is_err());
        }
        for transaction_slot in [FORK_SLOT - 1, FORK_SLOT] {
            let historical = ForkSender {
                inner: ArchiveRpc {
                    slot: transaction_slot,
                    program: Pubkey::new_unique(),
                },
                slot: Some(FORK_SLOT),
                signature_cursors: Mutex::default(),
            };
            let response = historical
                .send(RpcRequest::GetTransaction, json!(["signature"]))
                .await
                .unwrap();
            assert_eq!(response["slot"], transaction_slot);
        }
        for params in [json!(null), json!([]), json!([FORK_SLOT, "invalid"])] {
            assert!(sender.send(RpcRequest::GetBlock, params).await.is_err());
        }
        for method in [RpcRequest::GetAccountInfo, RpcRequest::GetMultipleAccounts] {
            let params = if method == RpcRequest::GetAccountInfo {
                json!(["missing"])
            } else {
                json!([["missing"]])
            };
            assert!(
                sender
                    .send(method, params)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("fork slot")
            );
        }
        for method in [
            RpcRequest::GetProgramAccounts,
            RpcRequest::GetLargestAccounts,
            RpcRequest::GetSignatureStatuses,
        ] {
            let error = sender.send(method, json!([])).await.unwrap_err();
            assert!(error.to_string().contains("Cannot answer"));
            assert!(!is_method_not_supported_error(&error));
        }
        let malformed = ForkSender {
            inner: ReturnsNull {
                requests: Arc::default(),
            },
            slot: Some(FORK_SLOT),
            signature_cursors: Mutex::default(),
        };
        assert!(
            malformed
                .send(RpcRequest::GetAccountInfo, json!(["missing"]))
                .await
                .is_err()
        );
        let unavailable = ForkSender {
            inner: ReturnsError,
            slot: Some(FORK_SLOT),
            signature_cursors: Mutex::default(),
        };
        assert!(
            unavailable
                .send(RpcRequest::GetAccountInfo, json!(["missing"]))
                .await
                .is_err()
        );
        let requests = Arc::default();
        let ordinary = ForkSender {
            inner: ReturnsNull {
                requests: Arc::clone(&requests),
            },
            slot: None,
            signature_cursors: Mutex::default(),
        };
        ordinary
            .send(RpcRequest::GetProgramAccounts, json!(["program"]))
            .await
            .unwrap();
        assert_eq!(
            requests.lock().unwrap()[0],
            (RpcRequest::GetProgramAccounts, json!(["program"]))
        );
    }

    #[tokio::test]
    async fn historical_scan_failures_cannot_become_local_only_results() {
        for fork_slot in [None, Some(FORK_SLOT)] {
            let client = SurfnetRemoteClient {
                fork_slot,
                client: RpcClient::new_sender(
                    ForkSender {
                        inner: ReturnsError,
                        slot: fork_slot,
                        signature_cursors: Mutex::default(),
                    },
                    RpcClientConfig::default(),
                )
                .into(),
            };
            let program_accounts = client
                .get_program_accounts(&Pubkey::new_unique(), RpcAccountInfoConfig::default(), None)
                .await;
            let largest_accounts = client.get_largest_accounts(None).await;
            if fork_slot.is_some() {
                assert!(program_accounts.is_err());
                assert!(largest_accounts.is_err());
            } else {
                assert!(matches!(
                    program_accounts,
                    Ok(RemoteRpcResult::MethodNotSupported)
                ));
                assert!(matches!(
                    largest_accounts,
                    Ok(RemoteRpcResult::MethodNotSupported)
                ));
            }
        }
    }

    struct ScriptedHistory {
        responses: Mutex<std::collections::VecDeque<ClientResult<serde_json::Value>>>,
        requests: Mutex<Vec<(RpcRequest, serde_json::Value)>>,
    }

    #[async_trait]
    impl RpcSender for ScriptedHistory {
        async fn send(
            &self,
            request: RpcRequest,
            params: serde_json::Value,
        ) -> ClientResult<serde_json::Value> {
            self.requests.lock().unwrap().push((request, params));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected datasource request")
        }
        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats::default()
        }
        fn url(&self) -> String {
            "http://history.example".into()
        }
    }

    fn history(responses: Vec<ClientResult<serde_json::Value>>) -> ForkSender<ScriptedHistory> {
        ForkSender {
            inner: ScriptedHistory {
                responses: Mutex::new(responses.into()),
                requests: Mutex::default(),
            },
            slot: Some(FORK_SLOT),
            signature_cursors: Mutex::default(),
        }
    }

    fn signature_row(n: u8, slot: Slot) -> serde_json::Value {
        json!({"signature": Signature::from([n; 64]).to_string(), "slot": slot,
            "err": null, "memo": null, "blockTime": null, "confirmationStatus": "finalized"})
    }

    #[tokio::test]
    async fn historical_signatures_fill_pages_and_reuse_the_cutoff() {
        let rows = [
            signature_row(1, FORK_SLOT + 2),
            signature_row(2, FORK_SLOT + 1),
            signature_row(3, FORK_SLOT),
            signature_row(4, FORK_SLOT),
            signature_row(5, FORK_SLOT - 1),
        ];
        let sender = history(vec![
            Ok(json!([rows[0]])),
            Ok(json!([rows[1], rows[2]])),
            Ok(json!([rows[3], rows[4]])),
            Ok(json!([rows[2], rows[3], rows[4]])),
        ]);
        let query = json!([Pubkey::new_unique().to_string(), {"limit": 3}]);
        for _ in 0..2 {
            let result = sender
                .send(RpcRequest::GetSignaturesForAddress, query.clone())
                .await
                .unwrap();
            assert_eq!(
                result
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|row| row["signature"].clone())
                    .collect::<Vec<_>>(),
                rows[2..]
                    .iter()
                    .map(|row| row["signature"].clone())
                    .collect::<Vec<_>>()
            );
        }
        let requests = sender.inner.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[1].1[1]["before"], rows[0]["signature"]);
        assert_eq!(requests[2].1[1]["before"], rows[2]["signature"]);
        assert_eq!(requests[3].1[1]["before"], rows[1]["signature"]);
        for (_, params) in requests.iter() {
            assert_eq!(params[1]["commitment"], "finalized");
            assert_eq!(params[1]["limit"], 1000);
        }
    }

    #[tokio::test]
    async fn historical_signature_cursors_are_exclusive_and_resolved_at_the_fork() {
        let before = signature_row(1, FORK_SLOT);
        let until = signature_row(4, FORK_SLOT - 1);
        let row = signature_row(2, FORK_SLOT);
        let sender = history(vec![
            Ok(json!({"slot": FORK_SLOT})),
            Ok(json!({"slot": FORK_SLOT - 1})),
            Ok(json!([row])),
            Ok(json!([])),
        ]);
        let result = sender.send(RpcRequest::GetSignaturesForAddress,
            json!([Pubkey::new_unique().to_string(), {"before": before["signature"], "until": until["signature"]}])).await.unwrap();
        assert_eq!(result.as_array().unwrap().len(), 1);
        {
            let requests = sender.inner.requests.lock().unwrap();
            assert_eq!(requests[2].1[1]["before"], before["signature"]);
            assert_eq!(requests[3].1[1]["before"], row["signature"]);
            assert_eq!(requests[3].1[1]["until"], until["signature"]);
        }
        for resolved in [json!(null), json!({"slot": FORK_SLOT + 1})] {
            let sender = history(vec![Ok(resolved)]);
            assert!(
                sender
                    .send(
                        RpcRequest::GetSignaturesForAddress,
                        json!([Pubkey::new_unique().to_string(), {"before": before["signature"]}])
                    )
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn historical_signatures_reject_incomplete_or_disordered_history() {
        let row = signature_row(1, FORK_SLOT);
        for responses in [
            vec![Ok(json!([row])), Ok(json!([row]))],
            vec![Ok(json!([row, signature_row(2, FORK_SLOT + 1)]))],
            vec![
                Ok(json!([row])),
                Err(ClientErrorKind::Custom("unavailable".into()).into()),
            ],
        ] {
            let sender = history(responses);
            assert!(
                sender
                    .send(
                        RpcRequest::GetSignaturesForAddress,
                        json!([Pubkey::new_unique().to_string(), {"limit": 3}])
                    )
                    .await
                    .is_err()
            );
        }
        // A failed walk retains safe progress through future history for the next attempt.
        let future = signature_row(3, FORK_SLOT + 1);
        let sender = history(vec![
            Ok(json!([future])),
            Err(ClientErrorKind::Custom("unavailable".into()).into()),
            Ok(json!([row])),
        ]);
        let query = json!([Pubkey::new_unique().to_string(), {"limit": 1}]);
        assert!(
            sender
                .send(RpcRequest::GetSignaturesForAddress, query.clone())
                .await
                .is_err()
        );
        assert_eq!(
            sender
                .send(RpcRequest::GetSignaturesForAddress, query)
                .await
                .unwrap()[0]["signature"],
            row["signature"]
        );
        assert_eq!(
            sender.inner.requests.lock().unwrap()[2].1[1]["before"],
            future["signature"]
        );
    }

    #[tokio::test]
    async fn historical_block_ranges_are_bounded_and_validate_order() {
        let sender = history(vec![
            Ok(json!([FORK_SLOT - 2, FORK_SLOT])),
            Ok(json!([FORK_SLOT - 2, FORK_SLOT, FORK_SLOT + 1])),
        ]);
        assert_eq!(
            sender
                .send(
                    RpcRequest::GetBlocks,
                    json!([FORK_SLOT - 2, FORK_SLOT + 10])
                )
                .await
                .unwrap(),
            json!([FORK_SLOT - 2, FORK_SLOT])
        );
        assert_eq!(
            sender
                .send(RpcRequest::GetBlocksWithLimit, json!([FORK_SLOT - 2, 3]))
                .await
                .unwrap(),
            json!([FORK_SLOT - 2, FORK_SLOT])
        );
        assert_eq!(
            sender.inner.requests.lock().unwrap()[0].1,
            json!([FORK_SLOT - 2, FORK_SLOT, {"commitment": "finalized"}])
        );
        assert_eq!(
            sender
                .send(RpcRequest::GetBlocks, json!([FORK_SLOT + 1]))
                .await
                .unwrap(),
            json!([])
        );
        for invalid in [
            json!([FORK_SLOT + 1]),
            json!([FORK_SLOT, FORK_SLOT - 1]),
            json!([FORK_SLOT, FORK_SLOT]),
        ] {
            assert!(
                history(vec![Ok(invalid)])
                    .send(RpcRequest::GetBlocks, json!([FORK_SLOT - 2, FORK_SLOT]))
                    .await
                    .is_err()
            );
        }
    }

    fn token_page(keys: &[Pubkey], next: serde_json::Value) -> serde_json::Value {
        json!({"context": {"slot": FORK_SLOT}, "value": keys.iter().map(|key| json!({"pubkey": key.to_string()})).collect::<Vec<_>>(), "pageKey": next})
    }

    fn token_bytes() -> serde_json::Value {
        json!({"context": {"slot": FORK_SLOT}, "value": {"lamports": 2_039_280,
            "owner": spl_token_interface::id().to_string(), "data": ["AQ==", "base64"],
            "executable": false, "rentEpoch": 0}})
    }

    #[tokio::test]
    async fn historical_owner_scan_paginates_and_preserves_encoding() {
        let keys = [Pubkey::new_unique(), Pubkey::new_unique()];
        let sender = history(vec![
            Ok(token_page(&keys[..1], json!("next"))),
            Ok(token_bytes()),
            Ok(token_page(&keys[1..], json!(null))),
            Ok(token_bytes()),
        ]);
        let query = json!([Pubkey::new_unique().to_string(), {"programId": spl_token_interface::id().to_string()},
            {"encoding": "base64", "dataSlice": {"offset": 0, "length": 1}}]);
        let result = sender
            .send(RpcRequest::GetTokenAccountsByOwner, query.clone())
            .await
            .unwrap();
        assert_eq!(result["value"].as_array().unwrap().len(), 2);
        let requests = sender.inner.requests.lock().unwrap();
        assert_eq!(
            requests[0].0,
            RpcRequest::Custom {
                method: "getTokenAccountsByOwnerAtSlot"
            }
        );
        assert_eq!(
            requests[2].1[2],
            json!({"slot": FORK_SLOT, "pageLimit": 1000, "pageKey": "next"})
        );
        assert_eq!(requests[1].0, RpcRequest::GetAccountInfo);
        assert_eq!(requests[1].1[1]["dataSlice"], query[2]["dataSlice"]);
        assert_eq!(requests[1].1[1]["encoding"], "base64");
        assert_eq!(requests[1].1[1]["slot"], FORK_SLOT);
    }

    #[tokio::test]
    async fn historical_owner_scan_rejects_incomplete_or_inconsistent_pages() {
        let key = Pubkey::new_unique();
        let mut wrong_slot = token_page(&[], json!(null));
        wrong_slot["context"]["slot"] = json!(FORK_SLOT + 1);
        for responses in [
            vec![Ok(wrong_slot)],
            vec![
                Ok(token_page(&[key], json!("next"))),
                Ok(token_bytes()),
                Err(ClientErrorKind::Custom("unavailable".into()).into()),
            ],
            vec![
                Ok(token_page(&[], json!("same"))),
                Ok(token_page(&[], json!("same"))),
            ],
            vec![
                Ok(token_page(&[key], json!("next"))),
                Ok(token_bytes()),
                Ok(token_page(&[key], json!(null))),
            ],
            vec![
                Ok(token_page(&[key], json!(null))),
                Ok(json!({"context": {"slot": FORK_SLOT}, "value": null})),
            ],
        ] {
            assert!(history(responses).send(RpcRequest::GetTokenAccountsByOwner,
                json!([Pubkey::new_unique().to_string(), {"programId": spl_token_interface::id().to_string()}])).await.is_err());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn historical_block_rpcs_merge_local_slots_and_propagate_errors() {
        use crate::{
            rpc::full::{Full, SurfpoolFullRpc},
            tests::helpers::TestSetup,
        };
        let mut setup = TestSetup::new(SurfpoolFullRpc);
        setup.context.svm_locker.with_svm_writer(|svm| {
            svm.genesis_slot = FORK_SLOT;
            svm.latest_epoch_info.absolute_slot = FORK_SLOT + 3;
        });
        setup.context.remote_rpc_client = Some(SurfnetRemoteClient {
            fork_slot: Some(FORK_SLOT),
            client: RpcClient::new_sender(
                history(vec![
                    Ok(json!([FORK_SLOT - 2, FORK_SLOT - 1])),
                    Ok(json!([
                        FORK_SLOT - 2,
                        FORK_SLOT - 1,
                        FORK_SLOT,
                        FORK_SLOT + 1
                    ])),
                    Ok(json!(1_600_000_005)),
                    Err(ClientErrorKind::Custom("unavailable".into()).into()),
                ]),
                RpcClientConfig::default(),
            )
            .into(),
        });
        assert_eq!(
            setup
                .rpc
                .get_blocks(Some(setup.context.clone()), FORK_SLOT - 2, None, None)
                .await
                .unwrap(),
            (FORK_SLOT - 2..=FORK_SLOT + 3).collect::<Vec<_>>()
        );
        // This spans over 500,000 slots but asks for only four blocks.
        assert_eq!(
            setup
                .rpc
                .get_blocks_with_limit(Some(setup.context.clone()), 0, 4, None)
                .await
                .unwrap(),
            (FORK_SLOT - 2..=FORK_SLOT + 1).collect::<Vec<_>>()
        );
        assert_eq!(
            setup
                .rpc
                .get_block_time(Some(setup.context.clone()), FORK_SLOT - 1)
                .await
                .unwrap(),
            Some(1_600_000_005)
        );
        assert!(
            setup
                .rpc
                .get_block_time(Some(setup.context.clone()), FORK_SLOT + 1)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            setup
                .rpc
                .get_blocks(Some(setup.context), FORK_SLOT - 2, None, None)
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn historical_owner_scan_merges_local_writes() {
        use solana_account::Account;
        use solana_account_decoder::UiAccountEncoding;
        use solana_program_pack::Pack;
        use spl_token_interface::state::{Account as SplAccount, AccountState};
        let (svm, _, _) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let keys: Vec<_> = (0..3).map(|_| Pubkey::new_unique()).collect();
        let make_account = |owner, amount| {
            let mut data = vec![0; SplAccount::LEN];
            SplAccount {
                owner,
                mint,
                amount,
                state: AccountState::Initialized,
                ..Default::default()
            }
            .pack_into_slice(&mut data);
            Account {
                lamports: 2_039_280,
                data,
                owner: spl_token_interface::id(),
                ..Default::default()
            }
        };
        locker.with_svm_writer(|svm| {
            svm.latest_epoch_info.absolute_slot = FORK_SLOT + 10;
            svm.set_account(&keys[0], make_account(owner, 42)).unwrap();
            svm.set_account(&keys[2], make_account(owner, 5)).unwrap();
        });
        let remote = SurfnetRemoteClient {
            fork_slot: Some(FORK_SLOT),
            client: RpcClient::new_sender(
                history(vec![
                    Ok(token_page(&keys[..2], json!(null))),
                    Ok(token_bytes()),
                    Ok(token_bytes()),
                ]),
                RpcClientConfig::default(),
            )
            .into(),
        };
        let accounts = locker
            .get_token_accounts_by_owner(
                &Some(remote),
                owner,
                &TokenAccountsFilter::ProgramId(spl_token_interface::id()),
                &RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    min_context_slot: Some(FORK_SLOT + 10),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(accounts.slot, FORK_SLOT + 10);
        let found: HashSet<_> = accounts
            .inner
            .iter()
            .map(|account| account.pubkey.clone())
            .collect();
        assert_eq!(
            found,
            [keys[0], keys[1], keys[2]]
                .iter()
                .map(ToString::to_string)
                .collect()
        );
        let updated = accounts
            .inner
            .iter()
            .find(|account| account.pubkey == keys[0].to_string())
            .unwrap()
            .account
            .data
            .decode()
            .unwrap();
        assert_eq!(SplAccount::unpack(&updated).unwrap().amount, 42);
    }

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
            fork_slot: None,
            client: RpcClient::new_sender(
                ReturnsNull {
                    requests: Arc::clone(&requests),
                },
                RpcClientConfig::default(),
            )
            .into(),
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
            fork_slot: None,
            client: RpcClient::new_sender(ReturnsError, RpcClientConfig::default()).into(),
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

    struct RecordsRequests {
        requests: Arc<Mutex<Vec<(RpcRequest, serde_json::Value)>>>,
    }

    #[async_trait]
    impl RpcSender for RecordsRequests {
        async fn send(
            &self,
            request: RpcRequest,
            params: serde_json::Value,
        ) -> ClientResult<serde_json::Value> {
            self.requests
                .lock()
                .expect("request recorder mutex should not be poisoned")
                .push((request, params));
            Ok(json!({
                "context": { "slot": 1 },
                "value": [null],
            }))
        }

        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats::default()
        }

        fn url(&self) -> String {
            "http://records.example".to_string()
        }
    }

    #[tokio::test]
    async fn multiple_account_fetch_uses_the_requested_commitment() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let client = SurfnetRemoteClient {
            fork_slot: None,
            client: RpcClient::new_sender(
                RecordsRequests {
                    requests: Arc::clone(&requests),
                },
                RpcClientConfig::default(),
            )
            .into(),
        };

        let pubkey = Pubkey::new_unique();
        client
            .get_multiple_accounts(&[pubkey], CommitmentConfig::confirmed())
            .await
            .expect("remote account fetch should succeed");

        let requests = requests
            .lock()
            .expect("request recorder mutex should not be poisoned");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, RpcRequest::GetMultipleAccounts);
        assert_eq!(requests[0].1[1]["commitment"], "confirmed");
    }

    #[test]
    fn cloned_remote_clients_share_the_rpc_client() {
        let client = SurfnetRemoteClient::new("http://127.0.0.1:8899");
        let cloned_client = client.clone();

        assert!(Arc::ptr_eq(&client.client, &cloned_client.client));
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

    struct RejectsUnknownMint;

    #[async_trait]
    impl RpcSender for RejectsUnknownMint {
        async fn send(
            &self,
            _request: RpcRequest,
            _params: serde_json::Value,
        ) -> ClientResult<serde_json::Value> {
            Err(ClientErrorKind::RpcError(RpcError::RpcResponseError {
                code: -32602,
                message:
                    "Error getting token program id and mint: Invalid param: could not find mint"
                        .to_string(),
                data: RpcResponseErrorData::Empty,
            })
            .into())
        }

        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats::default()
        }

        fn url(&self) -> String {
            "http://rejects-unknown-mint.example".to_string()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_mint_unknown_to_the_datasource_has_no_remote_token_accounts() {
        let client = SurfnetRemoteClient {
            fork_slot: None,
            client: RpcClient::new_sender(RejectsUnknownMint, RpcClientConfig::default()).into(),
        };

        let accounts = client
            .get_token_accounts_by_owner(
                Pubkey::new_unique(),
                &TokenAccountsFilter::Mint(Pubkey::new_unique()),
                &RpcAccountInfoConfig::default(),
            )
            .await
            .expect("a rejected filter is an empty remote answer, not a failure");

        assert!(accounts.is_empty());
        // An error during a historical walk may occur after earlier pages succeeded.
        let historical = SurfnetRemoteClient {
            fork_slot: Some(FORK_SLOT),
            ..client
        };
        assert!(
            historical
                .get_token_accounts_by_owner(
                    Pubkey::new_unique(),
                    &TokenAccountsFilter::Mint(Pubkey::new_unique()),
                    &RpcAccountInfoConfig::default()
                )
                .await
                .is_err()
        );
    }

    struct RejectsParams;

    #[async_trait]
    impl RpcSender for RejectsParams {
        async fn send(
            &self,
            _request: RpcRequest,
            _params: serde_json::Value,
        ) -> ClientResult<serde_json::Value> {
            Err(ClientErrorKind::RpcError(RpcError::RpcResponseError {
                code: -32602,
                message: "Invalid param: unsupported encoding".to_string(),
                data: RpcResponseErrorData::Empty,
            })
            .into())
        }

        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats::default()
        }

        fn url(&self) -> String {
            "http://rejects-params.example".to_string()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn any_other_invalid_params_rejection_is_still_an_error() {
        let client = SurfnetRemoteClient {
            fork_slot: None,
            client: RpcClient::new_sender(RejectsParams, RpcClientConfig::default()).into(),
        };

        let error = client
            .get_token_accounts_by_owner(
                Pubkey::new_unique(),
                &TokenAccountsFilter::Mint(Pubkey::new_unique()),
                &RpcAccountInfoConfig::default(),
            )
            .await
            .expect_err("a rejected encoding is a request error, not an empty answer");

        assert!(error.to_string().contains("unsupported encoding"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_mint_answer_on_a_program_filter_is_still_an_error() {
        let client = SurfnetRemoteClient {
            fork_slot: None,
            client: RpcClient::new_sender(RejectsUnknownMint, RpcClientConfig::default()).into(),
        };

        let error = client
            .get_token_accounts_by_owner(
                Pubkey::new_unique(),
                &TokenAccountsFilter::ProgramId(Pubkey::new_unique()),
                &RpcAccountInfoConfig::default(),
            )
            .await
            .expect_err("only a Mint filter can be answered by an unknown mint");

        assert!(error.to_string().contains("could not find mint"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_accounts_provider_failure_is_still_an_error() {
        let client = SurfnetRemoteClient {
            fork_slot: None,
            client: RpcClient::new_sender(ReturnsError, RpcClientConfig::default()).into(),
        };

        let error = client
            .get_token_accounts_by_owner(
                Pubkey::new_unique(),
                &TokenAccountsFilter::Mint(Pubkey::new_unique()),
                &RpcAccountInfoConfig::default(),
            )
            .await
            .expect_err("a provider failure must not be reported as an empty answer");

        assert!(
            error
                .to_string()
                .contains("Failed to get token accounts by owner")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fork_born_mint_keeps_its_local_token_accounts() {
        use solana_account::Account;
        use solana_account_decoder::UiAccountEncoding;
        use solana_program_pack::Pack;
        use spl_token_interface::state::{Account as TokenAccount, AccountState};

        let (svm, _, _) = SurfnetSvm::default();
        let locker = SurfnetSvmLocker::new(svm);
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let token_account_pubkey = Pubkey::new_unique();
        let mut data = vec![0u8; TokenAccount::LEN];
        TokenAccount {
            mint,
            owner,
            amount: 42,
            state: AccountState::Initialized,
            ..Default::default()
        }
        .pack_into_slice(&mut data);
        locker.with_svm_writer(|svm| {
            svm.set_account(
                &token_account_pubkey,
                Account {
                    lamports: 2_039_280,
                    data,
                    owner: spl_token_interface::id(),
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        });
        let remote = SurfnetRemoteClient {
            fork_slot: None,
            client: RpcClient::new_sender(RejectsUnknownMint, RpcClientConfig::default()).into(),
        };
        let config = RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            ..RpcAccountInfoConfig::default()
        };

        let accounts = locker
            .get_token_accounts_by_owner(
                &Some(remote),
                owner,
                &TokenAccountsFilter::Mint(mint),
                &config,
            )
            .await
            .expect("local token accounts survive a datasource that has never seen the mint")
            .inner;

        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].pubkey, token_account_pubkey.to_string());
    }
}
