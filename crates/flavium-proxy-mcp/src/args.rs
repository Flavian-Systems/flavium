//! `tools/call` params → the [`ToolCall`] the core decides about.
//!
//! The conversion rules are the core's ([`flavium_core::ToolCall`]), and
//! this module is where they are applied: a missing or `null` `arguments`
//! object is an empty map; JSON strings become [`ArgValue::Str`]; integers
//! that fit an `i64` become [`ArgValue::Int`]; everything else — floats,
//! `-0`, `1e3`, booleans, `null`, arrays, objects, integers past `i64` —
//! becomes [`ArgValue::Other`], which no constraint ever admits.
//!
//! Two refusals happen here rather than downstream, both fail-closed:
//!
//! - **`arguments` is not an object** (or `params` is not one, or `name`
//!   is missing or not a string). There is no call to decide about.
//! - **Duplicate keys**, in `params` or inside `arguments`. JSON parsers
//!   disagree about which value wins, the frame crosses the proxy
//!   byte-faithfully, and the upstream runs its own parser — so resolving
//!   the ambiguity either way is a guess about *someone else's* parser
//!   that could make the decision be about a value the upstream never
//!   sees. `serde_json` into a `BTreeMap` silently keeps the last one,
//!   which is exactly why the visitor below is hand-written.
//!
//! Every value is read as a [`RawValue`] first, so a malformed member can
//! never abort the walk before the tool name has been captured — a refused
//! call still names its tool in the trace.

use std::collections::{BTreeMap, BTreeSet};

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

use flavium_core::{ArgValue, ToolCall};

/// Why `tools/call` params were refused before any grant decision.
///
/// The client only ever sees `-32602 "Invalid params"`; `detail` is for
/// the operator's log, and `tool` reaches the trace when it could be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedParams {
    /// The requested tool name, when the params named one unambiguously.
    pub tool: Option<String>,
    /// What was wrong. Operator-facing; never sent to the client.
    pub detail: &'static str,
}

/// A well-formed `tools/call`: the tool name and the arguments as the
/// core models them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallParams {
    /// The requested tool name, exactly as sent.
    pub name: String,
    /// The arguments by name.
    pub args: BTreeMap<String, ArgValue>,
}

impl CallParams {
    /// The call as the core sees it, with no normalization applied.
    pub fn into_tool_call(self) -> ToolCall {
        ToolCall {
            tool: self.name,
            args: self.args,
        }
    }
}

/// Reads `tools/call` params.
///
/// # Errors
///
/// [`MalformedParams`] when the params are absent, not an object, carry a
/// duplicate key, have no string `name`, or have an `arguments` member
/// that is neither `null` nor an object (or that carries a duplicate key).
pub fn parse_call_params(params: Option<&str>) -> Result<CallParams, MalformedParams> {
    let Some(params) = params else {
        return Err(MalformedParams {
            tool: None,
            detail: "tools/call has no params",
        });
    };
    let wire: ParamsWire = match serde_json::from_str(params) {
        Ok(wire) => wire,
        Err(_) => {
            return Err(MalformedParams {
                tool: None,
                detail: "tools/call params are not an object",
            })
        }
    };
    if let Some(detail) = wire.problem {
        return Err(MalformedParams {
            tool: wire.name,
            detail,
        });
    }
    let Some(name) = wire.name else {
        return Err(MalformedParams {
            tool: None,
            detail: "tools/call params have no string `name`",
        });
    };
    Ok(CallParams {
        name,
        args: wire.args.unwrap_or_default(),
    })
}

/// The params object as walked: the tool name when it was readable, the
/// arguments when they were, and the first problem found.
///
/// Problems are carried rather than raised so the walk always finishes
/// and the tool name survives into the trace.
struct ParamsWire {
    name: Option<String>,
    args: Option<BTreeMap<String, ArgValue>>,
    problem: Option<&'static str>,
}

impl<'de> Deserialize<'de> for ParamsWire {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(ParamsVisitor)
    }
}

struct ParamsVisitor;

