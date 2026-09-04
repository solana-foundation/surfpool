use std::collections::BTreeMap;

use surfpool_types::{OverrideTemplate, YamlOverrideTemplateCollection};

pub const PYTH_V2_IDL_CONTENT: &str = include_str!("./protocols/pyth/v2/idl.json");
pub const PYTH_V2_OVERRIDES_CONTENT: &str = include_str!("./protocols/pyth/v2/overrides.yaml");

pub const JUPITER_V6_IDL_CONTENT: &str = include_str!("./protocols/jupiter/v6/idl.json");
pub const JUPITER_V6_OVERRIDES_CONTENT: &str =
    include_str!("./protocols/jupiter/v6/overrides.yaml");

pub const RAYDIUM_CLMM_IDL_CONTENT: &str = include_str!("./protocols/raydium/v3/idl.json");
pub const RAYDIUM_CLMM_OVERRIDES_CONTENT: &str =
    include_str!("./protocols/raydium/v3/overrides.yaml");

pub const RAYDIUM_AMM_V4_IDL_CONTENT: &str = include_str!("./protocols/raydium/v4/idl.json");
pub const RAYDIUM_AMM_V4_OVERRIDES_CONTENT: &str =
    include_str!("./protocols/raydium/v4/overrides.yaml");

pub const METEORA_DLMM_IDL_CONTENT: &str = include_str!("./protocols/meteora/dlmm/v1/idl.json");
pub const METEORA_DLMM_OVERRIDES_CONTENT: &str =
    include_str!("./protocols/meteora/dlmm/v1/overrides.yaml");
pub const KAMINO_V1_IDL_CONTENT: &str = include_str!("./protocols/kamino/v1/idl.json");
pub const KAMINO_V1_OVERRIDES_CONTENT: &str = include_str!("./protocols/kamino/v1/overrides.yaml");

pub const KAMINO_SCOPE_IDL_CONTENT: &str = include_str!("./protocols/kamino/scope/v1/idl.json");
pub const KAMINO_SCOPE_OVERRIDES_CONTENT: &str =
    include_str!("./protocols/kamino/scope/v1/overrides.yaml");

pub const KAMINO_FARMS_IDL_CONTENT: &str = include_str!("./protocols/kamino/farms/v1/idl.json");
pub const KAMINO_FARMS_OVERRIDES_CONTENT: &str =
    include_str!("./protocols/kamino/farms/v1/overrides.yaml");

pub const KAMINO_SWAP_IDL_CONTENT: &str = include_str!("./protocols/kamino/swap/v1/idl.json");
pub const KAMINO_SWAP_OVERRIDES_CONTENT: &str =
    include_str!("./protocols/kamino/swap/v1/overrides.yaml");

pub const KAMINO_VAULT_IDL_CONTENT: &str = include_str!("./protocols/kamino/vault/v1/idl.json");
pub const KAMINO_VAULT_OVERRIDES_CONTENT: &str =
    include_str!("./protocols/kamino/vault/v1/overrides.yaml");

pub const KAMINO_LIQUIDITY_IDL_CONTENT: &str =
    include_str!("./protocols/kamino/liquidity/v1/idl.json");
pub const KAMINO_LIQUIDITY_OVERRIDES_CONTENT: &str =
    include_str!("./protocols/kamino/liquidity/v1/overrides.yaml");

pub const DRIFT_V2_IDL_CONTENT: &str = include_str!("./protocols/drift/v2/idl.json");
pub const DRIFT_V2_OVERRIDES_CONTENT: &str = include_str!("./protocols/drift/v2/overrides.yaml");

pub const WHIRLPOOL_IDL_CONTENT: &str = include_str!("./protocols/whirlpool/idl.json");
pub const WHIRLPOOL_OVERRIDES_CONTENT: &str = include_str!("./protocols/whirlpool/overrides.yaml");

pub const SPL_TOKEN_IDL_CONTENT: &str = include_str!("./protocols/spl-token/idl.json");
pub const SPL_TOKEN_OVERRIDES_CONTENT: &str = include_str!("./protocols/spl-token/overrides.yaml");

pub const PUMP_V1_IDL_CONTENT: &str = include_str!("./protocols/pump/v1/idl.json");
pub const PUMP_V1_OVERRIDES_CONTENT: &str = include_str!("./protocols/pump/v1/overrides.yaml");

pub const PUMP_AMM_V1_IDL_CONTENT: &str = include_str!("./protocols/pump-amm/v1/idl.json");
pub const PUMP_AMM_V1_OVERRIDES_CONTENT: &str =
    include_str!("./protocols/pump-amm/v1/overrides.yaml");

/// Registry for managing override templates loaded from YAML files
#[derive(Clone, Debug, Default)]
pub struct TemplateRegistry {
    /// Map of template ID to template
    pub templates: BTreeMap<String, OverrideTemplate>,
}

impl TemplateRegistry {
    /// Create a new template registry
    pub fn new() -> Self {
        let mut default = Self::default();
        default.load_pyth_overrides();
        default.load_jupiter_overrides();
        default.load_raydium_overrides();
        default.load_meteora_overrides();
        default.load_kamino_overrides();
        default.load_drift_overrides();
        default.load_whirlpool_overrides();
        default.load_spl_token_overrides();
        default.load_pump_overrides();
        default
    }

    pub fn load_pyth_overrides(&mut self) {
        self.load_protocol_overrides(PYTH_V2_IDL_CONTENT, PYTH_V2_OVERRIDES_CONTENT, "pyth");
    }

    pub fn load_jupiter_overrides(&mut self) {
        self.load_protocol_overrides(
            JUPITER_V6_IDL_CONTENT,
            JUPITER_V6_OVERRIDES_CONTENT,
            "jupiter",
        );
    }

    pub fn load_meteora_overrides(&mut self) {
        self.load_protocol_overrides(
            METEORA_DLMM_IDL_CONTENT,
            METEORA_DLMM_OVERRIDES_CONTENT,
            "meteora",
        );
    }

    pub fn load_raydium_overrides(&mut self) {
        self.load_protocol_overrides(
            RAYDIUM_CLMM_IDL_CONTENT,
            RAYDIUM_CLMM_OVERRIDES_CONTENT,
            "raydium",
        );
        self.load_protocol_overrides(
            RAYDIUM_AMM_V4_IDL_CONTENT,
            RAYDIUM_AMM_V4_OVERRIDES_CONTENT,
            "raydium",
        );
    }

