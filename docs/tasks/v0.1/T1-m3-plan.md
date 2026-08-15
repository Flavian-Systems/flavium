# T1 / M3 — flavium-core types + TraceSink (milestone plan)

Status: **approved 2026-08-15, implemented the same day** (branch
`m3-core-types`). Milestone 3 of the approved [T1 plan](T1-mcp-proxy-core.md).
Two adversarial review passes (semantics/soundness; M4/M5/T2–T4 integration
fit) were run against a first draft; their findings are folded in. Where
the code and this plan disagree, the code and its rustdoc are right.

## Context

T1's M1/M2 (transparent multi-upstream proxy) and the docs track are merged;
`crates/flavium-core` is still the two-field placeholder. The T1 plan fixes
M3 as: **flavium-core types + TraceSink; invariant preserved: delegation
strictly attenuates — subset on every axis, property-tested; zero-dep, no
unsafe/unwrap; hand-rolled Error impls (no thiserror in core).**

M3 gives M4 (grant→Cedar compiler + authorizer) and M5 (wiring, JSONL sink,
CLI config) their vocabulary and makes the core invariant *executable and
tested* before any enforcement exists. No runtime behaviour changes: the
binary, the proxy crate, and the policy crate are untouched. One PR, core
only.

## Scope

**In:** rewrite `crates/flavium-core` (types, reference semantics,
attenuation, `Authorizer` + `TraceSink` traits, trace catalog, in-memory
sinks), unit + property tests, rustdoc stating the invariants, doc
touch-ups (T1 plan status/M3 note, architecture doc §10 sentence, glossary).
**Out:** flavium-policy (M4); proxy/CLI changes (M5); budgets (T2 — axis
reserved, D7); spawn/supervision (T3 — but `attenuates` is its check);
serde/JSONL (CLI, M5); hash chain, `ts`/`seq`/session id (recorder, M5/T4).

## Design decisions (each a sentence you may want to veto)

- **D1 — Principal factored out of `Grant`.** `GrantEnvelope { principal,
  grants }` is what an agent holds; `Grant { tool, constraints, expires }`
  is pure authority. DESIGN's tuple = `GrantEnvelope.principal × Grant`.
  Attenuation compares authority; who may hold a child envelope is T3's
  spawn check. `attenuates` therefore takes `&[Grant]`.
- **D2 — Reference semantics live in core (type-homes deviation, flagged
  in the T1 plan note).** `GrantEnvelope::decide(&ToolCall, now) ->
  Decision` is the executable specification of "outside the envelope";
  Cedar (M4) is the runtime engine, differential-tested against it. Without
  it INV-1 is only algebraic; with it we test the theorem's local form.
- **D3 — Constraint vocabulary = the T1 acceptance table + `Absent`.**
  `Constraint::{Prefix(String), Suffix(String), OneOf(BTreeSet<String>),
  Range{min: Option<i64>, max: Option<i64>}, Absent}` — one constraint per
  argument name (`BTreeMap<String, Constraint>`); `admits(Option<&ArgValue>)`
  so `Absent` admits exactly "argument missing" (the only way to forbid
  `cc`/`bcc` — the exfiltration case the demo needs; adding it later would
  change `decide`'s core rule). `ArgValue::{Str(String), Int(i64), Other}`,
  `Other` = anything no constraint can admit (float, bool, null, array,
  object). Rules, all fail-closed: constrained arg missing ⇒ deny (also
  blocks default-argument escapes); type mismatch or `Other` ⇒ deny; byte-wise
  string comparison, no normalization (path normalization stays in M5,
  ahead of the check); unconstrained args are not looked at. Rustdoc pitfalls
  spelled out: `Prefix("/data/inv")` admits `/data/invalid` (byte prefix,
  not path component); write suffixes with `@`; a scalar recipient holding
  several addresses passes `Suffix` — the T5 tool takes one address;
  list-valued args are `Other` in T1.
- **D4 — Attenuation is ⊆ (equal allowed) and conservative.**
  `attenuates(parent: &[Grant], child: &[Grant]) -> Result<(), Uncovered{child:
  usize}>` is sound, not complete (may refuse a semantic subset, never
  accepts a non-subset; a child grant covered only by the *union* of two
  parent grants is refused). Pairwise `Grant::covers(&self, child) ->
  Result<(), Axis::{Tool, Expiry, Constraint(arg)}>` in a fixed order (tool
  → expiry → constraints in key order); diagnostics are per candidate parent
  (advisory). `Constraint::includes(&self, other)` is a structural table:
  same-kind rows; `OneOf ⊆ Prefix/Suffix`; `Absent ⊆ Absent`; a "parent
  admits all strings" pre-row (`Prefix("")`/`Suffix("")` include any
  string-kind child); everything else `false`. Bounds and expiry are compared
  by two explicit 4-row `match` helpers (`lower_bound_within`,
  `upper_bound_within`) — never derived `Option` `Ord`, whose `None < Some`
  would accept a never-expiring child under an expiring parent.
