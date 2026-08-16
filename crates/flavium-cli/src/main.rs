//! The `flavium` binary. One subcommand so far: `proxy`, the T1 MCP
//! proxy — multiple upstreams behind one stdio server face, with every
//! `tools/call` authorized against a grant file before it is forwarded.
//! The operator-facing reference (flags, config file, exit codes, startup
//! errors, the trace file, client wiring) is `docs/cli.md`.
//!
//! This is the only crate that knows Cedar exists: it compiles the grant
//! file into a [`CedarAuthorizer`] and hands the proxy a
//! [`flavium_core::Authorizer`]. The proxy, and every proxy test, sees
//! only the trait.

#![forbid(unsafe_code)]

mod grants;
mod trace;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use flavium_core::{Authorizer, NullSink, TraceSink};
use flavium_policy::CedarAuthorizer;
use flavium_proxy_mcp::config::UpstreamSpec;
use flavium_proxy_mcp::enforcement::{Enforcement, SystemClock};
use flavium_proxy_mcp::router::ProxyConfig;
use flavium_proxy_mcp::stdio;
use tracing::{error, info, warn};

use crate::grants::GrantConfig;
use crate::trace::JsonlSink;

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
    /// Upstreams and grants come from one TOML config file (--config):
    /// `version`, `principal`, one `[[upstream]]` table per server (stdio
    /// `command` or streamable-HTTP `url` + optional `headers`), and one
    /// `[[grant]]` table per authority granted. The proxy answers
    /// initialize itself, shows the client only granted tools, and
    /// authorizes every call before forwarding it; MCP frames own stdout,
    /// logs go to stderr (level via RUST_LOG, default `info`).
    ///
    /// A config with no grants refuses to start. The transparent
    /// middlebox — every tool exposed, every call forwarded — is
    /// available behind --unenforced, which is also the only way to use
    /// the `-- <COMMAND>` shorthand.
    Proxy(ProxyCmd),
}

#[derive(Args)]
struct ProxyCmd {
    /// Path to the config file (TOML): upstreams and grants.
    #[arg(long, value_name = "FILE", conflicts_with = "upstream")]
    config: Option<PathBuf>,

    /// Run with no enforcement: expose every upstream tool and forward
    /// every call. Logs a warning every session and writes no trace.
    #[arg(long, conflicts_with = "trace")]
    unenforced: bool,

    /// Append a JSONL trace of the session to this file (created 0600).
    #[arg(long, value_name = "FILE")]
    trace: Option<PathBuf>,

