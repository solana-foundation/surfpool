#![allow(dead_code)]

use std::any::type_name;

use base64::prelude::*;
use jsonrpc_core::{Error, Result};
use litesvm::types::TransactionMetadata;
use solana_client::{
    rpc_config::{RpcTokenAccountsFilter, RpcTransactionConfig},
    rpc_filter::RpcFilterType,
    rpc_request::{MAX_GET_CONFIRMED_SIGNATURES_FOR_ADDRESS2_LIMIT, TokenAccountsFilter},
};
use solana_commitment_config::CommitmentConfig;
use solana_hash::Hash;
use solana_message::{
    AccountKeys, VersionedMessage,
    v1::{MAX_TRANSACTION_SIZE, V1_PREFIX},
};
use solana_packet::PACKET_DATA_SIZE;
use solana_pubkey::{ParsePubkeyError, Pubkey};
use solana_signature::Signature;
use solana_transaction_status::{
    InnerInstruction, InnerInstructions, TransactionBinaryEncoding, UiInnerInstructions,
    UiTransactionEncoding, parse_ui_inner_instructions,
};

use crate::error::{SurfpoolError, SurfpoolResult};

pub fn convert_transaction_metadata_from_canonical(
    transaction_metadata: &TransactionMetadata,
) -> surfpool_types::TransactionMetadata {
    surfpool_types::TransactionMetadata {
        signature: transaction_metadata.signature,
        logs: transaction_metadata.logs.clone(),
        inner_instructions: transaction_metadata.inner_instructions.clone(),
        compute_units_consumed: transaction_metadata.compute_units_consumed,
        return_data: transaction_metadata.return_data.clone(),
        fee: transaction_metadata.fee,
    }
}

fn optimize_filters(filters: &mut [RpcFilterType]) {
    filters.iter_mut().for_each(|filter_type| {
        if let RpcFilterType::Memcmp(compare) = filter_type {
            if let Err(err) = compare.convert_to_raw_bytes() {
                // All filters should have been previously verified
                warn!("Invalid filter: bytes could not be decoded, {err}");
            }
        }
    })
}

fn verify_filter(input: &RpcFilterType) -> Result<()> {
    input
        .verify()
        .map_err(|e| Error::invalid_params(format!("Invalid param: {e:?}")))
}

pub fn verify_pubkey(input: &str) -> SurfpoolResult<Pubkey> {
    input
        .parse()
        .map_err(|e: ParsePubkeyError| SurfpoolError::invalid_pubkey(input, e.to_string()))
}

pub fn verify_pubkeys(input: &[String]) -> SurfpoolResult<Vec<Pubkey>> {
    input
        .iter()
        .enumerate()
        .map(|(i, s)| {
            verify_pubkey(s)
                .map_err(|e| SurfpoolError::invalid_pubkey_at_index(s, i, e.to_string()))
        })
        .collect::<SurfpoolResult<Vec<_>>>()
}

fn verify_hash(input: &str) -> Result<Hash> {
    input
        .parse()
        .map_err(|e| Error::invalid_params(format!("Invalid param: {e:?}")))
}

fn verify_signature(input: &str) -> Result<Signature> {
    input
        .parse()
        .map_err(|e| Error::invalid_params(format!("Invalid param: {e:?}")))
}

fn verify_token_account_filter(
    token_account_filter: RpcTokenAccountsFilter,
) -> Result<TokenAccountsFilter> {
    match token_account_filter {
        RpcTokenAccountsFilter::Mint(mint_str) => {
            let mint = verify_pubkey(&mint_str)?;
            Ok(TokenAccountsFilter::Mint(mint))
        }
        RpcTokenAccountsFilter::ProgramId(program_id_str) => {
            let program_id = verify_pubkey(&program_id_str)?;
            Ok(TokenAccountsFilter::ProgramId(program_id))
        }
    }
}

fn verify_and_parse_signatures_for_address_params(
    address: String,
    before: Option<String>,
    until: Option<String>,
    limit: Option<usize>,
) -> Result<(Pubkey, Option<Signature>, Option<Signature>, usize)> {
    let address = verify_pubkey(&address)?;
    let before = before
        .map(|ref before| verify_signature(before))
        .transpose()?;
    let until = until.map(|ref until| verify_signature(until)).transpose()?;
    let limit = limit.unwrap_or(MAX_GET_CONFIRMED_SIGNATURES_FOR_ADDRESS2_LIMIT);

    if limit == 0 || limit > MAX_GET_CONFIRMED_SIGNATURES_FOR_ADDRESS2_LIMIT {
        return Err(Error::invalid_params(format!(
            "Invalid limit; max {MAX_GET_CONFIRMED_SIGNATURES_FOR_ADDRESS2_LIMIT}"
        )));
    }
    Ok((address, before, until, limit))
}

