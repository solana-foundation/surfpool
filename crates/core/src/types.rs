use std::{collections::HashSet, vec};

use agave_reserved_account_keys::ReservedAccountKeys;
use base64::{Engine, prelude::BASE64_STANDARD};
use bytemuck::{Pod, bytes_of, from_bytes};
use chrono::Utc;
use litesvm::types::TransactionMetadata;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use solana_account::Account;
use solana_account_decoder::{
    parse_account_data::{AccountAdditionalDataV3, SplTokenAdditionalDataV2},
    parse_token::UiTokenAmount,
};
use solana_clock::{Epoch, Slot};
use solana_hash::Hash;
use solana_message::{
    AccountKeys, VersionedMessage,
    v0::{LoadedAddresses, LoadedMessage, MessageAddressTableLookup},
};
use solana_program_option::COption;
use solana_program_pack::Pack;
use solana_pubkey::Pubkey;
use solana_transaction::{
    sanitized::SanitizedTransaction,
    versioned::{TransactionVersion, VersionedTransaction},
};
use solana_transaction_context::TransactionReturnData;
use solana_transaction_error::TransactionError;
use solana_transaction_status::{
    Encodable, EncodableWithMeta, EncodeError, EncodedTransaction,
    EncodedTransactionWithStatusMeta, InnerInstruction, InnerInstructions, Reward,
    TransactionBinaryEncoding, TransactionConfirmationStatus, TransactionStatus,
    TransactionStatusMeta, TransactionTokenBalance, UiAccountsList, UiLoadedAddresses,
    UiTransaction, UiTransactionEncoding, UiTransactionStatusMeta,
    option_serializer::OptionSerializer,
    parse_accounts::{parse_legacy_message_accounts, parse_v0_message_accounts},
    parse_ui_inner_instructions,
};
use spl_token_2022_interface::extension::{
    StateWithExtensions, interest_bearing_mint::InterestBearingConfig,
    scaled_ui_amount::ScaledUiAmountConfig,
};
use txtx_addon_kit::indexmap::IndexMap;

use crate::{
    error::{SurfpoolError, SurfpoolResult},
    surfnet::locker::{format_ui_amount, format_ui_amount_string},
};

/// Helper function to serialize a Pod type to base64
fn serialize_pod_to_base64<T: Pod>(value: &T) -> String {
    BASE64_STANDARD.encode(bytes_of(value))
}

/// Helper function to deserialize a Pod type from base64
fn deserialize_pod_from_base64<T: Pod + Copy>(encoded: &str) -> Result<T, String> {
    let bytes = BASE64_STANDARD
        .decode(encoded)
        .map_err(|e| format!("base64 decode error: {}", e))?;
    if bytes.len() != std::mem::size_of::<T>() {
        return Err(format!(
            "Invalid byte length: expected {}, got {}",
            std::mem::size_of::<T>(),
            bytes.len()
        ));
    }
    Ok(*from_bytes::<T>(&bytes))
}

/// Serializable version of SplTokenAdditionalDataV2
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableSplTokenAdditionalData {
    pub decimals: u8,
    /// InterestBearingConfig serialized as base64, paired with unix timestamp
    pub interest_bearing_config: Option<(String, i64)>,
    /// ScaledUiAmountConfig serialized as base64, paired with unix timestamp
    pub scaled_ui_amount_config: Option<(String, i64)>,
}

impl From<SplTokenAdditionalDataV2> for SerializableSplTokenAdditionalData {
    fn from(data: SplTokenAdditionalDataV2) -> Self {
        Self {
            decimals: data.decimals,
            interest_bearing_config: data
                .interest_bearing_config
                .map(|(config, ts)| (serialize_pod_to_base64(&config), ts)),
            scaled_ui_amount_config: data
                .scaled_ui_amount_config
                .map(|(config, ts)| (serialize_pod_to_base64(&config), ts)),
        }
    }
}

impl TryFrom<SerializableSplTokenAdditionalData> for SplTokenAdditionalDataV2 {
    type Error = String;

    fn try_from(data: SerializableSplTokenAdditionalData) -> Result<Self, Self::Error> {
        Ok(Self {
            decimals: data.decimals,
            interest_bearing_config: data
                .interest_bearing_config
                .map(|(encoded, ts)| {
                    deserialize_pod_from_base64::<InterestBearingConfig>(&encoded)
                        .map(|config| (config, ts))
                })
                .transpose()?,
            scaled_ui_amount_config: data
                .scaled_ui_amount_config
                .map(|(encoded, ts)| {
                    deserialize_pod_from_base64::<ScaledUiAmountConfig>(&encoded)
                        .map(|config| (config, ts))
                })
                .transpose()?,
        })
    }
}

/// Serializable version of AccountAdditionalDataV3
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableAccountAdditionalData {
    pub spl_token_additional_data: Option<SerializableSplTokenAdditionalData>,
}

impl From<AccountAdditionalDataV3> for SerializableAccountAdditionalData {
    fn from(data: AccountAdditionalDataV3) -> Self {
        Self {
            spl_token_additional_data: data.spl_token_additional_data.map(Into::into),
        }
    }
}

impl TryFrom<SerializableAccountAdditionalData> for AccountAdditionalDataV3 {
    type Error = String;

    fn try_from(data: SerializableAccountAdditionalData) -> Result<Self, Self::Error> {
        Ok(Self {
            spl_token_additional_data: data
                .spl_token_additional_data
                .map(TryInto::try_into)
                .transpose()?,
        })
    }
}

/// Serializable version of TransactionTokenBalance
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableTransactionTokenBalance {
    pub account_index: u8,
    pub mint: String,
    pub ui_token_amount: UiTokenAmount,
    pub owner: String,
    pub program_id: String,
}

