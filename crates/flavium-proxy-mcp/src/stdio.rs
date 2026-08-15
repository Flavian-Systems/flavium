//! Production wiring: this process's stdin/stdout as the client side,
//! configured upstreams (spawned children and HTTP endpoints) behind
//! the router.
//!
//! stdout carries only MCP frames; all logging goes to stderr, and
//! spawned children inherit stderr so their logs surface alongside the
//! proxy's.

use tracing::info;

use crate::config::{self, TransportSpec, UpstreamSpec};
use crate::http::{HttpSetupError, HttpTransport};
use crate::router::{self, PreparedUpstream, ProxyConfig, RunError, SessionSummary};
use crate::transport::{SpawnError, StdioTransport, Transport};

/// Errors starting or finishing a stdio-served session.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The upstream set is structurally invalid.
    #[error(transparent)]
    Config(#[from] config::ConfigError),

    /// A stdio upstream could not be spawned.
    #[error("upstream {name:?} could not be started")]
    Spawn {
        /// The upstream that failed.
        name: String,
        /// Why.
        #[source]
        source: SpawnError,
    },

    /// An HTTP upstream's transport could not be built.
    #[error("upstream {name:?} has an unusable HTTP configuration")]
    Http {
        /// The upstream that failed.
        name: String,
        /// Why.
        #[source]
        source: HttpSetupError,
    },

    /// The session failed to start or an internal task died.
    #[error(transparent)]
    Run(#[from] RunError),
}

/// Serves one MCP session over this process's stdin/stdout, fronting
/// every configured upstream. Returns when the session ends; spawned
/// children are reaped and HTTP sessions terminated on the way out.
pub async fn serve(
    config: ProxyConfig,
    specs: &[UpstreamSpec],
) -> Result<SessionSummary, ServeError> {
    config::validate(specs)?;
    info!(
        upstreams = specs.len(),
        "starting flavium MCP proxy (multi-upstream)"
    );

    let mut prepared = Vec::with_capacity(specs.len());
    for spec in specs {
        let transport = match &spec.transport {
            TransportSpec::Stdio { command } => {
                info!(upstream = %spec.name, command = %command.join(" "), "spawning stdio upstream");
                match StdioTransport::spawn(command, config.max_frame_bytes) {
                    Ok(t) => Transport::stdio(t),
                    Err(source) => {
                        close_all(prepared).await;
                        return Err(ServeError::Spawn {
                            name: spec.name.clone(),
                            source,
                        });
                    }
                }
            }
            TransportSpec::Http { url, headers } => {
                info!(upstream = %spec.name, url = %url, "connecting streamable-HTTP upstream");
                match HttpTransport::new(&spec.name, url, headers, config.max_frame_bytes) {
                    Ok(t) => Transport::http(t),
                    Err(source) => {
                        close_all(prepared).await;
                        return Err(ServeError::Http {
                            name: spec.name.clone(),
                            source,
                        });
                    }
                }
            }
        };
        prepared.push(PreparedUpstream {
            name: spec.name.clone(),
            transport,
        });
    }

    let summary = router::run(config, prepared, tokio::io::stdin(), tokio::io::stdout()).await?;
    Ok(summary)
}

/// Closes transports already built when a later one fails to build, so
/// no spawned child outlives a startup error.
async fn close_all(prepared: Vec<PreparedUpstream>) {
    for mut upstream in prepared {
        upstream.transport.close().await;
    }
}
