//! MCP proxy: presents as an MCP server to clients and an MCP client to
//! upstream tool servers; every tools/call will be authorized, metered,
//! and recorded.
//!
//! Current milestone (T1/M2, see `docs/tasks/v0.1/T1-mcp-proxy-core.md`):
//! a **protocol-terminating** proxy for multiple upstreams. The proxy
//! answers the client's `initialize` itself, initializes each configured
//! upstream separately (stdio child processes and streamable-HTTP
//! endpoints), merges their tool lists — pagination drained internally,
//! duplicate tool names rejected — and routes `tools/call` by tool name
//! with ids translated both ways. `params`/`result` bodies cross the
//! proxy byte-faithfully. No enforcement yet: grants and tracing land in
//! later T1 milestones.

#![forbid(unsafe_code)]

pub mod builder;
pub mod config;
pub mod connection;
pub mod envelope;
pub mod framing;
pub mod http;
pub mod idmap;
pub mod protocol;
pub mod router;
pub mod splice;
pub mod sse;
pub mod stdio;
pub mod toolset;
pub mod transport;
