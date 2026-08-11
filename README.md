# Flavium

**The capability runtime for AI agents.** Bounded authority · enforced budgets · replayable audit — in one binary between agents and the world.

> A VM decides *where* the agent runs. Flavium decides *what it may do.*

**Status: pre-v0.1 — design phase.** The [design document](DESIGN.md) is the current source of truth. Code lands here in the open as it is written.

## Why

Agents are hired for their authority, not their compute: they hold credentials to email, files, databases, APIs, and money. Prompt injection makes every agent a confused deputy — an attacker doesn't need to escape a sandbox when the agent will misuse authority it legitimately holds. VMs and sandboxes protect the *machine*; Flavium protects the *mission*.

Flavium sits between agents and everything they touch and enforces true capability-based authority:

- **Grants, not allowlists** — unforgeable authorizations with argument-level constraints (`recipient.endsWith("@yourco.com")`, `path.startsWith("/data/invoices/")`), evaluated on every call.
- **Budgets with teeth** — token spend, tool-call counts, and wall-clock limits enforced *mid-execution*. Runaway loops die at their cap, not on the invoice.
- **Attenuated delegation** — a sub-agent's authority is strictly a subset of its parent's, by construction. The root's grant envelope bounds the entire agent tree.
- **Per-agent namespaces** — the same agent code runs at different trust tiers by name remapping alone.
- **Flight recorder** — a hash-chained, tamper-evident trace of every action and denial, with deterministic replay.

Flavium does **not** claim to prevent prompt injection. It makes the consequences bounded, computable in advance, and provable after the fact — the worst case of any agent is the union of its grants, an envelope a security team can read *before* deployment.

## How it will work

One Rust binary, local-first, no SDK: Flavium presents as an MCP server to your client and as an MCP client to your tool servers, plus an OpenAI-compatible endpoint in front of hosted APIs and local engines. Existing agents adopt it by pointing at a different address.

```text
agents / MCP clients ──▶ FLAVIUM ──▶ tools · data · models · spend
                 grants · budgets · namespaces · flight recorder
                    every call authorized, metered, recorded
```

Enforcement deepens over time without changing the policy model: proxy enforcement in v0.1; kernel-level sandboxing of spawned tools (Landlock, seccomp, cgroups) in v0.2; optional WebAssembly component isolation in v0.3. The formal-methods goal is one machine-checked theorem: *no sequence of runtime operations allows an agent to act outside its grant envelope, and every delegation strictly attenuates.*

## Roadmap

| Phase | Target |
|---|---|
| v0.1 | Grants, namespaces, budgets, attenuated delegation, flight recorder; MCP proxy; injection demo |
| v0.2 | Kernel-level sandboxing; LLM proxy with mid-generation budget kills; deterministic replay tooling |
| v0.3 | Wasm isolation; machine-checked confinement core; compliance/audit exports |

See [DESIGN.md](DESIGN.md) for the full architecture, threat model, and honest boundaries.

## Contributing

Early contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Security reports go through [SECURITY.md](SECURITY.md), not public issues.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

---

Flavium — a [Flavian Systems](https://github.com/Flavian-Systems) project · [flavium.ai](https://flavium.ai) · hello@flavium.ai
