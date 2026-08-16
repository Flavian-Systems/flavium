# T1 / M4 — flavium-policy: grant→Cedar compiler + authorizer (plan)

Status: **approved 2026-08-16, implemented.** Milestone 4 of the approved
[T1 plan](T1-mcp-proxy-core.md); builds on [M3](T1-m3-plan.md). Every Cedar
shape below was verified against cedar-policy 4.12 in a throwaway spike before
this plan was written — the "verified" notes are things that were *run*, not
recalled.

Three things came out different from the plan below, all forced by what the
implementation found. They are recorded here rather than edited into the
decisions, so the plan still reads as what was approved:

1. **`request_context` returns a `Context`, not a `serde_json::Value`** (the
   crate-layout table says otherwise). D3's shape is unchanged — the same four
   keys — but the context is built from `RestrictedExpression`s instead of
   JSON. The JSON path was a real P1 break: `__expr` is a reserved escape in
   Cedar's *value* grammar, so a call whose only string argument was named
   `__expr` made the whole context fail to parse, and the engine denied a call
   the specification allows. Argument names are client-supplied and validated
   nowhere, so this was reachable from outside. P4 is restated in the crate
   docs as "nothing is ever parsed" rather than "nothing is ever interpolated":
   not being *formatted* into a grammar is no protection if you are still
   *read* by one.
2. **The `when` condition is a balanced tree of `&&`, not a left fold.** A
   grant with sixteen constrained arguments — an ordinary tool signature —
   overflowed the stack during Cedar's recursive parse and aborted the
   process. Halving makes the depth `log2(N)`; 4096 constrained arguments now
   compile. Reassociating is sound because every conjunct is total by
   construction. This changes the rendered form, which is what the
   compiled-form test exists to make visible.
3. **`cedar-policy = "4.12"`** is a caret range, not an `=` pin, matching every
   other dependency in the workspace. The drift the risk section cares about is
   caught either way: an API change fails to compile, an encoding change fails
   `a_representative_grant_renders_as_expected`.

Both defects were found by the adversarial review this plan calls for, and
both now have regression tests (`__expr` in the differential suite's hostile
universe; 16/64/4096 constrained arguments in `tests/encoding.rs`).

## Context

M3 gave the workspace its vocabulary: `GrantEnvelope`, `Constraint`,
`ToolCall`, `Decision`, the reference semantics `decide`, and the `Authorizer`
trait. M4 implements that trait for real in `flavium-policy` on top of Cedar —
the mandated engine — so M5 can wire enforcement into the proxy without the
proxy ever depending on Cedar.

The deliverable is one crate with two halves: a **compiler** (grants → Cedar
policies, once per session) and an **authorizer** (call → `Decision`, per
call). Nothing is wired: `flavium-proxy-mcp` and `flavium-cli` are untouched,
so the binary behaves exactly as today. One PR.

## Scope

