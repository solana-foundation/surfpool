use std::{collections::HashMap, str::FromStr};

use serde::{Deserialize, Serialize};
use solana_clock::Slot;
use solana_pubkey::Pubkey;
use uuid::Uuid;

use crate::Idl;

// ========================================
// Constants Types (for UI comboboxes and LLM choices)
// ========================================

/// A single option within a constant definition
/// Used to define selectable values like AMM configs or well-known tokens
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConstantOption {
    /// Unique identifier for this option (e.g., "standard", "sol", "usdc")
    pub id: String,
    /// Human-readable label shown in UI (e.g., "Standard (25 bps)", "SOL (Wrapped)")
    pub label: String,
    /// Description explaining when to use this option
    #[serde(default)]
    pub description: Option<String>,
    /// The actual value (typically a pubkey string)
    pub value: String,
    /// Additional metadata for context (e.g., decimals, tick_spacing, fee rates)
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// A constant definition containing multiple selectable options
/// Used to define things like AMM fee tiers, well-known tokens, etc.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConstantDefinition {
    /// Human-readable label for this constant type (e.g., "Fee Tier", "Token")
    pub label: String,
    /// Description of what this constant represents
    #[serde(default)]
    pub description: Option<String>,
    /// The available options to choose from
    pub options: Vec<ConstantOption>,
}

impl ConstantDefinition {
    /// Get an option by its ID
    pub fn get_option(&self, id: &str) -> Option<&ConstantOption> {
        self.options.iter().find(|o| o.id == id)
    }

    /// Get the value for an option by ID
    pub fn get_value(&self, id: &str) -> Option<&str> {
        self.get_option(id).map(|o| o.value.as_str())
    }
}

// ========================================
// Core Scenarios Types
// ========================================

/// Why a single PDA seed could not be converted to bytes.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SeedError {
    #[error("'{0}' is not a valid pubkey")]
    InvalidPubkey(String),
    #[error("seed references property '{0}' but no values were provided")]
    NoValues(String),
    #[error("property '{0}' not found in values")]
    UnknownProperty(String),
    #[error("property '{name}' is {found}, expected {expected}")]
    WrongType {
        name: String,
        expected: &'static str,
        found: &'static str,
    },
    #[error("property '{name}' value {value} does not fit in u16")]
    U16OutOfRange { name: String, value: u64 },
    #[error("'{0}' is not a 32-byte hex string")]
    InvalidBytes32(String),
    #[error("seed {index} of derived PDA for program {program_id}: {source}")]
    DerivedSeed {
        program_id: String,
        index: usize,
        source: Box<SeedError>,
    },
}

/// Why an account address could not be resolved to a pubkey.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ScenarioError {
    #[error("invalid account address: {0}")]
    InvalidAddress(#[from] SeedError),
    #[error("invalid program id '{0}'")]
    InvalidProgramId(String),
    #[error("PDA for program {program_id}, seed {index}: {source}")]
    Seed {
        program_id: String,
        index: usize,
        source: SeedError,
    },
    #[error("template '{template_id}': {source}")]
    Template {
        template_id: String,
        source: Box<ScenarioError>,
    },
}

/// Why a scheduled override could not be applied at its slot.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OverrideError {
    #[error("{0}")]
    Resolve(#[from] ScenarioError),
    #[error(
        "account {account} does not exist locally; overrides patch existing accounts (enable fetchBeforeUse or create the account first)"
    )]
    AccountNotFound { account: String },
    #[error("account {account} has {len} byte(s), too small for an 8-byte discriminator")]
    AccountTooSmall { account: String, len: usize },
    #[error("no IDL registered for program {program_id} (owner of account {account})")]
    NoIdlForOwner { program_id: String, account: String },
    #[error("failed to forge account data for {account}: {message}")]
    Forge { account: String, message: String },
}

/// Defines how an account address should be determined
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[doc = "Defines how an account address should be determined"]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS), ts(export))]
pub enum AccountAddress {
    /// A specific public key
    #[doc = "A specific public key"]
    Pubkey(String),
    /// A Program Derived Address with seeds
    #[doc = "A Program Derived Address with seeds"]
    #[serde(rename_all = "camelCase")]
    Pda {
        program_id: String,
        seeds: Vec<PdaSeed>,
    },
}

/// Seeds used for PDA derivation
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[doc = "Seeds used for PDA derivation"]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS), ts(export))]
pub enum PdaSeed {
    Pubkey(String),
    String(String),
    Bytes(Vec<u8>),
    /// Reference to a property value
    PropertyRef(String),
    /// A u16 value converted to big-endian bytes (useful for config indices)
    U16Be(u16),
    /// Reference to a property that should be converted to u16 big-endian bytes
    U16BeRef(String),
    /// A u16 value converted to little-endian bytes (useful for Pyth shard IDs)
    U16Le(u16),
    /// Reference to a property that's a 32-byte hex string (e.g., Pyth feed ID)
    Bytes32Ref(String),
    /// A nested PDA derivation - derives a PDA from inner seeds and uses it as the seed
    #[serde(rename_all = "camelCase")]
    DerivedPda {
        program_id: String,
        seeds: Vec<PdaSeed>,
    },
}

/// Look up a property referenced by a seed, distinguishing a missing values
/// map from a missing key.
fn property_value<'a>(
    values: Option<&'a HashMap<String, serde_json::Value>>,
    prop: &str,
) -> Result<&'a serde_json::Value, SeedError> {
    values
        .ok_or_else(|| SeedError::NoValues(prop.to_string()))?
        .get(prop)
        .ok_or_else(|| SeedError::UnknownProperty(prop.to_string()))
}

fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