impl From<TransactionTokenBalance> for SerializableTransactionTokenBalance {
    fn from(ttb: TransactionTokenBalance) -> Self {
        Self {
            account_index: ttb.account_index,
            mint: ttb.mint,
            ui_token_amount: ttb.ui_token_amount,
            owner: ttb.owner,
            program_id: ttb.program_id,
        }
    }
}

impl From<SerializableTransactionTokenBalance> for TransactionTokenBalance {
    fn from(sttb: SerializableTransactionTokenBalance) -> Self {
        Self {
            account_index: sttb.account_index,
            mint: sttb.mint,
            ui_token_amount: sttb.ui_token_amount,
            owner: sttb.owner,
            program_id: sttb.program_id,
        }
    }
}

/// Serializable version of TransactionStatusMeta
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableTransactionStatusMeta {
    pub status: Result<(), TransactionError>,
    pub fee: u64,
    pub pre_balances: Vec<u64>,
    pub post_balances: Vec<u64>,
    pub inner_instructions: Option<Vec<InnerInstructions>>,
    pub log_messages: Option<Vec<String>>,
    pub pre_token_balances: Option<Vec<SerializableTransactionTokenBalance>>,
    pub post_token_balances: Option<Vec<SerializableTransactionTokenBalance>>,
    pub rewards: Option<Vec<Reward>>,
    pub loaded_addresses: LoadedAddresses,
    pub return_data: Option<TransactionReturnData>,
    pub compute_units_consumed: Option<u64>,
    pub cost_units: Option<u64>,
}

impl From<TransactionStatusMeta> for SerializableTransactionStatusMeta {
    fn from(meta: TransactionStatusMeta) -> Self {
        Self {
            status: meta.status,
            fee: meta.fee,
            pre_balances: meta.pre_balances,
            post_balances: meta.post_balances,
            inner_instructions: meta.inner_instructions,
            log_messages: meta.log_messages,
            pre_token_balances: meta
                .pre_token_balances
                .map(|v| v.into_iter().map(Into::into).collect()),
            post_token_balances: meta
                .post_token_balances
                .map(|v| v.into_iter().map(Into::into).collect()),
            rewards: meta.rewards,
            loaded_addresses: meta.loaded_addresses,
            return_data: meta.return_data,
            compute_units_consumed: meta.compute_units_consumed,
            cost_units: meta.cost_units,
        }
    }
}

impl From<SerializableTransactionStatusMeta> for TransactionStatusMeta {
    fn from(smeta: SerializableTransactionStatusMeta) -> Self {
        Self {
            status: smeta.status,
            fee: smeta.fee,
            pre_balances: smeta.pre_balances,
            post_balances: smeta.post_balances,
            inner_instructions: smeta.inner_instructions,
            log_messages: smeta.log_messages,
            pre_token_balances: smeta
                .pre_token_balances
                .map(|v| v.into_iter().map(Into::into).collect()),
            post_token_balances: smeta
                .post_token_balances
                .map(|v| v.into_iter().map(Into::into).collect()),
            rewards: smeta.rewards,
            loaded_addresses: smeta.loaded_addresses,
            return_data: smeta.return_data,
            compute_units_consumed: smeta.compute_units_consumed,
            cost_units: smeta.cost_units,
        }
    }
}

/// Helper struct for serializing TransactionWithStatusMeta
/// Note: VersionedTransaction uses bincode internally, so we serialize it as base64-encoded bytes
#[derive(Serialize, Deserialize)]
struct SerializableTransactionWithStatusMeta {
    pub slot: u64,
    /// Base64-encoded bincode serialization of VersionedTransaction
    pub transaction_bytes: String,
    pub meta: SerializableTransactionStatusMeta,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum SurfnetTransactionStatus {
    Received,
    Processed(Box<(TransactionWithStatusMeta, HashSet<Pubkey>)>),
}

impl SurfnetTransactionStatus {
    pub fn expect_processed(&self) -> &(TransactionWithStatusMeta, HashSet<Pubkey>) {
        match &self {
            SurfnetTransactionStatus::Received => unreachable!(),
            SurfnetTransactionStatus::Processed(data) => data,
        }
    }

    pub fn as_processed(self) -> Option<(TransactionWithStatusMeta, HashSet<Pubkey>)> {
        match self {
            SurfnetTransactionStatus::Received => None,
            SurfnetTransactionStatus::Processed(data) => Some(*data),
        }
    }

    pub fn processed(status: TransactionWithStatusMeta, updated_accounts: HashSet<Pubkey>) -> Self {
        Self::Processed(Box::new((status, updated_accounts)))
    }
}

#[derive(Debug, Clone, Default)]
pub struct TransactionWithStatusMeta {
    pub slot: u64,
    pub transaction: VersionedTransaction,
    pub meta: TransactionStatusMeta,
}

impl Serialize for TransactionWithStatusMeta {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Serialize VersionedTransaction using bincode, then base64 encode
        let tx_bytes = bincode::serialize(&self.transaction)
            .map_err(|e| serde::ser::Error::custom(format!("bincode error: {}", e)))?;
        let tx_base64 = BASE64_STANDARD.encode(&tx_bytes);

