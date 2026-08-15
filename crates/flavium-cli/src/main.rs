//! The `flavium` binary. One subcommand so far: `proxy`, the T1 MCP
//! proxy — multiple upstreams behind one stdio server face. The
//! operator-facing reference (flags, config file, exit codes, startup
//! errors, client wiring) is `docs/cli.md`.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use flavium_proxy_mcp::config::{TransportSpec, UpstreamSpec};
use flavium_proxy_mcp::router::ProxyConfig;
use flavium_proxy_mcp::stdio;
use serde::Deserialize;
use tracing::{error, info};

/// The capability runtime for AI agents.
#[derive(Parser)]
#[command(name = "flavium", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the MCP proxy in front of one or more upstream servers.
    ///
    /// Upstreams come from a TOML config file (--config) with one
    /// `[[upstream]]` table per server (stdio `command` or streamable-HTTP
    /// `url` + optional `headers`), or — for a single stdio upstream —
    /// from the command line after `--`. The proxy answers initialize
    /// itself, merges the upstreams' tools, and routes calls by tool
    /// name; MCP frames own stdout, logs go to stderr (level via
    /// RUST_LOG, default `info`).
    Proxy(ProxyCmd),
}

#[derive(Args)]
struct ProxyCmd {
    /// Path to the upstream config file (TOML).
    #[arg(long, value_name = "FILE", conflicts_with = "upstream")]
    config: Option<PathBuf>,

    /// Single upstream MCP server command line (after `--`).
    #[arg(last = true, num_args = 1.., value_name = "COMMAND")]
    upstream: Vec<String>,
}

/// The config file: a list of upstreams.
///
/// ```toml
/// [[upstream]]
/// name = "fs"
/// command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/data"]
///
/// [[upstream]]
/// name = "search"
/// url = "https://example.com/mcp"
/// headers = { Authorization = "Bearer …" }
/// ```
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    upstream: Vec<UpstreamEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpstreamEntry {
    name: String,
    #[serde(default)]
    command: Option<Vec<String>>,
    #[serde(default)]
    url: Option<String>,
    /// Extra headers for an HTTP upstream. Values are secrets; they are
    /// never logged.
    #[serde(default)]
    headers: Option<std::collections::BTreeMap<String, String>>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        None => {
            banner();
            ExitCode::SUCCESS
        }
        Some(Cmd::Proxy(args)) => run_proxy(&args),
    }
}

fn banner() {
    println!("flavium {} — pre-v0.1", env!("CARGO_PKG_VERSION"));
    println!("The capability runtime for AI agents. https://flavium.ai");
}

/// Resolves the upstream set from --config or the legacy `-- command`
/// form (one stdio upstream named "upstream").
fn resolve_upstreams(args: &ProxyCmd) -> Result<Vec<UpstreamSpec>, String> {
    match (&args.config, args.upstream.is_empty()) {
        (Some(path), true) => {
            let text = std::fs::read_to_string(path)
                .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
            let parsed: ConfigFile = toml::from_str(&text)
                .map_err(|err| format!("cannot parse {}: {err}", path.display()))?;
            if parsed.upstream.is_empty() {
                return Err(format!("{}: no [[upstream]] entries", path.display()));
            }
            parsed
                .upstream
                .into_iter()
                .map(|entry| {
                    let name = entry.name;
                    let transport = match (entry.command, entry.url) {
                        (Some(command), None) => {
                            if entry.headers.is_some() {
                                return Err(format!(
                                    "upstream {name:?}: `headers` only applies to `url` upstreams"
                                ));
                            }
                            TransportSpec::Stdio { command }
                        }
                        (None, Some(url)) => TransportSpec::Http {
                            url,
                            headers: entry.headers.unwrap_or_default().into_iter().collect(),
                        },
                        _ => {
                            return Err(format!(
                                "upstream {name:?}: exactly one of `command` or `url` is required"
                            ))
                        }
                    };
                    Ok(UpstreamSpec { name, transport })
                })
                .collect()
        }
        (None, false) => Ok(vec![UpstreamSpec {
            name: "upstream".to_owned(),
            transport: TransportSpec::Stdio {
                command: args.upstream.clone(),
            },
        }]),
        (None, true) => Err("either --config or an upstream command after `--` is required".into()),
        // conflicts_with makes this unreachable; refuse anyway.
        (Some(_), false) => Err("--config and an upstream command are mutually exclusive".into()),
    }
}