impl<'de> Visitor<'de> for ParamsVisitor {
    type Value = ParamsWire;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a tools/call params object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut wire = ParamsWire {
            name: None,
            args: None,
            problem: None,
        };
        let mut seen: BTreeSet<String> = BTreeSet::new();
        // Values are read as raw JSON so that no member's shape can abort
        // the walk; each is classified after the fact.
        while let Some((key, raw)) = map.next_entry::<String, &RawValue>()? {
            if !seen.insert(key.clone()) {
                wire.problem = wire
                    .problem
                    .or(Some("tools/call params have a duplicate key"));
                if key == "name" {
                    // Two `name` members: neither reading may be
                    // preferred, so the trace records no tool rather than
                    // the one that happened to come first.
                    wire.name = None;
                }
                continue;
            }
            match key.as_str() {
                "name" => match serde_json::from_str::<String>(raw.get()) {
                    Ok(name) => wire.name = Some(name),
                    Err(_) => {
                        wire.problem = wire.problem.or(Some("tools/call `name` is not a string"))
                    }
                },
                "arguments" => match parse_arguments(raw.get()) {
                    Ok(args) => wire.args = Some(args),
                    Err(detail) => wire.problem = wire.problem.or(Some(detail)),
                },
                // Everything else (`_meta`, members not yet invented)
                // crosses to the upstream untouched and is not read here.
                _ => {}
            }
        }
        Ok(wire)
    }
}

/// Reads the `arguments` member: `null` is an empty map, an object is
/// converted value by value, anything else is refused.
fn parse_arguments(raw: &str) -> Result<BTreeMap<String, ArgValue>, &'static str> {
    if raw.trim() == "null" {
        return Ok(BTreeMap::new());
    }
    let wire: ArgumentsWire =
        serde_json::from_str(raw).map_err(|_| "tools/call `arguments` is not an object")?;
    match wire.problem {
        Some(detail) => Err(detail),
        None => Ok(wire.args),
    }
}

struct ArgumentsWire {
    args: BTreeMap<String, ArgValue>,
    problem: Option<&'static str>,
}

impl<'de> Deserialize<'de> for ArgumentsWire {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(ArgumentsVisitor)
    }
}

struct ArgumentsVisitor;

impl<'de> Visitor<'de> for ArgumentsVisitor {
    type Value = ArgumentsWire;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a tools/call arguments object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut args = BTreeMap::new();
        let mut problem = None;
        while let Some((key, raw)) = map.next_entry::<String, &RawValue>()? {
            if args.insert(key, classify(raw.get())).is_some() {
                problem = problem.or(Some("tools/call `arguments` has a duplicate key"));
            }
        }
        Ok(ArgumentsWire { args, problem })
    }
}