**In:** `crates/flavium-policy` — dependencies, compiler, request context,
`CedarAuthorizer` implementing `flavium_core::Authorizer`, typed errors,
rustdoc, unit tests, and the differential test against `decide`.
**Out:** the TOML grant file and CLI wiring (M5 — see D6); budget metering
(T2, despite the crate's current module doc — that line gets corrected);
namespaces; anything in the proxy.

## Design decisions

Each decision states what it is, why, and what it rules out. Vocabulary
(policy, permit, policy set, context, schema, …) is fixed in
[GLOSSARY.md](../../GLOSSARY.md#policy-engine-cedar).

### D1 — One `permit` policy per grant, named by the grant's index

**Decision.** Every grant in the envelope compiles to exactly one Cedar
`permit`, whose policy id is that grant's position in the envelope (`"0"`,
`"1"`, …). The policy's scope pins the principal, the action
(`Flavium::Action::"call"`) and the tool; the grant's constraints and expiry
become its `when` condition. Nothing else is ever emitted — in particular no
`forbid`.

**Why.** Two properties fall out for free. First, deny-by-default is
structural rather than a rule that could be misordered: a call no permit
covers is denied because nothing allowed it. Second, when Cedar says "allowed",
it names the policy that did it, and that name *is* the grant index — so
`Decision::Allow { grant }` maps back to the exact grant with no bookkeeping on
our side, and the trace can point an auditor at the line of the grant file that
authorised a call.

**Instead of.** One big policy with a disjunction of every grant (loses the
"which grant" answer), or `forbid` policies for denials (unnecessary: absence
already denies, and mixing the two makes reasoning about precedence a new
thing to verify).

*Verified:* a policy set refuses duplicate ids, so index-as-id cannot collide.

### D2 — Compile to structured JSON, never to Cedar text

**Decision.** Policies are built as Cedar's JSON policy format (EST) and parsed
with `Policy::from_json`; entity UIDs are built with `EntityUid::from_json`.
No part of a grant is ever formatted into a string of Cedar source. The
per-constraint encodings, all verified to parse and evaluate:

| Constraint | Cedar |
|---|---|
| `Prefix(p)` | `context.str.<arg> like [{"Literal": p}, "Wildcard"]` |
| `Suffix(s)` | `context.str.<arg> like ["Wildcard", {"Literal": s}]` |
| `OneOf(set)` | `[…].contains(context.str.<arg>)` (empty set ⇒ never true) |
| `Range{min,max}` | `min <= context.int.<arg>` and/or `context.int.<arg> <= max`; an absent bound emits no comparison |
| `Absent` | `!(context.present.contains("<arg>"))` |
| expiry `Some(t)` | `context.now < t` |
| no constraints, no expiry | `when { true }` |

**Why.** Grant values are *data* — a path, an address pattern, a tool name —
and data must never be able to become syntax. Text-building a policy would put
a quote or a backslash from a grant file one escaping bug away from changing
what the policy means; the structured path has no such bug to make. This is the
T1 plan's "no string interpolation" rule, and it is why the T1 plan chose the
JSON format in the first place.

**Instead of.** `format!`-ing Cedar source and calling `Policy::from_str`
(readable, and exactly the injection seam this product exists to argue
against).

*Verified, and it changed the design:* `EntityUid::from_str` **fails** on ids
containing `"` or `\`, while `from_json` carries them intact — so text-built
UIDs would be both fragile and unsafe. The `like` pattern **must** be the
structured array; a plain `"pfx*"` string is rejected outright. And because the
literal is structured, Cedar escapes it itself: `Prefix("/a*b\c")` renders as
`like "/a\*b\\c*"`, matches `/a*b\c/d`, and does **not** match `/aQQb\c/d` —
so a `*` inside a grant is a plain character, and the escaping hazard the M3
notes flagged does not exist on this path. A test pins it.

### D3 — Arguments reach Cedar in typed submaps, behind `has` guards

**Decision.** The request context is always these four keys:

```json
{ "str": {"path": "/data/x"}, "int": {"n": 5}, "present": ["path", "n"], "now": 1700000000 }
```

String arguments go in `str`, `i64` arguments in `int`, `ArgValue::Other`
(floats, booleans, null, arrays, objects) is **omitted from both**, and
`present` lists the names of every argument the call supplied. Every
constraint reference is wrapped in `context.<submap> has <arg> && …`.

**Why.** It makes the two ways Cedar could *error* unreachable, which matters
because an engine error is a decision flavium has to make without the engine.
A missing argument fails its guard; an argument of the wrong type is in the
other submap (or in neither), so it also fails the guard. Both deny — which is
exactly what the reference semantics do, so the two agree instead of one
erroring and the other denying. `present` exists because `Absent` cannot
otherwise be said: "no value here" is not something a value-typed lookup can
express, so the set of supplied names is passed explicitly.

**Instead of.** One flat `context.args` record (a wrong-typed argument then
hits `like` and raises an evaluation error, splitting the two implementations
apart on exactly the inputs an attacker controls).

*Verified both directions:* with the guards, a wrongly-typed argument denies
with **no** Cedar error; and omitting a context key **does** raise "record does
not have the attribute" — hence the four keys are emitted unconditionally, as
invariant P5 with a test of its own.

### D4 — No Cedar schema

**Decision.** Flavium does not declare a Cedar schema and does not run the
validator; correctness of the generated policies is established by tests
instead.

**Why.** A schema's job is to catch type errors in policies before they run.
Ours are not written by anyone — they are generated from a closed vocabulary of
five constraint kinds, and D3 already removes the error class a schema would
catch. What a schema *would* add is a maintenance obligation with a nasty
failure mode: Cedar records must declare their attributes, so the schema would
have to list every argument name appearing in every grant of a given file, and
any drift between that list and the compiler would turn into blanket denials.

**Instead of.** Generating a per-grant-file schema and validating at load
(more moving parts, a new way to fail closed too aggressively, and no error
left for it to find). Revisit if T5's hardening finds a reason.

**Backstop, unchanged.** Any entry in the response's error diagnostics ⇒
`Deny(EvaluationError { detail })`. The engine failing is never a reason to
allow.

### D5 — `authorize` = principal check → `tool_status` → Cedar → classify

**Decision.** The four denial reasons are produced by two different mechanisms.
A principal that is not the envelope's holder, and the `NotGranted` /
`Expired` distinction, come from `flavium_core::tool_status` — pure, no Cedar.
Cedar is consulted **only** when the status is `Live`, and its answer maps as:
`Allow` ⇒ `Allow { grant: min(determining ids) }`; a clean `Deny` ⇒
`OutOfEnvelope`; any error diagnostics ⇒ `EvaluationError`. `granted_tools`
delegates to core entirely.

**Why.** Cedar answers Allow or Deny; it has no vocabulary for "the tool is not
in your envelope at all" versus "your grant for it expired" versus "these
arguments are outside it" — and that distinction is exactly what the client
sees (the first two are indistinguishable from an unknown tool, `-32602`; the
third is a recoverable `isError` result). Deriving it from the envelope keeps
the classification in one place and keeps `granted_tools` and `authorize`
agreeing on the tool axis by construction (core's INV-3), rather than by two
implementations happening to match.

**Instead of.** Trying to encode expiry-versus-envelope in Cedar policy ids or
diagnostics and reverse-engineering the reason from them (fragile, and it would
make the client-visible denial surface depend on engine internals).

*Verified, and it changed the design:* the determining-policy set comes back
**unordered** across runs, so taking the *minimum* index is required — not
cosmetic — to reproduce the reference semantics' "first admitting live grant".
*Corollary worth stating:* Cedar is only ever asked about a tool some grant
names, so the resource UID is built from the grant's validated `ToolName`, and
a client's arbitrary tool string never reaches the engine.

### D6 — The TOML grant file is M5's, not M4's

**Decision.** M4's public surface takes a `GrantEnvelope` and nothing else.
Reading `grants.toml`, validating it and turning it into core types lands in
M5 with the rest of the CLI configuration; M4's tests construct envelopes
directly.

**Why.** It keeps this crate's dependency list at the three approved crates
(no `toml` here), keeps M4 about one thing — *does Cedar agree with the
specification* — and matches the T1 plan's M5 line, which already assigns CLI
config to that milestone. The audit boundary is unaffected: whatever envelope
the loader produces, enforcement is sound; a loader bug is a config bug, in the
same class as today's proxy config.

**Instead of.** Putting the loader in flavium-policy (needs a new dependency
approval and mixes file parsing into the enforcement crate).

**M5 inherits** the file format, its validation, and its failing-case tests:
unknown keys, unparseable expiry, unknown constraint kind, a name with a
control character ⇒ refuse to start, fail closed.

### D7 — Compile once at startup, evaluate per call

**Decision.** `CedarAuthorizer::new(envelope) -> Result<Self, CompileError>`
compiles the whole envelope up front and holds the policy set, the envelope,
the principal UID and an empty entity store. `authorize` builds only the
per-call context.

**Why.** A grant that cannot be compiled should stop the process at startup —
when an operator is watching and no agent is running — rather than surface as a
mid-session denial that looks like policy. It also keeps the per-call path
allocation-light, and makes the compiled policy set a stable artifact that a
trace or a debug command can print.

**Instead of.** Compiling lazily per call (repeats work, and moves a
configuration error into the request path, where the only safe answer is a
denial the operator never sees the cause of).

## Dependencies

Already approved in the T1 plan; all still current on crates.io as of
2026-08-16: **cedar-policy 4.12**, **serde_json 1**, **thiserror 2**. No
dev-dependencies beyond those. `flavium-core` stays zero-dep and untouched.

## Crate layout (`crates/flavium-policy/src/`)

| File | Contents |
|---|---|
| `lib.rs` | crate docs: the enforcement seam, the invariants below, "specification vs engine"; re-exports |
| `compile.rs` | `compile(&GrantEnvelope) -> Result<PolicySet, CompileError>`; per-grant policy, per-constraint expression builders; `CompileError` (thiserror) |
| `context.rs` | `request_context(&ToolCall, Timestamp) -> serde_json::Value` — the four keys, always |
| `authorizer.rs` | `CedarAuthorizer` + `impl flavium_core::Authorizer`; the classification of Cedar's answer into `Decision` |
| `tests/differential.rs` | the equality property against `flavium_core::decide` |

## Invariants (crate rustdoc; each function names the one it keeps)

- **P1 (agreement)** — for every envelope, call and time,
  `CedarAuthorizer::authorize` returns exactly what `flavium_core::decide`
  returns, including the grant index and the denial reason. This is the
  milestone's headline property and the differential test's assertion.
- **P2 (deny by default)** — no policy matches ⇒ `Deny`; an envelope with no
  grants compiles to an empty `PolicySet` that allows nothing.
- **P3 (fail closed)** — any Cedar evaluation error, any unparseable
  determining id, any principal mismatch ⇒ `Deny`, never `Allow`.
- **P4 (no interpolation)** — every policy, entity UID and context value is
  built as structured JSON; no Cedar text is ever formatted from a grant or a
  call.
- **P5 (total context)** — the request context always carries `str`, `int`,
  `present` and `now`, so no policy can reference a missing attribute.

## Tests

- **Differential (the acceptance test):** generators mirroring M3's property
  suite (same small universes: 3 tools, 3 args, short-string alphabet
  including `*`, `\` and a multibyte character, `i64` sentinels, times spanning
  every expiry) build random envelopes and calls; assert
  `authorize(...) == decide(...)` — the full `Decision`, not just allow/deny —
  over thousands of cases, with an asserted floor of non-vacuous `Allow`s so
  the test cannot silently become trivial. Any mismatch prints the envelope,
  the call, the time, and the compiled policy text.
- **Per-axis denial table** (the T1 acceptance criterion, at the policy layer):
  path outside prefix, off-pattern recipient, out-of-range number, expired
  grant, ungranted tool, `Other`-valued argument, missing constrained
  argument, `Absent` violated — each with the exact `DenialReason`.
- **Cedar-specific rows:** literal `*`/`\` in a prefix or suffix matches
  literally and nothing else; empty `OneOf` admits nothing; `min > max` admits
  nothing; `i64::MIN`/`i64::MAX` bounds and values; a tool name containing `"`
  or `\` compiles and matches (the `from_json` path); multiple matching grants
  ⇒ the lowest index; a call whose argument is of the wrong type denies with
  **no** Cedar error.
- **Fail-closed rows:** foreign principal ⇒ `NotGranted`; a hand-built policy
  set that does error ⇒ `EvaluationError` (exercising the classifier
  directly); compile of an envelope whose grants are fine ⇒ never errors.
- **Compiled-form readability:** one test asserting the rendered Cedar text of
  a representative grant, so a reviewer can see what a grant becomes and a
  change to the encoding is visible in the diff.

## Verification

The full gate (`cargo fmt --all`, `cargo clippy --workspace --all-targets
-- -D warnings`, `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`,
`cargo test --workspace`), plus: `cargo tree -p flavium-core` still prints only
the crate (M4 must not leak a dependency into core); `cargo tree
-p flavium-proxy-mcp` does **not** contain cedar-policy; no `unwrap`/`expect`
outside tests in the new crate. Then the usual adversarial multi-lens review
with fixes before the PR.

## Delivery

Branch `m4-policy-cedar` → PR `Add flavium-policy: grant-to-Cedar compiler and
authorizer` (66 chars); `-s` sign-off; PR URL handed over.

## Risks

1. **Cedar semantics drifting from `decide`** — the differential test is the
   whole mitigation, and it is written first, against the generators M3 already
   proved non-vacuous.
2. **Cedar upgrade changing the EST or the diagnostics API** — the encodings
   live in one module behind `compile`, and the rendered-form test makes a
   silent change loud. The dependency is pinned to 4.12.
3. **Per-call cost** — one context allocation plus one `is_authorized`; if it
   ever matters, the measurement (not a guess) goes in T5's hardening.
