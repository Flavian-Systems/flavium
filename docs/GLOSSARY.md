# Glossary

The vocabulary used across flavium's docs, code, and traces — MCP terms
as this project uses them, plus flavium's own. [DESIGN.md](../DESIGN.md)
is the source of truth for architecture; this file only fixes the words.

## Roles and topology

- **Agent** — the AI system doing work on someone's behalf. It holds
  credentials and authority; flavium exists to bound what it can do
  with them. From flavium's perspective the agent sits behind an MCP
  client.
- **Client (MCP client)** — the program that connects *to* an MCP
  server and issues requests (Claude Desktop, Claude Code, an agent
  runtime). Flavium presents as a server to the client; the client
  needs no modification — that is the point.
- **Server (MCP server)** — a program exposing tools over MCP. Flavium
  is a server on one face and a client on the other.
- **Proxy / middlebox** — flavium's position in the topology: an MCP
  *server* toward the client, an MCP *client* toward each upstream,
  with every call crossing (and, from M4/M5 on, being authorized and
  traced at) the seam in between. Since M2 the proxy **terminates** the
  protocol — it answers `initialize` itself rather than relaying
  another server's handshake.
- **Upstream** — one tool server the proxy fronts: a spawned child
  process (stdio) or a streamable-HTTP endpoint. Named in config
  (`[[upstream]] name = "fs"`); the name appears in logs and errors
  only, never in tool names.
- **Multi-upstream** — one proxy session fronting several upstreams at
  once: their tool lists are merged into one, and each `tools/call` is
  routed to the upstream that owns the tool name.
- **Principal** — the identity a call is attributed to for
  authorization and tracing. In T1 it is static per proxy process,
  from config; `clientInfo` is untrusted data and never identity.

## Protocol

- **MCP (Model Context Protocol)** — the JSON-RPC-based protocol
  between clients and tool servers. Flavium speaks revisions
  2025-06-18 and 2025-11-25 (`protocol.rs`); the batching-era
  revisions (2025-03-26 and older) are deliberately unsupported.
- **Frame** — one wire unit carrying one JSON-RPC message: a
  newline-delimited line on stdio, an HTTP body or SSE event payload
  over streamable HTTP. Frames are size-capped everywhere
  (`max_frame_bytes`, default 16 MiB).
- **Envelope** — the typed parse boundary over a frame
  (`envelope.rs`): just deep enough to classify the message and read
  its id/method, leaving `params`/`result` as raw bytes. Anything that
  does not parse as a single well-formed JSON-RPC 2.0 object is
  rejected (fail closed).
- **Request / notification / response** — the three JSON-RPC message
  kinds: a request has `method` + `id` and expects a response; a
  notification has `method` only and expects nothing; a response
  answers an id with exactly one of `result`/`error`.
- **Session** — one client's connection lifetime through the proxy,
  from its `initialize` to EOF/teardown. Each upstream also has its own
  session with the proxy (over HTTP, tracked by `MCP-Session-Id`).
- **Handshake** — the `initialize` request/response followed by the
  `notifications/initialized` notification. Since M2 there are two per
  proxied session: client↔flavium and flavium↔each upstream.
- **Version negotiation** — the client offers a protocol revision in
  `initialize`; the server answers with the revision the session will
  run. Flavium echoes a supported offer, otherwise answers with the
  newest revision it speaks; an upstream that negotiates an unsupported
  revision is refused at startup.
- **Capability** — a feature set declared during the handshake (tools,
  resources, prompts, sampling, …). Flavium advertises to the client
  only what it honors (`tools.listChanged`) and declares **no**
  capabilities upstream — see *attenuation*.
- **Instructions** — the free-text usage hint a server may return from
  `initialize`. With several upstreams, flavium labels and concatenates
  them; with one, it passes them through verbatim.
- **Ping** — the liveness request either side may send; the proxy
  answers pings itself on both faces (they are about *its* liveness).

## Tools

- **Tool** — a named, callable capability an upstream exposes
  (`read_file`, `send_mail`). Flavium forwards tool objects
  byte-identically — it routes by `name` and does not interpret the
  rest.
