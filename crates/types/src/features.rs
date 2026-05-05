use std::str::FromStr;

use agave_feature_set::FEATURE_NAMES;
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

/// Returns the pubkey for a known feature name (kebab-case or snake_case).
///
/// This covers the set of features previously exposed as CLI names.
fn lookup_feature_by_name(name: &str) -> Option<Pubkey> {
    use agave_feature_set::*;

    // Normalize: accept both kebab-case and snake_case by converting underscores to hyphens
    let normalized = name.replace('_', "-");
    let s = normalized.as_str();

    let pubkey = match s {
        "move-precompile-verification-to-svm" => move_precompile_verification_to_svm::id(),
        "stricter-abi-and-runtime-constraints" => stricter_abi_and_runtime_constraints::id(),
        "enable-bpf-loader-set-authority-checked-ix" => {
            enable_bpf_loader_set_authority_checked_ix::id()
        }
        "enable-loader-v4" => enable_loader_v4::id(),
        "deplete-cu-meter-on-vm-failure" => deplete_cu_meter_on_vm_failure::id(),
        "abort-on-invalid-curve" => abort_on_invalid_curve::id(),
        "blake3-syscall-enabled" => blake3_syscall_enabled::id(),
        "curve25519-syscall-enabled" => curve25519_syscall_enabled::id(),
        "disable-deploy-of-alloc-free-syscall" => disable_deploy_of_alloc_free_syscall::id(),
        "disable-fees-sysvar" => disable_fees_sysvar::id(),
        "disable-sbpf-v0-execution" => disable_sbpf_v0_execution::id(),
        "enable-alt-bn128-compression-syscall" => enable_alt_bn128_compression_syscall::id(),
        "enable-alt-bn128-syscall" => enable_alt_bn128_syscall::id(),
        "enable-big-mod-exp-syscall" => enable_big_mod_exp_syscall::id(),
        "enable-get-epoch-stake-syscall" => enable_get_epoch_stake_syscall::id(),
        "enable-poseidon-syscall" => enable_poseidon_syscall::id(),
        "enable-sbpf-v1-deployment-and-execution" => enable_sbpf_v1_deployment_and_execution::id(),
        "enable-sbpf-v2-deployment-and-execution" => enable_sbpf_v2_deployment_and_execution::id(),
        "enable-sbpf-v3-deployment-and-execution" => enable_sbpf_v3_deployment_and_execution::id(),
        "get-sysvar-syscall-enabled" => get_sysvar_syscall_enabled::id(),
        "last-restart-slot-sysvar" => last_restart_slot_sysvar::id(),
        "reenable-sbpf-v0-execution" => reenable_sbpf_v0_execution::id(),
        "remaining-compute-units-syscall-enabled" => remaining_compute_units_syscall_enabled::id(),
        "remove-bpf-loader-incorrect-program-id" => remove_bpf_loader_incorrect_program_id::id(),
        "move-stake-and-move-lamports-ixs" => move_stake_and_move_lamports_ixs::id(),
        "stake-raise-minimum-delegation-to-1-sol" => stake_raise_minimum_delegation_to_1_sol::id(),
        "deprecate-legacy-vote-ixs" => deprecate_legacy_vote_ixs::id(),
        "mask-out-rent-epoch-in-vm-serialization" => mask_out_rent_epoch_in_vm_serialization::id(),
        "simplify-alt-bn128-syscall-error-codes" => simplify_alt_bn128_syscall_error_codes::id(),
        "fix-alt-bn128-multiplication-input-length" => {
            fix_alt_bn128_multiplication_input_length::id()
        }
        "increase-tx-account-lock-limit" => increase_tx_account_lock_limit::id(),
        "enable-extend-program-checked" => enable_extend_program_checked::id(),
        "formalize-loaded-transaction-data-size" => formalize_loaded_transaction_data_size::id(),
        "disable-zk-elgamal-proof-program" => disable_zk_elgamal_proof_program::id(),
        "reenable-zk-elgamal-proof-program" => reenable_zk_elgamal_proof_program::id(),
        "raise-cpi-nesting-limit-to-8" => raise_cpi_nesting_limit_to_8::id(),
        "account-data-direct-mapping" => account_data_direct_mapping::id(),
        "provide-instruction-data-offset-in-vm-r2" => {
            provide_instruction_data_offset_in_vm_r2::id()
        }
        "increase-cpi-account-info-limit" => increase_cpi_account_info_limit::id(),
        "vote-state-v4" => vote_state_v4::id(),
        "poseidon-enforce-padding" => poseidon_enforce_padding::id(),
        "fix-alt-bn128-pairing-length-check" => fix_alt_bn128_pairing_length_check::id(),
        // "lift-cpi-caller-restriction" not available in agave-feature-set 3.1.x
        "remove-accounts-executable-flag-checks" => remove_accounts_executable_flag_checks::id(),
        "loosen-cpi-size-restriction" => loosen_cpi_size_restriction::id(),
        "disable-rent-fees-collection" => disable_rent_fees_collection::id(),
        "deprecate-rent-exemption-threshold" => deprecate_rent_exemption_threshold::id(),
        "replace-spl-token-with-p-token" => replace_spl_token_with_p_token::id(),
        _ => return None,
    };

    Some(pubkey)
}

