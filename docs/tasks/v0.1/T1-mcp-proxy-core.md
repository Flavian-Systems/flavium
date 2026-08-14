# T1 — MCP proxy core (plan)

Status: **approved 2026-08-14** — includes the dependency-approval list
below (flavium-core: none; flavium-policy: cedar-policy, serde_json,
thiserror).

## Goal & non-goals

`flavium` presents a stdio MCP server to a real client (Claude Desktop),
proxies to configured stdio + streamable-HTTP upstreams, forwards
initialize / tools/list / tools/call faithfully, Cedar-authorizes every
tools/call against a grant file (path prefixes, recipient/domain patterns,
numeric ranges, expiry), filters tools/list to granted tools, traces every
decision. Deferred: budgets→T2; delegation→T3; hash-chained SQLite
recorder→T4 (T1 traces JSONL behind a swappable sink); fuzz harness→T5
(T1 keeps the parser boundary fuzz-ready); tool namespacing; HTTP server
face; MCP 2026-07-28 "modern era".

## Design decisions

- **MCP layer: hand-rolled, not rmcp.** Typed JSON-RPC envelope with
  `params`/`result` kept as `serde_json::RawValue`; only the three
  intercepted methods parsed deeper; unknown methods, notifications,
  `_meta`, unmodeled fields forward byte-faithfully. rmcp's typed
  round-trip drops unknown fields, and a small self-owned parser is the
  auditable trust boundary this product sells.
- **Protocol.** Proxy answers the client's initialize itself (offers
  2025-11-25, accepts 2025-06-18+; 2025-03-26 not offered ⇒ no JSON-RPC
  batches) and initializes each upstream separately. Capabilities:
  advertise to the client only what the proxy honors (tools.listChanged —
  it re-lists and re-emits); attenuate client capabilities upstream;
  sampling/elicitation not declared upstream, deliberately closing the
  server→client request channel for T1.
- **Routing.** One id-translation module; `notifications/cancelled`
  requestIds rewritten both ways; late responses after cancel dropped.
  tools/list: drain each upstream's pagination internally, return one
  unpaginated merged list; a cursor the proxy didn't mint ⇒ -32602.
  Duplicate tool names across upstreams: reject at startup discovery and
  on list_changed re-list (typed error event); `server.tool` namespacing
  is the documented fallback, not T1 work.
- **Grants.** TOML → flavium-core types → Cedar via the JSON policy format
  (`Policy::from_json`; no string interpolation). Grant file and compiled
  policies validated at load; call args + `now` become Context; paths
  normalized pre-eval (`..`, doubled and `\` separators); float/null/
  non-object args ⇒ deny; any Cedar diagnostics error ⇒ deny (fail closed).
- **Denial surface.** Ungranted tool ⇒ JSON-RPC -32602, indistinguishable
  from nonexistent (matches the filtered tools/list); an expired grant is
  no grant — the tool vanishes from subsequent tools/list and calls get
  -32602; out-of-envelope args on a granted tool ⇒ isError:true result
  "denied by policy" (agent-visible, recoverable, leaks no grant
  internals). Allow AND deny emit trace events.
- **Type homes.** flavium-core (zero-dep): Grant/Constraint/Principal/
  Decision/DenialReason, TraceEvent + TraceSink trait (no I/O),
  `attenuates()`. flavium-policy: grant→Cedar compiler + `authorize()`
  behind one trait — the sole enforcement seam. flavium-proxy-mcp:
  framing, routing, transports, enforcement hook. flavium-cli: config,
  wiring, JSONL sink (serialization lives here; core stays serde-free).
- **Principal**: static per proxy process from config; clientInfo is
  untrusted data, never identity.

## Milestones (one PR each, lands green)

1. Transparent stdio↔stdio proxy, one upstream *(no core/policy changes)*.
   Demo: Claude Desktop drives the filesystem server through flavium
   unmodified — riskiest unknown first; record the negotiated protocol
   version, pin tests to it.
2. Multi-upstream + streamable-HTTP upstream client (POST JSON/SSE,
   Mcp-Session-Id, GET stream); id-translation; merged tools/list;
   collision-reject *(no core/policy changes)*.
3. flavium-core types + TraceSink. Invariant preserved: delegation
   strictly attenuates — subset on every axis, property-tested; zero-dep,
   no unsafe/unwrap.
4. flavium-policy compiler + authorizer *(gated on approval below)*.
   Invariants: deny-by-default; evaluation error ⇒ deny.
5. Enforcement wired: tools/call gated, tools/list filtered, JSONL trace,
   CLI config. Demo: full acceptance run. README updated same PR.

## Dependencies requiring human approval

- flavium-core: **none** — stays zero-dependency. Flagged deviation:
  hand-rolled Error impls instead of thiserror, keeping the verification
  target minimal.
- flavium-policy: **cedar-policy** (pinned 4.12) — the mandated engine;
  heavy (~50 crates), confined behind the authorize trait; **serde_json**
  (build policy JSON safely); **thiserror** (errors convention).
- Unrestricted crates, listed for transparency: proxy — tokio, serde,
  serde_json, thiserror, tracing, reqwest, eventsource-stream; cli —
  clap, toml, tracing-subscriber.

## Tests & acceptance

- *Claude Desktop works unmodified*: manual checklist at M1/M5 (Claude
  Desktop + Claude Code as second real client) + scripted-session
  integration test asserting `params`/`result` RawValue bytes round-trip
  unmodified (ids are rewritten, so identity is asserted at body level).
- *Grant file denies out-of-envelope calls*: per-axis table — path outside
  prefix (incl. `../` and `\`), off-pattern recipient, out-of-range
  number, expired grant, ungranted tool, unrepresentable args — each never
  reaches the upstream, asserting the pinned client-visible shape AND the
  trace event (principal, tool, reason).
- *Denials are logged*: covered by that pairing; allow events asserted
  too; Cedar-error ⇒ deny included.
- *Routing*: colliding upstream ids, translated cancellations, dropped
  late responses. *Parser failing paths*: oversized line, invalid UTF-8,
  id:null, duplicate id ⇒ typed error, no panic — the designated T5 fuzz
  seam.

## Risks

1. Claude Desktop protocol drift (2026-07-28 rolling out) — M1 tests the
   real client first; byte-faithful passthrough tolerates unknown methods.
2. Cedar fails open on erroring policies — load-time validation makes eval
   errors unreachable; deny-on-any-diagnostics as backstop, with tests.
3. Bidirectional id-translation bugs (silent cancel no-ops) — one routing
   module, property tests over id/cancel/progress interleavings.
