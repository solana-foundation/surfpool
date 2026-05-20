use std::{
    collections::BTreeMap,
    env,
    fs::File,
    panic::{AssertUnwindSafe, catch_unwind, set_hook, take_hook},
    path::PathBuf,
    process,
    str::FromStr,
};

use chrono::Local;
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand};
use clap_complete::{Generator, Shell};
use fern::colors::{Color, ColoredLevelConfig};
#[cfg(not(target_os = "windows"))]
use fork::{Fork, daemon};
use hiro_system_kit::{self, Logger};
use log::info;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::{EncodableKey, Signer};
use surfpool_core::surfnet::svm::SurfnetSvmConfig;
use surfpool_mcp::McpOptions;
use surfpool_types::{
    AccountSnapshot, BlockProductionMode, CHANGE_TO_DEFAULT_STUDIO_PORT_ONCE_SUPERVISOR_MERGED,
    DEFAULT_DEVNET_RPC_URL, DEFAULT_GOSSIP_PORT, DEFAULT_MAINNET_RPC_URL, DEFAULT_NETWORK_HOST,
    DEFAULT_RPC_PORT, DEFAULT_SLOT_TIME_MS, DEFAULT_TESTNET_RPC_URL, DEFAULT_TPU_PORT,
    DEFAULT_TPU_QUIC_PORT, DEFAULT_WS_PORT, RpcConfig, SimnetConfig, SimnetEvent, StudioConfig,
    SubgraphConfig, SurfpoolConfig, SvmFeatureConfig, parse_feature_pubkey,
};
use txtx_core::manifest::WorkspaceManifest;
use txtx_gql::kit::{helpers::fs::FileLocation, types::frontend::LogLevel};

use crate::{cli::update::handle_update_command, runbook::handle_execute_runbook_command};
mod simnet;
mod update;

#[derive(Clone)]
pub struct Context {
    pub logger: Option<Logger>,
    #[allow(dead_code)]
    pub tracer: bool,
}

pub const DEFAULT_RUNBOOK: &str = "deployment";
pub const DEFAULT_AIRDROP_AMOUNT: &str = "10000000000000";

lazy_static::lazy_static! {
    pub static ref DEFAULT_SOLANA_KEYPAIR_PATH: String = {
        PathBuf::from("~").join(".config").join("solana")
            .join("id.json")
            .display()
            .to_string()
    };

    pub static ref DEFAULT_LOG_DIR: String = {
        PathBuf::from(".surfpool").join("logs")
            .display()
            .to_string()
    };
}

/// Gets the user's home directory, accounting for the Snap confinement environment.
/// We set out snap build to set this environment variable to the real home directory,
/// because by default, snaps run in a confined environment where the home directory is not
/// the user's actual home directory.
pub fn get_home_dir() -> String {
    if let Ok(real_home) = env::var("SNAP_REAL_HOME") {
        let path_buf = PathBuf::from(real_home);
        path_buf.display().to_string()
    } else {
        dirs::home_dir().unwrap().display().to_string()
    }
}

/// Resolves a path, expanding the `~` to the user's home directory if present.
pub fn resolve_path(path: &str) -> PathBuf {
    let path = if let Some(stripped) = path.strip_prefix("~") {
        let joined = format!("{}{}", get_home_dir(), stripped);
        joined
    } else {
        path.to_string()
    };
    PathBuf::from(path)
}

impl Context {
    #[allow(dead_code)]
    pub fn empty() -> Context {
        Context {
            logger: None,
            tracer: false,
        }
    }

    #[allow(dead_code)]
    pub fn try_log<F>(&self, closure: F)
    where
        F: FnOnce(&Logger),
    {
        if let Some(ref logger) = self.logger {
            closure(logger)
        }
    }
}

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None, name = "surfpool", bin_name = "surfpool")]
struct Opts {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
enum Command {
    /// Start a local Surfnet
    #[clap(name = "start", bin_name = "start", aliases = &["simnet"])]
    Simnet(StartSimnet),
    /// Generate shell completion scripts
    #[clap(name = "completions", bin_name = "completions", aliases = &["completion"])]
    Completions(Completions),
    /// Execute a runbook
    #[clap(name = "run", bin_name = "run")]
    Run(ExecuteRunbook),
    /// List runbooks in the current workspace
    #[clap(name = "ls", bin_name = "ls")]
    List(ListRunbooks),
    /// Start the Surfpool MCP server
    #[clap(name = "mcp", bin_name = "mcp")]
    Mcp,
    /// Update Surfpool to the latest version
    #[clap(name = "update", bin_name = "update")]
    Update(UpdateCommand),
}

const START_AFTER_LONG_HELP: &str = r#"Examples:
  Start a local Surfnet:
    surfpool start

  Fork devnet instead of mainnet:
    surfpool start --network devnet

  Use a custom datasource RPC URL:
    surfpool start --rpc-url https://my-custom-rpc-url.com

  Print logs instead of launching the TUI:
    surfpool start --no-tui

  Start without a remote RPC datasource:
    surfpool start --offline

  Airdrop SOL to multiple accounts:
    surfpool start --airdrop <PUBKEY> --airdrop <PUBKEY>

  Load account state from snapshots:
    surfpool start --snapshot ./snapshot1.json --snapshot ./snapshot2.json

  Redeploy programs when target/deploy changes:
    surfpool start --watch

