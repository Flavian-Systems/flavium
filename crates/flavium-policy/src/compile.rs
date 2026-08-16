//! The grant compiler: a [`GrantEnvelope`] becomes a Cedar [`PolicySet`].
//!
//! One grant compiles to exactly one Cedar `permit`, whose policy id is the
//! grant's index in the envelope. Nothing else is ever emitted — in
//! particular no `forbid`, because deny-by-default is structural here: a call
//! no permit covers is denied because nothing allowed it, not because a rule
//! said so. Index-as-id is what lets Cedar's answer be mapped straight back
//! to the grant that authorized the call
//! ([`Decision::Allow { grant }`](flavium_core::Decision::Allow)).
//!
//! # What a grant becomes
//!
//! The policy's scope pins the three axes that do not depend on the call:
//!
//! ```cedar
//! permit(
//!   principal == Flavium::Principal::"invoice-bot",
//!   action == Flavium::Action::"call",
//!   resource == Flavium::Tool::"read_file"
//! ) when { … };
//! ```
//!
//! and its `when` condition is the conjunction of one expression per
//! constraint plus, if the grant expires, one for the expiry:
//!
//! | Constraint | Cedar |
//! |---|---|
//! | `Prefix(p)` | `context.str has <arg> && context.str.<arg> like "<p>*"` |
//! | `Suffix(s)` | `context.str has <arg> && context.str.<arg> like "*<s>"` |
//! | `OneOf(set)` | `context.str has <arg> && […].contains(context.str.<arg>)` |
//! | `Range{min,max}` | `context.int has <arg>`, then `min <= context.int.<arg>` and/or `context.int.<arg> <= max` — an absent bound emits no comparison |
//! | `Absent` | `!(context.present.contains("<arg>"))` |
//! | expiry `Some(t)` | `context.now < t` |
//! | no constraints, no expiry | `true` |
//!
//! Every value-typed reference sits behind a `has` guard, which is what makes
//! a Cedar evaluation error unreachable: a missing argument fails its guard,
//! and an argument of the wrong type is in the other submap (or, for
//! [`ArgValue::Other`](flavium_core::ArgValue::Other), in neither), so it
//! fails the guard too. Both deny — exactly what the reference semantics do.
//!
//! # Everything is structured, nothing is text
//!
//! Policies are built as Cedar's JSON policy format (EST) and parsed with
//! `Policy::from_json`; entity UIDs are built with `EntityUid::from_json`. No
//! part of a grant is ever formatted into a string of Cedar source
//! (**P4**). Grant values are *data* — a path, an address pattern, a tool
//! name — and data must never be able to become syntax. Two consequences,
//! both verified against Cedar rather than assumed:
//!
//! - an entity id containing `"` or `\` survives intact through
//!   `from_json`, where `EntityUid::from_str` would fail outright;
//! - a `like` pattern is an array of literal and wildcard pieces, so Cedar
//!   escapes the literal itself: `Prefix("/a*b\\c")` compiles to
//!   `like "/a\*b\\c*"`, matches `/a*b\c/d`, and does **not** match
//!   `/aQQb\c/d`. A `*` inside a grant is a plain character.

use std::collections::BTreeMap;

use cedar_policy::{EntityUid, Policy, PolicyId, PolicySet};
use flavium_core::{Constraint, Grant, GrantEnvelope, Principal, Timestamp, ToolName};
use serde_json::{json, Map, Value};

use crate::context::{INT, NOW, PRESENT, STR};

/// The Cedar entity type of a flavium principal.
pub(crate) const PRINCIPAL_TYPE: &str = "Flavium::Principal";
/// The Cedar entity type of the one action flavium authorizes.
pub(crate) const ACTION_TYPE: &str = "Flavium::Action";
/// The id of that action: making a tool call.
pub(crate) const CALL_ACTION: &str = "call";
/// The Cedar entity type of a tool.
pub(crate) const TOOL_TYPE: &str = "Flavium::Tool";