    pub fn load_kamino_overrides(&mut self) {
        self.load_protocol_overrides(KAMINO_V1_IDL_CONTENT, KAMINO_V1_OVERRIDES_CONTENT, "kamino");

        self.load_protocol_overrides(
            KAMINO_SCOPE_IDL_CONTENT,
            KAMINO_SCOPE_OVERRIDES_CONTENT,
            "kamino-scope",
        );

        self.load_protocol_overrides(
            KAMINO_FARMS_IDL_CONTENT,
            KAMINO_FARMS_OVERRIDES_CONTENT,
            "kamino-farms",
        );

        self.load_protocol_overrides(
            KAMINO_SWAP_IDL_CONTENT,
            KAMINO_SWAP_OVERRIDES_CONTENT,
            "kamino-swap",
        );

        self.load_protocol_overrides(
            KAMINO_VAULT_IDL_CONTENT,
            KAMINO_VAULT_OVERRIDES_CONTENT,
            "kamino-vault",
        );

        self.load_protocol_overrides(
            KAMINO_LIQUIDITY_IDL_CONTENT,
            KAMINO_LIQUIDITY_OVERRIDES_CONTENT,
            "kamino-liquidity",
        );
    }

    pub fn load_drift_overrides(&mut self) {
        self.load_protocol_overrides(DRIFT_V2_IDL_CONTENT, DRIFT_V2_OVERRIDES_CONTENT, "drift");
    }

    pub fn load_whirlpool_overrides(&mut self) {
        self.load_protocol_overrides(
            WHIRLPOOL_IDL_CONTENT,
            WHIRLPOOL_OVERRIDES_CONTENT,
            "whirlpool",
        );
    }

    pub fn load_spl_token_overrides(&mut self) {
        self.load_protocol_overrides(
            SPL_TOKEN_IDL_CONTENT,
            SPL_TOKEN_OVERRIDES_CONTENT,
            "spl-token",
        );
    }

    pub fn load_pump_overrides(&mut self) {
        self.load_protocol_overrides(PUMP_V1_IDL_CONTENT, PUMP_V1_OVERRIDES_CONTENT, "pump");
        self.load_protocol_overrides(
            PUMP_AMM_V1_IDL_CONTENT,
            PUMP_AMM_V1_OVERRIDES_CONTENT,
            "pump-amm",
        );
    }

    fn load_protocol_overrides(
        &mut self,
        idl_content: &str,
        overrides_content: &str,
        protocol_name: &str,
    ) {
        let idl = match serde_json::from_str(idl_content) {
            Ok(idl) => idl,
            Err(e) => panic!("unable to load {} idl: {}", protocol_name, e),
        };

        let collection =
            match serde_yaml::from_str::<YamlOverrideTemplateCollection>(overrides_content) {
                Ok(c) => c,
                Err(e) => panic!("unable to load {} overrides: {}", protocol_name, e),
            };

        // Convert all templates in the collection
        let templates = collection.to_override_templates(idl);

        // Register each template
        for template in templates {
            let template_id = template.id.clone();
            self.templates.insert(template_id.clone(), template);
        }
    }

    /// Get a template by ID
    pub fn get(&self, template_id: &str) -> Option<&OverrideTemplate> {
        self.templates.get(template_id)
    }

    /// Get all templates
    pub fn all(&self) -> Vec<&OverrideTemplate> {
        self.templates.values().collect()
    }

    /// Get templates for a specific protocol
    pub fn by_protocol(&self, protocol: &str) -> Vec<&OverrideTemplate> {
        self.templates
            .values()
            .filter(|t| t.protocol.eq_ignore_ascii_case(protocol))
            .collect()
    }

    /// Get templates matching any of the given tags
    pub fn by_tags(&self, tags: &[String]) -> Vec<&OverrideTemplate> {
        self.templates
            .values()
            .filter(|t| t.tags.iter().any(|tag| tags.contains(tag)))
            .collect()
    }

    /// Get the number of loaded templates
    pub fn count(&self) -> usize {
        self.templates.len()
    }

    /// Check if a template exists
    pub fn contains(&self, template_id: &str) -> bool {
        self.templates.contains_key(template_id)
    }

