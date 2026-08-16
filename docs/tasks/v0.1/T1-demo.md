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
- [x] *(M5+)* Enforcement: the same session under a grant file — the
      tool list is filtered, an in-envelope call succeeds, an
      out-of-envelope one is denied, and both are in the trace. Worked
      example below.

The M1/M2 rows of this checklist are the unenforced proxy; the M5
section below is the enforced one. Both have now run clean — the M5
runs, and the one flavor correction they forced, are recorded at the
bottom.

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

- [x] **Startup.** The log shows `enforcing grants principal=desktop-bot
      grants=2` before the upstreams come up, and
      `all upstreams initialized … enforced=true`.
- [x] **The list is filtered.** Claude Desktop shows **2** tools for
      this server, not the filesystem server's full set — no
      `write_file`, no `edit_file`, no `move_file`. (The upstream still
      offers them; the client is not shown them.)
- [x] **In-envelope call succeeds.** "Read
      `C:\Users\flavi\Desktop\flavium-demo\ok.txt`" returns the file's
      contents.
- [x] **Out-of-envelope call is denied.** "Read the other file on my
      Desktop" (any path outside `flavium-demo\`) comes back as a tool
      error reading `denied by policy`; the model can see the denial and
      usually says so. The log has a
      `WARN … call denied tool=read_text_file … reason=arguments outside
      the grant envelope` line.
- [x] **Traversal is denied too** — the row the path flavor exists for.
      Ask for
      `C:\Users\flavi\Desktop\flavium-demo\..\<the other file>`; it must
      be denied, and the trace must record the *normalized* path
      (`c:/users/flavi/desktop/<the other file>` — separators unified,
      `..` resolved, ASCII case folded because the grant declared the
      Windows flavor), not the one that was typed. This is the check
      that a byte-prefix comparison alone would fail. Note that this row
      is therefore indistinguishable in the trace from a direct read of
      the same file — that is the intended record, not a gap: the
      decision was made on the resolved path, so the record shows the
      resolved path.
- [x] **An ungranted tool is invisible and unusable.** Asking to write a
      file gets "no such tool" from the model; if a call is made by hand
      it answers `-32602 Unknown tool: write_file` — the same bytes as a
      tool no upstream offers.
- [x] **The trace.** `flavium-trace.jsonl` has one JSON object per line,
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

### M5 run, part one — scripted client, 2026-08-16

The enforcement rows above are written for Claude Desktop and the GUI
half is still owed; the *semantics* were run first against the same real
upstream (`secure-filesystem-server` 0.2.0 via `npx.cmd`, 14 tools
offered) by piping JSON-RPC frames straight into
`flavium proxy --config … --trace …`, which is what the checklist's
*Checking the wire directly* section does, extended to a whole session.
That run is what forced the case-folding change below, so it is recorded
here rather than folded into the Claude Desktop run.

Grant file: exactly the two grants above. What the ten frames showed:

- startup `enforcing grants principal=desktop-bot grants=2`, then
  `all upstreams initialized … upstreams=1 tools=14 enforced=true`;
- `tools_listed offered=14 granted=2` — the client is shown two tools;
- `flavium-demo\ok.txt` allowed; `outside.txt` denied
  (`denied by policy`, `isError: true`);
- `flavium-demo\..\outside.txt` denied, and the trace records the
  **normalized** `C:/Users/flavi/Desktop/outside.txt` — the row a byte
  prefix comparison alone would fail;
- `Desktop\..\Desktop\flavium-demo\ok.txt` allowed, which is the same
  normalization pointing the other way;
- forward slashes in a Windows-flavored path allowed;
- `write_file` answered `-32602 Unknown tool: write_file`;
- trace: 16 lines, `seq` dense from 1, `session_started` carrying the
  envelope → a `call_decided` per call → a `call_completed` per allowed
  call → `session_ended`; no orphan processes.

**The one disagreement, and what it changed.**
`c:\users\flavi\desktop\flavium-demo\ok.txt` was **denied** while the
same file spelled `C:\Users\…` was allowed — flavium deciding on the
spelling, not the resource. A false denial, so nothing leaked, but it is
the gap this checklist row exists to find. The Windows flavor now folds
ASCII case on both sides; see the D4 entry in
[T1-mcp-proxy-core.md](T1-mcp-proxy-core.md)'s M5 note. No other
disagreement surfaced.

Before changing the normalizer the upstream's *own* case behavior was
measured, unenforced (`--unenforced -- npx.cmd …`, allowed directory
`C:\Users\flavi\Desktop`), because "Windows is case-insensitive" is not
a fact about an upstream, it is a hypothesis about one:

| Path asked for | `server-filesystem` 0.2.0 |
|---|---|
| `C:\Users\flavi\Desktop\flavium-demo\ok.txt` | served |
| `C:\Users\flavi\Desktop\FLAVIUM-DEMO\ok.txt` | **served** |
| `C:\Users\flavi\Desktop\flavium-demo\OK.TXT` | **served** |
| `c:\Users\flavi\Desktop\flavium-demo\ok.txt` | **served** |
| `c:\users\flavi\desktop\flavium-demo\ok.txt` | refused: *path outside allowed directories* |