const MAX_BASE58_SIZE: usize = 5594; // Golden, bump if MAX_TRANSACTION_SIZE changes
const MAX_BASE64_SIZE: usize = 5464; // Golden, bump if MAX_TRANSACTION_SIZE changes

/// Highest transaction version supported by this Surfpool release.
pub const MAX_SUPPORTED_TRANSACTION_VERSION: u8 = 1;

fn wire_size_limit(wire_output: &[u8]) -> usize {
    // V1 messages and transactions both start with the V1 message prefix. Legacy and V0
    // transactions start with their short-vec signature count instead.
    if wire_output.first().copied() == Some(V1_PREFIX) {
        MAX_TRANSACTION_SIZE
    } else {
        PACKET_DATA_SIZE
    }
}

pub fn decode_and_deserialize<T>(
    encoded: String,
    encoding: TransactionBinaryEncoding,
) -> Result<(Vec<u8>, T)>
where
    T: wincode::DeserializeOwned<Dst = T>,
{
    let wire_output = match encoding {
        TransactionBinaryEncoding::Base58 => {
            if encoded.len() > MAX_BASE58_SIZE {
                return Err(Error::invalid_params(format!(
                    "base58 encoded {} too large: {} bytes (max: encoded/raw {}/{})",
                    type_name::<T>(),
                    encoded.len(),
                    MAX_BASE58_SIZE,
                    MAX_TRANSACTION_SIZE,
                )));
            }
            bs58::decode(encoded)
                .into_vec()
                .map_err(|e| Error::invalid_params(format!("invalid base58 encoding: {e:?}")))?
        }
        TransactionBinaryEncoding::Base64 => {
            if encoded.len() > MAX_BASE64_SIZE {
                return Err(Error::invalid_params(format!(
                    "base64 encoded {} too large: {} bytes (max: encoded/raw {}/{})",
                    type_name::<T>(),
                    encoded.len(),
                    MAX_BASE64_SIZE,
                    MAX_TRANSACTION_SIZE,
                )));
            }
            BASE64_STANDARD
                .decode(encoded)
                .map_err(|e| Error::invalid_params(format!("invalid base64 encoding: {e:?}")))?
        }
    };
    let size_limit = wire_size_limit(&wire_output);
    if wire_output.len() > size_limit {
        return Err(Error::invalid_params(format!(
            "decoded {} too large: {} bytes (max: {} bytes)",
            type_name::<T>(),
            wire_output.len(),
            size_limit
        )));
    }
    wincode::deserialize(&wire_output)
        .map_err(|err| {
            Error::invalid_params(format!(
                "failed to deserialize {}: {}",
                type_name::<T>(),
                &err.to_string()
            ))
        })
        .map(|output| (wire_output, output))
}

/// Decode the RPC `data` parameter of `sendTransaction` and
/// `simulateTransaction` into a `solana_transaction::VersionedTransaction`.
///
/// Both methods accept a `UiTransactionEncoding` on their config that defaults
/// to `Base58`, map it to the internal `TransactionBinaryEncoding`, and feed
/// the result into `decode_and_deserialize`. The mapping can fail (the RPC
/// only accepts base58 and base64), which is reported back as an
/// `Error::invalid_params` listing the supported encodings.
pub fn decode_rpc_versioned_transaction(
    data: String,
    encoding: Option<UiTransactionEncoding>,
) -> Result<solana_transaction::versioned::VersionedTransaction> {
    let tx_encoding = encoding.unwrap_or(UiTransactionEncoding::Base58);
    let binary_encoding = tx_encoding.into_binary_encoding().ok_or_else(|| {
        Error::invalid_params(format!(
            "unsupported encoding: {tx_encoding}. Supported encodings: base58, base64"
        ))
    })?;
    let (_, unsanitized_tx) = decode_and_deserialize::<
        solana_transaction::versioned::VersionedTransaction,
    >(data, binary_encoding)?;
    Ok(unsanitized_tx)
}

