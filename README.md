# Flavium

**The capability runtime for AI agents.** Bounded authority · enforced budgets · replayable audit — in one binary between agents and the world.

> A VM decides *where* the agent runs. Flavium decides *what it may do.*

**Status: pre-v0.1 — under construction.** The [design document](DESIGN.md) is the current source of truth. Code lands here in the open as it is written.

**What works today: an enforcing multi-upstream MCP proxy.** `flavium` presents an MCP server to any stdio client (Claude Desktop, Claude Code, …) and fronts one or more upstream tool servers — local processes *and* streamable-HTTP endpoints — merging their tools into one list and routing every call by tool name, with `params`/`result` bodies crossing byte-faithfully. On top of that, since T1/M5: the client is shown **only the tools your grant file names**, **every `tools/call` is authorized before it is forwarded** (argument prefixes and suffixes, value sets, numeric ranges, required-absent arguments, expiry — with path arguments normalized so `../` cannot walk out of a granted directory), and every decision, denial and refusal can be **recorded to a JSONL trace**. A config file with no grants refuses to start. Budgets (T2), attenuated delegation (T3) and the hash-chained recorder (T4) are the milestones behind it ([plan](docs/tasks/v0.1/T1-mcp-proxy-core.md)).

```bash
flavium proxy --config flavium.toml --trace flavium-trace.jsonl
```

```toml
version = 1
principal = "invoice-bot"

[[upstream]]
name = "fs"
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/data"]

[[upstream]]
name = "search"
url = "https://example.com/mcp"
headers = { Authorization = "Bearer …" }   # optional; values never logged

[[grant]]                                  # read invoices, until September
tool = "read_file"
expires = 2026-09-01T00:00:00Z
[grant.args]
path = { path-prefix = "/data/invoices/" }

[[grant]]                                  # mail colleagues, never blind-copy
tool = "send_mail"
[grant.args]
to  = { suffix = "@yourco.com" }
bcc = { absent = true }
```

Anything the grants do not name is not in the tool list and not callable. An out-of-envelope call comes back as `denied by policy` — the agent can see it and retry inside its envelope, and learns nothing about what the envelope is.

The transparent M1/M2 middlebox is still there when you want it, but you have to ask:

```bash
flavium proxy --unenforced -- npx -y @modelcontextprotocol/server-filesystem /data
```

Flags, the full config-file and grant reference, the trace format, exit codes, startup errors, and client wiring are in [docs/cli.md](docs/cli.md).

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

See [DESIGN.md](DESIGN.md) for the full architecture, threat model, and honest boundaries, and [docs/architecture/proxy-mcp.md](docs/architecture/proxy-mcp.md) for how the MCP proxy crate is built — modules, tasks, message flows, invariants.

## Contributing

Early contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). The project vocabulary (upstream, grant, attenuation, …) is fixed in [docs/GLOSSARY.md](docs/GLOSSARY.md). Security reports go through [SECURITY.md](SECURITY.md), not public issues.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

---

Flavium — a [Flavian Systems](https://github.com/Flavian-Systems) project · [flavium.ai](https://flavium.ai) · hello@flavium.ai