Rows 2–4 are the false denials the old rule produced: the upstream
serves those paths, flavium refused them. They are what the folding
buys. Row 5 is the upstream's own allowed-directories check, which
compares its root case-**sensitively** (Node's `path.resolve` upper-cases
the drive letter, which is why row 4 passes and row 5 does not), so under
the new rule flavium allows that spelling and the upstream refuses it —
the call still fails, the denial simply comes from the upstream instead
of from policy. Worth stating plainly: folding does not promise the
upstream will serve a path, only that **flavium is not the one refusing
it on spelling**. And rows 2–3 are the evidence that matters for the
other direction — an upstream that resolves case-insensitively below its
root means a folded match is the same resource, not a different one.

### M5 run, part two — Claude Desktop, 2026-08-16

Two sessions on the folding build, `client_name="claude-ai"` version
0.1.0, client face negotiating **2025-11-25**, same grant file, same
upstream. Together they tick every row above.

Session `1786904332-884` (18:18–18:23) — `tools_listed offered=14
granted=2`, an in-envelope read served, `…\outside.txt` denied
(`out_of_envelope`, with the matching `WARN … call denied` line), and
`frames_to_upstream=2` against three calls: the denied call never
reached the upstream, which is the property the row is really about.

Session `1786905437-45460` (18:37–18:39) — the traversal row and the
ungranted-tool row. `…\flavium-demo\..\outside.txt` was denied and
recorded as `c:/users/flavi/desktop/outside.txt`, `..` resolved before
the comparison; `frames_to_upstream=1` against three calls. Asking the
model to write a file produced **no `write_file` frame at all** — no
denial to log, because a tool it was never shown is a tool it does not
reach for. That row's negative half is what the log can show; the
model's own "I don't have a tool for that" is only visible in the UI.

**One observation the enforced runs produced that the scripted one did
not.** In session `1786904332-884` a call was *allowed* by policy and
came back `is_error: true` from the upstream: the client had sent a
fully lowercased path, flavium folded it and let it through, and
`server-filesystem`'s own allowed-directories check — case-sensitive on
its root — refused it. Correct behavior on both sides, and the reason
the folding change is worded as it is: folding promises only that
flavium is not the one refusing on spelling.

It also exposed a limit of the record, **since fixed and re-run** — see
part three. The two reads in that session were traced with identical
`args`, one served and one refused upstream, because the trace stored
only the value the decision was made on and the difference between them
was case. The record reproduced the decision (D9) but could not show
what the agent asked for, and no other artifact could either — Claude
Desktop's MCP log does not record params. `call_decided` now carries
`args_as_sent` beside `args`, holding the caller's own spelling for the
arguments normalization changed and only those.

### M5 run, part three — the record proving itself, 2026-08-16

The point of a flight recorder is that someone can read it afterwards
and tell what happened, so the field added for that was put in front of
a real client rather than trusted from unit tests. Session
`1786906802-45268`, Claude Desktop `claude-ai` 0.1.0, 19:00–19:03, same
grant file, four asks. `frames_to_upstream=3` against five calls,
`end=ClientEof … rejected=0 discarded=0`, no orphans.

| # | `args` (evaluated) | `args_as_sent` | Outcome |
|---|---|---|---|
| 0 | `c:/users/flavi/desktop/flavium-demo/ok.txt` | `C:\Users\flavi\Desktop\flavium-demo\ok.txt` | allow, served |
| 1 | `c:/users/flavi/desktop/outside.txt` | `C:\Users\flavi\Desktop\outside.txt` | **deny**, never forwarded |
| 2 | *(`list_allowed_directories`)* | — | allow, served |
| 3 | `c:/users/flavi/desktop/outside.txt` | `C:\Users\flavi\Desktop\flavium-demo\..\outside.txt` | **deny**, never forwarded |
| 4 | `c:/users/flavi/desktop/flavium-demo/ok.txt` | `c:\users\flavi\desktop\flavium-demo\ok.txt` | allow, upstream `is_error` |

**Calls 1 and 3 are the pair the field exists for.** Identical evaluated
paths, identical decisions, and until this build identical lines — an
auditor could not have told an honest read of `outside.txt` from a
traversal aimed out of the granted directory. Now the record says which
was which, and still says the decision was made on the resolved path.

**Calls 0 and 4 are the other half.** Same evaluated path, same `allow`,
**different outcomes** — one served, one refused by the upstream's own
case-sensitive root check. That pair is what part two could only
describe; the record now carries its own explanation, which is the
difference between a log and an audit trail.

Call 2 was the model's, not the checklist's: after the first denial it
asked what directories were allowed. Worth noting because the answer it
got (`C:\Users\flavi\Desktop`, the *upstream's* scope) does not describe
the grant, and reading the two as one thing is the natural mistake —
the envelope is narrower than anything the upstream will tell you.

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
| 2026-08-16 | M5 | Claude Desktop (clientInfo `claude-ai`) | 0.1.0 | **2025-11-25** |

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

M5 run: **performed 2026-08-16**, in three parts recorded above — a
scripted client first, which forced one flavor correction (ASCII case
folding under `windows-path-prefix`), then two Claude Desktop sessions
on the corrected build covering every checklist row, then a third that
re-ran the ambiguous pair once `args_as_sent` existed. Upstream
`secure-filesystem-server` 0.2.0 throughout; client face 2025-11-25.
This is T1's acceptance criterion met: a real client works unmodified
through the proxy, a grant file denies out-of-envelope calls, and the
denials are logged.

Current pin: **2025-11-25**.