  Persist Surfnet state in an on-disk SQLite database:
    surfpool start --db ./surfnet.sqlite --surfnet-id local-dev
"#;

#[derive(Parser, PartialEq, Clone, Debug)]
#[command(after_long_help = START_AFTER_LONG_HELP)]
pub struct StartSimnet {
    #[command(flatten)]
    pub network: StartNetworkOptions,
    #[command(flatten)]
    pub project: StartProjectOptions,
    #[command(flatten)]
    pub accounts: StartAccountOptions,
    #[command(flatten)]
    pub runtime: StartRuntimeOptions,
    #[command(flatten)]
    pub svm: StartSvmOptions,
    #[command(flatten)]
    pub observability: StartObservabilityOptions,
    /// Deprecated; accepted for backward compatibility.
    #[arg(
        long = "subgraph-db",
        short = 'd',
        default_value = ":memory:",
        hide = true
    )]
    pub subgraph_db: Option<String>,
}

#[derive(Args, PartialEq, Clone, Debug)]
#[command(next_help_heading = "Network & Ports")]
pub struct StartNetworkOptions {
    /// Bind the JSON-RPC server to this port.
    #[arg(
        long = "port",
        short = 'p',
        default_value_t = DEFAULT_RPC_PORT,
        value_name = "PORT",
        long_help = "Bind the JSON-RPC server to this port.\n\nExample: surfpool start --port 8080"
    )]
    pub simnet_port: u16,
    /// Bind the WebSocket server to this port.
    #[arg(
        long = "ws-port",
        short = 'w',
        default_value_t = DEFAULT_WS_PORT,
        value_name = "PORT"
    )]
    pub ws_port: u16,
    /// Bind RPC, WebSocket, and Studio services to this host.
    #[arg(
        long = "host",
        short = 'o',
        default_value = DEFAULT_NETWORK_HOST,
        value_name = "HOST",
        long_help = "Bind RPC, WebSocket, and Studio services to this host.\n\nExample: surfpool start --host 0.0.0.0"
    )]
    pub network_host: String,
    /// Fork from this datasource RPC URL.
    #[arg(
        long = "rpc-url",
        short = 'u',
        conflicts_with = "network",
        env = "SURFPOOL_DATASOURCE_RPC_URL",
        value_name = "RPC_URL",
        long_help = "Fork from this datasource RPC URL. Cannot be used with --network.\n\nThis can also be set with SURFPOOL_DATASOURCE_RPC_URL.\n\nExample: surfpool start --rpc-url https://api.mainnet-beta.solana.com"
    )]
    pub rpc_url: Option<String>,
    /// Fork from a predefined Solana network.
    #[arg(
        long = "network",
        short = 'n',
        value_enum,
        conflicts_with = "rpc_url",
        value_name = "NETWORK",
        long_help = "Fork from a predefined Solana network. Cannot be used with --rpc-url.\n\nExample: surfpool start --network devnet"
    )]
    pub network: Option<NetworkType>,
    /// Run without a remote RPC datasource.
    #[clap(
        long = "offline",
        action=ArgAction::SetTrue,
        default_value = "false",
        long_help = "Run without a remote RPC datasource. Use this to simulate an offline environment.\n\nExample: surfpool start --offline"
    )]
    pub offline: bool,
}

#[derive(Args, PartialEq, Clone, Debug)]
#[command(next_help_heading = "Project & Deployment")]
pub struct StartProjectOptions {
    /// Path to the runbook manifest.
    #[arg(long = "manifest-file-path", short = 'm', default_value = "./txtx.yml")]
    pub manifest_path: String,
    /// Disable automatic program deployments.
    #[clap(long = "no-deploy", default_value = "false")]
    pub no_deploy: bool,
    /// Runbook ID to execute after startup.
    #[arg(
        long = "runbook",
        short = 'r',
        default_value = DEFAULT_RUNBOOK,
        value_name = "RUNBOOK",
        long_help = "Runbook ID to execute after startup. Can be specified multiple times.\n\nExample: surfpool start --runbook deployment --runbook seed"
    )]
    pub runbooks: Vec<String>,
    /// Provide an input file to runbook execution.
    #[arg(
        long = "runbook-input",
        short = 'i',
        value_name = "INPUT_PATH",
        long_help = "Provide an input file to runbook execution. Can be specified multiple times.\n\nExample: surfpool start --runbook-input myInputs.json"
    )]
    pub runbook_input: Vec<String>,
    /// Skip runbook generation prompts.
    #[clap(long = "yes", short = 'y', action=ArgAction::SetTrue,  default_value = "false")]
    pub skip_runbook_generation_prompts: bool,
    /// Watch programs in your artifacts folder (default: `target/deploy`), and automatically re-execute the deployment runbook when the `.so` files change. (eg. surfpool start --watch)
    #[clap(long = "watch", action=ArgAction::SetTrue, default_value = "false")]
    pub watch: bool,
    /// Directory containing .so program artifacts.
    #[arg(
        long = "artifacts-path",
        value_name = "ARTIFACTS_PATH",
        long_help = "Directory containing .so program artifacts. Defaults to target/deploy.\n\nExample: surfpool start --artifacts-path ./target/deploy/debug"
    )]
    pub artifacts_path: Option<String>,
    /// Use defaults suited for legacy Anchor test suites.
    #[clap(long = "legacy-anchor-compatibility", action=ArgAction::SetTrue, default_value = "false")]
    pub anchor_compat: bool,
    /// Anchor Test.toml file to inspect.
    #[arg(
        long = "anchor-test-config-path",
        value_name = "TEST_TOML",
        long_help = "Anchor Test.toml file to inspect. Can be specified multiple times.\n\nExample: surfpool start --anchor-test-config-path ./path/to/Test.toml"
    )]
    pub anchor_test_config_paths: Vec<String>,
}

