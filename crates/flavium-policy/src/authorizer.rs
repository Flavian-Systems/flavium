//! [`CedarAuthorizer`]: the runtime engine behind
//! [`flavium_core::Authorizer`].
//!
//! # How one call is decided
//!
//! Four steps, and only the third is Cedar's:
//!
//! 1. **Principal.** A principal that is not the envelope's holder holds
//!    nothing here — [`DenialReason::NotGranted`], without asking Cedar.
//! 2. **Tool.** [`flavium_core::tool_status`] says whether the tool is
//!    `NotGranted`, `Expired` or `Live`, and the first two are returned as
//!    such. Cedar has no vocabulary for the difference: it answers Allow or
//!    Deny, and "the tool is not in your envelope at all" versus "your grant
//!    for it expired" versus "these arguments are outside it" is exactly the
//!    distinction the client sees — the first two are indistinguishable from
//!    an unknown tool (`-32602`), the third is a recoverable error result the
//!    agent can act on. Deriving it from the envelope also keeps
//!    [`Authorizer::granted_tools`] and [`Authorizer::authorize`] agreeing on
//!    the tool axis by construction (**INV-3**) rather than by two
//!    implementations happening to match.
//! 3. **Cedar**, only for a `Live` tool, against the policy set compiled at
//!    startup and the context built for this call.
//! 4. **Classification.** Any evaluation error ⇒
//!    [`DenialReason::EvaluationError`]; a clean `Deny` ⇒
//!    [`DenialReason::OutOfEnvelope`]; `Allow` ⇒
//!    [`Decision::Allow`] naming the *lowest* determining policy id.
//!
//! The lowest, not the first: Cedar reports the determining policies as an
//! unordered set (verified — it comes back in a different order than the
//! grants went in), so taking the minimum index is what reproduces the
//! reference semantics' "first admitting live grant".
//!
//! Because Cedar is only ever asked about a tool some grant names, the
//! resource UID is built from the grant's validated
//! [`ToolName`] — a client's arbitrary tool string
//! never reaches the engine.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use cedar_policy::{Entities, EntityUid, PolicySet, Request, Response};
use flavium_core::{
    Authorizer, Decision, DenialReason, GrantEnvelope, Principal, Timestamp, ToolCall, ToolName,
    ToolStatus,
};

use crate::compile::{action_uid, compile, principal_uid, tool_uids, CompileError};
use crate::context::request_context;

/// The Cedar-backed [`Authorizer`]: it decides exactly what
/// [`flavium_core::decide`] specifies (**P1**), and denies whenever it cannot
/// (**P3**).
///
/// Built once from an envelope and used for every call in the session. The
/// compile happens up front so that a grant which cannot be compiled stops
/// startup — when an operator is watching and no agent is running — rather
/// than surfacing mid-session as a denial that looks like policy.
///
/// # Example
///
/// ```
/// use std::collections::BTreeMap;
/// use flavium_core::{
///     ArgValue, Authorizer, Constraint, Decision, DenialReason, Grant, GrantEnvelope,
///     Principal, Timestamp, ToolCall, ToolName,
/// };
/// use flavium_policy::CedarAuthorizer;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let envelope = GrantEnvelope {
///     principal: Principal::new("invoice-bot")?,
///     grants: vec![Grant {
///         tool: ToolName::new("read_file")?,
///         constraints: BTreeMap::from([(
///             "path".to_string(),
///             Constraint::Prefix("/data/invoices/".into()),
///         )]),
///         expires: Some(Timestamp::from_unix_secs(1_800_000_000)),
///     }],
/// };
/// let engine = CedarAuthorizer::new(envelope)?;
///
/// let bot = Principal::new("invoice-bot")?;
/// let now = Timestamp::from_unix_secs(1_700_000_000);
/// let call = |path: &str| ToolCall {
///     tool: "read_file".into(),
///     args: BTreeMap::from([("path".to_string(), ArgValue::Str(path.into()))]),
/// };
///
/// assert_eq!(
///     engine.authorize(&bot, &call("/data/invoices/2026-01.pdf"), now),
///     Decision::Allow { grant: 0 }
/// );
/// assert_eq!(
///     engine.authorize(&bot, &call("/etc/passwd"), now),
///     Decision::Deny(DenialReason::OutOfEnvelope)
/// );
/// // Past the expiry, an expired grant is no grant.
/// assert_eq!(
///     engine.authorize(&bot, &call("/data/invoices/2026-01.pdf"), Timestamp::from_unix_secs(1_900_000_000)),
///     Decision::Deny(DenialReason::Expired)
/// );
/// # Ok(()) }
/// ```
pub struct CedarAuthorizer {
    /// The envelope this authorizer speaks for. Kept because the tool axis
    /// and the principal check are answered from it, not from Cedar.
    envelope: GrantEnvelope,
    /// One `permit` per grant, policy id = grant index.
    policies: PolicySet,
    /// The holder's UID, built once.
    principal: EntityUid,
    /// `Flavium::Action::"call"`, built once.
    action: EntityUid,
    /// Every granted tool's UID, built once, so the request path never
    /// constructs one from a client-supplied string.
    tools: BTreeMap<ToolName, EntityUid>,
    /// Flavium has no entity hierarchy: all authority is in the policies.
    entities: Entities,
    /// Cedar's evaluator (stateless).
    engine: cedar_policy::Authorizer,
}