/// One argument value, from its raw JSON text.
///
/// Only the two shapes a constraint can speak about are modelled. Numbers
/// are classified through `serde_json::Number`, which represents `-0` as a
/// float (it would otherwise lose the sign), so `-0` is `Other` — as are
/// `3.0`, `1e3`, and anything outside `i64`; `i64::MIN` and `i64::MAX`
/// survive exactly. Nested duplicate keys inside a value are not looked
/// for: every object and array is `Other` already, so no constraint's
/// answer could depend on which reading wins.
fn classify(raw: &str) -> ArgValue {
    let text = raw.trim();
    match text.as_bytes().first() {
        Some(b'"') => match serde_json::from_str::<String>(text) {
            Ok(s) => ArgValue::Str(s),
            // Unreachable: the text came from a parsed RawValue.
            Err(_) => ArgValue::Other,
        },
        Some(b'-') | Some(b'0'..=b'9') => match serde_json::from_str::<serde_json::Number>(text) {
            Ok(number) => match number.as_i64() {
                Some(n) => ArgValue::Int(n),
                None => ArgValue::Other,
            },
            Err(_) => ArgValue::Other,
        },
        _ => ArgValue::Other,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn args(params: &str) -> BTreeMap<String, ArgValue> {
        parse_call_params(Some(params)).unwrap().args
    }

    fn s(v: &str) -> ArgValue {
        ArgValue::Str(v.to_owned())
    }

    #[test]
    fn reads_name_and_arguments() {
        let parsed = parse_call_params(Some(
            r#"{"name": "read_file", "arguments": {"path": "/data/x", "n": 2}, "_meta": {"progressToken": "t"}}"#,
        ))
        .unwrap();
        assert_eq!(parsed.name, "read_file");
        assert_eq!(
            parsed.args,
            BTreeMap::from([
                ("path".to_owned(), s("/data/x")),
                ("n".to_owned(), ArgValue::Int(2)),
            ])
        );
    }

    #[test]
    fn missing_and_null_arguments_are_an_empty_map() {
        assert!(args(r#"{"name": "t"}"#).is_empty());
        assert!(args(r#"{"name": "t", "arguments": null}"#).is_empty());
        assert!(args(r#"{"name": "t", "arguments": {}}"#).is_empty());
    }

    /// The classification table the reference semantics rely on. Every
    /// row here was run before the M5 plan asserted it.
    #[test]
    fn argument_values_classify_exactly_as_the_core_models_them() {
        let rows: &[(&str, ArgValue)] = &[
            (r#""x""#, s("x")),
            (r#""""#, s("")),
            (r#""-""#, s("-")),
            ("0", ArgValue::Int(0)),
            ("-1", ArgValue::Int(-1)),
            ("9223372036854775807", ArgValue::Int(i64::MAX)),
            ("-9223372036854775808", ArgValue::Int(i64::MIN)),
            // The whole point of the table: these are *not* integers.
            ("-0", ArgValue::Other),
            ("3.0", ArgValue::Other),
            ("1e3", ArgValue::Other),
            ("1E2", ArgValue::Other),
            ("9223372036854775808", ArgValue::Other),
            ("18446744073709551616", ArgValue::Other),
            ("true", ArgValue::Other),
            ("false", ArgValue::Other),
            ("null", ArgValue::Other),
            ("[1]", ArgValue::Other),
            (r#"{"a": 1}"#, ArgValue::Other),
        ];
        for (json, expected) in rows {
            let params = format!(r#"{{"name": "t", "arguments": {{"v": {json}}}}}"#);
            assert_eq!(
                args(&params).get("v"),
                Some(expected),
                "argument value {json}"
            );
            assert_eq!(classify(json), *expected, "classify {json}");
        }
    }

    /// A duplicate key is a refusal, not a guess — and the companion
    /// assertion shows what the guess would have been.
    #[test]
    fn duplicate_keys_are_refused_rather_than_resolved() {
        let dup_args = r#"{"name": "read_file", "arguments": {"path": "/etc/passwd", "path": "/data/invoices/ok.pdf"}}"#;
        let err = parse_call_params(Some(dup_args)).unwrap_err();
        assert_eq!(err.tool.as_deref(), Some("read_file"));
        assert_eq!(err.detail, "tools/call `arguments` has a duplicate key");

        // What a plain deserialization would have decided on instead:
        // the *second* value, while the upstream's parser may take the
        // first. That divergence is why the visitor above is by hand.
        let silent: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(r#"{"path": "/etc/passwd", "path": "/data/invoices/ok.pdf"}"#)
                .unwrap();
        assert_eq!(silent["path"], "/data/invoices/ok.pdf");
        assert_eq!(silent.len(), 1);

        let dup_top = r#"{"name": "a", "name": "b", "arguments": {}}"#;
        let err = parse_call_params(Some(dup_top)).unwrap_err();
        assert_eq!(err.detail, "tools/call params have a duplicate key");
        assert_eq!(
            err.tool, None,
            "a duplicated `name` names nothing; recording the first reading would be a guess"
        );

        // A duplicate elsewhere still refuses, and there the tool *was*
        // stated unambiguously.
        let dup_meta = r#"{"name": "read_file", "_meta": {}, "_meta": {}}"#;
        let err = parse_call_params(Some(dup_meta)).unwrap_err();
        assert_eq!(err.tool.as_deref(), Some("read_file"));
    }

    #[test]
    fn malformed_shapes_are_refused_and_name_the_tool_when_they_can() {
        let cases: &[(&str, Option<&str>)] = &[
            (r#"{"name": "t", "arguments": 42}"#, Some("t")),
            (r#"{"name": "t", "arguments": []}"#, Some("t")),
            (r#"{"name": "t", "arguments": "path=/x"}"#, Some("t")),
            (r#"{"name": 7, "arguments": {}}"#, None),
            (r#"{"arguments": {}}"#, None),
            ("{}", None),
            ("42", None),
            ("[]", None),
            ("null", None),
            ("not json", None),
        ];
        for (params, tool) in cases {
            let err = parse_call_params(Some(params)).unwrap_err();
            assert_eq!(err.tool.as_deref(), *tool, "params {params}");
        }
        assert_eq!(
            parse_call_params(None).unwrap_err().detail,
            "tools/call has no params"
        );
    }

    #[test]
    fn a_tool_name_that_could_never_be_granted_still_parses() {
        // Validation lives on the grant side; a name like this simply
        // matches no grant and falls out as NotGranted.
        let parsed = parse_call_params(Some(r#"{"name": "read\nfile"}"#)).unwrap();
        assert_eq!(parsed.name, "read\nfile");
        let call = parsed.into_tool_call();
        assert_eq!(call.tool, "read\nfile");
        assert!(call.args.is_empty());
    }
}
