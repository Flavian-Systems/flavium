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

- `crates/flavium-core` — grant/principal/budget/trace types + invariants
- `crates/flavium-policy` — Cedar evaluation + stateful budget metering
  (both must pass for a call to proceed)
- `crates/flavium-proxy-mcp` — MCP middlebox (server to clients, client
  to upstream tool servers)
- `crates/flavium-cli` — the `flavium` binary

## Workflow (matches CI exactly)

- Branch → PR → green CI → squash merge. Never commit to `main` directly;
  it is protected and requires the status check.
- Before every commit run: `cargo fmt --all` then
  `cargo clippy --workspace --all-targets -- -D warnings` then
  `cargo test --workspace`. The pre-commit hook enforces fmt+clippy;
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
- Every grant decision, denial, budget tick, spawn, and termination is a
  trace event. If you add behavior that isn't traced, it isn't done.
- Security posture: this is a security tool. No telemetry, no network
  calls except those the proxy exists to mediate, no new external
  endpoints. Parser-facing code (MCP JSON-RPC) gets fuzz coverage.
- Tests: unit tests beside code; integration tests under `tests/`;
  property-style tests for invariants where feasible. A feature without a
  failing-case test (what gets *denied*) is incomplete.
- Docs: public items get rustdoc; user-facing behavior changes update
  README.md or DESIGN.md in the same PR.

## Scope guardrails

The v0.1 work breakdown lives in `docs/tasks/v0.1/` — read 
`docs/tasks/v0.1/README.md` before starting any task.

Out of scope until explicitly requested: kernel/sandbox work (Landlock,
seccomp — that's v0.2), Wasm isolation, formal proofs, RISC-V/CHERI, any
bespoke policy DSL (we use Cedar), GUI.

When a task seems to require breaking a rule above, stop and ask instead
of proceeding.