- **D5 — Traits in core, no I/O.** `Authorizer: Send + Sync { authorize(&self,
  &Principal, &ToolCall, Timestamp) -> Decision; granted_tools(&self,
  &Principal, Timestamp) -> BTreeSet<ToolName> }` — home moved from
  flavium-policy to core (deviation, flagged) so the proxy depends on core
  only and never on Cedar; M4's Cedar authorizer implements it; `impl
  Authorizer for GrantEnvelope` is the reference (rustdoc: specification, not
  the runtime engine). `TraceSink: Send + Sync { record(&self, &TraceEvent)
  -> Result<(), Box<dyn Error + Send + Sync>> }` — fallible on purpose
  (recommended M5 policy: sink failure ends the session — fail closed on
  audit); blanket `impl for Arc<T>`; documented ordering contract (the
  router emits from one task in causal order, so a sink may assign
  `seq`/prev-hash under its lock). `MemorySink` (std `Mutex`,
  poison-tolerant) and `NullSink` ship in core.
- **D6 — Trace catalog defined now, exhaustive enum.** Deliberately not
  `#[non_exhaustive]`: every sink handles every variant at compile time (a
  T2/T3 addition is a wanted ripple). Events are clock-free (decisions
  carry the `now` they used); `ts`/`seq`/hash/session id are the recorder's.
  Catalog (all `Clone + Debug + PartialEq + Eq`), keyed by a per-session
  `CallId(u64)` the proxy mints:
  `SessionStarted{envelope}` · `HandshakeCompleted{offered_protocol_version,
  protocol_version, client_name, client_version}` (untrusted data, recorded
  so protocol drift is observable) · `ToolsListed{principal, now, offered:
  u64, granted: u64}` · `CallRefused{principal, call_id, tool:
  Option<String>, reason: RefusalReason::{MalformedParams, UnknownTool,
  DuplicateRequestId}}` · `CallDecided{principal, call_id, call: ToolCall,
  now, decision}` · `CallCompleted{principal, call_id, outcome:
  CallOutcome::{Result{is_error}, Error{code}, NotForwarded{UpstreamBusy |
  UpstreamUnavailable | Untranslatable}, Cancelled, Abandoned}}` ·
  `FrameRejected{code}` · `FrameDiscarded{kind: DiscardKind::{StrayResponse,
  UnroutableNotification, StaleResponse, CancelUnreadable, CancelNotInFlight,
  CancelNotForwarded, NotificationBeforeReady, UnknownResponseId,
  OutOfScopeProgress}}` · `UpstreamEnded{upstream, error: Option<String>}` ·
  `SessionEnded{reason: SessionEndReason::{ClientEof, ClientReadError,
  ClientWriteFailed, UpstreamGone{upstream}, ToolCollision{tool}, Internal},
  undelivered: u64, delivery_failed: bool}` (clean iff `ClientEof` and not
  `delivery_failed` — the exit-code criterion). A run that fails during
  startup has no session and leaves no trace (logs + exit code only). This
  is the T1 acceptance row "trace event (principal,
  tool, reason)" for every denial *and* refusal, and I11's counters as
  events. `CallDecided.call` carries the args `decide` saw (replay of the
  decision needs exactly envelope + call + now); size/redaction is an M5/T4
  sink knob, noted there.
