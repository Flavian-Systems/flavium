# Flavium — Design Document

**The capability runtime for AI agents**

v1.0 · August 2026 · [flavium.ai](https://flavium.ai) · hello@flavium.ai · open source (MIT/Apache-2.0)

## 1. Thesis

Agents are hired for their authority, not their compute. An AI agent is useful precisely because it holds credentials — to email, files, databases, APIs, and money — and every consequential failure of agentic AI is a misuse of authority the agent legitimately held. Prompt injection makes this structural: any agent that reads untrusted content is a confused deputy, and no known model-layer technique prevents it. Virtual machines and sandboxes protect the machine; nothing widely deployed protects the mission.

Flavium is a single-binary, local-first runtime that sits between agents and everything they touch and enforces true capability-based authority: unforgeable grants with argument-level constraints, budgets enforced mid-execution, strictly attenuated delegation to sub-agents, and a tamper-evident, replayable record of every action. It does not prevent prompt injection; it makes the consequences bounded, computable in advance, and provable after the fact. The worst case of any agent becomes the union of its grants — an envelope a security team can read before deployment.

## 2. The problem

Enterprises are not blocked on agent capability; they are blocked on an unanswerable review question: "what is the worst this agent can do?" Today's honest answer is "everything its credentials can reach." The market's current answers each cover a slice: VMs and sandboxes contain what agents do outside their granted channels but say nothing about misuse through them; MCP gateways ship allowlists and coarse trust tiers; LLM gateways meter spend but not tools; none unify authority, budgets, and audit, and none can make a machine-checked claim about confinement. Meanwhile agent protocols are standardizing now, largely around ambient authority — the window to make capability scoping the idiom is open and closing.

## 3. The approach: isolation, factored

Flavium factors "isolation" into four orthogonal mechanisms, each set independently per agent:

| Mechanism | Question answered | Implementation |
|---|---|---|
| **Authority** | May this agent act on this resource? | Unforgeable grants; Cedar policy evaluation; attenuated delegation |
| **Visibility** | What can it even name? | Per-agent namespaces with remapping and virtualization |
| **Quantity** | How much may it consume? | Token, spend, call, and time budgets — enforced mid-execution |
| **Accountability** | What exactly happened? | Hash-chained flight recorder; deterministic replay |

A grant is a tuple: principal, tool, argument constraints, expiry, budget. Constraints operate on the arguments of tool calls — path prefixes, recipient and domain patterns, numeric ranges — which is the layer where agent behavior is meaningful: at the packet or syscall level, "email the CFO" and "email the attacker" are byte-identical. Illustrative policy:

```text
permit(invoice-bot, fs.read)     when path.startsWith("/data/invoices/2026-");
permit(invoice-bot, email.send)  when recipient.endsWith("@yourco.com")
                                 budget 5/day    expires 2026-09-01;
```

Delegation strictly attenuates: a sub-agent's grant set must be equal to or narrower than its parent's on every axis, enforced at spawn. The root's envelope therefore bounds the entire agent tree — the property that makes multi-agent systems analyzable.

## 4. System shape

One Rust binary, deployed as a local daemon, no SDK required: it presents as an MCP server to clients and an MCP client to upstream tool servers, and exposes an OpenAI-compatible endpoint that fronts hosted model APIs and local engines alike — so existing agents adopt it by pointing at a different address. Every tool call and model call flows through a single, dependency-light policy core (Cedar evaluation plus stateful budget metering; both must pass) and into an append-only, hash-chained event log in SQLite. Because every nondeterministic input transits the runtime, any session can be re-run against recorded responses: stochastic agent failures become reproducible debugging sessions, and traces become audit evidence.

Enforcement deepens without changing the policy model: v0.1 enforces at the proxy; v0.2 cages spawned tool servers with Landlock, seccomp, namespaces, and cgroups so grants stop being advisory; v0.3 adds optional WebAssembly component isolation. Linux (x86-64/aarch64) is the primary, fully hardened target; macOS is a first-class developer client with documented, softer enforcement; Windows is served via WSL2. Per-platform enforcement is stated plainly — nothing is claimed above what the platform delivers.

## 5. What v0.1 ships

- **Capability-scoped tool invocation** — grants with argument-level constraints, evaluated on every call.
- **Per-agent namespaces** — the same agent code runs at different trust tiers by name remapping, zero code change.
- **Budgets with mid-flight enforcement** — token spend, tool-call counts, wall-clock; runaway loops die at their cap.
- **Supervised, attenuated delegation** — children hold strictly weaker authority, by construction.
- **Flight recorder** — hash-chained, versioned, replayable trace of every action and denial.

Flagship demo, run twice: an email assistant processes an inbox containing a prompt-injection message instructing it to forward a sensitive thread to an external address. Without Flavium, the agent complies using credentials it legitimately holds; with Flavium, the send falls outside the grant, is denied mechanically and logged, and the legitimate task completes. One config file, no model changes, five minutes.

## 6. The verification flag

The formal-methods strategy is one theorem, not broad verification: no sequence of runtime operations allows an agent to act outside its grant envelope, and every delegation strictly attenuates. Cedar's existing formal model covers policy evaluation; TLA+ specifies the stateful protocols around it (delegation, revocation, budget accounting); Verus/Kani check the deliberately small Rust policy core against those invariants. seL4's lesson, applied: verify the property the users are buying.

## 7. Honest boundaries

Flavium bounds consequences; it does not prevent injection, and a grant to a destination permits content to that destination — within-envelope exfiltration is addressed by composable outbound filters and, on the research roadmap, information-flow labeling in the CaMeL lineage. Proxy-mode enforcement (v0.1) can be bypassed by a malicious local process until the v0.2 sandbox ladder lands; the model-call boundary is inspected and budgeted, not cryptographically contained. These limits are documented wherever the guarantees are.

## 8. What is not mediated yet

MCP is larger than tools, and v0.1 mediates tools. The proxy advertises only `tools` to the client and declares no capabilities upstream, so what it cannot police it does not offer: `resources/*`, `prompts/*` and `completion/complete` answer `-32601`, and the server→client channel — sampling, elicitation, roots — is closed in both directions. That is fail-closed by construction, not an oversight. For a runtime whose claim is that the worst case of an agent is the union of its grants, a surface forwarded without being authorized would be authority outside the envelope, and a capability advertised without being enforced would be a false statement in the handshake.

Closing the gaps is three problems, not one. **Prompts** already fit the grant vocabulary — a name and arguments, like a tool — behind an axis saying which kind is meant. **Resources** do not: they are URIs, needing their own constraint and their own normalizer, and `resources/subscribe` is a standing authority that a per-call grant cannot express at all. **The server→client channel** is the one worth having rather than merely restoring: an upstream that may call your model is spending your budget and writing into your agent's context, so it cannot be opened before the budgets of v0.1 exist to bound it, and an upstream that may question your user is an injection surface aimed at a human, which must be marked as upstream-originated wherever it is ever allowed. **OAuth for HTTP upstreams** is not a gap in mediation but a question of custody: flavium holding the token is the thesis stated in code — the agent gets an address, never a credential — and it brings a token store, refresh, and a loopback redirect listener, the last being a change to the posture in §7 that has to be declared as one rather than slipped in.

The scheduling constraint is the flight recorder. Grant vocabulary and trace schema are published as versioned specifications with it, and each of these adds either a grant axis or an event kind. Deciding their shape before that publication costs nothing; deciding it after costs a spec revision. The plan is [docs/tasks/mcp-surface-and-auth.md](docs/tasks/mcp-surface-and-auth.md) — proposed, and deliberately not assigned to a release.

## 9. Project facts

Rust (tokio/axum), Cedar, SQLite; single founder (25+ years systems engineering, prior startup CTO). The runtime is and stays open source (MIT/Apache-2.0); a commercial compliance layer will fund continued development. Roadmap, in order: v0.1; then kernel-level enforcement, replay tooling, and first design partners; then a verified confinement core and the paid compliance layer. Contact: hello@flavium.ai · [flavium.ai](https://flavium.ai).
