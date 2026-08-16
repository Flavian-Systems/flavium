# Contributing to Flavium

Thank you for considering a contribution. Pre-v0.1, the most valuable
contributions are still design review, threat-model critique, and issues
that sharpen the grant model — but the workspace is past a skeleton now
(an enforcing MCP proxy, four crates), so code PRs are welcome.

## Ground rules

- **Discuss before large changes.** Open an issue first for anything beyond
  a small fix so we can agree on direction.
- **PRs into `main` via review.** `main` is protected: pull request,
  squash merge, linear history, and the required check must pass.
- **The gate is four commands, and `cargo doc` is one of them.** CI runs
  `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
  -D warnings`, `cargo doc --workspace --no-deps` with
  `RUSTDOCFLAGS="-D warnings"`, and `cargo test --workspace`. Two
  workspace lints do most of the surprising: `missing_docs` (so every
  public item needs rustdoc) and `clippy::unwrap_used`/`expect_used`
  (test modules opt out explicitly) — both are `warn` in `Cargo.toml`
  and become errors under `-D warnings`. The `cargo doc` step is what
  catches a broken intra-doc link. `.githooks/pre-commit` runs the first
  three locally; enable it with
  `git config core.hooksPath .githooks`.
- **The policy core is special.** Changes under `crates/flavium-core` and
  `crates/flavium-policy` get the strictest review; they are the future
  verification target and are kept small and dependency-light on purpose.

## Developer Certificate of Origin

We use the [DCO](https://developercertificate.org/) instead of a CLA.
Sign off each commit (`git commit -s`), which adds:

    Signed-off-by: Your Name <your@email>

By signing off you certify you have the right to submit the work under this
project's licenses.

## Licensing of contributions

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in Flavium by you, as defined in the Apache-2.0
license, shall be dual licensed under Apache-2.0 and MIT, without any
additional terms or conditions.
