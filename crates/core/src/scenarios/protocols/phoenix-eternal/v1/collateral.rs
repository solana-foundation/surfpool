use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

use phoenix_rise_accounts::{
    PhoenixAccount, PhoenixAccountDecodeError, multi_arena::MultiArenaHeader, trader::TraderHeader,
};
use solana_account::Account;
use solana_pubkey::Pubkey;

use super::state_builder::PHOENIX_ETERNAL_PROGRAM_ID;
use crate::error::{SurfpoolError, SurfpoolResult};

pub fn trader_header(trader: &Pubkey, account: &Account) -> SurfpoolResult<TraderHeader> {
    if account.owner != PHOENIX_ETERNAL_PROGRAM_ID {
        return Err(SurfpoolError::invalid_account_owner(
            *trader,
            None::<PhoenixAccountDecodeError>,
        ));
    }
    let header = TraderHeader::try_read_from_account_bytes(&account.data).map_err(|error| {
        SurfpoolError::invalid_account_data(
            trader,
            "Expected a valid Phoenix Eternal Trader account",
            Some(error),
        )
    })?;
    if Pubkey::new_from_array(header.key) != *trader {
        return Err(SurfpoolError::invalid_account_data(
            trader,
            "Phoenix Trader header key does not match its account address",
            None::<String>,
        ));
    }
    Ok(header)
}

/// A hot Trader's TraderState is also stored in its GlobalTraderIndex record, and Phoenix's
/// margin view (Hawkeye) reads collateral from that record, not from the Trader. Only collateral
/// is written to the record here, so any other TraderState field is refused rather than set on
/// the Trader alone.
pub fn validate_hot_trader_fields(
    values: &HashMap<String, serde_json::Value>,
) -> SurfpoolResult<()> {
    if let Some(field) = values.keys().find(|field| {
        field.as_str() == "traderState"
            || (field.starts_with("traderState.")
                && field.as_str() != "traderState.quoteLotCollateral")
    }) {
        return Err(SurfpoolError::internal(format!(
            "Phoenix TraderState field '{field}' is unsupported; only traderState.quoteLotCollateral is mirrored into the GlobalTraderIndex for hot Traders"
        )));
    }
    Ok(())
}

/// Every hot Trader the GlobalTraderIndex tree reaches, paired with the byte range of its
/// TraderState record. The walk starts at the root, so freed nodes, which keep stale keys, flags
/// and collateral, are skipped. A key reached twice makes the tree invalid.
pub fn index_trader_state_ranges(index: &Account) -> SurfpoolResult<Vec<(Pubkey, Range<usize>)>> {
    let invalid = || SurfpoolError::internal("Invalid Phoenix GlobalTraderIndex tree");
    if index.owner != PHOENIX_ETERNAL_PROGRAM_ID {
        return Err(SurfpoolError::internal(
            "Expected a Phoenix-owned GlobalTraderIndex account",
        ));
    }
    let header = MultiArenaHeader::try_from_account_bytes(
        "GlobalTraderIndex",
        &index.data,
        PhoenixAccount::GlobalTraderIndexHeader.discriminant(),
    )
    .map_err(|error| {
        SurfpoolError::internal(format!("Invalid Phoenix GlobalTraderIndex: {error}"))
    })?;
    if header.num_arenas() != 1 || header.superblock().num_active_arenas() != 1 {
        return Err(SurfpoolError::internal(
            "Phoenix collateral overrides currently require a single-arena GlobalTraderIndex",
        ));
    }
    // MultiArenaHeader (48), superblock (32), tree root and padding (16), then
    // 1-based Sokoban nodes: four u32 registers, a 32-byte key, and IDL TraderState.
    const NODES_START: usize = 96;
    const NODE_LEN: usize = 64;
    let data = &index.data;
    if data.len() < NODES_START || !(data.len() - NODES_START).is_multiple_of(NODE_LEN) {
        return Err(invalid());
    }
    let capacity = (data.len() - NODES_START) / NODE_LEN;
    let read_u32 = |offset| u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
    let mut pending = vec![read_u32(80)];
    let mut visited = HashSet::new();
    let mut keys = HashSet::new();
    let mut ranges = Vec::new();
    while let Some(node) = pending.pop() {
        if node == 0 {
            continue;
        }
        if node >= header.superblock().bump_index()
            || node as usize > capacity
            || !visited.insert(node)
        {
            return Err(invalid());
        }
        let start = NODES_START + (node as usize - 1) * NODE_LEN;
        pending.extend([read_u32(start), read_u32(start + 4)]);
        let key = Pubkey::new_from_array(data[start + 16..start + 48].try_into().unwrap());
        if !keys.insert(key) {
            return Err(invalid());
        }
        ranges.push((key, start + 48..start + 64));
    }
    if visited.len() != header.superblock().size() as usize {
        return Err(invalid());
    }
    Ok(ranges)
}

