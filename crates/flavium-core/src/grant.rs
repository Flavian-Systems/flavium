//! Grants, envelopes, calls, and the reference decision semantics.
//!
//! [`decide`] is the executable specification of what a set of grants
//! *means*: it is what `flavium-policy`'s Cedar-backed authorizer is tested
//! against, and what the attenuation property test uses as its oracle. It
//! is deliberately not clever — a reader should be able to confirm the
//! crate-level "semantics in one paragraph" line by line here.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::constraint::{ArgValue, Constraint};
use crate::name::{Principal, ToolName};
use crate::time::Timestamp;

/// Authority over one tool: which arguments must look like what, and until
/// when.
///
/// A grant is *live* at `now` iff it never expires or `now < expires`
/// (invariant **INV-3**: at `now == expires` it is already gone). It
/// *admits* a call iff the call names its tool and every constrained
/// argument is admitted by its constraint ([`Constraint::admits`]); the
/// call's other arguments are not examined.
///
/// The budget axis (DESIGN §3: `budget 5/day`) is reserved for T2 and not
/// modelled yet — a field that is parsed but not enforced would be a lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The tool this grant authorizes.
    pub tool: ToolName,
    /// Per-argument constraints, keyed by argument name.
    pub constraints: BTreeMap<String, Constraint>,
    /// When the grant stops existing; `None` = never.
    pub expires: Option<Timestamp>,
}

impl Grant {
    /// Is the grant live at `now`? (`now < expires`, or no expiry.)
    pub fn is_live(&self, now: Timestamp) -> bool {
        match self.expires {
            None => true,
            Some(expires) => now < expires,
        }
    }

    /// Does the grant admit `call`, ignoring time? True iff the call names
    /// this grant's tool and every constraint admits the call's value for
    /// its argument (`None` when the argument is missing).
    pub fn admits(&self, call: &ToolCall) -> bool {
        if self.tool.as_str() != call.tool {
            return false;
        }
        self.constraints
            .iter()
            .all(|(argument, constraint)| constraint.admits(call.args.get(argument)))
    }
}

/// One `tools/call` as the core sees it: the requested tool name and its
/// arguments, converted from JSON by the caller.
///
/// `tool` is a plain `String` — clients may ask for any name; one that could
/// never be a valid [`ToolName`] simply matches no grant. Conversion rules
/// for `args` (applied by the proxy, documented here so the semantics are
/// in one place): a missing or `null` `arguments` object is an empty map;
/// JSON strings become [`ArgValue::Str`], integers that fit `i64` become
/// [`ArgValue::Int`], everything else [`ArgValue::Other`]. An `arguments`
/// that is not an object, or has duplicate keys, is refused before
/// conversion ([`crate::RefusalReason::MalformedParams`]) and never becomes
/// a `ToolCall`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// The requested tool name, as sent.
    pub tool: String,
    /// The call's arguments by name.
    pub args: BTreeMap<String, ArgValue>,
}

/// Where a tool stands with respect to a set of grants at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// No grant names the tool.
    NotGranted,
    /// Grants name the tool, but none is live at `now`.
    Expired,
    /// At least one live grant names the tool.
    Live,
}

/// Why a call was denied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenialReason {
    /// No grant names the tool. Client-visible as an unknown tool.
    NotGranted,
    /// Every grant naming the tool has expired. Client-visible as an
    /// unknown tool — an expired grant is no grant.
    Expired,
    /// A live grant names the tool, but no live grant admits these
    /// arguments. Client-visible as a policy denial the agent can act on.
    OutOfEnvelope,
    /// The runtime engine could not evaluate the call and denied it, fail
    /// closed. Never produced by [`decide`]; carried here so decisions and
    /// trace events share one type.
    EvaluationError {
        /// Operator-facing diagnostic; never shown to the client.
        detail: String,
    },
}

impl fmt::Display for DenialReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DenialReason::NotGranted => f.write_str("tool not granted"),
            DenialReason::Expired => f.write_str("every grant for the tool has expired"),
            DenialReason::OutOfEnvelope => f.write_str("arguments outside the grant envelope"),
            DenialReason::EvaluationError { detail } => {
                write!(f, "policy evaluation failed: {detail}")
            }
        }
    }
}