    /// Single upstream MCP server command line (after `--`). Requires
    /// --unenforced: the shorthand cannot carry grants.
    #[arg(last = true, num_args = 1.., value_name = "COMMAND")]
    upstream: Vec<String>,
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

/// What the command line and the config file resolve to.
#[derive(Debug)]
struct Resolved {
    upstreams: Vec<UpstreamSpec>,
    /// `None` is `--unenforced`.
    grants: Option<GrantConfig>,
}

/// Resolves upstreams and grants, refusing every combination that would
/// leave an operator believing they are protected when they are not.
///
/// Three postures are possible for a config without grants — enforce,
/// refuse, or forward — and only *refuse* makes the absence impossible to
/// overlook. A warning would not do: warnings are not read, and this one
/// would be the only thing between an agent and every tool it can see.
fn resolve(args: &ProxyCmd) -> Result<Resolved, String> {
    match (&args.config, args.upstream.is_empty()) {
        (Some(path), true) => {
            let config = grants::load(path)?;
            for warning in &config.warnings {
                warn!("{}: {warning}", path.display());
            }
            match (config.grants, args.unenforced) {
                (Some(_), true) => Err(format!(
                    "{}: --unenforced was given but the file declares grants; \
                     remove --unenforced to enforce them",
                    path.display()
                )),
                (None, false) => Err(format!(
                    "{}: no [[grant]] entries, so nothing would be enforced. \
                     Add grants (see docs/cli.md §3), or pass --unenforced to run \
                     the transparent proxy on purpose.",
                    path.display()
                )),
                (grants, _) => Ok(Resolved {
                    upstreams: config.upstreams,
                    grants,
                }),
            }
        }
        (None, false) => {
            if !args.unenforced {
                return Err("the `-- <COMMAND>` shorthand cannot carry grants; \
                            pass --unenforced to run it as a transparent proxy, \
                            or use --config with a grant file"
                    .into());
            }
            Ok(Resolved {
                upstreams: vec![UpstreamSpec {
                    name: "upstream".to_owned(),
                    transport: flavium_proxy_mcp::config::TransportSpec::Stdio {
                        command: args.upstream.clone(),
                    },
                }],
                grants: None,
            })
        }
        (None, true) => Err("either --config or an upstream command after `--` is required".into()),
        // conflicts_with makes this unreachable; refuse anyway.
        (Some(_), false) => Err("--config and an upstream command are mutually exclusive".into()),
    }
}

/// Builds the enforcement bundle: Cedar behind the trait, a sink, and the
/// host clock.
///
/// The compile happens here, at startup, so a grant that cannot be
/// compiled stops the process while an operator is watching rather than
/// surfacing mid-session as a denial that looks like policy.
fn enforcement(grants: GrantConfig, trace: Option<&PathBuf>) -> Result<Enforcement, String> {
    let authorizer = CedarAuthorizer::new(grants.envelope.clone())
        .map_err(|err| format!("cannot compile grants: {err}"))?;
    let sink: Arc<dyn TraceSink> = match trace {
        None => Arc::new(NullSink),
        Some(path) => {
            let sink = JsonlSink::create(path)
                .map_err(|err| format!("cannot open trace file {}: {err}", path.display()))?;
            info!(path = %path.display(), session = sink.session_id(), "recording the session trace");
            Arc::new(sink)
        }
    };
    Ok(Enforcement {
        envelope: grants.envelope,
        authorizer: Arc::new(authorizer) as Arc<dyn Authorizer>,
        sink,
        clock: Arc::new(SystemClock),
        path_flavors: grants.path_flavors,
    })
}

fn run_proxy(args: &ProxyCmd) -> ExitCode {
    init_logging();
    let resolved = match resolve(args) {
        Ok(resolved) => resolved,
        Err(message) => {
            error!("{message}");
            return ExitCode::FAILURE;
        }
    };

    let enforcement = match resolved.grants {
        None => {
            warn!(
                "running UNENFORCED: every upstream tool is exposed and every call is \
                 forwarded, and nothing is recorded"
            );
            None
        }
        Some(grants) => {
            info!(
                principal = %grants.envelope.principal,
                grants = grants.envelope.grants.len(),
                "enforcing grants"
            );
            match enforcement(grants, args.trace.as_ref()) {
                Ok(enforcement) => Some(enforcement),
                Err(message) => {
                    error!("{message}");
                    return ExitCode::FAILURE;
                }
            }
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

    let result = runtime.block_on(stdio::serve(
        ProxyConfig::default(),
        &resolved.upstreams,
        enforcement,
    ));

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
    use flavium_proxy_mcp::config::TransportSpec;

    fn cmd(config: Option<&str>, upstream: &[&str], unenforced: bool) -> ProxyCmd {
        ProxyCmd {
            config: config.map(PathBuf::from),
            unenforced,
            trace: None,
            upstream: upstream.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn write_config(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("flavium-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.toml"));
        std::fs::write(&path, contents).unwrap();
        path
    }

    const UPSTREAM: &str = "[[upstream]]\nname = \"fs\"\ncommand = [\"srv\"]\n";

    #[test]
    fn the_shorthand_needs_unenforced_and_is_one_stdio_upstream() {
        let refused = resolve(&cmd(None, &["server", "--flag"], false)).unwrap_err();
        assert!(refused.contains("--unenforced"), "{refused}");

        let resolved = resolve(&cmd(None, &["server", "--flag"], true)).unwrap();
        assert!(resolved.grants.is_none());
        assert_eq!(resolved.upstreams.len(), 1);
        assert_eq!(resolved.upstreams[0].name, "upstream");
        assert!(matches!(
            &resolved.upstreams[0].transport,
            TransportSpec::Stdio { command } if command == &["server", "--flag"]
        ));
    }

    #[test]
    fn no_source_is_an_error() {
        assert!(resolve(&cmd(None, &[], false)).is_err());
        assert!(resolve(&cmd(None, &[], true)).is_err());
    }

    /// The failure mode worth engineering against is an operator who
    /// believes they are protected and is not — so a grant-less config
    /// stops the process instead of quietly forwarding everything.
    #[test]
    fn a_config_without_grants_refuses_to_start() {
        let path = write_config("nogrants", &format!("version = 1\n{UPSTREAM}"));
        let message = resolve(&cmd(path.to_str(), &[], false)).unwrap_err();
        assert!(message.contains("no [[grant]] entries"), "{message}");
        assert!(message.contains("--unenforced"), "{message}");

        // …and the same file is fine when the operator says so.
        let resolved = resolve(&cmd(path.to_str(), &[], true)).unwrap();
        assert!(resolved.grants.is_none());
    }

    /// The other half of the same mistake: grants written and then
    /// ignored.
    #[test]
    fn unenforced_with_grants_is_refused() {
        let path = write_config(
            "withgrants",
            &format!("version = 1\nprincipal = \"bot\"\n{UPSTREAM}[[grant]]\ntool = \"t\"\n"),
        );
        let message = resolve(&cmd(path.to_str(), &[], true)).unwrap_err();
        assert!(message.contains("declares grants"), "{message}");

        let resolved = resolve(&cmd(path.to_str(), &[], false)).unwrap();
        let grants = resolved.grants.unwrap();
        assert_eq!(grants.envelope.principal.as_str(), "bot");
        assert_eq!(grants.envelope.grants.len(), 1);
    }

    #[test]
    fn config_errors_are_reported_with_the_file_name() {
        let path = write_config("typo", "version = 1\ncomand_typo = 1\n");
        let message = resolve(&cmd(path.to_str(), &[], false)).unwrap_err();
        assert!(message.contains("comand_typo"), "{message}");
        assert!(resolve(&cmd(Some("no-such-file.toml"), &[], false))
            .unwrap_err()
            .contains("cannot read"));
    }

    /// The grant file compiles all the way to a live Cedar authorizer
    /// here, which is what makes a bad grant a startup error.
    #[test]
    fn grants_compile_into_an_enforcement_bundle() {
        let path = write_config(
            "compiles",
            &format!(
                "version = 1\nprincipal = \"bot\"\n{UPSTREAM}\
                 [[grant]]\ntool = \"read_file\"\n[grant.args]\n\
                 path = {{ path-prefix = \"/data/invoices/\" }}\n"
            ),
        );
        let resolved = resolve(&cmd(path.to_str(), &[], false)).unwrap();
        let bundle = enforcement(resolved.grants.unwrap(), None).unwrap();
        assert_eq!(bundle.principal().as_str(), "bot");
        assert!(bundle
            .path_flavors
            .for_tool("read_file")
            .unwrap()
            .contains_key("path"));

        // The engine behind the trait is the real one, and it answers
        // what the reference semantics say.
        use flavium_core::{ArgValue, Decision, DenialReason, Timestamp, ToolCall};
        let call = |path: &str| ToolCall {
            tool: "read_file".into(),
            args: std::collections::BTreeMap::from([(
                "path".to_owned(),
                ArgValue::Str(path.into()),
            )]),
        };
        let now = Timestamp::from_unix_secs(1_700_000_000);
        assert_eq!(
            bundle
                .authorizer
                .authorize(bundle.principal(), &call("/data/invoices/x.pdf"), now),
            Decision::Allow { grant: 0 }
        );
        assert_eq!(
            bundle
                .authorizer
                .authorize(bundle.principal(), &call("/etc/passwd"), now),
            Decision::Deny(DenialReason::OutOfEnvelope)
        );
    }

    #[test]
    fn a_trace_path_that_cannot_be_opened_fails_at_startup() {
        let path = write_config(
            "traced",
            &format!(
                "version = 1\nprincipal = \"bot\"\n{UPSTREAM}[[grant]]\ntool = \"read_file\"\n"
            ),
        );
        let resolved = resolve(&cmd(path.to_str(), &[], false)).unwrap();
        let dir = std::env::temp_dir().join(format!("flavium-trace-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let message = enforcement(resolved.grants.unwrap(), Some(&dir)).unwrap_err();
        assert!(message.contains("cannot open trace file"), "{message}");
    }
}