/// Parse a feature from either a name (kebab-case or snake_case) or a base58 pubkey string,
/// validating it against known agave feature gates.
pub fn parse_feature_pubkey(s: &str) -> Result<Pubkey, String> {
    // 1. Try name lookup (supports both kebab-case and snake_case)
    if let Some(pubkey) = lookup_feature_by_name(s) {
        return Ok(pubkey);
    }

    // 2. Try base58 pubkey parse
    let pubkey = Pubkey::from_str(s).map_err(|_| {
        format!(
            "Invalid feature pubkey: '{}'. Expected a base58-encoded pubkey of a known agave feature gate.",
            s
        )
    })?;

    if !FEATURE_NAMES.contains_key(&pubkey) {
        let mut available: Vec<_> = FEATURE_NAMES
            .iter()
            .map(|(k, name)| format!("  {} ({})", k, name))
            .collect();
        available.sort();
        return Err(format!(
            "Available features:\n{}\n\nUnknown feature: '{}'. Not a known agave feature gate. Available features listed above.",
            pubkey,
            available.join("\n")
        ));
    }

    Ok(pubkey)
}

/// Configuration for SVM features, specifying which features to enable or disable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SvmFeatureConfig {
    /// Features to explicitly enable (override defaults)
    pub enable: Vec<Pubkey>,
    /// Features to explicitly disable (override defaults)
    pub disable: Vec<Pubkey>,
}

impl SvmFeatureConfig {
    /// Creates a new empty feature configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the default mainnet feature configuration.
    ///
    /// This reflects features currently active on Solana mainnet-beta.
    /// Note: This may need periodic updates as mainnet features change.
    /// Last updated: 2026-05-05 (queried from mainnet RPC)
    pub fn default_mainnet_features() -> Self {
        use agave_feature_set::*;

        // Features that are NOT yet active on mainnet (should be disabled)
        let disable = vec![
            // Account data direct mapping not yet on mainnet
            account_data_direct_mapping::id(),
            // Blake3 syscall not yet on mainnet
            blake3_syscall_enabled::id(),
            // Legacy vote deprecation not yet on mainnet
            deprecate_legacy_vote_ixs::id(),
            // SBPF v0 disable not yet on mainnet
            disable_sbpf_v0_execution::id(),
            // big_mod_exp syscall not yet on mainnet
            enable_big_mod_exp_syscall::id(),
            // Extended program checked not yet on mainnet
            enable_extend_program_checked::id(),
            // Loader v4 not yet on mainnet
            enable_loader_v4::id(),
            // SBPF v3 not yet on mainnet (v1 and v2 ARE active)
            enable_sbpf_v3_deployment_and_execution::id(),
            // Increase tx account lock limit to 128 not yet on mainnet
            increase_tx_account_lock_limit::id(),
            // CPI nesting limit raise to 8 not yet on mainnet
            raise_cpi_nesting_limit_to_8::id(),
            // SBPF v0 reenable not yet on mainnet
            reenable_sbpf_v0_execution::id(),
            // ZK ElGamal reenable not yet on mainnet (disable IS active)
            reenable_zk_elgamal_proof_program::id(),
            // remaining_compute_units syscall not yet on mainnet
            remaining_compute_units_syscall_enabled::id(),
            // SPL token / p-token replacement not yet on mainnet
            replace_spl_token_with_p_token::id(),
            // Stake minimum delegation raise not yet on mainnet
            stake_raise_minimum_delegation_to_1_sol::id(),
            // Stricter ABI and runtime constraints not yet on mainnet
            stricter_abi_and_runtime_constraints::id(),
        ];

        Self {
            enable: vec![],
            disable,
        }
    }

    /// Adds a feature to enable.
    pub fn enable(mut self, feature: Pubkey) -> Self {
        if !self.enable.contains(&feature) {
            self.enable.push(feature);
        }
        // Remove from disable if present
        self.disable.retain(|f| f != &feature);
        self
    }

    /// Adds a feature to disable.
    pub fn disable(mut self, feature: Pubkey) -> Self {
        if !self.disable.contains(&feature) {
            self.disable.push(feature);
        }
        // Remove from enable if present
        self.enable.retain(|f| f != &feature);
        self
    }