        let helper = SerializableTransactionWithStatusMeta {
            slot: self.slot,
            transaction_bytes: tx_base64,
            meta: self.meta.clone().into(),
        };
        helper.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TransactionWithStatusMeta {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let helper = SerializableTransactionWithStatusMeta::deserialize(deserializer)?;

        // Decode base64 and deserialize using bincode
        let tx_bytes = BASE64_STANDARD
            .decode(&helper.transaction_bytes)
            .map_err(|e| serde::de::Error::custom(format!("base64 decode error: {}", e)))?;
        let transaction: VersionedTransaction = bincode::deserialize(&tx_bytes)
            .map_err(|e| serde::de::Error::custom(format!("bincode deserialize error: {}", e)))?;

        Ok(Self {
            slot: helper.slot,
            transaction,
            meta: helper.meta.into(),
        })
    }
}

impl TransactionWithStatusMeta {
    pub fn into_status(&self, current_slot: u64) -> TransactionStatus {
        TransactionStatus {
            slot: self.slot,
            confirmations: Some((current_slot - self.slot) as usize),
            status: self.meta.status.clone(),
            err: match &self.meta.status {
                Ok(_) => None,
                Err(e) => Some(e.clone()),
            },
            confirmation_status: Some(TransactionConfirmationStatus::Finalized),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        slot: u64,
        transaction: VersionedTransaction,
        transaction_meta: TransactionMetadata,
        accounts_before: &[Option<Account>],
        accounts_after: &[Option<Account>],
        pre_token_accounts_with_indexes: &[(usize, TokenAccount)],
        post_token_accounts_with_indexes: &[(usize, TokenAccount)],
        token_mints: Vec<MintAccount>,
        pre_token_program_ids: &[Pubkey],
        post_token_program_ids: &[Pubkey],
        loaded_addresses: LoadedAddresses,
    ) -> Self {
        let signatures_len = transaction.signatures.len();
        Self {
            slot,
            transaction,
            meta: TransactionStatusMeta {
                status: Ok(()),
                fee: 5000 * signatures_len as u64,
                pre_balances: accounts_before
                    .iter()
                    .map(|a| a.clone().map(|a| a.lamports).unwrap_or(0))
                    .collect(),
                post_balances: accounts_after
                    .iter()
                    .map(|a| a.clone().map(|a| a.lamports).unwrap_or(0))
                    .collect(),
                inner_instructions: Some(
                    transaction_meta
                        .inner_instructions
                        .iter()
                        .enumerate()
                        .filter_map(|(i, ixs)| {
                            if ixs.is_empty() {
                                None
                            } else {
                                Some(InnerInstructions {
                                    index: i as u8,
                                    instructions: ixs
                                        .iter()
                                        .map(|ix| InnerInstruction {
                                            instruction: ix.instruction.clone(),
                                            stack_height: Some(ix.stack_height as u32),
                                        })
                                        .collect(),
                                })
                            }
                        })
                        .collect(),
                ),
                log_messages: Some(transaction_meta.logs),
                pre_token_balances: Some(
                    pre_token_accounts_with_indexes
                        .iter()
                        .zip(token_mints.clone())
                        .zip(pre_token_program_ids)
                        .map(|(((i, a), mint), token_program)| TransactionTokenBalance {
                            account_index: *i as u8,
                            mint: a.mint().to_string(),
                            ui_token_amount: UiTokenAmount {
                                ui_amount: Some(format_ui_amount(a.amount(), mint.decimals())),
                                decimals: mint.decimals(),
                                amount: a.amount().to_string(),
                                ui_amount_string: format_ui_amount_string(
                                    a.amount(),
                                    mint.decimals(),
                                ),
                            },
                            owner: a.owner().to_string(),
                            program_id: token_program.to_string(),
                        })
                        .collect(),
                ),
                post_token_balances: Some(
                    post_token_accounts_with_indexes
                        .iter()
                        .zip(token_mints)
                        .zip(post_token_program_ids)
                        .map(|(((i, a), mint), token_program)| TransactionTokenBalance {
                            account_index: *i as u8,
                            mint: a.mint().to_string(),
                            ui_token_amount: UiTokenAmount {
                                ui_amount: Some(format_ui_amount(a.amount(), mint.decimals())),
                                decimals: mint.decimals(),
                                amount: a.amount().to_string(),
                                ui_amount_string: format_ui_amount_string(
                                    a.amount(),
                                    mint.decimals(),
                                ),
                            },
                            owner: a.owner().to_string(),
                            program_id: token_program.to_string(),
                        })
                        .collect(),
                ),
                rewards: Some(vec![]),
                loaded_addresses,
                return_data: Some(transaction_meta.return_data),
                compute_units_consumed: Some(transaction_meta.compute_units_consumed),
                cost_units: None,
            },
        }
    }

    pub fn encode(
        &self,
        encoding: UiTransactionEncoding,
        max_supported_transaction_version: Option<u8>,
        show_rewards: bool,
    ) -> Result<EncodedTransactionWithStatusMeta, EncodeError> {
        let version = self.validate_version(max_supported_transaction_version)?;
        Ok(EncodedTransactionWithStatusMeta {
            transaction: match encoding {
                UiTransactionEncoding::Binary => EncodedTransaction::LegacyBinary(
                    bs58::encode(bincode::serialize(&self.transaction).unwrap()).into_string(),
                ),
                UiTransactionEncoding::Base58 => EncodedTransaction::Binary(
                    bs58::encode(bincode::serialize(&self.transaction).unwrap()).into_string(),
                    TransactionBinaryEncoding::Base58,
                ),
                UiTransactionEncoding::Base64 => EncodedTransaction::Binary(
                    BASE64_STANDARD.encode(bincode::serialize(&self.transaction).unwrap()),
                    TransactionBinaryEncoding::Base64,
                ),
                UiTransactionEncoding::Json => EncodedTransaction::Json(UiTransaction {
                    signatures: self
                        .transaction
                        .signatures
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                    message: match &self.transaction.message {
                        VersionedMessage::Legacy(message) => {
                            message.encode(UiTransactionEncoding::Json)
                        }
                        VersionedMessage::V0(message) => message.json_encode(),
                    },
                }),
                UiTransactionEncoding::JsonParsed => EncodedTransaction::Json(UiTransaction {
                    signatures: self
                        .transaction
                        .signatures
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                    message: match &self.transaction.message {
                        VersionedMessage::Legacy(message) => {
                            message.encode(UiTransactionEncoding::JsonParsed)
                        }
                        VersionedMessage::V0(message) => {
                            message.encode_with_meta(UiTransactionEncoding::JsonParsed, &self.meta)
                        }
                    },
                }),
            },
            meta: Some(match encoding {
                UiTransactionEncoding::JsonParsed => {
                    parse_ui_transaction_status_meta_with_account_keys(
                        self.meta.clone(),
                        self.transaction.message.static_account_keys(),
                        show_rewards,
                    )
                }
                _ => {
                    let mut meta = parse_ui_transaction_status_meta(self.meta.clone());
                    if !show_rewards {
                        meta.rewards = OptionSerializer::None;
                    }
                    meta
                }
            }),
            version,
        })
    }

    pub fn to_json_accounts(
        &self,
        max_supported_transaction_version: Option<u8>,
        show_rewards: bool,
    ) -> Result<EncodedTransactionWithStatusMeta, EncodeError> {
        let version = self.validate_version(max_supported_transaction_version)?;
        let reserved_account_keys = ReservedAccountKeys::new_all_activated();

        let account_keys = match &self.transaction.message {
            VersionedMessage::Legacy(message) => parse_legacy_message_accounts(message),
            VersionedMessage::V0(message) => {
                let loaded_message = LoadedMessage::new_borrowed(
                    message,
                    &self.meta.loaded_addresses,
                    &reserved_account_keys.active,
                );
                parse_v0_message_accounts(&loaded_message)
            }
        };

        Ok(EncodedTransactionWithStatusMeta {
            transaction: EncodedTransaction::Accounts(UiAccountsList {
                signatures: self
                    .transaction
                    .signatures
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                account_keys,
            }),
            meta: Some(build_simple_ui_transaction_status_meta(
                self.meta.clone(),
                show_rewards,
            )),
            version,
        })
    }

    fn validate_version(
        &self,
        max_supported_transaction_version: Option<u8>,
    ) -> Result<Option<TransactionVersion>, EncodeError> {
        match (
            max_supported_transaction_version,
            self.transaction.version(),
        ) {
            // Set to none because old clients can't handle this field
            (None, TransactionVersion::LEGACY) => Ok(None),
            (None, TransactionVersion::Number(version)) => {
                Err(EncodeError::UnsupportedTransactionVersion(version))
            }
            (Some(_), TransactionVersion::LEGACY) => Ok(Some(TransactionVersion::LEGACY)),
            (Some(max_version), TransactionVersion::Number(version)) => {
                if version <= max_version {
                    Ok(Some(TransactionVersion::Number(version)))
                } else {
                    Err(EncodeError::UnsupportedTransactionVersion(version))
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_failure(
        slot: u64,
        transaction: VersionedTransaction,
        failure: &litesvm::types::FailedTransactionMetadata,
        accounts_before: &[Option<Account>],
        accounts_after: &[Option<Account>],
        pre_token_accounts_with_indexes: &[(usize, TokenAccount)],
        token_mints: Vec<MintAccount>,
        token_program_ids: &[Pubkey],
        loaded_addresses: LoadedAddresses,
    ) -> Self {
        let pre_balances: Vec<u64> = accounts_before
            .iter()
            .map(|a| a.as_ref().map(|a| a.lamports).unwrap_or(0))
            .collect();

        let fee = 5000 * transaction.signatures.len() as u64;

        let post_balances: Vec<u64> = accounts_after
            .iter()
            .map(|a| a.clone().map(|a| a.lamports).unwrap_or(0))
            .collect();

        let balances: Vec<TransactionTokenBalance> = pre_token_accounts_with_indexes
            .iter()
            .zip(token_mints)
            .zip(token_program_ids)
            .map(|(((i, a), mint), token_program)| TransactionTokenBalance {
                account_index: *i as u8,
                mint: a.mint().to_string(),
                ui_token_amount: UiTokenAmount {
                    ui_amount: Some(format_ui_amount(a.amount(), mint.decimals())),
                    decimals: mint.decimals(),
                    amount: a.amount().to_string(),
                    ui_amount_string: format_ui_amount_string(a.amount(), mint.decimals()),
                },
                owner: a.owner().to_string(),
                program_id: token_program.to_string(),
            })
            .collect();

        Self {
            slot,
            transaction,
            meta: TransactionStatusMeta {
                status: Err(failure.err.clone()),
                fee,
                pre_balances,
                post_balances,
                inner_instructions: Some(
                    failure
                        .meta
                        .inner_instructions
                        .iter()
                        .enumerate()
                        .filter_map(|(i, ixs)| {
                            if ixs.is_empty() {
                                None
                            } else {
                                Some(InnerInstructions {
                                    index: i as u8,
                                    instructions: ixs
                                        .iter()
                                        .map(|ix| InnerInstruction {
                                            instruction: ix.instruction.clone(),
                                            stack_height: Some(ix.stack_height as u32),
                                        })
                                        .collect(),
                                })
                            }
                        })
                        .collect(),
                ),
                log_messages: Some(failure.meta.logs.clone()),
                pre_token_balances: Some(balances.clone()),
                post_token_balances: Some(balances),
                rewards: Some(vec![]),
                loaded_addresses,
                return_data: Some(failure.meta.return_data.clone()),
                compute_units_consumed: Some(failure.meta.compute_units_consumed),
                cost_units: None,
            },
        }
    }
}

fn parse_ui_transaction_status_meta_with_account_keys(
    meta: TransactionStatusMeta,
    static_keys: &[Pubkey],
    show_rewards: bool,
) -> UiTransactionStatusMeta {
    let account_keys = AccountKeys::new(static_keys, Some(&meta.loaded_addresses));
    UiTransactionStatusMeta {
        err: meta.status.clone().map_err(Into::into).err(),
        status: meta.status.map_err(Into::into),
        fee: meta.fee,
        pre_balances: meta.pre_balances,
        post_balances: meta.post_balances,
        inner_instructions: meta
            .inner_instructions
            .map(|ixs| {
                ixs.into_iter()
                    .map(|ix| parse_ui_inner_instructions(ix, &account_keys))
                    .collect()
            })
            .into(),
        log_messages: meta.log_messages.into(),
        pre_token_balances: meta
            .pre_token_balances
            .map(|balance| balance.into_iter().map(Into::into).collect())
            .into(),
        post_token_balances: meta
            .post_token_balances
            .map(|balance| balance.into_iter().map(Into::into).collect())
            .into(),
        rewards: if show_rewards { meta.rewards } else { None }.into(),
        loaded_addresses: OptionSerializer::Skip,
        return_data: OptionSerializer::or_skip(
            meta.return_data.map(|return_data| return_data.into()),
        ),
        compute_units_consumed: OptionSerializer::or_skip(meta.compute_units_consumed),
        cost_units: OptionSerializer::or_skip(meta.cost_units),
    }
}

// FIXME: use native transform from the solana official crate
fn parse_ui_transaction_status_meta(meta: TransactionStatusMeta) -> UiTransactionStatusMeta {
    UiTransactionStatusMeta {
        err: meta.status.clone().map_err(Into::into).err(),
        status: meta.status.map_err(Into::into),
        fee: meta.fee,
        pre_balances: meta.pre_balances,
        post_balances: meta.post_balances,
        inner_instructions: meta
            .inner_instructions
            .map(|ixs| ixs.into_iter().map(Into::into).collect())
            .into(),
        log_messages: meta.log_messages.into(),
        pre_token_balances: meta
            .pre_token_balances
            .map(|balance| balance.into_iter().map(Into::into).collect())
            .into(),
        post_token_balances: meta
            .post_token_balances
            .map(|balance| balance.into_iter().map(Into::into).collect())
            .into(),
        rewards: meta.rewards.into(),
        loaded_addresses: Some(UiLoadedAddresses::from(&meta.loaded_addresses)).into(),
        return_data: OptionSerializer::or_skip(
            meta.return_data.map(|return_data| return_data.into()),
        ),
        compute_units_consumed: OptionSerializer::or_skip(meta.compute_units_consumed),
        cost_units: OptionSerializer::or_skip(meta.cost_units),
    }
}

fn build_simple_ui_transaction_status_meta(
    meta: TransactionStatusMeta,
    show_rewards: bool,
) -> UiTransactionStatusMeta {
    UiTransactionStatusMeta {
        err: meta.status.clone().map_err(Into::into).err(),
        status: meta.status.map_err(Into::into),
        fee: meta.fee,
        pre_balances: meta.pre_balances,
        post_balances: meta.post_balances,
        inner_instructions: OptionSerializer::Skip,
        log_messages: OptionSerializer::Skip,
        pre_token_balances: meta
            .pre_token_balances
            .map(|balance| balance.into_iter().map(Into::into).collect())
            .into(),
        post_token_balances: meta
            .post_token_balances
            .map(|balance| balance.into_iter().map(Into::into).collect())
            .into(),
        rewards: if show_rewards {
            meta.rewards.into()
        } else {
            OptionSerializer::Skip
        },
        loaded_addresses: OptionSerializer::Skip,
        return_data: OptionSerializer::Skip,
        compute_units_consumed: OptionSerializer::Skip,
        cost_units: OptionSerializer::Skip,
    }
}

pub fn surfpool_tx_metadata_to_litesvm_tx_metadata(
    metadata: &surfpool_types::TransactionMetadata,
) -> litesvm::types::TransactionMetadata {
    litesvm::types::TransactionMetadata {
        compute_units_consumed: metadata.compute_units_consumed,
        logs: metadata.logs.clone(),
        return_data: metadata.return_data.clone(),
        inner_instructions: metadata.inner_instructions.clone(),
        signature: metadata.signature,
        fee: metadata.fee,
    }
}

#[derive(Debug, Clone)]
pub enum RemoteRpcResult<T> {
    Ok(T),
    MethodNotSupported,
}

impl<T> RemoteRpcResult<T> {
    /// Converts RemoteRpcResult to SurfpoolResult
    pub fn into_result(self) -> SurfpoolResult<T> {
        match self {
            RemoteRpcResult::Ok(value) => Ok(value),
            RemoteRpcResult::MethodNotSupported => Err(SurfpoolError::rpc_method_not_supported()),
        }
    }

    /// Converts RemoteRpcResult to SurfpoolResult, treating MethodNotSupported as a default value
    pub fn into_result_or_default(self, default: T) -> SurfpoolResult<T> {
        match self {
            RemoteRpcResult::Ok(value) => Ok(value),
            RemoteRpcResult::MethodNotSupported => Ok(default),
        }
    }

    /// Handles RemoteRpcResult with a callback for MethodNotSupported that returns a default value
    pub fn handle_method_not_supported<F>(self, on_not_supported: F) -> T
    where
        F: FnOnce() -> T,
    {
        match self {
            RemoteRpcResult::Ok(value) => value,
            RemoteRpcResult::MethodNotSupported => on_not_supported(),
        }
    }

    /// Maps the Ok variant while preserving MethodNotSupported
    pub fn map<U, F>(self, f: F) -> RemoteRpcResult<U>
    where
        F: FnOnce(T) -> U,
    {
        match self {
            RemoteRpcResult::Ok(value) => RemoteRpcResult::Ok(f(value)),
            RemoteRpcResult::MethodNotSupported => RemoteRpcResult::MethodNotSupported,
        }
    }
}

/// Discriminant byte used for serializing token program variants.
/// Ensures consistent encoding between TokenAccount and MintAccount.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenProgramDiscriminant {
    SplToken = 0,
    SplToken2022 = 1,
}

impl TokenProgramDiscriminant {
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::SplToken),
            1 => Some(Self::SplToken2022),
            _ => None,
        }
    }

    pub fn as_byte(self) -> u8 {
        self as u8
    }
}

/// Emits `Serialize`/`Deserialize` impls for the two-variant
/// (`SplToken2022`, `SplToken`) packable enums `TokenAccount` and
/// `MintAccount`. The algorithm is identical for both: prepend a
/// discriminant byte, `pack_into_slice` into a fixed-length buffer, and
/// base64-encode; reverse on the way in. The only things that vary per
/// enum are the packed inner types and the human label used in error
/// messages, so those are the macro parameters.
macro_rules! impl_token_program_packable_serde {
    ($enum_name:ident, $ty_2022:ty, $ty_legacy:ty, $label:literal) => {
        impl Serialize for $enum_name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let mut bytes = Vec::with_capacity(1 + <$ty_2022>::LEN);
                match self {
                    Self::SplToken2022(inner) => {
                        bytes.push(TokenProgramDiscriminant::SplToken2022.as_byte());
                        let mut dst = [0u8; <$ty_2022>::LEN];
                        inner.pack_into_slice(&mut dst);
                        bytes.extend_from_slice(&dst);
                    }
                    Self::SplToken(inner) => {
                        bytes.push(TokenProgramDiscriminant::SplToken.as_byte());
                        let mut dst = [0u8; <$ty_legacy>::LEN];
                        inner.pack_into_slice(&mut dst);
                        bytes.extend_from_slice(&dst);
                    }
                }
                let encoded = BASE64_STANDARD.encode(&bytes);
                serializer.serialize_str(&encoded)
            }
        }