- **D7 — Budget axis deferred to T2.** No `budget` field: an accepted but
  unenforced `max_calls` would be a lie. `Grant` rustdoc reserves it; T2 adds
  `Axis::Budget`, extends `covers`, adds `DenialReason::OverBudget`. Noted
  for T2: "first admitting grant" becomes load-bearing under per-grant
  budgets — hence `admitting_grants` (below) so `Decision` stays stable.
- **D8 — Time.** `Timestamp(i64)` unix seconds (Cedar `long`, chrono
  `timestamp()`); no arithmetic impls; live iff `now < expires` (boundary =
  expired). Callers supply `now`; core has no clock.
- **D9 — Names.** `Principal`, `ToolName`: validated newtypes (non-empty, no
  ASCII control chars), `new(&str) -> Result<_, InvalidName>`, `Display`,
  `Borrow<str>` (set lookups by `&str`). Only *grants* use `ToolName`;
  `ToolCall.tool` and trace tool fields are `String` — an upstream/client
  name that would fail validation can never equal a grant's name ⇒
  `NotGranted`, fail closed, no special path.
- **D10 — Zero dependencies, dev-deps included.** Property tests use a
  10-line SplitMix64 PRNG and small-scope generators. If you prefer
  `proptest` (shrinking), that is a dev-dependency needing your explicit
  approval — say so and D10 flips.

## Crate layout (`crates/flavium-core/src/`)

| File | Contents |
|---|---|
| `lib.rs` | crate docs (L1/L2, INV-1..6 below, "specification vs engine", Kani/Verus notes), `#![forbid(unsafe_code)]`, `#![deny(clippy::unwrap_used, clippy::expect_used)]` (test modules `#[allow]`, as the proxy does), re-exports |
| `name.rs` | `Principal`, `ToolName`, `InvalidName` |
| `time.rs` | `Timestamp` |
| `constraint.rs` | `ArgValue`, `Constraint`, `admits(Option<&ArgValue>)`, `includes`; crate-private `lower_bound_within`/`upper_bound_within` (explicit 4-row tables; in-crate harnesses) |
| `grant.rs` | `Grant`, `GrantEnvelope`, `ToolCall{tool: String, args: BTreeMap<String, ArgValue>}`, `Decision::{Allow{grant: usize}, Deny(DenialReason)}`, `DenialReason::{NotGranted, Expired, OutOfEnvelope, EvaluationError{detail: String}}`, `ToolStatus::{NotGranted, Expired, Live}`, `GrantEnvelope::{tool_status, admitting_grants, decide, granted_tools}` |
| `attenuate.rs` | `Axis`, `Uncovered`, `Grant::covers`, `attenuates` |
| `authorize.rs` | `Authorizer` trait, `impl Authorizer for GrantEnvelope` |
| `trace.rs` | `CallId`, the `TraceEvent` catalog and payload enums, `TraceSink`, `Arc` blanket impl, `MemorySink`, `NullSink` |
| `tests/properties.rs` | PRNG, generators, the property suite |

`decide` = `tool_status` (no grant for the tool ⇒ `NotGranted`; none live ⇒
`Expired`) then `admitting_grants` (indices of live grants whose every
constraint admits, in envelope order) ⇒ first index as `Allow{grant}` or
`OutOfEnvelope`. `EvaluationError` is never produced by `decide`; it is the
runtime engine's fail-closed answer and lives here so `Decision`/trace types
are shared. Pub fields on `Grant`/`GrantEnvelope`/`ToolCall`; constructors
only on the validated newtypes. Errors are hand-rolled `Display +
std::error::Error`. Every fn is total (no panics on any input).

## Invariants (crate-level rustdoc; each fn cites the one it maintains)

