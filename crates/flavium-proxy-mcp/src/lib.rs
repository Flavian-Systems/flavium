//! MCP proxy: presents as an MCP server to clients and an MCP client to
//! upstream tool servers; every tools/call will be authorized, metered,
//! and recorded.
//!
//! Current milestone (T1/M5, see `docs/tasks/v0.1/T1-mcp-proxy-core.md`):
//! a **protocol-terminating, enforcing** proxy for multiple upstreams. The
//! proxy answers the client's `initialize` itself, initializes each
//! configured upstream separately (stdio child processes and
//! streamable-HTTP endpoints), merges their tool lists — pagination
//! drained internally, duplicate tool names rejected — and routes
//! `tools/call` by tool name with ids translated both ways.
//! `params`/`result` bodies cross the proxy byte-faithfully.
//!
//! Since M5 every `tools/call` is authorized before it is forwarded and
//! every `tools/list` shows only granted tools ([`enforcement`],
//! [`router`]). The proxy does not know which engine answers: it holds a
//! [`flavium_core::Authorizer`] and a [`flavium_core::TraceSink`], and the
//! CLI decides that Cedar is behind them. Tool-path budgets are T2a and the
//! model boundary T2b (its own face, not this crate's), delegation and
//! supervision T3, the hash-chained recorder and replay T4.
//!
//! How the crate is built — module map, task and channel model, message
//! flows, invariants — is documented in `docs/architecture/proxy-mcp.md`.

#![forbid(unsafe_code)]

pub mod args;
pub mod builder;
pub mod config;
pub mod connection;
pub mod enforcement;
pub mod envelope;
pub mod framing;
pub mod http;
pub mod idmap;
pub mod normalize;
pub mod protocol;
pub mod router;
pub mod splice;
pub mod sse;
pub mod stdio;
pub mod toolset;
pub mod transport;
