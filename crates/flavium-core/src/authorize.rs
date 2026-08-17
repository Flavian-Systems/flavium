//! The [`Authorizer`] trait: the seam through which the proxy asks "may this
//! principal make this call now?" without knowing which engine answers.
//!
//! The runtime engine lives in `flavium-policy` (Cedar evaluation, and from
//! T2a stateful budget metering — both must pass). It implements this trait;
//! the CLI wires it; the proxy depends only on this crate. The reference
//! implementation on [`GrantEnvelope`] is the *specification* the engine is
//! tested against and a convenient test double — it is not what production
//! runs.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::grant::{Decision, DenialReason, GrantEnvelope, ToolCall};
use crate::name::{Principal, ToolName};
use crate::time::Timestamp;

/// Answers authorization questions for principals. No I/O, no clock: the
/// caller supplies `now`, so every answer is replayable.
pub trait Authorizer: Send + Sync {
    /// May `principal` make `call` at `now`? Never panics; an engine that
    /// cannot evaluate answers `Deny(EvaluationError)`.
    fn authorize(&self, principal: &Principal, call: &ToolCall, now: Timestamp) -> Decision;

    /// The tools `principal` holds a live grant for at `now` — the set a
    /// `tools/list` may show. Must agree with [`Authorizer::authorize`] on
    /// the tool axis: a tool outside this set is `NotGranted` or `Expired`
    /// for every call (**INV-3**).
    fn granted_tools(&self, principal: &Principal, now: Timestamp) -> BTreeSet<ToolName>;
}

/// The reference authorizer: an envelope authorizes exactly its holder,
/// per [`crate::decide`]. Any other principal holds nothing here.
///
/// Note the name overlap: `GrantEnvelope` also has the inherent
/// single-argument [`GrantEnvelope::granted_tools`]`(now)`; method-call
/// syntax on a concrete envelope resolves to that one, so to call *this*
/// trait method on a concrete envelope write
/// `Authorizer::granted_tools(&envelope, &principal, now)` (an arity
/// mismatch is a compile error, never a silent fallback).
impl Authorizer for GrantEnvelope {
    fn authorize(&self, principal: &Principal, call: &ToolCall, now: Timestamp) -> Decision {
        if *principal != self.principal {
            return Decision::Deny(DenialReason::NotGranted);
        }
        self.decide(call, now)
    }

    fn granted_tools(&self, principal: &Principal, now: Timestamp) -> BTreeSet<ToolName> {
        if *principal != self.principal {
            return BTreeSet::new();
        }
        GrantEnvelope::granted_tools(self, now)
    }
}

impl<T: Authorizer + ?Sized> Authorizer for Arc<T> {
    fn authorize(&self, principal: &Principal, call: &ToolCall, now: Timestamp) -> Decision {
        (**self).authorize(principal, call, now)
    }

    fn granted_tools(&self, principal: &Principal, now: Timestamp) -> BTreeSet<ToolName> {
        (**self).granted_tools(principal, now)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::grant::Grant;
    use std::collections::BTreeMap;

    fn envelope() -> GrantEnvelope {
        GrantEnvelope {
            principal: Principal::new("bot").unwrap(),
            grants: vec![Grant {
                tool: ToolName::new("read_file").unwrap(),
                constraints: BTreeMap::new(),
                expires: None,
            }],
        }
    }
    fn call() -> ToolCall {
        ToolCall {
            tool: "read_file".into(),
            args: BTreeMap::new(),
        }
    }

    #[test]
    fn envelope_authorizes_only_its_holder() {
        let e = envelope();
        let now = Timestamp::from_unix_secs(0);
        let bot = Principal::new("bot").unwrap();
        let other = Principal::new("other").unwrap();
        assert!(e.authorize(&bot, &call(), now).is_allow());
        assert_eq!(
            e.authorize(&other, &call(), now),
            Decision::Deny(DenialReason::NotGranted)
        );
        assert!(Authorizer::granted_tools(&e, &bot, now).contains("read_file"));
        assert!(Authorizer::granted_tools(&e, &other, now).is_empty());
    }

    #[test]
    fn works_behind_arc_dyn() {
        let shared: Arc<dyn Authorizer> = Arc::new(envelope());
        let bot = Principal::new("bot").unwrap();
        assert!(shared
            .authorize(&bot, &call(), Timestamp::from_unix_secs(0))
            .is_allow());
        assert_eq!(
            shared
                .granted_tools(&bot, Timestamp::from_unix_secs(0))
                .len(),
            1
        );
    }
}