pub fn transform_tx_metadata_to_ui_accounts(
    meta: TransactionMetadata,
    message: &VersionedMessage,
    loaded_addresses: Option<&solana_message::v0::LoadedAddresses>,
) -> Vec<UiInnerInstructions> {
    // Create AccountKeys from the transaction message with loaded addresses from ALTs
    let account_keys = AccountKeys::new(message.static_account_keys(), loaded_addresses);

    meta.inner_instructions
        .into_iter()
        .enumerate()
        .filter_map(|(i, ixs)| {
            let instructions: Vec<InnerInstruction> = ixs
                .iter()
                .map(|ix| InnerInstruction {
                    instruction: ix.instruction.clone(),
                    stack_height: Some(ix.stack_height as u32),
                })
                .collect();
            if instructions.is_empty() {
                None
            } else {
                // Create InnerInstructions and then parse it into UiInnerInstructions
                // This will properly convert CompiledInstruction to UiInstruction format
                let inner_instructions = InnerInstructions {
                    index: i as u8,
                    instructions,
                };
                Some(parse_ui_inner_instructions(
                    inner_instructions,
                    &account_keys,
                ))
            }
        })
        .collect()
}

/// Substrings that, when present in a lowercased error message, indicate the
/// remote RPC method is not supported by the upstream (often a public endpoint
/// that has gated methods behind a 410 Gone response or a custom refusal).
const METHOD_NOT_SUPPORTED_NEEDLES: &[&str] = &[
    "not supported",
    "unsupported",
    "unavailable",
    "method blocked",
    "invalid request",
    "is blocked",
    "if you need this method",
    "client error 410",
    "410 gone",
    "(410 gone)",
    " status 410",
    "http 410",
    "client error (410",
];

/// Returns true if the error indicates the remote method is not supported.
pub fn is_method_not_supported_error<E: std::fmt::Display>(err: &E) -> bool {
    let msg = err.to_string().to_lowercase();
    METHOD_NOT_SUPPORTED_NEEDLES
        .iter()
        .any(|needle| msg.contains(needle))
}

pub fn get_default_transaction_config() -> RpcTransactionConfig {
    RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Json),
        commitment: Some(CommitmentConfig::default()),
        max_supported_transaction_version: Some(MAX_SUPPORTED_TRANSACTION_VERSION),
    }
}

pub fn adjust_default_transaction_config(config: &mut RpcTransactionConfig) {
    if config.encoding.is_none() {
        config.encoding = Some(UiTransactionEncoding::Json);
    }
    if config.max_supported_transaction_version.is_none() {
        config.max_supported_transaction_version = Some(MAX_SUPPORTED_TRANSACTION_VERSION);
    }
    if config.commitment.is_none() {
        config.commitment = Some(CommitmentConfig::default());
    }
}

#[cfg(test)]
mod tests {
    use solana_keypair::Keypair;
    use solana_message::{
        MESSAGE_VERSION_PREFIX, MessageHeader, compiled_instruction::CompiledInstruction, legacy,
        v0, v1,
    };
    use solana_signer::Signer;
    use solana_transaction::versioned::VersionedTransaction;

    use super::*;

