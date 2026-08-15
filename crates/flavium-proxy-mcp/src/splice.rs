//! Surgical rewriting of single members inside JSON objects.
//!
//! The M2 proxy terminates the MCP protocol: request ids are translated
//! between the client's id space and each upstream's, and cancellation
//! notifications have their `requestId` rewritten. Everything else must
//! keep flowing byte-faithfully — so instead of re-serializing frames
//! (which would reformat numbers and drop unknown members), this module
//! rebuilds an object from its original member *value* bytes, replacing
//! exactly one member's value.
//!
//! What is and is not preserved:
//! - member **values** are preserved byte-for-byte (they are captured as
//!   [`RawValue`] slices of the input);
//! - member **order** is preserved;
//! - unknown members are preserved;
//! - member **keys** are re-encoded canonically and inter-member
//!   whitespace is normalized — the T1 acceptance criterion is
//!   body-level (`params`/`result`) byte identity, which key re-encoding
//!   cannot affect.
//!
//! Objects with duplicate member names are rejected (fail closed): a
//! frame that says `"id"` twice has no unambiguous rewrite.

use std::fmt::Write as _;

use serde::de::{MapAccess, Visitor};
use serde::Deserialize;
use serde_json::value::RawValue;

/// Errors from [`rewrite_member`].
#[derive(Debug, thiserror::Error)]
pub enum SpliceError {
    /// The input is not a JSON object.
    #[error("input is not a JSON object")]
    NotAnObject,

    /// The object contains the same member name twice; there is no
    /// unambiguous rewrite, so the input is rejected.
    #[error("object has a duplicate member {0:?}")]
    DuplicateMember(String),

    /// The member to rewrite is not present in the object.
    #[error("object has no member {0:?}")]
    MissingMember(&'static str),
}

/// The members of a JSON object in source order, values as raw slices
/// of the input.
struct Members<'a>(Vec<(String, &'a RawValue)>);

impl<'de> Deserialize<'de> for Members<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct MembersVisitor;

        impl<'de> Visitor<'de> for MembersVisitor {
            type Value = Members<'de>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut members = Vec::new();
                while let Some(entry) = map.next_entry::<String, &'de RawValue>()? {
                    members.push(entry);
                }
                Ok(Members(members))
            }
        }

        deserializer.deserialize_map(MembersVisitor)
    }
}

/// Rebuilds `object` with `member`'s value replaced by `new_value`,
/// preserving every other member's value bytes and the member order.
///
/// `new_value` must be a self-contained valid JSON value; callers in
/// this crate only pass values they minted (integer ids) or values
/// captured from already-validated frames.
pub fn rewrite_member(
    object: &str,
    member: &'static str,
    new_value: &str,
) -> Result<String, SpliceError> {
    let Members(members) =
        serde_json::from_str::<Members<'_>>(object).map_err(|_| SpliceError::NotAnObject)?;

    let mut seen = std::collections::HashSet::with_capacity(members.len());
    for (name, _) in &members {
        if !seen.insert(name.as_str()) {
            return Err(SpliceError::DuplicateMember(name.clone()));
        }
    }
    if !members.iter().any(|(name, _)| name == member) {
        return Err(SpliceError::MissingMember(member));
    }

    let mut out = String::with_capacity(object.len() + new_value.len());
    out.push('{');
    for (i, (name, value)) in members.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        // Encoding a string key cannot fail; write_str on a String
        // cannot fail either. The fallible signatures are absorbed here
        // rather than surfaced as impossible error variants.
        let _ = write!(out, "{}", EscapedKey(name));
        out.push(':');
        if name == member {
            out.push_str(new_value);
        } else {
            out.push_str(value.get());
        }
    }
    out.push('}');
    Ok(out)
}

/// The exact value bytes of one member of a JSON object, or `None` if
/// absent. Same strictness as [`rewrite_member`]: non-objects and
/// objects with any duplicate member are refused.
pub fn member_value(object: &str, member: &str) -> Result<Option<String>, SpliceError> {
    let Members(members) =
        serde_json::from_str::<Members<'_>>(object).map_err(|_| SpliceError::NotAnObject)?;
    let mut seen = std::collections::HashSet::with_capacity(members.len());
    for (name, _) in &members {
        if !seen.insert(name.as_str()) {
            return Err(SpliceError::DuplicateMember(name.clone()));
        }
    }
    Ok(members
        .iter()
        .find(|(name, _)| name == member)
        .map(|(_, value)| value.get().to_owned()))
}

/// A JSON string encoding of a member key, written without allocating.
struct EscapedKey<'a>(&'a str);

