//! The per-call Cedar request context: [`request_context`].
//!
//! Cedar policies read the call's arguments out of the request *context*, a
//! record flavium builds fresh for every call. Its shape is fixed — four
//! keys, always, whatever the call looks like:
//!
//! ```text
//! { str: {"path": "/data/x"}, int: {"n": 5}, present: ["path", "n"], now: 1700000000 }
//! ```
//!
//! - `str` — the call's string arguments by name.
//! - `int` — the call's `i64` arguments by name.
//! - `present` — the names of *every* argument the call supplied, including
//!   the ones in neither submap. This is the only way [`Constraint::Absent`]
//!   can be expressed: "no value here" is not something a value-typed lookup
//!   can say, so the set of supplied names is passed explicitly.
//! - `now` — the timestamp the decision is made at, as a Cedar `long`.
//!
//! [`ArgValue::Other`] — every JSON shape the core does not model — appears
//! in `present` and in neither submap, so a constraint's `has` guard fails
//! and the call is denied. That is what the reference semantics do too
//! ([`Constraint::admits`] never admits `Other`), which is the point: the two
//! implementations agree by construction instead of one erroring where the
//! other denies.
//!
//! # Why this is built with `RestrictedExpression`, not with JSON
//!
//! Argument names come from the client and are validated nowhere: an MCP tool
//! may take a parameter called anything at all. Cedar's *JSON* value parser
//! reserves three keys — `__expr`, `__entity` and `__extn` — as escapes, and
//! a single-key record spelled `{"__expr": "…"}` is not read as a record with
//! an oddly-named field but as a (removed) escape, which makes the whole
//! context fail to parse. Routing the context through that parser would hand
//! a client a way to turn one of its own tool's arguments into an engine
//! failure — a denial of a call the specification allows, reported to the
//! operator as "the engine broke".
//!
//! Building the context with [`RestrictedExpression`] instead skips the value
//! grammar altogether: names are carried as record keys verbatim, with no
//! vocabulary in which any of them means something. That is P4 (**no
//! interpolation**) applied one layer deeper than the compiler — not just
//! "no grant value is ever formatted into Cedar syntax", but "no name or
//! value is ever handed to a parser that could reinterpret it".
//!
//! [`Constraint::admits`]: flavium_core::Constraint::admits
//! [`Constraint::Absent`]: flavium_core::Constraint::Absent

use cedar_policy::{Context, RestrictedExpression};
use flavium_core::{ArgValue, Timestamp, ToolCall};

/// The context key holding the call's string arguments.
pub(crate) const STR: &str = "str";
/// The context key holding the call's integer arguments.
pub(crate) const INT: &str = "int";
/// The context key holding the names of every supplied argument.
pub(crate) const PRESENT: &str = "present";
/// The context key holding the decision time.
pub(crate) const NOW: &str = "now";

/// Why a call's request context could not be built.
///
/// Neither variant is reachable from a [`ToolCall`]: a call's arguments are a
/// map, so no submap can have a duplicate key, and the four context keys are
/// distinct literals. The type exists so that "unreachable" is a typed result
/// rather than a panic on the request path, and so that a future change which
/// makes it reachable denies (**P3**) instead of aborting.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    /// One of the two argument submaps could not be built as a record.
    #[error("the `{submap}` submap could not be built: {detail}")]
    Submap {
        /// Which submap: `str` or `int`.
        submap: &'static str,
        /// Cedar's diagnostic.
        detail: String,
    },
    /// The four keys could not be assembled into a context.
    #[error("the request context could not be built: {detail}")]
    Context {
        /// Cedar's diagnostic.
        detail: String,
    },
}