impl PdaSeed {
    /// Check that literal pubkey content parses. Reference seeds are not
    /// validated here; they depend on instance values.
    pub(crate) fn validate_literals(&self) -> Result<(), SeedError> {
        match self {
            PdaSeed::Pubkey(s) => Pubkey::from_str(s)
                .map(|_| ())
                .map_err(|_| SeedError::InvalidPubkey(s.clone())),
            PdaSeed::DerivedPda { program_id, seeds } => {
                Pubkey::from_str(program_id)
                    .map_err(|_| SeedError::InvalidPubkey(program_id.clone()))?;
                for (i, seed) in seeds.iter().enumerate() {
                    seed.validate_literals()
                        .map_err(|e| SeedError::DerivedSeed {
                            program_id: program_id.clone(),
                            index: i,
                            source: Box::new(e),
                        })?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Convert a seed to bytes, optionally using values for PropertyRef resolution
    pub fn to_bytes(
        &self,
        values: Option<&HashMap<String, serde_json::Value>>,
    ) -> Result<Vec<u8>, SeedError> {
        match self {
            PdaSeed::Pubkey(pk_str) => Pubkey::from_str(pk_str)
                .map(|pk| pk.to_bytes().to_vec())
                .map_err(|_| SeedError::InvalidPubkey(pk_str.clone())),
            PdaSeed::String(s) => Ok(s.as_bytes().to_vec()),
            PdaSeed::Bytes(b) => Ok(b.clone()),
            PdaSeed::PropertyRef(prop) => {
                let v = property_value(values, prop)?;

                // Handle string values (could be pubkey or raw string)
                if let Some(s) = v.as_str() {
                    if let Ok(pk) = Pubkey::from_str(s) {
                        return Ok(pk.to_bytes().to_vec());
                    }
                    return Ok(s.as_bytes().to_vec());
                }
                // Handle numeric values (u64)
                if let Some(n) = v.as_u64() {
                    return Ok(n.to_le_bytes().to_vec());
                }
                Err(SeedError::WrongType {
                    name: prop.clone(),
                    expected: "string or u64",
                    found: json_type_name(v),
                })
            }
            PdaSeed::U16Be(n) => Ok(n.to_be_bytes().to_vec()),
            PdaSeed::U16BeRef(prop) => {
                let v = property_value(values, prop)?;

                // A scenario file may carry the index as a JSON number or as a
                // decimal string; both mean the same two big-endian bytes.
                if let serde_json::Value::String(s) = v {
                    let index = s.parse::<u16>().map_err(|_| SeedError::WrongType {
                        name: prop.clone(),
                        expected: "u16",
                        found: json_type_name(v),
                    })?;
                    return Ok(index.to_be_bytes().to_vec());
                }

                // Handle numeric values - convert to u16 big-endian
                if let Some(n) = v.as_u64() {
                    let n16 = u16::try_from(n).map_err(|_| SeedError::U16OutOfRange {
                        name: prop.clone(),
                        value: n,
                    })?;
                    return Ok(n16.to_be_bytes().to_vec());
                }
                Err(SeedError::WrongType {
                    name: prop.clone(),
                    expected: "u64",
                    found: json_type_name(v),
                })
            }
            PdaSeed::U16Le(n) => Ok(n.to_le_bytes().to_vec()),
            PdaSeed::Bytes32Ref(prop) => {
                let v = property_value(values, prop)?;

                // Handle hex string values (e.g., "0xef0d8b6f..." for Pyth feed IDs)
                if let Some(s) = v.as_str() {
                    // Remove 0x prefix if present
                    let hex_str = s.strip_prefix("0x").unwrap_or(s);
                    // Parse as 32-byte hex
                    match hex::decode(hex_str) {
                        Ok(bytes) if bytes.len() == 32 => return Ok(bytes),
                        _ => {
                            return Err(SeedError::InvalidBytes32(hex_str.to_string()));
                        }
                    }
                }
                Err(SeedError::WrongType {
                    name: prop.clone(),
                    expected: "hex string",
                    found: json_type_name(v),
                })
            }
            PdaSeed::DerivedPda { program_id, seeds } => {
                // Derive a nested PDA and use its pubkey as the seed
                let program_pubkey = Pubkey::from_str(program_id)
                    .map_err(|_| SeedError::InvalidPubkey(program_id.clone()))?;

                // Convert inner seeds to bytes
                let seed_bytes: Vec<Vec<u8>> = seeds
                    .iter()
                    .enumerate()
                    .map(|(i, seed)| {
                        seed.to_bytes(values).map_err(|e| SeedError::DerivedSeed {
                            program_id: program_id.clone(),
                            index: i,
                            source: Box::new(e),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;

                // Create seed slices for find_program_address
                let seed_slices: Vec<&[u8]> = seed_bytes.iter().map(|s| s.as_slice()).collect();

                // Derive the nested PDA
                let (pda, _bump) = Pubkey::find_program_address(&seed_slices, &program_pubkey);
                Ok(pda.to_bytes().to_vec())
            }
        }
    }
}

impl AccountAddress {
    /// Resolve the account address to a Pubkey
    /// For PDA addresses, this derives the address from the program_id and seeds
    /// For PropertyRef seeds, values map is used to resolve the reference
    pub fn resolve(
        &self,
        values: Option<&HashMap<String, serde_json::Value>>,
    ) -> Result<Pubkey, ScenarioError> {
        match self {
            AccountAddress::Pubkey(pubkey_str) => Pubkey::from_str(pubkey_str).map_err(|_| {
                ScenarioError::InvalidAddress(SeedError::InvalidPubkey(pubkey_str.clone()))
            }),
            AccountAddress::Pda { program_id, seeds } => {
                let program_pubkey = Pubkey::from_str(program_id)
                    .map_err(|_| ScenarioError::InvalidProgramId(program_id.clone()))?;

                // Convert all seeds to bytes
                let seed_bytes: Vec<Vec<u8>> = seeds
                    .iter()
                    .enumerate()
                    .map(|(i, seed)| {
                        seed.to_bytes(values).map_err(|e| ScenarioError::Seed {
                            program_id: program_id.clone(),
                            index: i,
                            source: e,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;

                // Create seed slices for find_program_address
                let seed_slices: Vec<&[u8]> = seed_bytes.iter().map(|s| s.as_slice()).collect();

                // Derive the PDA
                let (pda, _bump) = Pubkey::find_program_address(&seed_slices, &program_pubkey);
                Ok(pda)
            }
        }
    }

    /// Resolve the account address to a Pubkey without any values for PropertyRef
    /// This is a convenience method when no PropertyRef seeds are expected
    pub fn resolve_simple(&self) -> Result<Pubkey, ScenarioError> {
        self.resolve(None)
    }

    /// Get the names of values that are referenced by PDA seeds
    /// These should be filtered out when applying account data overrides
    /// since they're only used for address derivation, not account data modification
    pub fn get_pda_seed_references(&self) -> Vec<String> {
        match self {
            AccountAddress::Pubkey(_) => vec![],
            AccountAddress::Pda { seeds, .. } => {
                let mut refs = Vec::new();
                Self::collect_seed_references(seeds, &mut refs);
                refs
            }
        }
    }

    /// Recursively collect property references from seeds
    fn collect_seed_references(seeds: &[PdaSeed], refs: &mut Vec<String>) {
        for seed in seeds {
            match seed {
                PdaSeed::PropertyRef(name) => refs.push(name.clone()),
                PdaSeed::U16BeRef(name) => refs.push(name.clone()),
                PdaSeed::Bytes32Ref(name) => refs.push(name.clone()),
                PdaSeed::DerivedPda { seeds: inner, .. } => {
                    Self::collect_seed_references(inner, refs);
                }
                _ => {}
            }
        }
    }
}

/// The type of a property - determines how it's rendered in the UI
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PropertyKind {
    /// A regular field from the IDL (default behavior)
    #[default]
    Field,
    /// A reference to a constant definition (renders as dropdown/combobox in UI)
    ConstantRef,
}

/// Defines a property in a template with full metadata
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Property {
    /// The path to the field in the IDL (e.g., "liquidity", "fees.swap_fee_numerator")
    pub path: String,
    /// The type of property - determines rendering behavior
    #[serde(default, rename = "type")]
    pub kind: PropertyKind,
    /// Human-readable label for the UI (optional, defaults to path)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Description of the field (optional, can come from IDL)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// For constant_ref type: the name of the constant definition to use
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constant: Option<String>,
}

impl Property {
    /// Create a new field property
    pub fn field(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: PropertyKind::Field,
            label: None,
            description: None,
            constant: None,
        }
    }

    /// Create a new constant_ref property
    pub fn constant_ref(path: impl Into<String>, constant: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: PropertyKind::ConstantRef,
            label: None,
            description: None,
            constant: Some(constant.into()),
        }
    }

    /// Check if this is a constant reference
    pub fn is_constant_ref(&self) -> bool {
        matches!(self.kind, PropertyKind::ConstantRef)
    }

    /// Get the constant name if this is a constant reference
    pub fn constant_name(&self) -> Option<&str> {
        self.constant.as_deref()
    }

    /// Get the display label (falls back to path if no label set)
    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.path)
    }
}

/// Legacy tagged property representation.
///
/// Use [`Property`] for new code. This type remains deserializable for callers
/// that still send the old tagged JSON shape, such as
/// `{"type":"field","name":"price"}`.
#[deprecated(
    since = "1.5.0",
    note = "use Property; PropertyType remains only for legacy tagged JSON compatibility"
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum PropertyType {
    /// A regular field from the IDL (default behavior)
    Field { name: String },
    /// A reference to a constant definition (renders as combobox in UI)
    ConstantRef { name: String, constant: String },
}

#[allow(deprecated)]
impl PropertyType {
    /// Get the property name regardless of type
    pub fn name(&self) -> &str {
        match self {
            PropertyType::Field { name } => name,
            PropertyType::ConstantRef { name, .. } => name,
        }
    }

    /// Check if this is a constant reference
    pub fn is_constant_ref(&self) -> bool {
        matches!(self, PropertyType::ConstantRef { .. })
    }

    /// Get the constant name if this is a constant reference
    pub fn constant_name(&self) -> Option<&str> {
        match self {
            PropertyType::ConstantRef { constant, .. } => Some(constant),
            _ => None,
        }
    }
}

#[allow(deprecated)]
impl From<PropertyType> for Property {
    fn from(pt: PropertyType) -> Self {
        match pt {
            PropertyType::Field { name } => Property::field(name),
            PropertyType::ConstantRef { name, constant } => Property::constant_ref(name, constant),
        }
    }
}

/// A reusable template for creating account overrides
/// Values are mapped directly to IDL fields using dot notation (e.g., "agg.price", "expo")
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverrideTemplate {
    /// Unique identifier for the template
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// Description of what this template does
    pub description: String,
    /// Protocol this template is for (e.g., "Pyth", "Switchboard")
    pub protocol: String,
    /// IDL for the account structure - defines all available fields and types
    pub idl: Idl,
    /// How to determine the account address
    pub address: AccountAddress,
    /// Account type name from the IDL (e.g., "PriceAccount")
    /// This specifies which account struct in the IDL to use
    pub account_type: String,
    /// List of editable properties with full metadata
    pub properties: Vec<Property>,
    /// Protocol-specific constants (e.g., AMM configs, well-known tokens)
    #[serde(default)]
    pub constants: HashMap<String, ConstantDefinition>,
    /// Tags for categorization and search
    pub tags: Vec<String>,
    /// Optional context/instructions specifically for LLMs using this template
    /// This helps LLMs understand how to correctly use the template
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_context: Option<String>,
}

impl OverrideTemplate {
    pub fn new(
        id: String,
        name: String,
        description: String,
        protocol: String,
        idl: Idl,
        address: AccountAddress,
        properties: Vec<Property>,
        account_type: String,
    ) -> Self {
        Self {
            id,
            name,
            description,
            protocol,
            idl,
            address,
            account_type,
            properties,
            constants: HashMap::new(),
            tags: Vec::new(),
            llm_context: None,
        }
    }

    /// Get a constant definition by name
    pub fn get_constant(&self, name: &str) -> Option<&ConstantDefinition> {
        self.constants.get(name)
    }

    /// Check if a property is a constant reference
    pub fn is_property_constant_ref(&self, property_name: &str) -> bool {
        self.properties
            .iter()
            .any(|p| p.path == property_name && p.is_constant_ref())
    }

    /// Get the constant definition for a property if it's a constant reference
    pub fn get_property_constant(&self, property_name: &str) -> Option<&ConstantDefinition> {
        self.properties
            .iter()
            .find(|p| p.path == property_name)
            .and_then(|p| p.constant_name())
            .and_then(|const_name| self.constants.get(const_name))
    }

    /// Get a property by its path
    pub fn get_property(&self, path: &str) -> Option<&Property> {
        self.properties.iter().find(|p| p.path == path)
    }

    /// Get the list of property paths (for backward compatibility)
    pub fn property_paths(&self) -> Vec<&str> {
        self.properties.iter().map(|p| p.path.as_str()).collect()
    }
}

/// A concrete instance of an override template with specific values
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS), ts(export))]
pub struct OverrideInstance {
    /// Unique identifier for this instance (UUID v4)
    #[schemars(description = "Unique identifier for this instance (UUID v4 format)")]
    pub id: String,
    /// Reference to the template being used - MUST match a template id from get_override_templates
    #[schemars(
        description = "Template ID from get_override_templates (e.g., 'raydium-clmm-custom', 'kamino-obligation-health')"
    )]
    pub template_id: String,
    /// Values for the template properties as a JSON object (NOT a string)
    #[schemars(
        description = "JSON object mapping property names to values. Keys must be from template.properties. Example: {\"liquidity\": 1000000, \"sqrt_price_x64\": 18446744073709551616}"
    )]
    #[cfg_attr(feature = "ts-bindings", ts(type = "Record<string, unknown>"))]
    pub values: HashMap<String, serde_json::Value>,
    /// Relative slot when this override should be applied (1 = 400ms after registration)
    #[schemars(
        description = "Slot offset from scenario registration (integer, e.g., 1, 2, 3). Each slot is ~400ms."
    )]
    #[cfg_attr(feature = "ts-bindings", ts(type = "number | bigint"))]
    pub scenario_relative_slot: Slot,
    /// Optional human-readable label for this instance
    #[schemars(description = "Human-readable label describing what this override does")]
    #[cfg_attr(feature = "ts-bindings", ts(optional))]
    pub label: Option<String>,
    /// Whether this override is enabled
    #[schemars(description = "Whether this override is active (true/false)")]
    pub enabled: bool,
    /// Whether to fetch fresh account data just before transaction execution
    #[schemars(
        description = "If true, fetches fresh account data from mainnet before applying override"
    )]
    #[serde(default)]
    #[cfg_attr(feature = "ts-bindings", ts(as = "Option<bool>", optional))]
    pub fetch_before_use: bool,
    /// Account address to override - use pubkey for known addresses or pda for derived addresses
    #[schemars(
        description = "Account address: either {\"pubkey\": \"base58_address\"} or {\"pda\": {\"programId\": \"...\", \"seeds\": [...]}}"
    )]
    pub account: AccountAddress,
}

