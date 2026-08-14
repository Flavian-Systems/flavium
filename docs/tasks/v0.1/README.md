# Flavium v0.1 — task breakdown

Six tasks deliver v0.1 (see DESIGN.md §5). Work them roughly in order —
later tasks build on earlier ones. Each task gets a detailed plan file
here (`T<n>-<slug>.md`) written in plan mode and human-approved *before*
implementation starts; the acceptance criteria below are fixed, the plans
are living documents.

## T1 — MCP proxy core

A transparent MCP middlebox: presents as an MCP server to clients (stdio),
connects as an MCP client to configured upstream servers (stdio + HTTP),
forwards initialize / tools/list / tools/call faithfully. Then the grant
engine: Cedar-backed authorization of every tools/call with argument-level
constraints (path prefixes, domain/recipient patterns, numeric ranges,
expiry), and tools/list filtered to granted tools.
**Done when:** a real client (Claude Desktop) works unmodified through the
proxy; a grant file denies out-of-envelope calls; denials are logged.

## T2 — Budgets, enforced mid-execution

Stateful metering beside Cedar: token spend, tool-call counts, wall-clock
per task. An OpenAI-compatible /v1/chat/completions proxy fronting hosted
APIs and local engines; over-budget generations are terminated mid-stream
where the backend streams.
**Done when:** a deliberately looping agent is killed at its cap, in both
tool-call count and token spend, with the denial traced.

## T3 — Attenuated delegation \& supervision

Spawning sub-agents with grant sets that are provably subsets of the
parent's on every axis (the `attenuates` invariant in flavium-core),
enforced at spawn. Budget exhaustion becomes a supervised failure
(restart / escalate / halt policies).
**Done when:** a parent cannot mint authority it lacks; a child's breach
surfaces as a supervision event, not silent misbehavior.

## T4 — Flight recorder \& replay

Append-only, hash-chained event log in SQLite with a versioned public
schema: every call, decision, denial, budget tick, spawn, termination.
Deterministic replay: re-run a session feeding recorded model/tool
responses instead of live ones.
**Done when:** a recorded session replays to identical decisions; trace
tampering is detectable; the schema is documented as a spec.

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

- One plan file per task, approved before code. Keep plans ≤1 page.
- Acceptance criteria above are contractual (grant milestones) — scope
&#x20; changes need explicit human sign-off.
- Everything in CLAUDE.md applies, especially the flavium-core rules.