        impl<'de> Deserialize<'de> for $enum_name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let encoded = String::deserialize(deserializer)?;
                let bytes = BASE64_STANDARD
                    .decode(&encoded)
                    .map_err(serde::de::Error::custom)?;

                if bytes.is_empty() {
                    return Err(serde::de::Error::custom(concat!(
                        "Empty ", $label, " bytes"
                    )));
                }

                let discriminant =
                    TokenProgramDiscriminant::from_byte(bytes[0]).ok_or_else(|| {
                        serde::de::Error::custom(format!(
                            concat!("Unknown ", $label, " discriminant: {}"),
                            bytes[0]
                        ))
                    })?;
                let data = &bytes[1..];

                match discriminant {
                    TokenProgramDiscriminant::SplToken2022 => {
                        let inner = <$ty_2022>::unpack(data).map_err(serde::de::Error::custom)?;
                        Ok($enum_name::SplToken2022(inner))
                    }
                    TokenProgramDiscriminant::SplToken => {
                        let inner = <$ty_legacy>::unpack(data).map_err(serde::de::Error::custom)?;
                        Ok($enum_name::SplToken(inner))
                    }
                }
            }
        }
    };
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum TokenAccount {
    SplToken2022(spl_token_2022_interface::state::Account),
    SplToken(spl_token_interface::state::Account),
}