    /// List all template IDs
    pub fn list_ids(&self) -> Vec<String> {
        self.templates.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use anchor_lang_idl::types::IdlType;
    use std::{collections::BTreeSet, collections::HashMap, str::FromStr};

    use solana_pubkey::Pubkey;
    use surfpool_types::{AccountAddress, PdaSeed};

    use super::*;

    #[test]
    fn raydium_config_index_options_derive_their_documented_address() {
        let registry = TemplateRegistry::new();
        let template = registry.get("raydium-clmm-custom").expect("template");

        let AccountAddress::Pda { seeds, .. } = &template.address else {
            panic!("the pool address is a PDA");
        };
        let derived_pda_seed = seeds
            .iter()
            .find(|seed| matches!(seed, PdaSeed::DerivedPda { .. }))
            .expect("the pool PDA derives the amm config PDA");

        let options = &template
            .constants
            .get("amm_config_index")
            .expect("amm_config_index constant")
            .options;
        assert!(!options.is_empty(), "the fee tiers are the fixture here");

        for option in options {
            let expected = option
                .metadata
                .get("derived_address")
                .and_then(|address| address.as_str())
                .map(|address| Pubkey::from_str(address).expect("a valid address"))
                .unwrap_or_else(|| panic!("option {} documents no derived_address", option.id));

            let values = HashMap::from([(
                "config_index".to_string(),
                serde_json::Value::String(option.value.clone()),
            )]);
            let bytes = derived_pda_seed
                .to_bytes(Some(&values))
                .unwrap_or_else(|| panic!("option {} did not resolve", option.id));

            assert_eq!(
                Pubkey::try_from(bytes.as_slice()).expect("32 bytes"),
                expected,
                "option {}",
                option.id
            );
        }
    }

    /// The expected address is not ours: SOL/USDC at fee tier 1 holds a live CLMM
    /// PoolState on mainnet (owner CAMMCzo5…, discriminator 247 237 227 245 215 195 222 70),
    /// so this pins the whole chain — catalogue value, config PDA, pool PDA — against
    /// something outside the fixtures. Mint order is part of the recipe: Raydium expects
    /// the lower mint first, and the reversed order derives an address that holds nothing.
    #[test]
    fn raydium_template_derives_the_live_sol_usdc_pool() {
        let registry = TemplateRegistry::new();
        let template = registry.get("raydium-clmm-custom").expect("template");
        let sol = "So11111111111111111111111111111111111111112";
        let usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

        let values = |mint_0: &str, mint_1: &str| {
            HashMap::from([
                (
                    "config_index".to_string(),
                    serde_json::Value::String("1".to_string()),
                ),
                (
                    "token_mint_0".to_string(),
                    serde_json::Value::String(mint_0.to_string()),
                ),
                (
                    "token_mint_1".to_string(),
                    serde_json::Value::String(mint_1.to_string()),
                ),
            ])
        };

        assert_eq!(
            template
                .address
                .resolve(Some(&values(sol, usdc)))
                .expect("resolves"),
            Pubkey::from_str("3tD34VtprDSkYCnATtQLCiVgTkECU3d12KtjupeR6N2X").expect("address"),
        );

        assert_ne!(
            template
                .address
                .resolve(Some(&values(usdc, sol)))
                .expect("resolves"),
            Pubkey::from_str("3tD34VtprDSkYCnATtQLCiVgTkECU3d12KtjupeR6N2X").expect("address"),
            "swapping the mints must not land on the same pool"
        );
    }

    #[test]
    fn raydium_pool_address_needs_every_seed_to_resolve() {
        let registry = TemplateRegistry::new();
        let template = registry.get("raydium-clmm-custom").expect("template");

        let mints: Vec<String> = template
            .constants
            .get("token_mint")
            .expect("token_mint constant")
            .options
            .iter()
            .take(2)
            .map(|option| option.value.clone())
            .collect();
        assert_eq!(mints.len(), 2, "the pool address needs two mints");

        let mut values = HashMap::from([
            (
                "config_index".to_string(),
                serde_json::Value::String("1".to_string()),
            ),
            (
                "token_mint_0".to_string(),
                serde_json::Value::String(mints[0].clone()),
            ),
            (
                "token_mint_1".to_string(),
                serde_json::Value::String(mints[1].clone()),
            ),
        ]);

        assert!(
            template.address.resolve(Some(&values)).is_some(),
            "every seed resolves, so the pool address does too"
        );

        values.remove("config_index");
        assert_eq!(
            template.address.resolve(Some(&values)),
            None,
            "a seed that cannot resolve must not derive a shorter address"
        );
    }

    /// Both singleton addresses are documented in pump-public-docs: the Pump Global
    /// account at 4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf and the PumpSwap
    /// GlobalConfig at ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw.
    #[test]
    fn pump_singletons_derive_their_documented_addresses() {
        let registry = TemplateRegistry::new();

        let global = registry.get("pump-global").expect("template");
        assert_eq!(
            global.address.resolve(None).expect("resolves"),
            Pubkey::from_str("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf").expect("address"),
        );

        let config = registry.get("pump-amm-global-config").expect("template");
        assert_eq!(
            config.address.resolve(None).expect("resolves"),
            Pubkey::from_str("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw").expect("address"),
        );
    }

    /// The expected addresses are not ours. pump-public-docs (PUMP_SWAP_README.md)
    /// documents the canonical pool GseMAnNDvntR5uFePZ51yZBXzNSn7GdFPkfHwfr6d77J of the
    /// migrated coin 7LSsEoJG…pump, with the Pump pool-authority PDA 9XDYTfQK… as its
    /// creator, so deriving both pins the whole canonical chain: index 0 as u16 LE, the
    /// nested pool-authority PDA, the base mint, and wrapped SOL. The bonding curve
    /// address was checked against mainnet on 2026-08-06: owner 6EF8rrec…, discriminator
    /// 23 183 248 55 96 216 172 96 (BondingCurve), complete = true.
    #[test]
    fn pump_templates_derive_the_documented_migrated_coin_accounts() {
        let registry = TemplateRegistry::new();
        let mint = "7LSsEoJGhLeZzGvDofTdNg7M3JttxQqGWNLo6vWMpump";

        let curve = registry.get("pump-bonding-curve-custom").expect("template");
        let values = HashMap::from([(
            "token_mint".to_string(),
            serde_json::Value::String(mint.to_string()),
        )]);
        assert_eq!(
            curve.address.resolve(Some(&values)).expect("resolves"),
            Pubkey::from_str("3MUkKMbuornHohtAtzrToSzqkj1gEEhQqYVz8sZnmQg1").expect("address"),
        );

        let pool = registry.get("pump-amm-canonical-pool").expect("template");
        let values = HashMap::from([(
            "base_mint".to_string(),
            serde_json::Value::String(mint.to_string()),
        )]);

        let AccountAddress::Pda { seeds, .. } = &pool.address else {
            panic!("the pool address is a PDA");
        };
        let pool_authority = seeds
            .iter()
            .find(|seed| matches!(seed, PdaSeed::DerivedPda { .. }))
            .expect("the pool PDA derives the pool authority PDA")
            .to_bytes(Some(&values))
            .expect("the pool authority resolves");
        assert_eq!(
            Pubkey::try_from(pool_authority.as_slice()).expect("32 bytes"),
            Pubkey::from_str("9XDYTfQKwW8sHPqnFdUreMmtmffmkHVPGTNV2e3LKxNW").expect("address"),
        );

        assert_eq!(
            pool.address.resolve(Some(&values)).expect("resolves"),
            Pubkey::from_str("GseMAnNDvntR5uFePZ51yZBXzNSn7GdFPkfHwfr6d77J").expect("address"),
        );
    }

    #[test]
    fn pump_pool_address_needs_every_seed_to_resolve() {
        let registry = TemplateRegistry::new();
        let template = registry.get("pump-amm-canonical-pool").expect("template");

        assert_eq!(
            template.address.resolve(Some(&HashMap::new())),
            None,
            "a missing base mint must not derive a shorter address"
        );
    }

    #[test]
    fn test_registry_loads_all_protocols() {
        let registry = TemplateRegistry::new();

        // Pyth (1) + Jupiter (1) + Raydium CLMM (1) + Raydium AMM v4 (4) + Drift (4) + Meteora (2)
        // + Kamino (Lend 17, Scope 3, Farms 5, Swap 2, Vault 5, Liquidity 4 = 36)
        // + Whirlpool (6) + SPL Token (2) + Pump (2) + PumpSwap (3) = 62
        assert_eq!(
            registry.count(),
            62,
            "Registry should load 62 templates total"
        );

        assert!(registry.contains("pyth-price-feed-v2"));

        assert!(registry.contains("jupiter-token-ledger-override"));

        assert!(registry.contains("raydium-clmm-custom"));

        assert!(registry.contains("raydium-amm-pool-state"));
        assert!(registry.contains("raydium-amm-fees"));
        assert!(registry.contains("raydium-amm-swap-stats"));
        assert!(registry.contains("raydium-amm-custom"));

        assert!(registry.contains("meteora-dlmm-sol-usdc"));
        assert!(registry.contains("meteora-dlmm-usdt-sol"));

        assert!(registry.contains("kamino-reserve-state"));
        assert!(registry.contains("kamino-reserve-config"));
        assert!(registry.contains("kamino-reserve-status"));
        assert!(registry.contains("kamino-reserve-limits"));
        assert!(registry.contains("kamino-reserve-fees"));
        assert!(registry.contains("kamino-reserve-interest-rate"));
        assert!(registry.contains("kamino-reserve-oracle"));
        assert!(registry.contains("kamino-obligation-health"));
        assert!(registry.contains("kamino-obligation-positions"));
        assert!(registry.contains("kamino-obligation-orders"));
        assert!(registry.contains("kamino-lending-market-risk"));
        assert!(registry.contains("kamino-lending-market-elevation-groups"));
        assert!(registry.contains("kamino-reserve-rewards"));
        assert!(registry.contains("kamino-reserve-debt-term"));
        assert!(registry.contains("kamino-withdraw-ticket"));
        assert!(registry.contains("kamino-scope-price"));
        assert!(registry.contains("kamino-scope-price-source"));
        assert!(registry.contains("kamino-scope-twap"));
        assert!(registry.contains("kamino-farms-reward-emissions"));
        assert!(registry.contains("kamino-farms-reward-accumulator"));
        assert!(registry.contains("kamino-farms-user-rewards"));
        assert!(registry.contains("kamino-farms-farm-config"));
        assert!(registry.contains("kamino-farms-global-config"));
        assert!(registry.contains("kamino-swap-order"));
        assert!(registry.contains("kamino-swap-global-config"));
        assert!(registry.contains("kamino-vault-state"));
        assert!(registry.contains("kamino-vault-allocation"));
        assert!(registry.contains("kamino-vault-rewards"));
        assert!(registry.contains("kamino-vault-reserve-whitelist"));
        assert!(registry.contains("kamino-liquidity-strategy-balances"));
        assert!(registry.contains("kamino-liquidity-strategy-rewards"));
        assert!(registry.contains("kamino-liquidity-strategy-guards"));

        assert!(registry.contains("drift-perp-market"));
        assert!(registry.contains("drift-spot-market"));
        assert!(registry.contains("drift-user-state"));
        assert!(registry.contains("drift-global-state"));

        assert!(registry.contains("whirlpool-sol-usdc"));
        assert!(registry.contains("whirlpool-sol-usdt"));
        assert!(registry.contains("whirlpool-msol-sol"));
        assert!(registry.contains("whirlpool-orca-usdc"));
        assert!(registry.contains("whirlpool-popcat-sol"));
        assert!(registry.contains("whirlpool-custom"));

        assert!(registry.contains("spl-token-account-balance"));
        assert!(registry.contains("spl-token-mint-supply"));

        assert!(registry.contains("pump-bonding-curve-custom"));
        assert!(registry.contains("pump-global"));

        assert!(registry.contains("pump-amm-pool-state"));
        assert!(registry.contains("pump-amm-canonical-pool"));
        assert!(registry.contains("pump-amm-global-config"));
    }

    #[test]
    fn test_jupiter_template_loads_correctly() {
        let registry = TemplateRegistry::new();

        let jupiter_template = registry
            .get("jupiter-token-ledger-override")
            .expect("Jupiter template should exist");

        assert_eq!(jupiter_template.protocol, "Jupiter");
        assert_eq!(jupiter_template.account_type, "TokenLedger");
        assert_eq!(jupiter_template.name, "Override Jupiter Token Ledger");
        assert_eq!(jupiter_template.properties.len(), 2);

        let property_paths: Vec<&str> = jupiter_template.property_paths();
        assert!(property_paths.contains(&"tokenAccount"));
        assert!(property_paths.contains(&"amount"));
        assert!(jupiter_template.tags.contains(&"dex".to_string()));
        assert!(jupiter_template.tags.contains(&"aggregator".to_string()));
        assert!(jupiter_template.tags.contains(&"swap".to_string()));
        assert!(jupiter_template.tags.contains(&"defi".to_string()));
    }

    #[test]
    fn test_filter_by_protocol() {
        let registry = TemplateRegistry::new();

        let pyth_templates = registry.by_protocol("Pyth");
        assert_eq!(pyth_templates.len(), 1, "Should have 1 Pyth template");

        let jupiter_templates = registry.by_protocol("Jupiter");
        assert_eq!(jupiter_templates.len(), 1, "Should have 1 Jupiter template");

        let raydium_templates = registry.by_protocol("Raydium");
        assert_eq!(
            raydium_templates.len(),
            5,
            "Should have 5 Raydium templates (1 CLMM + 4 AMM v4)"
        );

        let kamino_templates = registry.by_protocol("kamino");
        assert_eq!(
            kamino_templates.len(),
            17,
            "Should have 17 Kamino Lend templates"
        );
        assert_eq!(
            registry.by_protocol("kamino-scope").len(),
            3,
            "Should have 3 Kamino Scope templates"
        );
        assert_eq!(
            registry.by_protocol("kamino-farms").len(),
            5,
            "Should have 5 Kamino Farms templates"
        );
        assert_eq!(
            registry.by_protocol("kamino-swap").len(),
            2,
            "Should have 2 Kamino Swap templates"
        );
        assert_eq!(
            registry.by_protocol("kamino-vault").len(),
            5,
            "Should have 5 Kamino Earn vault templates"
        );
        assert_eq!(
            registry.by_protocol("kamino-liquidity").len(),
            4,
            "Should have 4 Kamino Liquidity templates"
        );

        // Each Kamino-family protocol must cover the accounts worth overriding
        for (protocol, expected_accounts) in [
            (
                "kamino",
                vec!["Reserve", "Obligation", "LendingMarket", "WithdrawTicket"],
            ),
            (
                "kamino-scope",
                vec!["OraclePrices", "OracleMappings", "OracleTwaps"],
            ),
            (
                "kamino-farms",
                vec!["FarmState", "UserState", "GlobalConfig"],
            ),
            ("kamino-swap", vec!["Order", "GlobalConfig"]),
            ("kamino-vault", vec!["VaultState", "ReserveWhitelistEntry"]),
            ("kamino-liquidity", vec!["WhirlpoolStrategy"]),
        ] {
            let account_types: BTreeSet<&str> = registry
                .by_protocol(protocol)
                .iter()
                .map(|t| t.account_type.as_str())
                .collect();
            for expected in expected_accounts {
                assert!(
                    account_types.contains(expected),
                    "{} should have at least one template for the {} account",
                    protocol,
                    expected
                );
            }
        }

        let whirlpool_templates = registry.by_protocol("Whirlpool");
        assert_eq!(
            whirlpool_templates.len(),
            6,
            "Should have 6 Whirlpool templates"
        );

        let pump_templates = registry.by_protocol("Pump");
        assert_eq!(pump_templates.len(), 2, "Should have 2 Pump templates");

        let pump_swap_templates = registry.by_protocol("PumpSwap");
        assert_eq!(
            pump_swap_templates.len(),
            3,
            "Should have 3 PumpSwap templates"
        );
    }

    #[test]
    fn test_filter_by_tags() {
        let registry = TemplateRegistry::new();

        let oracle_templates = registry.by_tags(&[vec!["oracle".to_string()]].concat());
        assert_eq!(
            oracle_templates.len(),
            4,
            "Should find 4 oracle templates (Pyth + 3 Kamino Scope)"
        );

        let rewards_templates = registry.by_tags(&[vec!["rewards".to_string()]].concat());
        assert_eq!(
            rewards_templates.len(),
            5,
            "Should find 5 rewards templates (Kamino Farms)"
        );

        let dex_templates = registry.by_tags(&[vec!["dex".to_string()]].concat());
        assert_eq!(
            dex_templates.len(),
            1,
            "Should find 1 dex template (Jupiter)"
        );

        let aggregator_templates = registry.by_tags(&[vec!["aggregator".to_string()]].concat());
        assert_eq!(
            aggregator_templates.len(),
            1,
            "Should find 1 aggregator template (Jupiter)"
        );
    }

    #[test]
    fn test_jupiter_idl_has_token_ledger_account() {
        let registry = TemplateRegistry::new();
        let jupiter_template = registry.get("jupiter-token-ledger-override").unwrap();
        let has_token_ledger = jupiter_template
            .idl
            .accounts
            .iter()
            .any(|acc| acc.name == "TokenLedger");

        assert!(has_token_ledger, "IDL should contain TokenLedger account");
    }

    #[test]
    fn test_list_all_template_ids() {
        let registry = TemplateRegistry::new();
        let ids = registry.list_ids();

        assert!(ids.contains(&"raydium-clmm-custom".to_string()));
        assert!(ids.contains(&"raydium-amm-pool-state".to_string()));
        assert!(ids.contains(&"raydium-amm-custom".to_string()));
        assert!(ids.contains(&"jupiter-token-ledger-override".to_string()));
        assert!(ids.contains(&"pyth-price-feed-v2".to_string()));
        assert!(ids.contains(&"meteora-dlmm-sol-usdc".to_string()));
        assert!(ids.contains(&"kamino-reserve-state".to_string()));
        assert!(ids.contains(&"kamino-reserve-config".to_string()));
        assert!(ids.contains(&"kamino-obligation-health".to_string()));
        assert!(ids.contains(&"kamino-obligation-positions".to_string()));
        assert!(ids.contains(&"kamino-reserve-oracle".to_string()));
        assert!(ids.contains(&"kamino-lending-market-risk".to_string()));
        assert!(ids.contains(&"kamino-scope-price".to_string()));
        assert!(ids.contains(&"kamino-farms-user-rewards".to_string()));
        assert!(ids.contains(&"drift-perp-market".to_string()));
        assert!(ids.contains(&"whirlpool-sol-usdc".to_string()));
        assert!(ids.contains(&"whirlpool-sol-usdt".to_string()));
        assert!(ids.contains(&"whirlpool-msol-sol".to_string()));
        assert!(ids.contains(&"whirlpool-orca-usdc".to_string()));
        assert!(ids.contains(&"whirlpool-popcat-sol".to_string()));
        assert!(ids.contains(&"whirlpool-custom".to_string()));
    }

    #[test]
    fn test_raydium_clmm_custom_loads_verified_tokens() {
        let registry = TemplateRegistry::new();

        let raydium_template = registry
            .get("raydium-clmm-custom")
            .expect("Raydium CLMM custom template should exist");

        // Check that token_mint constant exists and has options from verified_tokens
        let token_mint_constant = raydium_template
            .constants
            .get("token_mint")
            .expect("token_mint constant should exist");

        // Should have many tokens loaded from verified_tokens.csv
        assert!(
            token_mint_constant.options.len() > 100,
            "Should have many verified tokens loaded, got {}",
            token_mint_constant.options.len()
        );

        // Check that common tokens are present, keyed by their mint address
        let sol_option = token_mint_constant
            .options
            .iter()
            .find(|o| o.value == "So11111111111111111111111111111111111111112")
            .expect("SOL token should be present");
        assert_eq!(
            sol_option.id, sol_option.value,
            "option ids are mint addresses so colliding symbols keep every mint"
        );

        let usdc_option = token_mint_constant
            .options
            .iter()
            .find(|o| o.value == "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
            .expect("USDC token should be present");
        assert_eq!(
            usdc_option.metadata.get("symbol").and_then(|v| v.as_str()),
            Some("USDC"),
            "USDC symbol should be in metadata"
        );

        // Check metadata is populated
        assert!(
            usdc_option.metadata.contains_key("symbol"),
            "Token should have symbol in metadata"
        );
        assert!(
            usdc_option.metadata.contains_key("decimals"),
            "Token should have decimals in metadata"
        );
    }

    #[test]
    fn test_raydium_amm_v4_has_only_openbook_market_options() {
        let registry = TemplateRegistry::new();

        // Test the raydium-amm-custom template which uses openbook_market constant_ref
        let raydium_v4_template = registry
            .get("raydium-amm-custom")
            .expect("Raydium AMM v4 custom template should exist");

        // Print ALL constants in this template to debug
        println!("Constants in raydium-amm-custom template:");
        for (name, constant) in &raydium_v4_template.constants {
            println!("  - {}: {} options", name, constant.options.len());
            for (i, opt) in constant.options.iter().take(3).enumerate() {
                println!("      {}: id={}, value={}", i, opt.id, opt.value);
            }
        }

        // Check that openbook_market constant exists
        let openbook_market_constant = raydium_v4_template
            .constants
            .get("openbook_market")
            .expect("openbook_market constant should exist");

        println!(
            "\nopenbook_market has {} options",
            openbook_market_constant.options.len()
        );

        // Print first 5 options to debug
        for (i, opt) in openbook_market_constant.options.iter().take(5).enumerate() {
            println!(
                "  Option {}: id={}, label={}, value={}",
                i, opt.id, opt.label, opt.value
            );
        }

        // Should have around 100 OpenBook markets (not thousands of tokens)
        assert!(
            openbook_market_constant.options.len() <= 200,
            "openbook_market should have only market options, not verified tokens. Got {} options",
            openbook_market_constant.options.len()
        );

        // Should NOT contain token symbols like "sol" or "usdc" as IDs
        // Market IDs should be like "sol-usdc" or "ray-sol"
        let has_standalone_sol = openbook_market_constant
            .options
            .iter()
            .any(|o| o.id == "sol");
        assert!(
            !has_standalone_sol,
            "openbook_market should NOT have standalone 'sol' option (that's a token, not a market)"
        );

        // Should have market pair IDs like "sol-usdc"
        let has_sol_usdc_market = openbook_market_constant
            .options
            .iter()
            .any(|o| o.id == "sol-usdc" || o.id.contains("-usdc") || o.id.contains("-sol"));
        assert!(
            has_sol_usdc_market,
            "openbook_market should have market pair IDs like 'sol-usdc'"
        );

        // Also make sure raydium-amm-custom does NOT have token_mint constant
        // (that's for CLMM v3, not AMM v4)
        let has_token_mint = raydium_v4_template.constants.contains_key("token_mint");
        assert!(
            !has_token_mint,
            "AMM v4 template should NOT have token_mint constant (that's for CLMM v3)"
        );
    }

    #[test]
    fn test_pyth_price_feed_pda_derivation() {
        use std::{collections::HashMap, str::FromStr};

        use solana_pubkey::Pubkey;

        // Test direct derivation first to verify the algorithm
        let program_id = Pubkey::from_str("pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT")
            .expect("Valid program ID");

        // SOL/USD feed ID (32 bytes)
        let feed_id_hex = "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";
        let feed_id_bytes = hex::decode(feed_id_hex).expect("Valid hex");
        assert_eq!(feed_id_bytes.len(), 32, "Feed ID must be 32 bytes");

        // Shard ID 0 as u16 little-endian (2 bytes)
        let shard_id: u16 = 0;
        let shard_bytes = shard_id.to_le_bytes();

        // Derive PDA with seeds: [shard_id (u16 LE), feed_id (32 bytes)]
        let seeds: &[&[u8]] = &[&shard_bytes, &feed_id_bytes];
        let (direct_pda, _bump) = Pubkey::find_program_address(seeds, &program_id);

        println!("Direct PDA derivation:");
        println!("  Program ID: {}", program_id);
        println!("  Shard bytes (u16 LE): {:?}", shard_bytes);
        println!("  Feed ID bytes (first 8): {:?}...", &feed_id_bytes[..8]);
        println!("  Derived PDA: {}", direct_pda);

        // Expected address (verified on-chain as SOL/USD price feed)
        let expected_address =
            Pubkey::from_str("7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE").expect("Valid pubkey");

        println!("  Expected PDA: {}", expected_address);

        // Now test via the registry
        let registry = TemplateRegistry::new();
        let pyth_template = registry
            .get("pyth-price-feed-v2")
            .expect("Pyth price feed template should exist");

        let sol_feed_id = "0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";

        let mut values = HashMap::new();
        values.insert(
            "feed_id".to_string(),
            serde_json::Value::String(sol_feed_id.to_string()),
        );

        let resolved_address = pyth_template
            .address
            .resolve(Some(&values))
            .expect("Should resolve PDA address");

        println!("\nRegistry PDA derivation:");
        println!("  Resolved PDA: {}", resolved_address);

        // Check if both match
        assert_eq!(
            direct_pda, resolved_address,
            "Direct and registry derivation should match"
        );

        assert_eq!(
            resolved_address, expected_address,
            "Pyth SOL/USD PDA should match expected address.\nGot: {}\nExpected: {}",
            resolved_address, expected_address
        );

        // Also verify direct derivation matches
        assert_eq!(
            direct_pda, expected_address,
            "Direct PDA derivation should match expected SOL/USD address"
        );
    }

    #[test]
    fn test_get_pda_seed_references() {
        use surfpool_types::AccountAddress;

        // Test with Bytes32Ref seed (Pyth feed_id)
        let account_json = r#"{
            "pda": {
                "programId": "pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT",
                "seeds": [
                    {"u16Le": 0},
                    {"bytes32Ref": "feed_id"}
                ]
            }
        }"#;

        let account: AccountAddress =
            serde_json::from_str(account_json).expect("Should parse AccountAddress from JSON");

        let refs = account.get_pda_seed_references();
        assert_eq!(
            refs,
            vec!["feed_id"],
            "Should extract feed_id as PDA seed reference"
        );

        // Test with PropertyRef seed (Raydium token mints)
        let raydium_json = r#"{
            "pda": {
                "programId": "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK",
                "seeds": [
                    {"string": "pool"},
                    {"propertyRef": "token_mint_0"},
                    {"propertyRef": "token_mint_1"},
                    {"u16Be": 100}
                ]
            }
        }"#;

        let raydium_account: AccountAddress =
            serde_json::from_str(raydium_json).expect("Should parse Raydium AccountAddress");

        let raydium_refs = raydium_account.get_pda_seed_references();
        assert_eq!(
            raydium_refs,
            vec!["token_mint_0", "token_mint_1"],
            "Should extract both token mint refs"
        );

        // Test with plain Pubkey (no PDA refs)
        let pubkey_json = r#"{"pubkey": "7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE"}"#;
        let pubkey_account: AccountAddress =
            serde_json::from_str(pubkey_json).expect("Should parse Pubkey AccountAddress");

        let pubkey_refs = pubkey_account.get_pda_seed_references();
        assert!(
            pubkey_refs.is_empty(),
            "Pubkey address should have no PDA refs"
        );
    }

