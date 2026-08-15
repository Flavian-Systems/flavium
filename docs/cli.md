# `flavium` — command-line reference

**As of T1/M2 (2026-08).** The `flavium` binary has one subcommand,
`proxy`, which runs the MCP proxy in front of one or more upstream tool
servers. This page is the operator's reference: commands and flags, the
`flavium.toml` config file, what goes to stdout and stderr, exit codes,
startup errors and how to fix them, and how to wire the proxy into a
client. How the proxy works inside is in
[docs/architecture/proxy-mcp.md](architecture/proxy-mcp.md); vocabulary
is in [GLOSSARY.md](GLOSSARY.md).

Enforcement — grants, budgets, tracing — is not wired yet (T1 M3–M5).
Today the proxy is a faithful, bounded middlebox: every tool the
upstreams offer is offered to the client, and every call is forwarded.

## Contents

1. [Synopsis](#1-synopsis)
2. [`flavium proxy`](#2-flavium-proxy)
3. [The config file](#3-the-config-file)
4. [stdout, stderr, and logging](#4-stdout-stderr-and-logging)
5. [Exit codes](#5-exit-codes)
6. [Startup errors and what they mean](#6-startup-errors-and-what-they-mean)
7. [Wiring a client](#7-wiring-a-client)
8. [Fixed limits](#8-fixed-limits)
9. [Not yet](#9-not-yet)

## 1. Synopsis

```text
flavium                                  print the banner and exit 0
flavium --version | -V                   print the version
flavium --help | -h                      top-level help
flavium proxy --config <FILE>            proxy: upstreams from a TOML file
flavium proxy -- <COMMAND> [ARGS...]     proxy: one stdio upstream from the command line
flavium proxy --help                     subcommand help
```

`--config` and `-- <COMMAND>` are mutually exclusive; exactly one is
required.

## 2. `flavium proxy`

Runs one MCP session: the proxy presents an MCP server on **this
process's stdin/stdout**, connects to every configured upstream,
merges their tools, and serves the client until the client closes its
input (or something fails — §5). One client launch is one process; the
process exits when the session ends.

| Flag / argument | Meaning |
|---|---|
| `--config <FILE>` | Path to the upstream config file (§3). |
| `-- <COMMAND> [ARGS...]` | Everything after `--` is the command line of a single stdio upstream. The upstream is named `upstream` in logs. Shorthand equivalent to a one-entry config file with `command = [<COMMAND>, ARGS...]`. |
| `-h`, `--help` | Help. |

There are no flags for log level, frame size, or timeouts today: the
log level comes from `RUST_LOG` (§4); the limits are compiled in (§8).

Startup order: the config is validated structurally → every stdio
upstream is spawned and every HTTP transport built (if one fails, the
ones already started are shut down) → every upstream is initialized,
concurrently, and its tool list drained → tool names are checked for
collisions → only then is the client's `initialize` read and answered.

## 3. The config file

TOML. One `[[upstream]]` table per upstream server; nothing else at the
top level. Unknown keys anywhere are an error (typos are caught rather
than ignored).

```toml
[[upstream]]
name = "fs"
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/data"]

[[upstream]]
name = "search"
url = "https://example.com/mcp"
headers = { Authorization = "Bearer …" }
```

### `[[upstream]]` keys

| Key | Type | Required | Meaning |
|---|---|---|---|
| `name` | string | yes | Operator-chosen label. Must be non-empty and unique across the file. Appears in logs and error messages only — it is **not** prepended to tool names (namespacing is a documented follow-up, not M2). With several upstreams supplying `instructions`, each block is headed `## <name>` in the merged instructions the client receives. |
| `command` | array of strings | one of `command`/`url` | Program followed by its arguments, as separate array elements — no shell is involved, so no quoting, globbing, or `$VAR` expansion. The program is resolved on `PATH` like any spawned process. The child's stdin/stdout carry MCP; its stderr is inherited from the proxy (so its logs appear next to the proxy's). Must be non-empty with a non-empty program. |
| `url` | string | one of `command`/`url` | A streamable-HTTP MCP endpoint. Must parse and use the `http` or `https` scheme. HTTPS uses rustls with bundled roots; redirects are refused. |
| `headers` | table of string → string | no (only with `url`) | Extra HTTP headers sent on every request to that upstream — typically `Authorization`. Names and values must be legal HTTP header syntax (a value with a newline is rejected at startup). Values are treated as secrets: never logged, never echoed in errors. Specifying `headers` on a `command` upstream is an error. |

Exactly one of `command` or `url` is required per entry; both or
neither is an error.

**Order matters.** Upstreams are numbered in file order; the merged
`tools/list` presents upstream 0's tools first, then upstream 1's, and
so on. Reordering entries reorders what the client sees (it does not
change routing — routing is by tool name).

**Tool-name collisions are refused, not resolved.** If two upstreams
(or one upstream, twice) declare the same tool name, the proxy exits at
startup with `tool "x" is offered by both "a" and "b"`. Pick upstreams
whose tool families do not overlap, or drop one.

**Secrets live in this file literally.** There is no environment
variable substitution — `Authorization = "Bearer $TOKEN"` sends the
literal text `$TOKEN`. Protect the file with filesystem
permissions and keep it out of version control (the repo's
`.gitignore` already excludes a root-level `flavium.toml`).

**Windows.** TOML strings need doubled backslashes
(`"C:\\Users\\me\\Desktop"`), and `npx` must be invoked as `npx.cmd`
(the proxy spawns processes directly, without a shell that would
resolve `.cmd` for you). Use absolute paths in client configs.

### Config errors

All are reported on stderr and exit 1 (§5). The message names the
offending upstream where there is one.

| Message | Cause |
|---|---|
| `cannot read <file>: …` | Missing or unreadable path. |
| `cannot parse <file>: …` | Not valid TOML, or an unknown key (`unknown field …`). |
| `<file>: no [[upstream]] entries` | Empty or entry-less file. |
| ``upstream "x": exactly one of `command` or `url` is required`` | Neither or both given. |
| ``upstream "x": `headers` only applies to `url` upstreams`` | `headers` on a stdio entry. |
| `no upstreams configured` · `upstream #N has an empty name` · `duplicate upstream name "x"` · `upstream "x" has an empty command` · `upstream "x" has an invalid url "…"` | Structural validation of the resolved set (URLs are shown redacted: scheme, host, port, path only). |
| `upstream "x" has an unusable HTTP configuration: invalid header name "…"` / `invalid value for header "…"` | Header syntax; the value is never printed. |

## 4. stdout, stderr, and logging

- **stdout is the MCP wire.** Only newline-delimited JSON-RPC frames
  are written to it. Nothing else — no banner, no logs — because the
  client on the other end is parsing it.
- **stderr is for humans**: the proxy's own log lines, plus whatever
  the spawned stdio upstreams write to *their* stderr (inherited).
  Clients that capture MCP server stderr (Claude Desktop writes it to
  `mcp-server-<entry name>.log`) capture both.
- **Log level** comes from `RUST_LOG`, default `info`. Examples:
  `RUST_LOG=debug` for everything (per-frame drop/discard reasons live
  at `debug`); `RUST_LOG=flavium_proxy_mcp=debug` for the proxy crate
  only; `RUST_LOG=warn` for problems only. Set it in the client's
  server-entry environment (`"env": {"RUST_LOG": "debug"}` in Claude
  Desktop's config) — the proxy is normally not launched from a shell
  you control.
- **What is logged**: the handshake on each face (offered/negotiated
  protocol version, client and server names/versions), each upstream
  spawn/connect, the tool count, every session-ending condition, and
  the end-of-session summary (frames forwarded, delivered,
  undelivered, rejected, discarded). Never logged: header values, URL
  userinfo/query strings, frame contents.

## 5. Exit codes

| Code | When |
|---|---|
| `0` | The banner/`--help`/`--version` paths; and a proxy session that ended **cleanly**: the client closed its input (normal MCP shutdown) *and* every frame accepted for the client was delivered. Logged as `session closed cleanly`. |
| `1` | Any of: the upstream source is missing (``either --config or an upstream command after `--` is required``); a config or startup error (`proxy failed: <error chain>` — §6); or the session ended **abnormally** (`session ended abnormally` with the summary): an upstream exited or failed, an HTTP session expired, the client stopped reading, a re-list introduced a tool-name collision, or an internal task died. Until supervision lands (T3), *any* upstream ending ends the session — the proxy prefers exiting loudly to serving a tool surface that silently shrank. |
| `2` | Command-line usage error from the argument parser (unknown flag, `--config` together with `-- <COMMAND>`). |

## 6. Startup errors and what they mean

Startup problems are printed as one line, `proxy failed: <error>:
<cause>: <cause>…`, with the full chain — the outermost error names
the upstream, the innermost says what actually happened.

| Error chain contains | Meaning | Usual fix |
|---|---|---|
| ``could not be started: failed to spawn upstream `prog`: …`` | The stdio command's program could not be started — not on `PATH`, not executable, or (Windows) a `.cmd` shim invoked without the extension. The OS error follows. | Use an absolute path, or `npx.cmd` on Windows. |
| `failed to connect: upstream did not complete initialize in time` | No `initialize` response within 60 s. Common with `npx -y …` on a cold cache. | Run the upstream once by hand to warm the cache; check it speaks MCP on stdio. |
| `failed to connect: upstream closed during initialize` | The child exited (or the pipe closed) before answering. Its own stderr, just above, usually says why. | Fix the upstream's arguments/environment. |
| `failed to connect: upstream refused initialize with error -32603: upstream request failed` | **HTTP upstream unreachable or unusable**: the `initialize` POST failed — connection refused, DNS, TLS, a redirect (refused on purpose), a non-success status, or a body that was not a JSON-RPC response. The transport turns a failed POST into a synthesized error response, which is why it reads as a refusal; the real reason is in the `WARN … upstream POST failed … reason=…` line just above it. | Check URL, headers, network, TLS. |
| `failed to connect: upstream refused initialize with error <other code>: …` | The server itself answered `initialize` with a JSON-RPC error. | Read the message; it is the server's. |
| `failed to connect: upstream negotiated unsupported protocol version "…"` | The server insists on a revision the proxy does not speak (it accepts `2025-06-18` and `2025-11-25`; batching-era `2025-03-26` and older, and the 2026-07-28 revision, are refused). | Use a server on a supported revision. |
| `failed to connect: transport failed during initialize: …` | The transport itself died during the handshake: for HTTP, the server ended the session (404 under a session id) or the ordered `initialized` POST failed; for stdio, an I/O error on the pipes. | See the cause after the last colon and the surrounding log lines. |
| `failed to list tools: …` | `initialize` succeeded but `tools/list` failed, did not parse, paged past 1 000 pages, declared more than 10 000 tools, or exceeded the byte budget. | The upstream is misbehaving; check its output with the MCP Inspector. |
| `tool "x" is offered by both "a" and "b"` | Name collision across (or within) upstreams. | Remove one, or wait for namespacing. |
| `no upstreams configured` and the other messages in §3 | Config errors. | Fix the file. |

If the client (Claude Desktop, Claude Code) shows the server as failed
without more detail, the proxy's stderr — captured in the client's MCP
log — has the line above.

## 7. Wiring a client

Point any stdio MCP client at the `flavium` binary with the arguments
above. Use absolute paths; clients rarely inherit your shell's `PATH`
or working directory.

**Claude Desktop** (`claude_desktop_config.json`, Settings → Developer
→ Edit Config; fully quit and restart the app afterwards):

```json
{
  "mcpServers": {
    "flavium": {
      "command": "/absolute/path/to/flavium",
      "args": ["proxy", "--config", "/absolute/path/to/flavium.toml"],
      "env": { "RUST_LOG": "info" }
    }
  }
}
```

Single-upstream shorthand, no config file:

```json
"args": ["proxy", "--", "npx", "-y", "@modelcontextprotocol/server-filesystem", "/data"]
```

On Windows use `flavium.exe`, `npx.cmd`, and doubled backslashes in
the JSON strings.

**Claude Code:**

```bash
claude mcp add flavium -- /absolute/path/to/flavium proxy --config /absolute/path/to/flavium.toml
```

**Any other client:** run `flavium proxy …` as the server command; the
proxy identifies itself as `serverInfo.name = "flavium"` and advertises
exactly one capability, `tools` with `listChanged`. The MCP Inspector
(`npx @modelcontextprotocol/inspector`) is a convenient way to see the
merged tool list and the negotiated protocol version.

The recorded runs against real clients — what was checked and what
they answered — are in
[docs/tasks/v0.1/T1-m1-demo.md](tasks/v0.1/T1-m1-demo.md).

## 8. Fixed limits

Compiled in for now (`ProxyConfig::default()`); a `--config` knob for
these is not implemented. Full table with sources in the
[architecture doc, §9](architecture/proxy-mcp.md#9-limits-and-tuning-knobs).

| Limit | Value |
|---|---|
| Max frame size, both directions, all transports (also the per-upstream tool-table byte budget) | 16 MiB |
| Per-upstream `initialize` deadline | 60 s |
| Per-page `tools/list` deadline | 30 s |
| Shutdown grace for actors and the client writer | 5 s |
| Time a stdio child gets to exit after its stdin closes, before it is killed | 5 s |
| One frame write to a child may stall before the pipe is declared dead | 30 s |
| `tools/list` pages / tools accepted per upstream | 1 000 / 10 000 |
| HTTP connect timeout · session `DELETE` budget · GET-stream reconnect backoff | 10 s · 2 s · 1–30 s |
| Protocol revisions offered / accepted | `2025-11-25` / `2025-06-18`, `2025-11-25` |

## 9. Not yet

Stated so nobody hunts for a flag that does not exist:

- **No grants, budgets, or trace output** — T1 M3–M5, T2, T4. Today
  every upstream tool is exposed and every call is forwarded.
- **No log-level, timeout, or frame-size flags** — `RUST_LOG` and the
  compiled-in defaults above.
- **No environment-variable expansion in the config file.**
- **No tool namespacing** — colliding names are refused.
- **No upstream supervision or restart** — an upstream ending ends the
  session (T3).
- **No HTTP *server* face** — the client side is stdio only.
- **No SSE resumability** on HTTP upstreams; a dropped response stream
  yields a synthesized error for that request.
- **The 2026-07-28 MCP revision** is not spoken on either face.
