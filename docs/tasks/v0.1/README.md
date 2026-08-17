# Flavium v0.1 — task breakdown

Seven tasks under six numbers deliver v0.1 (see DESIGN.md §5) — T2 is two
tasks, T2a and T2b, so every existing reference to T3–T6 stays valid. Work
them roughly in order — later tasks build on earlier ones. Each task gets a
detailed plan file here (`T<n>-<slug>.md`) written in plan mode and
human-approved *before* implementation starts; the acceptance criteria below
are fixed, the plans are living documents.

## T1 — MCP proxy core

A transparent MCP middlebox: presents as an MCP server to clients (stdio),
connects as an MCP client to configured upstream servers (stdio + HTTP),
forwards initialize / tools/list / tools/call faithfully. Then the grant
engine: Cedar-backed authorization of every tools/call with argument-level
constraints (path prefixes, domain/recipient patterns, numeric ranges,
expiry), and tools/list filtered to granted tools.
**Done when:** a real client (Claude Desktop) works unmodified through the
proxy; a grant file denies out-of-envelope calls; denials are logged.

## T2a — Budgets on the tool path

Stateful metering beside Cedar, and the axis it needs: a `budget` field on
`Grant` in flavium-core, `Axis::Budget`, and a new arm in `covers` so a
child's cap can only be smaller (T1/M3 D7 reserved exactly this room).
Tool-call counts and wall-clock, scoped per task. The meter is stateful and
therefore sits *beside* `decide`, never inside it — `decide` stays pure;
`admitting_grants` already returns every matching index so the meter can pick
the first admitting grant *with budget left* without changing `Decision`.
Exhaustion is a denial like any other: typed, traced, agent-visible.
**Done when:** a deliberately looping agent is killed at its tool-call cap and
at its wall-clock cap, with the denial traced; a child grant that raises a
budget is refused by `attenuates`.

## T2b — The model boundary

An OpenAI-compatible /v1/chat/completions proxy fronting hosted APIs and
local engines — the proxy's first HTTP *server* face, and the point at which
model calls stop being outside the runtime. Token and spend budgets on that
traffic, carried by T2a's axis in their own units; over-budget generations
terminated mid-stream where the backend streams. Model calls and kills are
trace events, because T4 replays what transits the runtime and the model is
the dominant nondeterministic input.
**Done when:** a deliberately looping agent is killed at its token cap
mid-generation, with the kill traced; a session's model calls are in the
trace as first-class events rather than inferred from tool calls.

