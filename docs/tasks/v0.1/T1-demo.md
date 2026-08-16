# T1 — manual demo checklist

The acceptance demo, first run at M1, refreshed at M2, and kept current
as the proxy evolves: **a real MCP client drives a real MCP server
through flavium unmodified.** Claude Desktop is the primary client;
Claude Code is the second one, and both have run this checklist clean
since M2. Run it after any change to the proxy path. The **M5 variant**
below adds the other half of T1's acceptance — a grant file denying
out-of-envelope calls, and the trace that records them.

This file is deliberately not named after a milestone: it is one living
checklist for all of T1, and the runs recorded at the bottom say which
milestone each belongs to.

Since M2 the proxy **terminates MCP on both faces** (see
[docs/GLOSSARY.md](../../GLOSSARY.md)): flavium answers the client's
`initialize` itself and separately initializes each upstream. Two
consequences for this checklist: the `initialize` response now carries
`serverInfo.name: "flavium"` instead of the upstream's own name (on the
wire only — Claude Desktop's UI shows the config entry name regardless;
see *Checking the wire directly* below), and there are *two* handshakes
to verify in the log instead of one.

**Since M5 every command in this file carries `--unenforced`.** The
checklist rows below M1/M2 are the *transparent* proxy, and M5 made that
posture something an operator asks for by name: the `-- <COMMAND>`
shorthand has nowhere to put grants and now refuses without the flag
(`the -- <COMMAND> shorthand cannot carry grants`), and a config file
without `version` or without `[[grant]]` entries refuses too. The
commands were updated in place — the *recorded runs* at the bottom
predate the flag and are left as they were run.

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
         "args": ["proxy", "--unenforced", "--", "npx.cmd", "-y",
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
- [x] *(M2+)* Multi-upstream: point the entry at a config file instead
      and check that both servers' tools appear in one merged list, that
      a call to each routes to the right server, and that a deliberate
      tool-name collision is refused. Worked example below.
- [ ] *(M5+)* Enforcement: the same session under a grant file — the
      tool list is filtered, an in-envelope call succeeds, an
      out-of-envelope one is denied, and both are in the trace. Worked
      example below.

The M1/M2 rows of this checklist are the unenforced proxy; the M5
section below is the enforced one, and it is the only part still
outstanding.

## Checking the wire directly

Claude Desktop never displays `serverInfo` — its server list, tool
picker, and its own log lines all use the entry name from
`claude_desktop_config.json`. To see what the proxy actually answers,
send one `initialize` frame by hand and read the first stdout line
(no client needed; the upstream is spawned and reaped as usual):

**cmd.exe** (works as is):

```bat
echo {"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"manual","version":"0"}}} | target\release\flavium.exe proxy --unenforced -- npx.cmd -y @modelcontextprotocol/server-filesystem C:\Users\flavi\Desktop 2>nul
```

**Windows PowerShell 5.1** — set the pipe encoding first, or the frame
reaches the proxy re-encoded (BOM/OEM codepage) and is answered with a
`-32700 Parse error`, which is the proxy correctly refusing a mangled
frame, not a proxy bug:

```powershell
$OutputEncoding = New-Object Text.UTF8Encoding $false
'{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"manual","version":"0"}}}' | .\target\release\flavium.exe proxy --unenforced -- npx.cmd -y @modelcontextprotocol/server-filesystem C:\Users\flavi\Desktop 2>$null | Select-Object -First 1
```

(macOS/Linux: the same `echo … | flavium proxy --unenforced -- npx …`
works in any shell.)

Expected first line, re-observed 2026-08-16 against the M5 build:

```json
{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"flavium","title":"Flavium MCP proxy","version":"0.1.0-alpha.0"}}}
```

(The M2 build emitted the same three members with `capabilities` first;
member order in a JSON object carries no meaning, and nothing asserts it
— compare the values, not the bytes.)

On the M1 build the same command returned the upstream's own
`serverInfo` (`secure-filesystem-server` 0.2.0) — that difference is
the protocol termination made visible. The scripted tests pin the M2
shape (`router_session.rs`, `proxy_e2e.rs`); MCP Inspector
(`npx @modelcontextprotocol/inspector`) shows the same field in its
Server Info panel if a GUI is preferred.

## Multi-upstream variant

Config file (e.g. `flavium.toml` at the repo root; TOML needs doubled
backslashes). `server-memory` is a good second upstream: zero-config
via `npx`, a completely different tool family, no name overlap with
the filesystem server.

```toml
version = 1

[[upstream]]
name = "fs"
command = ["npx.cmd", "-y", "@modelcontextprotocol/server-filesystem",
           "C:\\Users\\flavi\\Desktop"]

[[upstream]]
name = "memory"
command = ["npx.cmd", "-y", "@modelcontextprotocol/server-memory"]
```

Claude Desktop entry (name it `flavium` — the log file follows the
entry name, so it lands in `mcp-server-flavium.log`). This variant is
about *routing*, so it declares no grants and therefore needs
`--unenforced`; the grant-bearing version is the M5 variant below:

```json
{
  "mcpServers": {
    "flavium": {
      "command": "D:\\flavi\\Projects\\Flavian-Systems\\flavium\\target\\release\\flavium.exe",
      "args": ["proxy", "--unenforced", "--config",
               "D:\\flavi\\Projects\\Flavian-Systems\\flavium\\flavium.toml"]
    }
  }
}
```

**Success case.** The server connects with one merged list — the 14
filesystem tools *and* the 9 memory tools (23 total). "List what's on
my Desktop" routes to `fs`; "remember that my favorite editor is
Neovim, then read back the whole knowledge graph" routes
`create_entities` + `read_graph` to `memory`. The log shows two
`upstream initialized` lines (`upstream="fs"` / `upstream="memory"`),
`all upstreams initialized; serving the client upstreams=2 tools=23`,
the usual client handshake, and a clean `session ended`. Quitting
leaves none of the three processes behind.

**Failure case (collision refusal).** Add a third block that duplicates
`fs` under another name (`name = "fs2"`, same command) and restart.
The server must show as failed/disconnected in Claude Desktop, and the
log must contain
`proxy failed: tool "read_file" is offered by both "fs" and "fs2"` —
the proxy refuses ambiguous authority at startup rather than picking a
winner. Remove the block afterwards.

## M5 variant — the enforcement run

This is the run T1's acceptance criteria are written against: *a real
client works unmodified through the proxy*, *a grant file denies
out-of-envelope calls*, and *denials are logged*. The first is the
checklist above; the other two are here.

Config file (`flavium.toml` at the repo root — gitignored on purpose;
this file's contents are the only ones that ever appear in the repo).
Note the single-quoted TOML literal string for the Windows path: it
avoids doubling every backslash, which matters most in a grant.

```toml
version = 1
principal = "desktop-bot"

[[upstream]]
name = "fs"
command = ["npx.cmd", "-y", "@modelcontextprotocol/server-filesystem",
           "C:\\Users\\flavi\\Desktop"]

[[grant]]
tool = "list_allowed_directories"

[[grant]]
tool = "read_text_file"
[grant.args]
path = { windows-path-prefix = 'C:\Users\flavi\Desktop\flavium-demo\' }
```

Claude Desktop entry — same as the multi-upstream one, plus a trace:

```json
"args": ["proxy",
         "--config", "D:\\flavi\\Projects\\Flavian-Systems\\flavium\\flavium.toml",
         "--trace",  "D:\\flavi\\Projects\\Flavian-Systems\\flavium\\flavium-trace.jsonl"]
```

Prepare `C:\Users\flavi\Desktop\flavium-demo\ok.txt` with any content,
and leave at least one other file directly on the Desktop.

- [ ] **Startup.** The log shows `enforcing grants principal=desktop-bot
      grants=2` before the upstreams come up, and
      `all upstreams initialized … enforced=true`.
- [ ] **The list is filtered.** Claude Desktop shows **2** tools for
      this server, not the filesystem server's full set — no
      `write_file`, no `edit_file`, no `move_file`. (The upstream still
      offers them; the client is not shown them.)
- [ ] **In-envelope call succeeds.** "Read
      `C:\Users\flavi\Desktop\flavium-demo\ok.txt`" returns the file's
      contents.
- [ ] **Out-of-envelope call is denied.** "Read the other file on my
      Desktop" (any path outside `flavium-demo\`) comes back as a tool
      error reading `denied by policy`; the model can see the denial and
      usually says so. The log has a
      `WARN … call denied tool=read_text_file … reason=arguments outside
      the grant envelope` line.
- [ ] **Traversal is denied too** — the row the path flavor exists for.
      Ask for
      `C:\Users\flavi\Desktop\flavium-demo\..\<the other file>`; it must
      be denied, and the trace must record the *normalized* path
      (`C:/Users/flavi/Desktop/<the other file>`), not the one that was
      typed. This is the check that a byte-prefix comparison alone would
      fail.
- [ ] **An ungranted tool is invisible and unusable.** Asking to write a
      file gets "no such tool" from the model; if a call is made by hand
      it answers `-32602 Unknown tool: write_file` — the same bytes as a
      tool no upstream offers.
- [ ] **The trace.** `flavium-trace.jsonl` has one JSON object per line,
      `seq` dense from 1, beginning `session_started` (with the envelope)
      and ending `session_ended`, with a `call_decided` for every call
      above and a `call_completed` for each allowed one. On unix the file
      is `0600`; on Windows it inherits the directory's ACL (noted as a
      known gap — see below).

Two things worth eyeballing in the trace, because they are what the
record exists for: the `args` of a denied call are the values the
decision was made on (normalized), and the allow/deny pair for the same
tool sits under one `principal` with distinct `call_id`s.

**What this run is the first evidence for.** `windows-path-prefix` was
verified against flavium's own normalizer, never against
`server-filesystem`'s path resolution. This is where the two meet. If a
path that flavium normalizes one way resolves another way inside the
upstream, that is a false allow or a false denial and it shows up here
first — the flavor map is one module (`normalize.rs`) if it must change.

**Known gap, Windows.** The trace file is created `0600` on unix only;
on Windows it inherits the parent directory's ACL. Put it somewhere
already protected until the packaging work in T5.

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

Single-upstream sessions to date: `secure-filesystem-server` 0.2.0 via
`npx.cmd`.

Multi-upstream variant, run 2026-08-15 on Claude Desktop against the
M2 build: success case — `fs` (secure-filesystem-server 0.2.0) +
`memory` (memory-server 0.6.3), both negotiating 2025-11-25, 23 tools
merged, two clean sessions (`end=ClientEof … rejected=0 discarded=0`,
one with three routed calls), no orphans; failure case — with a
duplicate `fs2` block, all three upstreams initialized and the proxy
then refused service with
`proxy failed: tool "read_file" is offered by both "fs" and "fs2"`,
Claude Desktop reporting the server disconnected.

M5 run: **not yet performed.** The M5 code landed with the scripted and
end-to-end suites green (including the real Cedar engine and a real
trace file in `proxy_e2e.rs`), but the enforcement variant above is a
manual run against Claude Desktop on Windows and is still owed. It is
the first contact between `windows-path-prefix` and a real filesystem
server; record the result — and any flavor correction it forces — here.

Current pin: **2025-11-25**.