impl OverrideInstance {
    pub fn new(template_id: String, scenario_relative_slot: Slot, account: AccountAddress) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            template_id,
            values: HashMap::new(),
            scenario_relative_slot,
            label: None,
            enabled: true,
            fetch_before_use: false,
            account,
        }
    }

    pub fn with_values(mut self, values: HashMap<String, serde_json::Value>) -> Self {
        self.values = values;
        self
    }

    pub fn with_label(mut self, label: String) -> Self {
        self.label = Some(label);
        self
    }
}

/// A scenario containing a timeline of overrides
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS), ts(export))]
pub struct Scenario {
    /// Unique identifier for the scenario (UUID v4 format)
    #[schemars(
        description = "Unique identifier for the scenario (UUID v4 format, e.g., '550e8400-e29b-41d4-a716-446655440000')"
    )]
    pub id: String,
    /// Human-readable name
    #[schemars(description = "Human-readable name for the scenario")]
    pub name: String,
    /// Description of this scenario
    #[schemars(description = "Description of what this scenario does")]
    pub description: String,
    /// List of override instances in this scenario - MUST be an array, NOT a string
    #[schemars(
        description = "Array of override instances. IMPORTANT: This must be a JSON array [], not a JSON string. Each element is an OverrideInstance object."
    )]
    pub overrides: Vec<OverrideInstance>,
    /// Tags for categorization
    #[schemars(
        description = "Array of string tags for categorization (e.g., ['liquidation', 'arbitrage'])"
    )]
    pub tags: Vec<String>,
}