/// The outcome of authorizing one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Allowed by the grant at this index in the grant set (the first
    /// admitting live grant, in set order).
    Allow {
        /// Index into the grant set that was evaluated.
        grant: usize,
    },
    /// Denied, with the reason.
    Deny(DenialReason),
}

impl Decision {
    /// True for [`Decision::Allow`].
    pub fn is_allow(&self) -> bool {
        matches!(self, Decision::Allow { .. })
    }
}

/// The grants one principal holds — its *envelope*, the union of what it
/// may do.
///
/// The methods delegate to the free functions over `&[Grant]` below, which
/// are the primitives verification harnesses drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantEnvelope {
    /// The holder.
    pub principal: Principal,
    /// The grants, in file order (indices are stable and appear in
    /// [`Decision::Allow`] and in the trace — do not re-sort or dedupe).
    pub grants: Vec<Grant>,
}

impl GrantEnvelope {
    /// See [`tool_status`].
    pub fn tool_status(&self, tool: &str, now: Timestamp) -> ToolStatus {
        tool_status(&self.grants, tool, now)
    }

    /// See [`admitting_grants`].
    pub fn admitting_grants(&self, call: &ToolCall, now: Timestamp) -> Vec<usize> {
        admitting_grants(&self.grants, call, now)
    }

    /// See [`decide`].
    pub fn decide(&self, call: &ToolCall, now: Timestamp) -> Decision {
        decide(&self.grants, call, now)
    }

    /// See [`granted_tools`].
    pub fn granted_tools(&self, now: Timestamp) -> BTreeSet<ToolName> {
        granted_tools(&self.grants, now)
    }
}

/// Where `tool` stands at `now`: [`ToolStatus::Live`] if some live grant
/// names it, [`ToolStatus::Expired`] if grants name it but none is live,
/// [`ToolStatus::NotGranted`] otherwise. Maintains **INV-2** and **INV-3**.
pub fn tool_status(grants: &[Grant], tool: &str, now: Timestamp) -> ToolStatus {
    let mut named = false;
    for grant in grants {
        if grant.tool.as_str() == tool {
            if grant.is_live(now) {
                return ToolStatus::Live;
            }
            named = true;
        }
    }
    if named {
        ToolStatus::Expired
    } else {
        ToolStatus::NotGranted
    }
}

/// The indices of every grant that is live at `now` and admits `call`, in
/// set order. Empty when the call is not allowed.
pub fn admitting_grants(grants: &[Grant], call: &ToolCall, now: Timestamp) -> Vec<usize> {
    grants
        .iter()
        .enumerate()
        .filter(|(_, grant)| grant.is_live(now) && grant.admits(call))
        .map(|(index, _)| index)
        .collect()
}

/// The reference decision: does `grants` allow `call` at `now`?
///
/// - no grant names the tool ⇒ `Deny(NotGranted)`;
/// - grants name it but none is live ⇒ `Deny(Expired)`;
/// - some live grant admits the call ⇒ `Allow { grant }` with the first
///   such index; otherwise ⇒ `Deny(OutOfEnvelope)`.
///
/// Pure and total (**INV-4**); denies by default (**INV-2**); treats
/// expired grants as absent (**INV-3**). This is the specification the
/// runtime engine is measured against, not the engine itself.
pub fn decide(grants: &[Grant], call: &ToolCall, now: Timestamp) -> Decision {
    match tool_status(grants, &call.tool, now) {
        ToolStatus::NotGranted => Decision::Deny(DenialReason::NotGranted),
        ToolStatus::Expired => Decision::Deny(DenialReason::Expired),
        ToolStatus::Live => match admitting_grants(grants, call, now).first() {
            Some(&grant) => Decision::Allow { grant },
            None => Decision::Deny(DenialReason::OutOfEnvelope),
        },
    }
}

