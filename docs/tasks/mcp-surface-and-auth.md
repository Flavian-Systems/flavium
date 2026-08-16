# The rest of MCP — surface coverage, the server→client channel, upstream auth (plan)

Status: **proposed — not approved, and nothing here is contractual.** No
task number, no release. This file exists so that three gaps found while
running T1's acceptance demo are argued once, while the reasoning is
fresh, instead of being rediscovered as surprises when a user asks why
their favourite MCP server is half-mute behind the proxy.

Unlike the v0.1 milestone plans, **none of the external-API claims below
were spiked.** Method names, the MCP authorization spec's RFC set, and
client behaviour are *reasoned* from the protocol, not executed. Anything
promoted from this file into an approved plan gets the usual treatment
first: a throwaway spike, and *Verified* markers on what was run.

## Context

v0.1 mediates tools, and only tools. Three consequences, each a
deliberate T1 decision rather than an omission:

- `resources/*`, `prompts/*`, `completion/complete` and every other
  method outside `tools/*` answer `-32601 Method not found`
  (`router.rs`, the `(Phase::Ready, _)` arm). The proxy advertises only
  `tools.listChanged` to the client, so a compliant client never asks.
- The **server→client** request channel is closed: flavium declares no
  client capabilities upstream, and an upstream request that is not
  `ping` is answered `-32601` (`connection.rs`). Sampling, elicitation
  and roots are therefore unreachable in both directions. This one is
  written down as intentional in
  [T1's plan](v0.1/T1-mcp-proxy-core.md) — "sampling/elicitation not
  declared upstream, deliberately closing the server→client request
  channel for T1".
- HTTP upstreams authenticate with **static headers only**
  (`headers = { Authorization = "Bearer …" }`, values never logged).
  There is no OAuth, so a hosted connector that requires the MCP
  authorization flow cannot be fronted at all.

The first two are the same rule applied twice: **flavium does not offer
what it cannot authorize.** That rule is right and should survive
everything below. What follows is how each surface earns an
authorization story, not how each gets forwarded.

## Scope

**In:** the grant-vocabulary and trace-schema *shape* needed to express
authority over resources, prompts, sampling, elicitation and upstream
credentials; the sequencing constraint that shape imposes on T4; the
decisions that say which of these is a gap and which is a feature.

**Out:** implementation of any of it. Also out: extending Cedar's
schema, any client-side UI, and non-MCP protocols (A2A, OpenAI's tool
surface) — the same questions will arrive there, and answering them for
one protocol first is the cheaper mistake.

## Decisions

### D1 — Three tasks, not one "finish MCP"

**Decision.** Surface coverage (resources/prompts/completions), the
server→client channel, and upstream OAuth become **separate tasks**,
sequenced independently and allowed to land in different releases.

**Why.** They share only the word "MCP". Their prerequisites differ —
sampling is meaningless before budgets exist, token custody is reckless
before the recorder exists, prompts need neither. Their risk classes
differ — a resource URI normalizer is a false-allow surface, a token
store is a credential-at-rest surface, elicitation is a human-factors
surface. And one of them is not a gap at all (D6). A single task would
be scheduled by its slowest part and reviewed by whoever happened to
pick it up, which is how a URI normalizer gets written by someone
thinking about OAuth.

**What this rules out.** A "MCP 100% coverage" milestone, and the
marketing that would come with it. Coverage is not the goal; *mediated*
coverage is, and the two have different shapes.

### D2 — Prompts join the existing vocabulary behind a kind axis; resources do not

**Decision.** `prompts/get` is authorized by the **existing** `Grant`
machinery — a name plus arguments, exactly like a tool — distinguished
by a new axis on `Grant` naming which kind of thing the name refers to.
Resources get their own constraint over URIs, not a reuse of `tool`.

The option space, since there were more than two candidates:

- **A — one namespace, no kind axis.** A grant naming `summarize`
  authorizes the tool *and* the prompt of that name. Rejected: two
  different authorities collapse into one, and an operator who granted a
  read-only prompt would silently be granting a tool. That is a false
  allow manufactured by the vocabulary.
- **B — prefixed names** (`tool:summarize`, `prompt:summarize`).
  Rejected: it puts structure inside a string the core compares
  byte-wise, which is precisely the mistake `path-prefix` exists to
  correct. The first person to write `prompt:` in a tool name finds the
  seam.
- **C — a kind axis on `Grant` (chosen).** One more field, closed
  vocabulary, refused when unrecognised. The `Constraint` machinery,
  attenuation, and the trace all keep working unchanged, because a
  prompt's arguments are arguments.
- **D — separate grant types per kind.** Rejected: it duplicates
  expiry, attenuation and the argument constraints across types, and the
  attenuation invariant then has to be proved once per type. The core is
  a verification target; one `Grant` with one more axis is cheaper to
  prove than three structurally similar ones.

**Why resources are different.** `resources/read` identifies its target
by URI, not by name-plus-arguments. Constraining it means prefix
matching over URIs, and URIs bring their own normalization: percent
encoding, a case-insensitive scheme and host beside a case-sensitive
path, RFC 3986 dot-segment removal, and default ports. That is a second
normalizer with a *worse* encoding story than paths — and the path
normalizer has already produced two false allows and one false denial
across M5 and this branch. It is its own task with its own table of
adversarial rows, not a paragraph in someone else's.

**What this rules out.** Shipping resource support by pointing
`Constraint::Prefix` at a URI string and calling it done. A byte prefix
over an unnormalized URI is the `/data/invoices/../../etc/passwd` bug
with more ways to spell it.

### D3 — `resources/subscribe` is a lease, and is named as new vocabulary rather than forced into a grant

**Decision.** Subscription is **explicitly deferred**, and recorded as
needing a concept flavium does not have: an authority that is granted
once and consumed continuously.

**Why.** Every authority in the model today is decided per call:
`authorize(principal, call, now) → Decision`, one decision, one trace
event, one outcome. A subscription is one decision followed by an
unbounded stream of `notifications/resources/updated` that no further
decision gates. Two things break. The trace stops describing what the
agent saw, because pushed content is not a call and would go unrecorded.
And expiry becomes ambiguous: a subscription authorized at `now` under a
grant that lapses an hour later either keeps delivering — authority
outliving its grant — or must be torn down by something that watches
grants for lapse, which does not exist.

**What this rules out.** Treating `subscribe` as just another method to
route. It is the first authority in flavium that has a *lifetime*, and
that is T3's supervision vocabulary and T4's event vocabulary meeting,
not a routing change.

### D4 — Sampling is opened only once it can be metered, and roots are answered from the envelope

**Decision.** `sampling/createMessage` stays refused until budgets (T2)
exist, and then becomes a grant axis with a token ceiling and a trace
event per sample. `roots/list` is **answered by flavium itself** from
the grant envelope — the roots an upstream sees are the path prefixes it
has been granted — rather than forwarded to the client.

**Why sampling waits.** An upstream that may sample is spending the
principal's token budget and writing content into the agent's context.
It is the confused-deputy channel pointing backwards: the tool server
borrows the agent's model. "This upstream may sample" is not a
statement anyone can review without a bound attached, and the bound is
exactly what T2 builds. Allowing it first and metering it later would
ship the unbounded version to the people most likely to keep it.

**Why roots are answered locally.** Forwarding `roots/list` tells an
upstream about directories outside its envelope — information
disclosure with no upside, since it cannot act on them anyway. Deriving
the answer from the grants is strictly more informative *to the honest
upstream* (it learns exactly where it may work) and strictly less
useful to a hostile one. It is also the only place in the design where
policy can answer a protocol question directly, which is worth having as
a precedent.

**What this rules out.** A transparent sampling passthrough behind a
boolean, which is how every other gateway will ship it.

### D5 — Elicitation is never proxied transparently

**Decision.** If `elicitation/create` is ever allowed, the question
reaching the human must be **marked as upstream-originated**, naming
which upstream asked, and its full text must be traced verbatim.
Flavium must not relay it as though the agent had asked.

**Why.** Elicitation lets a tool server put text in front of a person,
with the agent's framing and the client's chrome around it. That is a
prompt-injection channel aimed at the one component with no policy
engine. The technical mediation flavium can offer is thin — it cannot
judge whether a question is honest — but attribution is not thin: "the
`invoices` server is asking for your password" is a question a user can
refuse, where "please confirm your password to continue" is not. The
trace requirement is the other half: an elicitation is the one event
where the *human* was the resource being accessed, and a record that
omits it cannot answer what happened.

**What this rules out.** Allowing elicitation as a capability flag with
passthrough semantics. If attribution is not implementable in a given
client, the honest answer is to keep refusing.

### D6 — Upstream OAuth is custody, not connectivity, and it changes a stated posture

**Decision.** OAuth for HTTP upstreams is scoped as a **credential
custody** feature: flavium performs the authorization flow, holds and
refreshes the tokens, and the agent never sees one. It is not scheduled
before the recorder (T4), and it requires an explicit amendment to
DESIGN §7 before any code lands.

**Why it is a feature.** "Agents are hired for their authority" is the
thesis; a proxy that holds the credential while the agent holds only an
address is that thesis in the smallest possible form. Static headers
already do this in a crude way — the token is in flavium's config, not
the agent's — and OAuth is the version that works with hosted
connectors instead of long-lived secrets.

**Why it waits for T4.** Token acquisition, refresh and failure are
security events. Building the custody path before the tamper-evident
recorder exists means the first version of the most sensitive code in
the product runs unrecorded, and the second version has to retrofit
events into flows already written.

**The posture change.** The authorization-code flow needs a redirect
URI, which means flavium **listens** — a loopback socket, transient, but
inbound. DESIGN §7 and the security posture in CLAUDE.md currently say
no new external endpoints and no network calls except those the proxy
exists to mediate. A token endpoint is mediated and fits; a listening
socket is new and does not. Loopback-only, bound for the duration of one
flow, is defensible — but it is an amendment, and a security tool that
quietly grows a listener has spent something it cannot get back.

**What this rules out.** Treating OAuth as a transport detail of the
HTTP upstream, which is where it would naturally be implemented and
where nobody would review it as custody code.

### D7 — Reserve the axes before T4 publishes the schema; implement afterwards

**Decision.** T4's grant-format and trace-format specifications must be
designed knowing these axes are coming: a grant that says *which kind*
of authority it is about rather than assuming tools, and an event
vocabulary that does not assume every authority is one call with one
outcome. No implementation is pulled forward — only the shape.

**Why.** T4 publishes both formats as versioned specifications with
compatibility obligations; every line already carries `"v": 1` so that
day is visible. Adding a kind axis to an unpublished grant format is a
field. Adding it afterwards is a spec revision, a migration story, and a
compatibility matrix for a project whose entire pitch is that the
envelope is reviewable. The cost of getting this wrong is not
engineering time, it is the credibility of the format.

**What this rules out.** The comfortable order — ship v0.1, publish the
spec, then discover that half of MCP does not fit it. That is the
default outcome if this decision is not taken explicitly, because
nothing in the current task list forces the question.

## Sequencing, if all of it were taken

| Piece | Blocked on | Why that order |
|---|---|---|
| Kind axis reserved in the grant format | — | Must precede T4's publication (D7) |
| Event vocabulary that admits non-call authorities | — | Same |
| Prompts | the kind axis | Vocabulary already fits (D2) |
| Roots answered from the envelope | — | Pure attenuation, no new axis (D4) |
| Sampling | T2 | Needs a budget to be reviewable (D4) |
| Resources (read/list/templates) | a URI normalizer task | Own false-allow surface (D2) |
| Upstream OAuth | T4 + a DESIGN §7 amendment | Custody code must be recorded (D6) |
| `resources/subscribe` | T3 + T4 | First authority with a lifetime (D3) |
| Elicitation | a client that can attribute it | Attribution is the mediation (D5) |

Two items have no prerequisite at all — the reserved axes — and they are
the two with a deadline.

## Risks

- **The kind axis is reserved and then never used**, leaving a field
  that always says `tool`. Acceptable: one closed-vocabulary field with
  one value costs a line in the format spec, where the reverse mistake
  costs a revision.
- **Elicitation attribution turns out to be unimplementable** in the
  clients people actually use, leaving D5 as a permanent refusal.
  Acceptable, and better stated now than discovered by a user who was
  promised support.
- **The URI normalizer repeats the path normalizer's defects.** Likely,
  on the evidence: that module has produced a false allow or a false
  denial in every review it has had. Mitigation is not care, it is the
  adversarial row table written *before* the code, as M5 eventually did.
- **OAuth expands the binary's attack surface more than any other item
  here** — a listener, tokens at rest, and a refresh path — in the one
  product where that is least affordable.

## What approval would need

This file is an argument, not a commitment. To become real work each
piece needs, in the repo's usual order: a spike that turns its
*reasoned* claims into *verified* ones, a task number and acceptance
criteria in a release's `README.md`, and — for D6 — a decision about
DESIGN §7 taken on its own, not bundled with an implementation PR.
