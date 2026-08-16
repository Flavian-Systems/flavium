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
  with every call crossing — and, since M5, being authorized and traced
  at — the seam in between. Since M2 the proxy **terminates** the
  protocol: it answers `initialize` itself rather than relaying another
  server's handshake.
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

- **Grant** — an unforgeable authorization over one tool: argument
  constraints + expiry (+ budget, T2). The unit of authority in flavium
  (`flavium_core::Grant`, M3); DESIGN's tuple (principal, tool,
  constraints, expiry, budget) is the envelope's principal × the grant.
  Cedar-backed and enforced on every call since M5.
- **Grant file** — the operator-written `flavium.toml`: a `version`, a
  `principal`, the `[[upstream]]` tables, and one `[[grant]]` table per
  authority granted. It is the *only* language flavium asks anyone to
  write — Cedar is an implementation detail of enforcing it. One
  constraint key per argument, unknown keys refused, and a file with no
  grants refuses to start (see *unenforced mode*). Everything in it is a
  security decision written by hand, so every ambiguity is a startup
  error rather than a mid-session denial that reads like policy.
- **Unenforced mode** — `flavium proxy --unenforced`: the M1/M2
  transparent middlebox, kept as an explicit choice. Every upstream tool
  is exposed, every call forwarded, and **nothing is traced** — the
  session's first trace event is the envelope in force, and recording an
  empty one for a session that allowed everything would be a false
  statement in the audit record. It exists so that "transparent" is
  something an operator asked for, never something a missing config
  section produced.
- **Constraint** — the argument-level part of a grant, one per argument
  name (`flavium_core::Constraint`): `Prefix`, `Suffix`, `OneOf`,
  `Range` (inclusive `i64` bounds), and `Absent` (the argument must not
  be supplied — how `cc`/`bcc` get closed). Byte-wise, fail closed: a
  constrained argument that is missing, of the wrong type, or of an
  unmodelled shape is not admitted; arguments no constraint names are
  not examined (an authoring pitfall: constrain every argument that
  matters).
- **Path normalization** — rewriting a path-valued argument into its
  canonical form (separators unified and collapsed, `.` dropped, `..`
  resolved, never escaping the root) *before* its constraint is checked,
  so the check is about the resource rather than the spelling. Opt-in per
  argument in the grant file (M5): only the path-flavored constraints
  (`path-prefix`, `windows-path-prefix`) normalize, because normalizing a
  value that is not a path silently changes what the decision was about.
  Without it, `Prefix("/data/")` admits `/data/../etc/passwd` — a byte
  prefix match whose resource is `/etc/passwd`. The decision *and the
  trace* use the normalized value; the forwarded frame keeps the client's
  original bytes. Normalization must never *widen*: a prefix keeps a
  trailing separator its author wrote (dropping it would make
  `/data/invoices/` also admit `/data/invoices.bak`), a prefix that
  reduces to nothing (`.`, `./`) is refused rather than compiled to the
  everything-prefix, and a **leading** run of two or more separators is
  preserved rather than collapsed — on Windows `\\host\share` is another
  machine, and POSIX leaves a leading `//` implementation-defined. No case
  folding, no filesystem access, no symlink resolution — those are outside
  what a proxy can see.
