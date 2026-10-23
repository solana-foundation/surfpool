//! `getTransactionsForAddress` options a historical surfnet serves from SVM Replay's
//! provider-index query (see its backend archive): oldest-first history and token-account
//! filters. SVM Replay cuts the provider's pages at the fork position and the provider
//! applies the filters; Surfpool merges the fork's own transactions in order and encodes the
//! archived entries as its own.

use serde_json::{Map, Value, json};
use solana_pubkey::Pubkey;
use solana_transaction_error::TransactionError;
use solana_transaction_status::{
    TransactionConfirmationStatus as SolanaTransactionConfirmationStatus, UiTransactionEncoding,
    VersionedTransactionWithStatusMeta,
};

use super::{SurfnetSvmLocker, SvmAccessContext, historical_archive};
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    rpc::full::{
        ComparisonFilter, RpcGetTransactionsForAddressConfig, RpcTransactionForAddressEntry,
        RpcTransactionForAddressFullInfo, RpcTransactionForAddressSignatureInfo,
        RpcTransactionsForAddressResult, SortOrder, TransactionsForAddressDetails,
    },
    surfnet::backend::ArchiveReader,
};

/// Provider pages one request reads before returning what it has with a continuation token.
const PAGES: usize = 10;

type Context = SvmAccessContext<RpcTransactionsForAddressResult>;

/// A provider page token: `slot:position`.
fn cursor(token: &str) -> Option<(u64, u64)> {
    let (slot, index) = token.split_once(':')?;
    Some((slot.parse().ok()?, index.parse().ok()?))
}

/// The bounds a comparison filter sets, as the provider takes them; none is no filter.
fn bounds<T: serde::Serialize>(filter: Option<&ComparisonFilter<T>>) -> Value {
    let Some(filter) = filter else {
        return Value::Null;
    };
    let mut bounds = Map::new();
    for (op, bound) in [
        ("gte", &filter.gte),
        ("gt", &filter.gt),
        ("lte", &filter.lte),
        ("lt", &filter.lt),
    ] {
        if let Some(bound) = bound {
            bounds.insert(op.into(), json!(bound));
        }
    }
    match bounds.is_empty() {
        true => Value::Null,
        false => Value::Object(bounds),
    }
}

/// The page size and detail mode `getTransactionsForAddress` serves.
fn page_size(config: &RpcGetTransactionsForAddressConfig) -> (usize, bool) {
    let full =
        config.transaction_details.unwrap_or_default() == TransactionsForAddressDetails::Full;
    let max = if full { 100 } else { 1000 };
    (config.limit.unwrap_or(max).min(max), full)
}

impl SurfnetSvmLocker {
    /// One page of archived history from SVM Replay, with the provider's next page token.
    fn archived_address_page(
        &self,
        reader: &ArchiveReader,
        pubkey: &Pubkey,
        config: &RpcGetTransactionsForAddressConfig,
        order: SortOrder,
        limit: usize,
        token: Option<String>,
    ) -> SurfpoolResult<(Vec<Value>, Option<String>)> {
        let filters = config.filters.as_ref();
        let page = reader.transactions_for_address(
            pubkey,
            order,
            filters.and_then(|f| f.token_accounts).unwrap_or_default(),
            filters.and_then(|f| f.status).unwrap_or_default(),
            limit,
            token,
            bounds(filters.and_then(|f| f.slot.as_ref())),
            bounds(filters.and_then(|f| f.block_time.as_ref())),
        )?;
        let malformed =
            || SurfpoolError::historical("SOURCE_INTEGRITY", "archived history is malformed", None);
        // More entries than asked for is out of contract (SVM Replay refuses it too).
        let entries = page["data"]
            .as_array()
            .filter(|entries| entries.len() <= limit)
            .ok_or_else(malformed)?
            .clone();
        let next = match &page["paginationToken"] {
            Value::Null => None,
            Value::String(next) => Some(next.clone()),
            _ => return Err(malformed()),
        };
        Ok((entries, next))
    }

