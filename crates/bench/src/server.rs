//! Lifecycle of the surfpool process under test. The benchmark never links the
//! surfnet; it launches the real binary and talks to it like any client.

use std::{
    net::TcpListener,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use tempfile::TempDir;
use tokio::process::{Child, Command};

/// How long to wait for `getHealth` to answer before giving up on a start.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// Flags that differ between scenarios. Everything else is fixed so every
/// scenario sees the same surfnet: offline, no TUI, no Studio, no logs, no
/// runbook deployment, no startup airdrop.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    pub instruction_profiling: bool,
    pub slot_time_ms: u64,
}

/// A running surfpool process bound to free localhost ports. Killed on drop,
/// along with its scratch directory.
pub struct SurfpoolServer {
    child: Child,
    // Held only so the directory outlives the process.
    _workdir: TempDir,
    stderr_path: PathBuf,
    pub rpc_url: String,
}

impl SurfpoolServer {
    /// Launches `bin start` with `opts`, waits until the RPC server reports
    /// healthy, and returns the handle. Fails if the process exits early or
    /// never becomes healthy; in that case the tail of its stderr is included
    /// in the error.
    pub async fn start(bin: &Path, opts: &ServerOptions) -> Result<Self> {
        // The child runs in a scratch directory, so a relative binary path has
        // to be resolved against the caller's directory first.
        let bin = std::fs::canonicalize(bin)
            .with_context(|| format!("resolving surfpool binary {}", bin.display()))?;
        let rpc_port = free_port()?;
        let ws_port = free_port()?;
        // A private directory created exclusively, so nothing another user
        // planted under a guessable name can be followed when the log is
        // created.
        let workdir = tempfile::Builder::new()
            .prefix("surfpool-bench-")
            .tempdir()
            .context("creating surfpool scratch directory")?;
        let stderr_path = workdir.path().join("surfpool.stderr");
        let stderr_file =
            std::fs::File::create(&stderr_path).context("creating surfpool stderr capture")?;

        let mut cmd = Command::new(&bin);
        cmd.arg("start")
            .arg("--offline")
            .arg("--no-tui")
            .arg("--no-studio")
            .arg("--no-deploy")
            .arg("--log-level")
            .arg("none")
            .arg("--airdrop-amount")
            .arg("0")
            .arg("--port")
            .arg(rpc_port.to_string())
            .arg("--ws-port")
            .arg(ws_port.to_string())
            .arg("--slot-time")
            .arg(opts.slot_time_ms.to_string());
        if !opts.instruction_profiling {
            cmd.arg("--disable-instruction-profiling");
        }
        cmd.current_dir(workdir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file))
            .kill_on_drop(true);

        let child = cmd
            .spawn()
            .with_context(|| format!("spawning {}", bin.display()))?;
        let rpc_url = format!("http://127.0.0.1:{rpc_port}");
        let mut server = Self {
            child,
            _workdir: workdir,
            stderr_path,
            rpc_url,
        };
        server.wait_until_healthy().await?;
        Ok(server)
    }

    async fn wait_until_healthy(&mut self) -> Result<()> {
        let client = RpcClient::new(self.rpc_url.clone());
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().context("polling surfpool")? {
                bail!(
                    "surfpool exited during startup with {status}\n{}",
                    self.stderr_tail()
                );
            }
            if client.get_health().await.is_ok() {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!(
                    "surfpool did not become healthy within {STARTUP_TIMEOUT:?}\n{}",
                    self.stderr_tail()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn stderr_tail(&self) -> String {
        let text = std::fs::read_to_string(&self.stderr_path).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(30);
        format!(
            "--- surfpool stderr (last 30 lines) ---\n{}",
            lines[start..].join("\n")
        )
    }

    /// Kills the process and waits for it to exit. The scratch directory is
    /// removed when the handle drops.
    pub async fn stop(mut self) -> Result<()> {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        Ok(())
    }
}

/// Returns `bin --version` with surrounding whitespace trimmed.
pub async fn surfpool_version(bin: &Path) -> Result<String> {
    let output = Command::new(bin)
        .arg("--version")
        .output()
        .await
        .with_context(|| format!("running {} --version", bin.display()))?;
    if !output.status.success() {
        bail!("{} --version exited with {}", bin.display(), output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Asks the kernel for an unused localhost TCP port. The port is released
/// before surfpool binds it, so a concurrent process could still take it; the
/// startup health check catches that case.
fn free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("reserving a localhost port")?;
    Ok(listener.local_addr()?.port())
}