- **Tool table (merged list)** — flavium's session-wide table of every
  upstream's tools, built at startup and rebuilt on `list_changed`.
  `tools/list` serves it as one unpaginated list in upstream order.
  Byte-budgeted per upstream (one frame's worth) on top of page and
  count caps.
- **Collision** — the same tool name offered twice (across upstreams or
  within one). Ambiguous routing is ambiguous authority, so collisions
  are refused: at startup the proxy exits; mid-session (a re-list) the
  session ends. Namespacing is the documented follow-up, not T1 work.
- **Pagination / cursor** — how an upstream splits a long `tools/list`
  across pages (`nextCursor`). The proxy drains pagination internally
  and never mints a cursor of its own, so any cursor a client sends is
  foreign by definition and answered with `-32602`.
- **`list_changed`** — the notification a server emits when its tool
  set changed. The proxy intercepts an upstream's, re-lists that
  upstream, re-merges, and emits its *own* `list_changed` to the
  client.

## Ids and correlation

- **Request id** — JSON-RPC's correlation key (integer or string),
  unique among a sender's in-flight requests.
- **Minted (upstream) id** — the integer id flavium assigns when
  forwarding a request to an upstream. Each upstream connection has its
  own id space, so client ids never leak upstream and two upstreams'
  ids can never be confused — a malicious upstream cannot forge a
  response to a request it was never sent.
- **Id translation** — the rewrite between the client's id space and
  each upstream's (`idmap.rs`, `splice.rs`): outbound the minted id is
  spliced into the frame; inbound the client's original id bytes are
  restored. Everything else crosses byte-identically.
- **In-flight** — a request forwarded but not yet answered. The router
  tracks in-flight client ids (duplicates are refused with `-32600`);
  each connection tracks its pending minted ids.
- **Cancellation** — `notifications/cancelled` naming an in-flight
  request. The proxy rewrites its `requestId` into the upstream's id
  space and forgets the mapping, so the **late response** — one arriving
  after the cancel — no longer translates and is dropped. Best-effort by
  specification.
- **Progress token** — the client-chosen token (`_meta.progressToken`)
  a server echoes in `notifications/progress`. The proxy forwards
  progress only for tokens it actually sent to that upstream — the same
  containment responses get from minted ids.

## Transports

- **stdio transport** — MCP over a child process's stdin/stdout,
  newline-delimited frames, logs on stderr. How Claude Desktop launches
  flavium, and how flavium launches `command`-type upstreams.
- **Streamable HTTP** — MCP over HTTP against a single endpoint URL:
  every client→server frame is a POST; responses arrive as one JSON
  body or an SSE stream; an optional long-lived GET stream carries
  unsolicited server messages. How flavium reaches `url`-type
  upstreams.
- **SSE (Server-Sent Events)** — the `text/event-stream` format used
  by streamable HTTP for streaming. Flavium parses it with its own
  bounded parser (`sse.rs`) — a fuzz-ready trust boundary like the
  stdio framing.
- **`MCP-Session-Id`** — the header a streamable-HTTP server may assign
  at `initialize`; flavium echoes it on every subsequent request. A
  later 404 means the server ended the session — fatal in T1/M2.
- **`MCP-Protocol-Version`** — the header flavium sends on every
  post-handshake HTTP request, carrying the negotiated revision.

## Enforcement (the vocabulary the rest of v0.1 builds on)

- **Grant** — an unforgeable authorization: principal + tool +
  argument constraints + expiry (+ budget). The unit of authority in
  flavium; lands in M4/M5 (Cedar-backed).
- **Constraint** — the argument-level part of a grant: path prefixes,
  recipient/domain patterns, numeric ranges — evaluated on every call.
- **Grant envelope** — the union of an agent's grants: the precomputable
  worst case of what it can do.
- **Attenuation** — authority only ever narrows as it flows down. The
  core invariant (`attenuates()`, M3): a child's grant set is a subset
  of its parent's on every axis. M2 already practices it at the
  protocol level: flavium declares no capabilities upstream, closing
  the server→client request channel.
- **Delegation** — a parent agent spawning a sub-agent with (strictly
  attenuated) grants; T3 work.
- **Budget** — a quantitative cap (tokens, spend, calls, wall-clock)
  enforced mid-execution; T2 work.
- **Namespace** — per-agent renaming/virtualization of what an agent
  can even name; v0.1 scope, after T1.
- **Trace / flight recorder** — the append-only record of every call,
  decision, denial, budget tick, spawn, and termination. T1 emits
  JSONL behind a swappable sink; the hash-chained SQLite recorder with
  deterministic replay is T4.
- **Denial surface** — the pinned, client-visible shape of every
  refusal. Notably: a tool outside the table answers `-32602` exactly
  like a tool outside the grant envelope will (M5) — denial is
  indistinguishable from nonexistence.
- **Fail closed** — when input cannot be handled exactly, refuse it;
  never repair, guess, or forward it. Unparseable frames are rejected
  or dropped, evaluation errors deny, invalid encodings are refused
  rather than patched.

## Inside the proxy (module vocabulary)

- **Router** (`router.rs`) — the session brain: the client-facing
  state machine, dispatch, the tool table, and in-flight tracking. It
  never blocks on an actor's queue (a saturated upstream answers
  `-32603 upstream busy`).
- **Connection actor** (`connection.rs`) — one task per upstream owning
  its transport, handshake, minted-id space, and progress-token scope;
  talks to the router via commands in / events out.
- **Transport** (`transport.rs`, `http.rs`) — the frame-in/frame-out
  seam under an actor: stdio child or streamable HTTP. Writes run on a
  dedicated writer task with a stall deadline, so a wedged peer fails
  loudly instead of deadlocking the session.
- **Writer task** — the single owner of an outbound byte stream (the
  client's stdout, a child's stdin); everything funnels through it so
  frames never interleave mid-write.
- **Frame cap (`max_frame_bytes`)** — the per-frame size bound applied
  on every read path (default 16 MiB) and used as the per-upstream
  tool-table byte budget.