- **L1** `includes(p, c)` ⇒ ∀v∈Option<ArgValue>: `c.admits(v)` ⇒ `p.admits(v)`.
- **L2** `covers(p, c).is_ok()` ⇒ ∀call, now: c admits call ⇒ p admits call.
- **INV-1 attenuation soundness** — `attenuates(p, c).is_ok()` ⇒ ∀call, now:
  `decide_c` is `Allow{_}` ⇒ `decide_p` is `Allow{_}` (index-agnostic; over
  `&[Grant]`, principal-free). **INV-1b** (visibility attenuates too, proven
  from `covers`' tool + expiry axes, not a corollary of INV-1): ∀now
  `granted_tools_c(now) ⊆ granted_tools_p(now)`.
- **INV-2 deny by default** — empty envelope allows nothing; no grant for
  the tool ⇒ `NotGranted`.
- **INV-3 an expired grant is no grant** — `t ∈ granted_tools(now)` ⇔
  `tool_status(t, now) = Live` ⇔ ∀call on t: `decide ∉ {NotGranted, Expired}`.
- **INV-4 determinism/totality** — `decide`, `covers`, `attenuates`,
  `includes` are pure, total functions (no clock, no I/O, `BTreeMap`/`BTreeSet`
  order only, no arithmetic that can overflow).
- **INV-5 order** — `attenuates` is reflexive (self-delegation; "strictly"
  means enforced-⊆, not proper subset) and transitive (the root's envelope
  bounds the tree — DESIGN §3).
- **INV-6 monotonicity** — adding a parent grant, removing a child grant, or
  tightening a child (narrower prefix/suffix, smaller `OneOf`, tighter
  bounds, earlier or newly-`Some` expiry, added constraint) preserves `Ok`.

## Tests

- **Property suite** (`tests/properties.rs`; fixed seed; universes: tools
  {a,b,c}, args {x,y,z}, strings from {"", "/", "a", "b", "@", ".", "*", "\\",
  "é"}^≤3 (generated short-biased so prefix/suffix relations are common),
  ints −3..=3 plus sentinels {−4, 4, i64::MIN, i64::MAX}, times 0..=6,
  grants/envelope 0..=3, `OneOf` size 0..=2, per-arg call values ∈ {absent,
  Str, Int, Other}). Positive properties generate the child *from* the
  parent by random tightening steps — same-kind narrowing plus the
  cross-kind rows (`Prefix`/`Suffix` → `OneOf` of matching strings, empty
  `Prefix`/`Suffix` → any string kind) — and assert `attenuates` is `Ok`
  (doubles as completeness-in-practice), then check INV-1/INV-1b over
  sampled calls × every time; L1 is checked over the whole value universe
  for derived and independent constraint pairs with a per-row floor of
  non-trivial inclusions (child ≠ parent, child admits something) for every
  true row of the `includes` table; INV-5 (reflexive; transitive over
  derived and mixed chains); INV-6; INV-3 (incl. tool-name equality of the
  allowing grant, checked without going through `Grant::admits`).
  Independent-pairs runs of INV-1 and L2 stay, with asserted minimum counts
  of non-vacuous cases so a regression cannot trivialize them. Mutation
  checks performed by hand: derived-`Option` ordering on expiry, `Suffix ⊇
  OneOf` with `any` for `all`, inverted `Suffix ⊇ Suffix`, `Grant::admits`
  ignoring the tool — each fails the suite. Runs in ~0.15 s.
- **Failing-case tables** (unit tests beside code): every reason `decide`
  produces and every `Axis` reachable; per-axis loosening past the covering
  bound is `Err` (shorter prefix/suffix, `OneOf` gaining an outside element,
  bound widened or dropped to `None`, expiry later or `None`, dropped
  constraint, `Absent` vs present, tool no parent names); the 4-row bound
  helper tables; boundaries (`now == expires`, range ends, `Prefix("")` and
  `Suffix("")` admit all strings, empty `OneOf` admits none, `Other`, type
  mismatch, missing arg); an exhaustive one-representative-per-kind table
  asserting `includes` is true only on the documented rows; a grant for
  another tool lends no authority; an expired-but-admitting grant is skipped
  in favour of a later live one.
- Sinks: `MemorySink` records in order and survives poisoning; `NullSink`;
  `Arc<dyn TraceSink>` works. Names: empty/control-char rejection,
  `Borrow<str>` lookups. `impl Authorizer for GrantEnvelope` refuses a
  foreign principal (`NotGranted`).

## Docs & housekeeping (same PR)

- `docs/tasks/v0.1/T1-mcp-proxy-core.md`: status line "M3 landed" + an M3
  note listing the deviations/refinements (D1, D2, D3 `Absent`, D5 trait
  home, D6 catalog, D7), like the M2 deviation note.
- `docs/architecture/proxy-mcp.md` §10: one sentence naming the core types
  and traits that now exist; wiring text stays (M5).
- `docs/GLOSSARY.md`: "Grant" line (principal now the envelope's), "Grant
  envelope" → `GrantEnvelope`, `Absent`, attenuation is ⊆ and conservative,
  the unconstrained-args pitfall; the JSON-RPC "Envelope" entry is untouched.
- README/DESIGN untouched (no user-facing behaviour change).
- After merge: update the memory file (M3 done, D1–D10, M4 next, and the M4/M5
  notes below).

## Verification

Full gate before commit: `cargo fmt --all` · `cargo clippy --workspace
--all-targets -- -D warnings` · `RUSTDOCFLAGS="-D warnings" cargo doc
--workspace --no-deps` · `cargo test --workspace`. Core-specific: `cargo
tree -p flavium-core` prints only the crate; grep confirms no
`unwrap`/`expect`/`unsafe` outside `#[cfg(test)]`; property suite time
bounded. Then the usual adversarial multi-lens review pass on the code
(soundness of `includes`/`covers`, API fit for M4/M5, rustdoc as spec) with
fixes, before the PR.

## Delivery

Branch `m3-core-types` → PR `Add flavium-core grant types, attenuation
invariant, and TraceSink` (the squash message, 66 chars); commits `-s` with
the Co-Authored-By line; hand over the `pull/new/m3-core-types` URL.

## Notes carried forward to M4/M5 (recorded, not M3 work)

- **M4 (Cedar):** JSON policy format `like` takes `[{"Literal":…},"Wildcard"]`
  — no escaping to get wrong; `Prefix("")`/`Suffix("")` ⇒ `["Wildcard"]`.
  Build the Context from `ToolCall` as typed submaps (`str`, `int`, a
  `present` name set for `Absent`, `now`) with `Other` omitted and every
  reference guarded by `has`, so a type mismatch is *absent* and denies —
  Cedar then never errors on compiled policies and the differential test
  compares `Decision` exactly. Cedar's determining-policy set ⇒ min index;
  policy ids are grant indices. Cedar authorizer = `tool_status` + Cedar
  (so `NotGranted`/`Expired` precedence matches `decide`). Whether M5 runs
  both evaluators is an M4/M5 call.
- **M5 (wiring):** `arguments` absent/`null` ⇒ empty map; duplicate keys ⇒
  `-32602` (fail closed; `BTreeMap` cannot represent them); `-0`, `3.0`,
  `1e3`, out-of-i64 ⇒ `Other`; non-object `arguments` ⇒ `-32602` +
  `CallRefused{MalformedParams}`, not a `Decision`. `DenialReason` → client
  shape mapping lives in the proxy (`NotGranted`/`Expired` ⇒ `-32602` like
  an unknown tool; `OutOfEnvelope` ⇒ `isError` "denied by policy";
  `EvaluationError`'s client shape must be pinned). `ClientTable` must
  carry tool name + `CallId` for `CallCompleted`; `is_error` needs a
  read-only peek at `result.isError` (I1 unaffected). JSONL sink: flush per
  event, never stdout; `router::SessionEnd` may become core's
  `SessionEndReason`. Trace arg size/redaction knob.
- **T5 demo tool:** scalar recipient argument; `cc`/`bcc` constrained
  `Absent`.