#[derive(Args, PartialEq, Clone, Debug)]
#[command(next_help_heading = "Accounts & State")]
pub struct StartAccountOptions {
    /// Pubkey to airdrop SOL to at startup.
    #[arg(
        long = "airdrop",
        short = 'a',
        value_name = "PUBKEY",
        long_help = "Pubkey to airdrop SOL to at startup. Can be specified multiple times.\n\nExample: surfpool start --airdrop <PUBKEY> --airdrop <PUBKEY>"
    )]
    pub airdrop_addresses: Vec<String>,
    /// Keypair path whose pubkey should receive an airdrop.
    #[arg(
        long = "airdrop-keypair-path",
        short = 'k',
        default_value = DEFAULT_SOLANA_KEYPAIR_PATH.as_str(),
        value_name = "KEYPAIR_PATH",
        long_help = "Keypair path whose pubkey should receive an airdrop. Can be specified multiple times.\n\nExample: surfpool start --airdrop-keypair-path ~/.config/solana/id.json"
    )]
    pub airdrop_keypair_path: Vec<String>,
    /// Quantity of lamports to airdrop to each address on startup. Set to 0 to skip startup airdrops entirely.
    /// Values greater than 0 but below the rent-exempt minimum are rejected and result in airdrops being skipped.
    #[arg(
        long = "airdrop-amount",
        short = 'q',
        default_value = DEFAULT_AIRDROP_AMOUNT,
        value_name = "LAMPORTS"
    )]
    pub airdrop_token_amount: u64,
    /// JSON account snapshot to preload.
    #[arg(
        long = "snapshot",
        value_name = "SNAPSHOT_PATH",
        long_help = "JSON account snapshot to preload. Can be specified multiple times.\n\nThe snapshot format matches the surfnet_exportSnapshot RPC output. Account values can be null to fetch the account from the remote RPC. Later files override earlier files for duplicate keys.\n\nExample: surfpool start --snapshot ./snapshot1.json --snapshot ./snapshot2.json"
    )]
    pub snapshot: Vec<String>,
    /// Database connection URL for persistent Surfnet state.
    #[arg(
        long = "db",
        value_name = "DB_URL",
        long_help = "Database connection URL for persistent Surfnet state.\n\nUse \":memory:\" for an in-memory SQLite database. Use a filename ending in .sqlite for on-disk SQLite. PostgreSQL URLs require building from source with the postgres feature enabled."
    )]
    pub db: Option<String>,
    /// Storage namespace for this Surfnet instance.
    #[arg(
        long = "surfnet-id",
        default_value = "default",
        value_name = "SURFNET_ID"
    )]
    pub surfnet_id: String,
}

#[derive(Args, PartialEq, Clone, Debug)]
#[command(next_help_heading = "Runtime & UI")]
pub struct StartRuntimeOptions {
    /// Print logs instead of launching the terminal UI.
    #[clap(
        long = "no-tui",
        default_value = "false",
        long_help = "Print logs instead of launching the terminal UI dashboard.\n\nExample: surfpool start --no-tui"
    )]
    pub no_tui: bool,
    /// Disable Surfpool Studio.
    #[clap(long = "no-studio", default_value = "false")]
    pub no_studio: bool,
    /// Bind Studio to this port.
    #[arg(
        long = "studio-port",
        short = 's',
        default_value_t = CHANGE_TO_DEFAULT_STUDIO_PORT_ONCE_SUPERVISOR_MERGED,
        value_name = "PORT"
    )]
    pub studio_port: u16,
    /// Run Surfpool as a background process.
    #[clap(long = "daemon", action=ArgAction::SetTrue, default_value = "false")]
    pub daemon: bool,
    /// Use settings suitable for CI.
    #[clap(
        long = "ci",
        action=ArgAction::SetTrue,
        default_value = "false",
        long_help = "Use settings suitable for CI. This disables the TUI, Studio, instruction profiling, and log output."
    )]
    pub ci: bool,
}

#[derive(Args, PartialEq, Clone, Debug)]
#[command(next_help_heading = "SVM Behavior")]
pub struct StartSvmOptions {
    /// Slot time in milliseconds.
    #[arg(
        long = "slot-time",
        short = 't',
        default_value_t = DEFAULT_SLOT_TIME_MS,
        value_name = "MILLISECONDS"
    )]
    pub slot_time: u64,
    /// Block production mode.
    #[arg(
        long = "block-production-mode",
        short = 'b',
        default_value_t = BlockProductionMode::Clock,
        value_name = "MODE"
    )]
    pub block_production_mode: BlockProductionMode,
    /// Enable an SVM feature by pubkey.
    #[arg(
        long = "feature",
        short = 'f',
        value_parser = parse_feature_pubkey,
        value_name = "FEATURE_PUBKEY",
        long_help = "Enable an SVM feature by pubkey. Can be specified multiple times.\n\nProviding feature names is deprecated. Previously supported names still work, but support will be removed in a future release."
    )]
    pub features: Vec<Pubkey>,
    /// Disable an SVM feature by pubkey.
    #[arg(
        long = "disable-feature",
        value_parser = parse_feature_pubkey,
        value_name = "FEATURE_PUBKEY",
        long_help = "Disable an SVM feature by pubkey. Can be specified multiple times.\n\nProviding feature names is deprecated. Previously supported names still work, but support will be removed in a future release."
    )]
    pub disable_features: Vec<Pubkey>,
    /// Enable all SVM features from agave-feature-set.
    #[clap(long = "features-all", action=ArgAction::SetTrue, default_value = "false")]
    pub all_features: bool,
    /// Skip transaction signature verification.
    #[clap(long = "skip-signature-verification", action=ArgAction::SetTrue, default_value = "false")]
    pub skip_signature_verification: bool,
    /// Skip transaction blockhash validation.
    #[clap(long = "skip-blockhash-check", action=ArgAction::SetTrue, default_value = "false")]
    pub skip_blockhash_check: bool,
}

