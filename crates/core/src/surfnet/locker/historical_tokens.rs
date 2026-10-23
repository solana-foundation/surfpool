//! Token-account queries of a historical surfnet. SVM Replay names the accounts that may
//! answer at the fork position, from its provider's lists at the boundary's parent slot and
//! the boundary block's prefix; Surfpool adds the accounts this fork indexed, keeps those
//! whose current image answers and encodes them as its own. An answer whose completeness
//! cannot be shown is refused, never guessed.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use solana_account::Account;
use solana_account_decoder::{
    UiAccountEncoding,
    parse_account_data::{AccountAdditionalDataV3, SplTokenAdditionalDataV2},
    parse_token::token_amount_to_ui_amount_v3,
};
use solana_client::{
    rpc_config::RpcAccountInfoConfig,
    rpc_request::TokenAccountsFilter,
    rpc_response::{RpcKeyedAccount, RpcTokenAccountBalance},
};
use solana_pubkey::Pubkey;

use super::{SurfnetSvmLocker, SurfpoolContextualizedResult, is_supported_token_program};
use crate::{
    error::{SurfpoolError, SurfpoolResult},
    surfnet::{backend::ArchiveReader, svm::spl_token_additional_data},
    types::TokenAccount,
};

/// How many holders a ranking reads: first a page, then the provider's maximum when the
/// first cannot show the top 20.
const HOLDER_LIMITS: [u64; 2] = [1_000, 10_000];
const LARGEST: usize = 20;

fn malformed(what: &str) -> SurfpoolError {
    SurfpoolError::historical(
        "SOURCE_INTEGRITY",
        &format!("SVM Replay's {what} is malformed"),
        None,
    )
}

fn pubkeys(list: &Value, what: &str) -> SurfpoolResult<BTreeSet<Pubkey>> {
    list.as_array()
        .ok_or_else(|| malformed(what))?
        .iter()
        .map(|key| {
            key.as_str()
                .and_then(|key| key.parse().ok())
                .ok_or_else(|| malformed(what))
        })
        .collect()
}

/// The token account an image holds, if it is one.
fn token_account(account: &Account) -> Option<TokenAccount> {
    is_supported_token_program(&account.owner)
        .then(|| TokenAccount::unpack(&account.data).ok())
        .flatten()
}

impl SurfnetSvmLocker {
    /// Each mint's UI-amount inputs from its current image at the fork's clock, as Agave
    /// derives them; nothing is registered in this fork's indexes.
    fn historical_mint_data(
        &self,
        reader: &ArchiveReader,
        mints: impl IntoIterator<Item = Pubkey>,
    ) -> SurfpoolResult<BTreeMap<Pubkey, Option<SplTokenAdditionalDataV2>>> {
        let mints: Vec<Pubkey> = mints
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let now = self.with_svm_reader(|svm| svm.inner.clock().unix_timestamp);
        Ok(mints
            .iter()
            .zip(reader.accounts(&mints)?)
            .map(|(mint, image)| {
                let data = image.and_then(|image| spl_token_additional_data(&image.data, now));
                (*mint, data)
            })
            .collect())
    }

    /// `getTokenAccountsByOwner` on a historical fork: SVM Replay's candidates at the fork
    /// position and every account this fork indexed for `owner`, kept when the current image
    /// is a token account of `owner` that passes `filter`.
    pub fn historical_token_accounts_by_owner(
        &self,
        reader: &ArchiveReader,
        owner: Pubkey,
        filter: &TokenAccountsFilter,
        config: &RpcAccountInfoConfig,
    ) -> SurfpoolContextualizedResult<Vec<RpcKeyedAccount>> {
        let (mint, program_id) = match filter {
            TokenAccountsFilter::Mint(mint) => (Some(mint.to_string()), None),
            TokenAccountsFilter::ProgramId(program_id) => (None, Some(program_id.to_string())),
        };
        let listed = reader.token_accounts_by_owner(&owner, mint, program_id)?;
        let mut candidates = pubkeys(&listed["accounts"], "token account list")?;
        candidates.extend(
            self.with_svm_reader(|svm| svm.get_parsed_token_accounts_by_owner(&owner))
                .into_iter()
                .map(|(pubkey, _)| pubkey),
        );
        if candidates.len() > 100 {
            return Err(SurfpoolError::historical_unavailable(
                "getTokenAccountsByOwner",
                "at most 100 candidate accounts can be acquired per query",
            ));
        }
        let candidates: Vec<Pubkey> = candidates.into_iter().collect();
        let mut found = Vec::new();
        for (pubkey, image) in candidates.iter().zip(reader.accounts(&candidates)?) {
            let Some((account, token)) = image.and_then(|a| token_account(&a).map(|t| (a, t)))
            else {
                continue;
            };
            let matches = token.owner() == owner
                && match filter {
                    TokenAccountsFilter::Mint(mint) => token.mint() == *mint,
                    TokenAccountsFilter::ProgramId(program_id) => account.owner == *program_id,
                };
            if matches {
                found.push((*pubkey, account, token.mint()));
            }
        }
        let mints = self.historical_mint_data(reader, found.iter().map(|(_, _, mint)| *mint))?;
        let encoding = config.encoding.unwrap_or(UiAccountEncoding::Base64);
        let encoded =
            self.with_svm_reader(|svm| {
                found
                    .iter()
                    .map(|(pubkey, account, mint)| RpcKeyedAccount {
                        pubkey: pubkey.to_string(),
                        account: svm.encode_ui_account(
                            pubkey,
                            account,
                            encoding,
                            mints.get(mint).cloned().flatten().map(|data| {
                                AccountAdditionalDataV3 {
                                    spl_token_additional_data: Some(data),
                                }
                            }),
                            config.data_slice,
                        ),
                    })
                    .collect::<Vec<_>>()
            });
        Ok(self
            .with_contextualized_svm_reader(|_| ())
            .with_new_value(encoded))
    }