impl TokenAccount {
    pub fn token_program_id(&self) -> Pubkey {
        match self {
            Self::SplToken2022(_) => spl_token_2022_interface::id(),
            Self::SplToken(_) => spl_token_interface::id(),
        }
    }

    pub fn unpack(bytes: &[u8]) -> SurfpoolResult<Self> {
        if let Ok(account) = spl_token_2022_interface::state::Account::unpack(bytes) {
            Ok(Self::SplToken2022(account))
        } else if let Ok(account) = spl_token_interface::state::Account::unpack(bytes) {
            Ok(Self::SplToken(account))
        } else if let Ok(account) =
            StateWithExtensions::<spl_token_2022_interface::state::Account>::unpack(bytes)
        {
            Ok(Self::SplToken2022(account.base))
        } else {
            Err(SurfpoolError::unpack_token_account())
        }
    }

    pub fn new(
        token_program_id: &Pubkey,
        owner: Pubkey,
        mint: Pubkey,
        is_native: Option<u64>,
    ) -> Self {
        if token_program_id == &spl_token_2022_interface::id() {
            Self::SplToken2022(spl_token_2022_interface::state::Account {
                mint,
                owner,
                state: spl_token_2022_interface::state::AccountState::Initialized,
                is_native: is_native.into(),
                ..Default::default()
            })
        } else {
            Self::SplToken(spl_token_interface::state::Account {
                mint,
                owner,
                state: spl_token_interface::state::AccountState::Initialized,
                is_native: is_native.into(),
                ..Default::default()
            })
        }
    }