impl Scenario {
    pub fn new(name: String, description: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            description,
            overrides: Vec::new(),
            tags: Vec::new(),
        }
    }

    pub fn add_override(&mut self, override_instance: OverrideInstance) {
        self.overrides.push(override_instance);
        // Sort by slot for efficient lookup
        self.overrides.sort_by_key(|o| o.scenario_relative_slot);
    }

    pub fn remove_override(&mut self, override_id: &str) {
        self.overrides.retain(|o| o.id != override_id);
    }

    pub fn get_overrides_for_slot(&self, slot: Slot) -> Vec<&OverrideInstance> {
        self.overrides
            .iter()
            .filter(|o| o.enabled && o.scenario_relative_slot == slot)
            .collect()
    }
}

/// Configuration for scenario execution
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioConfig {
    /// Whether scenarios are enabled
    pub enabled: bool,
    /// Currently active scenario
    pub active_scenario: Option<String>,
    /// Whether to auto-save scenario changes
    pub auto_save: bool,
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            active_scenario: None,
            auto_save: true,
        }
    }
}

// ========================================
// YAML Template File Types
// ========================================

/// YAML representation of an override template loaded from file
/// References an external IDL file via idl_file_path
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct YamlOverrideTemplateFile {
    pub id: String,
    pub name: String,
    pub description: String,
    pub protocol: String,
    pub version: String,
    pub account_type: String,
    #[serde(default)]
    pub properties: Vec<YamlProperty>,
    #[serde(default)]
    pub constants: HashMap<String, YamlConstantDefinition>,
    pub idl_file_path: String,
    pub address: YamlAccountAddress,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional context/instructions specifically for LLMs using this template
    #[serde(default)]
    pub llm_context: Option<String>,
}

/// Convert a template's YAML address, attaching the template id to any failure.
fn template_address(
    template_id: &str,
    address: YamlAccountAddress,
) -> Result<AccountAddress, ScenarioError> {
    AccountAddress::try_from(address).map_err(|e| ScenarioError::Template {
        template_id: template_id.to_string(),
        source: Box::new(e),
    })
}

impl YamlOverrideTemplateFile {
    /// Convert file-based template to runtime OverrideTemplate with loaded IDL
    pub fn to_override_template(self, idl: Idl) -> Result<OverrideTemplate, ScenarioError> {
        let address = template_address(&self.id, self.address)?;

        Ok(OverrideTemplate {
            id: self.id,
            name: self.name,
            description: self.description,
            protocol: self.protocol,
            idl,
            address,
            account_type: self.account_type,
            properties: self.properties.into_iter().map(Into::into).collect(),
            constants: self
                .constants
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            tags: self.tags,
            llm_context: self.llm_context,
        })
    }
}

