# `flavium-proxy-mcp` — architecture

**As of T1/M2 (2026-08).** This document describes how the MCP proxy
crate is built: its modules, its tasks and channels, the shape of every
message flow, and the invariants the code is written to hold. It is a
map for readers and reviewers, not a specification —
[DESIGN.md](../../DESIGN.md) is the source of truth for the system, and
[the T1 plan](../tasks/v0.1/T1-mcp-proxy-core.md) for what this crate
must deliver. Where this document and the code disagree, the code is
right and this document has a bug. Vocabulary is fixed in
[GLOSSARY.md](../GLOSSARY.md).

M3 added the types, traits and trace vocabulary in `flavium-core`; M4
and M5 will add the engine and the enforcement hook (§10). The flows
below gain a step; their shape does not change.

## Contents

1. [What the crate does](#1-what-the-crate-does)
2. [Module map](#2-module-map)
3. [Task and channel model](#3-task-and-channel-model)
4. [Key types and who owns what](#4-key-types-and-who-owns-what)
5. [Session lifecycle](#5-session-lifecycle)
6. [Flows](#6-flows)
7. [Invariants](#7-invariants)
8. [Client-visible error surface](#8-client-visible-error-surface)
9. [Limits and tuning knobs](#9-limits-and-tuning-knobs)
10. [Where enforcement plugs in](#10-where-enforcement-plugs-in)
11. [Tests as executable specification](#11-tests-as-executable-specification)
12. [How the CLI uses the crate](#12-how-the-cli-uses-the-crate)

## 1. What the crate does

The proxy is an MCP **server** toward one client and an MCP **client**
toward *N* upstream tool servers. It terminates the protocol on both
faces: it answers the client's `initialize` itself, runs a separate
`initialize` against every upstream, drains and merges their tool
lists into one, and routes each `tools/call` to the upstream that
declared the tool. Request bodies (`params`, `result`, `error`) cross
the proxy as the original bytes; only JSON-RPC ids (and a
cancellation's `requestId`) are rewritten, because each upstream is
spoken to in an id space the proxy mints itself.

```mermaid
flowchart LR
    C[MCP client<br/>Claude Desktop, Claude Code, …]
    subgraph P[flavium proxy — one process, one session]
        R[router<br/>server face]
        A1[connection actor 0]
        A2[connection actor 1]
        A3[connection actor n]
        R --> A1
        R --> A2
        R --> A3
    end
    U1[(upstream 0<br/>stdio child)]
    U2[(upstream 1<br/>streamable HTTP)]
    U3[(upstream n)]
    C <-- stdin/stdout --> R
    A1 <-- pipes --> U1
    A2 <-- POST / SSE / GET --> U2
    A3 <--> U3
```

Nothing is enforced yet. M2's job was to be a correct, bounded,
fail-closed middlebox in front of real clients; grants, budgets, and
tracing land on top of it (§10).

## 2. Module map

Modules are listed bottom-up. Each layer depends only on layers above
it in this table (i.e. lower in the stack), never the reverse.

| Layer | Module | One line |
|---|---|---|
| Wire boundary | `framing` | `\n`-delimited frame reader/writer with a hard per-frame byte cap. Never looks inside a frame. Designated fuzz seam (T5). |
| | `sse` | Sans-I/O incremental Server-Sent Events parser with a per-event cap; refuses invalid UTF-8 rather than repairing it. Designated fuzz seam. |
| | `envelope` | Classifies one frame as request / notification / response; validates `jsonrpc`, ids, and shape; leaves `params`/`result`/`error` as `RawValue` borrows of the frame. Rejects batches. |
| | `splice` | Rewrites exactly one member's value inside a JSON object while preserving every other member's bytes and order. Rejects duplicate members. |
| | `builder` | Constructs the frames the proxy originates (results, errors, its own requests/notifications) and the fixed error-code table. |
| | `protocol` | Version policy: what the proxy offers (`2025-11-25`) and accepts (`2025-06-18`, `2025-11-25`), and its `serverInfo`/`clientInfo` name. |
| Transports | `transport` | The `Transport` enum (stdio or HTTP) with a uniform send/recv/close surface and a shared failure taxonomy; the `StdioTransport` implementation and its writer task; `normalize_newlines`. |
| | `http` | The streamable-HTTP client: one POST per frame, JSON or SSE response bodies, `MCP-Session-Id`, the optional GET listening stream, DELETE at close. |
| Protocol, per upstream | `idmap` | `PendingMap` (one per upstream: minted id → what is waiting) and `ClientTable` (one per session: client id → upstream index). All id translation lives here. |
| | `connection` | `connect()` runs the upstream handshake and spawns the connection **actor**, which owns one transport and one `PendingMap` and speaks `Command`/`Event` with the router. |
| | `toolset` | `ListPage` parsing and the merged, collision-checked `ToolSet` (name → upstream, plus each tool's raw bytes). |
| Session | `router` | The proxy's server face: startup (connect + list every upstream), the serve loop, client-face phase machine, `tools/list`/`tools/call` handling, re-lists, teardown, `SessionSummary`. |
| Wiring | `config` | Transport-agnostic `UpstreamSpec`/`TransportSpec` and structural validation; `redact_url`. |
| | `stdio` | Production entry point `serve()`: builds transports from specs and runs the router over this process's stdin/stdout. |
| | `lib` | Re-exports; `#![forbid(unsafe_code)]`. |

```mermaid
flowchart TD
    stdio --> router
    stdio --> config
    stdio --> transport
    stdio --> http
    router --> connection
    router --> idmap
    router --> toolset
    router --> transport
    router --> framing
    router --> envelope
    router --> builder
    router --> splice
    router --> protocol
    router --> config
    connection --> transport
    connection --> idmap
    connection --> envelope
    connection --> builder
    connection --> splice
    connection --> protocol
    transport --> http
    transport --> framing
    http --> sse
    http --> envelope
    http --> builder
    http --> transport
    http -. redact_url .-> config
    idmap --> envelope
    builder --> envelope
```

(`toolset`, `framing`, `sse`, `envelope`, `splice`, `protocol`, and
`config` depend on nothing else in the crate.)

(`transport` and `http` reference each other: `transport` wraps
`HttpTransport` in its enum; `http` uses `transport`'s shared error and
`Received`/`DropReason` types and `normalize_newlines`. They form one
layer.)

## 3. Task and channel model

One session is one call to `router::run`. Everything else is tasks it
spawns or that its actors spawn. All queues are bounded `tokio::mpsc`
channels of capacity 64.

```mermaid
flowchart LR
    subgraph caller["caller's task (router::run → Session::serve)"]
        S["serve loop<br/>select! over: client frames · upstream events · re-list joins"]
    end
    W["client writer task<br/>(one per session)"]
    S -- "to_client: mpsc&lt;Vec&lt;u8&gt;&gt;" --> W
    W -- write_frame --> CO[(client stdout)]
    CI[(client stdin)] -- FrameReader --> S

    subgraph up0["per upstream i"]
        A["connection actor i<br/>owns Transport + PendingMap"]
        SW["stdio writer task<br/>(stdio only)"]
        PT["POST task per frame +<br/>GET listen task (HTTP only)"]
        A -- "outbound: mpsc" --> SW
        SW -- write_frame --> CH[(child stdin)]
        CH2[(child stdout)] -- FrameReader --> A
        A -- "spawn per send" --> PT
        PT -- "inbound: mpsc" --> A
    end
    S -- "commands_i: mpsc&lt;Command&gt;<br/>try_send only" --> A
    A -- "events: mpsc&lt;Event&gt; (shared)" --> S

    RL["re-list tasks<br/>JoinSet&lt;RelistOutcome&gt;"]
    S -- spawn on ListChanged --> RL
    RL -- "Handle::call (may await)" --> A
```

Tasks, in the order they come to exist:

| Task | Spawned by | Owns | Ends when |
|---|---|---|---|
| **Connection actor** (one per upstream) | `connection::connect`, after the handshake succeeded on the caller's task | the `Transport`, the `PendingMap`, progress-token scope tables, one clone of the events sender | `Command::Shutdown`, its command channel closing, its transport ending/failing (then it emits `Event::Ended`/`Fatal`), or the router's event receiver being gone |
| **Stdio writer** (one per stdio upstream) | `StdioTransport::from_streams` | the child's stdin | outbound channel closes (shutdown) or a write fails/stalls past 30 s |
| **HTTP POST task** (one per sent frame) | `HttpTransport::send` | one in-flight request | the response body (JSON or SSE) is fully read, or the POST fails |
| **HTTP GET listener** (≤ 1 per HTTP upstream) | `HttpTransport::start_listening` after `initialized` | the long-lived server→client stream | 405 (server offers no stream), session expiry, or `close()` |
| **Client writer** (one per session) | `router::run`, once startup succeeded | the client-side `AsyncWrite` | the `to_client` channel closes, or a write fails (it then drains and counts the rest as undelivered) |
| **Re-list task** (transient) | `Session::schedule_relist` on `Event::ListChanged` | a `Handle` clone | its `tools/list` drain completes or fails |

Three rules keep this graph deadlock-free and bounded:

1. **The serve loop never awaits an actor's command queue.** It uses
   `Handle::try_send` only. An actor may be parked on `events.send`
   (its queue toward the router is full) while the router is parked on
   `commands.send` (the actor's queue is full) — that is a cycle. So a
   full actor queue answers the client `-32603 upstream busy` instead
   of waiting. `Handle::send`/`Handle::call` (which *do* await) are
   only used from independent tasks: startup and re-lists.
2. **Every client-bound frame funnels through the one writer task**,
   so frames never interleave mid-write and the serve loop never
   blocks on the client's stdout.
3. **Writes to a child never run on the actor's task.** The stdio
   writer task owns the child's stdin behind a bounded queue and a
   30 s stall deadline; the actor keeps draining the child's stdout
   while a write is pending, so a child that stops reading its stdin
   with full OS pipe buffers cannot wedge the session — it trips the
   deadline and fails loudly.

The events channel is shared: every actor holds a clone of the sender.
Each live actor reports its own end before dropping its sender, so if
the router ever sees the channel closed *without* such a report, an
actor died silently — surfaced as `SessionEnd::Internal`, a bug
signal, rather than a hang.

## 4. Key types and who owns what

```mermaid
classDiagram
    direction LR
    class Session {
        config: ProxyConfig
        handles: Vec~Handle~
        infos: Vec~UpstreamInfo~
        toolset: ToolSet
        phase: Phase
        in_flight: ClientTable
        relists: JoinSet~RelistOutcome~
        to_client: Sender~Vec u8~
    }
    class Handle {
        commands: Sender~Command~
        try_send()
        send()
        call()
    }
    class Actor {
        upstream: usize
        transport: Transport
        pending: PendingMap
        events: Sender~Event~
        progress_tokens
    }
    class Transport {
        <<enum>>
        Stdio(Box~StdioTransport~)
        Http(HttpTransport)
    }
    class StdioTransport {
        reader: FrameReader
        outbound: Sender~Vec u8~
        child: Option~Child~
    }
    class HttpTransport {
        shared: Arc~Shared~
        inbound_rx: Receiver~Inbound~
        tasks: JoinSet
    }
    class PendingMap {
        next_id: u64
        pending: HashMap~u64, Pending~
        by_client: HashMap~RequestId, u64~
    }
    class ClientTable {
        routes: HashMap~RequestId, usize~
    }
    class ToolSet {
        per_upstream: Vec~Vec~ToolEntry~~
        routes: HashMap~String, usize~
    }
    Session "1" *-- "n" Handle
    Session "1" *-- "1" ClientTable
    Session "1" *-- "1" ToolSet
    Handle ..> Actor : commands channel
    Actor ..> Session : events channel
    Actor "1" *-- "1" Transport
    Actor "1" *-- "1" PendingMap
    Transport --> StdioTransport
    Transport --> HttpTransport
```

| Type | Module | Owned by | Role |
|---|---|---|---|
| `Session` (private) | `router` | the caller's task | All per-session state on the server face. Single-threaded by construction — only the serve loop touches it. |
| `Handle` | `connection` | `Session` (one per upstream); cloned into re-list tasks | The router's only way to talk to an actor. `try_send` for the serve loop; `send`/`call` for independent tasks. |
| `Actor` (private) | `connection` | its own task | Owns one upstream end-to-end: transport, minted-id space, progress-token scope. Translates ids both ways so the router never sees an upstream id. |
| `Transport` | `transport` | one `Actor` (or `connect()` during the handshake) | Uniform frame-in/frame-out over stdio or HTTP. Knows nothing about JSON-RPC except what HTTP forces on it (`SendContext`). |
| `PendingMap<T>` | `idmap` | one `Actor` | Minted id → `Pending::Client{client_id, client_id_raw}` or `Pending::Internal(reply)`. Cancel *removes* the mapping, which is what makes late responses drop. |
| `ClientTable` | `idmap` | `Session` | Client id → upstream index for requests in flight, session-wide. Rejects a duplicate in-flight id; lets the router drop a response from the wrong upstream. |
| `ToolSet` | `toolset` | `Session` (rebuilt whole on re-list) | Name → upstream index, plus each tool's raw bytes in upstream order. `build` fails on any duplicate name. `merged_result()` is the client's unpaginated `tools/list` result. |
| `Command` / `Event` | `connection` | — | The only vocabulary between router and actor. `Command`: `Forward`, `Cancel`, `Request` (proxy-internal), `Shutdown`. `Event`: `Response`, `Notification`, `ListChanged`, `Ended`, `Fatal`. |
| `SessionSummary` / `SessionEnd` | `router` | returned by `run` | End-of-session accounting: why it ended, per-direction frame counters, rejected/discarded counts. `clean_shutdown()` = client EOF with everything delivered. |

## 5. Session lifecycle

`router::run` has three phases, then teardown. The client is not read
from at all until every upstream is up and its tools are known — a
collision is refused at startup, never discovered mid-session.

```mermaid
stateDiagram-v2
    [*] --> Connecting : run()
    Connecting --> Listing : every upstream initialized
    Connecting --> [*] : StartupError (Connect)
    Listing --> Serving : ToolSet built, no collision
    Listing --> [*] : StartupError (List or Collision)
    state Serving {
        [*] --> PreInit
        PreInit --> Initializing : initialize answered
        Initializing --> Ready : notifications/initialized
    }
    Serving --> Teardown : SessionEnd
    Teardown --> [*] : SessionSummary
```

Client-face phases gate what the router will do (`Session::on_client_request`):

| Phase | `ping` | `initialize` | anything else |
|---|---|---|---|
| `PreInit` | `{}` | answered → `Initializing` | `-32002 Server not initialized` |
| `Initializing` | `{}` | `-32600 Already initialized` | `-32002` |
| `Ready` | `{}` | `-32600` | `tools/list`, `tools/call` handled; other methods `-32601` |

Upstream-originated notifications are dropped until the client face is
`Ready`; a re-list that completes before `Ready` sets
`pending_list_changed`, and one `notifications/tools/list_changed` is
flushed at the transition.

Teardown (the tail of `run`), in order: `try_send(Command::Shutdown)`
to every actor → abort re-list tasks → drop the router's `Handle`s
(closing every command channel — the fallback shutdown signal for an
actor whose queue was full) → join each actor with `shutdown_grace`,
aborting stragglers → drop `to_client` → join the writer with the same
grace, aborting it if the client stopped reading. Each actor's own
teardown fails its internal callers with `CallError::Closed`, emits its
end event, and closes its transport (stdio: close stdin, wait 5 s, kill;
HTTP: abort tasks, best-effort `DELETE`).

## 6. Flows

Participants used below: **Client**, **Router** (the serve loop / `Session`),
**Writer** (client writer task), **Actor i**, **Transport i**, **Upstream i**.

### 6.1 Startup: connect and list every upstream

```mermaid
sequenceDiagram
    autonumber
    participant Run as router::run
    participant Ci as connect(i) — one per upstream, concurrently
    participant Ti as Transport i
    participant Ui as Upstream i
    participant Ai as Actor i

    Run->>Ci: connection::connect(i, name, transport, init_timeout, events)
    Ci->>Ti: send initialize {protocolVersion 2025-11-25, capabilities {}}
    Ti->>Ui: frame
    loop until the initialize response (deadline: init_timeout)
        Ui-->>Ti: frame
        Ti-->>Ci: Received
        alt response to the minted init id
            Note over Ci: parse InitializeResult, require a supported version
        else server ping
            Ci->>Ti: pong
        else anything else
            Note over Ci: dropped with a log line
        end
    end
    Ci->>Ti: send_ordered notifications/initialized
    Ci->>Ti: start_listening (HTTP: open GET stream)
    Ci->>Ai: spawn actor (owns transport + PendingMap)
    Ci-->>Run: (Handle, JoinHandle, UpstreamInfo)
    Note over Run: join_all — any failure ⇒ shutdown_all + StartupError::Connect

    Run->>Ai: Handle::call("tools/list") — one drain per upstream, concurrently
    loop pages, ≤ MAX_LIST_PAGES, ≤ MAX_TOOLS_PER_UPSTREAM, ≤ max_frame_bytes retained
        Ai->>Ui: tools/list {cursor?}
        Ui-->>Ai: page {tools, nextCursor?}
    end
    Ai-->>Run: Vec<ToolEntry>
    Note over Run: ToolSet::build — duplicate name ⇒ StartupError::Collision
    Note over Run: spawn client writer, enter Session::serve (PreInit)
```

Details worth knowing: the handshake declares **no** client
capabilities upstream (no roots, sampling, elicitation) — the
server→client request channel is closed on purpose; `initialized` is
sent with `send_ordered` because over HTTP a detached POST could race
the next request; an upstream that declares no `tools` capability
contributes an empty list and a warning, not a failure.

### 6.2 Client handshake and `tools/list`

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant R as Router
    participant W as Writer

    C->>R: initialize {protocolVersion, clientInfo}
    Note over R: negotiate — echo a supported offer, else answer 2025-11-25
    R->>W: result {protocolVersion, capabilities {tools {listChanged true}}, serverInfo, instructions?}
    W-->>C: frame
    Note over R: phase = Initializing
    C->>R: notifications/initialized
    Note over R: phase = Ready (flush pending list_changed if any)

    C->>R: tools/list {cursor?}
    alt cursor present and non-null
        R->>W: -32602 Unknown cursor
    else merged list + 64 bytes > max_frame_bytes
        R->>W: -32603 merged tool list exceeds the frame limit
    else
        R->>W: result ToolSet::merged_result() — every tool's original bytes, no cursor
    end
    W-->>C: frame
```

`tools/list` never touches an upstream: the table was drained at
startup and is refreshed only by `list_changed` (§6.5). The proxy never
mints a cursor, so any cursor is foreign by definition. `instructions`
is the single upstream's text verbatim, or `## name` sections when
several upstreams supply one.

### 6.3 `tools/call`: routing and id translation

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant R as Router
    participant A as Actor i
    participant T as Transport i
    participant U as Upstream i
    participant W as Writer

    C->>R: tools/call {id: X, params {name, arguments, _meta.progressToken?}}
    Note over R: name → upstream i via ToolSet::route (none ⇒ -32602 Unknown tool)
    Note over R: ClientTable.insert(X, i) (already in flight ⇒ -32600)
    R->>A: try_send Command::Forward {client_id X, client_id_raw, frame}
    Note over R,A: Busy ⇒ -32603 upstream busy, Closed ⇒ -32603 upstream unavailable (entry removed)
    Note over A: PendingMap.insert_client(X) mints upstream id N
    Note over A: splice id X → N (rest of the frame byte-identical), record progressToken under N
    A->>T: send(frame', Request{N})
    T->>U: frame'
    opt progress
        U-->>T: notifications/progress {progressToken}
        T-->>A: frame
        Note over A: token in scope for this actor? else dropped
        A-->>R: Event::Notification
        R->>W: frame (only if phase Ready)
    end
    U-->>T: response {id: N, result | error}
    T-->>A: frame
    Note over A: PendingMap.complete(N) → Pending::Client{X, raw}, release token
    Note over A: splice id N → original bytes of X
    A-->>R: Event::Response {upstream i, client_id X, frame}
    Note over R: ClientTable.route(X) == i ? remove and deliver : drop (stale or cancelled)
    R->>W: frame
    W-->>C: response {id: X, result | error} — body bytes as the upstream sent them
```

Two independent checks bound a response's blast radius: the actor only
completes ids *it* minted (`PendingMap`), and the router only delivers
a response whose client id is still in flight *toward that upstream*
(`ClientTable`). A malicious or buggy upstream can therefore neither
answer another upstream's request nor hijack a client id that has been
reused since. Progress notifications get the same scoping through the
recorded `progressToken`.

If the frame parses at the envelope boundary but resists an unambiguous
rewrite (duplicate members), the actor fails closed: it never reaches
the upstream and the client gets `-32600 Invalid Request`. If an
upstream *response* resists rewrite, the client still gets a typed
answer (`-32603 upstream response could not be translated`) rather than
a hang.

### 6.4 Cancellation and late responses

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant R as Router
    participant A as Actor i
    participant U as Upstream i

    C->>R: notifications/cancelled {requestId: X}
    Note over R: ClientTable.remove(X) → i (not in flight ⇒ dropped, counted)
    R->>A: try_send Command::Cancel {client_id X, frame}
    Note over A: PendingMap.cancel_client(X) → N and forgets the mapping
    Note over A: rewrite params.requestId X → N, everything else preserved
    A->>U: notifications/cancelled {requestId: N}
    U-->>A: (late) response {id: N}
    Note over A: PendingMap.complete(N) = None ⇒ dropped
```

Cancellation is best-effort by specification. If the actor's queue is
full the cancel is not forwarded — but the router already removed the
in-flight entry, so the eventual response is dropped by the route check
in §6.3 instead. Either way the client never sees a response to a
request it cancelled.

### 6.5 `list_changed` and re-listing

```mermaid
sequenceDiagram
    autonumber
    participant U as Upstream i
    participant A as Actor i
    participant R as Router
    participant RL as re-list task
    participant W as Writer

    U-->>A: notifications/tools/list_changed
    A-->>R: Event::ListChanged {i}
    alt a re-list for i is already running
        Note over R: relist_dirty[i] = true (coalesced, one more run after this one)
    else
        R->>RL: spawn drain_tools(handle i)
        RL->>A: Handle::call("tools/list") … pages (may await queue space — not the serve loop)
        RL-->>R: RelistOutcome {i, result}
        alt Ok(tools) and ToolSet::build ok
            Note over R: replace the table
            R->>W: notifications/tools/list_changed (deferred until phase Ready)
        else Ok(tools) but a name now collides
            Note over R: SessionEnd::ToolCollision — session ends
        else Err
            Note over R: keep the previous table, warn (availability may lag)
        end
    end
```

Why a failed re-list is tolerated but a collision is not: a stale table
only affects *availability* — grants will gate every call from M5
regardless of what the table says — whereas a name that routes to two
upstreams is ambiguous authority, and the proxy refuses to serve
ambiguity.

### 6.6 Shutdown and failure propagation

Every ending funnels into one `SessionEnd`, and every ending — clean or
not — runs the same teardown (§5).

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant R as Router
    participant A as Actor i
    participant T as Transport i
    participant U as Upstream i
    participant W as Writer

    rect rgb(235, 245, 235)
        Note over C,W: normal: client closes its input
        C--xR: EOF on stdin
        Note over R: SessionEnd::ClientEof
    end
    rect rgb(250, 240, 230)
        Note over C,W: an upstream goes away
        U--xT: process exits / pipe breaks / HTTP 404 under a session id
        T-->>A: Ok(None) or Err(TransportError)
        Note over A: fail internal callers with CallError::Closed
        A-->>R: Event::Ended {i} or Event::Fatal {i, error}
        A->>T: close()
        Note over R: SessionEnd::UpstreamGone {i}
    end
    rect rgb(245, 235, 235)
        Note over C,W: the client stops reading
        W--xC: write fails
        Note over W: drain and count the rest as undelivered, close the pipe
        Note over R: next deliver() fails ⇒ SessionEnd::ClientWriteFailed
    end
    Note over R,W: teardown for every case
    R->>A: try_send Shutdown, then drop Handles (closes command channels)
    A->>T: close() — stdio: close stdin, wait ≤5 s, kill · HTTP: abort tasks, DELETE
    R->>W: drop to_client, join with shutdown_grace (abort if the client is not reading)
    Note over R: SessionSummary {end, counters}
```

The policy behind the middle case is stated in `router.rs`: until
supervision lands (T3), *any* upstream ending ends the whole session,
because dying loudly beats serving a session whose tool surface
silently shrank. Whether the exit code is success is decided by
`SessionSummary::clean_shutdown()` — client EOF *and* every accepted
frame delivered.

### 6.7 Streamable HTTP, one request

The HTTP transport turns each outbound frame into its own POST and
reassembles the upstream's side of the conversation from response
bodies and the optional GET stream. The actor above it sees the same
`send`/`recv` surface as for stdio.

```mermaid
sequenceDiagram
    autonumber
    participant A as Actor
    participant H as HttpTransport
    participant P as POST task
    participant G as GET listener
    participant S as HTTP server

    A->>H: send(frame, Request{N})
    H->>P: spawn post_task
    P->>S: POST frame — Accept json+event-stream, Mcp-Session-Id?, MCP-Protocol-Version?
    alt 202 Accepted (notifications, responses)
        Note over P: done
    else 200 application/json
        S-->>P: one JSON body (capped at max_frame_bytes, newlines normalized)
        P-->>H: inbound Frame
    else 200 text/event-stream
        loop SSE events until the stream ends
            S-->>P: event (default type)
            P-->>H: inbound Frame (server messages, then the response to N)
        end
        Note over P: stream ended without answering N ⇒ treated as a failed POST
    else 404 while holding a session id
        P-->>H: inbound Fatal(SessionExpired)
    else other failure
        P-->>H: inbound Frame — synthesized error response for N: -32603 "upstream request failed"
    end
    H-->>A: recv() → Received::Frame / Dropped, or Err(fatal)

    par unsolicited server messages
        G->>S: GET — Accept text/event-stream (after initialized)
        S-->>G: SSE events (e.g. notifications/tools/list_changed)
        G-->>H: inbound Frame
        Note over G: 405 or 404 without a session ⇒ no stream offered, stop · other failures ⇒ retry with backoff 1–30 s, honouring retry hints
    end
```

Design points: a failed POST is a *per-request* failure — hosted
endpoints blip — so the transport synthesizes a JSON-RPC error
*response* for that id, and the actor's `PendingMap` accounting
resolves normally; the synthesized error is the only frame this
transport ever originates. Session expiry (404 under a session id) is
the one HTTP condition treated as fatal, because silently
re-initializing would put the upstream in a state the client never
observed. Redirects are refused; SSE resumability (`Last-Event-ID`) is
deliberately not implemented in M2. Header values are marked sensitive
and URLs are redacted before they can reach a log.

## 7. Invariants

What the code is written to guarantee, and where each guarantee lives.
A reviewer checking a change should be able to say which of these it
touches.

| # | Invariant | Where it is enforced |
|---|---|---|
| I1 | **Bodies are byte-faithful.** `params`, `result`, and `error` are never re-serialized in either direction; what is forwarded is the original frame with exactly one member rewritten (`id`, or `params.requestId` for a cancel). Member keys of a rewritten frame are re-encoded canonically and inter-member whitespace normalized — body-level identity is unaffected. | `envelope` (borrows `RawValue`s, never re-encodes), `splice::rewrite_member`, `toolset` (tool objects kept as raw bytes) |
| I2 | **Fail closed at every parse boundary.** A frame that is not one well-formed JSON-RPC 2.0 object is a typed error and is never forwarded: batches, non-objects, bad `jsonrpc`, non-integer/string ids, ambiguous shapes, duplicate members, oversized frames, invalid UTF-8, raw newlines inside JSON strings. | `framing`, `sse`, `envelope`, `splice`, `transport::normalize_newlines` |
| I3 | **Id containment.** A client id never reaches an upstream; an upstream id never reaches the client. Each upstream is spoken to in a monotonic id space the proxy mints. A response is delivered only if (a) the actor minted its id and it is still pending, and (b) the router still has the client id in flight toward *that* upstream. Progress notifications are delivered only for tokens the same actor forwarded. | `idmap::PendingMap`, `idmap::ClientTable`, `connection::Actor` (progress scope), `router::on_upstream_event` |
| I4 | **Cancellation removes the mapping.** Late responses after a cancel are dropped as a consequence of the map, not by a timer. | `PendingMap::cancel_client`, `ClientTable::remove` |
| I5 | **Capabilities are attenuated on both faces.** The client is told exactly what the proxy honors (`tools.listChanged`); upstreams are told the proxy has no client capabilities, so server→client requests are refused (`ping` answered as a courtesy) and notifications outside `tools` are dropped. | `router::on_initialize`, `connection::initialize`, `Actor::handle_upstream_frame` |
| I6 | **No ambiguous routing.** A tool name offered twice — across upstreams or within one — is refused at startup and on re-list; the proxy never picks a winner silently. | `ToolSet::build`, `router::startup`, `Session::on_relist` |
| I7 | **Bounded memory.** Per-frame cap in both directions and on every transport (16 MiB default); per-event cap in SSE; per-upstream tool table bounded by pages, count, and bytes; every queue bounded at 64. | `framing`, `sse`, `router::drain_tools`, channel constructors |
| I8 | **No deadlock through queues or pipes.** The serve loop never awaits an actor queue; child stdin is written by its own task with a stall deadline; each HTTP POST is its own task. | §3 rules 1–3 |
| I9 | **Secrets never reach logs.** Header values are `sensitive`; URLs are redacted to scheme/host/port/path; `TransportSpec`'s `Debug` redacts; HTTP failure reasons are scrubbed of URLs. | `http`, `config::redact_url`, `config::TransportSpec` |
| I10 | **Loud failure.** Any upstream ending ends the session with a typed `SessionEnd`; a task dying without reporting surfaces as `Internal`, not a hang; internal callers of a dead actor get `CallError::Closed`. | `router::serve`, `Actor::run` teardown |
| I11 | **Every frame is accounted for.** Frames forwarded, delivered, undelivered, rejected at the parse boundary, and discarded by the router are counted and reported in `SessionSummary`. (The trace sink of M3/M5 will make these events, not just counters.) | `router` counters, `WriterCounters` |
| I12 | **No `unsafe`, no panics on the request path.** `#![forbid(unsafe_code)]`; workspace clippy lints flag `unwrap`/`expect` (warn, promoted to errors by `-D warnings` in the CI gate; test modules opt out explicitly); every failure is a typed error. | `lib.rs`, workspace `Cargo.toml` `[workspace.lints]` |

## 8. Client-visible error surface

All proxy-originated errors are built in `builder` from the fixed code
table. Ids echo the request's original bytes; `null` when the id could
not be read.

| Code | Meaning here | Raised when |
|---|---|---|
| `-32700` Parse error | frame unreadable | oversized client frame; invalid UTF-8; invalid JSON |
| `-32600` Invalid Request | readable, illegal | not a JSON-RPC 2.0 object (batch, bad shape, bad id); `initialize` after init; a client id already in flight; a request that resists id rewrite |
| `-32601` Method not found | outside advertised capabilities | any method other than `ping`/`initialize`/`tools/list`/`tools/call` once `Ready` |
| `-32602` Invalid params | | malformed `initialize`/`tools/call` params; a `tools/list` cursor (the proxy never mints one); **a tool no upstream offers** — deliberately the same shape M5 will use for a tool outside the grant envelope, so absence and denial are indistinguishable |
| `-32603` Internal error | | actor queue full (`upstream busy`); actor gone (`upstream unavailable`); merged tool list would exceed the frame cap; upstream response untranslatable; HTTP POST failed for that request (`upstream request failed`, synthesized by the transport) |
| `-32002` Server not initialized | | any non-`ping`, non-`initialize` request before the client handshake completes |

Everything else the client sees is an upstream's own response, bytes
intact.

## 9. Limits and tuning knobs

| Knob | Default | Where | Purpose |
|---|---|---|---|
| `ProxyConfig::max_frame_bytes` | 16 MiB | `framing::DEFAULT_MAX_FRAME_BYTES` | Per-frame cap, all transports, both directions; also the per-upstream tool-table byte budget and the SSE per-event cap. |
| `ProxyConfig::shutdown_grace` | 5 s | `router` | How long actors and the writer get to finish at teardown before being aborted. |
| `ProxyConfig::init_timeout` | 60 s | `router` → `connection::connect` | Per-upstream `initialize` deadline (generous: `npx …` with a cold cache). |
| `ProxyConfig::list_timeout` | 30 s | `router::drain_tools` | Per-page `tools/list` deadline. |
| `CLIENT_QUEUE_FRAMES`, `COMMAND_QUEUE`, `OUTBOUND_QUEUE`, `INBOUND_QUEUE` | 64 | `router`, `connection`, `transport`, `http` | Every bounded channel. |
| `MAX_LIST_PAGES` / `MAX_TOOLS_PER_UPSTREAM` | 1 000 / 10 000 | `toolset` | Pagination and count caps per upstream. |
| `CHILD_EXIT_GRACE` | 5 s | `transport` | Wait after closing a child's stdin before killing it. |
| `WRITE_STALL_TIMEOUT` | 30 s | `transport` | One frame write to a child may stall this long before the pipe is declared dead. |
| `CONNECT_TIMEOUT` / `DELETE_TIMEOUT` | 10 s / 2 s | `http` | HTTP connect budget; best-effort session DELETE budget. |
| `RECONNECT_MIN` … `RECONNECT_MAX` | 1 s … 30 s | `http` | GET-stream reconnect backoff bounds (SSE `retry` hints clamped into this range). |
| `OFFERED_VERSION` / `SUPPORTED_VERSIONS` | `2025-11-25` / `2025-06-18`, `2025-11-25` | `protocol` | Version policy, both faces. Batching-era revisions are excluded on purpose. |

Only `ProxyConfig` is runtime-configurable today; the CLI passes
`ProxyConfig::default()`.

## 10. Where enforcement plugs in

What the T1 plan says lands next, and the seams in this crate that were
left for it. This section is a forecast — update it as M4–M5 merge.

- **The vocabulary exists (M3, `flavium-core`).** `GrantEnvelope`
  (principal + grants), `Grant`/`Constraint`/`ArgValue`, `ToolCall`,
  `Decision`/`DenialReason`, the reference `decide`, `attenuates`, and
  two traits with no I/O: `Authorizer` (`authorize(principal, call,
  now) -> Decision`, `granted_tools(principal, now)`) and `TraceSink`
  (`record(&TraceEvent) -> Result`). The proxy will depend on
  `flavium-core` only; the Cedar engine (M4, `flavium-policy`)
  implements `Authorizer` and the CLI wires it. The full `TraceEvent`
  catalog — session start/end, handshake, `ToolsListed`, `CallRefused`
  / `CallDecided` / `CallCompleted` keyed by a per-session `CallId`,
  `FrameRejected`, `FrameDiscarded`, `UpstreamEnded` — is defined
  there; M5 emits it.
- **`tools/call` authorization (M5, via `flavium-policy`).** The hook
  point is `Session::on_tools_call`, between `ToolSet::route` and
  `Command::Forward`: the call's `name` and `arguments` become a
  `ToolCall` (strings and `i64` integers as themselves, everything else
  `ArgValue::Other`; a non-object `arguments` or duplicate keys ⇒
  `-32602` + `CallRefused`), `now` is taken once, and the `Authorizer`
  answers. `NotGranted`/`Expired` answer `-32602` exactly as an unknown
  tool does today; `OutOfEnvelope` answers an `isError: true` result
  (`denied by policy`) — agent-visible and recoverable, leaking no
  grant internals. Neither reaches the upstream.
- **`tools/list` filtering (M5).** `Session::on_tools_list` will
  project `ToolSet` down to `Authorizer::granted_tools(principal, now)`;
  an expired grant makes the tool vanish from the list and its calls
  return `-32602`.
- **Trace events (M5 wiring).** The counters in §7/I11 become
  `TraceEvent`s through the `TraceSink` (the CLI supplies a JSONL
  sink; a sink failure ends the session — fail closed on audit).
  Emission points are all in `router`: session start, handshake
  answered, each list, each refusal/decision/completion, each
  rejection/discard, each upstream end, session end. `ClientTable`
  gains the tool name and `CallId` per in-flight call so completions
  can be attributed. Two of the actor's drops (unknown response ids,
  out-of-scope progress) have `DiscardKind`s reserved; the rest of the
  upstream-face drops are not traced in T1.
- **Principal.** Static per proxy process, from CLI config; `clientInfo`
  is untrusted data and never identity (it is recorded in
  `HandshakeCompleted` as data).
- **Deliberately deferred beyond T1/M5:** upstream supervision and
  restart policies (T3 — today any upstream ending ends the session);
  tool namespacing (`server.tool`) as the collision fallback; an HTTP
  *server* face; SSE resumability; the 2026-07-28 protocol revision;
  budgets (T2); the hash-chained recorder (T4); the fuzz harness over
  `framing`/`sse`/`envelope`/`splice` (T5).

## 11. Tests as executable specification

| Where | What it pins |
|---|---|
| `tests/router_session.rs` (22 tests) | The M2 contract end to end over in-memory pipes with two scripted upstreams: proxy-answered `initialize`, per-upstream handshakes, merged and drained `tools/list`, routing by name with ids translated both ways, cancellation and late-response drops, collisions, `list_changed` re-lists, phase gating, and the T1 acceptance criterion that `params`/`result` bytes round-trip identically. |
| `tests/http_upstream.rs` (3 tests) | The streamable-HTTP transport against a real in-process axum server: session id assigned at `initialize` and required afterwards, protocol-version header, 202 for notifications, JSON and SSE bodies (multi-line data exercising newline normalization), the GET stream, DELETE at close, 404 after expiry. |
| `crates/flavium-cli/tests/proxy_e2e.rs` (8 tests) | The real binary over real child processes (`examples/scripted_upstream`): config-file and `-- command` forms, multi-upstream merge, collision refusal, exit codes. |
| Unit tests in each module | The parser boundaries (`framing`, `sse`, `envelope`, `splice`) — including the failing paths: oversized, invalid UTF-8, `id: null`, duplicate members, batches; `idmap` translation and cancel semantics; `toolset` merge/collision; `builder` escaping; `protocol` version policy; `http` setup redaction. |
| `examples/scripted_upstream.rs` | A minimal stdio MCP server (one tool, name from argv) for the e2e tests and for driving the proxy by hand. |

The demo checklists in `docs/tasks/v0.1/` record what was verified
against real clients (Claude Desktop, Claude Code) at each milestone.

## 12. How the CLI uses the crate

`flavium proxy` (in `crates/flavium-cli`) resolves an upstream set —
from `--config <file>` (TOML, one `[[upstream]]` per server) or the
single-stdio-upstream `-- command…` form — into `Vec<UpstreamSpec>`,
starts a multi-threaded tokio runtime, and calls
`stdio::serve(ProxyConfig::default(), &specs)`. `stdio::serve`
validates the specs, builds one `Transport` per upstream (spawning
children, constructing HTTP clients — closing what it already built if
a later one fails), and hands them to `router::run` over this process's
stdin/stdout. Logs go to stderr (`RUST_LOG`, default `info`); stdout
carries only MCP frames; spawned children inherit stderr. The exit code
is success iff `SessionSummary::clean_shutdown()`. The operator-facing
reference — flags, config keys, exit codes, startup errors, client
wiring — is [docs/cli.md](../cli.md).
