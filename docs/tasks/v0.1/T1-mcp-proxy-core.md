# T1 — MCP proxy core (plan)

Status: **approved 2026-08-14** — includes the dependency-approval list
below (flavium-core: none; flavium-policy: cedar-policy, serde_json,
thiserror). Progress: M1 landed 2026-08-15 (#1, demo checklist in
`T1-demo.md`); M2 followed — one deviation from the plan's crate
list: `eventsource-stream` was not adopted (the SSE parser is
hand-rolled next to the framing, as a bounded, fuzz-ready seam), and
`futures-util` + `axum` (dev-only) were added alongside `reqwest`.
M3 landed 2026-08-15 (milestone plan and decisions D1–D10 in
`T1-m3-plan.md`), with these refinements to the "Type homes" and
"Grants" bullets: the principal is the envelope's, not the grant's
(`GrantEnvelope { principal, grants }`); the *reference* decision
semantics (`decide`) live in flavium-core as the executable
specification Cedar is tested against, and the `Authorizer` trait lives
in flavium-core beside `TraceSink` so the proxy never depends on Cedar
(flavium-policy implements it — still the sole *runtime* enforcement
seam); the constraint vocabulary gained `Absent` (an argument must not
be supplied — how `cc`/`bcc` get closed); the trace catalog was defined
in full (`TraceEvent`, exhaustive enum) rather than accreted in M5; the
budget axis is reserved for T2. flavium-core remains zero-dependency,
dev-dependencies included. M4 landed 2026-08-16 (`T1-m4-plan.md`).
**M5 landed 2026-08-16** (`T1-m5-plan.md`); see the M5 note at the end
of this file for what that plan got wrong. T1's code is complete; the
manual acceptance run against Claude Desktop under a grant file
(`T1-demo.md`, "M5 variant") is still owed.

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
- flavium-policy: **cedar-policy** 4.12 — the mandated engine; heavy
  (68 crates transitively, measured at M5), confined behind the
  authorize trait; **serde_json** (build policy JSON safely);
  **thiserror** (errors convention). Written as the workspace's usual
  semver-compatible range rather than an `=` pin: the drift worth
  catching is a change to the EST or the diagnostics API, and an API
  change fails to compile while an encoding change fails
  `a_representative_grant_renders_as_expected` (M4 plan, note 3).
- Unrestricted crates, listed for transparency: proxy — tokio, serde,
  serde_json, thiserror, tracing, reqwest, futures-util (**not**
  `eventsource-stream`, per the deviation recorded above; `axum` is
  dev-only, for the in-process HTTP upstream in the tests); cli —
  clap, toml, tracing-subscriber, **and `sha2`** (added at M5, approved
  in chat 2026-08-16, for the trace's truncation digest; T4's
  hash-chained recorder needs a cryptographic hash regardless, so it is
  an early dependency rather than a new one). The proxy also gained a
  path dependency on flavium-core at M5 — the enforcement vocabulary
  only, which pulls in nothing, so `cargo tree -p flavium-proxy-mcp`
  still contains no `cedar-policy`.

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

## M5 note — where this plan and the milestone plan were wrong

Recorded here so the next reader is not misled by either document.

- **This plan's CLI crate list did not name `sha2`.** Corrected above.
- **`router::run` takes `Option<Enforcement>`, not `Enforcement`.** The
  M5 plan's D7 wrote the parameter unconditionally, but D1 also keeps the
  transparent middlebox alive behind `--unenforced`, and those two cannot
  both be true. `None` is the honest spelling: the type says there is no
  enforcement, rather than an "allow everything" authorizer sitting in
  the codebase where anyone could construct one. W1 reads "when
  enforcement is configured, the only path to `Command::Forward` runs
  through a `Decision::Allow`", which is still one branch in one
  function.