pub fn index_trader_state_range(
    index: &Account,
    trader_key: &[u8; 32],
) -> SurfpoolResult<Range<usize>> {
    let trader_key = Pubkey::new_from_array(*trader_key);
    index_trader_state_ranges(index)?
        .into_iter()
        .find_map(|(key, range)| (key == trader_key).then_some(range))
        .ok_or_else(|| {
            SurfpoolError::internal("Hot Phoenix Trader has no reachable GlobalTraderIndex entry")
        })
}

pub fn current_quote_lot_collateral(
    header: &TraderHeader,
    index: Option<&Account>,
) -> SurfpoolResult<i64> {
    // The index is passed whenever Phoenix reads this Trader from its record, even a cold-flagged
    // Trader the local index still lists.
    if index.is_none() && !header.trader_state.is_hot() {
        return Ok(header.trader_state.quote_lot_collateral.as_inner());
    }
    let index = index.ok_or_else(|| {
        SurfpoolError::internal("Hot Phoenix Trader requires its GlobalTraderIndex account")
    })?;
    let range = index_trader_state_range(index, &header.key)?;
    Ok(i64::from_le_bytes(
        index.data[range.start..range.start + 8].try_into().unwrap(),
    ))
}

/// A target near i64::MIN overflows Phoenix's checked margin math instead of making the trader
/// liquidatable.
pub const MIN_QUOTE_LOT_COLLATERAL: i64 = i64::MIN / 2;

pub fn parse_quote_lot_collateral(value: &serde_json::Value) -> SurfpoolResult<i64> {
    let collateral = value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| {
            SurfpoolError::internal("Phoenix collateral must be a signed 64-bit integer")
        })?;
    ensure_collateral_floor(collateral)?;
    Ok(collateral)
}

