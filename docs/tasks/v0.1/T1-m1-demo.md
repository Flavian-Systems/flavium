# T1/M1 — manual demo checklist

M1's acceptance demo: **Claude Desktop drives a real MCP server through
flavium unmodified.** Run it after any change to the proxy path, and at
M5 again as part of the full acceptance run (with Claude Code as the
second real client).

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
- [x] A chat request that reads a file (e.g. "list what's on my
      Desktop") round-trips through flavium and succeeds.
- [x] Claude Desktop's MCP log for the server
      (`%APPDATA%\Claude\logs\mcp-server-filesystem.log` on Windows,
      `~/Library/Logs/Claude/` on macOS) contains flavium's
      `observed initialize request` and `MCP handshake complete` lines —
      the latter records the **negotiated protocol version**.
- [x] Quitting Claude Desktop leaves no orphan `flavium` or server
      processes behind.
- [x] Repeat the connect + tool-call steps with Claude Code as a second
      real client: `claude mcp add filesystem -- <same command line>`.

## Negotiated protocol version — record here

The scripted tests pin the protocol version they exercise. If the value
observed live differs from the pin, update the `PINNED_PROTOCOL_VERSION`
constants (in `crates/flavium-proxy-mcp/tests/scripted_session.rs` and
`crates/flavium-cli/tests/proxy_e2e.rs`) and the `scripted_upstream`
example together, in the same PR as this file.

| Date | Client | Client version | Negotiated protocol version |
|---|---|---|---|
| 2026-08-15 | Claude Desktop (clientInfo `claude-ai`) | 0.1.0 | **2025-11-25** |
| 2026-08-15 | Claude Code (clientInfo `claude-code`) | 2.1.173, 2.1.233 | **2025-11-25** |

Claude Desktop: two sessions observed, both clean (`client_end=Eof
upstream_end=Eof delivery_failed=false rejected=0 dropped=0`). Claude
Code: two sessions (its MCP logs live under
`%LOCALAPPDATA%\claude-cli-nodejs\Cache\<project>\mcp-logs-<server>\`),
both offering and negotiating 2025-11-25. Upstream in all sessions:
`secure-filesystem-server` 0.2.0 via `npx.cmd`.

Current pin: **2025-11-25**.
