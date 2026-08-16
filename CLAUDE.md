# CLAUDE.md — project instructions for Claude Code

Flavium is a capability runtime for AI agents: a Rust proxy that enforces
grants, budgets, namespaces, and a replayable trace between agents and
their tools/models. **Read DESIGN.md before non-trivial work** — it is the
source of truth for architecture, threat model, and scope.

## The one rule that outranks the others

`crates/flavium-core` and `crates/flavium-policy` are the **enforcement
core and future formal-verification target**. In these two crates:
- keep them small and dependency-light — adding a dependency there needs
  explicit human approval first;
- no `unsafe`; no `unwrap`/`expect` outside tests;
- every change must state which invariant it preserves (the key one:
  delegation strictly attenuates — child grants ⊆ parent grants on every
  axis);
- prefer boring, obvious code over clever code. This core will be read
  line-by-line by auditors and verification tools.

## Workspace layout

- `crates/flavium-core` — grant/principal/trace types, the reference
  decision semantics, the attenuation invariant
- `crates/flavium-policy` — Cedar evaluation
- `crates/flavium-proxy-mcp` — MCP middlebox (server to clients, client
  to upstream tool servers)
- `crates/flavium-cli` — the `flavium` binary

The budget axis is **T2a and not modelled yet** — no `budget` field on
`Grant`, no metering in flavium-policy, and a `budget` key in a grant
file is a startup error rather than a silently accepted one. DESIGN §4's
"Cedar evaluation plus stateful budget metering, both must pass" is what
T2a builds on the tool path and T2b on the model path; until they land,
do not write it as though it already holds. How the two crates work
today: [docs/architecture/core-and-policy.md](docs/architecture/core-and-policy.md).

## Workflow (matches CI exactly)

- Branch → PR → green CI → squash merge. Never commit to `main` directly;
  it is protected and requires the status check.
- Before every commit run: `cargo fmt --all` then
  `cargo clippy --workspace --all-targets -- -D warnings` then
  `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` then
  `cargo test --workspace`. The pre-commit hook enforces fmt+clippy+doc;
  don't bypass it (`--no-verify` is for humans in emergencies only).
- Sign off every commit: `git commit -s` (DCO). Keep PR titles usable as
  squash-commit messages: imperative, ≤72 chars.
- All files LF (`.gitattributes`/`.editorconfig` enforce this). No
  committed binaries, no `Cargo.lock` (workspace is a library-first repo
  for now), no generated files.

## Engineering conventions

- Rust stable, edition 2021. Errors: `thiserror` in libraries; no panics
  on the request path — every denial is a typed result that reaches the
  trace.
- Every grant decision, denial, budget tick, model call, spawn, and
  termination is a trace event. If you add behavior that isn't traced,
  it isn't done.
- Security posture: this is a security tool. No telemetry, and no network
  calls except those the proxy exists to mediate. Listening sockets are
  the one part of that rule v0.1 changes: it acquires exactly one, T2b's
  OpenAI-compatible endpoint, declared in DESIGN §7. Any further inbound
  endpoint needs the same explicit declaration first — never a silent
  addition. Parser-facing code (MCP JSON-RPC, and T2b's HTTP/SSE face)
  gets fuzz coverage.
- Tests: unit tests beside code; integration tests under `tests/`;
  property-style tests for invariants where feasible. A feature without a
  failing-case test (what gets *denied*) is incomplete.
- Docs: public items get rustdoc (`missing_docs` is a workspace lint and
  `cargo doc` runs with `-D warnings` in CI, so a missing doc or a
  broken intra-doc link fails the build); user-facing behavior changes
  update README.md or DESIGN.md in the same PR.

## Scope guardrails

The v0.1 work breakdown lives in `docs/tasks/v0.1/` — read 
`docs/tasks/v0.1/README.md` before starting any task.

Out of scope until explicitly requested: kernel/sandbox work (Landlock,
seccomp — that's v0.2), Wasm isolation, formal proofs, RISC-V/CHERI, any
bespoke policy DSL (we use Cedar), GUI.

Also out of scope, and **already argued** — do not re-derive it, and do
not implement it without a task number: the MCP surfaces v0.1 does not
mediate. Methods outside `tools/*` answer `-32601`, the server→client
channel (sampling, elicitation, roots) is closed, and HTTP upstreams
take static headers only. That is fail-closed on purpose — flavium does
not offer what it cannot authorize (DESIGN §8) — so "add resources
support" is a vocabulary question, not a routing one.
[docs/tasks/mcp-surface-and-auth.md](docs/tasks/mcp-surface-and-auth.md)
has the decisions and what each rules out; it is a proposal, and its
claims are reasoned rather than spiked. One item there has a deadline
rather than a release: D7 says T4 must reserve room in the grant and
trace formats before publishing them as versioned specs.

When a task seems to require breaking a rule above, stop and ask instead
of proceeding.