#[derive(Args, PartialEq, Clone, Debug)]
#[command(next_help_heading = "Observability & Performance")]
pub struct StartObservabilityOptions {
    /// Geyser plugin config file to load.
    #[arg(
        long = "geyser-plugin-config",
        short = 'g',
        value_name = "PLUGIN_CONFIG_PATH",
        long_help = "Geyser plugin config file to load. Can be specified multiple times.\n\nExample: surfpool start --geyser-plugin-config plugin1.json --geyser-plugin-config plugin2.json"
    )]
    pub plugin_config_path: Vec<String>,
    /// Disable instruction profiling.
    #[clap(long = "disable-instruction-profiling", action=ArgAction::SetTrue)]
    pub disable_instruction_profiling: bool,
    /// Transaction profiles to retain in memory.
    #[arg(
        long = "max-profiles",
        short = 'c',
        default_value = "200",
        value_name = "COUNT",
        long_help = "Transaction profiles to retain in memory. Higher values increase memory usage.\n\nExample: surfpool start --max-profiles 2000"
    )]
    pub max_profiles: usize,
    /// Maximum bytes stored for each transaction log.
    #[arg(
        long = "log-bytes-limit",
        default_value = "10000",
        value_name = "BYTES",
        long_help = "Maximum bytes stored for each transaction log. Set to 0 for unlimited logs.\n\nExample: surfpool start --log-bytes-limit 64000"
    )]
    pub log_bytes_limit: usize,
    /// Simnet log level.
    #[arg(
        long = "log-level",
        short = 'l',
        default_value = "info",
        value_name = "LEVEL",
        long_help = "Simnet log level. Valid values are trace, debug, info, warn, error, or none.\n\nExample: surfpool start --log-level debug"
    )]
    pub log_level: String,
    /// Directory for simnet logs.
    #[arg(long = "log-path", default_value = DEFAULT_LOG_DIR.as_str(), value_name = "LOG_DIR")]
    pub log_dir: String,
    /// Enable Prometheus metrics endpoint
    #[cfg(feature = "prometheus")]
    #[arg(long = "metrics-enabled", env = "SURFPOOL_METRICS_ENABLED")]
    pub metrics_enabled: bool,
    #[cfg(feature = "prometheus")]
    /// Prometheus metrics endpoint address
    #[arg(
        long = "metrics-addr",
        default_value = "127.0.0.1:9000",
        env = "SURFPOOL_METRICS_ADDR"
    )]
    pub metrics_addr: String,
}

#[derive(clap::ValueEnum, PartialEq, Clone, Debug)]
pub enum NetworkType {
    /// Solana Mainnet-Beta (https://api.mainnet-beta.solana.com)
    Mainnet,
    /// Solana Devnet (https://api.devnet.solana.com)
    Devnet,
    /// Solana Testnet (https://api.testnet.solana.com)
    Testnet,
}

impl StartSimnet {
    pub fn get_airdrop_addresses(&self) -> (Vec<Pubkey>, Vec<SimnetEvent>) {
        let mut airdrop_addresses = vec![];
        let mut events = vec![];

        for address in self.accounts.airdrop_addresses.iter() {
            match Pubkey::from_str(address).map_err(|e| e.to_string()) {
                Ok(pubkey) => airdrop_addresses.push(pubkey),
                Err(e) => {
                    events.push(SimnetEvent::warn(format!(
                        "Unable to airdrop pubkey {}: Error parsing pubkey: {e}",
                        address
                    )));
                    continue;
                }
            }
        }

        let airdrop_keypair_path = self.accounts.airdrop_keypair_path.clone();

        if airdrop_keypair_path.is_empty() {
            let default_resolved_path = resolve_path(&DEFAULT_SOLANA_KEYPAIR_PATH);
            // No keypair paths provided: try default
            match Keypair::read_from_file(&default_resolved_path) {
                Ok(kp) => {
                    airdrop_addresses.push(kp.pubkey());
                    events.push(SimnetEvent::info(format!(
                        "No airdrop addresses provided; Using default keypair at {}",
                        DEFAULT_SOLANA_KEYPAIR_PATH.as_str()
                    )));
                }
                Err(_) => {
                    events.push(SimnetEvent::info(format!(
                        "No keypair found at default location {}, if you want to airdrop to a specific keypair provide the -k flag; skipping airdrops",
                        DEFAULT_SOLANA_KEYPAIR_PATH.as_str()
                    )));
                }
            }
        } else {
            // User provided paths: load each, warn on failures
            for keypair_path in airdrop_keypair_path.iter() {
                let path = resolve_path(keypair_path);
                match Keypair::read_from_file(&path) {
                    Ok(pubkey) => {
                        airdrop_addresses.push(pubkey.pubkey());
                    }
                    Err(_) => {
                        events.push(SimnetEvent::info(format!(
                            "No keypair found at provided path {}; skipping airdrop for that keypair",
                            path.display()
                        )));
                    }
                }
            }
        }

        (airdrop_addresses, events)
    }