/// Builds the Cedar request context for one call at one time.
///
/// Maintains **P5 (total context)**: all four keys are emitted
/// unconditionally, so no generated policy can reference an attribute that is
/// not there. (A missing context key is the one way flavium's policies could
/// raise a Cedar evaluation error — verified: Cedar reports "record does not
/// have the attribute" — and emitting the keys always is what removes it.)
///
/// Maintains **P4 (no interpolation)**: every name and value is placed into a
/// [`RestrictedExpression`] directly. Nothing is formatted into text, and
/// nothing passes through a parser that could read a name as anything but a
/// name — see the module documentation for the `__expr` collision that rules
/// the JSON path out.
///
/// Pure and total: no clock, no I/O, no panics. The caller supplies `now`, so
/// the same call at the same time always builds the same context.
///
/// # Errors
///
/// [`ContextError`] if Cedar refuses the record or the context. Not reachable
/// from a `ToolCall` — its arguments are a map, so submap keys are unique —
/// and returned rather than unwrapped so that the request path cannot panic.
///
/// # Example
///
/// ```
/// use std::collections::BTreeMap;
/// use cedar_policy::EvalResult;
/// use flavium_core::{ArgValue, Timestamp, ToolCall};
/// use flavium_policy::request_context;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let call = ToolCall {
///     tool: "read_file".into(),
///     args: BTreeMap::from([
///         ("path".to_string(), ArgValue::Str("/data/x".into())),
///         ("depth".to_string(), ArgValue::Int(5)),
///         ("flags".to_string(), ArgValue::Other),
///     ]),
/// };
/// let context = request_context(&call, Timestamp::from_unix_secs(1_700_000_000))?;
///
/// let strings = match context.get("str") {
///     Some(EvalResult::Record(record)) => record,
///     other => panic!("expected a record, got {other:?}"),
/// };
/// assert_eq!(
///     strings.get("path"),
///     Some(&EvalResult::String("/data/x".to_string()))
/// );
/// // `Other` is in neither submap — so a constraint's `has` guard denies it …
/// assert!(strings.get("flags").is_none());
/// // … but it is still reported as supplied, so `Absent` denies it too.
/// assert!(matches!(context.get("present"), Some(EvalResult::Set(_))));
/// assert_eq!(context.get("now"), Some(EvalResult::Long(1_700_000_000)));
/// # Ok(()) }
/// ```
pub fn request_context(call: &ToolCall, now: Timestamp) -> Result<Context, ContextError> {
    let mut strings: Vec<(String, RestrictedExpression)> = Vec::new();
    let mut ints: Vec<(String, RestrictedExpression)> = Vec::new();
    let mut present: Vec<RestrictedExpression> = Vec::with_capacity(call.args.len());

    for (name, value) in &call.args {
        present.push(RestrictedExpression::new_string(name.clone()));
        match value {
            ArgValue::Str(text) => {
                strings.push((name.clone(), RestrictedExpression::new_string(text.clone())));
            }
            ArgValue::Int(number) => {
                ints.push((name.clone(), RestrictedExpression::new_long(*number)));
            }
            // Carried in `present` only: no constraint admits it, and the
            // `has` guards turn "not in a submap" into a denial.
            ArgValue::Other => {}
        }
    }

    let strings =
        RestrictedExpression::new_record(strings).map_err(|error| ContextError::Submap {
            submap: STR,
            detail: error.to_string(),
        })?;
    let ints = RestrictedExpression::new_record(ints).map_err(|error| ContextError::Submap {
        submap: INT,
        detail: error.to_string(),
    })?;

    Context::from_pairs([
        (STR.to_string(), strings),
        (INT.to_string(), ints),
        (PRESENT.to_string(), RestrictedExpression::new_set(present)),
        (
            NOW.to_string(),
            RestrictedExpression::new_long(now.unix_secs()),
        ),
    ])
    .map_err(|error| ContextError::Context {
        detail: error.to_string(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use cedar_policy::EvalResult;
    use std::collections::BTreeMap;

    fn call(args: &[(&str, ArgValue)]) -> ToolCall {
        ToolCall {
            tool: "t".into(),
            args: args
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        }
    }

    fn built(args: &[(&str, ArgValue)], now: i64) -> Context {
        request_context(&call(args), Timestamp::from_unix_secs(now)).unwrap()
    }

    #[track_caller]
    fn submap(context: &Context, key: &str) -> cedar_policy::Record {
        match context.get(key) {
            Some(EvalResult::Record(record)) => record,
            other => panic!("{key} is not a record: {other:?}"),
        }
    }

    #[track_caller]
    fn names(context: &Context, key: &str) -> Vec<String> {
        match context.get(key) {
            Some(EvalResult::Set(set)) => set
                .iter()
                .map(|value| match value {
                    EvalResult::String(text) => text.clone(),
                    other => panic!("{key} holds a non-string: {other:?}"),
                })
                .collect(),
            other => panic!("{key} is not a set: {other:?}"),
        }
    }

    #[test]
    fn p5_all_four_keys_are_always_present() {
        for args in [
            vec![],
            vec![("x", ArgValue::Other)],
            vec![("x", ArgValue::Str(String::new()))],
            vec![("x", ArgValue::Int(0))],
        ] {
            let context = built(&args, 0);
            for key in [STR, INT, PRESENT, NOW] {
                assert!(context.get(key).is_some(), "{key} missing from {context}");
            }
        }
    }

    #[test]
    fn empty_call_is_empty_submaps_and_an_empty_present_list() {
        let context = built(&[], -1);
        assert!(submap(&context, STR).is_empty());
        assert!(submap(&context, INT).is_empty());
        assert!(names(&context, PRESENT).is_empty());
        assert_eq!(context.get(NOW), Some(EvalResult::Long(-1)));
    }

    #[test]
    fn values_are_split_by_type_and_every_name_is_reported_present() {
        let context = built(
            &[
                ("z", ArgValue::Int(i64::MIN)),
                ("a", ArgValue::Str("s".into())),
                ("m", ArgValue::Other),
            ],
            i64::MAX,
        );
        let strings = submap(&context, STR);
        let ints = submap(&context, INT);
        assert_eq!(strings.get("a"), Some(&EvalResult::String("s".into())));
        assert_eq!(strings.len(), 1);
        assert_eq!(ints.get("z"), Some(&EvalResult::Long(i64::MIN)));
        assert_eq!(ints.len(), 1);
        let mut present = names(&context, PRESENT);
        present.sort();
        assert_eq!(present, vec!["a", "m", "z"]);
        assert_eq!(context.get(NOW), Some(EvalResult::Long(i64::MAX)));
    }

    /// Argument names are not validated anywhere in the core — they are map
    /// keys, and a key is data however it is spelled. In particular the three
    /// names Cedar's *JSON* value parser reserves as escapes must be ordinary
    /// names here, including as the sole argument of a call.
    #[test]
    fn reserved_and_hostile_names_are_ordinary_record_keys() {
        for name in [
            "__expr", "__entity", "__extn", "", "a\"b", "é", "\n", "context", "str", "now",
        ] {
            let context = built(&[(name, ArgValue::Str("v".into()))], 0);
            assert_eq!(
                submap(&context, STR).get(name),
                Some(&EvalResult::String("v".into())),
                "name {name:?} did not survive as a record key"
            );
            assert_eq!(names(&context, PRESENT), vec![name.to_string()]);
            // The four context keys are still exactly the four (**P5**): an
            // argument called `str` or `now` shadows nothing.
            for key in [STR, INT, PRESENT, NOW] {
                assert!(context.get(key).is_some(), "{key} lost for name {name:?}");
            }
            assert_eq!(context.get(NOW), Some(EvalResult::Long(0)));
        }
    }

    #[test]
    fn argument_name_collisions_across_types_cannot_happen() {
        // `args` is a map, so one name has one value and lands in one submap.
        let mut args = BTreeMap::new();
        args.insert("x".to_string(), ArgValue::Str("s".into()));
        args.insert("x".to_string(), ArgValue::Int(1));
        let context = request_context(
            &ToolCall {
                tool: "t".into(),
                args,
            },
            Timestamp::from_unix_secs(0),
        )
        .unwrap();
        assert!(submap(&context, STR).is_empty());
        assert_eq!(submap(&context, INT).get("x"), Some(&EvalResult::Long(1)));
        assert_eq!(names(&context, PRESENT), vec!["x".to_string()]);
    }

    /// A call with many arguments builds a context without recursing on the
    /// stack — the mirror of `compile`'s balanced conjunction.
    #[test]
    fn a_call_with_many_arguments_builds() {
        let args: Vec<(String, ArgValue)> = (0..2_000)
            .map(|i| (format!("a{i:05}"), ArgValue::Str(format!("v{i}"))))
            .collect();
        let context = request_context(
            &ToolCall {
                tool: "t".into(),
                args: args.into_iter().collect(),
            },
            Timestamp::from_unix_secs(0),
        )
        .unwrap();
        assert_eq!(submap(&context, STR).len(), 2_000);
    }
}
