//! Flavium's policy engine: grants compiled to Cedar, and the authorizer
//! that answers with them.
//!
//! This crate is one half of the enforcement core (CLAUDE.md): it is kept
//! small and dependency-light, has no `unsafe`, no `unwrap`/`expect` outside
//! tests, no clock and no I/O, and every function on the request path is
//! total — it returns a [`Decision`](flavium_core::Decision) for every input
//! and never panics.
//!
//! # Specification versus engine
//!
//! [`flavium_core::decide`] is the *specification* of what a grant means. It
//! is small enough to read line by line and is what auditors and verification
//! tools are pointed at. It is not what production runs.
//!
//! This crate is the *engine*: [`CedarAuthorizer`] answers the same questions
//! using [Cedar](https://www.cedarpolicy.com/), which has a formal semantics
//! and a mechanised model behind it, so the policy-evaluation half of the
//! verification story is someone else's proven work. The two are held
//! together by one property, not by inspection:
//!
//! > **P1 (agreement)** — for every envelope, call and time,
//! > `CedarAuthorizer::authorize` returns exactly what
//! > [`flavium_core::decide`] returns: the same
//! > [`Decision`](flavium_core::Decision), the same grant index, the same
//! > [`DenialReason`](flavium_core::DenialReason).
//!
//! `tests/differential.rs` is that property, run over thousands of randomly
//! generated envelopes, calls and times. If Cedar and the specification ever
//! disagree, that test is where it shows up.
//!
//! # The two halves
//!
//! - [`compile()`] — envelope → [`PolicySet`](cedar_policy::PolicySet), once,
//!   at startup. One `permit` per grant; the policy id is the grant's index,
//!   so Cedar's answer maps back to the grant that authorized the call.
//! - [`request_context`] — call → the four-key Cedar context, once per call.
//! - [`CedarAuthorizer`] — the two joined, implementing
//!   [`flavium_core::Authorizer`], which is all the proxy ever sees. The
//!   proxy does not depend on this crate or on Cedar; the CLI wires them.
//!
//! Flavium never asks anyone to write Cedar. Grants are the user-facing
//! language; Cedar is an implementation detail of enforcing them.
//!
//! # Invariants
//!
//! Stated once, here; each item's documentation names the ones it maintains.
//!
//! - **P1 (agreement)** — as above: the engine decides what the
//!   specification decides, reason and grant index included.
//! - **P2 (deny by default)** — no policy matching means denied. An envelope
//!   with no grants compiles to an empty policy set, which allows nothing,
//!   and flavium never emits a `forbid`: absence already denies, so there is
//!   no rule ordering to get wrong.
//! - **P3 (fail closed)** — any Cedar evaluation error, any determining
//!   policy id that is not a grant index, any principal mismatch ⇒ denied.
//!   The engine failing is never a reason to allow.
//! - **P4 (nothing is ever parsed)** — no name or value from a grant or a
//!   call is ever handed to something that could read it as more than data.
//!   Policies and entity UIDs are built as Cedar's structured JSON, never
//!   formatted as Cedar source; the per-call context is built as
//!   [`RestrictedExpression`](cedar_policy::RestrictedExpression)s, so it does
//!   not pass through Cedar's JSON *value* grammar either.
//! - **P5 (total context)** — the request context always carries `str`,
//!   `int`, `present` and `now`, so no generated policy can reference an
//!   attribute that is not there. This is what makes a Cedar evaluation error
//!   unreachable rather than merely unlikely.
//!
//! P4 and P5 are the two that earn their keep. P4 is the reason a path or an
//! address in a grant file cannot become policy syntax — the injection seam
//! this product exists to argue against. P5 is the reason the engine and the
//! specification agree on hostile input: an argument that is missing or of
//! the wrong type fails a `has` guard and denies, instead of raising an error
//! that the two implementations would answer differently.
//!
//! P4 is stated as "nothing is ever parsed" rather than "no string is ever
//! interpolated" because the weaker version was not enough. The context was
//! first built as a `serde_json::Value` — no interpolation anywhere — and a
//! call whose only string argument was named `__expr` still broke P1: that is
//! a reserved escape in Cedar's JSON value grammar, so the context failed to
//! parse and the engine denied a call the specification allows. Not being
//! *formatted* into a grammar is no protection if you are still *read* by
//! one. The differential suite's hostile universe now carries `__expr` as a
//! regression guard.
//!
//! # Example
//!
//! ```
//! use std::collections::BTreeMap;
//! use flavium_core::{
//!     ArgValue, Authorizer, Constraint, Decision, DenialReason, Grant, GrantEnvelope,
//!     Principal, Timestamp, ToolCall, ToolName, decide,
//! };
//! use flavium_policy::CedarAuthorizer;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut constraints = BTreeMap::new();
//! constraints.insert("to".to_string(), Constraint::Suffix("@yourco.com".into()));
//! constraints.insert("bcc".to_string(), Constraint::Absent);
//! let envelope = GrantEnvelope {
//!     principal: Principal::new("mail-bot")?,
//!     grants: vec![Grant {
//!         tool: ToolName::new("send_mail")?,
//!         constraints,
//!         expires: None,
//!     }],
//! };
//! let engine = CedarAuthorizer::new(envelope.clone())?;
//!
//! let bot = Principal::new("mail-bot")?;
//! let now = Timestamp::from_unix_secs(1_700_000_000);
//! let call = |to: &str| ToolCall {
//!     tool: "send_mail".into(),
//!     args: BTreeMap::from([("to".to_string(), ArgValue::Str(to.into()))]),
//! };
//!
//! assert_eq!(engine.authorize(&bot, &call("alice@yourco.com"), now), Decision::Allow { grant: 0 });
//! assert_eq!(
//!     engine.authorize(&bot, &call("attacker@evil.com"), now),
//!     Decision::Deny(DenialReason::OutOfEnvelope)
//! );
//!
//! // P1: the engine and the specification agree, always.
//! for to in ["alice@yourco.com", "attacker@evil.com", "x@yourco.com.evil"] {
//!     assert_eq!(engine.authorize(&bot, &call(to), now), decide(&envelope.grants, &call(to), now));
//! }
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod authorizer;
pub mod compile;
pub mod context;

pub use authorizer::CedarAuthorizer;
pub use compile::{compile, CompileError};
pub use context::{request_context, ContextError};
