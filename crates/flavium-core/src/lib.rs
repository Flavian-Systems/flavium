//! Flavium core: grant types, the attenuation invariant, the reference
//! decision semantics, and the trace vocabulary.
//!
//! This crate is the **enforcement core and future formal-verification
//! target** (DESIGN.md §6). It is deliberately small, has no dependencies
//! (not even dev-dependencies), no `unsafe`, no `unwrap`/`expect` outside
//! tests, no clock and no I/O. Every function here is total: it returns a
//! value for every input and never panics. Where a choice existed between
//! clever and obvious, obvious won — this code is meant to be read line by
//! line by auditors and by verification tools.
//!
//! # What lives here
//!
//! - **Names and time** — [`Principal`], [`ToolName`] (validated newtypes)
//!   and [`Timestamp`] (Unix seconds; the caller supplies `now`).
//! - **Grants** — a [`Grant`] is authority over one tool: argument
//!   [`Constraint`]s plus an optional expiry. A [`GrantEnvelope`] is the set
//!   of grants one principal holds — the *envelope*: the precomputable
//!   worst case of what that agent can do.
//! - **Reference semantics** — [`decide`] says whether an envelope allows a
//!   [`ToolCall`] at a given time and, if not, why ([`DenialReason`]). This
//!   is the *specification* of what a grant means; the runtime engine
//!   (`flavium-policy`, Cedar) is tested against it, and the proxy reaches
//!   whichever engine it is wired to through the [`Authorizer`] trait.
//! - **Attenuation** — [`attenuates`] checks that a child grant set is a
//!   subset of its parent's on every axis. It is the check T3 (delegation)
//!   runs at spawn.
//! - **Trace** — [`TraceEvent`] is everything the runtime records about a
//!   session; [`TraceSink`] is where events go (the CLI supplies a JSONL
//!   sink; T4 supplies the hash-chained recorder).
//!
//! # Semantics in one paragraph
//!
//! A call `(tool, args)` at time `now` is allowed by an envelope iff some
//! grant in it names that tool, is live (`now < expires`, or never expires),
//! and every one of its constraints admits the corresponding argument.
//! Constraints are per argument name and fail closed: a constrained
//! argument that is missing, of the wrong type, or of an unmodelled type
//! ([`ArgValue::Other`]) is not admitted; [`Constraint::Absent`] admits only
//! a *missing* argument. Arguments no constraint mentions are not looked at
//! — a grant author must constrain every argument that matters (for an
//! email tool: `to`, and `cc`/`bcc` as `Absent`). String comparison is
//! byte-wise; nothing is normalized here (path normalization happens before
//! the call reaches this crate).
//!
//! # Invariants
//!
//! Stated once, here; each function's documentation names the ones it
//! maintains. `p`/`c` are parent/child grant sets, `call` any [`ToolCall`],
//! `now` any [`Timestamp`].
//!
//! - **L1 (constraint inclusion is sound)** —
//!   `p.includes(&c)` ⇒ ∀ v: `c.admits(v)` ⇒ `p.admits(v)`.
//! - **L2 (grant coverage is sound)** —
//!   `p.covers(&c).is_ok()` ⇒ ∀ call, now: `c` is live and admits `call` ⇒
//!   `p` is live and admits `call`.
//! - **INV-1 (attenuation is sound)** — `attenuates(p, c).is_ok()` ⇒
//!   ∀ call, now: `decide(c, call, now)` is `Allow` ⇒ `decide(p, call, now)`
//!   is `Allow`. Follows from L2 and the ∀∃ shape of `attenuates`.
//! - **INV-1b (visibility attenuates too)** — `attenuates(p, c).is_ok()` ⇒
//!   ∀ now, `granted_tools(c, now)` ⊆ `granted_tools(p, now)`. Follows from
//!   the tool and expiry axes of `covers` alone (a child grant may name a
//!   live tool yet admit no call, so this is not a corollary of INV-1).
//! - **INV-2 (deny by default)** — an empty grant set allows nothing; a tool
//!   no grant names is [`DenialReason::NotGranted`].
//! - **INV-3 (an expired grant is no grant)** — `t ∈ granted_tools(g, now)`
//!   ⇔ `tool_status(g, t, now)` is [`ToolStatus::Live`] ⇔ for every call on
//!   `t`, `decide` is neither `NotGranted` nor `Expired`.
//! - **INV-4 (determinism and totality)** — `decide`, `covers`,
//!   `attenuates`, `includes` are pure, total functions of their arguments:
//!   no clock, no I/O, no panics, iteration order fixed by `BTreeMap`/`Vec`.
//! - **INV-5 (attenuation is a preorder)** — `attenuates` is reflexive
//!   (self-delegation is legal; "strictly attenuates" in DESIGN means
//!   *always enforced ⊆*, not proper subset) and transitive (the root's
//!   envelope bounds the whole agent tree).
//! - **INV-6 (monotonicity)** — adding a grant to the parent, removing a
//!   grant from the child, or tightening a child grant (narrower prefix or
//!   suffix, smaller `OneOf`, tighter bounds, earlier or newly set expiry,
//!   an added constraint) preserves `attenuates(p, c).is_ok()`.
//!
//! `attenuates` is **sound but not complete**: it may refuse a child that
//! is semantically a subset (for instance one covered only by the union of
//! two parent grants); it never accepts one that is not. Soundness is the
//! property the theorem needs; incompleteness only ever costs a delegation
//! that must be written more explicitly.
//!
//! # Example
//!
//! ```
//! use std::collections::BTreeMap;
//! use flavium_core::{
//!     attenuates, ArgValue, Constraint, Decision, DenialReason, Grant, GrantEnvelope,
//!     Principal, Timestamp, ToolCall, ToolName,
//! };
//!
//! # fn main() -> Result<(), flavium_core::InvalidName> {
//! let mut constraints = BTreeMap::new();
//! constraints.insert("path".to_string(), Constraint::Prefix("/data/invoices/".into()));
//! let read_invoices = Grant {
//!     tool: ToolName::new("read_file")?,
//!     constraints,
//!     expires: Some(Timestamp::from_unix_secs(1_800_000_000)),
//! };
//! let envelope = GrantEnvelope {
//!     principal: Principal::new("invoice-bot")?,
//!     grants: vec![read_invoices.clone()],
//! };
//!
//! let now = Timestamp::from_unix_secs(1_700_000_000);
//! let call = |path: &str| ToolCall {
//!     tool: "read_file".into(),
//!     args: BTreeMap::from([("path".to_string(), ArgValue::Str(path.into()))]),
//! };
//! assert_eq!(envelope.decide(&call("/data/invoices/2026-01.pdf"), now), Decision::Allow { grant: 0 });
//! assert_eq!(
//!     envelope.decide(&call("/etc/passwd"), now),
//!     Decision::Deny(DenialReason::OutOfEnvelope)
//! );
//!
//! // A child may narrow the prefix …
//! let mut narrower = read_invoices.clone();
//! narrower.constraints.insert("path".into(), Constraint::Prefix("/data/invoices/2026-".into()));
//! assert!(attenuates(&envelope.grants, &[narrower]).is_ok());
//! // … but never widen it.
//! let mut wider = read_invoices;
//! wider.constraints.insert("path".into(), Constraint::Prefix("/data/".into()));
//! assert!(attenuates(&envelope.grants, &[wider]).is_err());
//! # Ok(()) }
//! ```
//!
//! # Notes for verification tooling
//!
//! The decision and attenuation logic is reachable without constructing
//! envelopes or principals: the free functions over `&[Grant]`
//! ([`decide`], [`tool_status`], [`admitting_grants`], [`granted_tools`],
//! [`attenuates`]) and the `Constraint` methods over one value
//! ([`Constraint::admits`], [`Constraint::includes`]). The `Option<i64>`
//! bound helpers in `constraint` are crate-private, so a harness for them
//! lives in-crate (e.g. under `#[cfg(kani)]`). `trace` (which holds a
//! `Mutex` and a boxed error) and `authorize` (a trait) are outside the
//! harness set.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod attenuate;
pub mod authorize;
pub mod constraint;
pub mod grant;
pub mod name;
pub mod time;
pub mod trace;

pub use attenuate::{attenuates, Axis, Uncovered};
pub use authorize::Authorizer;
pub use constraint::{ArgValue, Constraint};
pub use grant::{
    admitting_grants, decide, granted_tools, tool_status, Decision, DenialReason, Grant,
    GrantEnvelope, ToolCall, ToolStatus,
};
pub use name::{InvalidName, Principal, ToolName};
pub use time::Timestamp;
pub use trace::{
    CallId, CallOutcome, DiscardKind, MemorySink, NotForwardedReason, NullSink, RefusalReason,
    SessionEndReason, SinkError, TraceEvent, TraceSink,
};