fn run_proxy(args: &ProxyCmd) -> ExitCode {
    init_logging();
    let specs = match resolve_upstreams(args) {
        Ok(specs) => specs,
        Err(message) => {
            error!("{message}");
            return ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            error!(error = %err, "failed to start async runtime");
            return ExitCode::FAILURE;
        }
    };

    let result = runtime.block_on(stdio::serve(ProxyConfig::default(), &specs));

    // Don't let a lingering blocked stdin read hold the process open.
    runtime.shutdown_background();

    match result {
        Ok(summary) if summary.clean_shutdown() => {
            info!("session closed cleanly");
            ExitCode::SUCCESS
        }
        Ok(summary) => {
            error!(?summary, "session ended abnormally");
            ExitCode::FAILURE
        }
        Err(err) => {
            // The full error chain, since these are startup problems
            // the operator must act on.
            let mut chain = err.to_string();
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                chain.push_str(": ");
                chain.push_str(&cause.to_string());
                source = cause.source();
            }
            error!("proxy failed: {chain}");
            ExitCode::FAILURE
        }
    }
}

/// Logs go to stderr only — stdout belongs to the MCP session.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn cmd(config: Option<&str>, upstream: &[&str]) -> ProxyCmd {
        ProxyCmd {
            config: config.map(PathBuf::from),
            upstream: upstream.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn legacy_form_is_one_stdio_upstream() {
        let specs = resolve_upstreams(&cmd(None, &["server", "--flag"])).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "upstream");
        assert!(matches!(
            &specs[0].transport,
            TransportSpec::Stdio { command } if command == &["server", "--flag"]
        ));
    }

    #[test]
    fn no_source_is_an_error() {
        assert!(resolve_upstreams(&cmd(None, &[])).is_err());
    }

    #[test]
    fn config_file_parses_both_transport_kinds() {
        let dir = std::env::temp_dir().join(format!("flavium-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ok.toml");
        std::fs::write(
            &path,
            r#"
[[upstream]]
name = "fs"
command = ["npx", "-y", "server-filesystem", "/data"]

[[upstream]]
name = "web"
url = "https://example.com/mcp"
headers = { Authorization = "Bearer token" }
"#,
        )
        .unwrap();
        let specs = resolve_upstreams(&cmd(path.to_str(), &[])).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "fs");
        assert!(matches!(&specs[0].transport, TransportSpec::Stdio { .. }));
        match &specs[1].transport {
            TransportSpec::Http { url, headers } => {
                assert_eq!(url, "https://example.com/mcp");
                assert_eq!(headers[0].0, "Authorization");
            }
            other => panic!("expected http, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_file_rejects_ambiguous_and_unknown_entries() {
        let dir = std::env::temp_dir().join(format!("flavium-cli-test-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let both = dir.join("both.toml");
        std::fs::write(
            &both,
            "[[upstream]]\nname = \"x\"\ncommand = [\"a\"]\nurl = \"https://e.com/\"\n",
        )
        .unwrap();
        assert!(resolve_upstreams(&cmd(both.to_str(), &[])).is_err());

        let neither = dir.join("neither.toml");
        std::fs::write(&neither, "[[upstream]]\nname = \"x\"\n").unwrap();
        assert!(resolve_upstreams(&cmd(neither.to_str(), &[])).is_err());

        let typo = dir.join("typo.toml");
        std::fs::write(
            &typo,
            "[[upstream]]\nname = \"x\"\ncommand = [\"a\"]\ncomand_typo = 1\n",
        )
        .unwrap();
        assert!(resolve_upstreams(&cmd(typo.to_str(), &[])).is_err());

        let headers_on_stdio = dir.join("hs.toml");
        std::fs::write(
            &headers_on_stdio,
            "[[upstream]]\nname = \"x\"\ncommand = [\"a\"]\nheaders = { A = \"b\" }\n",
        )
        .unwrap();
        assert!(resolve_upstreams(&cmd(headers_on_stdio.to_str(), &[])).is_err());

        let empty = dir.join("empty.toml");
        std::fs::write(&empty, "").unwrap();
        assert!(resolve_upstreams(&cmd(empty.to_str(), &[])).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