impl CedarAuthorizer {
    /// Compiles `envelope` and returns the authorizer that enforces it.
    ///
    /// # Errors
    ///
    /// [`CompileError`] if Cedar refuses a generated policy or entity id —
    /// a startup failure by design (**D7**), never a request-path one.
    pub fn new(envelope: GrantEnvelope) -> Result<Self, CompileError> {
        let policies = compile(&envelope)?;
        let principal = principal_uid(&envelope.principal)?;
        let action = action_uid()?;
        let tools = tool_uids(&envelope)?;
        Ok(CedarAuthorizer {
            envelope,
            policies,
            principal,
            action,
            tools,
            entities: Entities::empty(),
            engine: cedar_policy::Authorizer::new(),
        })
    }

    /// The envelope this authorizer enforces.
    pub fn envelope(&self) -> &GrantEnvelope {
        &self.envelope
    }

    /// The compiled policy set — a stable artifact a debug command or a
    /// trace can print, so an operator can see what their grants became.
    pub fn policies(&self) -> &PolicySet {
        &self.policies
    }

    /// Asks Cedar about a call on a tool known to be live.
    fn ask_cedar(&self, tool: &EntityUid, call: &ToolCall, now: Timestamp) -> Decision {
        let context = match request_context(call, now) {
            Ok(context) => context,
            Err(error) => return evaluation_error(format!("request context: {error}")),
        };
        let request = match Request::new(
            self.principal.clone(),
            self.action.clone(),
            tool.clone(),
            context,
            None,
        ) {
            Ok(request) => request,
            Err(error) => return evaluation_error(format!("request: {error}")),
        };
        self.classify(
            &self
                .engine
                .is_authorized(&request, &self.policies, &self.entities),
        )
    }

    /// Turns Cedar's answer into a [`Decision`], failing closed (**P3**).
    fn classify(&self, response: &Response) -> Decision {
        // Errors first and unconditionally: the engine failing to evaluate is
        // never a reason to allow, whatever decision it reached alongside.
        let errors: Vec<String> = response
            .diagnostics()
            .errors()
            .map(|error| error.to_string())
            .collect();
        if !errors.is_empty() {
            return evaluation_error(errors.join("; "));
        }
        match response.decision() {
            cedar_policy::Decision::Deny => Decision::Deny(DenialReason::OutOfEnvelope),
            cedar_policy::Decision::Allow => {
                let mut lowest: Option<usize> = None;
                for id in response.diagnostics().reason() {
                    let text = id.to_string();
                    let index = match text.parse::<usize>() {
                        Ok(index) => index,
                        Err(_) => {
                            return evaluation_error(format!(
                                "determining policy id {text:?} is not a grant index"
                            ))
                        }
                    };
                    if index >= self.envelope.grants.len() {
                        return evaluation_error(format!(
                            "determining policy id {index} is outside the envelope's {} grants",
                            self.envelope.grants.len()
                        ));
                    }
                    lowest = Some(match lowest {
                        None => index,
                        Some(current) => current.min(index),
                    });
                }
                match lowest {
                    Some(grant) => Decision::Allow { grant },
                    // Cedar allowed without naming a policy: impossible for a
                    // set of `permit`s, and not something to allow on.
                    None => evaluation_error("allowed with no determining policy".to_string()),
                }
            }
        }
    }
}

