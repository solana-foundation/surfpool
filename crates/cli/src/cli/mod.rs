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
use clap::{ArgAction, CommandFactory, Parser, Subcommand};
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

use crate::runbook::handle_execute_runbook_command;

mod simnet;

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
    /// Start Simnet
    #[clap(name = "start", bin_name = "start", aliases = &["simnet"])]
    Simnet(StartSimnet),
    /// Generate shell completions scripts
    #[clap(name = "completions", bin_name = "completions", aliases = &["completion"])]
    Completions(Completions),
    /// Run, runbook, run!
    #[clap(name = "run", bin_name = "run")]
    Run(ExecuteRunbook),
    /// List runbooks present in the current directory
    #[clap(name = "ls", bin_name = "ls")]
    List(ListRunbooks),
    /// Start MCP server
    #[clap(name = "mcp", bin_name = "mcp")]
    Mcp,
}

#[derive(Parser, PartialEq, Clone, Debug)]
pub struct StartSimnet {
    /// Path to the runbook manifest, used to locate the root of the project (eg. surfpool start --manifest-file-path ./txtx.yml)
    #[arg(long = "manifest-file-path", short = 'm', default_value = "./txtx.yml")]
    pub manifest_path: String,
    /// Set the Simnet RPC port (eg. surfpool start --port 8080)
    #[arg(long = "port", short = 'p', default_value_t = DEFAULT_RPC_PORT)]
    pub simnet_port: u16,
    /// Set the Simnet WS port
    #[arg(long = "ws-port", short = 'w', default_value_t = DEFAULT_WS_PORT)]
    pub ws_port: u16,
    /// Set the Simnet host address (eg. surfpool start --host 127.0.0.1)
    #[arg(long = "host", short = 'o', default_value = DEFAULT_NETWORK_HOST)]
    pub network_host: String,
    /// Set the slot time (eg. surfpool start --slot-time 400)
    #[arg(long = "slot-time", short = 't', default_value_t = DEFAULT_SLOT_TIME_MS)]
    pub slot_time: u64,
    /// Set the block production mode (eg. surfpool start --block-production-mode transaction)
    #[arg(long = "block-production-mode", short = 'b', default_value_t = BlockProductionMode::Clock)]
    pub block_production_mode: BlockProductionMode,
    /// Set a datasource RPC URL (cannot be used with --network). Can also be set via SURFPOOL_DATASOURCE_RPC_URL. (eg. surfpool start --rpc-url https://api.mainnet-beta.solana.com)
    #[arg(
        long = "rpc-url",
        short = 'u',
        conflicts_with = "network",
        env = "SURFPOOL_DATASOURCE_RPC_URL"
    )]
    pub rpc_url: Option<String>,
    /// Choose a predefined network (cannot be used with --rpc-url) (eg. surfpool start --network mainnet)
    #[arg(long = "network", short = 'n', value_enum, conflicts_with = "rpc_url")]
    pub network: Option<NetworkType>,
    /// Display streams of logs instead of terminal UI dashboard(eg. surfpool start --no-tui)
    #[clap(long = "no-tui", default_value = "false")]
    pub no_tui: bool,
    /// Disable auto deployments (eg. surfpool start --no-deploy)
    #[clap(long = "no-deploy", default_value = "false")]
    pub no_deploy: bool,
    /// List of runbooks-id to run (eg. surfpool start --runbook runbook-1 --runbook runbook-2)
    #[arg(long = "runbook", short = 'r', default_value = DEFAULT_RUNBOOK)]
    pub runbooks: Vec<String>,
    /// Skip prompts for generating runbooks, and assume "yes" for all (eg. surfpool start -y)
    #[clap(long = "yes", short = 'y', action=ArgAction::SetTrue,  default_value = "false")]
    pub skip_runbook_generation_prompts: bool,
    /// List of pubkeys to airdrop (eg. surfpool start --airdrop 5cQvx... --airdrop 5cQvy...)
    #[arg(long = "airdrop", short = 'a')]
    pub airdrop_addresses: Vec<String>,
    /// Quantity of tokens to airdrop
    #[arg(long = "airdrop-amount", short = 'q', default_value = DEFAULT_AIRDROP_AMOUNT)]
    pub airdrop_token_amount: u64,
    /// List of keypair paths to airdrop (eg. surfpool start --airdrop-keypair-path ~/.config/solana/id.json --airdrop-keypair-path ~/.config/solana/id2.json)
    #[arg(long = "airdrop-keypair-path", short = 'k', default_value = DEFAULT_SOLANA_KEYPAIR_PATH.as_str())]
    pub airdrop_keypair_path: Vec<String>,
    /// Watch programs in your artifacts folder (default: `target/deploy`), and automatically re-execute the deployment runbook when the `.so` files change. (eg. surfpool start --watch)
    #[clap(long = "watch", action=ArgAction::SetTrue, default_value = "false")]
    pub watch: bool,
    /// Override the path where .so program artifacts are loaded from (default: target/deploy). (eg. surfpool start --artifacts-path ./target/deploy/debug)
    #[arg(long = "artifacts-path")]
    pub artifacts_path: Option<String>,
    /// List of geyser plugins to load (eg. surfpool start --geyser-plugin-config plugin1.json --geyser-plugin-config plugin2.json)
    #[arg(long = "geyser-plugin-config", short = 'g')]
    pub plugin_config_path: Vec<String>,
    /// This flag has been deprecated.
    #[arg(long = "subgraph-db", short = 'd', default_value = ":memory:")]
    pub subgraph_db: Option<String>,
    /// Disable Studio (eg. surfpool start --no-studio)
    #[clap(long = "no-studio", default_value = "false")]
    pub no_studio: bool,
    /// Set the Studio port (eg. surfpool start --studio-port 8080)
    #[arg(long = "studio-port", short = 's', default_value_t = CHANGE_TO_DEFAULT_STUDIO_PORT_ONCE_SUPERVISOR_MERGED)]
    pub studio_port: u16,
    /// Start surfpool without a remote RPC client to simulate an offline environment (eg. surfpool start --offline)
    #[clap(long = "offline", action=ArgAction::SetTrue, default_value = "false")]
    pub offline: bool,
    /// Disable instruction profiling (eg. surfpool start --disable-instruction-profiling)
    #[clap(long = "disable-instruction-profiling", action=ArgAction::SetTrue)]
    pub disable_instruction_profiling: bool,
    /// The log level to use for simnet logs. Options are "trace", "debug", "info", "warn", "error", or "none". (eg. surfpool start --log-level debug)
    #[arg(long = "log-level", short = 'l', default_value = "info")]
    pub log_level: String,
    /// The directory to put simnet logs. (eg. surfpool start --log-path ./logs)
    #[arg(long = "log-path", default_value = DEFAULT_LOG_DIR.as_str())]
    pub log_dir: String,
    /// The maximum number of transaction profiles to hold in memory.
    /// Changing this will affect the memory usage of surfpool. (eg. surfpool start --max-profiles 2000)
    #[arg(long = "max-profiles", short = 'c', default_value = "200")]
    pub max_profiles: usize,
    /// The maximum number of bytes to allow in transaction logs. Set to 0 for unlimited. (eg. surfpool start --log-bytes-limit 64000)
    #[arg(long = "log-bytes-limit", default_value = "10000")]
    pub log_bytes_limit: usize,
    /// Start Surfpool as a background process (eg. surfpool start --daemon)
    #[clap(long = "daemon", action=ArgAction::SetTrue, default_value = "false")]
    pub daemon: bool,
    /// Start surfpool with some CI adequate settings  (eg. surfpool start --ci)
    #[clap(long = "ci", action=ArgAction::SetTrue, default_value = "false")]
    pub ci: bool,
    /// Apply suggested defaults for runbook generation and execution when running as part of an anchor test suite (eg. surfpool start --legacy-anchor-compatibility)
    #[clap(long = "legacy-anchor-compatibility", action=ArgAction::SetTrue, default_value = "false")]
    pub anchor_compat: bool,
    /// Path to the Test.toml test suite files to load (eg. surfpool start --anchor-test-config-path ./path/to/Test.toml)
    #[arg(long = "anchor-test-config-path")]
    pub anchor_test_config_paths: Vec<String>,
    /// Enable specific SVM features by pubkey. Can be specified multiple times. (eg. surfpool start --feature <base58-pubkey> --feature <another-pubkey>)
    /// Note: providing feature names has been deprecated. Previously supported feature names will still work, but this will be dropped in the future.
    /// Instead, you should migrate to providing the pubkey for the feature.
    #[arg(long = "feature", short = 'f', value_parser = parse_feature_pubkey)]
    pub features: Vec<Pubkey>,
    /// Disable specific SVM features by pubkey. Can be specified multiple times. (eg. surfpool start --disable-feature <base58-pubkey> --disable-feature <another-pubkey>)
    /// Note: providing feature names has been deprecated. Previously supported feature names will still work, but this will be dropped in the future.
    /// Instead, you should migrate to providing the pubkey for the feature.
    #[arg(long = "disable-feature", value_parser = parse_feature_pubkey)]
    pub disable_features: Vec<Pubkey>,
    /// Enable all SVM features from agave-feature-set (override mainnet defaults)
    #[clap(long = "features-all", action=ArgAction::SetTrue, default_value = "false")]
    pub all_features: bool,
    /// A set of inputs to use for the runbook (eg. surfpool start --runbook-input myInputs.json)
    #[arg(long = "runbook-input", short = 'i')]
    pub runbook_input: Vec<String>,
    /// Surfnet database connection URL for persistent Surfnets. For an in-memory sqlite database, use ":memory:". For an on-disk sqlite database, use a filename ending in '.sqlite'.
    #[arg(long = "db")]
    pub db: Option<String>,
    /// Unique identifier for this surfnet instance. Used to isolate database storage when multiple surfnets share the same database. Defaults to "default".
    #[arg(long = "surfnet-id", default_value = "default")]
    pub surfnet_id: String,
    /// Path to JSON snapshot file(s) to preload accounts from. Can be specified multiple times.
    /// (eg. surfpool start --snapshot ./snapshot1.json --snapshot ./snapshot2.json)
    /// The snapshot format matches the output of surfnet_exportSnapshot RPC method.
    /// Account values can be null to fetch the account from the remote RPC instead.
    /// When multiple files are provided, later files override earlier ones for duplicate keys.
    #[arg(long = "snapshot")]
    pub snapshot: Vec<String>,
    /// Enable Prometheus metrics endpoint
    #[cfg(feature = "prometheus")]
    /// Enable Prometheus metrics endpoint
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
    /// Skip signature verification for all transactions (eg. surfpool start --skip-signature-verification)
    #[clap(long = "skip-signature-verification", action=ArgAction::SetTrue, default_value = "false")]
    pub skip_signature_verification: bool,
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

        for address in self.airdrop_addresses.iter() {
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

        let airdrop_keypair_path = self.airdrop_keypair_path.clone();

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
                _ => self.network_host.clone(),
            },
            bind_port: self.simnet_port,
            ws_port: self.ws_port,
            gossip_port: DEFAULT_GOSSIP_PORT,
            tpu_port: DEFAULT_TPU_PORT,
            tpu_quic_port: DEFAULT_TPU_QUIC_PORT,
        }
    }

    pub fn studio_config(&self) -> StudioConfig {
        StudioConfig {
            bind_host: match env::var("SURFPOOL_NETWORK_HOST") {
                Ok(value) => value,
                _ => self.network_host.clone(),
            },
            bind_port: self.studio_port,
        }
    }

    pub fn feature_config(&self) -> SvmFeatureConfig {
        let mut config = if self.all_features {
            // Enable all SVM features from agave-feature-set (override mainnet defaults)
            let mut cfg = SvmFeatureConfig::default();
            for pubkey in agave_feature_set::FEATURE_NAMES.keys() {
                cfg = cfg.enable(*pubkey);
            }
            cfg
        } else {
            // Use mainnet defaults by default
            SvmFeatureConfig::default_mainnet_features()
        };

        // Apply explicit enables (these override defaults)
        for pubkey in &self.features {
            config = config.enable(*pubkey);
        }

        // Apply explicit disables (these override defaults)
        for pubkey in &self.disable_features {
            config = config.disable(*pubkey);
        }

        config
    }

    pub fn svm_config(&self) -> SurfnetSvmConfig {
        SurfnetSvmConfig {
            surfnet_id: self.surfnet_id.clone(),
            feature_config: self.feature_config(),
            slot_time: self.slot_time,
            instruction_profiling_enabled: !self.disable_instruction_profiling,
            max_profiles: self.max_profiles,
            log_bytes_limit: if self.log_bytes_limit == 0 {
                None
            } else {
                Some(self.log_bytes_limit)
            },
        }
    }

    pub fn simnet_config(
        &self,
        airdrop_addresses: Vec<Pubkey>,
        snapshot: BTreeMap<String, Option<AccountSnapshot>>,
    ) -> SimnetConfig {
        let remote_rpc_url = if !self.offline {
            Some(self.datasource_rpc_url())
        } else {
            None
        };

        SimnetConfig {
            remote_rpc_url,
            slot_time: self.slot_time,
            block_production_mode: self.block_production_mode.clone(),
            airdrop_addresses,
            airdrop_token_amount: self.airdrop_token_amount,
            expiry: None,
            offline_mode: self.offline,
            instruction_profiling_enabled: !self.disable_instruction_profiling,
            max_profiles: self.max_profiles,
            log_bytes_limit: if self.log_bytes_limit == 0 {
                None
            } else {
                Some(self.log_bytes_limit)
            },
            skip_signature_verification: self.skip_signature_verification,
            surfnet_id: self.surfnet_id.clone(),
            snapshot,
        }
    }

    pub fn datasource_rpc_url(&self) -> String {
        match self.network {
            Some(NetworkType::Mainnet) => DEFAULT_MAINNET_RPC_URL.to_string(),
            Some(NetworkType::Devnet) => DEFAULT_DEVNET_RPC_URL.to_string(),
            Some(NetworkType::Testnet) => DEFAULT_TESTNET_RPC_URL.to_string(),
            None => self
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
    /// Specify which shell to generation completions script for
    #[arg(ignore_case = true)]
    pub shell: Shell,
}

#[derive(Parser, PartialEq, Clone, Debug)]
pub struct ListRunbooks {
    /// Path to the manifest
    #[arg(long = "manifest-file-path", short = 'm', default_value = "./txtx.yml")]
    pub manifest_path: String,
}

#[derive(Parser, PartialEq, Clone, Debug)]
#[command(group = clap::ArgGroup::new("execution_mode").multiple(false).args(["unsupervised", "web_console", "term_console"]).required(false))]
pub struct ExecuteRunbook {
    /// Path to the manifest
    #[arg(long = "manifest-file-path", short = 'm', default_value = "./txtx.yml")]
    pub manifest_path: String,
    /// Name of the runbook as indexed in the txtx.yml, or the path of the .tx file to run
    pub runbook: String,
    /// Execute the runbook without supervision
    #[arg(long = "unsupervised", short = 'u', action=ArgAction::SetTrue, group = "execution_mode")]
    pub unsupervised: bool,
    /// Execute the runbook with supervision via the browser UI (this is the default execution mode)
    #[arg(long = "browser", short = 'b', action=ArgAction::SetTrue, group = "execution_mode")]
    pub web_console: bool,
    /// Execute the runbook with supervision via the terminal console (coming soon)
    #[arg(long = "terminal", short = 't', action=ArgAction::SetTrue, group = "execution_mode")]
    pub term_console: bool,
    /// When running in unsupervised mode, print outputs in JSON format. If a directory is provided, the output will be written a file at the directory.
    #[arg(long = "output-json")]
    pub output_json: Option<Option<String>>,
    /// Pick a specific output to stdout at the end of the execution
    #[arg(long = "output", conflicts_with = "output_json")]
    pub output: Option<String>,
    /// Explain how the runbook will be executed.
    #[arg(long = "explain", action=ArgAction::SetTrue)]
    pub explain: bool,
    /// Set the port for hosting the web UI
    #[arg(long = "port", short = 'p', default_value = txtx_supervisor_ui::DEFAULT_BINDING_PORT )]
    #[cfg(feature = "supervisor_ui")]
    pub network_binding_port: u16,
    /// Set the port for hosting the web UI
    #[arg(long = "ip", short = 'i', default_value = txtx_supervisor_ui::DEFAULT_BINDING_ADDRESS )]
    #[cfg(feature = "supervisor_ui")]
    pub network_binding_ip_address: String,
    /// Choose the environment variable to set from those configured in the txtx.yml
    #[arg(long = "env")]
    pub environment: Option<String>,
    /// A set of inputs to use for batch processing
    #[arg(long = "input")]
    pub inputs: Vec<String>,
    /// Execute the Runbook even if the cached state suggests this Runbook has already been executed
    #[arg(long = "force", short = 'f')]
    pub force_execution: bool,
    /// The log level to use for the runbook execution. Options are "trace", "debug", "info", "warn", "error".
    #[arg(long = "log-level", short = 'l', default_value = "info")]
    pub log_level: String,
    /// The directory to put runbook execution logs.
    #[arg(long = "log-path", default_value = DEFAULT_LOG_DIR.as_str())]
    pub log_dir: String,
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
            if cmd.ci {
                cmd.disable_instruction_profiling = true;
                cmd.no_studio = true;
                cmd.no_tui = true;
                cmd.log_level = "none".to_string();
            }

            if cmd.daemon {
                // The only way to support daemon mode on macos is to either:
                // - enforce --offline
                // - set OBJC_DISABLE_INITIALIZE_FORK_SAFETY=YES, which disables fork safety for Objective-C runtime
                // Known issue: https://github.com/firebase/firebase-tools/issues/6628
                // Both of these options are confusing for users, so we just emit a warning and disable daemon mode
                if !cfg!(target_os = "linux") {
                    println!("Daemon mode is only supported on Linux");
                    cmd.daemon = false;
                } else {
                    cmd.no_tui = true;
                }
            }

            if !cmd.log_level.eq_ignore_ascii_case("none") {
                setup_logger(&cmd.log_dir, None, "simnet", &cmd.log_level, cmd.no_tui)?;
            }

            if cmd.daemon {
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