pub fn ensure_collateral_floor(collateral: i64) -> SurfpoolResult<()> {
    if collateral < MIN_QUOTE_LOT_COLLATERAL {
        return Err(SurfpoolError::internal(format!(
            "Phoenix collateral must be at least {MIN_QUOTE_LOT_COLLATERAL} quote lots"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use phoenix_rise_accounts::trader::TRADER_CAPABILITY_HOT;

    use super::*;
    use crate::scenarios::protocols::phoenix_eternal::v1::state_builder::build_phoenix_collateral_scenario;

    const FIRST_KEY: [u8; 32] = [11; 32];
    const SECOND_KEY: [u8; 32] = [22; 32];

    #[test]
    fn collateral_values_preserve_signed_integer_precision() {
        for value in [
            MIN_QUOTE_LOT_COLLATERAL,
            -9_007_199_254_740_993,
            0,
            i64::MAX,
        ] {
            assert_eq!(
                parse_quote_lot_collateral(&serde_json::json!(value.to_string())).unwrap(),
                value
            );
            assert_eq!(
                parse_quote_lot_collateral(&serde_json::json!(value)).unwrap(),
                value
            );
        }
        for value in [
            serde_json::json!("9223372036854775808"),
            serde_json::json!(-1.5),
            serde_json::json!(null),
            serde_json::json!(i64::MIN),
            serde_json::json!((MIN_QUOTE_LOT_COLLATERAL - 1).to_string()),
        ] {
            assert!(parse_quote_lot_collateral(&value).is_err());
        }
    }

    fn write_u32(data: &mut [u8], offset: usize, value: u32) {
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn index_account() -> Account {
        let mut data = vec![0; 96 + 3 * 64];
        data[..8].copy_from_slice(&PhoenixAccount::GlobalTraderIndexHeader.discriminant());
        write_u32(&mut data, 48, 2);
        data[52..54].copy_from_slice(&1_u16.to_le_bytes());
        data[54..56].copy_from_slice(&1_u16.to_le_bytes());
        write_u32(&mut data, 56, 3);
        write_u32(&mut data, 60, 4);
        write_u32(&mut data, 64, 3);
        write_u32(&mut data, 80, 2);
        write_u32(&mut data, 160, 1);
        write_u32(&mut data, 104, 2);
        for (slot, key, collateral) in [
            (0, FIRST_KEY, 111_i64),
            (1, SECOND_KEY, 222_i64),
            (2, FIRST_KEY, 999_i64),
        ] {
            let start = 96 + slot * 64;
            data[start + 16..start + 48].copy_from_slice(&key);
            data[start + 48..start + 56].copy_from_slice(&collateral.to_le_bytes());
            write_u32(&mut data, start + 56, TRADER_CAPABILITY_HOT);
        }
        Account {
            data,
            owner: PHOENIX_ETERNAL_PROGRAM_ID,
            ..Account::default()
        }
    }

    fn trader_account(key: [u8; 32], collateral: i64, hot: bool) -> Account {
        let mut data = vec![0; core::mem::size_of::<TraderHeader>() + 16];
        data[..8].copy_from_slice(&PhoenixAccount::Trader.discriminant());
        data[24..56].copy_from_slice(&key);
        data[88..96].copy_from_slice(&collateral.to_le_bytes());
        if hot {
            write_u32(&mut data, 96, TRADER_CAPABILITY_HOT);
        }
        Account {
            data,
            owner: PHOENIX_ETERNAL_PROGRAM_ID,
            ..Account::default()
        }
    }

    #[test]
    fn index_lookup_selects_reachable_keys_and_rejects_malformed_trees() {
        let index = index_account();
        assert_eq!(
            index_trader_state_range(&index, &FIRST_KEY).unwrap(),
            144..160
        );
        assert_eq!(
            index_trader_state_range(&index, &SECOND_KEY).unwrap(),
            208..224
        );
        assert!(index_trader_state_range(&index, &[44; 32]).is_err());

        type Corrupt = fn(&mut Account);
        let corruptions: [(&str, Corrupt); 8] = [
            ("only a freed duplicate matches", |index| {
                index.data[112..144].copy_from_slice(&[33; 32])
            }),
            ("cycle", |index| write_u32(&mut index.data, 96, 2)),
            ("size disagrees with the reachable nodes", |index| {
                write_u32(&mut index.data, 48, 3)
            }),
            ("child beyond capacity", |index| {
                write_u32(&mut index.data, 60, 100);
                write_u32(&mut index.data, 164, 4);
            }),
            ("child above the bump index", |index| {
                write_u32(&mut index.data, 60, 2)
            }),
            ("duplicate reachable key", |index| {
                index.data[176..208].copy_from_slice(&FIRST_KEY)
            }),
            ("wrong owner", |index| index.owner = Pubkey::new_unique()),
            ("wrong discriminator", |index| index.data[..8].fill(0)),
        ];
        for (case, corrupt) in corruptions {
            let mut index = index_account();
            corrupt(&mut index);
            assert!(
                index_trader_state_range(&index, &FIRST_KEY).is_err(),
                "{case}"
            );
        }
        for (arenas, active) in [(0_u16, 1_u16), (2, 1), (1, 0), (1, 2)] {
            let mut index = index_account();
            index.data[52..54].copy_from_slice(&arenas.to_le_bytes());
            index.data[54..56].copy_from_slice(&active.to_le_bytes());
            assert!(index_trader_state_range(&index, &FIRST_KEY).is_err());
        }
        for len in [0, 79, 95, 96 + 3 * 64 - 1] {
            let mut index = index_account();
            index.data.truncate(len);
            assert!(index_trader_state_range(&index, &FIRST_KEY).is_err());
        }
    }

    #[test]
    fn index_listing_returns_only_reachable_records() {
        let index = index_account();
        let mut listed = index_trader_state_ranges(&index).unwrap();
        listed.sort_by_key(|(_, range)| range.start);
        assert_eq!(
            listed,
            vec![
                (Pubkey::new_from_array(FIRST_KEY), 144..160),
                (Pubkey::new_from_array(SECOND_KEY), 208..224),
            ],
            "slot 2 is a freed duplicate of FIRST_KEY, unreachable from the root and skipped"
        );

        let mut duplicated = index_account();
        duplicated.data[112..144].copy_from_slice(&SECOND_KEY);
        assert!(
            index_trader_state_ranges(&duplicated).is_err(),
            "a key reached twice"
        );
    }

    #[tokio::test]
    async fn materialize_patches_selected_hot_trader_and_index_record_only() {
        use super::super::state_builder::PHOENIX_GLOBAL_TRADER_INDEX;
        use crate::surfnet::svm::SurfnetSvm;

        // A Trader the index lists is read from its record even when its own flag says cold, and
        // its record caps the target even when its account copy holds less.
        for (key, other_key, collateral_offset, target, flagged_hot, account_collateral) in [
            (FIRST_KEY, SECOND_KEY, 144, 1_i64, true, 9_999),
            (
                SECOND_KEY,
                FIRST_KEY,
                208,
                -9_007_199_254_740_993,
                true,
                9_999,
            ),
            (FIRST_KEY, SECOND_KEY, 144, 1_i64, false, 9_999),
            (FIRST_KEY, SECOND_KEY, 144, 100_i64, false, 50),
        ] {
            let trader = Pubkey::new_from_array(key);
            let other_trader = Pubkey::new_from_array(other_key);
            let index_key = PHOENIX_GLOBAL_TRADER_INDEX;
            let mut before_trader = trader_account(key, account_collateral, flagged_hot);
            before_trader.lamports = 1;
            let mut before_other = trader_account(other_key, 8_888, true);
            before_other.lamports = 1;
            let mut before_index = index_account();
            before_index.lamports = 1;
            let scenario =
                build_phoenix_collateral_scenario(trader, &before_trader, &target.to_string())
                    .unwrap();
            let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
            svm.set_account(&trader, before_trader.clone()).unwrap();
            svm.set_account(&other_trader, before_other.clone())
                .unwrap();
            svm.set_account(&index_key, before_index.clone()).unwrap();
            svm.register_scenario(scenario, Some(100)).unwrap();

            assert_eq!(svm.get_account(&trader).unwrap().unwrap(), before_trader);
            assert_eq!(svm.get_account(&index_key).unwrap().unwrap(), before_index);
            svm.materialize_overrides_for_slot(&None, 100)
                .await
                .unwrap();

            let mut expected_trader = before_trader;
            expected_trader.data[88..96].copy_from_slice(&target.to_le_bytes());
            let mut expected_index = before_index;
            expected_index.data[collateral_offset..collateral_offset + 8]
                .copy_from_slice(&target.to_le_bytes());
            assert_eq!(svm.get_account(&trader).unwrap().unwrap(), expected_trader);
            assert_eq!(
                svm.get_account(&index_key).unwrap().unwrap(),
                expected_index
            );
            assert_eq!(
                svm.get_account(&other_trader).unwrap().unwrap(),
                before_other
            );
        }
    }

    #[tokio::test]
    async fn materialize_skips_invalid_or_missing_index_without_partial_trader_write() {
        use super::super::state_builder::PHOENIX_GLOBAL_TRADER_INDEX;
        use crate::surfnet::svm::SurfnetSvm;

        for failure in [
            "cycle",
            "missing key",
            "missing account",
            "mismatched key",
            "unsupported field",
            "raised collateral",
            "raised record of a cold-flagged Trader",
            "capabilities of a cold-flagged Trader",
        ] {
            let trader = Pubkey::new_from_array(FIRST_KEY);
            let index_key = PHOENIX_GLOBAL_TRADER_INDEX;
            let mut before_trader = trader_account(FIRST_KEY, 9_999, true);
            before_trader.lamports = 1;
            let mut before_index = index_account();
            before_index.lamports = 1;
            let mut scenario =
                build_phoenix_collateral_scenario(trader, &before_trader, "1").unwrap();
            match failure {
                "mismatched key" => before_trader.data[24..56].copy_from_slice(&SECOND_KEY),
                "unsupported field" => {
                    scenario.overrides[0]
                        .values
                        .insert("traderState.flags".to_string(), serde_json::json!(0));
                }
                "raised collateral" => {
                    scenario.overrides[0].values.insert(
                        "traderState.quoteLotCollateral".to_string(),
                        serde_json::json!("112"),
                    );
                }
                // Below the account's 9999 but above the 111 Phoenix reads from the record.
                "raised record of a cold-flagged Trader" => {
                    write_u32(&mut before_trader.data, 96, 0);
                    scenario.overrides[0].values.insert(
                        "traderState.quoteLotCollateral".to_string(),
                        serde_json::json!("500"),
                    );
                }
                // What the capabilities template writes; Phoenix reads the bits from the record.
                "capabilities of a cold-flagged Trader" => {
                    write_u32(&mut before_trader.data, 96, 0);
                    scenario.overrides[0].values =
                        HashMap::from([("traderState.flags".to_string(), serde_json::json!(54))]);
                }
                "cycle" => write_u32(&mut before_index.data, 96, 2),
                "missing key" => before_index.data[112..144].copy_from_slice(&[33; 32]),
                _ => {}
            }
            let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
            svm.set_account(&trader, before_trader.clone()).unwrap();
            if failure != "missing account" {
                svm.set_account(&index_key, before_index.clone()).unwrap();
            }
            svm.register_scenario(scenario, Some(100)).unwrap();

            // A rejected override is skipped, never an error out of the batch: that error
            // would abort block production.
            svm.materialize_overrides_for_slot(&None, 100)
                .await
                .unwrap_or_else(|error| panic!("{failure}: {error}"));
            assert_eq!(
                svm.get_account(&trader).unwrap().unwrap(),
                before_trader,
                "{failure}"
            );
            let after_index = svm.get_account(&index_key).unwrap();
            if failure == "missing account" {
                assert!(after_index.is_none());
            } else {
                assert_eq!(after_index.unwrap(), before_index, "{failure}");
            }
        }
    }

    #[tokio::test]
    async fn materialize_patches_cold_trader_and_skips_mismatched_header_key() {
        use crate::surfnet::svm::SurfnetSvm;

        let trader = Pubkey::new_from_array(FIRST_KEY);
        let matching = trader_account(FIRST_KEY, 9_999, false);
        for (on_chain_key, target, patched) in [
            (FIRST_KEY, "1", true),
            (SECOND_KEY, "1", false),
            (FIRST_KEY, "10000", false),
        ] {
            let mut scenario = build_phoenix_collateral_scenario(trader, &matching, "1").unwrap();
            scenario.overrides[0].values.insert(
                "traderState.quoteLotCollateral".to_string(),
                serde_json::json!(target),
            );
            let mut on_chain = trader_account(on_chain_key, 9_999, false);
            on_chain.lamports = 1;
            let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
            svm.set_account(&trader, on_chain.clone()).unwrap();
            svm.register_scenario(scenario, Some(100)).unwrap();
            svm.materialize_overrides_for_slot(&None, 100)
                .await
                .unwrap();

            let mut expected = on_chain;
            if patched {
                expected.data[88..96].copy_from_slice(&1_i64.to_le_bytes());
            }
            assert_eq!(svm.get_account(&trader).unwrap().unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn materialize_indexes_a_stressed_trader_by_owner() {
        use crate::surfnet::svm::SurfnetSvm;

        let trader = Pubkey::new_from_array(FIRST_KEY);
        let mut on_chain = trader_account(FIRST_KEY, 9_999, false);
        on_chain.lamports = 1;
        let scenario = build_phoenix_collateral_scenario(trader, &on_chain, "1").unwrap();
        let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        // A fetchBeforeUse refetch stores the Trader without indexing it by owner.
        svm.inner.set_account(trader, on_chain.clone()).unwrap();
        svm.register_scenario(scenario, Some(100)).unwrap();
        svm.materialize_overrides_for_slot(&None, 100)
            .await
            .unwrap();

        // getProgramAccounts serves the local copy of an account only when it is indexed by owner.
        let owned = svm
            .get_account_owned_by(&PHOENIX_ETERNAL_PROGRAM_ID)
            .unwrap();
        let (_, stressed) = owned
            .iter()
            .find(|(pubkey, _)| *pubkey == trader)
            .expect("the stressed Trader must be indexed by owner");
        let mut expected = on_chain;
        expected.data[88..96].copy_from_slice(&1_i64.to_le_bytes());
        assert_eq!(stressed, &expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fetch_before_use_refreshes_a_hot_traders_index_record() {
        use std::collections::HashMap;

        use base64::{Engine, prelude::BASE64_STANDARD};
        use solana_commitment_config::CommitmentConfig;

        use super::super::state_builder::{PHOENIX_GLOBAL_TRADER_INDEX, prepare_phoenix_override};
        use crate::{
            surfnet::{remote::SurfnetRemoteClient, svm::SurfnetSvm},
            tests::helpers::canned_rpc,
        };

        let trader = Pubkey::new_from_array(FIRST_KEY);
        let mut account = trader_account(FIRST_KEY, 9_999, true);
        account.lamports = 1;
        let mut upstream_index = index_account();
        upstream_index.lamports = 1;
        let range = index_trader_state_range(&upstream_index, &FIRST_KEY).unwrap();
        let upstream = i64::from_le_bytes(
            upstream_index.data[range.start..range.start + 8]
                .try_into()
                .unwrap(),
        );
        // The local VM holds a stressed record, the datasource the unstressed one. The local
        // record's flags and fee multipliers differ too, and only its collateral is refreshed.
        let mut stressed_index = upstream_index.clone();
        stressed_index.data[range.start..range.start + 8].copy_from_slice(&1_i64.to_le_bytes());
        stressed_index.data[range.start + 14..range.end].copy_from_slice(&[5, 6]);
        let url = canned_rpc(format!(
            r#"{{"context":{{"apiVersion":"2.1.0","slot":1}},"value":{{"data":["{}","base64"],"executable":false,"lamports":1,"owner":"{}","rentEpoch":0,"space":{}}}}}"#,
            BASE64_STANDARD.encode(&upstream_index.data),
            PHOENIX_ETERNAL_PROGRAM_ID,
            upstream_index.data.len()
        ))
        .await;
        let remote = Some((SurfnetRemoteClient::new(url), CommitmentConfig::confirmed()));
        let values = HashMap::from([(
            "traderState.quoteLotCollateral".to_string(),
            serde_json::json!((upstream / 2).to_string()),
        )]);

        for fetch_before_use in [false, true] {
            let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
            svm.set_account(&PHOENIX_GLOBAL_TRADER_INDEX, stressed_index.clone())
                .unwrap();
            let result = prepare_phoenix_override(
                &mut svm,
                &trader,
                &account,
                &values,
                &remote,
                fetch_before_use,
                100,
            )
            .await;
            if !fetch_before_use {
                assert!(
                    result.is_err(),
                    "without fetchBeforeUse the stressed record stays the ceiling"
                );
                continue;
            }
            let writes = result.unwrap().expect("a Trader override");
            let (_, index) = writes
                .iter()
                .find(|(pubkey, _)| *pubkey == PHOENIX_GLOBAL_TRADER_INDEX)
                .expect("the index record is written");
            assert_eq!(
                i64::from_le_bytes(index.data[range.start..range.start + 8].try_into().unwrap()),
                upstream / 2
            );
            assert_eq!(
                index.data[range.start + 8..range.end],
                stressed_index.data[range.start + 8..range.end],
                "the rest of the record stays local"
            );
        }
    }

    /// Core refetches the Trader only for the first override in a slot, so only that one refreshes
    /// the record. The refreshed collateral stays even when that override is refused, and it caps
    /// the later ones until one of them is applied.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_override_keeps_the_refreshed_record() {
        use std::collections::HashMap;

        use base64::{Engine, prelude::BASE64_STANDARD};
        use solana_commitment_config::CommitmentConfig;

        use super::super::state_builder::{PHOENIX_GLOBAL_TRADER_INDEX, prepare_phoenix_override};
        use crate::{
            surfnet::{remote::SurfnetRemoteClient, svm::SurfnetSvm},
            tests::helpers::canned_rpc,
        };

        let trader = Pubkey::new_from_array(FIRST_KEY);
        let mut account = trader_account(FIRST_KEY, 9_999, true);
        account.lamports = 1;
        let mut upstream_index = index_account();
        upstream_index.lamports = 1;
        let range = index_trader_state_range(&upstream_index, &FIRST_KEY).unwrap();
        upstream_index.data[range.start..range.start + 8].copy_from_slice(&100_i64.to_le_bytes());
        // The local record predates a drop of the trader's collateral upstream. Its fee multipliers
        // and the other trader's record differ locally too, and only the collateral is refreshed.
        let mut local_index = upstream_index.clone();
        local_index.data[range.start..range.start + 8].copy_from_slice(&1_000_i64.to_le_bytes());
        local_index.data[range.start + 14..range.end].copy_from_slice(&[5, 6]);
        let other = index_trader_state_range(&local_index, &SECOND_KEY).unwrap();
        local_index.data[other.start..other.start + 8].copy_from_slice(&2_000_i64.to_le_bytes());
        let mut refreshed = local_index.data.clone();
        refreshed[range.start..range.start + 8].copy_from_slice(&100_i64.to_le_bytes());
        let url = canned_rpc(format!(
            r#"{{"context":{{"apiVersion":"2.1.0","slot":1}},"value":{{"data":["{}","base64"],"executable":false,"lamports":1,"owner":"{}","rentEpoch":0,"space":{}}}}}"#,
            BASE64_STANDARD.encode(&upstream_index.data),
            PHOENIX_ETERNAL_PROGRAM_ID,
            upstream_index.data.len()
        ))
        .await;
        let remote = Some((SurfnetRemoteClient::new(url), CommitmentConfig::confirmed()));
        let collateral =
            |target: &str| ("traderState.quoteLotCollateral", serde_json::json!(target));

        // The first override is refused either for its target or for a field a hot Trader refuses.
        for first in [
            collateral("200"),
            ("traderState.flags", serde_json::json!(62)),
        ] {
            let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
            svm.set_account(&PHOENIX_GLOBAL_TRADER_INDEX, local_index.clone())
                .unwrap();
            for (fetch_before_use, (field, value), applied) in [
                (true, first, false),
                (false, collateral("500"), false),
                (false, collateral("50"), true),
                (false, collateral("80"), false),
            ] {
                let result = prepare_phoenix_override(
                    &mut svm,
                    &trader,
                    &account,
                    &HashMap::from([(field.to_string(), value.clone())]),
                    &remote,
                    fetch_before_use,
                    100,
                )
                .await;
                assert_eq!(result.is_ok(), applied, "{field} = {value}");
                if fetch_before_use {
                    let index = svm
                        .get_account(&PHOENIX_GLOBAL_TRADER_INDEX)
                        .unwrap()
                        .unwrap();
                    assert_eq!(index.data, refreshed, "{field} = {value}");
                }
                if let Ok(Some(writes)) = result {
                    for (pubkey, written) in writes {
                        svm.set_account(&pubkey, written).unwrap();
                    }
                }
            }
        }
    }

    /// A Trader the local index still lists is read from its record even with the HOT bit clear, so
    /// fetchBeforeUse refreshes that record from upstream: from the upstream record while upstream
    /// lists the Trader, otherwise from the refetched cold Trader account. A hot Trader's account
    /// copy is stale, so a hot Trader missing upstream keeps its record.
    #[tokio::test(flavor = "multi_thread")]
    async fn fetch_before_use_refreshes_the_record_of_a_listed_trader() {
        use std::collections::HashMap;

        use base64::{Engine, prelude::BASE64_STANDARD};
        use solana_commitment_config::CommitmentConfig;

        use super::super::state_builder::{PHOENIX_GLOBAL_TRADER_INDEX, prepare_phoenix_override};
        use crate::{
            surfnet::{remote::SurfnetRemoteClient, svm::SurfnetSvm},
            tests::helpers::canned_rpc,
        };

        let trader = Pubkey::new_from_array(FIRST_KEY);
        let mut local_index = index_account();
        local_index.lamports = 1;
        let range = index_trader_state_range(&local_index, &FIRST_KEY).unwrap();
        local_index.data[range.start..range.start + 8].copy_from_slice(&1_000_i64.to_le_bytes());
        let mut listing = local_index.clone();
        listing.data[range.start..range.start + 8].copy_from_slice(&100_i64.to_le_bytes());
        // The trader has left the upstream index: its reachable node now holds another key.
        let mut not_listing = local_index.clone();
        not_listing.data[96 + 16..96 + 48].copy_from_slice(&[33; 32]);
        assert!(index_trader_state_ranges(&not_listing).is_ok());
        assert!(index_trader_state_range(&not_listing, &FIRST_KEY).is_err());

        for (case, upstream, hot, target, applied, record) in [
            (
                "listed upstream, too high",
                &listing,
                false,
                500_i64,
                false,
                100_i64,
            ),
            (
                "left upstream, too high",
                &not_listing,
                false,
                500,
                false,
                100,
            ),
            ("listed upstream, lowered", &listing, false, 50, true, 50),
            ("left upstream, lowered", &not_listing, false, 50, true, 50),
            (
                "hot, left upstream",
                &not_listing,
                true,
                2_000,
                false,
                1_000,
            ),
        ] {
            let url = canned_rpc(format!(
                r#"{{"context":{{"apiVersion":"2.1.0","slot":1}},"value":{{"data":["{}","base64"],"executable":false,"lamports":1,"owner":"{}","rentEpoch":0,"space":{}}}}}"#,
                BASE64_STANDARD.encode(&upstream.data),
                PHOENIX_ETERNAL_PROGRAM_ID,
                upstream.data.len()
            ))
            .await;
            let remote = Some((SurfnetRemoteClient::new(url), CommitmentConfig::confirmed()));
            let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
            svm.set_account(&PHOENIX_GLOBAL_TRADER_INDEX, local_index.clone())
                .unwrap();
            // The Trader account as core just refetched it.
            let mut account = trader_account(FIRST_KEY, 100, hot);
            account.lamports = 1;
            let values = HashMap::from([(
                "traderState.quoteLotCollateral".to_string(),
                serde_json::json!(target.to_string()),
            )]);

            let result =
                prepare_phoenix_override(&mut svm, &trader, &account, &values, &remote, true, 100)
                    .await;

            assert_eq!(result.is_ok(), applied, "{case}");
            if let Ok(Some(writes)) = result {
                for (pubkey, written) in writes {
                    if pubkey == trader {
                        assert_eq!(written.data[88..96], target.to_le_bytes(), "{case}");
                    }
                    svm.set_account(&pubkey, written).unwrap();
                }
            }
            let index = svm
                .get_account(&PHOENIX_GLOBAL_TRADER_INDEX)
                .unwrap()
                .unwrap();
            assert_eq!(
                index.data[range.start..range.start + 8],
                record.to_le_bytes(),
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn materialize_refuses_cold_trader_writes_that_raise_collateral() {
        use crate::surfnet::svm::SurfnetSvm;

        let trader = Pubkey::new_from_array(FIRST_KEY);
        let matching = trader_account(FIRST_KEY, 9_999, false);
        // The whole TraderState is written too, so the collateral it ends with is checked.
        for (case, collateral, flags, patched) in [
            ("raise through traderState", 20_000_i64, 0, false),
            ("lower through traderState", 1, 0, true),
            (
                "lower to the floor through traderState",
                MIN_QUOTE_LOT_COLLATERAL,
                0,
                true,
            ),
            (
                "lower past the floor through traderState",
                MIN_QUOTE_LOT_COLLATERAL - 1,
                0,
                false,
            ),
            // The index does not list this Trader, so Phoenix could not find it as hot.
            ("set the HOT bit through traderState", 1, 63, false),
        ] {
            let mut scenario = build_phoenix_collateral_scenario(trader, &matching, "1").unwrap();
            scenario.overrides[0].values = HashMap::from([(
                "traderState".to_string(),
                serde_json::json!({
                    "quoteLotCollateral": collateral,
                    "flags": flags,
                    "padding": [0],
                    "globalPositionSequenceNumber": 0,
                    "makerFeeOverrideMultiplier": 0,
                    "takerFeeOverrideMultiplier": 0,
                }),
            )]);
            let mut on_chain = matching.clone();
            on_chain.lamports = 1;
            let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
            svm.set_account(&trader, on_chain.clone()).unwrap();
            svm.register_scenario(scenario, Some(100)).unwrap();
            svm.materialize_overrides_for_slot(&None, 100)
                .await
                .unwrap_or_else(|error| panic!("{case}: {error}"));

            let mut expected = on_chain;
            if patched {
                expected.data[88..96].copy_from_slice(&collateral.to_le_bytes());
            }
            assert_eq!(
                svm.get_account(&trader).unwrap().unwrap(),
                expected,
                "{case}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn materialize_indexes_fetched_global_trader_index_by_owner() {
        use base64::{Engine, prelude::BASE64_STANDARD};
        use solana_commitment_config::CommitmentConfig;

        use super::super::state_builder::PHOENIX_GLOBAL_TRADER_INDEX;
        use crate::{
            surfnet::{remote::SurfnetRemoteClient, svm::SurfnetSvm},
            tests::helpers::canned_rpc,
        };

        let trader = Pubkey::new_from_array(FIRST_KEY);
        let mut before_trader = trader_account(FIRST_KEY, 9_999, true);
        before_trader.lamports = 1;
        let mut remote_index = index_account();
        remote_index.lamports = 1;
        let mut scenario = build_phoenix_collateral_scenario(trader, &before_trader, "1").unwrap();
        // The canned RPC answers every request with the index, so the Trader must not be refetched.
        scenario.overrides[0].fetch_before_use = false;
        // The local VM holds the Trader but has never read the index, so the override fetches it.
        let url = canned_rpc(format!(
            r#"{{"context":{{"apiVersion":"2.1.0","slot":1}},"value":{{"data":["{}","base64"],"executable":false,"lamports":1,"owner":"{}","rentEpoch":0,"space":{}}}}}"#,
            BASE64_STANDARD.encode(&remote_index.data),
            PHOENIX_ETERNAL_PROGRAM_ID,
            remote_index.data.len()
        ))
        .await;
        let remote = Some((SurfnetRemoteClient::new(url), CommitmentConfig::confirmed()));
        let (mut svm, _events_rx, _geyser_rx) = SurfnetSvm::default();
        svm.set_account(&trader, before_trader).unwrap();
        svm.register_scenario(scenario, Some(100)).unwrap();

        svm.materialize_overrides_for_slot(&remote, 100)
            .await
            .unwrap();

        // getProgramAccounts serves the local copy of an account only when it is indexed by owner.
        let owned = svm
            .get_account_owned_by(&PHOENIX_ETERNAL_PROGRAM_ID)
            .unwrap();
        let (_, index) = owned
            .iter()
            .find(|(pubkey, _)| *pubkey == PHOENIX_GLOBAL_TRADER_INDEX)
            .expect("the fetched index must be indexed by owner");
        let mut expected = remote_index;
        expected.data[144..152].copy_from_slice(&1_i64.to_le_bytes());
        assert_eq!(index, &expected);
    }
}