/// Why an envelope could not be compiled into Cedar policies.
///
/// Every variant is a startup failure, never a request-path one (**D7**): a
/// grant that cannot be compiled stops the process while an operator is
/// watching, rather than surfacing mid-session as a denial that looks like
/// policy. None of these is reachable from a grant the core accepts — they
/// exist so that "unreachable" is a typed result rather than a panic.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompileError {
    /// A name could not be built into a Cedar entity UID.
    #[error("{role} name {name:?} is not usable as a Cedar entity id: {detail}")]
    EntityUid {
        /// Which axis the name came from (`principal`, `tool`, `action`).
        role: &'static str,
        /// The offending name.
        name: String,
        /// Cedar's diagnostic.
        detail: String,
    },
    /// The JSON built for a grant is not a valid Cedar policy.
    #[error("grant {index} did not compile to a valid Cedar policy: {detail}")]
    Policy {
        /// The grant's index in the envelope.
        index: usize,
        /// Cedar's diagnostic.
        detail: String,
    },
    /// The policy set refused a policy — a duplicate id, which the
    /// index-as-id scheme makes impossible.
    #[error("grant {index} was refused by the policy set: {detail}")]
    PolicySet {
        /// The grant's index in the envelope.
        index: usize,
        /// Cedar's diagnostic.
        detail: String,
    },
}

/// Compiles an envelope into the policy set that decides its calls.
///
/// One `permit` per grant, in envelope order, with the grant's index as its
/// policy id. Maintains **P2 (deny by default)** — an envelope with no grants
/// compiles to an empty policy set, which allows nothing — and **P4 (no
/// interpolation)**.
///
/// Runs once, at startup ([`CedarAuthorizer::new`](crate::CedarAuthorizer)),
/// not per call.
///
/// # Errors
///
/// [`CompileError`] if Cedar refuses a generated policy or an entity id. No
/// grant the core can construct is known to do this; the result type is how
/// "known" stays honest.
///
/// # Example
///
/// ```
/// use std::collections::BTreeMap;
/// use flavium_core::{Constraint, Grant, GrantEnvelope, Principal, ToolName};
/// use flavium_policy::compile;
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
///         expires: None,
///     }],
/// };
/// let policies = compile(&envelope)?;
/// assert_eq!(policies.policies().count(), 1);
/// # Ok(()) }
/// ```
pub fn compile(envelope: &GrantEnvelope) -> Result<PolicySet, CompileError> {
    let principal = principal_entity(&envelope.principal)?;
    let action = action_entity()?;

    let mut policies = PolicySet::new();
    for (index, grant) in envelope.grants.iter().enumerate() {
        let resource = tool_entity(&grant.tool)?;
        let json = policy_json(&principal, &action, &resource, grant);
        let policy = Policy::from_json(Some(policy_id(index)), json).map_err(|error| {
            CompileError::Policy {
                index,
                detail: error.to_string(),
            }
        })?;
        policies
            .add(policy)
            .map_err(|error| CompileError::PolicySet {
                index,
                detail: error.to_string(),
            })?;
    }
    Ok(policies)
}

/// The policy id of the grant at `index` — the index itself, as a string.
///
/// `PolicyId`'s `FromStr` cannot fail, so this is total; the round trip back
/// to an index is [`crate::authorizer`]'s job.
pub(crate) fn policy_id(index: usize) -> PolicyId {
    PolicyId::new(index.to_string())
}

/// The Cedar UID of a principal.
pub(crate) fn principal_uid(principal: &Principal) -> Result<EntityUid, CompileError> {
    entity_uid("principal", PRINCIPAL_TYPE, principal.as_str())
}

/// The Cedar UID of a tool.
pub(crate) fn tool_uid(tool: &ToolName) -> Result<EntityUid, CompileError> {
    entity_uid("tool", TOOL_TYPE, tool.as_str())
}