- **Path flavor** — which characters a path-flavored constraint treats as
  separators: POSIX (`/` only) or Windows (`/` and `\`). Declared per
  grant rather than guessed, because `\` is an ordinary filename byte on
  POSIX and *the* separator on Windows, so either fixed answer is a false
  allow on the other platform. A grant names one tool and a tool belongs
  to one upstream, so the grant is the scope at which the answer is known.
- **UNC root** — a Windows path beginning `\\host\share`, naming a share
  on another machine. Flavium keeps it distinct from a drive-rooted path
  (`\data\…`, the current drive) rather than collapsing the separator
  run, because a grant over a local directory that admitted the UNC
  spelling of the same first segment would let an agent write to a remote
  server — and hand that server the upstream's credentials.
- **Admits** — the "does this pass?" direction, over one *value*. A
  constraint admits an argument value (`Constraint::admits`); a grant
  admits a call when the call names its tool and every constraint admits
  the call's value for its argument (`Grant::admits`), a missing argument
  being `None`. Total and fail closed: every (constraint, value) pair
  yields a `bool`, and missing, wrong-typed and unmodelled values are not
  admitted.
- **Includes** — the "is this narrower?" direction, over one *argument*:
  does the parent's constraint admit every value the child's admits
  (`Constraint::includes`)? A structural table, not a solver — see
  *attenuation* for why sound-but-incomplete is the deliberate choice.
- **Covers** — the same question over one *grant*: is everything the
  child grant authorizes also authorized by the parent (`Grant::covers`)?
  Three axes in a fixed order — tool, expiry, then, for every argument
  the **parent** constrains, that the child constrains it too and the
  parent's constraint *includes* the child's. A child may add
  constraints; it may never drop or widen one. `attenuates` is this,
  lifted to sets: every child grant must be covered by at least one
  parent grant.
- **Grant envelope** — the union of an agent's grants: the precomputable
  worst case of what it can do (`flavium_core::GrantEnvelope`:
  principal + grants, in file order).
- **Decision** — the outcome of authorizing one call
  (`flavium_core::Decision`): `Allow { grant }` (index of the first
  live grant that admits it) or `Deny(reason)` with `NotGranted`,
  `Expired`, `OutOfEnvelope`, or `EvaluationError` (engine failure,
  fail closed). `decide` in flavium-core is the reference semantics the
  runtime engine is tested against.
- **Attenuation** — authority only ever narrows as it flows down. The
  core invariant (`flavium_core::attenuates`, M3): a child's grant set
  is a subset (⊆, equality allowed) of its parent's on every axis —
  tool, expiry, every constrained argument. The check is sound and
  conservative: it never accepts a child that could do something the
  parent cannot; it may refuse a child that must be written more
  explicitly. M2 already practices it at the protocol level: flavium
  declares no capabilities upstream, closing the server→client request
  channel.
- **Delegation** — a parent agent spawning a sub-agent with (strictly
  attenuated) grants; T3 work.
- **Budget** — a quantitative cap (tokens, spend, calls, wall-clock)
  enforced mid-execution; T2 work.
- **Namespace** — per-agent renaming/virtualization of what an agent
  can even name; v0.1 scope, after T1.
- **Trace / flight recorder** — the append-only record of every call,
  decision, denial, budget tick, spawn, and termination. The
  vocabulary is `flavium_core::TraceEvent` (M3; clock-free — a
  decision carries the `now` it used) behind the `TraceSink` trait;
  since M5 the CLI emits JSONL to `--trace <file>`; the hash-chained
  SQLite recorder with deterministic replay is T4.
- **Trace record (JSONL line)** — one event as M5 writes it: the
  recorder's four fields (`v`, a dense monotonic `seq`, a wall-clock
  `ts`, a session id — everything the clock-free event deliberately
  leaves out), then the event and its own fields. A decision records the
  call **as evaluated** — normalized, with unmodelled values as a bare
  type tag — because a record that disagreed with the decision it records
  could not reproduce it. A string argument past 4 KiB (`PATH_MAX`, so a
  path is never truncated) is cut and carries its full length and
  SHA-256; below the cap nothing is hashed, since a short low-entropy
  value is enumerable from its digest while the plaintext is already
  there. The format is **unstable** until T4 publishes it as a spec.
- **Sink failure** — a `TraceSink::record` that returns an error. The
  session ends and the process exits non-zero: a full disk should stop
  the agent, not run it unrecorded. It is the other half of `record`
  being fallible at all, and the one failure that is *not* treated like
  an engine failure — an engine that cannot evaluate denies the call and
  the session continues, because authority held.
- **Authorizer** — the seam the proxy asks "may this principal make this
  call now?" (`flavium_core::Authorizer`, M3): a trait with no I/O and
  no clock, so every answer is replayable. The runtime implementation is
  Cedar-backed (`flavium-policy`, M4); the implementation on
  `GrantEnvelope` is the *reference* one — see *reference semantics*.
- **Reference semantics** — `flavium_core::decide`: the small, boring
  function that *defines* what a grant means. It is the specification
  the runtime engine is measured against (see *differential test*), not
  the engine itself. When the two disagree, the engine has the bug.
- **Denial surface** — the pinned, client-visible shape of every
  refusal. Notably: a tool outside the table answers `-32602` with
  byte-identical bytes to a tool outside the grant envelope — denial is
  indistinguishable from nonexistence, which is what makes the filtered
  `tools/list` consistent rather than an oracle. Out-of-envelope
  *arguments* on a granted tool get the other shape: a successful
  response carrying `isError: true` and `denied by policy`, which the
  agent can act on and which says nothing about the envelope.
- **False allow** — flavium answered `Allow`, but the effect landed
  outside the grant. The security failure: it breaks the claim that an
  agent's worst case is the union of its grants. It arises wherever the
  runtime's model of a call diverges from what the upstream actually does
  with it — the argument's *spelling* versus the *resource* it resolves to
  — which is why it is the failure mode prompt injection reaches for: no
  new credential is needed, only a spelling the checker and the upstream
  read differently.
- **False denial** — flavium answered `Deny`, but the effect would have
  been inside the grant. Nothing escapes; a legitimate task just fails.
  The two errors are deliberately not treated as equals. A false denial
  announces itself — someone complains, and the fix is one line of grant
  file — while a false allow is silent by construction and is discovered,
  if ever, as an incident. That difference in *detectability* matters as
  much as the difference in cost, and is why "we have seen no problem" is
  not evidence of correctness.
- **Fail closed** — when input cannot be handled exactly, refuse it;
  never repair, guess, or forward it. Unparseable frames are rejected
  or dropped, evaluation errors deny, invalid encodings are refused
  rather than patched. Stated in terms of the two errors above: fail
  closed is the standing choice of a *false denial* over a *false allow*
  wherever the honest answer is "I cannot tell". It is why a constrained
  argument that is missing, wrong-typed or unmodelled is not admitted, why
  an empty `OneOf` and a `min > max` range admit nothing, and why
  `attenuates` is sound but not complete — it may refuse a child grant set
  that genuinely is a subset, but never accepts one that is not.

## Policy engine (Cedar)

The terms below are Cedar's, as flavium uses them (M4,
`flavium-policy`). Cedar is the mandated engine; flavium never asks a
user to write Cedar — grants are compiled into it.

- **Cedar** — the open-source authorization policy language and engine
  (`cedar-policy`, pinned 4.12) flavium evaluates every tool call
  against. Chosen because it has a formal semantics and a mechanised
  model behind it (DESIGN §6), so the policy-evaluation half of the
  verification story is someone else's proven work.
- **Policy** — one authorization rule. It has an **effect** (`permit` or
  `forbid`), a **scope** naming the principal, action and resource it
  applies to, and optional **conditions** (`when { … }` clauses).
  Flavium compiles **one `permit` policy per grant**; it never emits
  `forbid`, because deny-by-default is the whole model — anything no
  permit covers is already denied.
- **Policy set** — the collection of policies evaluated together for one
  request; flavium compiles a grant envelope into exactly one policy
  set, once, at startup.
- **Policy id** — a policy's name inside the set, unique by
  construction. Flavium uses **the grant's index in the envelope**, so
  Cedar's answer can be mapped straight back to the grant that allowed
  the call (`Decision::Allow { grant }`).
- **Entity / entity UID** — Cedar's addressable things, written
  `Type::"id"` (`Flavium::Principal::"invoice-bot"`,
  `Flavium::Tool::"read_file"`). Flavium builds every UID from
  structured JSON, never by formatting text — an id containing `"` or
  `\` is legal data and must not be able to alter a policy's meaning.
- **Authorization request** — the question put to the engine: a
  (principal, action, resource, context) quadruple. Flavium asks
  "may *principal* perform `Flavium::Action::"call"` on *tool*, given
  *this call's arguments*". Distinct from a JSON-RPC *request*.
- **Context** — the per-request data policies may read. Flavium's
  context is always four keys: `str` and `int` (the call's arguments by
  type), `present` (the names of every argument supplied, which is how
  `Absent` is expressed), and `now`. Always emitting all four is what
  keeps evaluation from erroring on a missing attribute. It is built
  from Cedar `RestrictedExpression`s rather than from JSON, because
  Cedar's JSON *value* grammar reserves `__expr`/`__entity`/`__extn`:
  argument names come from the client, so a name that a parser treats as
  a keyword must never reach one.
- **`has` guard** — Cedar's attribute-existence test
  (`context.str has path`). Every constraint that reads an argument's
  *value* is guarded by one, so an argument that is missing or of the
  wrong type fails the guard and denies, rather than raising an
  evaluation error. (`Absent` needs no guard: it asks whether a name is
  in `present`, which is a set membership test and cannot error.)
- **`like` / wildcard pattern** — Cedar's string matcher, where `*`
  matches any sequence. Prefix and suffix constraints compile to it. The
  pattern is emitted **structurally** (a literal plus a wildcard), so
  Cedar escapes the literal itself and a `*` inside a grant is matched
  as a plain character.
- **JSON policy format (EST)** — Cedar's machine-oriented policy
  representation, parsed by `Policy::from_json`. Flavium's compiler
  emits this rather than Cedar source text: there is no string
  interpolation anywhere on the path from a grant to a policy, so no
  grant value can ever be read as syntax.
- **Determining policies** — the policies that caused the answer,
  reported in the response's **diagnostics** alongside any evaluation
  **errors**. Several permits may match one call; the set is unordered,
  so flavium takes the lowest grant index to reproduce the reference
  semantics' "first admitting live grant".
- **Evaluation error** — a policy that could not be evaluated (a missing
  attribute, a type mismatch). Flavium's encoding makes these
  unreachable by construction, and treats any that appear as
  `Deny(EvaluationError)` — the engine failing is never a reason to
  allow.
- **Schema** — Cedar's optional type declaration for entities and
  context, used to validate policies ahead of time. Flavium deliberately
  does **not** use one: its policies are generated from a closed
  vocabulary rather than written by users, the `has` guards already
  remove the errors a schema would catch, and a schema would have to
  declare every argument name in every grant file.
- **Grant compiler** — flavium's half of the bargain
  (`flavium-policy::compile`): grant envelope → policy set. It runs once
  per session; a grant that cannot be compiled stops startup rather than
  failing mid-session.
- **Differential test** — the test that runs the engine and the
  reference semantics over the same random envelopes, calls and times
  and asserts the *same* `Decision`, index and reason. It is how "Cedar
  agrees with the specification" stops being a claim.

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
