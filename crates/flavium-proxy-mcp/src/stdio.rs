//! Production transport wiring: the proxy process's own stdin/stdout as
//! the client side, and a spawned child process as the upstream side.
//!
//! stdout carries only MCP frames; all logging goes to stderr, and the
//! upstream child's stderr is inherited so its own logs surface
//! alongside the proxy's.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::proxy::{self, ProxyConfig, ProxyError, ProxySummary};

/// How long the upstream child gets to exit after its stdin closes
/// before it is killed — the MCP stdio shutdown sequence.
const CHILD_EXIT_GRACE: Duration = Duration::from_secs(5);

/// Errors starting or finishing a stdio-served session.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// No upstream command was given.
    #[error("upstream command is empty")]
    EmptyCommand,

    /// The upstream process could not be started.
    #[error("failed to spawn upstream `{command}`")]
    Spawn {
        /// The program that failed to start.
        command: String,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// The spawned child was missing a piped stream — an internal bug.
    #[error("upstream child had no piped {stream}")]
    MissingPipe {
        /// Which stream was missing.
        stream: &'static str,
    },

    /// The proxy core failed.
    #[error(transparent)]
    Proxy(#[from] ProxyError),
}

/// Serves one MCP session: this process's stdin/stdout as the client
/// side, a freshly spawned `command` as the upstream side. Returns when
/// the session ends; the child is reaped (killed after a grace period if
/// it ignores stdin closing).
pub async fn serve(config: ProxyConfig, command: &[String]) -> Result<ProxySummary, ServeError> {
    let (program, args) = command.split_first().ok_or(ServeError::EmptyCommand)?;
    info!(upstream = %command.join(" "), "starting flavium MCP stdio proxy");

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| ServeError::Spawn {
            command: program.clone(),
            source,
        })?;
    let child_in = child
        .stdin
        .take()
        .ok_or(ServeError::MissingPipe { stream: "stdin" })?;
    let child_out = child
        .stdout
        .take()
        .ok_or(ServeError::MissingPipe { stream: "stdout" })?;

    let summary = proxy::run(
        config,
        tokio::io::stdin(),
        tokio::io::stdout(),
        child_out,
        child_in,
    )
    .await?;

    reap(child).await;
    Ok(summary)
}

/// Waits for the child to exit; kills it if it outlives the grace
/// period after its stdin closed.
async fn reap(mut child: Child) {
    match tokio::time::timeout(CHILD_EXIT_GRACE, child.wait()).await {
        Ok(Ok(status)) => info!(%status, "upstream exited"),
        Ok(Err(err)) => warn!(error = %err, "failed to wait for upstream"),
        Err(_) => {
            warn!("upstream did not exit after its stdin closed; killing it");
            if let Err(err) = child.kill().await {
                warn!(error = %err, "failed to kill upstream");
            }
        }
    }
}