/// The Cedar UID of the one action flavium authorizes.
pub(crate) fn action_uid() -> Result<EntityUid, CompileError> {
    entity_uid("action", ACTION_TYPE, CALL_ACTION)
}

/// A `{"type": …, "id": …}` entity reference, built structurally so that an
/// id containing `"` or `\` is carried as data (**P4**).
fn entity_json(entity_type: &str, id: &str) -> Value {
    json!({"type": entity_type, "id": id})
}

fn entity_uid(role: &'static str, entity_type: &str, id: &str) -> Result<EntityUid, CompileError> {
    EntityUid::from_json(entity_json(entity_type, id)).map_err(|error| CompileError::EntityUid {
        role,
        name: id.to_string(),
        detail: error.to_string(),
    })
}

fn principal_entity(principal: &Principal) -> Result<Value, CompileError> {
    // Built once and reused for every policy; the `EntityUid` round trip is
    // what proves the name is usable before any policy embeds it.
    principal_uid(principal)?;
    Ok(entity_json(PRINCIPAL_TYPE, principal.as_str()))
}

fn tool_entity(tool: &ToolName) -> Result<Value, CompileError> {
    tool_uid(tool)?;
    Ok(entity_json(TOOL_TYPE, tool.as_str()))
}

fn action_entity() -> Result<Value, CompileError> {
    action_uid()?;
    Ok(entity_json(ACTION_TYPE, CALL_ACTION))
}

/// The EST of one grant's `permit`.
fn policy_json(principal: &Value, action: &Value, resource: &Value, grant: &Grant) -> Value {
    json!({
        "effect": "permit",
        "principal": {"op": "==", "entity": principal},
        "action": {"op": "==", "entity": action},
        "resource": {"op": "==", "entity": resource},
        "conditions": [{"kind": "when", "body": condition(grant)}]
    })
}

/// The grant's `when` body: every constraint, then the expiry.
fn condition(grant: &Grant) -> Value {
    let mut conjuncts: Vec<Value> = grant
        .constraints
        .iter()
        .map(|(argument, constraint)| constraint_expr(argument, constraint))
        .collect();
    if let Some(expires) = grant.expires {
        conjuncts.push(live_at(expires));
    }
    conjunction(conjuncts)
}

/// One constraint as a Cedar expression, guard included.
///
/// Each expression is self-contained: it evaluates to a `bool` for every
/// possible context, never to an error. That is what lets them be joined with
/// `&&` in any order — Cedar short-circuits, and a false conjunct simply
/// stops the evaluation.
fn constraint_expr(argument: &str, constraint: &Constraint) -> Value {
    match constraint {
        Constraint::Prefix(prefix) => conjunction(vec![
            has(STR, argument),
            like(
                submap_attr(STR, argument),
                vec![literal(prefix), wildcard()],
            ),
        ]),
        Constraint::Suffix(suffix) => conjunction(vec![
            has(STR, argument),
            like(
                submap_attr(STR, argument),
                vec![wildcard(), literal(suffix)],
            ),
        ]),
        Constraint::OneOf(members) => conjunction(vec![
            has(STR, argument),
            contains(
                set(members.iter().map(|member| value_string(member))),
                submap_attr(STR, argument),
            ),
        ]),
        Constraint::Range { min, max } => {
            let mut parts = vec![has(INT, argument)];
            if let Some(min) = min {
                parts.push(at_most(value_long(*min), submap_attr(INT, argument)));
            }
            if let Some(max) = max {
                parts.push(at_most(submap_attr(INT, argument), value_long(*max)));
            }
            conjunction(parts)
        }
        // No `has` guard: `present` is a list of names, and the question is
        // exactly whether this name is in it.
        Constraint::Absent => not(contains(context_attr(PRESENT), value_string(argument))),
    }
}