impl std::fmt::Display for EscapedKey<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("\"")?;
        for c in self.0.chars() {
            match c {
                '"' => f.write_str("\\\"")?,
                '\\' => f.write_str("\\\\")?,
                '\n' => f.write_str("\\n")?,
                '\r' => f.write_str("\\r")?,
                '\t' => f.write_str("\\t")?,
                c if (c as u32) < 0x20 => write!(f, "\\u{:04x}", c as u32)?,
                c => f.write_char(c)?,
            }
        }
        f.write_str("\"")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_id_preserving_other_member_bytes_and_order() {
        let frame = r#"{ "jsonrpc": "2.0", "id": "call-1", "method": "tools/call", "params": { "a" :  [1,  2.5] }, "_meta": {"trace": 1e3}, "future": null }"#;
        let out = rewrite_member(frame, "id", "42").unwrap();
        assert_eq!(
            out,
            r#"{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{ "a" :  [1,  2.5] },"_meta":{"trace": 1e3},"future":null}"#
        );
    }

    #[test]
    fn rewrites_request_id_inside_cancel_params() {
        let params = r#"{"requestId": 7, "reason": "user asked", "x": [true]}"#;
        let out = rewrite_member(params, "requestId", "913").unwrap();
        assert_eq!(out, r#"{"requestId":913,"reason":"user asked","x":[true]}"#);
    }

    #[test]
    fn preserves_number_formatting_in_untouched_members() {
        let obj = r#"{"id":1,"k":[1e2, 0.5000, -0]}"#;
        let out = rewrite_member(obj, "id", "\"x\"").unwrap();
        assert_eq!(out, r#"{"id":"x","k":[1e2, 0.5000, -0]}"#);
    }

    #[test]
    fn rejects_non_objects_and_invalid_json() {
        for input in ["[1,2]", "42", "\"s\"", "null", "{broken"] {
            assert!(matches!(
                rewrite_member(input, "id", "1"),
                Err(SpliceError::NotAnObject)
            ));
        }
    }

    #[test]
    fn rejects_duplicate_members_even_unmodeled_ones() {
        let err = rewrite_member(r#"{"id":1,"x":1,"x":2}"#, "id", "2").unwrap_err();
        assert!(matches!(err, SpliceError::DuplicateMember(name) if name == "x"));
        let err = rewrite_member(r#"{"id":1,"id":2}"#, "id", "3").unwrap_err();
        assert!(matches!(err, SpliceError::DuplicateMember(name) if name == "id"));
    }

    #[test]
    fn missing_member_is_an_error() {
        let err = rewrite_member(r#"{"id":1}"#, "requestId", "2").unwrap_err();
        assert!(matches!(err, SpliceError::MissingMember("requestId")));
    }

    #[test]
    fn keys_with_escapes_are_reencoded_semantically() {
        // Exotic key spellings are decoded by the parse and re-encoded
        // canonically; the member value bytes still round-trip.
        let obj = "{\"a\\u0041\": {\"deep\": 1e1}, \"id\": 1}";
        let out = rewrite_member(obj, "id", "2").unwrap();
        assert_eq!(out, r#"{"aA":{"deep": 1e1},"id":2}"#);
    }

    #[test]
    fn control_characters_in_keys_reencode_safely() {
        let obj = "{\"k\\u0001\\\"\\\\\\n\": 1, \"id\": 3}";
        let out = rewrite_member(obj, "id", "4").unwrap();
        assert_eq!(out, "{\"k\\u0001\\\"\\\\\\n\":1,\"id\":4}");
        // The rewritten object must still parse.
        serde_json::from_str::<serde_json::Value>(&out).unwrap();
    }

    #[test]
    fn member_value_returns_exact_bytes() {
        let obj = r#"{ "id": "call-1", "params": { "a":  1e2 } }"#;
        assert_eq!(
            member_value(obj, "id").unwrap().as_deref(),
            Some(r#""call-1""#),
            "member_value must return the id's exact source bytes"
        );
        assert_eq!(
            member_value(obj, "params").unwrap().as_deref(),
            Some("{ \"a\":  1e2 }")
        );
        assert_eq!(member_value(obj, "missing").unwrap(), None);
        assert!(matches!(
            member_value(r#"{"a":1,"a":2}"#, "a"),
            Err(SpliceError::DuplicateMember(_))
        ));
        assert!(matches!(
            member_value("[1]", "a"),
            Err(SpliceError::NotAnObject)
        ));
    }

    #[test]
    fn unicode_keys_round_trip() {
        let obj = "{\"caf\u{e9}\": \"\u{2603}\", \"id\": 1}";
        let out = rewrite_member(obj, "id", "9").unwrap();
        assert_eq!(out, "{\"caf\u{e9}\":\"\u{2603}\",\"id\":9}");
    }
}
