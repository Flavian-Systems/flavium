# T1 / M5 — enforcement wired: gate, filter, grant file, trace (plan)

Status: **proposed 2026-08-16, awaiting approval.** Milestone 5 — the last
one — of the approved [T1 plan](T1-mcp-proxy-core.md); builds on
[M3](T1-m3-plan.md) (vocabulary) and [M4](T1-m4-plan.md) (engine). Every
external-API claim below was run in a throwaway spike before this plan
asserted it — `toml` 1.x's grant-file shape and what it does and does not
refuse, `serde_json`'s number classification and duplicate-key behaviour,
the TOML-datetime → Unix-seconds conversion against eight reference
instants, and the path-normalization table in D4 including both of its
false-allow directions. Statements marked *Verified* were executed; the
rest are reasoned.

## Context

M1/M2 made the proxy a faithful multi-upstream middlebox. M3 gave the
workspace its vocabulary and the reference semantics; M4 made Cedar answer
exactly what those semantics specify. Nothing is connected: today every
tool the upstreams offer is offered to the client, and every call is
forwarded.

M5 connects it, and only that. When it lands, T1's acceptance criteria are
met: *a real client works unmodified through the proxy* (M1/M2, re-run),
*a grant file denies out-of-envelope calls*, and *denials are logged*.

The shape of the milestone follows from one constraint: **`flavium-core`
and `flavium-policy` do not change** (D10). Every line of M5 lands in
`flavium-proxy-mcp` and `flavium-cli`, which is what a wiring milestone
should mean.

## Scope

**In:** the grant file and its loader (CLI); the `tools/call` gate and the
`tools/list` filter (proxy); path normalization; the trace-emission points
and the JSONL sink; the CLI surface (`--unenforced`, `--trace`); tests;
the doc set the milestone owes.