/// A denial carrying an operator-facing diagnostic.
fn evaluation_error(detail: String) -> Decision {
    Decision::Deny(DenialReason::EvaluationError { detail })
}

impl Authorizer for CedarAuthorizer {
    /// Maintains **P1** (it answers what [`flavium_core::decide`] answers),
    /// **P2** (nothing allows by default) and **P3** (every failure denies).
    fn authorize(&self, principal: &Principal, call: &ToolCall, now: Timestamp) -> Decision {
        if *principal != self.envelope.principal {
            return Decision::Deny(DenialReason::NotGranted);
        }
        match self.envelope.tool_status(&call.tool, now) {
            ToolStatus::NotGranted => Decision::Deny(DenialReason::NotGranted),
            ToolStatus::Expired => Decision::Deny(DenialReason::Expired),
            ToolStatus::Live => match self.tools.get(call.tool.as_str()) {
                Some(tool) => self.ask_cedar(tool, call, now),
                // A live tool always has a UID: both come from the same
                // grants. Denying is the only safe answer if it ever does not.
                None => evaluation_error(format!("no compiled tool entity for {:?}", call.tool)),
            },
        }
    }

    /// Delegates to the core entirely, so the tool axis has exactly one
    /// implementation (**INV-3**).
    fn granted_tools(&self, principal: &Principal, now: Timestamp) -> BTreeSet<ToolName> {
        Authorizer::granted_tools(&self.envelope, principal, now)
    }
}

impl fmt::Debug for CedarAuthorizer {
    /// Shows the envelope and the compiled policy count; `PolicySet` has no
    /// stable `Debug` worth reprinting per call site.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CedarAuthorizer")
            .field("principal", &self.envelope.principal)
            .field("grants", &self.envelope.grants.len())
            .field("policies", &self.policies.policies().count())
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use cedar_policy::{Policy, PolicyId};
    use flavium_core::{ArgValue, Constraint, Grant};
    use serde_json::json;

    fn t(secs: i64) -> Timestamp {
        Timestamp::from_unix_secs(secs)
    }
    fn bot() -> Principal {
        Principal::new("bot").unwrap()
    }
    fn grant(tool: &str, constraints: &[(&str, Constraint)], expires: Option<i64>) -> Grant {
        Grant {
            tool: ToolName::new(tool).unwrap(),
            constraints: constraints
                .iter()
                .map(|(name, constraint)| (name.to_string(), constraint.clone()))
                .collect(),
            expires: expires.map(t),
        }
    }
    fn envelope(grants: Vec<Grant>) -> GrantEnvelope {
        GrantEnvelope {
            principal: bot(),
            grants,
        }
    }
    fn engine(grants: Vec<Grant>) -> CedarAuthorizer {
        CedarAuthorizer::new(envelope(grants)).unwrap()
    }
    fn call(tool: &str, args: &[(&str, ArgValue)]) -> ToolCall {
        ToolCall {
            tool: tool.to_string(),
            args: args
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect(),
        }
    }
    fn s(text: &str) -> ArgValue {
        ArgValue::Str(text.to_string())
    }