    #[test]
    fn test_filter_pda_refs_from_override_values() {
        use std::collections::HashMap;

        use surfpool_types::AccountAddress;

        // Simulate what happens in materialize_overrides_for_slot
        let account_json = r#"{
            "pda": {
                "programId": "pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT",
                "seeds": [
                    {"u16Le": 0},
                    {"bytes32Ref": "feed_id"}
                ]
            }
        }"#;

        let account: AccountAddress = serde_json::from_str(account_json).unwrap();

        // Values from the override instance (includes both PDA ref and account data fields)
        let mut values: HashMap<String, serde_json::Value> = HashMap::new();
        values.insert(
            "feed_id".to_string(),
            serde_json::Value::String(
                "0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d".to_string(),
            ),
        );
        values.insert(
            "price_message.price".to_string(),
            serde_json::json!(15000000000i64),
        );
        values.insert("price_message.conf".to_string(), serde_json::json!(100));

        // Filter out PDA refs (this is what materialize_overrides_for_slot does)
        let pda_refs = account.get_pda_seed_references();
        let account_values: HashMap<String, serde_json::Value> = values
            .iter()
            .filter(|(key, _)| !pda_refs.contains(key))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // feed_id should be filtered out, only account data fields remain
        assert!(
            !account_values.contains_key("feed_id"),
            "feed_id should be filtered out as it's a PDA seed ref"
        );
        assert!(
            account_values.contains_key("price_message.price"),
            "price_message.price should remain"
        );
        assert!(
            account_values.contains_key("price_message.conf"),
            "price_message.conf should remain"
        );
        assert_eq!(
            account_values.len(),
            2,
            "Should have 2 account data fields after filtering"
        );
    }

    #[test]
    fn test_pda_derivation_from_json_override_instance() {
        use std::str::FromStr;

        use solana_pubkey::Pubkey;
        use surfpool_types::{AccountAddress, OverrideInstance};

        // First, test AccountAddress deserialization directly
        let account_json = r#"{
            "pda": {
                "programId": "pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT",
                "seeds": [
                    {"u16Le": 0},
                    {"bytes32Ref": "feed_id"}
                ]
            }
        }"#;

        let account: AccountAddress =
            serde_json::from_str(account_json).expect("Should parse AccountAddress from JSON");
        println!("Parsed AccountAddress: {:?}", account);

        // This JSON is exactly what the LLM sends
        let json = r#"{
            "id": "550e8400-e29b-41d4-a716-446655440004",
            "templateId": "pyth-price-feed-v2",
            "values": {
                "feed_id": "0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d",
                "price_message.price": 11000000000
            },
            "scenarioRelativeSlot": 2,
            "label": "SOL Price Rebounds to $110",
            "enabled": true,
            "fetchBeforeUse": false,
            "account": {
                "pda": {
                    "programId": "pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT",
                    "seeds": [
                        {"u16Le": 0},
                        {"bytes32Ref": "feed_id"}
                    ]
                }
            }
        }"#;

        let override_instance: OverrideInstance =
            serde_json::from_str(json).expect("Should parse OverrideInstance from JSON");

        println!("Parsed OverrideInstance:");
        println!("  Template ID: {}", override_instance.template_id);
        println!("  Values: {:?}", override_instance.values);
        println!("  Account: {:?}", override_instance.account);

        // Resolve the PDA using the values from the override instance
        let resolved_address = override_instance
            .account
            .resolve(Some(&override_instance.values))
            .expect("Should resolve PDA address from JSON");

        println!("  Resolved PDA: {}", resolved_address);

        // Expected SOL/USD price feed address
        let expected_address =
            Pubkey::from_str("7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE").expect("Valid pubkey");

        assert_eq!(
            resolved_address, expected_address,
            "PDA from JSON should match expected SOL/USD address.\nGot: {}\nExpected: {}",
            resolved_address, expected_address
        );
    }

    /// A property that does not exist in the IDL is dropped at materialization time with only
    /// a warning, so the scenario appears to run while changing nothing.
    #[test]
    fn test_all_template_property_paths_exist_in_idl() {
        let registry = TemplateRegistry::new();
        let mut errors = Vec::new();

        for template in registry.all() {
            for property in &template.properties {
                // constant_ref properties are UI dropdowns (e.g. token pickers), not
                // account fields, so they are not expected to resolve against the IDL.
                if property.is_constant_ref() {
                    continue;
                }
                if let Err(e) = surfpool_types::resolve_idl_type(
                    &template.idl,
                    &template.account_type,
                    &property.path,
                ) {
                    errors.push(format!("[{}] {}: {}", template.id, property.path, e));
                }
            }
        }

        assert!(
            errors.is_empty(),
            "{} template propert(ies) do not exist in their IDL:\n  {}",
            errors.len(),
            errors.join("\n  ")
        );
    }

    #[test]
    fn test_array_index_override_path_errors() {
        use txtx_addon_kit::{indexmap::IndexMap, types::types::Value};

        use crate::surfnet::svm::apply_override_to_decoded_account;

        let mut decoded = Value::Object(IndexMap::from([(
            "deposits".to_string(),
            Value::Array(Box::new(vec![Value::Integer(1), Value::Integer(2)])),
        )]));

        assert!(
            apply_override_to_decoded_account(&mut decoded, "deposits.1", &serde_json::json!(9))
                .is_ok()
        );
        match &decoded {
            Value::Object(map) => match map.get("deposits") {
                Some(Value::Array(items)) => assert_eq!(items[1], Value::Integer(9)),
                _ => panic!("expected deposits array"),
            },
            _ => panic!("expected object"),
        }

        // out-of-bounds index
        let err =
            apply_override_to_decoded_account(&mut decoded, "deposits.7", &serde_json::json!(1))
                .expect_err("index 7 is out of bounds for a 2-element array");
        assert!(
            format!("{err}").contains("out of bounds"),
            "unexpected error: {err}"
        );

        // non-numeric segment on an array
        let err = apply_override_to_decoded_account(
            &mut decoded,
            "deposits.first",
            &serde_json::json!(1),
        )
        .expect_err("'first' is not an array index");
        assert!(
            format!("{err}").contains("zero-based array index"),
            "unexpected error: {err}"
        );

        // empty segment
        assert!(
            apply_override_to_decoded_account(&mut decoded, "deposits..0", &serde_json::json!(1))
                .is_err()
        );
    }

    /// The Scope template must default to the Main Market's prices account, since every price
    /// recipe in the docs is written against its indices.
    #[test]
    fn test_kamino_scope_template_defaults_to_the_main_market() {
        let registry = TemplateRegistry::new();
        let template = registry
            .get("kamino-scope-price")
            .expect("kamino-scope-price template should exist");
        assert_eq!(
            template.address,
            AccountAddress::Pubkey("3t4JZcueEzTbVP6kLxXrL3VpWx45jDer4eqysweBchNH".to_string())
        );
    }

    /// These addresses are hardcoded facts about mainnet, so guard their shape and uniqueness.
    /// A liveness check would need network access.
    #[test]
    fn test_named_kamino_reserve_templates_have_baked_addresses() {
        use std::{collections::BTreeSet, str::FromStr};

        use solana_pubkey::Pubkey;

        let registry = TemplateRegistry::new();

        const NAMED: &[&str] = &["kamino-reserve-main-sol", "kamino-reserve-main-usdc"];

        let mut addresses = BTreeSet::new();
        for id in NAMED {
            let template = registry
                .get(id)
                .unwrap_or_else(|| panic!("named reserve template {} should exist", id));

            assert_eq!(
                template.account_type, "Reserve",
                "{} should target a Reserve",
                id
            );

            let surfpool_types::AccountAddress::Pubkey(address) = &template.address else {
                panic!("{} should carry a plain pubkey address, not a PDA", id);
            };
            assert!(
                Pubkey::from_str(address).is_ok(),
                "{} has an unparseable address: {}",
                id,
                address
            );
            assert!(
                addresses.insert(address.clone()),
                "{} reuses an address already used by another named template",
                id
            );

            let paths: Vec<&str> = template.property_paths();
            for required in [
                "config.liquidation_threshold_pct",
                "liquidity.market_price_sf",
            ] {
                assert!(
                    paths.contains(&required),
                    "{} should expose {}",
                    id,
                    required
                );
            }

            // Each must point at the template that moves its price, and name its Scope index -
            // the lookup a user would otherwise do by hand.
            let context = template.llm_context.as_deref().unwrap_or_default();
            assert!(
                context.contains("kamino-scope-price"),
                "{} should point at kamino-scope-price for moving its price",
                id
            );
            assert!(
                context.contains("index"),
                "{} should name the Scope index its price comes from",
                id
            );
        }

        assert_eq!(
            addresses.len(),
            NAMED.len(),
            "all addresses must be distinct"
        );
    }

    /// A path ending on an index must resolve to the array's ELEMENT type. Resolving it to the
    /// array instead sends the value down the untyped conversion, where an all-hex base58 pubkey
    /// such as the default one is mistaken for hex and panics the request.
    #[test]
    fn test_terminal_array_index_resolves_to_the_element_type() {
        use anchor_lang_idl::types::IdlType;

        let registry = TemplateRegistry::new();
        let template = registry
            .get("kamino-scope-price-source")
            .expect("kamino-scope-price-source should exist");

        for (path, expected) in [
            ("price_info_accounts.0", IdlType::Pubkey),
            ("price_types.0", IdlType::U8),
            ("ref_price.0", IdlType::U16),
        ] {
            let resolved =
                surfpool_types::resolve_idl_type(&template.idl, &template.account_type, path)
                    .unwrap_or_else(|e| panic!("{path} should resolve: {e}"));
            assert_eq!(
                *resolved, expected,
                "{path} should resolve to its element type, not the array"
            );
        }

        // An index mid-path already worked; keep it that way.
        let obligation = registry
            .get("kamino-obligation-positions")
            .expect("kamino-obligation-positions should exist");
        let resolved = surfpool_types::resolve_idl_type(
            &obligation.idl,
            &obligation.account_type,
            "deposits.0.deposit_reserve",
        )
        .expect("deposits.0.deposit_reserve should resolve");
        assert_eq!(*resolved, IdlType::Pubkey);
    }

    /// Descriptions come from the IDL's own `docs`, or from an explicit `description` in the
    /// YAML. Studio and any LLM reading a template rely on them.
    #[test]
    fn test_every_kamino_property_has_a_description() {
        let registry = TemplateRegistry::new();
        let mut missing = Vec::new();
        let mut described = 0;

        for protocol in [
            "kamino",
            "kamino-scope",
            "kamino-farms",
            "kamino-swap",
            "kamino-vault",
            "kamino-liquidity",
        ] {
            for template in registry.by_protocol(protocol) {
                for property in &template.properties {
                    match property.description.as_deref() {
                        Some(text) if !text.trim().is_empty() => described += 1,
                        _ => missing.push(format!("{}:{}", template.id, property.path)),
                    }
                }
            }
        }

        assert!(
            missing.is_empty(),
            "{} Kamino propert(ies) have no description ({} do):\n  {}",
            missing.len(),
            described,
            missing.join("\n  ")
        );
    }
}