    /// An archived history entry as Surfpool's own: signature metadata, or the archived
    /// transaction encoded as requested.
    fn archived_entry(
        &self,
        reader: &ArchiveReader,
        entry: &Value,
        full: bool,
        config: &RpcGetTransactionsForAddressConfig,
    ) -> SurfpoolResult<RpcTransactionForAddressEntry> {
        let malformed = || {
            SurfpoolError::historical(
                "SOURCE_INTEGRITY",
                "an archived history entry is malformed",
                None,
            )
        };
        let signature = entry["signature"]
            .as_str()
            .ok_or_else(malformed)?
            .to_owned();
        let slot = entry["slot"].as_u64().ok_or_else(malformed)?;
        let transaction_index = entry["transactionIndex"]
            .as_u64()
            .map(|index| index as usize);
        if full {
            let record = reader.transaction(&signature)?;
            let (transaction, block_time) = historical_archive::archived_transaction(&record)?;
            return Ok(RpcTransactionForAddressEntry::Full(Box::new(
                RpcTransactionForAddressFullInfo {
                    slot,
                    transaction_index,
                    block_time,
                    transaction: VersionedTransactionWithStatusMeta::from(transaction).encode(
                        config.encoding.unwrap_or(UiTransactionEncoding::Json),
                        config.max_supported_transaction_version,
                        true,
                    )?,
                },
            )));
        }
        let err: Option<TransactionError> =
            serde_json::from_value(entry["err"].clone()).map_err(|_| malformed())?;
        Ok(RpcTransactionForAddressEntry::Signature(
            RpcTransactionForAddressSignatureInfo {
                signature,
                slot,
                transaction_index,
                err,
                memo: entry["memo"].as_str().map(str::to_owned),
                block_time: entry["blockTime"].as_i64(),
                confirmation_status: Some(SolanaTransactionConfirmationStatus::Finalized),
            },
        ))
    }

    /// Oldest first: the archived history, then the fork's own transactions. A page token is
    /// the provider's `slot:position` inside the archive, the fork position once the archive
    /// is done, or a local signature among the fork's transactions.
    pub(super) fn historical_history_ascending(
        &self,
        reader: &ArchiveReader,
        pubkey: &Pubkey,
        config: &RpcGetTransactionsForAddressConfig,
    ) -> SurfpoolResult<Context> {
        let (limit, full) = page_size(config);
        let boundary = self
            .with_svm_reader(|svm| svm.historical_boundary)
            .ok_or_else(|| SurfpoolError::internal("a historical fork keeps its boundary"))?;
        let token = config.pagination_token.clone();
        let mut data = Vec::new();
        let mut local_token = token.clone().filter(|token| cursor(token).is_none());
        let archived = match token.as_deref().map(cursor) {
            None => Some(None),
            Some(Some(at)) if at < boundary => Some(token),
            Some(_) => None,
        };
        if let Some(mut next) = archived {
            let mut done = false;
            for _ in 0..PAGES {
                let (entries, more) = self.archived_address_page(
                    reader,
                    pubkey,
                    config,
                    SortOrder::Asc,
                    limit.saturating_sub(data.len()),
                    next,
                )?;
                for entry in &entries {
                    data.push(self.archived_entry(reader, entry, full, config)?);
                }
                next = more;
                done = next.is_none();
                if done || data.len() >= limit {
                    break;
                }
            }
            if !done || data.len() >= limit {
                let pagination_token = match done {
                    true => Some(format!("{}:{}", boundary.0, boundary.1)),
                    false => next,
                };
                let page = RpcTransactionsForAddressResult {
                    data,
                    pagination_token,
                };
                return Ok(self
                    .with_contextualized_svm_reader(|_| ())
                    .with_new_value(page));
            }
            local_token = None;
        }
        let mut local = config.clone();
        local.limit = Some(limit.saturating_sub(data.len()));
        local.pagination_token = local_token;
        let local = self.get_transactions_for_address_local(pubkey, &local)?;
        let pagination_token = local.inner.pagination_token.clone();
        let mut local_data = local.inner.data;
        data.append(&mut local_data);
        let page = RpcTransactionsForAddressResult {
            data,
            pagination_token,
        };
        Ok(SvmAccessContext::new(
            local.slot,
            local.latest_epoch_info,
            local.latest_blockhash,
            page,
        ))
    }

    /// Newest first with a token-account filter: after the fork's own transactions in
    /// `result`, the archived history the provider filtered by its own historical ownership
    /// records. Its page tokens are the provider's `slot:position`.
    pub(super) fn historical_history_token_accounts(
        &self,
        reader: &ArchiveReader,
        pubkey: &Pubkey,
        config: &RpcGetTransactionsForAddressConfig,
        mut result: Context,
        archived_token: Option<String>,
    ) -> SurfpoolResult<Context> {
        let (limit, full) = page_size(config);
        if result.inner.data.len() >= limit {
            return Ok(result);
        }
        if archived_token
            .as_deref()
            .is_some_and(|token| cursor(token).is_none())
        {
            return Err(SurfpoolError::invalid_params(
                "a token-account history page token is a provider slot:position",
            ));
        }
        let mut next = archived_token;
        for _ in 0..PAGES {
            if result.inner.data.len() >= limit {
                break;
            }
            let (entries, more) = self.archived_address_page(
                reader,
                pubkey,
                config,
                SortOrder::Desc,
                limit.saturating_sub(result.inner.data.len()),
                next,
            )?;
            for entry in &entries {
                result
                    .inner
                    .data
                    .push(self.archived_entry(reader, entry, full, config)?);
            }
            next = more;
            if next.is_none() {
                break;
            }
        }
        result.inner.pagination_token = next;
        Ok(result)
    }
}
