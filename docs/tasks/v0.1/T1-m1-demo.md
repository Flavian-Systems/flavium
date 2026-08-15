# T1 — manual demo checklist (Claude Desktop)

The acceptance demo, first run at M1 and kept current as the proxy
evolves: **Claude Desktop drives a real MCP server through flavium
unmodified.** Run it after any change to the proxy path, and at M5
again as part of the full acceptance run (with Claude Code as the
second real client).

Since M2 the proxy **terminates MCP on both faces** (see
[docs/GLOSSARY.md](../../GLOSSARY.md)): flavium answers the client's
`initialize` itself and separately initializes each upstream. Two
visible consequences for this checklist: Claude Desktop sees
`serverInfo.name: "flavium"` (no longer the upstream's own name), and
there are *two* handshakes to verify in the log instead of one.

## Setup

1. Build: `cargo build --release`. The binary is
   `target/release/flavium(.exe)`.
2. Add the proxied server to Claude Desktop's `claude_desktop_config.json`
   (Settings → Developer → Edit Config), using absolute paths:

   **Windows** (note: `npx` must be invoked as `npx.cmd`):

   ```json
   {
     "mcpServers": {
       "filesystem": {
         "command": "D:\\flavi\\Projects\\Flavian-Systems\\flavium\\target\\release\\flavium.exe",
         "args": ["proxy", "--", "npx.cmd", "-y",
                  "@modelcontextprotocol/server-filesystem",
                  "C:\\Users\\flavi\\Desktop"]
       }
     }
   }
   ```

   **macOS/Linux**: same shape with `flavium` and `npx`.

3. Fully restart Claude Desktop (quit from the tray/menu bar, not just
   the window).

## Checklist

- [x] The `filesystem` server shows as connected; its tools are listed.
      (First connect may take a few seconds longer than M1: the proxy
      brings the upstream up and drains its `tools/list` *before*
      answering Claude Desktop's `initialize`.)
- [x] A chat request that reads a file (e.g. "list what's on my
      Desktop") round-trips through flavium and succeeds.
- [x] Claude Desktop's MCP log for the server
      (`%APPDATA%\Claude\logs\mcp-server-filesystem.log` on Windows,
      `~/Library/Logs/Claude/` on macOS) shows both handshakes, in this
      order:
      1. `upstream initialized` — the upstream-side handshake, with the
         upstream's own negotiated version and `server_name`;
      2. `all upstreams initialized; serving the client`;
      3. `answered client initialize` — the client-side handshake; its
         `negotiated_protocol_version` is the value to record below;
      4. `client session initialized`.
- [x] Quitting Claude Desktop leaves no orphan `flavium` or server
      processes behind, and the log's final `session ended` line reads
      `end=ClientEof delivery_failed=false rejected=0 discarded=0`.
- [x] Repeat the connect + tool-call steps with Claude Code as a second
      real client: `claude mcp add filesystem -- <same command line>`.
- [ ] *(M2+, optional)* Multi-upstream: point the entry at a config file
      instead — `"args": ["proxy", "--config", "<path>\\flavium.toml"]`
      with two `[[upstream]]` entries — and check that both servers'
      tools appear in one merged list and a call to each routes to the
      right server.

## Negotiated protocol version — record here

Since M2 there are two negotiations. The **client-face** version is
chosen by flavium (it echoes a supported offer, otherwise answers with
the newest revision it speaks); each upstream negotiates its own
separately. Record the client-face value from the
`answered client initialize` log line.

The scripted tests pin this version. If the value observed live
differs from the pin, update the `PINNED_PROTOCOL_VERSION` constants
(in `crates/flavium-proxy-mcp/tests/router_session.rs` and
`crates/flavium-cli/tests/proxy_e2e.rs`), the `scripted_upstream`
example, and — if a new revision must be *accepted* — the supported
set in `crates/flavium-proxy-mcp/src/protocol.rs`, together, in the
same PR as this file.

| Date | Build | Client | Client version | Negotiated protocol version |
|---|---|---|---|---|
| 2026-08-15 | M1 | Claude Desktop (clientInfo `claude-ai`) | 0.1.0 | **2025-11-25** |
| 2026-08-15 | M1 | Claude Code (clientInfo `claude-code`) | 2.1.173, 2.1.233 | **2025-11-25** |
| 2026-08-15 | M2 | Claude Desktop (clientInfo `claude-ai`) | 0.1.0 | **2025-11-25** |
| 2026-08-15 | M2 | Claude Code (clientInfo `claude-code`) | 2.1.233 | **2025-11-25** |

M1 run: two Claude Desktop sessions and two Claude Code sessions, all
clean, all offering and negotiating 2025-11-25.

M2 run (the checklist as ticked above): two Claude Desktop sessions
and two Claude Code sessions. Every session showed the four handshake
lines in order; the upstream negotiated 2025-11-25 on its own face
(`server_name="secure-filesystem-server" server_version="0.2.0"
tools_declared=true`), 14 tools were merged, the client face offered
and negotiated 2025-11-25, and both Claude Desktop sessions ended
`end=ClientEof delivery_failed=false rejected=0 discarded=0` with no
orphan processes. (Claude Code's MCP logs live under
`%LOCALAPPDATA%\claude-cli-nodejs\Cache\<project>\mcp-logs-<server>\`.)

Upstream in all sessions to date: `secure-filesystem-server` 0.2.0
via `npx.cmd`. The optional multi-upstream item has not been run
against a real client yet (it is covered by the e2e tests).

Current pin: **2025-11-25**.
