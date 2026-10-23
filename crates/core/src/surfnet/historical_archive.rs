//! History before a historical surfnet's fork position. SVM Replay supplies every archived
//! fact (the boundary block's prefix, earlier transactions, blocks and signatures), already
//! cut at the position; Surfpool keeps none of it and encodes it exactly as its own history.

use base64::{Engine, prelude::BASE64_STANDARD};
use serde_json::Value;
use solana_clock::UnixTimestamp;
use solana_message::{compiled_instruction::CompiledInstruction, v0::LoadedAddresses};
use solana_pubkey::Pubkey;
use solana_rpc_client_api::config::{RpcBlockConfig, RpcTransactionConfig};
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_context::transaction::TransactionReturnData;
use solana_transaction_status::{
    BlockEncodingOptions, ConfirmedBlock, EncodedConfirmedTransactionWithStatusMeta,
    InnerInstruction, InnerInstructions, Reward, TransactionStatusMeta, TransactionTokenBalance,
    UiConfirmedBlock, UiInstruction, UiTransactionEncoding, UiTransactionStatusMeta,
    UiTransactionTokenBalance, VersionedTransactionWithStatusMeta,
    option_serializer::OptionSerializer,
};

use crate::{
    error::{SurfpoolError, SurfpoolResult},
    types::TransactionWithStatusMeta,
};

fn malformed(what: &str) -> SurfpoolError {
    SurfpoolError::historical(
        "SOURCE_INTEGRITY",
        &format!("archived {what} is malformed"),
        None,
    )
}

fn option<T>(value: OptionSerializer<T>) -> Option<T> {
    match value {
        OptionSerializer::Some(value) => Some(value),
        OptionSerializer::None | OptionSerializer::Skip => None,
    }
}

/// An archived transaction record (SVM Replay's wire bytes and the archived metadata) as
/// Surfpool holds its own transactions, with its block time.
pub fn archived_transaction(
    record: &Value,
) -> SurfpoolResult<(TransactionWithStatusMeta, Option<UnixTimestamp>)> {
    let slot = record["slot"]
        .as_u64()
        .ok_or_else(|| malformed("transaction slot"))?;
    let wire = record["transactionBase64"]
        .as_str()
        .and_then(|wire| BASE64_STANDARD.decode(wire).ok())
        .ok_or_else(|| malformed("transaction"))?;
    let transaction: VersionedTransaction =
        wincode::deserialize(&wire).map_err(|_| malformed("transaction"))?;
    let meta: UiTransactionStatusMeta = serde_json::from_value(record["meta"].clone())
        .map_err(|_| malformed("transaction metadata"))?;
    let block_time = match &record["blockTime"] {
        Value::Null => None,
        value => Some(value.as_i64().ok_or_else(|| malformed("block time"))?),
    };
    let transaction = TransactionWithStatusMeta {
        slot,
        transaction,
        meta: status_meta(meta)?,
    };
    Ok((transaction, block_time))
}

/// The internal status metadata an archived (JSON-encoded) one stands for.
fn status_meta(ui: UiTransactionStatusMeta) -> SurfpoolResult<TransactionStatusMeta> {
    let balances = |balances: OptionSerializer<Vec<UiTransactionTokenBalance>>| {
        option(balances).map(|balances| {
            balances
                .into_iter()
                .map(|balance| TransactionTokenBalance {
                    account_index: balance.account_index,
                    mint: balance.mint,
                    ui_token_amount: balance.ui_token_amount,
                    owner: option(balance.owner).unwrap_or_default(),
                    program_id: option(balance.program_id).unwrap_or_default(),
                })
                .collect()
        })
    };
    let instruction = |instruction: UiInstruction| match instruction {
        UiInstruction::Compiled(compiled) => Ok(InnerInstruction {
            instruction: CompiledInstruction {
                program_id_index: compiled.program_id_index,
                accounts: compiled.accounts,
                data: bs58::decode(&compiled.data)
                    .into_vec()
                    .map_err(|_| malformed("inner instruction data"))?,
            },
            stack_height: compiled.stack_height,
        }),
        UiInstruction::Parsed(_) => Err(malformed("inner instruction encoding")),
    };
    let inner_instructions = option(ui.inner_instructions)
        .map(|groups| {
            groups
                .into_iter()
                .map(|group| {
                    Ok(InnerInstructions {
                        index: group.index,
                        instructions: group
                            .instructions
                            .into_iter()
                            .map(instruction)
                            .collect::<SurfpoolResult<_>>()?,
                    })
                })
                .collect::<SurfpoolResult<Vec<_>>>()
        })
        .transpose()?;
    let pubkeys = |keys: Vec<String>| {
        keys.iter()
            .map(|key| {
                key.parse::<Pubkey>()
                    .map_err(|_| malformed("loaded address"))
            })
            .collect::<SurfpoolResult<Vec<_>>>()
    };
    let loaded_addresses = match option(ui.loaded_addresses) {
        Some(loaded) => LoadedAddresses {
            writable: pubkeys(loaded.writable)?,
            readonly: pubkeys(loaded.readonly)?,
        },
        None => LoadedAddresses::default(),
    };
    let return_data = option(ui.return_data)
        .map(|data| {
            Ok::<_, SurfpoolError>(TransactionReturnData {
                program_id: data
                    .program_id
                    .parse()
                    .map_err(|_| malformed("return data program"))?,
                data: BASE64_STANDARD
                    .decode(data.data.0)
                    .map_err(|_| malformed("return data"))?,
            })
        })
        .transpose()?;
    Ok(TransactionStatusMeta {
        status: match ui.err {
            Some(error) => Err(error.into()),
            None => Ok(()),
        },
        fee: ui.fee,
        pre_balances: ui.pre_balances,
        post_balances: ui.post_balances,
        inner_instructions,
        log_messages: option(ui.log_messages),
        pre_token_balances: balances(ui.pre_token_balances),
        post_token_balances: balances(ui.post_token_balances),
        rewards: option(ui.rewards),
        loaded_addresses,
        return_data,
        compute_units_consumed: option(ui.compute_units_consumed),
        cost_units: option(ui.cost_units),
    })
}

