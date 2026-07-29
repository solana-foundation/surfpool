//! TypeScript-binding mirrors for wire types that cannot derive [`ts_rs::TS`]
//! directly.
//!
//! Foreign types (from `solana-account-decoder-client-types`) cannot derive
//! `TS`, and ts-rs has no field-level override for enum tuple-variant
//! payloads, so any local enum carrying a foreign type in variant position
//! must be mirrored too. Each mirror carries the same serde attributes as the
//! type it mirrors and is renamed to the mirrored type's name in the
//! generated TypeScript.
//!
//! The `*_mirror_matches*` tests below serialize real and mirror values and
//! assert identical JSON, and exhaustiveness guards force a compile error
//! when a mirrored local enum gains a variant — so a shape change in the
//! mirrored types fails the binding regeneration rather than silently
//! shipping wrong TypeScript.

use serde::Serialize;
use ts_rs::TS;

/// Mirrors [`solana_account_decoder_client_types::UiAccountEncoding`].
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, rename = "UiAccountEncoding")]
pub enum UiAccountEncodingDef {
    Binary,
    Base58,
    Base64,
    JsonParsed,
    #[serde(rename = "base64+zstd")]
    Base64Zstd,
}

/// Mirrors [`solana_account_decoder_client_types::ParsedAccount`].
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, rename = "ParsedAccount")]
pub struct ParsedAccountDef {
    pub program: String,
    #[ts(type = "unknown")]
    pub parsed: serde_json::Value,
    pub space: u64,
}

/// Mirrors [`solana_account_decoder_client_types::UiAccountData`].
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase", untagged)]
#[ts(export, rename = "UiAccountData")]
pub enum UiAccountDataDef {
    LegacyBinary(String),
    Json(ParsedAccountDef),
    Binary(String, UiAccountEncodingDef),
}

/// Mirrors [`solana_account_decoder_client_types::UiAccount`].
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, rename = "UiAccount")]
pub struct UiAccountDef {
    pub lamports: u64,
    pub data: UiAccountDataDef,
    pub owner: String,
    pub executable: bool,
    pub rent_epoch: u64,
    pub space: Option<u64>,
}

/// Mirrors [`crate::types::UiAccountChange`], with [`UiAccountDef`] payloads.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase", tag = "type", content = "data")]
#[ts(export, rename = "UiAccountChange")]
pub enum UiAccountChangeDef {
    Create(UiAccountDef),
    Update(UiAccountDef, UiAccountDef),
    Delete(UiAccountDef),
    Unchanged(Option<UiAccountDef>),
}

/// Mirrors [`crate::types::UiAccountProfileState`], with [`UiAccountChangeDef`] payloads.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase", tag = "type", content = "accountChange")]
#[ts(export, rename = "UiAccountProfileState")]
pub enum UiAccountProfileStateDef {
    Readonly,
    Writable(UiAccountChangeDef),
}

#[cfg(test)]
mod tests {
    use std::env;

    use serde_json::{Value, json, to_value};
    use solana_account_decoder_client_types::{
        ParsedAccount, UiAccount, UiAccountData, UiAccountEncoding,
    };

    use super::*;
    use crate::types::{SURFNET_CHEATCODE_METHODS, UiAccountChange, UiAccountProfileState};

    fn sample_parsed_account() -> (ParsedAccount, ParsedAccountDef) {
        let real = ParsedAccount {
            program: "spl-token".to_string(),
            parsed: json!({ "info": { "decimals": 6 }, "type": "mint" }),
            space: 82,
        };
        let mirror = ParsedAccountDef {
            program: "spl-token".to_string(),
            parsed: json!({ "info": { "decimals": 6 }, "type": "mint" }),
            space: 82,
        };
        (real, mirror)
    }

    fn sample_ui_account(
        data: UiAccountData,
        data_def: UiAccountDataDef,
    ) -> (UiAccount, UiAccountDef) {
        let real = UiAccount {
            lamports: 5_000_000_000,
            data,
            owner: "11111111111111111111111111111111".to_string(),
            executable: false,
            rent_epoch: u64::MAX,
            space: Some(165),
        };
        let mirror = UiAccountDef {
            lamports: 5_000_000_000,
            data: data_def,
            owner: "11111111111111111111111111111111".to_string(),
            executable: false,
            rent_epoch: u64::MAX,
            space: Some(165),
        };
        (real, mirror)
    }

    fn assert_same_json<A: Serialize, B: Serialize>(real: &A, mirror: &B) {
        assert_eq!(
            to_value(real).unwrap(),
            to_value(mirror).unwrap(),
            "mirror type serializes differently from the type it mirrors"
        );
    }

    /// Exhaustiveness guards for the mirrored enums: a new variant on any of
    /// these makes the matches non-exhaustive and fails compilation until the
    /// corresponding mirror (and its tests) are updated.
    #[allow(dead_code)]
    fn assert_mirrored_enum_variants_covered(
        encoding: UiAccountEncoding,
        data: UiAccountData,
        change: UiAccountChange,
        state: UiAccountProfileState,
    ) {
        match encoding {
            UiAccountEncoding::Binary
            | UiAccountEncoding::Base58
            | UiAccountEncoding::Base64
            | UiAccountEncoding::JsonParsed
            | UiAccountEncoding::Base64Zstd => {}
        }
        match data {
            UiAccountData::LegacyBinary(_)
            | UiAccountData::Json(_)
            | UiAccountData::Binary(_, _) => {}
        }
        match change {
            UiAccountChange::Create(_)
            | UiAccountChange::Update(_, _)
            | UiAccountChange::Delete(_)
            | UiAccountChange::Unchanged(_) => {}
        }
        match state {
            UiAccountProfileState::Readonly | UiAccountProfileState::Writable(_) => {}
        }
    }

