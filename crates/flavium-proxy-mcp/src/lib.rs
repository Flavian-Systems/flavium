//! MCP proxy: presents as an MCP server to clients and an MCP client to
//! upstream tool servers; every tools/call will be authorized, metered,
//! and recorded.
//!
//! Current milestone (T1/M1, see `docs/tasks/v0.1/T1-mcp-proxy-core.md`):
//! a **transparent** stdio↔stdio proxy for a single upstream server —
//! byte-faithful forwarding behind a typed parse boundary, observation
//! of the `initialize` handshake, clean shutdown of the spawned child.
//! No enforcement yet: grants, tracing, and multi-upstream routing land
//! in later T1 milestones.

#![forbid(unsafe_code)]

pub mod envelope;
pub mod framing;
pub mod proxy;
pub mod stdio;