/// YAML representation of a constant option
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct YamlConstantOption {
    /// Unique identifier for this option
    pub id: String,
    /// Human-readable label
    pub label: String,
    /// Description of when to use this option
    #[serde(default)]
    pub description: Option<String>,
    /// The actual value (typically a pubkey)
    pub value: String,
    /// Additional metadata
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

impl From<YamlConstantOption> for ConstantOption {
    fn from(yaml: YamlConstantOption) -> Self {
        ConstantOption {
            id: yaml.id,
            label: yaml.label,
            description: yaml.description,
            value: yaml.value,
            metadata: yaml.metadata,
        }
    }
}

/// Source for constant options - either inline or from verified tokens registry
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum YamlConstantSource {
    /// Inline options defined in the YAML
    Inline { options: Vec<YamlConstantOption> },
    /// Reference to the verified tokens registry
    TokensRef {
        /// Type of reference - currently only "verified_tokens" is supported
        source: String,
        /// Optional filter for which tokens to include (e.g., by tags like "major", "stable")
        #[serde(default)]
        filter_tags: Vec<String>,
        /// Optional limit on number of tokens to include
        #[serde(default)]
        limit: Option<usize>,
    },
}

/// YAML representation of a constant definition
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct YamlConstantDefinition {
    /// Human-readable label for this constant type
    pub label: String,
    /// Description of what this constant represents
    #[serde(default)]
    pub description: Option<String>,
    /// The source of options - either inline or from a reference
    #[serde(flatten)]
    pub source: YamlConstantSource,
}

impl YamlConstantDefinition {
    /// Convert to runtime ConstantDefinition, resolving verified tokens references
    pub fn to_constant_definition(self) -> ConstantDefinition {
        let options = match self.source {
            YamlConstantSource::Inline { options } => options.into_iter().map(Into::into).collect(),
            YamlConstantSource::TokensRef {
                source,
                filter_tags,
                limit,
            } => {
                if source == "verified_tokens" {
                    use crate::verified_tokens::VERIFIED_TOKENS_BY_SYMBOL;

                    let mut tokens: Vec<_> = VERIFIED_TOKENS_BY_SYMBOL
                        .iter()
                        .filter(|(_, _token)| {
                            // If no filter tags specified, include all tokens
                            if filter_tags.is_empty() {
                                return true;
                            }
                            // Check if token has any of the filter tags
                            // The tags are stored in the CSV but not parsed into TokenInfo yet
                            // For now, we'll include all tokens when filter is specified
                            // TODO: Parse tags from CSV into TokenInfo struct
                            true
                        })
                        .map(|(symbol, token)| ConstantOption {
                            id: symbol.to_lowercase(),
                            label: format!("{} ({})", token.symbol, token.name),
                            description: Some(token.name.clone()),
                            value: token.address.clone(),
                            metadata: {
                                let mut meta = HashMap::new();
                                meta.insert(
                                    "symbol".to_string(),
                                    serde_json::Value::String(token.symbol.clone()),
                                );
                                meta.insert(
                                    "decimals".to_string(),
                                    serde_json::Value::Number(token.decimals.into()),
                                );
                                if let Some(ref logo) = token.logo_uri {
                                    meta.insert(
                                        "logo_uri".to_string(),
                                        serde_json::Value::String(logo.clone()),
                                    );
                                }
                                meta
                            },
                        })
                        .collect();

                    // Sort by symbol for consistent ordering
                    tokens.sort_by(|a, b| a.id.cmp(&b.id));

                    // Apply limit if specified
                    if let Some(limit) = limit {
                        tokens.truncate(limit);
                    }

                    tokens
                } else {
                    // Unknown source type - return empty options
                    Vec::new()
                }
            }
        };

        ConstantDefinition {
            label: self.label,
            description: self.description,
            options,
        }
    }
}

// Keep From impl for backward compatibility but use the new method
impl From<YamlConstantDefinition> for ConstantDefinition {
    fn from(yaml: YamlConstantDefinition) -> Self {
        yaml.to_constant_definition()
    }
}

/// YAML representation of a property (supports both simple string and full object format)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum YamlProperty {
    /// Simple string format: just the path (e.g., "liquidity", "fees.swap_fee_numerator")
    Simple(String),
    /// Full object format with all metadata
    Full {
        /// The path to the field in the IDL
        path: String,
        /// The type of property: "field" (default) or "constant_ref"
        #[serde(default, rename = "type")]
        kind: Option<String>,
        /// Human-readable label for the UI (optional)
        #[serde(default)]
        label: Option<String>,
        /// Description of the field (optional)
        #[serde(default)]
        description: Option<String>,
        /// For constant_ref type: the name of the constant definition to use
        #[serde(default)]
        constant: Option<String>,
    },
}

impl From<YamlProperty> for Property {
    fn from(yaml: YamlProperty) -> Self {
        match yaml {
            YamlProperty::Simple(path) => Property::field(path),
            YamlProperty::Full {
                path,
                kind,
                label,
                description,
                constant,
            } => {
                let kind = match kind.as_deref() {
                    Some("constant_ref") => PropertyKind::ConstantRef,
                    _ => PropertyKind::Field,
                };
                Property {
                    path,
                    kind,
                    label,
                    description,
                    constant,
                }
            }
        }
    }
}

/// YAML representation of a typed property (deprecated, for backward compatibility)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum YamlPropertyType {
    /// A regular field from the IDL
    Field { name: String },
    /// A reference to a constant definition
    ConstantRef { name: String, constant: String },
}

impl From<YamlPropertyType> for Property {
    fn from(yaml: YamlPropertyType) -> Self {
        match yaml {
            YamlPropertyType::Field { name } => Property::field(name),
            YamlPropertyType::ConstantRef { name, constant } => {
                Property::constant_ref(name, constant)
            }
        }
    }
}

#[allow(deprecated)]
impl From<YamlPropertyType> for PropertyType {
    fn from(yaml: YamlPropertyType) -> Self {
        match yaml {
            YamlPropertyType::Field { name } => PropertyType::Field { name },
            YamlPropertyType::ConstantRef { name, constant } => {
                PropertyType::ConstantRef { name, constant }
            }
        }
    }
}

/// Collection of override templates sharing the same IDL
/// Used when one YAML file defines multiple templates (e.g., multiple Pyth price feeds)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct YamlOverrideTemplateCollection {
    /// Protocol these templates are for
    pub protocol: String,
    /// Version identifier
    pub version: String,
    /// Account type name from the IDL (optional, can be overridden per template)
    #[serde(default)]
    pub account_type: Option<String>,
    /// Path to shared IDL file
    pub idl_file_path: String,
    /// Common tags for all templates
    #[serde(default)]
    pub tags: Vec<String>,
    /// Protocol-specific constants shared by all templates in this collection
    #[serde(default)]
    pub constants: HashMap<String, YamlConstantDefinition>,
    /// The templates
    pub templates: Vec<YamlOverrideTemplateEntry>,
}

/// Individual template entry in a collection
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct YamlOverrideTemplateEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Account type name from the IDL (overrides collection-level account_type)
    #[serde(default)]
    pub idl_account_name: Option<String>,
    /// Properties with full metadata
    #[serde(default)]
    pub properties: Vec<YamlProperty>,
    pub address: YamlAccountAddress,
    /// Optional context/instructions specifically for LLMs using this template
    #[serde(default)]
    pub llm_context: Option<String>,
}

impl YamlOverrideTemplateCollection {
    /// Convert collection to runtime OverrideTemplates with loaded IDL, validating addresses
    pub fn to_override_templates(self, idl: Idl) -> Result<Vec<OverrideTemplate>, ScenarioError> {
        // Convert constants once for sharing
        let constants: HashMap<String, ConstantDefinition> = self
            .constants
            .into_iter()
            .map(|(k, v)| (k, v.into()))
            .collect();

        let default_account_type = self.account_type.clone().unwrap_or_default();

        let mut templates = Vec::new();
        for entry in self.templates {
            let address = template_address(&entry.id, entry.address)?;

            templates.push(OverrideTemplate {
                id: entry.id,
                name: entry.name,
                description: entry.description,
                protocol: self.protocol.clone(),
                idl: idl.clone(),
                address,
                account_type: entry
                    .idl_account_name
                    .unwrap_or_else(|| default_account_type.clone()),
                properties: entry.properties.into_iter().map(Into::into).collect(),
                constants: constants.clone(),
                tags: self.tags.clone(),
                llm_context: entry.llm_context,
            });
        }

        Ok(templates)
    }
}

