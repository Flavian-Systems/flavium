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

## Design decisions

Each decision states what it is, why, and what it rules out. All ten
shipped as written unless a note says otherwise; the deviations from the
[T1 plan](T1-mcp-proxy-core.md) (D1, D2, D3's `Absent`, D5, D6, D7) are
recorded in its status note. Vocabulary is fixed in
[GLOSSARY.md](../../GLOSSARY.md).

### D1 — The principal belongs to the envelope, not to the grant

**Decision.** `GrantEnvelope { principal, grants }` is what an agent holds;
`Grant { tool, constraints, expires }` is pure authority with no holder.
`attenuates` therefore takes `&[Grant]`, not envelopes.

**Why.** Attenuation compares *authority*, and a child envelope is held by a
different principal by construction. Had the principal stayed inside `Grant`,
the headline invariant — "child ⊆ parent on every axis" — would have needed a
permanent exception for the one axis that always differs, which is exactly the
kind of footnote that makes a security claim unverifiable. DESIGN's five-tuple
is not lost: it is `GrantEnvelope.principal × Grant`.

**Instead of.** Keeping principal in the grant (invariant with an exception),
or making `attenuates` compare envelopes (conflates two questions: *is this
authority narrower* — here — and *may this parent hand it to that child* —
T3's spawn check).

### D2 — The reference semantics live in core

**Decision.** `GrantEnvelope::decide(&ToolCall, now) -> Decision` — about a
hundred lines of deliberately boring code — is in `flavium-core` and is the
executable specification of what a grant means. Cedar (M4) is the runtime
engine and is tested against it. Flagged as a deviation from the T1 plan's
"type homes" line.

**Why.** Something has to *define* the meaning of a prefix constraint. If only
the engine defines it, there is nothing to test the engine against, and INV-1
(attenuation soundness) can only be checked algebraically — which is precisely
the kind of test that would have let an unsound `includes` row through. With
`decide` in core, M4 gets a differential test ("Cedar agrees with the
specification, on thousands of random cases") and the attenuation property gets
a semantic oracle.

**Instead of.** Authorization living only in `flavium-policy` (the T1 plan's
original split). The rustdoc is explicit that `decide` is the specification and
not the engine, so nobody mistakes it for the enforcement path.

### D3 — Constraint vocabulary = the T1 acceptance table, plus `Absent`

**Decision.** `Constraint::{Prefix, Suffix, OneOf, Range{min, max}, Absent}`,
one per argument name, over `ArgValue::{Str, Int, Other}` where `Other` is
anything no constraint can admit (float, bool, null, array, object). All rules
fail closed: a constrained argument that is missing, of the wrong type, or
`Other` is not admitted; comparison is byte-wise with no normalization (path
normalization happens in M5, before the check); arguments no constraint names
are not examined.

**Why.** The first four kinds are exactly the axes the T1 acceptance table
names (path prefixes, recipient patterns, numeric ranges, expiry). `Absent` was
added during planning because the fail-closed rule has a consequence that only
shows up when you write a real grant: *any* constraint on `cc` forces every
call to supply `cc`, so there was no way to say "this argument must not be
supplied" — which is precisely the exfiltration hole the flagship demo has to
close. Adding it later would have changed `decide`'s core rule inside the
verification target, which is the most expensive kind of late change.

**Instead of.** A general expression language for constraints (a bespoke
policy DSL, explicitly out of scope — we use Cedar), or deferring `Absent`.

**Pitfalls, spelled out in the rustdoc** because they are authoring traps, not
bugs: `Prefix("/data/inv")` admits `/data/invalid` (it is a byte prefix, not a
path component); write suffixes with the `@`; one scalar argument holding
several addresses still passes `Suffix`, so the T5 demo tool takes exactly one
recipient; list-valued arguments are `Other` in T1.

### D4 — Attenuation is ⊆ (equality allowed) and conservative

**Decision.** `attenuates(parent: &[Grant], child: &[Grant]) -> Result<(),
Uncovered>` is **sound but not complete**: it never accepts a child that can do
something the parent cannot, but it may refuse a child that is semantically a
subset (notably one covered only by the *union* of two parent grants). Per
grant, `Grant::covers` checks tool → expiry → constraints in a fixed order;
per argument, `Constraint::includes` is a structural table (same-kind rows,
`OneOf ⊆ Prefix/Suffix`, `Absent ⊆ Absent`, an "admits every string" pre-row
for `Prefix("")`/`Suffix("")`, everything else false). Bounds and expiry are
compared by two explicit four-row `match` helpers.

**Why.** Soundness is the property the theorem needs; completeness is a
convenience. Refusing a subset that happens to be written in a shape the table
does not recognise costs an operator a more explicit grant file — it never
costs authority, and it never grants any. The explicit bound helpers exist
because the obvious one-liner is wrong in a way that is invisible on the page:
derived `Ord` on `Option` puts `None` *below* every `Some`, so
`child.expires <= parent.expires` would accept a **never-expiring child under
an expiring parent**. That single line is the bug class this crate exists not
to have, so it is written as a table and mutation-tested.

**Instead of.** A complete inclusion check (cross-kind reasoning,
union-of-parents coverage): more code and more to verify, in the one crate
where both are most expensive, for no gain in what can be authorised.

### D5 — Both traits live in core, with no I/O

**Decision.** `Authorizer` (authorize + granted_tools) and `TraceSink`
(fallible `record`) are defined in `flavium-core`, with `MemorySink`/`NullSink`
and blanket impls for `Arc<T>`. `impl Authorizer for GrantEnvelope` is the
reference implementation. Flagged as a deviation: the T1 plan put the
authorization trait in `flavium-policy`.

**Why.** If the trait lived in `flavium-policy`, then `flavium-proxy-mcp` would
have to depend on `flavium-policy` — and therefore compile Cedar's ~50 crates —
merely to *name* the type it calls, and proxy tests could not run without
Cedar. Putting the seam in the dependency-free crate keeps the arrow pointing
one way and lets proxy tests use the reference implementation as a double.
`TraceSink::record` is fallible on purpose: it lets the runtime fail closed on
audit (the recommended M5 policy is that a sink failure ends the session — a
full disk should stop the agent, not run it unrecorded).

**Instead of.** The trait in `flavium-policy` (drags Cedar into the proxy), or
an infallible sink (makes "if it isn't traced, it isn't done" unenforceable).

### D6 — The whole trace catalog is defined now, as an exhaustive enum

**Decision.** `TraceEvent` ships complete in M3 rather than accreting through
M5, and is deliberately **not** `#[non_exhaustive]`. Events are clock-free: a
decision carries the `now` it was made with, while `ts`, `seq`, hashes and the
session id belong to the recorder.

**Why.** The T1 acceptance criterion wants a trace event for every denial *and*
every refusal, and the architecture doc's I11 says the router's counters become
events — so M5 needs the entire vocabulary on its first day; defining it later
would mean a second `flavium-core` PR inside a milestone whose scope is proxy
wiring, and each change to the verification target is a ceremony. Exhaustive so
that adding a variant in T2 or T3 is a compile error in every sink rather than
an event that silently fails to be written. Clock-free because replaying a
decision needs exactly envelope + call + `now`, and a core that reads the clock
is neither replayable nor verifiable.

**Instead of.** `#[non_exhaustive]` plus accretion (silent gaps in the audit
record, which is the one thing an audit record may not have).

Catalog (all `Clone + Debug + PartialEq + Eq`), keyed by a per-session
`CallId(u64)` the proxy mints: `SessionStarted{envelope}` ·
`HandshakeCompleted{offered_protocol_version, protocol_version, client_name,
client_version}` (untrusted data, recorded so protocol drift is observable) ·
`ToolsListed{principal, now, offered: u64, granted: u64}` ·
`CallRefused{principal, call_id, tool: Option<String>, reason:
RefusalReason::{MalformedParams, UnknownTool, DuplicateRequestId}}` ·
`CallDecided{principal, call_id, call: ToolCall, now, decision}` ·
`CallCompleted{principal, call_id, outcome: CallOutcome::{Result{is_error},
Error{code}, NotForwarded{UpstreamBusy | UpstreamUnavailable |
Untranslatable}, Cancelled, Abandoned}}` · `FrameRejected{code}` ·
`FrameDiscarded{kind: DiscardKind::{StrayResponse, UnroutableNotification,
StaleResponse, CancelUnreadable, CancelNotInFlight, CancelNotForwarded,
NotificationBeforeReady, UnknownResponseId, OutOfScopeProgress}}` ·
`UpstreamEnded{upstream, error: Option<String>}` ·
`SessionEnded{reason: SessionEndReason::{ClientEof, ClientReadError,
ClientWriteFailed, UpstreamGone{upstream}, ToolCollision{tool}, Internal},
undelivered: u64, delivery_failed: bool}` (clean iff `ClientEof` and not
`delivery_failed` — the exit-code criterion).

A run that fails during startup has no session and leaves no trace (logs and
exit code only). `CallDecided.call` carries the arguments `decide` saw, since
replaying a decision needs exactly envelope + call + `now`; trace size and
redaction are an M5/T4 sink knob, noted there.

### D7 — The budget axis waits for T2

**Decision.** `Grant` has no `budget` field in M3. Its rustdoc reserves the
axis; T2 adds it together with `Axis::Budget`, an extension to `covers`, and
`DenialReason::OverBudget`.

**Why.** A grant file that accepts `max_calls = 5` and does not enforce it is
a lie told in the security-critical artifact — worse than an unsupported key,
because the operator believes they are protected. "Every axis" in M3 therefore
means tool, constraints and expiry, and says so.

**Instead of.** Modelling the field now and enforcing later (a documented
gap that reads as a feature).

**Note for T2.** Under per-grant budgets, `decide`'s "first admitting grant"
becomes load-bearing: the meter will want the first admitting grant *with
budget left*. `admitting_grants` (all matching indices, not just the first)
exists so that can be done without changing `Decision`.

### D8 — Time is an `i64` of Unix seconds, supplied by the caller

**Decision.** `Timestamp(i64)` seconds since the epoch, comparison only (no
arithmetic impls). A grant is live iff `now < expires`, so the boundary
instant is already expired. Core never reads a clock; every time-dependent
function takes `now`.

**Why.** `i64` seconds is what Cedar's `long` and the usual `timestamp()`
accessors both speak, so no conversion can go wrong at the M4 boundary. A core
with no clock is what makes decisions replayable (T4 re-runs a session by
feeding back the recorded `now`) and what keeps the verification target free of
ambient state. Strict `<` makes the boundary unambiguous in one direction
rather than "it depends".

**Instead of.** `SystemTime`/`Instant` (not serialisable, not comparable
across a replay, and an invitation to read the clock inside core).

### D9 — Grants use validated names; calls and traces use plain strings

**Decision.** `Principal` and `ToolName` are newtypes validated on
construction (non-empty, no ASCII control characters), with `Display` and
`Borrow<str>`. Only *grants* use them; `ToolCall.tool` and the tool fields of
trace events stay `String`.

**Why.** Names travel into log lines, JSONL records and Cedar entity ids, so a
control character inside a *grant* is worth refusing at the door. But the name
in a call arrives from the client and is attacker-influenced: validating it
there would add an error path and a new client-visible failure for no gain,
because an unrepresentable name simply cannot equal any grant's name and
therefore falls out as `NotGranted`. Fail closed, no special case.

**Instead of.** Validating the call's tool name (a new error path that changes
the denial surface), or using `String` everywhere (loses the guarantee exactly
where names are written down).

### D10 — Zero dependencies, dev-dependencies included

**Decision.** The property suite uses a ten-line SplitMix64 generator and
hand-written generators rather than a property-testing crate.

**Why.** CLAUDE.md makes `flavium-core` dependency-light by rule, and a
*test* dependency is not free in a crate whose point is auditability: anyone
reproducing the proofs has to vendor and trust it too, and it appears in the
supply chain of the verification target.

**Instead of.** `proptest` — whose shrinking is genuinely better than what a
fixed seed gives us. The offer stands: it is a dev-dependency needing explicit
approval, and D10 flips the moment that is granted.

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
