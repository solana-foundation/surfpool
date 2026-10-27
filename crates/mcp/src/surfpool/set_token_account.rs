use reqwest::Client;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer as _;
use spl_associated_token_account_interface::address::get_associated_token_address_with_program_id;
use surfpool_types::{
    types::{AccountUpdate, TokenAccountUpdate},
    verified_tokens::{VERIFIED_TOKENS, VERIFIED_TOKENS_BY_SYMBOL},
};

const SOL_DECIMALS: u32 = 9;
const DEFAULT_TOKEN_AMOUNT: u64 = 100_000_000_000;
const SOL_SYMBOL: &str = "SOL";

#[derive(Serialize)]
struct JsonRpcRequest {
    /// The JSON-RPC version
    jsonrpc: String,
    /// The request ID
    id: u64,
    /// The method to call
    method: String,
    /// The parameters to pass to the method
    params: serde_json::Value,
}

#[derive(Deserialize, Debug)]
struct JsonRpcResponse<T> {
    #[allow(dead_code)]
    result: Option<T>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct SolAccountUpdated {
    /// the account that was updated
    account: SeededAccount,
    /// The amount of SOL assigned to the wallet, in lamports.
    lamports: u64,
    /// the address of the owner of the account
    owner: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(untagged)]
pub enum SetAccountSuccess {
    Sol(SolAccountUpdated),
    Token(AccountUpdated),
}

#[derive(Serialize, Debug, Clone)]
pub struct SetTokenAccountsResponse {
    pub success: Option<Vec<SetAccountSuccess>>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct AccountUpdated {
    /// the account that was updated
    account: SeededAccount,
    /// The mint address of the token that was updated, or the "SOL" symbol
    token_mint: String,
    /// USDC/JUP/etc. If not provided, the token symbol will be inferred from the token mint address.
    token_symbol: Option<String>,
    /// the associated token address for the owner/token_mint/token_program_id combo
    associated_token_address: String,
    /// the amount of tokens assigned to the wallet, in human-readable units
    token_amount: u64,
    /// the address of the owner of a token
    owner: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub enum SeededAccount {
    /// Existing public key
    Provided(String),
    /// Details of a newly generated account
    Generated(NewAccount),
}

impl SeededAccount {
    pub fn new(input_account: Option<String>) -> Self {
        match input_account {
            Some(pubkey) => SeededAccount::Provided(pubkey),
            None => {
                let new_keypair = Keypair::new();
                SeededAccount::Generated(NewAccount {
                    secret_key: bs58::encode(new_keypair.to_bytes()).into_string(),
                    public_key: new_keypair.pubkey().to_string(),
                })
            }
        }
    }

    pub fn pubkey(&self) -> String {
        match self {
            SeededAccount::Provided(pubkey) => pubkey.clone(),
            SeededAccount::Generated(new_account) => new_account.public_key.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
pub struct NewAccount {
    /// Base58 encoded secret key
    secret_key: String,
    /// Base58 encoded public key
    public_key: String,
}

#[derive(Serialize, Debug, Clone)]
struct TokenDetails {
    /// If the token is SOL or not
    is_sol: bool,
    /// The mint address of the token
    mint_address: String,
    /// The symbol of the token
    symbol: Option<String>,
    /// The number of decimals of the token
    decimals: u32,
}

impl SetTokenAccountsResponse {
    pub fn success(details: Vec<SetAccountSuccess>) -> Self {
        Self {
            success: Some(details),
            error: None,
        }
    }

    pub fn error(message: String) -> Self {
        Self {
            success: None,
            error: Some(message),
        }
    }
}

/// Handles the MCP tool call to set a token balance for a specified account on a Surfnet instance.
///
/// This function can either fund an existing wallet or generate a new one.
/// It distinguishes between funding SOL (native currency) and SPL tokens (mint addresses).
///
/// # Arguments
/// * `surfnet_address`: The HTTP RPC endpoint of the target Surfnet instance .
/// * `wallet_address_opt`: An optional base58-encoded public key of the wallet to fund.
///   If `None`, a new keypair is generated, and its details are returned.
/// * `token_mint`: A string indicating the token to fund. If not provided, the token symbol will be inferred from the token mint address.
///   a base58-encoded SPL token mint address, or a known symbol.
/// * `token_amount`: An optional amount of the token to set (in its smallest unit, e.g., lamports for SOL).
///   If `None`, a default amount is used.
/// * `program_id_opt`: An optional base58-encoded public key of the token-issuing program.
///   If `None`, defaults to the standard SPL Token program ID.
///
/// # Returns
/// * `SetTokenAccountResponse`: Contains either details of the successful account update (including new wallet details if generated) or an error message.                             
pub async fn run(
    surfnet_address: String,
    owner_seeded_account: SeededAccount,
    token_mint: Option<String>,
    token_amount: Option<u64>,
    token_program_id: Option<String>,
    token_symbol: Option<String>,
) -> SetTokenAccountsResponse {
    let client = Client::new();
    let rpc_url = surfnet_address;
    let amount_to_set = token_amount.unwrap_or(DEFAULT_TOKEN_AMOUNT);
    let token_identifier = token_mint
        .or_else(|| token_symbol.clone())
        .unwrap_or_else(|| SOL_SYMBOL.to_string());

    let token_details = if token_identifier.to_uppercase() == SOL_SYMBOL {
        Ok(TokenDetails {
            is_sol: true,
            mint_address: SOL_SYMBOL.to_string(),
            symbol: Some(SOL_SYMBOL.to_string()),
            decimals: SOL_DECIMALS,
        })
    } else if let Ok(mint_pubkey) = Pubkey::try_from(token_identifier.as_str()) {
        let mint_str = mint_pubkey.to_string();
        let token_info = VERIFIED_TOKENS
            .iter()
            .find(|info| info.address == mint_str)
            .map(|info| (info.symbol.clone(), info.decimals as u32));
        if let Some((symbol, decimals)) = token_info {
            Ok(TokenDetails {
                is_sol: false,
                mint_address: mint_str,
                symbol: Some(symbol),
                decimals,
            })
        } else {
            Ok(TokenDetails {
                is_sol: false,
                mint_address: mint_str,
                symbol: None,
                decimals: 0,
            })
        }
    } else if let Some(token_info) = VERIFIED_TOKENS_BY_SYMBOL.get(&token_identifier.to_uppercase())
    {
        Ok(TokenDetails {
            is_sol: false,
            mint_address: token_info.address.clone(),
            symbol: Some(token_info.symbol.clone()),
            decimals: token_info.decimals as u32,
        })
    } else {
        Err(format!(
            "The token symbol provided '{}' is not a verified token. Please provide the token address instead.",
            token_identifier
        ))
    };

    let token_details = match token_details {
        Ok(details) => details,
        Err(e) => return SetTokenAccountsResponse::error(e),
    };

    let amount_in_smallest_unit = amount_to_set
        .checked_mul(10u64.pow(token_details.decimals))
        .unwrap_or(amount_to_set);

    let request_payload = if token_details.is_sol {
        let update_params = AccountUpdate {
            lamports: Some(amount_in_smallest_unit),
            ..Default::default()
        };
        let params_tuple = (owner_seeded_account.pubkey(), update_params);
        let params_value = match serde_json::to_value(params_tuple) {
            Ok(v) => v,
            Err(e) => {
                return SetTokenAccountsResponse::error(format!(
                    "Failed to serialize params for surfnet_setAccount: {}",
                    e
                ));
            }
        };
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: "surfnet_setAccount".to_string(),
            params: params_value,
        }
    } else {
        let update_params = TokenAccountUpdate {
            amount: Some(amount_in_smallest_unit),
            ..Default::default()
        };
        let params_tuple = (
            owner_seeded_account.pubkey(),
            token_details.mint_address.clone(),
            update_params,
            None::<String>,
        );
        let params_value = match serde_json::to_value(params_tuple) {
            Ok(v) => v,
            Err(e) => {
                return SetTokenAccountsResponse::error(format!(
                    "Failed to serialize params for surfnet_setTokenAccount: {}",
                    e
                ));
            }
        };
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: "surfnet_setTokenAccount".to_string(),
            params: params_value,
        }
    };

    match client.post(&rpc_url).json(&request_payload).send().await {
        Ok(response) => {
            if response.status().is_success() {
                match response.json::<JsonRpcResponse<serde_json::Value>>().await {
                    Ok(rpc_response) => {
                        if let Some(err) = rpc_response.error {
                            return SetTokenAccountsResponse::error(format!(
                                "RPC Error (code {}): {}",
                                err.code, err.message
                            ));
                        }

                        let success_details = if token_details.is_sol {
                            let sol_account_updated = SolAccountUpdated {
                                account: owner_seeded_account.clone(),
                                lamports: amount_in_smallest_unit,
                                owner: owner_seeded_account.pubkey(),
                            };
                            SetAccountSuccess::Sol(sol_account_updated)
                        } else {
                            let associated_token_address = {
                                let mint_pubkey =
                                    Pubkey::try_from(token_details.mint_address.as_str())
                                        .expect("Mint pubkey should be validated by now");
                                let owner_pubkey =
                                    Pubkey::try_from(owner_seeded_account.pubkey().as_str())
                                        .expect("Owner pubkey should be valid");
                                let token_program_id_pk = match &token_program_id {
                                    Some(id_str) => match Pubkey::try_from(id_str.as_str()) {
                                        Ok(pk) => pk,
                                        Err(_) => {
                                            return SetTokenAccountsResponse::error(format!(
                                                "Invalid program_id provided: {}",
                                                id_str
                                            ));
                                        }
                                    },
                                    None => spl_token_interface::id(),
                                };
                                get_associated_token_address_with_program_id(
                                    &owner_pubkey,
                                    &mint_pubkey,
                                    &token_program_id_pk,
                                )
                                .to_string()
                            };

                            let response_token_symbol =
                                token_symbol.clone().or(token_details.symbol);

                            let account_updated = AccountUpdated {
                                account: owner_seeded_account.clone(),
                                token_mint: token_details.mint_address,
                                token_symbol: response_token_symbol,
                                associated_token_address,
                                token_amount: amount_in_smallest_unit,
                                owner: owner_seeded_account.pubkey(),
                            };
                            SetAccountSuccess::Token(account_updated)
                        };
                        SetTokenAccountsResponse::success(vec![success_details])
                    }
                    Err(e) => SetTokenAccountsResponse::error(format!(
                        "Failed to parse JSON RPC response: {}",
                        e
                    )),
                }
            } else {
                let status = response.status();
                SetTokenAccountsResponse::error(format!(
                    "HTTP Error: {} - {}",
                    status,
                    response
                        .text()
                        .await
                        .unwrap_or_else(|_| "<failed to read body>".to_string())
                ))
            }
        }
        Err(_e) => SetTokenAccountsResponse::error(
            "RPC request failed (timeout or other network error)".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread::{self, JoinHandle},
    };

    use super::*;

    fn canned_rpc() -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind canned rpc");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0u8; 16 * 1024];
            assert_ne!(stream.read(&mut request).expect("read request"), 0);
            let body = r#"{"jsonrpc":"2.0","result":null,"id":1}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("write response");
        });
        (url, handle)
    }

    #[tokio::test]
    async fn resolves_shared_symbol_by_mint() {
        for mint in [
            "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh",
            "5XZw2LKTyrfvfiskJ78AMpackRjPcyCif1WhUsPDuVqQ",
        ] {
            let (url, server) = canned_rpc();
            let response = run(
                url,
                SeededAccount::new(None),
                Some(mint.to_string()),
                Some(1),
                None,
                None,
            )
            .await;
            server.join().expect("canned rpc thread");

            let success = response
                .success
                .unwrap_or_else(|| panic!("{mint}: {:?}", response.error));
            assert!(
                matches!(
                    success.as_slice(),
                    [SetAccountSuccess::Token(AccountUpdated {
                        token_amount: 100_000_000,
                        ..
                    })]
                ),
                "{mint}: {success:?}"
            );
        }
    }
}