    /// `getTokenLargestAccounts` on a historical fork: SVM Replay's ranking at the parent slot,
    /// with the current balance of every account the boundary prefix or this fork changed.
    /// The top 20 is answered only when no unranked account can belong in it.
    pub fn historical_token_largest_accounts(
        &self,
        reader: &ArchiveReader,
        mint: &Pubkey,
    ) -> SurfpoolContextualizedResult<Vec<RpcTokenAccountBalance>> {
        let local: BTreeSet<Pubkey> = self
            .with_svm_reader(|svm| svm.get_token_accounts_by_mint(mint))
            .into_iter()
            .map(|(pubkey, _)| pubkey)
            .collect();
        let mut reason = "";
        for limit in HOLDER_LIMITS {
            let ranked = reader.token_holders(mint, limit)?;
            let flag = |name: &str| {
                ranked[name]
                    .as_bool()
                    .ok_or_else(|| malformed("holder ranking"))
            };
            let (at_parent, complete) = (flag("mintAtParent")?, flag("complete")?);
            let mut balances = BTreeMap::new();
            for holder in ranked["holders"]
                .as_array()
                .ok_or_else(|| malformed("holder ranking"))?
            {
                let account: Pubkey = holder["account"]
                    .as_str()
                    .and_then(|key| key.parse().ok())
                    .ok_or_else(|| malformed("holder ranking"))?;
                let amount: u64 = holder["amount"]
                    .as_str()
                    .and_then(|amount| amount.parse().ok())
                    .ok_or_else(|| malformed("holder ranking"))?;
                balances.insert(account, amount);
            }
            // A ranked balance holds unless this fork loaded the account (its current image
            // decides); prefix and local accounts always take their current image.
            let changed = pubkeys(&ranked["touched"], "holder ranking")?;
            let keys: Vec<Pubkey> = balances
                .keys()
                .chain(&changed)
                .chain(&local)
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let mut images = BTreeMap::new();
            let mut unloaded = Vec::new();
            for (pubkey, peeked) in keys.iter().zip(reader.peek_accounts(&keys)?) {
                match peeked {
                    Some(image) => {
                        images.insert(*pubkey, image);
                    }
                    None if changed.contains(pubkey) || local.contains(pubkey) => {
                        unloaded.push(*pubkey)
                    }
                    None => {}
                }
            }
            if unloaded.len() > 100 {
                return Err(SurfpoolError::historical_unavailable(
                    "getTokenLargestAccounts",
                    "at most 100 unloaded holder accounts can be acquired per query",
                ));
            }
            images.extend(unloaded.iter().copied().zip(reader.accounts(&unloaded)?));
            for (pubkey, image) in images {
                match image
                    .as_ref()
                    .and_then(token_account)
                    .filter(|t| t.mint() == *mint)
                {
                    Some(token) => balances.insert(pubkey, token.amount()),
                    None => balances.remove(&pubkey),
                };
            }
            let mut ranking: Vec<(Pubkey, u64)> = balances.into_iter().collect();
            ranking.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let nonzero = ranking.iter().filter(|(_, amount)| *amount > 0).count();
            let proven = if !at_parent {
                true
            } else if complete {
                reason = "the mint has fewer than 20 nonzero token accounts, and its zero-balance accounts at a historical slot are not enumerable";
                nonzero >= LARGEST
            } else {
                reason = "this fork's changes reach past the provider's ranking of the mint's largest accounts at the fork's slot";
                let floor: u64 = ranked["floor"]
                    .as_str()
                    .and_then(|floor| floor.parse().ok())
                    .ok_or_else(|| malformed("holder ranking"))?;
                ranking
                    .get(LARGEST - 1)
                    .is_some_and(|(_, amount)| *amount >= floor)
            };
            if proven {
                ranking.truncate(LARGEST);
                let data = self
                    .historical_mint_data(reader, [*mint])?
                    .remove(mint)
                    .flatten()
                    .unwrap_or_default();
                let largest = ranking
                    .into_iter()
                    .map(|(pubkey, amount)| RpcTokenAccountBalance {
                        address: pubkey.to_string(),
                        amount: token_amount_to_ui_amount_v3(amount, &data),
                    })
                    .collect();
                return Ok(self
                    .with_contextualized_svm_reader(|_| ())
                    .with_new_value(largest));
            }
            if complete {
                break;
            }
        }
        Err(SurfpoolError::historical_unavailable(
            "getTokenLargestAccounts",
            reason,
        ))
    }
}