    pub fn rpc_config(&self) -> RpcConfig {
        RpcConfig {
            bind_host: match env::var("SURFPOOL_NETWORK_HOST") {
                Ok(value) => value,
                _ => self.network.network_host.clone(),
            },
            bind_port: self.network.simnet_port,
            ws_port: self.network.ws_port,
            gossip_port: DEFAULT_GOSSIP_PORT,
            tpu_port: DEFAULT_TPU_PORT,
            tpu_quic_port: DEFAULT_TPU_QUIC_PORT,
        }
    }

    pub fn studio_config(&self) -> StudioConfig {
        StudioConfig {
            bind_host: match env::var("SURFPOOL_NETWORK_HOST") {
                Ok(value) => value,
                _ => self.network.network_host.clone(),
            },
            bind_port: self.runtime.studio_port,
        }
    }

    pub fn feature_config(&self) -> SvmFeatureConfig {
        let mut config = if self.svm.all_features {
            // Enable all SVM features from agave-feature-set (override mainnet defaults)
            SvmFeatureConfig::all_features_enabled()
        } else {
            // No overrides → run with the mainnet baseline (applied by LiteSVM
            // when the SVM is constructed).
            SvmFeatureConfig::default()
        };

        // Apply explicit enables (these override defaults)
        for pubkey in &self.svm.features {
            config = config.enable(*pubkey);
        }

        // Apply explicit disables (these override defaults)
        for pubkey in &self.svm.disable_features {
            config = config.disable(*pubkey);
        }

        config
    }

    pub fn svm_config(&self) -> SurfnetSvmConfig {
        SurfnetSvmConfig {
            surfnet_id: self.accounts.surfnet_id.clone(),
            feature_config: self.feature_config(),
            slot_time: self.svm.slot_time,
            instruction_profiling_enabled: !self.observability.disable_instruction_profiling,
            max_profiles: self.observability.max_profiles,
            log_bytes_limit: if self.observability.log_bytes_limit == 0 {
                None
            } else {
                Some(self.observability.log_bytes_limit)
            },
            skip_blockhash_check: self.svm.skip_blockhash_check,
        }
    }

    pub fn simnet_config(
        &self,
        airdrop_addresses: Vec<Pubkey>,
        snapshot: BTreeMap<String, Option<AccountSnapshot>>,
    ) -> SimnetConfig {
        let remote_rpc_url = if !self.network.offline {
            Some(self.datasource_rpc_url())
        } else {
            None
        };

        SimnetConfig {
            remote_rpc_url,
            slot_time: self.svm.slot_time,
            block_production_mode: self.svm.block_production_mode.clone(),
            airdrop_addresses,
            airdrop_token_amount: self.accounts.airdrop_token_amount,
            expiry: None,
            offline_mode: self.network.offline,
            instruction_profiling_enabled: !self.observability.disable_instruction_profiling,
            max_profiles: self.observability.max_profiles,
            log_bytes_limit: if self.observability.log_bytes_limit == 0 {
                None
            } else {
                Some(self.observability.log_bytes_limit)
            },
            skip_signature_verification: self.svm.skip_signature_verification,
            skip_blockhash_check: self.svm.skip_blockhash_check,
            surfnet_id: self.accounts.surfnet_id.clone(),
            snapshot,
        }
    }

    pub fn datasource_rpc_url(&self) -> String {
        match self.network.network {
            Some(NetworkType::Mainnet) => DEFAULT_MAINNET_RPC_URL.to_string(),
            Some(NetworkType::Devnet) => DEFAULT_DEVNET_RPC_URL.to_string(),
            Some(NetworkType::Testnet) => DEFAULT_TESTNET_RPC_URL.to_string(),
            None => self
                .network
                .rpc_url
                .clone()
                .unwrap_or_else(|| DEFAULT_MAINNET_RPC_URL.to_string()),
        }
    }

    pub fn subgraph_config(&self) -> SubgraphConfig {
        SubgraphConfig {}
    }

    pub fn surfpool_config(
        &self,
        airdrop_addresses: Vec<Pubkey>,
        snapshot: BTreeMap<String, Option<AccountSnapshot>>,
    ) -> SurfpoolConfig {
        let plugin_config_path = self
            .observability
            .plugin_config_path
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();

        SurfpoolConfig {
            simnets: vec![self.simnet_config(airdrop_addresses, snapshot)],
            rpc: self.rpc_config(),
            subgraph: self.subgraph_config(),
            studio: self.studio_config(),
            plugin_config_path,
        }
    }
}

#[derive(Parser, PartialEq, Clone, Debug)]
struct Completions {
    /// Shell to generate completions for.
    #[arg(ignore_case = true, value_name = "SHELL")]
    pub shell: Shell,
}

#[derive(Parser, PartialEq, Clone, Debug)]
pub struct ListRunbooks {
    /// Path to the runbook manifest.
    #[arg(
        long = "manifest-file-path",
        short = 'm',
        default_value = "./txtx.yml",
        value_name = "MANIFEST_PATH"
    )]
    pub manifest_path: String,
}

