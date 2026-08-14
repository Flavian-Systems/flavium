//! A tiny scripted MCP server, used by the end-to-end tests and for
//! poking at the proxy by hand:
//!
//! ```text
//! cargo run -p flavium --bin flavium -- proxy -- \
//!     target/debug/examples/scripted_upstream
//! ```
//!
//! It answers `initialize` (protocol version 2025-06-18), `tools/list`
//! (one `echo` tool), and `tools/call` (echoes the params back), and
//! returns method-not-found for any other request. It exits cleanly
//! when its stdin closes.

// Test fixture: panicking on malformed input is acceptable here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, Write};

use serde_json::{json, Value};

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = line.expect("failed to read stdin");
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = serde_json::from_str(&line).expect("fixture received invalid JSON");
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id");

        let reply = match (method, id) {
            (Some("initialize"), Some(id)) => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "scripted-upstream", "version": "0.0.0" }
                }
            })),
            (Some("tools/list"), Some(id)) => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": "echo",
                        "description": "Echoes its arguments back.",
                        "inputSchema": { "type": "object" }
                    }]
                }
            })),
            (Some("tools/call"), Some(id)) => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": message
                            .get("params")
                            .map(Value::to_string)
                            .unwrap_or_default()
                    }]
                }
            })),
            (Some(_), Some(id)) => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "Method not found" }
            })),
            // Notifications get no reply.
            _ => None,
        };

        if let Some(reply) = reply {
            serde_json::to_writer(&mut out, &reply).expect("failed to write reply");
            out.write_all(b"\n").expect("failed to write delimiter");
            out.flush().expect("failed to flush");
        }
    }
}