    #[test]
    fn ui_account_encoding_mirror_matches() {
        for (real, mirror) in [
            (UiAccountEncoding::Binary, UiAccountEncodingDef::Binary),
            (UiAccountEncoding::Base58, UiAccountEncodingDef::Base58),
            (UiAccountEncoding::Base64, UiAccountEncodingDef::Base64),
            (
                UiAccountEncoding::JsonParsed,
                UiAccountEncodingDef::JsonParsed,
            ),
            (
                UiAccountEncoding::Base64Zstd,
                UiAccountEncodingDef::Base64Zstd,
            ),
        ] {
            assert_same_json(&real, &mirror);
        }
    }

    #[test]
    fn parsed_account_mirror_matches() {
        let (real, mirror) = sample_parsed_account();
        assert_same_json(&real, &mirror);
    }

    #[test]
    fn ui_account_mirror_matches_all_data_variants() {
        let (parsed, parsed_def) = sample_parsed_account();
        let variants = [
            (
                UiAccountData::LegacyBinary("legacy".to_string()),
                UiAccountDataDef::LegacyBinary("legacy".to_string()),
            ),
            (
                UiAccountData::Json(parsed),
                UiAccountDataDef::Json(parsed_def),
            ),
            (
                UiAccountData::Binary("AQID".to_string(), UiAccountEncoding::Base64),
                UiAccountDataDef::Binary("AQID".to_string(), UiAccountEncodingDef::Base64),
            ),
        ];
        for (data, data_def) in variants {
            let (real, mirror) = sample_ui_account(data, data_def);
            assert_same_json(&real, &mirror);
        }
        // `space: None` serializes as `null` (no skip attribute upstream).
        let (mut real, mut mirror) = sample_ui_account(
            UiAccountData::LegacyBinary(String::new()),
            UiAccountDataDef::LegacyBinary(String::new()),
        );
        real.space = None;
        mirror.space = None;
        assert_same_json(&real, &mirror);
        assert_eq!(to_value(&real).unwrap()["space"], Value::Null);
    }

    #[test]
    fn ui_account_change_mirror_matches_all_variants() {
        let ui = || {
            sample_ui_account(
                UiAccountData::LegacyBinary("data".to_string()),
                UiAccountDataDef::LegacyBinary("data".to_string()),
            )
        };
        let cases: Vec<(UiAccountChange, UiAccountChangeDef)> = vec![
            (
                UiAccountChange::Create(ui().0),
                UiAccountChangeDef::Create(ui().1),
            ),
            (
                UiAccountChange::Update(ui().0, ui().0),
                UiAccountChangeDef::Update(ui().1, ui().1),
            ),
            (
                UiAccountChange::Delete(ui().0),
                UiAccountChangeDef::Delete(ui().1),
            ),
            (
                UiAccountChange::Unchanged(Some(ui().0)),
                UiAccountChangeDef::Unchanged(Some(ui().1)),
            ),
            (
                UiAccountChange::Unchanged(None),
                UiAccountChangeDef::Unchanged(None),
            ),
        ];
        for (real, mirror) in cases {
            assert_same_json(&real, &mirror);
        }
    }

    #[test]
    fn ui_account_profile_state_mirror_matches_all_variants() {
        let (real_ui, mirror_ui) = sample_ui_account(
            UiAccountData::LegacyBinary("data".to_string()),
            UiAccountDataDef::LegacyBinary("data".to_string()),
        );
        assert_same_json(
            &UiAccountProfileState::Readonly,
            &UiAccountProfileStateDef::Readonly,
        );
        assert_same_json(
            &UiAccountProfileState::Writable(UiAccountChange::Create(real_ui)),
            &UiAccountProfileStateDef::Writable(UiAccountChangeDef::Create(mirror_ui)),
        );
    }

    #[test]
    fn scenario_enum_wire_keys_are_stable() {
        use crate::scenarios::{AccountAddress, PdaSeed};
        // Digit-adjacent camelCase variant names are the one rename edge case
        // where serde and ts-rs could disagree; pin serde's actual output so
        // regeneration reviews have a source of truth.
        assert_eq!(to_value(PdaSeed::U16Be(7)).unwrap(), json!({ "u16Be": 7 }));
        assert_eq!(
            to_value(PdaSeed::U16BeRef("x".to_string())).unwrap(),
            json!({ "u16BeRef": "x" })
        );
        assert_eq!(to_value(PdaSeed::U16Le(7)).unwrap(), json!({ "u16Le": 7 }));
        assert_eq!(
            to_value(PdaSeed::Bytes32Ref("x".to_string())).unwrap(),
            json!({ "bytes32Ref": "x" })
        );
        assert_eq!(
            to_value(AccountAddress::Pubkey("k".to_string())).unwrap(),
            json!({ "pubkey": "k" })
        );
    }

    /// Emits the method manifest alongside the ts-rs export tests. Writes
    /// only when `TS_RS_EXPORT_DIR` is set, i.e. during binding
    /// regeneration.
    #[test]
    fn export_bindings_methods_manifest() {
        let Ok(export_dir) = env::var("TS_RS_EXPORT_DIR") else {
            return;
        };
        let mut out = String::new();
        out.push_str("export const SURFNET_CHEATCODE_METHODS = [\n");
        for method in SURFNET_CHEATCODE_METHODS {
            out.push_str(&format!("  \"{method}\",\n"));
        }
        out.push_str("] as const;\n\n");
        out.push_str(
            "export type SurfnetCheatcodeMethod = (typeof SURFNET_CHEATCODE_METHODS)[number];\n",
        );
        std::fs::create_dir_all(&export_dir).unwrap();
        std::fs::write(std::path::Path::new(&export_dir).join("methods.ts"), out).unwrap();
    }
}