#[derive(Parser, PartialEq, Clone, Debug)]
#[command(group = clap::ArgGroup::new("execution_mode").multiple(false).args(["unsupervised", "web_console", "term_console"]).required(false))]
pub struct ExecuteRunbook {
    /// Path to the runbook manifest.
    #[arg(
        long = "manifest-file-path",
        short = 'm',
        default_value = "./txtx.yml",
        value_name = "MANIFEST_PATH",
        help_heading = "Project"
    )]
    pub manifest_path: String,
    /// Runbook ID from txtx.yml, or path to a .tx file.
    #[arg(value_name = "RUNBOOK")]
    pub runbook: String,
    /// Execute without the supervisor UI.
    #[arg(
        long = "unsupervised",
        short = 'u',
        action=ArgAction::SetTrue,
        group = "execution_mode",
        help_heading = "Execution Mode"
    )]
    pub unsupervised: bool,
    /// Execute with supervision in the browser UI.
    #[arg(
        long = "browser",
        short = 'b',
        action=ArgAction::SetTrue,
        group = "execution_mode",
        help_heading = "Execution Mode"
    )]
    pub web_console: bool,
    /// Execute with supervision in the terminal console.
    #[arg(
        long = "terminal",
        short = 't',
        action=ArgAction::SetTrue,
        group = "execution_mode",
        help_heading = "Execution Mode"
    )]
    pub term_console: bool,
    /// Print or write runbook outputs as JSON.
    #[arg(
        long = "output-json",
        value_name = "OUTPUT_DIR",
        help_heading = "Inputs & Outputs",
        long_help = "Print or write runbook outputs as JSON. When a directory is provided, output is written to a file in that directory."
    )]
    pub output_json: Option<Option<String>>,
    /// Print one named output at the end of execution.
    #[arg(
        long = "output",
        conflicts_with = "output_json",
        value_name = "OUTPUT_NAME",
        help_heading = "Inputs & Outputs"
    )]
    pub output: Option<String>,
    /// Explain how the runbook will execute.
    #[arg(long = "explain", action=ArgAction::SetTrue, help_heading = "Execution")]
    pub explain: bool,
    /// Set the port for hosting the web UI
    #[arg(
        long = "port",
        short = 'p',
        default_value = txtx_supervisor_ui::DEFAULT_BINDING_PORT,
        value_name = "PORT",
        help_heading = "Supervisor UI"
    )]
    #[cfg(feature = "supervisor_ui")]
    pub network_binding_port: u16,
    /// Set the port for hosting the web UI
    #[arg(
        long = "ip",
        short = 'i',
        default_value = txtx_supervisor_ui::DEFAULT_BINDING_ADDRESS,
        value_name = "IP_ADDRESS",
        help_heading = "Supervisor UI"
    )]
    #[cfg(feature = "supervisor_ui")]
    pub network_binding_ip_address: String,
    /// Environment from txtx.yml to use.
    #[arg(long = "env", value_name = "ENVIRONMENT", help_heading = "Project")]
    pub environment: Option<String>,
    /// Input file to use for batch processing.
    #[arg(
        long = "input",
        value_name = "INPUT_PATH",
        help_heading = "Inputs & Outputs"
    )]
    pub inputs: Vec<String>,
    /// Ignore cached execution state.
    #[arg(long = "force", short = 'f', help_heading = "Execution")]
    pub force_execution: bool,
    /// Runbook execution log level.
    #[arg(
        long = "log-level",
        short = 'l',
        default_value = "info",
        value_name = "LEVEL",
        help_heading = "Logging",
        long_help = "Runbook execution log level. Valid values are trace, debug, info, warn, or error."
    )]
    pub log_level: String,
    /// Directory for runbook execution logs.
    #[arg(
        long = "log-path",
        default_value = DEFAULT_LOG_DIR.as_str(),
        value_name = "LOG_DIR",
        help_heading = "Logging"
    )]
    pub log_dir: String,
}

#[derive(Parser, PartialEq, Clone, Debug)]
pub struct UpdateCommand {
    /// Flag to skip confirmation prompt
    #[arg(long = "yes", short = 'y')]
    pub skip_confirm: bool,
    /// To update to a specific version instead of the latest
    #[arg(long = "version", short = 'v')]
    pub version: Option<String>,
}

impl ExecuteRunbook {
    pub fn default_localnet(runbook_name: &str) -> ExecuteRunbook {
        ExecuteRunbook {
            manifest_path: "./txtx.yml".to_string(),
            runbook: runbook_name.to_string(),
            unsupervised: true,
            web_console: false,
            term_console: false,
            output_json: Some(Some(".surfpool/runbook-outputs".to_string())),
            output: None,
            explain: false,
            #[cfg(feature = "supervisor_ui")]
            network_binding_port: u16::from_str(txtx_supervisor_ui::DEFAULT_BINDING_PORT).unwrap(),
            #[cfg(feature = "supervisor_ui")]
            network_binding_ip_address: txtx_supervisor_ui::DEFAULT_BINDING_ADDRESS.to_string(),
            environment: Some("localnet".to_string()),
            inputs: vec![],
            force_execution: false,
            log_level: "info".to_string(),
            log_dir: DEFAULT_LOG_DIR.as_str().to_string(),
        }
    }

    pub fn with_manifest_path(mut self, manifest_path: String) -> Self {
        self.manifest_path = manifest_path;
        self
    }