**Out:** budgets (T2 — the grant file refuses a `budget` key rather than
accepting one it cannot enforce, per M3's D7); delegation and supervision
(T3); the hash-chained recorder, replay, and the published trace spec (T4
— M5's JSONL carries `"v": 1` and is documented as unstable); the fuzz
harness (T5); tool namespacing; an HTTP server face.

## Design decisions

Each states what it is, why, and what it rules out. Vocabulary is fixed in
[GLOSSARY.md](../../GLOSSARY.md).

### D1 — One config file, and no accidental unenforced run

**Decision.** Grants live in the same `flavium.toml` as upstreams: a
top-level `principal` and any number of `[[grant]]` tables beside the
`[[upstream]]` ones. Enforcement is not a flag — it follows from the file
having grants. A config with **no** grants makes `flavium proxy` **refuse
to start**, naming the fix; the transparent middlebox M1/M2 shipped
survives only behind an explicit `--unenforced`, which logs a `WARN` every
session, is refused together with `--trace`, and is the only way to use
the `-- <COMMAND>` shorthand.

**Why.** DESIGN §5's demo promise is "one config file". More importantly,
the failure mode worth engineering against is an operator who believes
they are protected and is not: a proxy that silently forwards everything
because a section was missing is exactly that. Three postures are
possible — enforce, refuse, or forward — and only *refuse* makes the
absence of a grant file impossible to overlook. `--unenforced` disables
the trace because there is nothing honest to write: `SessionStarted`
carries an envelope, and recording an empty envelope for a session that
allowed everything would be a false statement in the audit record.

**Instead of.** A separate `--grants <FILE>` (two files to keep in sync,
and the same silent-forward hole when it is absent), or defaulting to
transparent with a warning (warnings are not read; this one would be the
only thing between an agent and every tool it can see).

*Verified:* `[[upstream]]` and `[[grant]]` coexist in one file under a
single `deny_unknown_fields` struct, and unknown top-level keys are still
refused.

### D2 — The grant file is a closed vocabulary, and the loader fails closed

**Decision.** The file, in full:

```toml
version = 1                        # the file format's version, required
principal = "invoice-bot"

[[upstream]]                       # unchanged from M2
name = "fs"
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/data"]

[[grant]]
tool = "read_file"
expires = 2026-09-01T00:00:00Z     # optional, TOML offset date-time (D3)
[grant.args]
path = { path-prefix = "/data/invoices/" }

[[grant]]
tool = "send_mail"
[grant.args]
to    = { suffix = "@yourco.com" }
cc    = { absent = true }
bcc   = { absent = true }
count = { range = { min = 1, max = 10 } }
kind  = { one-of = ["invoice", "receipt"] }
```

One constraint key per argument — `prefix`, `path-prefix`,
`windows-path-prefix`, `suffix`, `one-of`, `range`, `absent` — mapping
one-to-one onto `flavium_core::Constraint`. The loader **refuses to
start** on: a missing or unrecognised `version`; an unknown key anywhere;
zero or two constraint keys on one argument; a `principal` or `tool` that
`Principal::new`/`ToolName::new` rejects (empty, or an ASCII control
character); an `expires` without a UTC offset (D3); a `range` with neither
bound. It **warns** and continues on a grant whose tool no upstream offers,
an empty `one-of`, and a `range` with `min > max` — each is a grant that
admits nothing, which is the safe direction but almost always a typo.

**Why.** Everything in this file is a security decision written by hand,
so every ambiguity has to become a startup error while an operator is
watching, never a mid-session denial that reads as policy. The split
between *refuse* and *warn* follows the direction of the mistake: a
malformed file could mean anything, so it stops the process; a grant that
can only deny costs availability, never authority, so it is reported and
kept. Refusing an unknown key is what makes `budget = 5` — the T2 axis
M3's D7 deliberately did not model — an error rather than a lie.

**Instead of.** Serde enum tagging for constraints (`kind = "prefix"`,
`value = "…"`) — more ceremony per line in the file operators read most
often; or accepting unknown keys for forward compatibility, which in a
policy file means silently ignoring the operator's intent.

**Forward compatibility, and what `version` is actually for.** Note first
what it is *not* for: a file written for a richer future flavium, read by
an older binary, already fails closed — the new key is an unknown key and
the process refuses to start. That is the dangerous direction, and
`deny_unknown_fields` covers it without a version field.

`version` exists for the direction nothing else can catch: an **old file
whose keys still parse but no longer mean what its author meant**. If
`prefix` ever became a path-component prefix rather than a byte prefix, or
`expires` moved off its exclusive boundary, an unversioned file would be
silently re-interpreted — and in a policy file, re-interpreted can mean
*widened*. So the contract is stated now, while there is one version:

- adding an **optional** key does not bump — old files still mean exactly
  what they said;
- changing what an existing key means, removing one, or changing a default
  **bumps**;
- a binary accepts the versions it implements **by exact match** and
  refuses every other value by number, naming what it does support.

And when a bump comes, the new binary **refuses** the old dialect rather
than interpreting it, pointing at a migration note. That is the unusual
choice here, so it is the one worth defending: silent multi-dialect
support is what you want from a build tool and the opposite of what you
want from a grant file, where the right response to "these words changed
meaning" is a human re-reading their own grants. It also costs one match
arm instead of two loaders, which is why it is likely to survive contact
with a release.

**Instead of (versioning).** No version at all (the semantic-drift hole
above, in the one file where drift means authority); an optional `version`
defaulting to the current one (the default *is* the reinterpretation —
it silently answers the question the field exists to ask); a semver string
(invites range logic, and ranges are how a file ends up interpreted by
rules it never declared); or pinning a minimum flavium version (couples a
policy file to release cadence rather than to meaning).

The cost is real and paid now: every existing config, every example in
`docs/cli.md`, the e2e fixtures and the demo `flavium.toml` gain a line.
M5 is the moment to pay it, because the file's shape is changing anyway —
doing it in T2 would break files that had settled.

The version does **not** need to reach the trace, which is worth saying
because it is the obvious next question. `SessionStarted` records the
compiled `GrantEnvelope`, not the file that produced it — so an auditor
reads the authority that was actually in force, in core's vocabulary,
whatever dialect it was written in. Recording the file version too would
need a change to a core type, which D10 forbids for good reason and which
would buy nothing an envelope does not already say.

*Verified, and it changed the loader's job list:* `deny_unknown_fields`
catches unknown top-level, grant, and constraint keys, and TOML itself
rejects duplicate keys at both levels — so those need no code. But TOML
**accepts** an argument table with zero constraint keys and one with two,
and **accepts** a control character in a quoted tool name, so those three
checks are the loader's own. The core newtypes catch the third by
construction, which is why they exist.

### D3 — Expiry is a TOML offset date-time, converted without a dependency

**Decision.** `expires` is written as a native TOML offset date-time
(`2026-09-01T00:00:00Z`, or with an explicit `+02:00`). A local date-time,
a bare date, or a bare time is **refused at startup**. Conversion to
`Timestamp` is ~15 lines of `days_from_civil` in the CLI; no date/time
crate is added.

**Why.** A grant's expiry is the one field where "what did the operator
mean?" must have exactly one answer: a naive `2026-09-01T00:00:00` means
different instants in different places, and a security artifact may not
have an ambiguous field. Using TOML's native type gets the syntax
validated by the parser rather than by us, and refusing the offset-less
forms is a one-line check on a field the parser already separates.

**Instead of.** Accepting an RFC 3339 string (re-implements what TOML
already parses, and invites `expires = "soon"`), or adding `jiff`/`time`
to the CLI (a fine crate for a problem that is fifteen tested lines here;
the offer stands if the review prefers it).

*Verified:* the conversion reproduces eight reference instants exactly —
the epoch, a leap day, the 2100 non-leap century boundary, a pre-epoch
negative, the 2038 boundary, and `+02:00` agreeing with its `Z`
equivalent. TOML parses the offset-less forms happily and hands back a
`Datetime` with `offset: None`, which is the check.

### D4 — Path normalization is opt-in per argument and declares its flavor

**Decision.** `path-prefix` (POSIX: `/` separates) and
`windows-path-prefix` (both `/` and `\` separate) are the only constraints
that normalize. Both compile to a plain `Constraint::Prefix` over a
normalized prefix; at call time the proxy normalizes *that argument of
that tool* the same way before building the `ToolCall`. Plain `prefix`,
`suffix`, and `one-of` compare bytes, unchanged. The normalizer is byte
level and total: separators unified, repeated separators collapsed, `.`
segments dropped, `..` resolved against the previous segment, never
escaping the root of an absolute path, a leading `..` of a relative path
kept, trailing separator dropped. No case folding, no filesystem access,
no symlink resolution. A `(tool, argument)` named by both a path-flavored
and a non-path constraint anywhere in the file is a startup error.

**Why.** Flavium decides on the *spelling* of an argument; the upstream
acts on the *resource* that spelling resolves to. Every gap between our
model of that resolution and the upstream's becomes a **false allow** or a
**false denial** (both defined in [GLOSSARY.md](../../GLOSSARY.md#enforcement-the-vocabulary-the-rest-of-v01-builds-on)),
and the two are not equally bad: a false denial announces itself and costs
availability, a false allow is silent and costs the guarantee. That
asymmetry decides every row below.

The decision splits into two questions that look like one.

#### Which values get normalized

**A — nothing.** Fails the first row of the T1 plan's per-axis denial
table (*path outside prefix, including `../` and `\`*): under
`Prefix("/data/invoices/")` the call
`read_file("/data/invoices/../../etc/passwd")` is a *byte* prefix match,
so the reference semantics **allow** it — verified against
`flavium-core` — while the upstream reads `/etc/passwd`. A false allow,
reachable by anything that can influence one argument.

**B — every string argument.** Unsound, not merely sloppy: normalization
preserves meaning only for values that *are* paths, and applied to an
address, a pattern or document text it silently changes what the decision
was about. It also manufactures authority — `Prefix("/data/")` with the
value `\data\x` normalizes to `/data/x` and allows, while a POSIX upstream
sees one file named `\data\x` in the working directory (verified: that
pair flips from deny to allow).

**C — only arguments a grant marks as a path.** Chosen. Whether an
argument is a path is the one fact the runtime cannot infer and the
operator always knows.

**D — require the value to already be canonical; deny otherwise.** The
strongest option on paper, and the one worth recording: it is sound, it is
less code, and it buys a property C cannot — the decision is about exactly
the bytes forwarded, so the parser-differential class disappears instead of
being managed. Rejected on the denial surface: the ordinary `./` and
doubled separators that clients and models emit constantly would answer
`denied by policy`, a message that by design leaks nothing, so the agent
has no way to learn it should re-spell the path. It turns an ergonomics
problem into a denial indistinguishable from a real breach. Still
available as a T5 hardening option if C proves shaky.

#### What `\` means

Given C, the normalizer must still decide whether a backslash separates.
Both fixed answers are wrong, and both directions were run:

| value | grant | POSIX reading | Windows reading |
|---|---|---|---|
| `\data\invoices\x` | `Prefix("/data/invoices/")` | deny — correct, that is one filename on POSIX | **allow — false, if the upstream is POSIX** |
| `C:\Users\me\Desktop\..\..\Administrator\secrets` | `Prefix("C:\Users\me\Desktop\")` | **allow — false, `..` never resolves without a separator** | deny — correct |

**i — `/` only, always.** Row 2: on Windows the operator writes the prefix
the way Windows spells it, and no `..` ever resolves. False allow.

**ii — `/` and `\`, always.** Row 1: false allow on POSIX.

**iii — from `cfg!(windows)`.** The proxy's own host says nothing about
where an upstream resolves paths: an HTTP upstream is another machine
entirely, and a stdio child can be in WSL or a container. A guess, and an
invisible one.

**iv — declared per grant, as two constraint spellings.** Chosen. A grant
names one tool; a tool belongs to one upstream; so the grant is exactly the
scope at which the operator knows the answer. Visible in the file an
auditor reads, and a wrong choice is a config error someone can see rather
than a platform guess nobody can.

**v — require both readings to agree.** Sound on both platforms with no
declaration at all, which is genuinely attractive. It dies on the demo:
`C:\Users\flavi\Desktop\notes.txt` fails the POSIX reading, so *every*
legitimate Windows path is denied. (This follows from the rows above
rather than from a separate run.)

**vi — one `path-style` per file.** Simpler than iv, and wrong for the
shape M2 exists to serve: a local Windows filesystem server beside a POSIX
HTTP upstream needs two answers in one file.

**Instead of.** Summarised: A (fails the acceptance row), B (manufactures
authority), D (sound but turns `./` into an unexplainable denial), i/ii
(each a false allow on one platform), iii (an invisible guess), v (denies
every Windows path), vi (one answer where a mixed deployment needs two).

**Consequences, stated plainly.** The decision is made on the normalized
value while the *original* bytes are forwarded (I1 is preserved), so the
trace records the normalized call — the form the decision was actually
about (D9). Case-insensitive filesystems can therefore refuse a call the
upstream would have served (`/DATA/x` under `/data/`): a false denial,
never a false allow. And normalization models path resolution, not the
filesystem: symlinks and hardlinks are outside what a proxy can see, which
is DESIGN §7's boundary and v0.2's job.

### D5 — The gate: route, claim the id, decide, then forward

**Decision.** `Session::on_tools_call` becomes, in order: read `name` and
`arguments` → `ToolSet::route` → `ClientTable::insert` → `Authorizer::
authorize` → `Command::Forward`. Each step has one client-visible answer
and one trace event:

| Step fails | Client sees | Trace |
|---|---|---|
| `arguments` not an object, duplicate keys, no `name` | `-32602` "Invalid params" | `CallRefused{MalformedParams}` |
| no upstream offers the tool | `-32602` "Unknown tool: x" | `CallRefused{UnknownTool}` |
| request id already in flight | `-32600` | `CallRefused{DuplicateRequestId}` |
| `Deny(NotGranted \| Expired)` | `-32602` "Unknown tool: x" — byte-identical to the row above | `CallDecided{Deny}` |
| `Deny(OutOfEnvelope \| EvaluationError)` | `result` with `isError: true`, `"denied by policy"` | `CallDecided{Deny}` |

Arguments convert per M3: strings to `Str`, integers that fit `i64` to
`Int`, everything else to `Other`; a denial removes the in-flight entry.
`now` is read once per call from a `Clock` seam and used for both the
decision and its trace event.

**Why.** The order is forced by what each step needs. Routing first
because the enforcement hook the architecture doc committed to sits
between `route` and `Forward`, and because a granted tool that no upstream
serves must not produce an `Allow` for a call that then cannot happen.
Claiming the id *before* deciding so that a duplicate id never produces an
`Allow` followed by a refusal for the same `CallId` — one call, one
terminal event. Deciding last so the denial is the last word.

The two client shapes are the T1 plan's denial surface: `NotGranted` and
`Expired` are indistinguishable from a tool that does not exist, which is
what makes the filtered `tools/list` consistent rather than a hint; an
`OutOfEnvelope` denial is agent-visible and recoverable because the agent
did name a tool it holds and can try again within the envelope.
`EvaluationError` gets the same shape as `OutOfEnvelope` — the client
learns nothing about engine internals, and its detail goes to the trace
and the log where an operator will see it.

**Instead of.** Authorizing before routing (an `Allow` for a call that
cannot be forwarded, and a trace that says a tool was used when it was
not); or answering `EvaluationError` with `-32603` (tells the agent the
failure is not its fault, which invites the retry loop).

*Verified:* `-0`, `3.0`, `1e3`, `1E2` and values past `i64::MAX` all
classify as `Other`; `i64::MIN`/`i64::MAX` survive exactly. A duplicate
key needs a hand-written visitor — deserializing into a `BTreeMap`
silently keeps the last one, which would let a call be decided on
arguments the upstream will not see.

### D6 — `tools/list` shows the tool axis, nothing finer

**Decision.** `on_tools_list` projects the merged table through
`Authorizer::granted_tools(principal, now)`: a tool is shown iff some live
grant names it. Constraints do not filter — a tool whose every call would
be denied is still listed. `ToolsListed{offered, granted}` records both
counts, and the frame-size check applies to the filtered list.

**Why.** This is core's INV-3 and INV-1b made visible: the list agrees
with `authorize` on the tool axis by construction, so an expired grant
makes a tool vanish *and* makes its calls answer `-32602`, one fact with
two faces. Filtering any finer is not possible without lying: whether a
call is in the envelope depends on arguments that do not exist yet at list
time.

**Instead of.** Rewriting each tool's `inputSchema` to advertise the
constraints (tempting — it would steer the model away from denials — but
it edits bytes I1 promises to forward, leaks the grant file's contents to
whatever reads the tool list, and can only ever approximate the policy).

*Verified:* `BTreeSet<ToolName>::contains(&str)` works through
`Borrow<str>`, so the filter needs no allocation per tool.

### D7 — One enforcement seam, and the proxy still does not know Cedar exists

**Decision.** `router::run` takes an `Enforcement` bundle:
`principal: Principal`, `authorizer: Arc<dyn flavium_core::Authorizer>`,
`sink: Arc<dyn TraceSink>`, `clock: Arc<dyn Clock>`, and the
`(tool, argument) → path flavor` map D4 needs. `Clock` is a proxy-side
trait (`SystemClock` in production, a settable one in tests) because core
is clock-free by rule. The CLI is the only place that constructs a
`CedarAuthorizer`.

**Why.** M3's D5 put both traits in the dependency-free crate precisely so
this parameter list can exist without dragging Cedar's ~50 crates into the
proxy or its tests. The dividend is immediate: the proxy's own tests use
`GrantEnvelope` — the reference implementation — as the authorizer, so
they test *wiring* against the specification, while the CLI's end-to-end
tests exercise the real engine. A `Clock` in the proxy rather than in core
keeps the verification target free of ambient state and is what makes
expiry testable at all.

**Instead of.** Passing a concrete `CedarAuthorizer` (proxy depends on
Cedar), or reading the clock inside core (unreplayable, and T4 needs to
feed a recorded `now` back).

### D8 — Only the serve loop traces, and a sink failure ends the session

**Decision.** Every `TraceEvent` is emitted from `router::run` or its serve
loop — one task, causal order. Connection actors, which are separate
tasks, emit none: the two `DiscardKind`s that are theirs
(`UnknownResponseId`, `OutOfScopeProgress`) stay unrecorded in T1, as the
architecture doc already says. What an actor knows and the router does not
— whether a response carried `isError`, an error code, or a substituted
untranslatable-response error — travels to the router inside the existing
`Event::Response`. `TraceSink::record` returning `Err` ends the session:
the proxy stops answering, tears down, and exits non-zero, with a
proxy-side `SessionEnd::TraceFailed` for the exit path and a final
best-effort `SessionEnded{Internal}` for the record.

**Why.** M3's trace docs promise a sink "events in causal order, from one
task at a time per session" — that is a property the emitter has to
maintain, and the cheapest way to maintain it is to have exactly one
emitter. Ending the session on a sink failure is the other half of
`record` being fallible at all: a full disk should stop the agent, not run
it unrecorded. The session-end reason is split because the two audiences
differ — the operator needs to know *audit failed* (exit code, log line),
while the trace's own vocabulary gains nothing from a variant describing
the event a broken sink cannot write.

**Instead of.** Sinks called from actor tasks (needs a lock and gives up
causal order — the ordering a hash chain will depend on in T4); logging a
sink failure and continuing (makes "if it isn't traced, it isn't done"
unenforceable at exactly the moment it matters); adding
`SessionEndReason::TraceFailed` to core (a change to the verification
target for a record that, by construction, usually cannot be written).

**Cost, accepted:** `record` is synchronous file I/O on the serve loop.
For a local append that is microseconds; if T4's recorder makes it more,
it moves to its own task and this decision is revisited there.

### D9 — What the JSONL sink writes

**Decision.** One JSON object per line, appended, flushed per event, file
opened at startup (so a bad path fails while the operator is watching) and
created `0600` on unix. Every line carries `"v": 1`, a monotonic `seq`, a
`ts` in Unix milliseconds, and a session id (`<start-secs>-<pid>`) — the
four things M3 reserved for the recorder. `CallDecided` records the
`ToolCall` **as evaluated** — normalized (D4), `Other` values as a type
tag with no payload. A string argument longer than **4 KiB** is recorded
as its first 4 KiB **plus its full byte length and the SHA-256 of the
whole value**; shorter ones are recorded whole and are not hashed.
Truncation is
a *sink* concern only — the decision was made on the complete value, never
on the prefix. Serialization is hand-written in the CLI; core stays
serde-free.

**Why.** The trace has to answer "why was this allowed?" — which needs the
values the decision was made on, not the ones on the wire, or the record
does not reproduce the decision (M3: replay needs exactly envelope + call
+ `now`). The truncation cap exists because an argument can be a megabyte
of document text and an audit log must not become a copy of the data
plane. `0600` because that file now contains the arguments of every call.
`"v": 1` because T4 publishes this as a versioned spec and will change it.

**Why 4 KiB and not a rounder number.** It is `PATH_MAX` — the longest
path Linux can express (4096 including the terminator). That makes the cap
a boundary with a meaning rather than a taste: **a path argument is never
truncated**, which matters because a path is the one value in the whole
trace an auditor most needs whole. "Which file did it read?" is the
question this record exists to answer, and a hash does not answer it.

The number also closes a small hole. If the cap sat below the longest
legal path — at 1 KiB as first drafted, or at 2 KiB — an attacker who
learned where it sat could pad a path until the interesting tail fell off
the record. Authority is unaffected either way (the decision uses the full
value, and the digest still identifies it), but the audit line becomes
deliberately less legible, and at `PATH_MAX` that move simply does not
exist. The cost of going from 1 KiB to 4 KiB is nothing that matters: the
case the cap defends against is a *megabyte* of document text, which 4 KiB
stops exactly as well.

Windows long paths (`\\?\`, up to ~32 K UTF-16 units) can still exceed it
and will truncate — exotic enough to accept, and the digest and length
still identify them.

**Why the digest, and only past the cap.** Truncation is the one place
this milestone deliberately *loses* information, so it should say what it
dropped. Length alone already answers "a 3 MB body or a 1.1 KB one"; the
digest adds identity — two calls carrying the same payload are visibly the
same payload, so a retry loop, the same document sent to two recipients,
or a value matching one recovered from an upstream's own logs are all
readable off the trace instead of inferred. It is also a commitment: when
T4's recorder retains full frames, a T1-era line can be checked against
one.

Hashing *only* past the cap is the deliberate half. Below it the plaintext
is already there, so a digest would be noise — and worse than noise for
privacy, because a short low-entropy value (an address, an identifier, a
token) is enumerable from its hash, while a kilobyte of text is not. So
the rule that keeps the log small and the rule that leaks least happen to
be the same rule. The digest covers exactly the bytes the record is
derived from — the evaluated value — so the line is internally
consistent; in practice nothing normalized is ever this long, since
path-flavored arguments are paths.

**Instead of.** Recording argument names only (cannot answer the question
the trace exists for); the raw pre-normalization values (a record that
disagrees with the decision it records); hashing *every* string (noise
below the cap, and a brute-forceable digest of exactly the short sensitive
values); dropping the cap altogether and relying on the digest (the cap
exists so the audit log is not a copy of the data plane — no cap bounds
nothing); a per-argument-kind cap, exempting only the arguments D4 already
marks as paths (principled, but it couples the sink's output to the grant
file, and `PATH_MAX` covers the same ground with one number); or a
non-cryptographic hash (`DefaultHasher` is documented as unstable across
releases, and a collidable digest in an audit artifact is worse than none
because it invites reliance).

The cap stays fixed in M5. Making it an operator knob, with redaction
beside it, is the T4 recorder's business — M3 already parked it there.

### D10 — `flavium-core` and `flavium-policy` are not touched

**Decision.** M5 changes no file in either crate. If the wiring seems to
need one, that is the signal to stop and ask rather than to edit.

**Why.** CLAUDE.md's rule that outranks the others makes those two crates
the enforcement core and the verification target; a milestone whose whole
job is plumbing is the last place a change to them should originate. It is
also a real review property: `git diff --stat` naming only
`flavium-proxy-mcp`, `flavium-cli`, `docs/` is checkable in one line, and
the attenuation invariant, the reference semantics, and P1–P5 are
therefore untouched by construction rather than by argument.

**Instead of.** Convenience additions — a `SessionEndReason` variant (D8),
serde derives for the JSONL sink (D9), a `normalize` helper next to
`Constraint` (D4). Each is one line in core and one more thing an auditor
has to re-read.

## Dependencies

**One added, approved 2026-08-16: `sha2` 0.11 in `flavium-cli`**, for
D9's digest. `flavium-proxy-mcp` is unchanged, `flavium-core` stays
zero-dep, and `flavium-policy` keeps its three approved crates — the
enforcement core does not grow, which is D10's point restated in the
dependency graph.

Two things made it cheap. It is spent in `flavium-cli`, which is not the
verification target and whose crates the T1 plan lists for transparency
rather than approval; and T4's hash-chained recorder needs a cryptographic
hash regardless, so this is an early dependency rather than a new one —
better to have it under test in a small place first. The alternative was
hand-rolling SHA-256: well specified and NIST-vectored, but still bespoke
crypto in an audit artifact for no gain over a RustCrypto crate.

*Verified:* `sha2` 0.11 is the current release, and its digests match the
three standard SHA-256 vectors (empty, `"abc"`, the 56-byte two-block
one) — so the milestone's own truncation test can assert against a known
value rather than against itself.

The other place a dependency was considered and declined is D3.

## Layout

| File | Change |
|---|---|
| `flavium-proxy-mcp/src/enforcement.rs` | *new* — `Enforcement`, `Clock`/`SystemClock`, the path-flavor map |
| `flavium-proxy-mcp/src/args.rs` | *new* — `tools/call` params → `ToolCall`; duplicate keys and non-objects refused |
| `flavium-proxy-mcp/src/normalize.rs` | *new* — the two-flavor normalizer (D4); pure, total, fuzz-ready |
| `flavium-proxy-mcp/src/router.rs` | the gate (D5), the filter (D6), every emission point (D8), `SessionEnd::TraceFailed` |
| `flavium-proxy-mcp/src/idmap.rs` | `ClientTable` carries tool name + `CallId` per in-flight call |
| `flavium-proxy-mcp/src/toolset.rs` | `merged_result` filtered by a granted-name predicate |
| `flavium-proxy-mcp/src/connection.rs` | `Event::Response` carries the outcome the actor already knows |
| `flavium-cli/src/grants.rs` | *new* — the grant file: parse, validate, `GrantEnvelope` + flavor map |
| `flavium-cli/src/trace.rs` | *new* — the JSONL sink |
| `flavium-cli/src/main.rs` | `--unenforced`, `--trace`, wiring `CedarAuthorizer` |

## Invariants (asserted by tests, stated in the rustdoc)

- **W1 — no authority without a grant.** The only path to
  `Command::Forward` for a client request runs through a `Decision::Allow`.
- **W2 — visibility ⊆ authority.** Every tool in a `tools/list` result has
  a live grant at the `now` used (core's INV-3/INV-1b, at the wire).
- **W3 — fail closed on audit.** After a sink error no further client frame
  is answered and the process exits non-zero.
- **W4 — the core is untouched.** No change under `crates/flavium-core` or
  `crates/flavium-policy`.
- **W5 — the bytes are the client's.** Normalization changes the decision,
  never the forwarded frame (I1 unchanged).
- **W6 — absence and denial are indistinguishable.** The reply to a call on
  an ungranted tool is byte-identical to the reply for a tool no upstream
  offers.

## Tests

- **The per-axis denial table** (the T1 plan's second acceptance
  criterion, *grant file denies out-of-envelope calls*, at the wire):
  path outside prefix — including `../` and, under the Windows flavor,
  `..\` — off-pattern recipient, out-of-range number, expired grant,
  ungranted tool, `Absent` violated, wrong-typed and unrepresentable
  arguments. Each row asserts three things: the upstream never saw the
  call, the client-visible bytes, and the trace event (principal, tool,
  reason).
- **The `EvaluationError` row, through a stub `Authorizer`.** The engine's
  fail-closed answer cannot be reached from a grant file — M4's P5 makes it
  unreachable by construction, and `decide`, which is what the proxy's own
  tests use as their authorizer (D7), never produces it at all. So the one
  test that pins its client-visible shape drives the gate with an
  `Authorizer` that returns `Deny(EvaluationError)` unconditionally, and
  asserts three things: the client sees the *same* bytes as an
  `OutOfEnvelope` denial, the `detail` reaches the trace event, and the
  session continues (an engine failure denied the call — authority held, so
  it is not a reason to kill the session; see D8's contrast with a sink
  failure).
- **Duplicate keys inside `arguments`.** `{"path": "/etc/passwd", "path":
  "/data/invoices/ok.pdf"}` is refused with `-32602` +
  `CallRefused{MalformedParams}` and never forwarded, because the two
  readings a JSON parser may take of it differ and the frame crosses
  byte-faithfully — resolving the ambiguity either way is a guess about the
  upstream's parser. Paired with a test that the same object deserialized
  into a `BTreeMap` would have silently taken the second value, so the
  reason the visitor is hand-written is visible in the suite.
- **Filtering and expiry**, over the settable clock: an ungranted tool is
  absent from `tools/list` and answers `-32602`; a grant expiring mid
  session makes its tool vanish from the next list and its calls fail,
  with no re-connect.
- **Loader tables:** every refusal in D2 and D3, each asserting the process
  refuses to start; every warning path asserting it does not. Including the
  `version` rows — absent, `0`, `2`, and a non-integer each refuse by
  number and name what this binary supports — since that check is the only
  thing standing between a future semantic change and a silently
  re-interpreted grant file.
- **Normalizer table:** the D4 rows, both flavors, plus the two false-allow
  cases as regression tests — they are the reason the flavor exists.
- **Trace:** one golden JSONL transcript of a session containing an allow,
  a denial, a refusal and a completion; a sink whose `record` fails ends
  the session (W3); `seq` is dense and monotonic. Truncation rows: a value
  just under the cap is recorded whole and unhashed, one just over carries
  the prefix, the full byte length and a digest matching a known SHA-256
  vector, and two calls with the same oversized value produce the same
  digest — which is the property the digest exists for. Plus the row the
  cap was chosen for: a path at Linux's `PATH_MAX` is recorded **whole**.
- **End to end, real binary, real Cedar** (`proxy_e2e.rs`): a config with
  grants denies and allows as written; `--unenforced` still works and
  refuses `--trace`; a config without grants refuses to start.
- **The M1/M2 suite, assertions unchanged:** each test gains an
  `Enforcement` bundle (an envelope granting its tools without
  constraints), and every byte-identity assertion must still pass with the
  gate in the path — that is the regression net for W5.

## Docs & housekeeping (same PR)

- `docs/cli.md`: intro and §9 stop saying enforcement is unwired; `version`
  and grants in §3, and **every** config example in the file grows the
  `version = 1` line — as do the e2e fixtures and the demo `flavium.toml`;
  `--unenforced`/`--trace` in §2; new exit-code and startup-error rows; a
  new section for the trace file.
- `docs/architecture/proxy-mcp.md`: §10 becomes past tense; §7's I11 and
  §8's error surface gain the denial rows; §11 and §12 gain the new tests
  and the new CLI wiring.
- `docs/GLOSSARY.md`: the "from M4/M5" markers become statements; entries
  for grant file, unenforced mode, trace record. (*Path normalization*,
  *path flavor*, *false allow* and *false denial* land with this plan, not
  with the milestone — D4 and D5 are unreadable without them.)
- `docs/tasks/v0.1/T1-mcp-proxy-core.md`: status line, and an M5 note with
  whatever this plan got wrong — including `sha2`, which that plan's CLI
  crate list (clap, toml, tracing-subscriber) does not yet name.
- `docs/tasks/v0.1/T1-demo.md`: the M5 acceptance run — the M1/M2
  checklist re-run against Claude Desktop with a grant file, plus the
  denial the demo exists to show.
- `README.md`: T1 is done; what the binary now enforces.
- After merge: the memory file (T1 closed, M5's decisions, T2 next).

## Verification

The full gate (`cargo fmt --all`, `cargo clippy --workspace --all-targets
-- -D warnings`, `RUSTDOCFLAGS="-D warnings" cargo doc --workspace
--no-deps`, `cargo test --workspace`), plus: `git diff --stat` touches no
file under `crates/flavium-core` or `crates/flavium-policy` (W4);
`cargo tree -p flavium-proxy-mcp` still does not contain `cedar-policy`;
no `unwrap`/`expect` outside `#[cfg(test)]` in the new modules;
dependencies confirmed current. Then the adversarial multi-lens review
with fixes before the PR — on this milestone the lenses that earn their
keep are fail-closed ordering in the gate, the normalizer against a
hostile path list, and trace-versus-reality divergence.

## Delivery

Two PRs. This plan (plus the glossary vocabulary it introduces) on branch
`m5-plan` → PR **Add the M5 plan and its enforcement vocabulary to the
glossary** (62 chars); then the milestone on branch `m5-enforcement` → PR
**Wire grant enforcement, tools/list filtering, and the JSONL trace** (65
chars). Both `-s`; PR URLs handed over.

## Risks

1. **The demo path is Windows and the flavor decision is new.** D4's
   `windows-path-prefix` is verified against the normalizer, not yet
   against `server-filesystem`'s own resolution; the M5 demo run is where
   that meets reality, and the flavor map is one module if it needs to
   change.
2. **The gate changes the shape of a live session.** M1/M2's byte-identity
   tests are the net; their assertions stay as written, so a regression
   shows up as a failure rather than as an edited expectation.
3. **Size.** This is the largest milestone of T1. If the review finds it
   unreviewable as one PR, the seam to split on is trace-plumbing first,
   gate second — but the milestone is specified as one PR and lands green
   as one.