    /// Checks if a feature should be enabled based on this configuration.
    /// Returns None if not explicitly configured (use default).
    pub fn is_enabled(&self, feature: &Pubkey) -> Option<bool> {
        if self.enable.contains(feature) {
            Some(true)
        } else if self.disable.contains(feature) {
            Some(false)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use agave_feature_set::*;

    use super::*;

    // ==================== parse_feature_pubkey tests ====================

    #[test]
    fn test_parse_feature_pubkey_valid() {
        let pubkey = parse_feature_pubkey(&enable_loader_v4::id().to_string()).unwrap();
        assert_eq!(pubkey, enable_loader_v4::id());
    }

    #[test]
    fn test_parse_feature_name_snake_case() {
        let pubkey = parse_feature_pubkey("enable_loader_v4").unwrap();
        assert_eq!(pubkey, enable_loader_v4::id());
    }

    #[test]
    fn test_parse_feature_name_kebab_case() {
        let pubkey = parse_feature_pubkey("enable-loader-v4").unwrap();
        assert_eq!(pubkey, enable_loader_v4::id());
    }

    #[test]
    fn test_parse_feature_name_prefers_name_over_pubkey() {
        // Verify that a known feature name resolves correctly
        let pubkey = parse_feature_pubkey("blake3_syscall_enabled").unwrap();
        assert_eq!(pubkey, blake3_syscall_enabled::id());
    }

    #[test]
    fn test_parse_feature_unknown_name() {
        let err = parse_feature_pubkey("nonexistent-feature-name").unwrap_err();
        assert!(err.contains("Invalid feature"));
        assert!(err.contains("nonexistent-feature-name"));
    }

    #[test]
    fn test_parse_feature_pubkey_unknown_pubkey() {
        // System program is not a feature gate
        let err = parse_feature_pubkey("11111111111111111111111111111111").unwrap_err();
        assert!(err.contains("Not a known agave feature gate"));
    }

    // ==================== SvmFeatureConfig basic tests ====================

    #[test]
    fn test_feature_config_new_is_empty() {
        let config = SvmFeatureConfig::new();
        assert!(config.enable.is_empty());
        assert!(config.disable.is_empty());
    }

    #[test]
    fn test_feature_config_default_is_empty() {
        let config = SvmFeatureConfig::default();
        assert!(config.enable.is_empty());
        assert!(config.disable.is_empty());
    }

    #[test]
    fn test_feature_config_enable() {
        let config = SvmFeatureConfig::new().enable(enable_loader_v4::id());
        assert_eq!(config.is_enabled(&enable_loader_v4::id()), Some(true));
        assert_eq!(config.enable.len(), 1);
        assert!(config.disable.is_empty());
    }

    #[test]
    fn test_feature_config_disable() {
        let config = SvmFeatureConfig::new().disable(disable_fees_sysvar::id());
        assert_eq!(config.is_enabled(&disable_fees_sysvar::id()), Some(false));
        assert!(config.enable.is_empty());
        assert_eq!(config.disable.len(), 1);
    }

    #[test]
    fn test_feature_config_is_enabled_not_configured() {
        let config = SvmFeatureConfig::new();
        assert_eq!(config.is_enabled(&blake3_syscall_enabled::id()), None);
    }

    // ==================== SvmFeatureConfig complex scenarios ====================

    #[test]
    fn test_feature_config_enable_then_disable() {
        let config = SvmFeatureConfig::new()
            .enable(enable_loader_v4::id())
            .disable(enable_loader_v4::id());

        assert_eq!(config.is_enabled(&enable_loader_v4::id()), Some(false));
        assert!(config.enable.is_empty());
        assert_eq!(config.disable.len(), 1);
    }

    #[test]
    fn test_feature_config_disable_then_enable() {
        let config = SvmFeatureConfig::new()
            .disable(enable_loader_v4::id())
            .enable(enable_loader_v4::id());

        assert_eq!(config.is_enabled(&enable_loader_v4::id()), Some(true));
        assert_eq!(config.enable.len(), 1);
        assert!(config.disable.is_empty());
    }

    #[test]
    fn test_feature_config_enable_idempotent() {
        let config = SvmFeatureConfig::new()
            .enable(enable_loader_v4::id())
            .enable(enable_loader_v4::id());

        assert_eq!(config.enable.len(), 1);
    }

    #[test]
    fn test_feature_config_disable_idempotent() {
        let config = SvmFeatureConfig::new()
            .disable(enable_loader_v4::id())
            .disable(enable_loader_v4::id());

        assert_eq!(config.disable.len(), 1);
    }

    #[test]
    fn test_feature_config_multiple_features() {
        let config = SvmFeatureConfig::new()
            .enable(enable_loader_v4::id())
            .enable(blake3_syscall_enabled::id())
            .disable(disable_fees_sysvar::id())
            .disable(disable_sbpf_v0_execution::id());

        assert_eq!(config.is_enabled(&enable_loader_v4::id()), Some(true));
        assert_eq!(config.is_enabled(&blake3_syscall_enabled::id()), Some(true));
        assert_eq!(config.is_enabled(&disable_fees_sysvar::id()), Some(false));
        assert_eq!(
            config.is_enabled(&disable_sbpf_v0_execution::id()),
            Some(false)
        );
        assert_eq!(config.enable.len(), 2);
        assert_eq!(config.disable.len(), 2);
    }

    // ==================== Mainnet defaults tests ====================

    #[test]
    fn test_mainnet_features_disabled_list() {
        let config = SvmFeatureConfig::default_mainnet_features();

        assert_eq!(
            config.is_enabled(&blake3_syscall_enabled::id()),
            Some(false)
        );
        assert_eq!(
            config.is_enabled(&deprecate_legacy_vote_ixs::id()),
            Some(false)
        );
        assert_eq!(
            config.is_enabled(&disable_sbpf_v0_execution::id()),
            Some(false)
        );
        assert_eq!(
            config.is_enabled(&reenable_sbpf_v0_execution::id()),
            Some(false)
        );
        assert_eq!(
            config.is_enabled(&reenable_zk_elgamal_proof_program::id()),
            Some(false)
        );
        assert_eq!(
            config.is_enabled(&enable_extend_program_checked::id()),
            Some(false)
        );
        assert_eq!(config.is_enabled(&enable_loader_v4::id()), Some(false));
        assert_eq!(
            config.is_enabled(&enable_sbpf_v3_deployment_and_execution::id()),
            Some(false)
        );
        assert_eq!(
            config.is_enabled(&raise_cpi_nesting_limit_to_8::id()),
            Some(false)
        );
        assert_eq!(
            config.is_enabled(&account_data_direct_mapping::id()),
            Some(false)
        );
        assert_eq!(
            config.is_enabled(&stake_raise_minimum_delegation_to_1_sol::id()),
            Some(false)
        );
    }

    #[test]
    fn test_mainnet_features_has_no_enables() {
        let config = SvmFeatureConfig::default_mainnet_features();
        assert!(config.enable.is_empty());
    }

    #[test]
    fn test_mainnet_features_override_with_enable() {
        let config = SvmFeatureConfig::default_mainnet_features().enable(enable_loader_v4::id());

        assert_eq!(config.is_enabled(&enable_loader_v4::id()), Some(true));
        assert_eq!(
            config.is_enabled(&blake3_syscall_enabled::id()),
            Some(false)
        );
        assert_eq!(
            config.is_enabled(&enable_extend_program_checked::id()),
            Some(false)
        );
    }

    #[test]
    fn test_mainnet_features_active_features_not_in_disable() {
        let config = SvmFeatureConfig::default_mainnet_features();

        // Features that ARE active on mainnet should not be in disable list
        assert_eq!(config.is_enabled(&disable_fees_sysvar::id()), None);
        assert_eq!(config.is_enabled(&curve25519_syscall_enabled::id()), None);
        assert_eq!(config.is_enabled(&enable_alt_bn128_syscall::id()), None);
        assert_eq!(config.is_enabled(&enable_poseidon_syscall::id()), None);
        assert_eq!(
            config.is_enabled(&enable_sbpf_v1_deployment_and_execution::id()),
            None
        );
        assert_eq!(
            config.is_enabled(&enable_sbpf_v2_deployment_and_execution::id()),
            None
        );
        assert_eq!(
            config.is_enabled(&disable_zk_elgamal_proof_program::id()),
            None
        );
        assert_eq!(config.is_enabled(&vote_state_v4::id()), None);
        assert_eq!(config.is_enabled(&poseidon_enforce_padding::id()), None);
        assert_eq!(
            config.is_enabled(&deprecate_rent_exemption_threshold::id()),
            None
        );
        assert_eq!(
            config.is_enabled(&move_precompile_verification_to_svm::id()),
            None
        );
        assert_eq!(
            config.is_enabled(&remove_accounts_executable_flag_checks::id()),
            None
        );
        assert_eq!(config.is_enabled(&loosen_cpi_size_restriction::id()), None);
    }

    // ==================== Serialization tests ====================

    #[test]
    fn test_feature_config_serde_roundtrip() {
        let config = SvmFeatureConfig::new()
            .enable(enable_loader_v4::id())
            .disable(disable_fees_sysvar::id());

        let json = serde_json::to_string(&config).unwrap();
        let parsed: SvmFeatureConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config, parsed);
    }

    // ==================== Edge cases ====================

    #[test]
    fn test_feature_config_clone() {
        let config = SvmFeatureConfig::new()
            .enable(enable_loader_v4::id())
            .disable(disable_fees_sysvar::id());

        let cloned = config.clone();
        assert_eq!(config, cloned);
    }
}