- **D4's "no case folding" was wrong for the Windows flavor**, and the
  first live run against a real filesystem server is what showed it: the
  same file was allowed as `C:\Users\…\ok.txt` and denied as
  `c:\users\…\ok.txt`. The rule was written from the POSIX argument (a
  proxy cannot see the filesystem, so it must not guess how a name
  resolves), but the flavor is not a guess — an operator who writes
  `windows-path-prefix` has *declared* the resolution rule, and
  case-insensitivity is as much a part of it as `\`. Deciding on the
  spelling rather than the resource is the exact failure the normalizer
  exists to prevent; that D4 landed on the safe side of it made it a
  lost call rather than a leaked one, not a correct rule. The Windows
  flavor now folds **ASCII** case on both sides. ASCII and not Unicode
  because full folding can merge characters Windows keeps apart and can
  change a string's length (`İ`), and either would turn the fix into a
  false *allow*; a non-ASCII case difference is still refused, which
  keeps D4's asymmetry where it belongs. Two residual gaps are now
  documented rather than implied: a POSIX upstream wrongly declared
  `windows-path-prefix` folds case too, and a case-sensitive NTFS
  directory can hold two files this normalizer reads as one.
- **A path *prefix* keeps its trailing separator; a path *value* does
  not.** D4's normalizer rules end with "trailing separator dropped",
  and applying that to the prefix an operator wrote would *widen* the
  grant: `/data/invoices/` normalized to `/data/invoices` is a byte
  prefix of `/data/invoices.bak/secret`. Normalization must never widen,
  so `normalize_prefix` re-appends it. (Dropping it from the *value* is
  safe — it can only shorten, which can only turn allows into denials.)
- **`absent = false` is a startup error.** D2 said "zero or two
  constraint keys" refuse; one key whose value negates the only meaning
  it has is the same class, and silently reading it as `Absent` would be
  the opposite of the author's intent.
- **The "grant names a tool no upstream offers" warning is emitted by
  the router, not the loader.** It is only knowable once every upstream
  has been listed, which happens inside `router::run`. Two warnings were
  added beside D2's list, both in the same "can only deny" class: an
  `expires` already in the past, and a `principal` set in a file with no
  grants.
- **`tools/call` params are parsed strictly even when unenforced.** D5
  writes the strict parse as part of the gate, but enforcement is about
  authority, not about tolerating an ambiguous frame — so an `arguments`
  that is not an object, or that carries a duplicate key, is refused on
  both paths. This is the one M1/M2 behaviour change that is not covered
  by an existing assertion.
- **Argument *names* are not capped in the trace.** D9 caps string
  *values* at 4 KiB, which is what it says and what was built; a
  client-supplied argument *name* is equally unbounded, so a hostile
  client can still write one frame's worth of key text per call into the
  log. Authority is unaffected. The fix belongs with T4's recorder,
  where redaction and the operator-facing cap knob already live.

## What the pre-PR review changed

The adversarial multi-lens review found six defects that the milestone's
own 282 tests and the full gate did not, three of them independently by
three lenses. All six are fixed on this branch, with the hostile inputs
kept as regression tests. Recorded because each generalises.

1. **A path prefix that normalizes to nothing became the
   everything-prefix.** `path-prefix = "."` compiled to `Prefix("")`,
   which admits every string; `path-prefix = "./"` was worse — D4's
   "re-append the trailing separator" rule turned it into `Prefix("/")`,
   promoting a *relative* prefix to the filesystem root. Either one
   silently deletes the path constraint, in the false-allow direction,
   from a grant file an operator might plausibly write. The step that
   exists to stop widening was the thing doing the widening.
   Fixed twice over: `normalize_prefix` no longer synthesises a root for
   an empty result, and the loader **refuses** a path-flavored prefix
   that normalizes to nothing. (`"/"` is still accepted — an operator who
   writes the root has said what they meant. The lesson worth keeping:
   *a "must never widen" property has to be tested at the degenerate
   inputs, not only the representative ones.*)
2. **The Windows flavor erased the UNC root.** `\\data\share\x` (a share
   on a host called `data`) and `\data\share\x` (a directory on the
   current drive) both normalized to `/data/share/x`, so a grant over the
   local directory admitted a write to a remote server — and the module's
   own rustdoc argued this was safe because it was "consistent on both
   sides". Consistency was the bug: it made two distinct resources
   indistinguishable, so no prefix could separate them. Now a **leading**
   run of two or more separators is preserved as exactly two, in both
   flavors — POSIX included, because the standard leaves a leading `//`
   implementation-defined and "I cannot tell" resolves to a denial.
3. **The duplicate-id refusal was an oracle for the tool table.** Because
   it happened after `ToolSet::route`, a reused id answered `-32600` for
   a name the upstreams offer and `-32602` for one they do not — letting
   a client enumerate exactly the tools the filtered `tools/list` hides,
   which is W6's whole point. The check moved ahead of routing; D5 only
   requires the claim to precede `authorize`, not to follow `route`.
4. **A stale response could be attributed to a newer call.** The router
   matched a response against the in-flight *id slot* and the upstream
   index, so a client that cancelled id X and immediately reused it
   toward the same upstream could receive the old call's payload as the
   new call's answer — and the trace would record the old outcome under
   the new `CallId`. M1/M2 had the delivery half of this; M5 added the
   audit half, which is worse, because a `CallCompleted` naming the wrong
   call is a false statement in the one artifact that exists to answer
   "what did this agent do?". `CallId` now travels with the request and
   back on the response, and the router requires it to match. The
   interleaving itself is not driven by a test — it needs a response
   queued behind two client frames, which the harness cannot schedule
   deterministically — so it is pinned at the unit level instead
   (`idmap`: a reused id always gets a fresh `CallId`) and is a candidate
   for T4's replay tooling.