    pub fn pack_into_vec(&self) -> Vec<u8> {
        match self {
            Self::SplToken2022(account) => {
                let mut dst = [0u8; spl_token_2022_interface::state::Account::LEN];
                account.pack_into_slice(&mut dst);
                dst.to_vec()
            }
            Self::SplToken(account) => {
                let mut dst = [0u8; spl_token_interface::state::Account::LEN];
                account.pack_into_slice(&mut dst);
                dst.to_vec()
            }
        }
    }

    pub fn owner(&self) -> Pubkey {
        match self {
            Self::SplToken2022(account) => account.owner,
            Self::SplToken(account) => account.owner,
        }
    }

    pub fn mint(&self) -> Pubkey {
        match self {
            Self::SplToken2022(account) => account.mint,
            Self::SplToken(account) => account.mint,
        }
    }

    pub fn delegate(&self) -> COption<Pubkey> {
        match self {
            Self::SplToken2022(account) => account.delegate,
            Self::SplToken(account) => account.delegate,
        }
    }

    pub fn set_delegate(&mut self, delegate: COption<Pubkey>) {
        match self {
            Self::SplToken2022(account) => account.delegate = delegate,
            Self::SplToken(account) => account.delegate = delegate,
        }
    }

    pub fn set_delegated_amount(&mut self, delegated_amount: u64) {
        match self {
            Self::SplToken2022(account) => account.delegated_amount = delegated_amount,
            Self::SplToken(account) => account.delegated_amount = delegated_amount,
        }
    }

    pub fn set_close_authority(&mut self, close_authority: COption<Pubkey>) {
        match self {
            Self::SplToken2022(account) => account.close_authority = close_authority,
            Self::SplToken(account) => account.close_authority = close_authority,
        }
    }