    #[test]
    fn is_send_and_sync_so_the_proxy_can_share_it() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CedarAuthorizer>();
        let shared: std::sync::Arc<dyn Authorizer> =
            std::sync::Arc::new(engine(vec![grant("read_file", &[], None)]));
        assert!(shared
            .authorize(&bot(), &call("read_file", &[]), t(0))
            .is_allow());
    }

    #[test]
    fn p2_an_empty_envelope_allows_nothing() {
        let engine = engine(vec![]);
        assert_eq!(engine.policies().policies().count(), 0);
        assert_eq!(
            engine.authorize(&bot(), &call("anything", &[]), t(0)),
            Decision::Deny(DenialReason::NotGranted)
        );
        assert!(Authorizer::granted_tools(&engine, &bot(), t(0)).is_empty());
    }

    #[test]
    fn p3_a_foreign_principal_holds_nothing() {
        let engine = engine(vec![grant("read_file", &[], None)]);
        let other = Principal::new("other").unwrap();
        assert_eq!(
            engine.authorize(&other, &call("read_file", &[]), t(0)),
            Decision::Deny(DenialReason::NotGranted)
        );
        assert!(Authorizer::granted_tools(&engine, &other, t(0)).is_empty());
    }

    #[test]
    fn p3_evaluation_errors_deny_even_when_cedar_allows() {
        // A hand-built policy that reads an attribute the context does not
        // have: Cedar reports an error, and the classifier must deny on it.
        // (Nothing `compile` emits can do this — that is what P5 buys — so
        // the classifier is exercised directly.)
        let engine = engine(vec![grant("read_file", &[], None)]);
        let broken = Policy::from_json(
            Some(PolicyId::new("0")),
            json!({
                "effect": "permit",
                "principal": {"op": "All"}, "action": {"op": "All"}, "resource": {"op": "All"},
                "conditions": [{"kind": "when", "body":
                    {".": {"left": {"Var": "context"}, "attr": "absent_key"}}}]
            }),
        )
        .unwrap();
        let mut policies = PolicySet::new();
        policies.add(broken).unwrap();
        let allowing = Policy::from_json(
            Some(PolicyId::new("1")),
            json!({
                "effect": "permit",
                "principal": {"op": "All"}, "action": {"op": "All"}, "resource": {"op": "All"},
                "conditions": [{"kind": "when", "body": {"Value": true}}]
            }),
        )
        .unwrap();
        policies.add(allowing).unwrap();

        let request = Request::new(
            engine.principal.clone(),
            engine.action.clone(),
            engine.tools.get("read_file").unwrap().clone(),
            request_context(&call("read_file", &[]), t(0)).unwrap(),
            None,
        )
        .unwrap();
        let response = engine
            .engine
            .is_authorized(&request, &policies, &engine.entities);
        assert_eq!(response.decision(), cedar_policy::Decision::Allow);
        match engine.classify(&response) {
            Decision::Deny(DenialReason::EvaluationError { detail }) => {
                assert!(detail.contains("absent_key"), "{detail}");
            }
            other => panic!("an evaluation error must deny, got {other:?}"),
        }
    }

    /// The sibling case: Cedar errors *and* reaches `Deny` on its own. Both
    /// paths deny, so only the reason distinguishes them — and the reason is
    /// what an operator acts on. `EvaluationError` means "the engine broke,
    /// look at it"; `OutOfEnvelope` means "the agent asked for something it
    /// does not have, which is the system working". Reporting the second when
    /// the first happened would hide an engine failure as routine policy, so
    /// the error check has to come before the decision, not after it.
    #[test]
    fn p3_an_evaluation_error_is_reported_as_such_even_when_cedar_denies() {
        let engine = engine(vec![grant("read_file", &[], None)]);
        let broken = Policy::from_json(
            Some(PolicyId::new("0")),
            json!({
                "effect": "permit",
                "principal": {"op": "All"}, "action": {"op": "All"}, "resource": {"op": "All"},
                "conditions": [{"kind": "when", "body":
                    {".": {"left": {"Var": "context"}, "attr": "absent_key"}}}]
            }),
        )
        .unwrap();
        let mut policies = PolicySet::new();
        policies.add(broken).unwrap();

        let request = Request::new(
            engine.principal.clone(),
            engine.action.clone(),
            engine.tools.get("read_file").unwrap().clone(),
            request_context(&call("read_file", &[]), t(0)).unwrap(),
            None,
        )
        .unwrap();
        let response = engine
            .engine
            .is_authorized(&request, &policies, &engine.entities);
        // The only policy errored, so Cedar itself answers Deny …
        assert_eq!(response.decision(), cedar_policy::Decision::Deny);
        // … and the classifier must still say *why*, not call it OutOfEnvelope.
        match engine.classify(&response) {
            Decision::Deny(DenialReason::EvaluationError { detail }) => {
                assert!(detail.contains("absent_key"), "{detail}");
            }
            other => panic!("expected EvaluationError, got {other:?}"),
        }
    }

    #[test]
    fn p3_an_unparseable_or_out_of_range_policy_id_denies() {
        let engine = engine(vec![grant("read_file", &[], None)]);
        for id in ["not-a-number", "1", "18446744073709551616"] {
            let policy = Policy::from_json(
                Some(PolicyId::new(id)),
                json!({
                    "effect": "permit",
                    "principal": {"op": "All"}, "action": {"op": "All"}, "resource": {"op": "All"},
                    "conditions": [{"kind": "when", "body": {"Value": true}}]
                }),
            )
            .unwrap();
            let mut policies = PolicySet::new();
            policies.add(policy).unwrap();
            let request = Request::new(
                engine.principal.clone(),
                engine.action.clone(),
                engine.tools.get("read_file").unwrap().clone(),
                request_context(&call("read_file", &[]), t(0)).unwrap(),
                None,
            )
            .unwrap();
            let response = engine
                .engine
                .is_authorized(&request, &policies, &engine.entities);
            assert_eq!(response.decision(), cedar_policy::Decision::Allow);
            assert!(
                matches!(
                    engine.classify(&response),
                    Decision::Deny(DenialReason::EvaluationError { .. })
                ),
                "policy id {id:?} must not produce an Allow"
            );
        }
    }

    #[test]
    fn allow_names_the_lowest_matching_grant() {
        let engine = engine(vec![
            grant(
                "read_file",
                &[("path", Constraint::Prefix("/a".into()))],
                None,
            ),
            grant("read_file", &[], None),
            grant(
                "read_file",
                &[("path", Constraint::Prefix("/a/b".into()))],
                None,
            ),
        ]);
        assert_eq!(
            engine.authorize(&bot(), &call("read_file", &[("path", s("/a/b/c"))]), t(0)),
            Decision::Allow { grant: 0 }
        );
        assert_eq!(
            engine.authorize(&bot(), &call("read_file", &[("path", s("/z"))]), t(0)),
            Decision::Allow { grant: 1 }
        );
    }

    /// Ten or more grants: the policy ids sort lexically inside Cedar
    /// (`"10" < "2"`), so a lexical minimum would answer 10 here.
    #[test]
    fn the_lowest_index_is_numeric_not_lexical() {
        let mut grants: Vec<Grant> = (0..12)
            .map(|_| {
                grant(
                    "read_file",
                    &[("path", Constraint::Prefix("/zz".into()))],
                    None,
                )
            })
            .collect();
        grants[2] = grant("read_file", &[], None);
        grants[10] = grant("read_file", &[], None);
        let engine = engine(grants);
        assert_eq!(
            engine.authorize(&bot(), &call("read_file", &[("path", s("/q"))]), t(0)),
            Decision::Allow { grant: 2 }
        );
    }

    #[test]
    fn debug_is_a_summary() {
        let engine = engine(vec![grant("a", &[], None), grant("b", &[], None)]);
        let text = format!("{engine:?}");
        assert!(text.contains("grants: 2"), "{text}");
        assert!(text.contains("policies: 2"), "{text}");
    }
}
