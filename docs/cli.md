# `flavium` — command-line reference

**As of T1/M5 (2026-08).** The `flavium` binary has one subcommand,
`proxy`, which runs the MCP proxy in front of one or more upstream tool
servers. This page is the operator's reference: commands and flags, the
`flavium.toml` config file, what goes to stdout and stderr, exit codes,
startup errors and how to fix them, the trace file, and how to wire the
proxy into a client. How the proxy works inside is in
[docs/architecture/proxy-mcp.md](architecture/proxy-mcp.md), and what a
grant *means* — the model, attenuation, the Cedar compilation, and one
grant followed from TOML to trace — is in
[docs/architecture/core-and-policy.md](architecture/core-and-policy.md);
vocabulary is in [GLOSSARY.md](GLOSSARY.md).

Since M5 the proxy **enforces**: the client is shown only the tools the
grant file names, every `tools/call` is authorized before it is
forwarded, and every decision can be recorded to a JSONL trace. A config
file with no grants refuses to start — the transparent middlebox is
available, but only when you ask for it by name (`--unenforced`).

Budgets (T2), delegation (T3) and the hash-chained recorder (T4) are
still ahead.

## Contents

1. [Synopsis](#1-synopsis)
2. [`flavium proxy`](#2-flavium-proxy)
3. [The config file](#3-the-config-file)
4. [Grants](#4-grants)
5. [stdout, stderr, and logging](#5-stdout-stderr-and-logging)
6. [The trace file](#6-the-trace-file)
7. [Exit codes](#7-exit-codes)
8. [Startup errors and what they mean](#8-startup-errors-and-what-they-mean)
9. [Wiring a client](#9-wiring-a-client)
10. [Fixed limits](#10-fixed-limits)
11. [Not yet](#11-not-yet)

## 1. Synopsis

```text
flavium                                        print the banner and exit 0
flavium --version | -V                         print the version
flavium --help | -h                            top-level help
flavium proxy --config <FILE>                  proxy: upstreams and grants from a TOML file
flavium proxy --config <FILE> --trace <FILE>   … and record the session
flavium proxy --unenforced --config <FILE>     proxy: no enforcement, no trace
flavium proxy --unenforced -- <COMMAND> [ARGS] proxy: one stdio upstream, no enforcement
flavium proxy --help                           subcommand help
```

`--config` and `-- <COMMAND>` are mutually exclusive; exactly one is
required. `--unenforced` and `--trace` are mutually exclusive.

## 2. `flavium proxy`

Runs one MCP session: the proxy presents an MCP server on **this
process's stdin/stdout**, connects to every configured upstream, merges
their tools, filters the merged list to the granted ones, authorizes
each call, and serves the client until the client closes its input (or
something fails — §7). One client launch is one process; the process
exits when the session ends.

| Flag / argument | Meaning |
|---|---|
| `--config <FILE>` | Path to the config file (§3): upstreams and grants. |
| `--unenforced` | Run the transparent middlebox: expose every upstream tool, forward every call, record nothing. Logs a `WARN` at startup, every session. Refused together with `--trace`, and refused when the config file declares grants. |
| `--trace <FILE>` | Append a JSONL trace of the session to this file (§6). Created `0600` on unix, opened at startup so a bad path fails immediately. |
| `-- <COMMAND> [ARGS...]` | Everything after `--` is the command line of a single stdio upstream, named `upstream` in logs. **Requires `--unenforced`**: the shorthand has nowhere to put grants. |
| `-h`, `--help` | Help. |

There are no flags for log level, frame size, or timeouts: the log level
comes from `RUST_LOG` (§5); the limits are compiled in (§10).

Startup order: the config is parsed and every grant validated → the
grants are compiled to policies (a grant that cannot be compiled stops
here) → the trace file is opened → every stdio upstream is spawned and
every HTTP transport built (if one fails, the ones already started are
shut down) → every upstream is initialized, concurrently, and its tool
list drained → tool names are checked for collisions → the session's
envelope is recorded → only then is the client's `initialize` read and
answered.

**Enforcement is not a flag.** It follows from the file having grants.
Three postures are possible for a config without them — enforce, refuse,
or forward — and only *refuse* makes their absence impossible to
overlook. A warning would not do: the failure mode worth engineering
against is an operator who believes they are protected and is not, and
that warning would be the only thing between an agent and every tool it
can see.

## 3. The config file

TOML. A `version`, an optional `principal`, one `[[upstream]]` table per
upstream server, and any number of `[[grant]]` tables. Unknown keys
anywhere are an error — a typo in a policy file is a security decision
nobody made.

```toml
version = 1                        # required
principal = "invoice-bot"          # required when there are grants

[[upstream]]
name = "fs"
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/data"]

[[upstream]]
name = "search"
url = "https://example.com/mcp"
headers = { Authorization = "Bearer …" }

[[grant]]
tool = "read_file"
expires = 2026-09-01T00:00:00Z
[grant.args]
path = { path-prefix = "/data/invoices/" }
```

### `version`

Required, and it must be the integer `1`. Any other value — a different
number, a string, a float — refuses at startup and names what this build
supports.

Note first what it is *not* for. A file written for a richer future
flavium, read by an older binary, already fails closed: the new key is an
unknown key. `version` exists for the direction nothing else can catch —
an **old file whose keys still parse but no longer mean what its author
meant**. In a policy file, re-interpreted can mean *widened*. So:

- adding an **optional** key does not bump — old files still mean exactly
  what they said;
- changing what an existing key means, removing one, or changing a
  default **bumps**;
- a binary accepts the versions it implements **by exact match**.

When a bump comes, the new binary **refuses** the old dialect rather than
interpreting it, and points at a migration note. Silent multi-dialect
support is what you want from a build tool and the opposite of what you
want from a grant file, where the right response to "these words changed
meaning" is a human re-reading their own grants.

### `principal`

The identity every call in the session is attributed to, for
authorization and for the trace. Required whenever the file declares
grants; non-empty, no ASCII control characters. MCP `clientInfo` is
untrusted data and is never identity.

### `[[upstream]]` keys

| Key | Type | Required | Meaning |
|---|---|---|---|
| `name` | string | yes | Operator-chosen label. Must be non-empty and unique across the file. Appears in logs, errors and the trace only — it is **not** prepended to tool names (namespacing is a documented follow-up). With several upstreams supplying `instructions`, each block is headed `## <name>` in the merged instructions the client receives. |
| `command` | array of strings | one of `command`/`url` | Program followed by its arguments, as separate array elements — no shell is involved, so no quoting, globbing, or `$VAR` expansion. The program is resolved on `PATH` like any spawned process. The child's stdin/stdout carry MCP; its stderr is inherited from the proxy (so its logs appear next to the proxy's). Must be non-empty with a non-empty program. |
| `url` | string | one of `command`/`url` | A streamable-HTTP MCP endpoint. Must parse and use the `http` or `https` scheme. HTTPS uses rustls with bundled roots; redirects are refused. |
| `headers` | table of string → string | no (only with `url`) | Extra HTTP headers sent on every request to that upstream — typically `Authorization`. Names and values must be legal HTTP header syntax (a value with a newline is rejected at startup). Values are treated as secrets: never logged, never echoed in errors, never traced. Specifying `headers` on a `command` upstream is an error. |

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
(`"C:\\Users\\me\\Desktop"`) — or use single-quoted literal strings
(`'C:\Users\me\Desktop'`), which is easier to read in grants — and `npx`
must be invoked as `npx.cmd` (the proxy spawns processes directly,
without a shell that would resolve `.cmd` for you). Use absolute paths in
client configs.

### Config errors

All are reported on stderr and exit 1 (§7). The message names the
offending upstream or grant where there is one.

| Message | Cause |
|---|---|
| `cannot read <file>: …` | Missing or unreadable path. |
| `cannot parse <file>: …` | Not valid TOML, or an unknown key (`unknown field …`), or a duplicate key (TOML rejects those itself). |
| `<file>: `version` is required` · `unsupported config `version` (…)` | See above. |
| `<file>: no [[upstream]] entries` | Empty or upstream-less file. |
| `<file>: no [[grant]] entries, so nothing would be enforced…` | The posture decision of §2. Add grants, or pass `--unenforced`. |
| `<file>: --unenforced was given but the file declares grants` | The other half of the same mistake: grants written and then ignored. |
| ``upstream "x": exactly one of `command` or `url` is required`` | Neither or both given. |
| ``upstream "x": `headers` only applies to `url` upstreams`` | `headers` on a stdio entry. |
| `no upstreams configured` · `upstream #N has an empty name` · `duplicate upstream name "x"` · `upstream "x" has an empty command` · `upstream "x" has an invalid url "…"` | Structural validation of the resolved set (URLs are shown redacted: scheme, host, port, path only). |
| `upstream "x" has an unusable HTTP configuration: invalid header name "…"` / `invalid value for header "…"` | Header syntax; the value is never printed. |

## 4. Grants

One `[[grant]]` table is authority over one tool. Several grants may name
the same tool; a call is allowed if **any** live grant admits it.

```toml
[[grant]]
tool = "send_mail"
[grant.args]
to    = { suffix = "@yourco.com" }
cc    = { absent = true }
bcc   = { absent = true }
count = { range = { min = 1, max = 10 } }
kind  = { one-of = ["invoice", "receipt"] }
```

| Key | Type | Meaning |
|---|---|---|
| `tool` | string | The tool name, exactly as the upstream declares it. Non-empty, no ASCII control characters. |
| `expires` | TOML offset date-time | When the grant stops existing. The boundary is exclusive: at `now == expires` it is already gone. Optional; absent means never. |
| `[grant.args]` | table | One entry per argument you want to constrain. |

### Argument constraints

Exactly **one** key per argument. Zero keys and two keys are both startup
errors — TOML accepts them, and each would be an ambiguity in a file
where ambiguity means authority.

| Key | Example | Admits |
|---|---|---|
| `prefix` | `{ prefix = "/data/" }` | A string starting with those **bytes**. No normalization: `prefix = "/data/inv"` admits `/data/invalid`. |
| `path-prefix` | `{ path-prefix = "/data/invoices/" }` | A **path** under that prefix, POSIX-style: `/` separates, `\` is an ordinary filename byte. Both the prefix and the call's value are normalized before comparison (§ below). |
| `windows-path-prefix` | `{ windows-path-prefix = 'C:\Users\me\Desktop\' }` | The same, Windows-style: both `/` and `\` separate. |
| `suffix` | `{ suffix = "@yourco.com" }` | A string ending with those bytes. Write the `@`: `suffix = "yourco.com"` also admits `x@evilyourco.com`. |
| `one-of` | `{ one-of = ["invoice", "receipt"] }` | Exactly one of those strings. |
| `range` | `{ range = { min = 1, max = 10 } }` | An integer within the inclusive bounds. Either bound may be omitted (but not both). A JSON number that is not an integer — `3.0`, `1e3`, `-0`, anything outside `i64` — is not an integer here and is never admitted. |
| `absent` | `{ absent = true }` | Only a **missing** argument. This is how `cc`/`bcc` get closed. `absent = false` is an error: there is no "must be present, anything goes" constraint. |

**An argument no constraint names is not examined.** This is the
authoring pitfall worth repeating: granting `send_mail` with only `to`
constrained lets the agent set `bcc` to anything. Constrain every
argument that matters, and close the rest with `absent`.

Comparison is byte-wise and fails closed: a constrained argument that is
missing (except under `absent`), of the wrong type, or of a shape the
core does not model is **not** admitted. The full semantics of each
constraint — including what happens when several grants name one tool,
and what a grant compiles to — are in
[docs/architecture/core-and-policy.md](architecture/core-and-policy.md).

### Path normalization, and why the flavor is yours to declare

The two path-flavored keys are the only ones that normalize. Before the
comparison, separators are unified and collapsed, `.` segments are
dropped, `..` is resolved against the previous segment (never escaping
the root of an absolute path), and a trailing separator is dropped from
the *value* — a prefix keeps the one you wrote, so `/data/invoices/`
never starts admitting `/data/invoices.bak`.

Without this, `path = { prefix = "/data/invoices/" }` **allows**
`read_file("/data/invoices/../../etc/passwd")`: it is a byte prefix
match, while the upstream reads `/etc/passwd`.

One separator run is *not* collapsed: a **leading** run of two or more.
On Windows `\\data\share\x` is a UNC path to a share on a host called
`data`, while `\data\share\x` is a directory on the current drive — two
different machines — so a grant over one must not admit the other. POSIX
gets the same treatment, because the standard leaves a leading `//`
implementation-defined and flavium will not guess. Write the prefix the
way the calls will be written: `windows-path-prefix = '\\server\share\'`
for a share, `'D:\data\'` or `'\data\'` for a local directory.

A path prefix that normalizes to **nothing** — `"."`, `"./"`, `""`,
`"a/.."` — is a startup error, not an empty prefix. An empty prefix
admits every string, so accepting one would silently turn "the working
directory" into "the whole filesystem", which is the one direction this
design says must never happen. Write the directory you mean. (`"/"` is
different: an operator who writes the root has said what they meant, and
it is accepted.)

Why two keys instead of one that guesses: `\` is an ordinary filename
byte on POSIX and *the* separator on Windows, so either fixed answer is a
silent over-permission on the other platform. The proxy's own host says
nothing about where an upstream resolves paths — an HTTP upstream is
another machine, and a stdio child can be in WSL or a container. A grant
names one tool, and a tool belongs to one upstream, so the grant is
exactly the scope at which you know the answer.

`windows-path-prefix` also folds **ASCII** case, on the prefix and on the
value alike: Windows resolves `C:\Users\Me\x` and `c:\users\me\x` to one
file, so a grant that admits one spelling and refuses the other is
deciding about the spelling rather than the resource. Declaring the
flavor declares the whole resolution rule, not just the separator. The
trace records the folded form, because it records what the decision was
made on. `path-prefix` folds nothing — POSIX filesystems are
case-sensitive.

What normalization does **not** do: fold anything outside ASCII (so
`C:\data\Ä\x` under `C:\data\ä\` is refused — a lost call, never a
leaked one; full Unicode folding can merge characters Windows keeps
apart, which would be the opposite kind of mistake), no filesystem
access, and **no symlink resolution**. Symlinks and hardlinks are outside
what a proxy can see; that boundary is DESIGN §7's and v0.2's job. One
more edge worth knowing: an NTFS directory switched to case-sensitive
(`fsutil file setCaseSensitiveInfo`) can hold two files whose names
differ only in case, and this normalizer treats them as one.

### What is refused, and what is only warned about

Refused at startup, because a malformed file could mean anything:

- a missing or unrecognised `version`;
- an unknown key anywhere, including `budget` (the T2 axis is
  deliberately not modelled yet — accepting a key it cannot enforce
  would be a lie);
- zero or two constraint keys on one argument, or `absent = false`;
- a `range` with neither bound;
- a `path-prefix` / `windows-path-prefix` that normalizes to nothing
  (above);
- a `principal` or `tool` that is empty or holds a control character;
- an `expires` without a UTC offset (`2026-09-01T00:00:00` means a
  different instant in every time zone; write `…Z` or `…+02:00`), or one
  that is only a date or only a time;
- the same `(tool, argument)` constrained both as a path and as bytes, or
  in two different path flavors.

Warned about and kept, because each is a grant that can only *deny* —
which costs availability, never authority:

- a grant naming a tool no upstream offers (reported once every upstream
  has been listed);
- an empty `one-of`;
- a `range` whose `min` is above its `max`;
- a `principal` set in a file with no grants.

Warned about and kept for the opposite reason — these are your own bytes,
so they are what you asked for, but they constrain nothing:

- an empty `prefix` or `suffix`, which admits every string value of that
  argument.

### What the client sees when a call is denied

Two shapes, and the difference is deliberate.

| Situation | Client sees |
|---|---|
| No grant names the tool, or every grant for it has expired | `-32602 Unknown tool: x` — **byte-identical** to a tool no upstream offers. An expired grant is no grant, and the filtered `tools/list` agrees with it. |
| A live grant names the tool, but the arguments are outside it (or the engine could not evaluate the call) | A successful JSON-RPC response carrying a tool error: `isError: true`, `"denied by policy"`. Agent-visible and recoverable — it named a tool it holds and may try again inside the envelope — and it leaks nothing about what the envelope is. |

Neither reaches the upstream. Both are logged (`WARN`) and traced.

## 5. stdout, stderr, and logging

- **stdout is the MCP wire.** Only newline-delimited JSON-RPC frames
  are written to it. Nothing else — no banner, no logs — because the
  client on the other end is parsing it.
- **stderr is for humans**: the proxy's own log lines, plus whatever
  the spawned stdio upstreams write to *their* stderr (inherited).
  Clients that capture MCP server stderr (Claude Desktop writes it to
  `mcp-server-<entry name>.log`) capture both.
- **Log level** comes from `RUST_LOG`, default `info`. Examples:
  `RUST_LOG=debug` for everything (per-call decisions and per-frame
  drop/discard reasons live at `debug`); `RUST_LOG=flavium_proxy_mcp=debug`
  for the proxy crate only; `RUST_LOG=warn` for problems only. Set it in
  the client's server-entry environment (`"env": {"RUST_LOG": "debug"}`
  in Claude Desktop's config) — the proxy is normally not launched from a
  shell you control.
- **What is logged**: the principal and grant count at startup; the
  handshake on each face (offered/negotiated protocol version, client and
  server names/versions); each upstream spawn/connect; the tool count;
  **every denial**, with the tool, the call id, the time used and the
  reason; every session-ending condition; and the end-of-session summary
  (frames forwarded, delivered, undelivered, rejected, discarded). Never
  logged: header values, URL userinfo/query strings, frame contents, or
  argument values — those go to the trace file, which has file
  permissions of its own.

## 6. The trace file

`--trace <FILE>` appends one JSON object per line, flushed per event,
`0600` on unix. **The format is unstable.** Every line carries `"v": 1`,
but T4 publishes the trace as a versioned specification and will change
this shape; nothing should parse it as a contract yet.

Every line has `v`, a dense monotonic `seq`, a wall-clock `ts` in Unix
milliseconds, a session id (`<start-secs>-<pid>`), and an `event`:

| `event` | When | Notable fields |
|---|---|---|
| `session_started` | Once, after every upstream is up | `principal`, `grants` — the policy in force, so every later `allow` index can be read against it |
| `handshake_completed` | The client's `initialize` was answered | offered and negotiated protocol version, the client's self-reported name/version (untrusted, informational) |
| `tools_listed` | Each `tools/list` | `offered`, `granted`, and the `now` the filter used |
| `call_refused` | A `tools/call` refused before any decision | `tool` (when it could be read), `reason`: `malformed_params`, `unknown_tool`, `duplicate_request_id` |
| `call_decided` | Every authorized call | `tool`, `args` **as evaluated**, `args_as_sent` when normalization changed a value, `now`, and `decision` |
| `call_completed` | Every allowed call, exactly once | `outcome`: a result (with `is_error`), an error code, `not_forwarded`, `cancelled`, or `abandoned` |
| `frame_rejected` | A client frame failed at the parse boundary | the JSON-RPC `code` sent back |
| `frame_discarded` | A frame the router consumed without forwarding or answering | `kind` |
| `upstream_ended` | An upstream connection ended | `upstream`, and the failure if it was one |
| `session_ended` | Last | `reason`, `undelivered`, `delivery_failed` |

Three things about `call_decided` are worth knowing:

- **The arguments are the ones the decision was made on** — normalized
  (§4), with values the core does not model recorded as a bare type tag.
  A record that disagreed with the decision it records could not
  reproduce it.
- **`args_as_sent` says what was asked for**, because the evaluated form
  cannot: normalization is lossy, so `/data/x/../y` and `/data/y`, or two
  case spellings of one Windows path, evaluate to the same value and
  would otherwise be the same line. It holds the caller's own spelling
  for the arguments where the two differ — and only those, so the key is
  absent from most lines and its presence means normalization changed
  something. Same 4 KiB cap as any other value.
- **A string argument longer than 4 KiB is truncated**, and then carries
  its full byte length and the SHA-256 of the whole value. An argument can
  be a megabyte of document text, and an audit log must not become a copy
  of the data plane. 4 KiB is `PATH_MAX`, so *a path argument is never
  truncated* — "which file did it read?" is the question this record
  exists to answer, and a digest does not answer it. Below the cap
  nothing is hashed: the plaintext is right there, and a short
  low-entropy value would be enumerable from its digest.

**A sink failure ends the session.** If a line cannot be written the
proxy stops answering, tears down, and exits non-zero. A full disk should
stop the agent, not run it unrecorded.

`--unenforced` refuses `--trace`, because there is nothing honest to
write: the session's first event is the envelope in force, and recording
an empty one for a session that allowed everything would be a false
statement in the audit record.

## 7. Exit codes

| Code | When |
|---|---|
| `0` | The banner/`--help`/`--version` paths; and a proxy session that ended **cleanly**: the client closed its input (normal MCP shutdown) *and* every frame accepted for the client was delivered. Logged as `session closed cleanly`. |
| `1` | Any of: the upstream/grant source is missing or refuses (§3, §4); a startup error (`proxy failed: <error chain>` — §8); or the session ended **abnormally** (`session ended abnormally` with the summary): an upstream exited or failed, an HTTP session expired, the client stopped reading, a re-list introduced a tool-name collision, the trace sink failed, or an internal task died. Until supervision lands (T3), *any* upstream ending ends the session — the proxy prefers exiting loudly to serving a tool surface that silently shrank. |
| `2` | Command-line usage error from the argument parser (unknown flag, `--config` together with `-- <COMMAND>`, `--unenforced` together with `--trace`). |

## 8. Startup errors and what they mean

Startup problems are printed as one line, `proxy failed: <error>:
<cause>: <cause>…`, with the full chain — the outermost error names
the upstream, the innermost says what actually happened. Config and
grant errors (§3, §4) are printed on their own, without the prefix.

| Error chain contains | Meaning | Usual fix |
|---|---|---|
| `cannot compile grants: …` | A grant survived validation but the policy engine refused it. | Read the cause; it names the grant. |
| `cannot open trace file <path>: …` | `--trace` points somewhere unwritable (or at a directory). | Pick a writable path. |
| ``could not be started: failed to spawn upstream `prog`: …`` | The stdio command's program could not be started — not on `PATH`, not executable, or (Windows) a `.cmd` shim invoked without the extension. The OS error follows. | Use an absolute path, or `npx.cmd` on Windows. |
| `failed to connect: upstream did not complete initialize in time` | No `initialize` response within 60 s. Common with `npx -y …` on a cold cache. | Run the upstream once by hand to warm the cache; check it speaks MCP on stdio. |
| `failed to connect: upstream closed during initialize` | The child exited (or the pipe closed) before answering. Its own stderr, just above, usually says why. | Fix the upstream's arguments/environment. |
| `failed to connect: upstream refused initialize with error -32603: upstream request failed` | **HTTP upstream unreachable or unusable**: the `initialize` POST failed — connection refused, DNS, TLS, a redirect (refused on purpose), a non-success status, or a body that was not a JSON-RPC response. The transport turns a failed POST into a synthesized error response, which is why it reads as a refusal; the real reason is in the `WARN … upstream POST failed … reason=…` line just above it. | Check URL, headers, network, TLS. |
| `failed to connect: upstream refused initialize with error <other code>: …` | The server itself answered `initialize` with a JSON-RPC error. | Read the message; it is the server's. |
| `failed to connect: upstream negotiated unsupported protocol version "…"` | The server insists on a revision the proxy does not speak (it accepts `2025-06-18` and `2025-11-25`; batching-era `2025-03-26` and older, and the 2026-07-28 revision, are refused). | Use a server on a supported revision. |
| `failed to connect: transport failed during initialize: …` | The transport itself died during the handshake: for HTTP, the server ended the session (404 under a session id) or the ordered `initialized` POST failed; for stdio, an I/O error on the pipes. | See the cause after the last colon and the surrounding log lines. |
| `failed to list tools: …` | `initialize` succeeded but `tools/list` failed, did not parse, paged past 1 000 pages, declared more than 10 000 tools, or exceeded the byte budget. | The upstream is misbehaving; check its output with the MCP Inspector. |
| `tool "x" is offered by both "a" and "b"` | Name collision across (or within) upstreams. | Remove one, or wait for namespacing. |

A `WARN … a grant names a tool no upstream offers` line means exactly
that: the grant is kept (it can only deny), but it is almost always a
typo in `tool`.

If the client (Claude Desktop, Claude Code) shows the server as failed
without more detail, the proxy's stderr — captured in the client's MCP
log — has the line above.

## 9. Wiring a client

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
      "args": [
        "proxy",
        "--config", "/absolute/path/to/flavium.toml",
        "--trace", "/absolute/path/to/flavium-trace.jsonl"
      ],
      "env": { "RUST_LOG": "info" }
    }
  }
}
```

Transparent, no grants (the M1/M2 behaviour, on purpose):

```json
"args": ["proxy", "--unenforced", "--", "npx", "-y", "@modelcontextprotocol/server-filesystem", "/data"]
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
filtered tool list and the negotiated protocol version.

The recorded runs against real clients — what was checked and what
they answered — are in
[docs/tasks/v0.1/T1-demo.md](tasks/v0.1/T1-demo.md).

## 10. Fixed limits

Compiled in for now (`ProxyConfig::default()`); a `--config` knob for
these is not implemented. Full table with sources in the
[architecture doc, §9](architecture/proxy-mcp.md#9-limits-and-tuning-knobs).

| Limit | Value |
|---|---|
| Max frame size, both directions, all transports (also the per-upstream tool-table byte budget) | 16 MiB |
| Longest string argument recorded whole in the trace | 4 KiB (`PATH_MAX`) |
| Per-upstream `initialize` deadline | 60 s |
| Per-page `tools/list` deadline | 30 s |
| Shutdown grace for actors and the client writer | 5 s |
| Time a stdio child gets to exit after its stdin closes, before it is killed | 5 s |
| One frame write to a child may stall before the pipe is declared dead | 30 s |
| `tools/list` pages / tools accepted per upstream | 1 000 / 10 000 |
| HTTP connect timeout · session `DELETE` budget · GET-stream reconnect backoff | 10 s · 2 s · 1–30 s |
| Protocol revisions offered / accepted | `2025-11-25` / `2025-06-18`, `2025-11-25` |

## 11. Not yet

Stated so nobody hunts for a flag that does not exist:

- **No budgets** — T2. A `budget` key in a grant is an error, not a
  silently ignored field.
- **No delegation or sub-agents** — T3.
- **No hash-chained recorder, no replay, no published trace spec** — T4.
  The JSONL of §6 is unstable.
- **No redaction or truncation knobs** for the trace — the 4 KiB cap is
  fixed; making it configurable is T4's business.
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