    pub fn do_start_supervisor_ui(&self) -> bool {
        self.web_console || (!self.unsupervised && !self.term_console)
    }
}

pub fn main() {
    let logger = hiro_system_kit::log::setup_logger();
    let _guard = hiro_system_kit::log::setup_global_logger(logger.clone());
    let ctx = Context {
        logger: Some(logger),
        tracer: false,
    };

    let opts: Opts = match Opts::try_parse() {
        Ok(opts) => opts,
        Err(e) => {
            let _ = e.print();
            process::exit(e.exit_code());
        }
    };

    if let Err(e) = handle_command(opts, &ctx) {
        eprintln!("Error: {e}");
        std::thread::sleep(std::time::Duration::from_millis(500));
        process::exit(1);
    }
}

#[derive(Subcommand, PartialEq, Clone, Debug)]
pub enum McpCommand {}

pub async fn handle_mcp_command(_ctx: &Context) -> Result<(), String> {
    surfpool_mcp::run_server(&McpOptions::default()).await?;
    Ok(())
}

fn handle_command(opts: Opts, ctx: &Context) -> Result<(), String> {
    match opts.command {
        Command::Simnet(mut cmd) => {
            if cmd.runtime.ci {
                cmd.observability.disable_instruction_profiling = true;
                cmd.runtime.no_studio = true;
                cmd.runtime.no_tui = true;
                cmd.observability.log_level = "none".to_string();
            }

            if cmd.runtime.daemon {
                // The only way to support daemon mode on macos is to either:
                // - enforce --offline
                // - set OBJC_DISABLE_INITIALIZE_FORK_SAFETY=YES, which disables fork safety for Objective-C runtime
                // Known issue: https://github.com/firebase/firebase-tools/issues/6628
                // Both of these options are confusing for users, so we just emit a warning and disable daemon mode
                if !cfg!(target_os = "linux") {
                    println!("Daemon mode is only supported on Linux");
                    cmd.runtime.daemon = false;
                } else {
                    cmd.runtime.no_tui = true;
                }
            }

            if !cmd.observability.log_level.eq_ignore_ascii_case("none") {
                setup_logger(
                    &cmd.observability.log_dir,
                    None,
                    "simnet",
                    &cmd.observability.log_level,
                    cmd.runtime.no_tui,
                )?;
            }

            if cmd.runtime.daemon {
                #[cfg(not(target_os = "windows"))]
                match daemon(false, false) {
                    Ok(Fork::Child) => {
                        info!("Starting surfpool in daemon mode");
                    }
                    Ok(Fork::Parent(pid)) => {
                        info!("Parent exiting {pid}");
                        return Ok(());
                    }
                    Err(e) => {
                        info!("Failed to start surfpool in daemon mode: {}", e);
                        return Ok(());
                    }
                };
            }
            hiro_system_kit::nestable_block_on(simnet::handle_start_local_surfnet_command(cmd, ctx))
        }
        Command::Completions(cmd) => {
            hiro_system_kit::nestable_block_on(generate_completion_helpers(cmd))
        }
        Command::Run(cmd) => {
            hiro_system_kit::nestable_block_on(handle_execute_runbook_command(cmd))
        }
        Command::List(cmd) => hiro_system_kit::nestable_block_on(handle_list_command(cmd, ctx)),
        Command::Mcp => hiro_system_kit::nestable_block_on(handle_mcp_command(ctx)),
        Command::Update(cmd) => hiro_system_kit::nestable_block_on(handle_update_command(cmd)),
    }
}

async fn generate_completion_helpers(cmd: Completions) -> Result<(), String> {
    let mut app = Opts::command();
    let file_name = cmd.shell.file_name("surfpool");
    let mut file = File::create(file_name.clone())
        .map_err(|e| format!("unable to create file {}: {}", file_name, e))?;

    let prev_hook = take_hook();

    set_hook(Box::new(|_| {}));

    if let Err(e) = catch_unwind(AssertUnwindSafe(|| {
        clap_complete::generate(cmd.shell, &mut app, "surfpool", &mut file);
    })) {
        let msg = match () {
            _ if e.downcast_ref::<&'static str>().is_some() => {
                format!(
                    "Completion error: {}",
                    e.downcast_ref::<&'static str>().unwrap()
                )
            }
            _ => {
                format!("Completion generation failed: {e:#?}")
            }
        };
        println!("{msg}");
        process::exit(1);
    }
    set_hook(prev_hook); // restore so other panics still get reported

    println!("{} {}", green!("Created file"), file_name.clone());
    println!("Check your shell's docs for how to enable completions for surfpool.");
    Ok(())
}

async fn handle_list_command(cmd: ListRunbooks, _ctx: &Context) -> Result<(), String> {
    let manifest_location = FileLocation::from_path_string(&cmd.manifest_path)?;
    let manifest = WorkspaceManifest::from_location(&manifest_location)?;
    if manifest.runbooks.is_empty() {
        println!(
            "{}: no runbooks referenced in the txtx.yml manifest.\nRun the command `txtx new` to create a new runbook.",
            yellow!("warning")
        );
        std::process::exit(1);
    }
    println!("{:<35}\t{}", "Name", yellow!("Description"));
    for runbook in manifest.runbooks {
        println!(
            "{:<35}\t{}",
            runbook.name,
            yellow!(format!("{}", runbook.description.unwrap_or("".into())))
        );
    }
    Ok(())
}