/// The tools some live grant names at `now` — what a `tools/list` may show.
/// Agrees with [`tool_status`] (**INV-3**).
pub fn granted_tools(grants: &[Grant], now: Timestamp) -> BTreeSet<ToolName> {
    grants
        .iter()
        .filter(|grant| grant.is_live(now))
        .map(|grant| grant.tool.clone())
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn t(secs: i64) -> Timestamp {
        Timestamp::from_unix_secs(secs)
    }
    fn tool(name: &str) -> ToolName {
        ToolName::new(name).unwrap()
    }
    fn grant(name: &str, constraints: &[(&str, Constraint)], expires: Option<i64>) -> Grant {
        Grant {
            tool: tool(name),
            constraints: constraints
                .iter()
                .map(|(k, c)| (k.to_string(), c.clone()))
                .collect(),
            expires: expires.map(t),
        }
    }
    fn call(name: &str, args: &[(&str, ArgValue)]) -> ToolCall {
        ToolCall {
            tool: name.to_string(),
            args: args
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        }
    }
    fn s(v: &str) -> ArgValue {
        ArgValue::Str(v.to_string())
    }
    fn prefix(p: &str) -> Constraint {
        Constraint::Prefix(p.to_string())
    }
    fn envelope(grants: Vec<Grant>) -> GrantEnvelope {
        GrantEnvelope {
            principal: Principal::new("bot").unwrap(),
            grants,
        }
    }

    #[test]
    fn empty_envelope_allows_nothing() {
        let e = envelope(vec![]);
        assert_eq!(
            e.decide(&call("read_file", &[]), t(0)),
            Decision::Deny(DenialReason::NotGranted)
        );
        assert!(e.granted_tools(t(0)).is_empty());
        assert_eq!(e.tool_status("read_file", t(0)), ToolStatus::NotGranted);
    }

    #[test]
    fn allow_reports_first_admitting_live_grant() {
        let e = envelope(vec![
            grant("read_file", &[("path", prefix("/a"))], None),
            grant("read_file", &[("path", prefix("/b"))], None),
            grant("read_file", &[], None),
        ]);
        let now = t(0);
        assert_eq!(
            e.decide(&call("read_file", &[("path", s("/a/1"))]), now),
            Decision::Allow { grant: 0 }
        );
        assert_eq!(
            e.decide(&call("read_file", &[("path", s("/b/1"))]), now),
            Decision::Allow { grant: 1 }
        );
        assert_eq!(
            e.decide(&call("read_file", &[("path", s("/c"))]), now),
            Decision::Allow { grant: 2 }
        );
        assert_eq!(
            e.admitting_grants(&call("read_file", &[("path", s("/a/1"))]), now),
            vec![0, 2]
        );
        assert!(e.decide(&call("read_file", &[]), now).is_allow());

        // An expired grant that would admit is skipped; a later live grant
        // that admits is chosen; a live one that does not admit ⇒ deny.
        let e = envelope(vec![
            grant("a", &[], Some(5)),
            grant("a", &[("x", prefix("/q"))], None),
        ]);
        assert_eq!(
            e.decide(&call("a", &[("x", s("/zzz"))]), t(6)),
            Decision::Deny(DenialReason::OutOfEnvelope)
        );
        assert!(e
            .admitting_grants(&call("a", &[("x", s("/zzz"))]), t(6))
            .is_empty());
        assert_eq!(
            e.decide(&call("a", &[("x", s("/q/1"))]), t(6)),
            Decision::Allow { grant: 1 }
        );
        assert_eq!(
            e.decide(&call("a", &[("x", s("/zzz"))]), t(0)),
            Decision::Allow { grant: 0 }
        );
    }

    #[test]
    fn a_grant_for_another_tool_lends_no_authority() {
        // Grant 0 admits everything — for tool "a". A call to "b" must be
        // judged by "b"'s grants alone.
        let e = envelope(vec![
            grant("a", &[], None),
            grant("b", &[("x", prefix("/q"))], None),
        ]);
        let c = call("b", &[("x", s("/zzz"))]);
        assert_eq!(
            e.decide(&c, t(0)),
            Decision::Deny(DenialReason::OutOfEnvelope)
        );
        assert!(e.admitting_grants(&c, t(0)).is_empty());
        assert!(!grant("a", &[], None).admits(&call("b", &[])));
        assert!(grant("a", &[], None).admits(&call("a", &[])));
    }

    #[test]
    fn denial_reasons_in_precedence_order() {
        let e = envelope(vec![
            grant("read_file", &[("path", prefix("/a"))], Some(10)),
            grant(
                "send_mail",
                &[("to", Constraint::Suffix("@yourco.com".into()))],
                Some(5),
            ),
        ]);
        // Not granted at all.
        assert_eq!(
            e.decide(&call("delete_file", &[]), t(0)),
            Decision::Deny(DenialReason::NotGranted)
        );
        // Granted but expired — regardless of the arguments.
        assert_eq!(
            e.decide(&call("send_mail", &[("to", s("a@yourco.com"))]), t(5)),
            Decision::Deny(DenialReason::Expired)
        );
        assert_eq!(
            e.decide(&call("send_mail", &[("to", s("a@evil.com"))]), t(99)),
            Decision::Deny(DenialReason::Expired)
        );
        // Live but out of envelope.
        assert_eq!(
            e.decide(&call("send_mail", &[("to", s("a@evil.com"))]), t(4)),
            Decision::Deny(DenialReason::OutOfEnvelope)
        );
        // Live, constrained argument missing ⇒ out of envelope.
        assert_eq!(
            e.decide(&call("read_file", &[]), t(0)),
            Decision::Deny(DenialReason::OutOfEnvelope)
        );
        // Live, constrained argument unrepresentable ⇒ out of envelope.
        assert_eq!(
            e.decide(&call("read_file", &[("path", ArgValue::Other)]), t(0)),
            Decision::Deny(DenialReason::OutOfEnvelope)
        );
        // Live, wrong type ⇒ out of envelope.
        assert_eq!(
            e.decide(&call("read_file", &[("path", ArgValue::Int(1))]), t(0)),
            Decision::Deny(DenialReason::OutOfEnvelope)
        );
    }

    #[test]
    fn expiry_boundary_is_exclusive() {
        let g = grant("read_file", &[], Some(10));
        assert!(g.is_live(t(9)));
        assert!(!g.is_live(t(10)));
        assert!(!g.is_live(t(11)));
        assert!(grant("read_file", &[], None).is_live(t(i64::MAX)));
        let e = envelope(vec![g]);
        assert_eq!(e.tool_status("read_file", t(9)), ToolStatus::Live);
        assert_eq!(e.tool_status("read_file", t(10)), ToolStatus::Expired);
        let before = e.granted_tools(t(9));
        let after = e.granted_tools(t(10));
        assert!(before.contains("read_file"));
        assert!(!after.contains("read_file"));
    }

    #[test]
    fn unconstrained_arguments_are_not_examined() {
        let e = envelope(vec![grant(
            "send_mail",
            &[("to", Constraint::Suffix("@yourco.com".into()))],
            None,
        )]);
        // `cc` is not constrained, so anything goes there — the documented
        // authoring pitfall; `Absent` is how a grant closes it.
        assert!(e
            .decide(
                &call(
                    "send_mail",
                    &[("to", s("a@yourco.com")), ("cc", s("x@evil.com"))]
                ),
                t(0)
            )
            .is_allow());
        let strict = envelope(vec![grant(
            "send_mail",
            &[
                ("to", Constraint::Suffix("@yourco.com".into())),
                ("cc", Constraint::Absent),
            ],
            None,
        )]);
        assert_eq!(
            strict.decide(
                &call(
                    "send_mail",
                    &[("to", s("a@yourco.com")), ("cc", s("x@evil.com"))]
                ),
                t(0)
            ),
            Decision::Deny(DenialReason::OutOfEnvelope)
        );
        assert!(strict
            .decide(&call("send_mail", &[("to", s("a@yourco.com"))]), t(0))
            .is_allow());
    }

    #[test]
    fn granted_tools_lists_live_tools_once() {
        let e = envelope(vec![
            grant("a", &[], Some(5)),
            grant("a", &[], None),
            grant("b", &[], Some(5)),
        ]);
        let names = |now: i64| -> Vec<String> {
            let live = e.granted_tools(t(now));
            live.into_iter().map(|name| name.to_string()).collect()
        };
        assert_eq!(names(7), vec!["a"]);
        assert_eq!(names(0), vec!["a", "b"]);
    }

    #[test]
    fn unrepresentable_tool_names_are_simply_not_granted() {
        let e = envelope(vec![grant("read_file", &[], None)]);
        assert_eq!(
            e.decide(&call("read\nfile", &[]), t(0)),
            Decision::Deny(DenialReason::NotGranted)
        );
        assert_eq!(
            e.decide(&call("", &[]), t(0)),
            Decision::Deny(DenialReason::NotGranted)
        );
    }

    #[test]
    fn display_of_reasons() {
        assert_eq!(DenialReason::NotGranted.to_string(), "tool not granted");
        assert_eq!(
            DenialReason::EvaluationError {
                detail: "boom".into()
            }
            .to_string(),
            "policy evaluation failed: boom"
        );
    }
}