*(Why this is two tasks, and why v0.1 mediates model calls at all:
[Scope decisions](#scope-decisions), below.)*

## T3 — Attenuated delegation & supervision

Spawning sub-agents with grant sets that are provably subsets of the
parent's on every axis (the `attenuates` invariant in flavium-core),
enforced at spawn. Budget exhaustion on either meter (T2a's or T2b's)
becomes a supervised failure (restart / escalate / halt policies).
**Done when:** a parent cannot mint authority it lacks; a child's breach
surfaces as a supervision event, not silent misbehavior.

## T4 — Flight recorder & replay

Append-only, hash-chained event log in SQLite with a versioned public
schema: every tool call, model call, decision, denial, budget tick, spawn,
termination — including a generation cut mid-stream. Deterministic replay:
re-run a session feeding recorded model/tool responses instead of live ones,
the model responses coming from T2b's face.
**Done when:** a recorded session replays to identical decisions; trace
tampering is detectable; the schema is documented as a spec.

**Read before designing the schema:**
[../mcp-surface-and-auth.md](../mcp-surface-and-auth.md), D7. Publishing
the grant and trace formats as versioned specs is the point at which
their shape stops being free to change, and the MCP surfaces v0.1 does
not mediate each add a grant axis or an event kind. That file argues for
reserving the room now. It is a proposal, not approved scope — but the
deadline it names is T4's, so T4 is where the question has to be
answered either way. T2a's budget axis and T2b's model-call, token-tick
and mid-stream-kill events are not in that category: they are approved
scope that lands *before* T4, so the published formats carry them rather
than reserve room for them.

## T5 — Demo, hardening, packaging

The reference prompt-injection demo (email assistant, run with/without
Flavium — DESIGN.md §5), fuzzing of the JSON-RPC parser, static binaries
(musl) + distroless OCI image, user docs, and the grant/trace formats
published as standalone specifications.
**Done when:** a stranger reproduces the demo in under 10 minutes from
the README.

## T6 — Dissemination

Talk materials, examples, and integration guides. Mostly human work;
agents assist with examples and docs.

## Ground rules

- **One plan file per task, approved before code.** Long tasks also get
  one plan per milestone beside it (`T1-m3-plan.md`, `T1-m4-plan.md`,
  `T1-m5-plan.md`). The original "keep plans ≤1 page" rule did not
  survive T1 and is retired: the milestone plans run 300–700 lines
  because each decision states what it is, **why**, and **what it rules
  out**, with the option space listed where more than two candidates
  were considered. That is the audit trail an auditor and a future
  contributor read to understand why the enforcement core looks the way
  it does, and a decision without its rejected alternative is
  unauditable. Length is a consequence, not a goal.
- **A plan is not edited after approval.** When the milestone contradicts
  it, the *task* file gains an "M<n> note — where this plan was wrong"
  section listing each deviation and why (see the end of
  `T1-mcp-proxy-core.md`). A plan the code has quietly diverged from is
  worse than no plan, because it is still read as the audit trail.
- Acceptance criteria above are contractual (grant milestones) — scope
  changes need explicit human sign-off, and the sign-off is recorded
  under *Scope decisions* below, with what it rules out.
- Everything in CLAUDE.md applies, especially the flavium-core rules.

## Scope decisions

### 2026-08-17 — v0.1 mediates model calls, and T2 splits in two

**Decision.** v0.1 mediates model calls as well as tool calls: the
OpenAI-compatible face, token and spend budgets, and deterministic replay
of recorded model responses are all v0.1 scope. Because that makes one
task the size of T1, T2 becomes **T2a** (budgets on the tool path) and
**T2b** (the model boundary). Recorded here because acceptance criteria
are contractual and this rewrites two of them.

**The question, and why it was open.** Three documents answered it
differently. This file put the OpenAI-compatible face in T2 and
deterministic replay in T4 — both v0.1. README's roadmap table put "LLM
proxy with mid-generation budget kills; deterministic replay tooling" in
v0.2. DESIGN §3, §4, §5 and §7 describe the model-call boundary as
inspected, budgeted and replayable in the shipping runtime, while §9's
roadmap sentence lists "replay tooling" after v0.1.

**Why v0.1, in order of weight.**

1. T2's own acceptance criterion was unsatisfiable otherwise. "Killed at
   its cap, in both tool-call count and **token spend**" — token spend is
   only observable to something sitting in front of the model.
2. DESIGN §4 grounds replay in "every nondeterministic input transits the
   runtime", and the model is the dominant nondeterministic input. Replay
   of tool responses alone is a materially weaker claim than §5 makes.
3. The budget axis lands on `Grant` in flavium-core — the enforcement core
   and verification target. Whether it carries tokens and spend or only
   calls and wall-clock has to be settled once, before the field is
   written, not extended afterwards. Deciding it twice is exactly the
   churn the flavium-core rules exist to prevent.

**What this rules out.** v0.1 as tools-only. That reading costs three
documented retreats — T2's done-when drops token spend, DESIGN §5's
budget bullet drops it too, and T4's replay narrows to tool responses —
and it withdraws "budgets with teeth: token spend" from the release it is
advertised on, immediately after building the core meant to deliver it.
(T5's demo is *not* among the costs: it is driven by a real client
through the proxy, and stays reproducible either way.)

**What was stale, and how that is known.** README's roadmap table. Its
three rows were authored 2026-08-11 in `7de3cfe`, one day before this
file (`0bf5d69`, 2026-08-12), and `git log -L 81,83:README.md` shows they
were never touched again — including by `d933a25`, "Document the
enforcement core, and fix stale docs". They are corrected in the same
change as this note. DESIGN
§9's "replay tooling" now says "replay debugging tooling", so it reads as
the steppers and diffs built *around* v0.1's replay rather than as the
capability itself, which §5 already claims for v0.1.

**Why split rather than one task.** Metering on the existing `tools/call`
path is small: it sits on seams T1 already built (`admitting_grants`,
`Decision`, `TraceEvent`, the `Enforcement` hook). The model boundary is a
second protocol face — an HTTP *server* face, which T1/M5 deferred by
name; SSE downstream; model-provider clients upstream; per-provider token
accounting; mid-stream kill semantics. That is T1's M1 and M2 over again,
and a single plan covering both would be one nobody could hold in their
head at approval time.

**Why 2a/2b rather than renumbering.** Both halves keep the number 2, so
every existing reference to T3, T4, T5 and T6 across the repo stays valid
and no approved plan has to be re-read. A bare "T2" written before this
note means the budget axis, and stays correct as T2a.

**The posture change this forces, declared rather than slipped in.** T2b
makes flavium *listen*: an OpenAI-compatible endpoint is an inbound socket,
which CLAUDE.md's security posture ("no new external endpoints") and
DESIGN §7 did not previously admit. `docs/tasks/mcp-surface-and-auth.md`
D6 argued that a listener is a posture change that must be declared as
one; that argument is upheld, and the declaration lands here and in DESIGN
§7 rather than arriving unannounced with OAuth. D6's own reasoning is
overtaken by it — see the note at the end of that file.

**Still open, and deliberately not answered here.** Budgets are scoped
"per task" and delegation is per agent tree, but the runtime's only
scoping concept is the session — one client connection, one static
principal from config, one process — with no notion of a task inside it
or an agent tree across it. T2a and T3 need the same missing concept. It is a design question, not a
code dependency, and it belongs in T2a's plan — before the meter invents a
scoping key that T3 then has to replace.
