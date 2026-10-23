use surfpool_types::{HistoricalSessionOptions, HistoricalSimnetConfig, SanitizedHistoricalConfig};

use super::*;

#[derive(Args, PartialEq, Clone, Default)]
#[command(next_help_heading = "Historical state")]
pub struct Options {
    /// Fork after a transaction signature or at the initialized next produced slot after a slot number.
    #[arg(long, value_name = "SIGNATURE|SLOT", conflicts_with_all = ["rpc_url", "network", "offline", "block_production_mode", "features", "disable_features", "all_features", "skip_signature_verification", "skip_blockhash_check"])]
    pub after: Option<String>,
    /// Historical provider adapter name, not a key (currently alchemy).
    #[arg(long, value_name = "NAME", requires = "after")]
    historical_provider: Option<String>,
    /// Provider settings; uses ./Surfpool.toml when present.
    #[arg(long, value_name = "FILE", requires = "after")]
    historical_config: Option<PathBuf>,
    /// Provider credential reference env:VARIABLE; literal credentials are refused.
    #[arg(
        long = "key",
        alias = "historical-key",
        value_name = "env:NAME",
        requires = "after",
        conflicts_with = "historical_key_file"
    )]
    key: Option<String>,
    /// Read the provider credential from FILE.
    #[arg(
        long = "key-file",
        alias = "historical-key-file",
        value_name = "FILE",
        requires = "after"
    )]
    historical_key_file: Option<PathBuf>,
    /// Store cached history and runs in DIR.
    #[arg(
        long,
        value_name = "DIR",
        requires = "after",
        default_value = ".surfpool/history"
    )]
    historical_data_dir: PathBuf,
    /// Advanced: use a reviewed historical runtime registry with its independent pin.
    #[arg(long, value_name = "FILE", requires_all = ["after", "historical_runtime_registry_sha256"], hide = true)]
    historical_runtime_registry: Option<PathBuf>,
    /// Trusted SHA-256 of the advanced runtime registry.
    #[arg(long, value_name = "SHA256", requires_all = ["after", "historical_runtime_registry"], hide = true)]
    historical_runtime_registry_sha256: Option<String>,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoricalOptions")
            .field("after", &self.after)
            .field("provider", &self.historical_provider)
            .field("config", &self.historical_config)
            .field("credential", &self.key.as_ref().map(|_| "[REDACTED]"))
            .field("key_file", &self.historical_key_file)
            .finish()
    }
}

/// SVM Replay's backend for `surfpool start --after`: Surfpool then runs its ordinary
/// runloop, RPC and workflows, and SVM Replay executes the historical state.
/// What interfaces (Studio's /config, the TUI) learn about a surfnet's state source: a
/// historical surfnet has no datasource and names what it forks after; a normal surfnet keeps
/// its sanitized datasource.
pub(super) fn interface_sources(
    cmd: &StartSimnet,
    datasource: Option<String>,
) -> (Option<String>, Option<SanitizedHistoricalConfig>) {
    match &cmd.historical.after {
        Some(after) => (
            None,
            Some(SanitizedHistoricalConfig {
                after: after.clone(),
            }),
        ),
        None => (datasource, None),
    }
}

pub(super) fn config(cmd: &StartSimnet) -> Result<Option<HistoricalSimnetConfig>, String> {
    let options = &cmd.historical;
    let Some(after) = &options.after else {
        return Ok(None);
    };
    if options
        .key
        .as_ref()
        .is_some_and(|key| !key.starts_with("env:") || key.len() == 4)
    {
        return Err(
            "Use --key env:NAME or --key-file FILE; literal provider credentials are refused"
                .into(),
        );
    }
    let config = match &options.historical_config {
        Some(path) => Some(path.clone()),
        None => match std::fs::symlink_metadata("Surfpool.toml") {
            Ok(_) => Some(PathBuf::from("Surfpool.toml")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("Cannot inspect Surfpool.toml: {error}")),
        },
    };
    let runtime_registry = match (
        &options.historical_runtime_registry,
        &options.historical_runtime_registry_sha256,
    ) {
        (Some(path), Some(pin)) => Some((path.clone(), pin.clone())),
        (None, None) => None,
        _ => return Err("Historical runtime registry requires its independent SHA-256 pin".into()),
    };
    // A persistent store keeps the fork's journal beside its history, bound to that store and
    // surfnet by digest only (a database URL is never written out). An in-memory store keeps
    // nothing to restore.
    let journal = match cmd.accounts.db.as_deref() {
        None | Some(":memory:") => None,
        Some(db) => {
            let identity = match db.contains("://") {
                true => database_identity(db),
                false => std::path::absolute(db)
                    .map_err(|e| format!("Cannot resolve --db {db}: {e}"))?
                    .to_string_lossy()
                    .into_owned(),
            };
            let binding = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(format!(
                "surfpool-historical-journal/v1|{identity}|{}",
                cmd.accounts.surfnet_id
            )));
            let journal = options
                .historical_data_dir
                .join("journals")
                .join(format!("{binding}.jsonl"));
            Some((journal, binding))
        }
    };
    HistoricalSimnetConfig::session(HistoricalSessionOptions {
        after: after.clone(),
        program: env::var("SVM_REPLAY_BIN").ok(),
        data_dir: Some(options.historical_data_dir.clone()),
        provider: options.historical_provider.clone(),
        config,
        key: options.key.clone(),
        key_file: options.historical_key_file.clone(),
        runtime_registry,
        journal,
    })
    .map(Some)
}

/// A database URL's identity without its password or parameters, so the journal's binding
/// derives from no credential.
fn database_identity(url: &str) -> String {
    let url = url.split(['?', '#']).next().unwrap_or(url);
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    match authority.rsplit_once('@') {
        Some((userinfo, host)) => {
            let user = userinfo.split(':').next().unwrap_or_default();
            format!("{scheme}://{user}@{host}{path}")
        }
        None => format!("{scheme}://{authority}{path}"),
    }
}

#[cfg(test)]
mod tests;