/// YAML representation of an override template with embedded IDL
/// Used for RPC methods where file access is not available
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct YamlOverrideTemplate {
    pub id: String,
    pub name: String,
    pub description: String,
    pub protocol: String,
    pub version: String,
    pub account_type: String,
    pub idl: Idl,
    pub address: YamlAccountAddress,
    #[serde(default)]
    pub properties: Vec<YamlProperty>,
    #[serde(default)]
    pub constants: HashMap<String, YamlConstantDefinition>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional context/instructions specifically for LLMs using this template
    #[serde(default)]
    pub llm_context: Option<String>,
}

impl YamlOverrideTemplate {
    /// Convert to runtime OverrideTemplate
    pub fn to_override_template(self) -> Result<OverrideTemplate, ScenarioError> {
        let address = template_address(&self.id, self.address)?;

        Ok(OverrideTemplate {
            id: self.id,
            name: self.name,
            description: self.description,
            protocol: self.protocol,
            idl: self.idl,
            address,
            account_type: self.account_type,
            properties: self.properties.into_iter().map(Into::into).collect(),
            constants: self
                .constants
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            tags: self.tags,
            llm_context: self.llm_context,
        })
    }
}

/// YAML representation of account address
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum YamlAccountAddress {
    Pubkey {
        #[serde(default)]
        value: Option<String>,
    },
    Pda {
        program_id: String,
        seeds: Vec<YamlPdaSeed>,
    },
}

impl std::convert::TryFrom<YamlAccountAddress> for AccountAddress {
    type Error = ScenarioError;

    fn try_from(yaml: YamlAccountAddress) -> Result<Self, Self::Error> {
        match yaml {
            // The empty string is the established wire placeholder for "address
            // supplied per override instance" (the spl-token templates rely on
            // it), so it stays until templates model the absent address
            // explicitly.
            YamlAccountAddress::Pubkey { value: None } => Ok(AccountAddress::Pubkey(String::new())),
            YamlAccountAddress::Pubkey { value: Some(s) } => {
                if Pubkey::from_str(&s).is_err() {
                    return Err(ScenarioError::InvalidAddress(SeedError::InvalidPubkey(s)));
                }
                Ok(AccountAddress::Pubkey(s))
            }
            YamlAccountAddress::Pda { program_id, seeds } => {
                Pubkey::from_str(&program_id)
                    .map_err(|_| ScenarioError::InvalidProgramId(program_id.clone()))?;

                let converted_seeds: Vec<PdaSeed> = seeds.into_iter().map(|s| s.into()).collect();

                for (i, seed) in converted_seeds.iter().enumerate() {
                    seed.validate_literals().map_err(|e| ScenarioError::Seed {
                        program_id: program_id.clone(),
                        index: i,
                        source: e,
                    })?;
                }

                Ok(AccountAddress::Pda {
                    program_id,
                    seeds: converted_seeds,
                })
            }
        }
    }
}

/// YAML representation of PDA seeds
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum YamlPdaSeed {
    String {
        value: String,
    },
    Bytes {
        value: Vec<u8>,
    },
    Pubkey {
        value: String,
    },
    PropertyRef {
        value: String,
    },
    /// A u16 value converted to big-endian bytes
    U16Be {
        value: u16,
    },
    /// Reference to a property that should be converted to u16 big-endian bytes
    U16BeRef {
        value: String,
    },
    /// A u16 value converted to little-endian bytes (useful for Pyth shard IDs)
    U16Le {
        value: u16,
    },
    /// Reference to a property that's a 32-byte hex string (e.g., Pyth feed ID)
    Bytes32Ref {
        value: String,
    },
    /// A nested PDA derivation - derives a PDA from inner seeds and uses it as the seed
    DerivedPda {
        program_id: String,
        seeds: Vec<YamlPdaSeed>,
    },
}