/// `context.now < expires` — the grant is live (**INV-3**: at `now ==
/// expires` it is already gone, so the comparison is strict).
fn live_at(expires: Timestamp) -> Value {
    json!({"<": {"left": context_attr(NOW), "right": value_long(expires.unix_secs())}})
}

// ---------------------------------------------------------------------------
// EST expression builders
//
// One function per Cedar node, so the JSON shapes appear exactly once each.
// ---------------------------------------------------------------------------

/// `&&` over the parts, split down the middle; an empty list is the literal
/// `true` (a grant with no constraints and no expiry admits every call on its
/// tool).
///
/// The split is what keeps the expression *shallow*: folding left instead
/// would make a grant with N constraints an N-deep spine of `&&`, and Cedar's
/// parse of that JSON is recursive — a grant with sixteen constrained
/// arguments, which is an ordinary tool signature, overflowed the stack and
/// aborted the process. Halving makes the depth `log2(N)`, so the failure mode
/// is gone rather than pushed further out.
///
/// Reassociating is sound because every conjunct is total: each evaluates to a
/// `bool` for every possible context and never to an error (see
/// [`constraint_expr`]), so no grouping can change the result — only which
/// conjuncts Cedar's short-circuit skips.
fn conjunction(mut parts: Vec<Value>) -> Value {
    match parts.len() {
        0 => json!({"Value": true}),
        1 => parts.remove(0),
        count => {
            let right = parts.split_off(count / 2);
            json!({"&&": {"left": conjunction(parts), "right": conjunction(right)}})
        }
    }
}

/// `context.<key>`
fn context_attr(key: &str) -> Value {
    json!({".": {"left": {"Var": "context"}, "attr": key}})
}

/// `context.<submap>.<argument>`
fn submap_attr(submap: &str, argument: &str) -> Value {
    json!({".": {"left": context_attr(submap), "attr": argument}})
}

/// `context.<submap> has <argument>`
fn has(submap: &str, argument: &str) -> Value {
    json!({"has": {"left": context_attr(submap), "attr": argument}})
}

/// `<subject> like <pattern>`
fn like(subject: Value, pattern: Vec<Value>) -> Value {
    json!({"like": {"left": subject, "pattern": pattern}})
}

/// A literal run of characters inside a `like` pattern. Cedar escapes it, so
/// a `*` or a `\` in a grant matches itself.
fn literal(text: &str) -> Value {
    json!({"Literal": text})
}

/// The `*` of a `like` pattern — the only wildcard flavium ever emits.
fn wildcard() -> Value {
    json!("Wildcard")
}

/// `<haystack>.contains(<needle>)`
fn contains(haystack: Value, needle: Value) -> Value {
    json!({"contains": {"left": haystack, "right": needle}})
}

/// `!<argument>`
fn not(argument: Value) -> Value {
    json!({"!": {"arg": argument}})
}

/// `[…]` — the EST spelling of a set literal.
fn set(members: impl Iterator<Item = Value>) -> Value {
    let mut object = Map::with_capacity(1);
    object.insert("Set".to_string(), Value::Array(members.collect()));
    Value::Object(object)
}

/// `<left> <= <right>`
fn at_most(left: Value, right: Value) -> Value {
    json!({"<=": {"left": left, "right": right}})
}

/// A string literal.
fn value_string(text: &str) -> Value {
    json!({"Value": text})
}

/// A `long` literal.
fn value_long(number: i64) -> Value {
    json!({"Value": number})
}

/// The tool UIDs of every tool the envelope grants, keyed by name — what
/// [`crate::CedarAuthorizer`] needs to build a request without touching the
/// client's tool string.
pub(crate) fn tool_uids(
    envelope: &GrantEnvelope,
) -> Result<BTreeMap<ToolName, EntityUid>, CompileError> {
    let mut uids = BTreeMap::new();
    for grant in &envelope.grants {
        if !uids.contains_key(&grant.tool) {
            uids.insert(grant.tool.clone(), tool_uid(&grant.tool)?);
        }
    }
    Ok(uids)
}