pub fn setup_logger(
    log_dir: &str,
    environment_selector: Option<&str>,
    filename: &str,
    log_filter: &str,
    log_to_stdout: bool,
) -> Result<(), String> {
    let log_location = {
        let mut log_location = FileLocation::from_path_string(log_dir)?;
        if let Some(env) = environment_selector {
            log_location.append_path(env)?;
        }
        let timestamp = chrono::Local::now()
            .format("%Y-%m-%d--%H-%M-%S")
            .to_string();
        let filename = format!("{}_{}.log", filename, timestamp);
        log_location.append_path(&filename)?;

        if !log_location.exists() {
            log_location
                .create_dir_and_file()
                .map_err(|e| format!("Failed to create log file {}: {}", log_location, e))?;
        }
        log_location
    };

    let log_filter = match log_filter.into() {
        LogLevel::Info => log::LevelFilter::Info,
        LogLevel::Warn => log::LevelFilter::Warn,
        LogLevel::Error => log::LevelFilter::Error,
        LogLevel::Debug => log::LevelFilter::Debug,
        LogLevel::Trace => log::LevelFilter::Trace,
    };

    let colors = ColoredLevelConfig::new()
        .info(Color::Green)
        .warn(Color::Yellow)
        .error(Color::Red)
        .debug(Color::Blue)
        .trace(Color::White);

    // File branch: full format, no filtering
    let file_config = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{} {} {}] {}",
                Local::now().format("%Y-%m-%d--%H-%M-%S"),
                record.level(),
                record.target(),
                message
            ))
        })
        .chain(
            fern::log_file(log_location.to_string())
                .map_err(|e| format!("Failed to create log file: {}", e))?,
        );

    // Stdout branch: filtered to only txtx/surfopol target, minimal + colored format
    let stdout_config = fern::Dispatch::new()
        .filter(|metadata| {
            metadata.target().starts_with("txtx") || metadata.target().starts_with("surfpool")
        })
        .format(move |out, message, record| {
            out.finish(format_args!(
                "{} {} {}",
                Local::now().format("%b %d %H:%M:%S%.3f"),
                colors.color(record.level()),
                message
            ))
        })
        .chain(std::io::stdout());

    let mut builder = fern::Dispatch::new().level(log_filter).chain(file_config);

    if log_to_stdout {
        builder = builder.chain(stdout_config)
    }

    builder
        .apply()
        .map_err(|e| format!("Failed to initialize logger: {}", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_long_help() -> String {
        let mut command = Opts::command();
        let start = command
            .find_subcommand_mut("start")
            .expect("start subcommand should be registered");
        let mut buffer = Vec::new();
        start
            .write_long_help(&mut buffer)
            .expect("start help should render");
        String::from_utf8(buffer).expect("help should be utf8")
    }

    fn parse_start(args: &[&str]) -> StartSimnet {
        match Opts::try_parse_from(args)
            .expect("start args should parse")
            .command
        {
            Command::Simnet(cmd) => cmd,
            command => panic!("expected start command, got {command:?}"),
        }
    }

    #[test]
    fn start_help_groups_related_flags() {
        let help = start_long_help();

        for heading in [
            "Network & Ports",
            "Project & Deployment",
            "Accounts & State",
            "Runtime & UI",
            "SVM Behavior",
            "Observability & Performance",
        ] {
            assert!(help.contains(heading), "missing heading {heading}");
        }

        for flag in [
            "--rpc-url <RPC_URL>",
            "--runbook <RUNBOOK>",
            "--airdrop <PUBKEY>",
            "--no-tui",
            "--feature <FEATURE_PUBKEY>",
            "--geyser-plugin-config <PLUGIN_CONFIG_PATH>",
        ] {
            assert!(help.contains(flag), "missing flag {flag}");
        }

        assert!(!help.contains("--subgraph-db"));
        assert!(help.contains("Examples:"));
        assert!(help.contains("surfpool start --network devnet"));
    }

    #[test]
    fn start_parser_keeps_existing_repeated_flags_and_defaults() {
        let default_cmd = parse_start(&["surfpool", "start"]);
        assert_eq!(default_cmd.project.runbooks, vec![DEFAULT_RUNBOOK]);

        let cmd = parse_start(&[
            "surfpool",
            "start",
            "--airdrop",
            "5cQvx11111111111111111111111111111111111111",
            "--airdrop",
            "5cQvy11111111111111111111111111111111111111",
            "--runbook",
            "deploy",
            "--runbook",
            "seed",
        ]);

        assert_eq!(
            cmd.accounts.airdrop_addresses,
            vec![
                "5cQvx11111111111111111111111111111111111111",
                "5cQvy11111111111111111111111111111111111111",
            ]
        );
        assert_eq!(cmd.project.runbooks, vec!["deploy", "seed"]);
    }

    #[test]
    fn start_parser_keeps_rpc_network_conflict() {
        let err = Opts::try_parse_from([
            "surfpool",
            "start",
            "--rpc-url",
            "https://api.mainnet-beta.solana.com",
            "--network",
            "devnet",
        ])
        .expect_err("rpc-url and network should conflict");

        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn start_parser_accepts_hidden_deprecated_subgraph_db() {
        let cmd = parse_start(&["surfpool", "start", "--subgraph-db", "./legacy.sqlite"]);
        assert_eq!(cmd.subgraph_db.as_deref(), Some("./legacy.sqlite"));
    }
}