/// An archived transaction encoded as `getTransaction` encodes Surfpool's own.
pub fn encoded_transaction(
    record: &Value,
    config: &RpcTransactionConfig,
) -> SurfpoolResult<EncodedConfirmedTransactionWithStatusMeta> {
    let (transaction, block_time) = archived_transaction(record)?;
    let slot = transaction.slot;
    let transaction_index = match &record["transactionIndex"] {
        Value::Null => None,
        index => Some(
            index
                .as_u64()
                .and_then(|index| u32::try_from(index).ok())
                .ok_or_else(|| malformed("transaction index"))?,
        ),
    };
    Ok(EncodedConfirmedTransactionWithStatusMeta {
        slot,
        transaction: VersionedTransactionWithStatusMeta::from(transaction).encode(
            config.encoding.unwrap_or(UiTransactionEncoding::Json),
            config.max_supported_transaction_version,
            true,
        )?,
        block_time,
        transaction_index,
    })
}

/// A block of the fork's history encoded as `getBlock` encodes Surfpool's own: `header`
/// carries the archived (or fork-local) header fields, `transactions` the block's
/// transactions in order.
pub fn encoded_block(
    header: &Value,
    blockhash: String,
    transactions: Vec<TransactionWithStatusMeta>,
    config: &RpcBlockConfig,
) -> SurfpoolResult<UiConfirmedBlock> {
    let text = |field: &str| {
        header[field]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| malformed(field))
    };
    let rewards: Vec<Reward> = match &header["rewards"] {
        Value::Null => vec![],
        rewards => {
            serde_json::from_value(rewards.clone()).map_err(|_| malformed("block rewards"))?
        }
    };
    // Optional header fields: null is an absent value, anything else must have its type.
    let optional = |field: &str| -> SurfpoolResult<Option<&Value>> {
        match &header[field] {
            Value::Null => Ok(None),
            value if value.is_u64() || (field == "blockTime" && value.is_i64()) => Ok(Some(value)),
            _ => Err(malformed(field)),
        }
    };
    let num_partitions = optional("numRewardPartitions")?.and_then(Value::as_u64);
    let block_time = optional("blockTime")?.and_then(Value::as_i64);
    let block_height = optional("blockHeight")?.and_then(Value::as_u64);
    Ok(ConfirmedBlock {
        previous_blockhash: text("previousBlockhash")?,
        blockhash,
        parent_slot: header["parentSlot"]
            .as_u64()
            .ok_or_else(|| malformed("parent slot"))?,
        transactions: transactions
            .into_iter()
            .map(|transaction| {
                solana_transaction_status::TransactionWithStatusMeta::Complete(transaction.into())
            })
            .collect(),
        rewards,
        num_partitions,
        block_time,
        block_height,
    }
    .encode_with_options(
        config.encoding.unwrap_or(UiTransactionEncoding::Json),
        BlockEncodingOptions {
            transaction_details: config.transaction_details.unwrap_or_default(),
            show_rewards: config.rewards.unwrap_or(true),
            max_supported_transaction_version: config.max_supported_transaction_version,
        },
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archived_v1_uses_the_canonical_wire_decoder() {
        let message = solana_message::v1::Message {
            header: solana_message::MessageHeader {
                num_required_signatures: 1,
                ..Default::default()
            },
            account_keys: vec![Pubkey::new_unique()],
            ..Default::default()
        };
        let tx = VersionedTransaction {
            signatures: vec![Default::default()],
            message: solana_message::VersionedMessage::V1(message),
        };
        let ui: UiTransactionStatusMeta = TransactionStatusMeta::default().into();
        let record = serde_json::json!({
            "slot": 1, "blockTime": 42, "meta": ui,
            "transactionBase64": BASE64_STANDARD.encode(wincode::serialize(&tx).unwrap())
        });
        let (decoded, time) = archived_transaction(&record).unwrap();
        assert_eq!(decoded.transaction, tx);
        assert_eq!(time, Some(42));
    }
}
