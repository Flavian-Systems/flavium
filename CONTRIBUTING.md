# Contributing to Flavium

Thank you for considering a contribution. Pre-v0.1, the most valuable
contributions are design review, threat-model critique, and issues that
sharpen the grant model — code PRs are welcome once the workspace skeleton
stabilizes.

## Ground rules

- **Discuss before large changes.** Open an issue first for anything beyond
  a small fix so we can agree on direction.
- **PRs into `main` via review.** CI (fmt, clippy, tests) must pass.
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