    pub fn amount(&self) -> u64 {
        match self {
            Self::SplToken2022(account) => account.amount,
            Self::SplToken(account) => account.amount,
        }
    }

    pub fn set_amount(&mut self, amount: u64) {
        match self {
            Self::SplToken2022(account) => account.amount = amount,
            Self::SplToken(account) => account.amount = amount,
        }
    }

    pub fn get_packed_len_for_token_program_id(token_program_id: &Pubkey) -> usize {
        if *token_program_id == spl_token_interface::id() {
            spl_token_interface::state::Account::get_packed_len()
        } else {
            spl_token_2022_interface::state::Account::get_packed_len()
        }
    }

    pub fn set_state_from_str(&mut self, state: &str) -> SurfpoolResult<()> {
        match self {
            Self::SplToken2022(account) => {
                account.state = match state {
                    "uninitialized" => spl_token_2022_interface::state::AccountState::Uninitialized,
                    "frozen" => spl_token_2022_interface::state::AccountState::Frozen,
                    "initialized" => spl_token_2022_interface::state::AccountState::Initialized,
                    _ => {
                        return Err(SurfpoolError::invalid_token_account_state(state));
                    }
                }
            }
            Self::SplToken(account) => {
                account.state = match state {
                    "uninitialized" => spl_token_interface::state::AccountState::Uninitialized,
                    "frozen" => spl_token_interface::state::AccountState::Frozen,
                    "initialized" => spl_token_interface::state::AccountState::Initialized,
                    _ => {
                        return Err(SurfpoolError::invalid_token_account_state(state));
                    }
                }
            }
        }
        Ok(())
    }
}

impl_token_program_packable_serde!(
    TokenAccount,
    spl_token_2022_interface::state::Account,
    spl_token_interface::state::Account,
    "TokenAccount"
);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfflineAccountConfig {
    pub include_owned_accounts: bool,
}

#[derive(Debug, Clone)]
pub enum MintAccount {
    SplToken2022(spl_token_2022_interface::state::Mint),
    SplToken(spl_token_interface::state::Mint),
}

impl_token_program_packable_serde!(
    MintAccount,
    spl_token_2022_interface::state::Mint,
    spl_token_interface::state::Mint,
    "MintAccount"
);

impl MintAccount {
    pub fn unpack(bytes: &[u8]) -> SurfpoolResult<Self> {
        if let Ok(mint) =
            StateWithExtensions::<spl_token_2022_interface::state::Mint>::unpack(bytes)
        {
            Ok(Self::SplToken2022(mint.base))
        } else if let Ok(mint) = spl_token_2022_interface::state::Mint::unpack(bytes) {
            Ok(Self::SplToken2022(mint))
        } else if let Ok(mint) = spl_token_interface::state::Mint::unpack(bytes) {
            Ok(Self::SplToken(mint))
        } else {
            Err(SurfpoolError::unpack_mint_account())
        }
    }

    pub fn decimals(&self) -> u8 {
        match self {
            Self::SplToken2022(mint) => mint.decimals,
            Self::SplToken(mint) => mint.decimals,
        }
    }
}

pub struct GeyserAccountUpdate {
    pub pubkey: Pubkey,
    pub account: Account,
    pub slot: u64,
    pub sanitized_transaction: Option<SanitizedTransaction>,
    pub write_version: u64,
}
impl GeyserAccountUpdate {
    pub fn transaction_update(
        pubkey: Pubkey,
        account: Account,
        slot: u64,
        sanitized_transaction: SanitizedTransaction,
        write_version: u64,
    ) -> Self {
        Self {
            pubkey,
            account,
            slot,
            sanitized_transaction: Some(sanitized_transaction),
            write_version,
        }
    }

    pub fn block_update(pubkey: Pubkey, account: Account, slot: u64, write_version: u64) -> Self {
        Self {
            pubkey,
            account,
            slot,
            sanitized_transaction: None,
            write_version,
        }
    }

    pub fn startup_update(pubkey: Pubkey, account: Account, slot: u64, write_version: u64) -> Self {
        Self {
            pubkey,
            account,
            slot,
            sanitized_transaction: None,
            write_version,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TimeTravelConfig {
    AbsoluteEpoch(Epoch),
    AbsoluteSlot(Slot),
    AbsoluteTimestamp(u64),
}

impl Default for TimeTravelConfig {
    fn default() -> Self {
        // chrono timestamp in ms, 1 hour from now
        Self::AbsoluteTimestamp(Utc::now().timestamp_millis() as u64 + 3600000)
    }
}

#[derive(Debug)]
pub enum TimeTravelError {
    PastTimestamp { target: u64, current: u64 },
    PastSlot { target: u64, current: u64 },
    PastEpoch { target: u64, current: u64 },
    ZeroSlotTime,
    ZeroSlotsInEpoch,
}

impl std::fmt::Display for TimeTravelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TimeTravelError::PastTimestamp { target, current } => {
                write!(
                    f,
                    "Cannot travel to past timestamp: target={}, current={}",
                    target, current
                )
            }
            TimeTravelError::PastSlot { target, current } => {
                write!(
                    f,
                    "Cannot travel to past slot: target={}, current={}",
                    target, current
                )
            }
            TimeTravelError::PastEpoch { target, current } => {
                write!(
                    f,
                    "Cannot travel to past epoch: target={}, current={}",
                    target, current
                )
            }
            TimeTravelError::ZeroSlotTime => {
                write!(f, "Cannot calculate time travel with zero slot time")
            }
            TimeTravelError::ZeroSlotsInEpoch => {
                write!(f, "Cannot calculate time travel with zero slots in epoch")
            }
        }
    }
}

impl std::error::Error for TimeTravelError {}