impl From<YamlPdaSeed> for PdaSeed {
    fn from(yaml: YamlPdaSeed) -> Self {
        match yaml {
            YamlPdaSeed::String { value } => PdaSeed::String(value),
            YamlPdaSeed::Bytes { value } => PdaSeed::Bytes(value),
            YamlPdaSeed::Pubkey { value } => PdaSeed::Pubkey(value),
            YamlPdaSeed::PropertyRef { value } => PdaSeed::PropertyRef(value),
            YamlPdaSeed::U16Be { value } => PdaSeed::U16Be(value),
            YamlPdaSeed::U16BeRef { value } => PdaSeed::U16BeRef(value),
            YamlPdaSeed::U16Le { value } => PdaSeed::U16Le(value),
            YamlPdaSeed::Bytes32Ref { value } => PdaSeed::Bytes32Ref(value),
            YamlPdaSeed::DerivedPda { program_id, seeds } => PdaSeed::DerivedPda {
                program_id,
                seeds: seeds.into_iter().map(|s| s.into()).collect(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::str::FromStr;

    use serde_json::json;
    use solana_pubkey::Pubkey;

    use super::{
        AccountAddress, Idl, OverrideError, PdaSeed, ScenarioError, SeedError, YamlAccountAddress,
        YamlOverrideTemplateCollection, YamlOverrideTemplateEntry, YamlPdaSeed,
    };

    /// Pin the rendered message of every error variant. Run with
    /// `--nocapture` to view them.
    #[test]
    fn error_messages_render_with_full_context() {
        let program = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
        let account = "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM";
        let cases: Vec<(String, String)> = vec![
            (
                SeedError::InvalidPubkey("garbage".to_string()).to_string(),
                "'garbage' is not a valid pubkey".to_string(),
            ),
            (
                SeedError::NoValues("owner".to_string()).to_string(),
                "seed references property 'owner' but no values were provided".to_string(),
            ),
            (
                SeedError::UnknownProperty("owner".to_string()).to_string(),
                "property 'owner' not found in values".to_string(),
            ),
            (
                SeedError::WrongType {
                    name: "owner".to_string(),
                    expected: "string",
                    found: "bool",
                }
                .to_string(),
                "property 'owner' is bool, expected string".to_string(),
            ),
            (
                SeedError::U16OutOfRange {
                    name: "index".to_string(),
                    value: 70_000,
                }
                .to_string(),
                "property 'index' value 70000 does not fit in u16".to_string(),
            ),
            (
                SeedError::InvalidBytes32("zz".to_string()).to_string(),
                "'zz' is not a 32-byte hex string".to_string(),
            ),
            (
                SeedError::DerivedSeed {
                    program_id: program.to_string(),
                    index: 1,
                    source: Box::new(SeedError::InvalidPubkey("garbage".to_string())),
                }
                .to_string(),
                format!("seed 1 of derived PDA for program {program}: 'garbage' is not a valid pubkey"),
            ),
            (
                ScenarioError::InvalidAddress(SeedError::InvalidPubkey("garbage".to_string()))
                    .to_string(),
                "invalid account address: 'garbage' is not a valid pubkey".to_string(),
            ),
            (
                ScenarioError::InvalidProgramId("garbage".to_string()).to_string(),
                "invalid program id 'garbage'".to_string(),
            ),
            (
                ScenarioError::Seed {
                    program_id: program.to_string(),
                    index: 2,
                    source: SeedError::UnknownProperty("owner".to_string()),
                }
                .to_string(),
                format!("PDA for program {program}, seed 2: property 'owner' not found in values"),
            ),
            (
                ScenarioError::Template {
                    template_id: "spl-token".to_string(),
                    source: Box::new(ScenarioError::InvalidProgramId("garbage".to_string())),
                }
                .to_string(),
                "template 'spl-token': invalid program id 'garbage'".to_string(),
            ),
            (
                OverrideError::Resolve(ScenarioError::InvalidProgramId("garbage".to_string()))
                    .to_string(),
                "invalid program id 'garbage'".to_string(),
            ),
            (
                OverrideError::AccountNotFound {
                    account: account.to_string(),
                }
                .to_string(),
                format!(
                    "account {account} does not exist locally; overrides patch existing accounts (enable fetchBeforeUse or create the account first)"
                ),
            ),
            (
                OverrideError::AccountTooSmall {
                    account: account.to_string(),
                    len: 4,
                }
                .to_string(),
                format!("account {account} has 4 byte(s), too small for an 8-byte discriminator"),
            ),
            (
                OverrideError::NoIdlForOwner {
                    program_id: program.to_string(),
                    account: account.to_string(),
                }
                .to_string(),
                format!("no IDL registered for program {program} (owner of account {account})"),
            ),
            (
                OverrideError::Forge {
                    account: account.to_string(),
                    message: "borsh decode failed".to_string(),
                }
                .to_string(),
                format!("failed to forge account data for {account}: borsh decode failed"),
            ),
        ];
        for (rendered, expected) in cases {
            println!("{rendered}");
            assert_eq!(rendered, expected);
        }
    }

    #[test]
    fn u16_be_ref_rejects_out_of_range_values() {
        let seed = PdaSeed::U16BeRef("index".to_string());
        let values = HashMap::from([("index".to_string(), json!(70_000))]);

        assert_eq!(
            seed.to_bytes(Some(&values)),
            Err(SeedError::U16OutOfRange {
                name: "index".to_string(),
                value: 70_000,
            })
        );
    }

    #[test]
    fn u16_be_ref_encodes_in_range_values() {
        let seed = PdaSeed::U16BeRef("index".to_string());
        let values = HashMap::from([("index".to_string(), json!(513))]);

        assert_eq!(seed.to_bytes(Some(&values)), Ok(vec![2, 1]));
    }

    #[test]
    fn pubkey_seed_with_garbage_string() {
        let seed = PdaSeed::Pubkey("not-a-pubkey".to_string());
        assert_eq!(
            seed.to_bytes(None),
            Err(SeedError::InvalidPubkey("not-a-pubkey".to_string()))
        );
    }

    #[test]
    fn property_ref_with_no_values() {
        let seed = PdaSeed::PropertyRef("some_prop".to_string());
        assert_eq!(
            seed.to_bytes(None),
            Err(SeedError::NoValues("some_prop".to_string()))
        );
    }

    #[test]
    fn property_ref_with_missing_key() {
        let seed = PdaSeed::PropertyRef("missing_key".to_string());
        let values = HashMap::from([("other_key".to_string(), json!("value"))]);
        assert_eq!(
            seed.to_bytes(Some(&values)),
            Err(SeedError::UnknownProperty("missing_key".to_string()))
        );
    }

    #[test]
    fn property_ref_with_wrong_type() {
        let seed = PdaSeed::PropertyRef("my_prop".to_string());
        let values = HashMap::from([("my_prop".to_string(), json!(true))]);
        assert_eq!(
            seed.to_bytes(Some(&values)),
            Err(SeedError::WrongType {
                name: "my_prop".to_string(),
                expected: "string or u64",
                found: "bool",
            })
        );
    }

    #[test]
    fn bytes32_ref_with_invalid_hex() {
        let seed = PdaSeed::Bytes32Ref("feed_id".to_string());
        let values = HashMap::from([("feed_id".to_string(), json!("0xzz"))]);
        assert_eq!(
            seed.to_bytes(Some(&values)),
            Err(SeedError::InvalidBytes32("zz".to_string()))
        );
    }

    #[test]
    fn derived_pda_with_bad_inner_seed() {
        let seed = PdaSeed::DerivedPda {
            program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
            seeds: vec![
                PdaSeed::String("valid_string".to_string()),
                PdaSeed::Pubkey("not-a-pubkey".to_string()),
            ],
        };

        match seed.to_bytes(None) {
            Ok(_) => panic!("expected error"),
            Err(SeedError::DerivedSeed {
                program_id,
                index,
                source,
            }) => {
                assert_eq!(program_id, "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
                assert_eq!(index, 1);
                assert_eq!(
                    *source,
                    SeedError::InvalidPubkey("not-a-pubkey".to_string())
                );
            }
            Err(e) => panic!("wrong error: {:?}", e),
        }
    }

    #[test]
    fn account_address_pubkey_with_garbage_resolve_simple() {
        let addr = AccountAddress::Pubkey("garbage".to_string());
        assert_eq!(
            addr.resolve_simple(),
            Err(ScenarioError::InvalidAddress(SeedError::InvalidPubkey(
                "garbage".to_string()
            )))
        );
    }

    #[test]
    fn account_address_pda_with_invalid_program_id() {
        let addr = AccountAddress::Pda {
            program_id: "invalid-program".to_string(),
            seeds: vec![PdaSeed::String("test".to_string())],
        };

        match addr.resolve_simple() {
            Ok(_) => panic!("expected error"),
            Err(ScenarioError::InvalidProgramId(id)) => {
                assert_eq!(id, "invalid-program");
            }
            Err(e) => panic!("wrong error: {:?}", e),
        }
    }

    #[test]
    fn account_address_pda_with_property_ref_no_values() {
        let addr = AccountAddress::Pda {
            program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
            seeds: vec![PdaSeed::PropertyRef("owner".to_string())],
        };

        match addr.resolve_simple() {
            Ok(_) => panic!("expected error"),
            Err(ScenarioError::Seed {
                program_id,
                index,
                source,
            }) => {
                assert_eq!(program_id, "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
                assert_eq!(index, 0);
                assert_eq!(source, SeedError::NoValues("owner".to_string()));
            }
            Err(e) => panic!("wrong error: {:?}", e),
        }
    }

    #[test]
    fn account_address_pda_success() {
        let seed_str = "test_seed";
        let addr = AccountAddress::Pda {
            program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
            seeds: vec![PdaSeed::String(seed_str.to_string())],
        };

        let resolved = addr.resolve_simple();
        assert!(resolved.is_ok());

        // Verify it matches the expected PDA computed via find_program_address
        let expected_program =
            Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
        let (expected_pda, _) =
            Pubkey::find_program_address(&[seed_str.as_bytes()], &expected_program);

        assert_eq!(resolved.ok(), Some(expected_pda));
    }

    #[test]
    fn u16_be_ref_encodes_decimal_strings() {
        let seed = PdaSeed::U16BeRef("index".to_string());
        let values = HashMap::from([("index".to_string(), json!("513"))]);

        assert_eq!(seed.to_bytes(Some(&values)), Ok(vec![2, 1]));
    }

    #[test]
    fn u16_be_ref_rejects_strings_that_are_not_a_u16() {
        let seed = PdaSeed::U16BeRef("index".to_string());

        for value in [json!("65536"), json!("abc"), json!("-1"), json!("")] {
            let values = HashMap::from([("index".to_string(), value.clone())]);
            assert!(seed.to_bytes(Some(&values)).is_err(), "value {value}");
        }
    }

    #[test]
    fn u16_be_ref_rejects_values_that_are_not_an_integer() {
        let seed = PdaSeed::U16BeRef("index".to_string());

        for value in [json!(1.5), json!(true), json!(null), json!([1]), json!({})] {
            let values = HashMap::from([("index".to_string(), value.clone())]);
            assert!(seed.to_bytes(Some(&values)).is_err(), "value {value}");
        }
    }

    #[test]
    fn yaml_account_address_pubkey_valid_base58_converts() {
        let yaml_addr = YamlAccountAddress::Pubkey {
            value: Some("So11111111111111111111111111111111111111112".to_string()),
        };

        let result = AccountAddress::try_from(yaml_addr);
        assert!(result.is_ok());

        if let Ok(AccountAddress::Pubkey(s)) = result {
            assert_eq!(s, "So11111111111111111111111111111111111111112");
        } else {
            panic!("Expected Pubkey variant");
        }
    }

    #[test]
    fn yaml_account_address_pubkey_invalid_string_errors() {
        let yaml_addr = YamlAccountAddress::Pubkey {
            value: Some("not-a-pubkey".to_string()),
        };

        let result = AccountAddress::try_from(yaml_addr);
        assert!(result.is_err());

        if let Err(ScenarioError::InvalidAddress(SeedError::InvalidPubkey(s))) = result {
            assert_eq!(s, "not-a-pubkey");
        } else {
            panic!("Expected InvalidAddress error with InvalidPubkey");
        }
    }

    #[test]
    fn yaml_account_address_pubkey_none_stays_empty_string() {
        let yaml_addr = YamlAccountAddress::Pubkey { value: None };

        let result = AccountAddress::try_from(yaml_addr);
        assert!(result.is_ok());

        if let Ok(AccountAddress::Pubkey(s)) = result {
            assert_eq!(s, "");
        } else {
            panic!("Expected Pubkey variant with empty string");
        }
    }

    #[test]
    fn yaml_account_address_pda_bad_program_id_errors() {
        let yaml_addr = YamlAccountAddress::Pda {
            program_id: "not-a-pubkey".to_string(),
            seeds: vec![],
        };

        let result = AccountAddress::try_from(yaml_addr);
        assert!(result.is_err());

        if let Err(ScenarioError::InvalidProgramId(id)) = result {
            assert_eq!(id, "not-a-pubkey");
        } else {
            panic!("Expected InvalidProgramId error");
        }
    }

    #[test]
    fn yaml_account_address_pda_bad_literal_pubkey_seed_errors() {
        let yaml_addr = YamlAccountAddress::Pda {
            program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
            seeds: vec![
                YamlPdaSeed::String {
                    value: "seed0".to_string(),
                },
                YamlPdaSeed::Pubkey {
                    value: "oops".to_string(),
                },
            ],
        };

        let result = AccountAddress::try_from(yaml_addr);
        assert!(result.is_err());

        if let Err(ScenarioError::Seed {
            program_id,
            index,
            source,
        }) = result
        {
            assert_eq!(program_id, "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
            assert_eq!(index, 1);
            assert_eq!(source, SeedError::InvalidPubkey("oops".to_string()));
        } else {
            panic!("Expected Seed error at index 1");
        }
    }

    #[test]
    fn yaml_account_address_pda_property_ref_seed_not_validated() {
        use std::convert::TryFrom;

        let yaml_addr = YamlAccountAddress::Pda {
            program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
            seeds: vec![YamlPdaSeed::PropertyRef {
                value: "some_property".to_string(),
            }],
        };

        let result = AccountAddress::try_from(yaml_addr);
        assert!(
            result.is_ok(),
            "PropertyRef seeds should not be validated at load time"
        );
    }

    #[test]
    fn yaml_override_template_collection_with_bad_address_wraps_error() {
        let entry = YamlOverrideTemplateEntry {
            id: "test-template".to_string(),
            name: "Test".to_string(),
            description: "Test template".to_string(),
            idl_account_name: None,
            properties: vec![],
            address: YamlAccountAddress::Pubkey {
                value: Some("invalid-pubkey".to_string()),
            },
            llm_context: None,
        };

        let collection = YamlOverrideTemplateCollection {
            protocol: "Test".to_string(),
            version: "1.0".to_string(),
            account_type: None,
            idl_file_path: "test.json".to_string(),
            tags: vec![],
            constants: HashMap::new(),
            templates: vec![entry],
        };

        let idl_json = r#"{
            "address": "11111111111111111111111111111111",
            "instructions": [],
            "accounts": [],
            "types": [],
            "errors": [],
            "metadata": {"name": "test", "version": "0.1.0", "spec": "0.1.0"}
        }"#;

        let idl: Idl = serde_json::from_str(idl_json).expect("Valid minimal IDL");

        let result = collection.to_override_templates(idl);
        assert!(result.is_err());

        if let Err(ScenarioError::Template {
            template_id,
            source: _,
        }) = result
        {
            assert_eq!(template_id, "test-template");
        } else {
            panic!("Expected Template error wrapper");
        }
    }
}