    fn signed_legacy_transaction(data_len: usize) -> VersionedTransaction {
        let payer = Keypair::new();
        let message = VersionedMessage::Legacy(legacy::Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            account_keys: vec![payer.pubkey(), Pubkey::new_unique()],
            recent_blockhash: Hash::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![0; data_len],
            }],
        });
        VersionedTransaction::try_new(message, &[&payer]).unwrap()
    }

    fn signed_v0_transaction(data_len: usize) -> VersionedTransaction {
        let payer = Keypair::new();
        let message = VersionedMessage::V0(v0::Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            account_keys: vec![payer.pubkey(), Pubkey::new_unique()],
            recent_blockhash: Hash::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![0; data_len],
            }],
            address_table_lookups: vec![],
        });
        VersionedTransaction::try_new(message, &[&payer]).unwrap()
    }

    fn signed_v1_transaction(data_len: usize) -> VersionedTransaction {
        let payer = Keypair::new();
        let message = VersionedMessage::V1(v1::Message::new(
            MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            v1::TransactionConfig::empty(),
            Hash::default(),
            vec![payer.pubkey(), Pubkey::new_unique()],
            vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![0; data_len],
            }],
        ));
        let transaction = VersionedTransaction::try_new(message, &[&payer]).unwrap();
        assert!(
            transaction.signatures[0]
                .as_ref()
                .iter()
                .any(|byte| *byte != 0)
        );
        transaction
    }

    #[test]
    fn uses_expected_wire_size_limits_by_transaction_version() {
        let legacy_wire = wincode::serialize(&signed_legacy_transaction(0)).unwrap();
        assert_eq!(wire_size_limit(&legacy_wire), PACKET_DATA_SIZE);
        assert_ne!(legacy_wire[0], V1_PREFIX);

        let v0_wire = wincode::serialize(&signed_v0_transaction(0)).unwrap();
        assert_eq!(wire_size_limit(&v0_wire), PACKET_DATA_SIZE);
        assert_ne!(v0_wire[0], V1_PREFIX);
        assert_eq!(v0_wire[1 + 64], MESSAGE_VERSION_PREFIX);

        let v1_wire = wincode::serialize(&signed_v1_transaction(0)).unwrap();
        assert_eq!(wire_size_limit(&v1_wire), MAX_TRANSACTION_SIZE);
        assert_eq!(v1_wire[0], V1_PREFIX);
    }

    #[test]
    fn decodes_signed_v1_transaction_larger_than_legacy_packet() {
        let transaction = signed_v1_transaction(PACKET_DATA_SIZE);
        let wire = wincode::serialize(&transaction).unwrap();
        assert!(wire.len() > PACKET_DATA_SIZE);
        assert!(wire.len() <= MAX_TRANSACTION_SIZE);
        assert_eq!(wire[0], V1_PREFIX);

        let encoded = BASE64_STANDARD.encode(&wire);
        let (_, decoded) = decode_and_deserialize::<VersionedTransaction>(
            encoded,
            TransactionBinaryEncoding::Base64,
        )
        .unwrap();

        assert_eq!(decoded, transaction);
    }

    #[test]
    fn rejects_oversized_legacy_v0_and_v1_wire_payloads() {
        let oversized_legacy = wincode::serialize(&signed_legacy_transaction(PACKET_DATA_SIZE))
            .expect("legacy transaction should serialize");
        assert!(oversized_legacy.len() > PACKET_DATA_SIZE);
        let legacy_error = decode_and_deserialize::<VersionedTransaction>(
            BASE64_STANDARD.encode(oversized_legacy),
            TransactionBinaryEncoding::Base64,
        )
        .unwrap_err();
        assert!(legacy_error.message.contains("max: 1232 bytes"));

        let oversized_v0 = wincode::serialize(&signed_v0_transaction(PACKET_DATA_SIZE))
            .expect("v0 transaction should serialize");
        assert!(oversized_v0.len() > PACKET_DATA_SIZE);
        let v0_error = decode_and_deserialize::<VersionedTransaction>(
            BASE64_STANDARD.encode(oversized_v0),
            TransactionBinaryEncoding::Base64,
        )
        .unwrap_err();
        assert!(v0_error.message.contains("max: 1232 bytes"));

        let mut oversized_v1 = vec![0; MAX_TRANSACTION_SIZE + 1];
        oversized_v1[0] = V1_PREFIX;
        let v1_error = decode_and_deserialize::<VersionedTransaction>(
            BASE64_STANDARD.encode(oversized_v1),
            TransactionBinaryEncoding::Base64,
        )
        .unwrap_err();
        assert!(v1_error.message.contains("max: 4096 bytes"));
    }

    #[test]
    fn worst_case_v1_encoded_size_goldens() {
        let wire = vec![0xff; MAX_TRANSACTION_SIZE];
        assert_eq!(bs58::encode(&wire).into_string().len(), MAX_BASE58_SIZE);
        assert_eq!(BASE64_STANDARD.encode(&wire).len(), MAX_BASE64_SIZE);
    }

    #[test]
    fn transaction_config_defaults_support_v1() {
        assert_eq!(
            get_default_transaction_config().max_supported_transaction_version,
            Some(MAX_SUPPORTED_TRANSACTION_VERSION)
        );

        let mut config = RpcTransactionConfig::default();
        adjust_default_transaction_config(&mut config);
        assert_eq!(
            config.max_supported_transaction_version,
            Some(MAX_SUPPORTED_TRANSACTION_VERSION)
        );
    }
}
