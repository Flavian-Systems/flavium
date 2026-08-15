//! The `flavium` binary. One subcommand so far: `proxy`, the T1/M1
//! transparent MCP stdio proxy.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use flavium_proxy_mcp::proxy::ProxyConfig;
use flavium_proxy_mcp::stdio;
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
    /// Run a transparent MCP stdio proxy in front of one upstream server.
    ///
    /// Everything after `--` is the upstream server's command line. The
    /// proxy relays the MCP session unmodified; MCP frames own stdout,
    /// logs go to stderr (level via RUST_LOG, default `info`).
    Proxy(ProxyCmd),
}

#[derive(Args)]
struct ProxyCmd {
    /// Upstream MCP server command line (after `--`).
    #[arg(last = true, required = true, num_args = 1.., value_name = "COMMAND")]
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

fn run_proxy(args: &ProxyCmd) -> ExitCode {
    init_logging();
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

    let result = runtime.block_on(stdio::serve(ProxyConfig::default(), &args.upstream));

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
            error!(error = %err, "proxy failed");
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