5. **`ToolsListed` was recorded before the frame-cap check**, so a list
   too large to serve was traced as having been shown. Ordering fixed.
6. **The loader was silent about constraints that admit everything.** It
   warned about the fail-closed mistakes (`one-of = []`, inverted
   `range`) and said nothing about an empty `prefix`/`suffix`. Now both
   directions are reported.

Two claims were raised and **refuted**, and are worth recording as
non-defects: `path-prefix` without a trailing separator admitting sibling
names (that is documented byte-prefix behaviour — write the separator),
and the granted directory itself never matching its own prefix (a false
denial, and the conservative reading).

## Where the M5 fixes were still wrong — the post-T1 bug hunt

A second adversarial multi-lens review, run against `863ea59` after T1
closed, found fourteen defects behind a green gate of 289 tests. **Five
were false allows**, and four of those were in `normalize.rs` and its
loader guard — the module written to close exactly this class, whose own
M5 note above says the lesson was *"test the invariant at the degenerate
inputs"*. It had been applied to half the degenerate space. Those five
are fixed here; the rest are listed at the end of this section.

1. **The bare root was accepted, and it crosses roots.** The M5 note
   above says `"/"` is fine because "an operator who writes the root has
   said what they meant" — **that is now reversed**. What they meant
   cannot be expressed as a byte prefix: `"//host/share/x"` starts with
   `"/"`, so a grant on this machine's root also admitted a write to an
   arbitrary SMB host. The very separation defect 2 above introduced —
   local root distinct from UNC root — is defeated at the one value where
   the two roots share a prefix. Refused at load time now, alongside the
   empty prefix. *The generalisation: a fix that makes two things
   distinguishable has to be checked at the value where one is a prefix
   of the other.*
2. **The degenerate-prefix guard covered `.` and not `..`.**
   `path-prefix = "../"` loaded clean and admitted `../../etc/passwd`,
   because `normalize` *stacks* leading `..` rather than cancelling them,
   so byte containment stops being path containment. Same shape as
   defect 1 in the M5 list, same module, missed because the degenerate
   table listed the `.`-relative spellings only. Refused now.
3. **`..` climbed out of a UNC root.** The server and share were ordinary
   poppable segments, so `\attacker\pub\..\..\fileserver\reports\x`
   read as `//fileserver/reports/x` and passed a grant on
   `\fileserver\reports\` — while Windows clamps `..` at the share and
   writes to the attacker's host. Defect 2 above kept the *root* distinct
   but left the names inside it poppable, which is the same collapse one
   level down. The root is flavor-aware now: under Windows a UNC server
   and share, and a drive letter, are pinned. A POSIX `//` root is
   deliberately *not* pinned — there are no names in it to protect, and
   pinning would have invented a false allow rather than closing one.
4. **A `list_changed` re-list could move a granted tool to another
   upstream.** A `Grant` names a tool, not an upstream, so the binding
   lives only in `ToolSet`. `ToolSet::build` refuses two upstreams
   claiming one name at the same instant, but drop-then-claim across two
   re-lists reached that end state one step at a time, silently, and the
   grant still matched by name — so the call was allowed and forwarded to
   a server the config never granted. The session now ends
   (`SessionEnd::ToolRebound`) the way the one-step case does. *The
   generalisation: a check that rejects a bad state must also reject
   every path that reaches it incrementally.*
5. **A NUL let the normalizer cancel a segment the upstream still sees.**
   `/etc/passwd\0/../../data/invoices/x` reduced to `/data/invoices/x` —
   inside the grant — while the raw bytes crossed to the upstream, where
   a consumer that stops at the NUL opens `/etc/passwd`. The gate decided
   about a string no upstream resolves to. Such a value now becomes
   `ArgValue::Other`, which no constraint admits, and the spelling still
   reaches the trace via `args_as_sent`.

Nine further defects were confirmed and are **not** fixed here, to keep
this change to the authority axis: `initialize` forwards each upstream's
`instructions` unfiltered (an info leak naming tools the filtered
`tools/list` hides); the `classify` arm commented "Unreachable" is
reachable via a lone-surrogate escape; a 202 to a request POST is treated
as success so the call never completes; a pre-epoch clock un-expires
grants while the rustdoc claims the opposite direction; a stale
`insert_client` never releases its progress-token scope; the connection
actor stops draining the upstream while parked on a full outbound queue;
the `tools/list` size guard budgets a flat 64 bytes for a
client-controlled id; the in-flight tables are unbounded; and the HTTP
client has no request or read timeout.

**Calibration, since this now has three data points.** M5: 18 findings,
6 real. T1-close: 14 confirmed of 28 candidates, 5 false allows. Both
times roughly half of what survived verification was real, and both times
the false allows clustered in the newest code written to prevent them.
