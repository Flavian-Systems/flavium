//! A tiny scripted MCP server, used by the end-to-end tests and for
//! poking at the proxy by hand:
//!
//! ```text
//! cargo run -p flavium --bin flavium -- proxy --unenforced -- \
//!     target/debug/examples/scripted_upstream
//! ```
//!
//! (`--unenforced` because the `-- <COMMAND>` shorthand has nowhere to
//! put grants; since M5 it is refused without the flag.)
//!
//! It answers `initialize` (protocol version 2025-11-25), `tools/list`
//! (one tool, named by the first argument, default `echo`), and
//! `tools/call` (echoes the params back), and returns method-not-found
//! for any other request. It exits cleanly when its stdin closes.
//!
//! With a tool name argument, several instances can sit behind one
//! proxy without colliding — or collide on purpose, when a test wants
//! the collision rejected.
//!
//! A **second** argument becomes the server's `instructions` string, the
//! free-text field real servers use to tell an agent how to drive them —
//! and routinely fill with their own tool names. Omitted, no field is
//! sent at all. It exists so a test can prove an enforced handshake
//! withholds it.

// Test fixture: panicking on malformed input is acceptable here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, Write};

use serde_json::{json, Value};

fn main() {
    let tool = std::env::args().nth(1).unwrap_or_else(|| "echo".to_owned());
    let instructions = std::env::args().nth(2);
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
            (Some("initialize"), Some(id)) => {
                let mut result = json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": { "tools": { "listChanged": true } },
                    "serverInfo": { "name": format!("scripted-upstream-{tool}"), "version": "0.0.0" }
                });
                if let Some(text) = &instructions {
                    result["instructions"] = Value::String(text.clone());
                }
                Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            }
            (Some("tools/list"), Some(id)) => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": tool,
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