#[derive(Debug, Default)]
/// Tracks the loaded addresses with its associated index within an Address Lookup Table
pub struct IndexedLoadedAddresses {
    pub writable: Vec<(u8, Pubkey)>,
    pub readonly: Vec<(u8, Pubkey)>,
}

impl IndexedLoadedAddresses {
    pub fn new(writable: Vec<(u8, Pubkey)>, readonly: Vec<(u8, Pubkey)>) -> Self {
        Self { writable, readonly }
    }
    pub fn writable_keys(&self) -> Vec<&Pubkey> {
        self.writable.iter().map(|(_, pubkey)| pubkey).collect()
    }
    pub fn readonly_keys(&self) -> Vec<&Pubkey> {
        self.readonly.iter().map(|(_, pubkey)| pubkey).collect()
    }
    pub fn is_empty(&self) -> bool {
        self.writable.is_empty() && self.readonly.is_empty()
    }
}

#[derive(Debug, Default)]
/// Maps an Address Lookup Table entry to its indexed loaded addresses
pub struct TransactionLoadedAddresses(IndexMap<Pubkey, IndexedLoadedAddresses>);

impl TransactionLoadedAddresses {
    pub fn new() -> Self {
        Self(IndexMap::new())
    }

    /// Filters the loaded addresses based on the provided writable and readonly sets.
    pub fn filter_from_members(
        &self,
        writable: &HashSet<Pubkey>,
        readonly: &HashSet<Pubkey>,
    ) -> Self {
        let mut filtered = Self::new();
        for (pubkey, loaded_addresses) in &self.0 {
            let mut new_loaded_addresses = IndexedLoadedAddresses::default();
            new_loaded_addresses.writable.extend(
                loaded_addresses
                    .writable
                    .iter()
                    .filter(|&(_, addr)| writable.contains(addr)),
            );
            new_loaded_addresses.readonly.extend(
                loaded_addresses
                    .readonly
                    .iter()
                    .filter(|&(_, addr)| readonly.contains(addr)),
            );
            if !new_loaded_addresses.is_empty() {
                filtered.insert(*pubkey, new_loaded_addresses);
            }
        }
        filtered
    }

    pub fn insert(&mut self, pubkey: Pubkey, loaded_addresses: IndexedLoadedAddresses) {
        self.0.insert(pubkey, loaded_addresses);
    }

    pub fn insert_members(
        &mut self,
        pubkey: Pubkey,
        writable: Vec<(u8, Pubkey)>,
        readonly: Vec<(u8, Pubkey)>,
    ) {
        self.0
            .insert(pubkey, IndexedLoadedAddresses::new(writable, readonly));
    }

    pub fn loaded_addresses(&self) -> LoadedAddresses {
        let mut loaded = LoadedAddresses::default();
        for (_, loaded_addresses) in &self.0 {
            loaded.writable.extend(loaded_addresses.writable_keys());
            loaded.readonly.extend(loaded_addresses.readonly_keys());
        }
        loaded
    }

    pub fn all_loaded_addresses(&self) -> Vec<&Pubkey> {
        let mut writable = vec![];
        let mut readonly = vec![];

        for (_, loaded_addresses) in &self.0 {
            writable.extend(loaded_addresses.writable_keys());
            readonly.extend(loaded_addresses.readonly_keys());
        }

        writable.append(&mut readonly);
        writable
    }

    pub fn alt_addresses(&self) -> Vec<Pubkey> {
        self.0.keys().cloned().collect()
    }

    pub fn to_address_table_lookups(&self) -> Vec<MessageAddressTableLookup> {
        self.0
            .iter()
            .map(|(pubkey, loaded_addresses)| MessageAddressTableLookup {
                account_key: *pubkey,
                writable_indexes: loaded_addresses
                    .writable
                    .iter()
                    .map(|(idx, _)| *idx)
                    .collect(),
                readonly_indexes: loaded_addresses
                    .readonly
                    .iter()
                    .map(|(idx, _)| *idx)
                    .collect(),
            })
            .collect()
    }

    pub fn writable_len(&self) -> usize {
        self.0
            .values()
            .map(|loaded_addresses| loaded_addresses.writable.len())
            .sum()
    }

    pub fn readonly_len(&self) -> usize {
        self.0
            .values()
            .map(|loaded_addresses| loaded_addresses.readonly.len())
            .sum()
    }
}

#[derive(Debug, Clone)]
pub struct SyntheticBlockhash(Hash);
impl SyntheticBlockhash {
    pub const PREFIX: &str = "SURFNETxSAFEHASHx";
    pub fn new(chain_tip_index: u64) -> Self {
        // Create a new synthetic blockhash - SURFNETxSAFEHASHxxxxxxxxxxxxxxxxxxxxxxxxx28
        // Create a string and decode it from base58 to get the raw bytes
        let index_hex = format!("{:08x}", chain_tip_index)
            .replace('0', "x") // Replace 0 with x
            .replace('O', "x"); // Replace O with x

        // Calculate how many 'x' characters we need to pad to reach a consistent length
        let target_length = 43; // 43 base58 sequence leads us to 32 bytes
        let padding_needed = target_length - SyntheticBlockhash::PREFIX.len() - index_hex.len();
        let padding = "x".repeat(padding_needed.max(0));
        let target_string = format!("{}{}{}", SyntheticBlockhash::PREFIX, padding, index_hex);

        let decoded_bytes = bs58::decode(&target_string).into_vec().unwrap_or_else(|_| {
            // Fallback if decode fails
            vec![0u8; 32]
        });

        let mut blockhash_bytes = [0u8; 32];
        blockhash_bytes[..decoded_bytes.len().min(32)]
            .copy_from_slice(&decoded_bytes[..decoded_bytes.len().min(32)]);
        Self(Hash::new_from_array(blockhash_bytes))
    }

    pub fn to_string(&self) -> String {
        self.0.to_string()
    }

    pub fn hash(&self) -> &Hash {
        &self.0
    }
}
