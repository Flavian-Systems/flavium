//! Minimal typed view over a single JSON-RPC 2.0 message.
//!
//! The proxy parses every frame just deeply enough to classify it and to
//! observe the MCP handshake; `params`/`result`/`error` bodies stay
//! untouched raw JSON, and what gets forwarded is the original frame
//! bytes — never a re-serialization. Unknown fields and unknown methods
//! therefore pass through byte-faithfully.
//!
//! Anything that does not parse as a single well-formed JSON-RPC 2.0
//! object is a typed error and is never forwarded (fail closed).

use serde::Deserialize;
use serde_json::value::RawValue;

/// A JSON-RPC request id: an integer or a string.
///
/// MCP requires request ids to be integers or strings; fractional
/// numbers, booleans, and other JSON types are rejected at the boundary.
/// Integers outside the `i64` range are likewise rejected.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    /// An integer id.
    Number(i64),
    /// A string id.
    String(String),
}

/// The id of a response: a [`RequestId`], or `null` — which JSON-RPC
/// reserves for error replies to requests whose id could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseId {
    /// The id of the request being answered.
    Id(RequestId),
    /// `null`: the responder could not determine the request id.
    Null,
}

/// One parsed JSON-RPC message: classified, bodies untouched.
///
/// Borrows from the frame it was parsed from.
#[derive(Debug)]
pub enum Message<'a> {
    /// A request: has both `method` and `id`.
    Request {
        /// The request id.
        id: RequestId,
        /// The method name.
        method: String,
        /// The raw, untouched `params` value, if present.
        params: Option<&'a RawValue>,
    },
    /// A notification: has `method` but no `id`.
    Notification {
        /// The method name.
        method: String,
        /// The raw, untouched `params` value, if present.
        params: Option<&'a RawValue>,
    },
    /// A response: has `id` and exactly one of `result` / `error`.
    Response {
        /// The id of the request being answered.
        id: ResponseId,
        /// The raw, untouched `result` value, if present.
        result: Option<&'a RawValue>,
        /// The raw, untouched `error` value, if present.
        error: Option<&'a RawValue>,
    },
}

/// Errors at the envelope boundary.
#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    /// The frame is not valid UTF-8.
    #[error("frame is not valid UTF-8")]
    InvalidUtf8,

    /// The frame is not well-formed JSON.
    #[error("frame is not valid JSON")]
    InvalidJson(#[source] serde_json::Error),

    /// The frame is a JSON array. JSON-RPC batches were removed from MCP
    /// and are not supported.
    #[error("JSON-RPC batches are not supported")]
    Batch,

    /// The frame is valid JSON but not an object.
    #[error("frame is not a JSON object")]
    NotAnObject,

    /// The `jsonrpc` member is missing or is not exactly `"2.0"`.
    #[error("the \"jsonrpc\" member must be exactly \"2.0\"")]
    BadVersion,

    /// A request id was `null` or some type other than integer/string.
    #[error("request id must be an integer or a string")]
    BadId,

    /// The combination of `method`/`id`/`result`/`error` members does
    /// not form a request, a notification, or a response.
    #[error("frame is not a valid request, notification, or response")]
    BadShape,
}

impl EnvelopeError {
    /// Whether the peer-visible reply should be a JSON-RPC parse error
    /// (`-32700`) rather than an invalid-request error (`-32600`).
    pub fn is_parse_error(&self) -> bool {
        matches!(self, Self::InvalidUtf8 | Self::InvalidJson(_))
    }
}

/// The envelope members, captured raw so that field-level type errors
/// are classified by us, not by serde.
///
/// Every structural member preserves present-but-`null`: a plain
/// `Option<&RawValue>` would read `"id": null` as an *absent* id,
/// conflating an (illegal) null request id with a notification, a
/// (legal) null-id error response with a shapeless frame — and a
/// `"method": null` response-shaped frame would sail past the
/// method-must-not-coexist-with-result rule.
#[derive(Deserialize)]
struct Wire<'a> {
    #[serde(borrow)]
    jsonrpc: Option<&'a RawValue>,
    #[serde(borrow, default, deserialize_with = "present")]
    id: Option<&'a RawValue>,
    #[serde(borrow, default, deserialize_with = "present")]
    method: Option<&'a RawValue>,
    #[serde(borrow, default, deserialize_with = "present")]
    params: Option<&'a RawValue>,
    #[serde(borrow, default, deserialize_with = "present")]
    result: Option<&'a RawValue>,
    #[serde(borrow, default, deserialize_with = "present")]
    error: Option<&'a RawValue>,
}

/// Maps a member that is present — with any value, `null` included — to
/// `Some`; absence is handled by `#[serde(default)]`.
fn present<'de, D>(deserializer: D) -> Result<Option<&'de RawValue>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <&RawValue>::deserialize(deserializer).map(Some)
}

/// Parses one frame into a classified [`Message`].
///
/// This is the trust boundary every frame crosses in both directions.
/// It accepts exactly one well-formed JSON-RPC 2.0 object — a request,
/// a notification, or a response — and borrows `params`/`result`/`error`
/// as raw slices of `frame` so callers forward the original bytes, never
/// a re-serialization.
///
/// # Errors
///
/// Fail closed: anything else is an [`EnvelopeError`] and must not be
/// forwarded — invalid UTF-8 or JSON, a JSON array (batches are
/// unsupported), a non-object, a `jsonrpc` member other than `"2.0"`, a
/// request id that is not an integer or string, or a member combination
/// that is neither request, notification, nor response. Use
/// [`EnvelopeError::is_parse_error`] to pick the JSON-RPC error code
/// for the reply.
pub fn parse(frame: &[u8]) -> Result<Message<'_>, EnvelopeError> {
    let text = std::str::from_utf8(frame).map_err(|_| EnvelopeError::InvalidUtf8)?;
    // Arrays are rejected before the struct parse: serde would happily
    // read a JSON array *as* the `Wire` struct (structs deserialize
    // from sequences), which must not slip past the no-batches rule.
    if first_non_ws_byte(text) == Some(b'[') {
        return Err(match serde_json::from_str::<serde::de::IgnoredAny>(text) {
            Ok(_) => EnvelopeError::Batch,
            Err(err) => EnvelopeError::InvalidJson(err),
        });
    }
    let wire: Wire<'_> = match serde_json::from_str(text) {
        Ok(wire) => wire,
        Err(err) => return Err(classify_unparseable(text, err)),
    };

    let version_ok = wire.jsonrpc.is_some_and(
        |raw| matches!(serde_json::from_str::<String>(raw.get()), Ok(v) if v == "2.0"),
    );
    if !version_ok {
        return Err(EnvelopeError::BadVersion);
    }

    match (wire.method, wire.id, wire.result, wire.error) {
        (Some(method), Some(id), None, None) => Ok(Message::Request {
            id: parse_request_id(id)?,
            method: parse_method(method)?,
            params: wire.params,
        }),
        (Some(method), None, None, None) => Ok(Message::Notification {
            method: parse_method(method)?,
            params: wire.params,
        }),
        (None, Some(id), result, error) if result.is_some() != error.is_some() => {
            Ok(Message::Response {
                id: parse_response_id(id)?,
                result,
                error,
            })
        }
        _ => Err(EnvelopeError::BadShape),
    }
}

/// Classifies non-array input that failed to parse into [`Wire`]:
/// distinguishes malformed JSON from structurally wrong (non-object)
/// JSON so the peer gets the JSON-RPC-correct error code.
fn classify_unparseable(text: &str, err: serde_json::Error) -> EnvelopeError {
    if serde_json::from_str::<serde::de::IgnoredAny>(text).is_err() {
        return EnvelopeError::InvalidJson(err);
    }
    match first_non_ws_byte(text) {
        // A well-formed JSON object that Wire still refused (e.g.
        // duplicate members) is treated as malformed input.
        Some(b'{') => EnvelopeError::InvalidJson(err),
        _ => EnvelopeError::NotAnObject,
    }
}

/// The first byte of `text` that is not JSON insignificant whitespace
/// (`\n` cannot occur — frames are newline-delimited).
fn first_non_ws_byte(text: &str) -> Option<u8> {
    text.bytes().find(|b| !matches!(b, b' ' | b'\t' | b'\r'))
}

/// Ids deserialized strictly: `1.5` fails the integer arm, non-string
/// non-number types fail both.
#[derive(Deserialize)]
#[serde(untagged)]
enum IdWire {
    Number(i64),
    String(String),
}

fn parse_request_id(raw: &RawValue) -> Result<RequestId, EnvelopeError> {
    match serde_json::from_str::<IdWire>(raw.get()) {
        Ok(IdWire::Number(n)) => Ok(RequestId::Number(n)),
        Ok(IdWire::String(s)) => Ok(RequestId::String(s)),
        Err(_) => Err(EnvelopeError::BadId),
    }
}

fn parse_response_id(raw: &RawValue) -> Result<ResponseId, EnvelopeError> {
    match serde_json::from_str::<Option<IdWire>>(raw.get()) {
        Ok(None) => Ok(ResponseId::Null),
        Ok(Some(IdWire::Number(n))) => Ok(ResponseId::Id(RequestId::Number(n))),
        Ok(Some(IdWire::String(s))) => Ok(ResponseId::Id(RequestId::String(s))),
        Err(_) => Err(EnvelopeError::BadId),
    }
}

fn parse_method(raw: &RawValue) -> Result<String, EnvelopeError> {
    serde_json::from_str::<String>(raw.get()).map_err(|_| EnvelopeError::BadShape)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn classifies_request() {
        let msg =
            parse(br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"x":1}}"#).unwrap();
        match msg {
            Message::Request { id, method, params } => {
                assert_eq!(id, RequestId::Number(7));
                assert_eq!(method, "tools/call");
                assert_eq!(params.unwrap().get(), r#"{"x":1}"#);
            }
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn classifies_string_id_request() {
        let msg = parse(br#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#).unwrap();
        assert!(matches!(
            msg,
            Message::Request { id: RequestId::String(ref s), .. } if s == "abc"
        ));
    }

    #[test]
    fn classifies_notification() {
        let msg = parse(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(matches!(msg, Message::Notification { ref method, .. }
            if method == "notifications/initialized"));
    }

    #[test]
    fn classifies_result_and_error_responses() {
        let msg = parse(br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#).unwrap();
        assert!(matches!(
            msg,
            Message::Response {
                result: Some(_),
                error: None,
                ..
            }
        ));

        let msg =
            parse(br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"x"}}"#).unwrap();
        assert!(matches!(
            msg,
            Message::Response {
                id: ResponseId::Null,
                result: None,
                error: Some(_),
            }
        ));
    }

    #[test]
    fn params_bytes_are_preserved_exactly() {
        let frame =
            br#"{ "jsonrpc": "2.0", "id": 2, "method": "m", "params": { "a" :  [1,  2.5] } }"#;
        let msg = parse(frame).unwrap();
        match msg {
            Message::Request { params, .. } => {
                assert_eq!(params.unwrap().get(), r#"{ "a" :  [1,  2.5] }"#);
            }
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn unknown_top_level_fields_are_tolerated() {
        parse(br#"{"jsonrpc":"2.0","id":1,"method":"m","_meta":{"k":1},"future":true}"#).unwrap();
    }

    #[test]
    fn rejects_invalid_utf8() {
        let err = parse(b"\xff\xfe{}").unwrap_err();
        assert!(matches!(err, EnvelopeError::InvalidUtf8));
        assert!(err.is_parse_error());
    }

    #[test]
    fn rejects_malformed_json() {
        let err = parse(b"{not json").unwrap_err();
        assert!(matches!(err, EnvelopeError::InvalidJson(_)));
        assert!(err.is_parse_error());
    }

    #[test]
    fn rejects_batches() {
        for input in [
            &br#"[{"jsonrpc":"2.0","id":1,"method":"m"}]"#[..],
            b"[]",
            b" [1]",
            // serde can deserialize a struct from a JSON sequence;
            // arrays must classify as batches, never as an envelope.
            br#"["2.0", 1, "m", null, null, null]"#,
        ] {
            let err = parse(input).unwrap_err();
            assert!(matches!(err, EnvelopeError::Batch), "input {input:?}");
            assert!(!err.is_parse_error());
        }
        // A malformed array is still just malformed JSON.
        let err = parse(b"[not json").unwrap_err();
        assert!(matches!(err, EnvelopeError::InvalidJson(_)));
    }

    #[test]
    fn rejects_non_objects() {
        for input in [&b"42"[..], br#""hello""#, b"true", b"null"] {
            let err = parse(input).unwrap_err();
            assert!(matches!(err, EnvelopeError::NotAnObject), "input {input:?}");
        }
    }

    #[test]
    fn rejects_missing_or_wrong_version() {
        for input in [
            &br#"{"id":1,"method":"m"}"#[..],
            br#"{"jsonrpc":"1.0","id":1,"method":"m"}"#,
            br#"{"jsonrpc":2.0,"id":1,"method":"m"}"#,
        ] {
            let err = parse(input).unwrap_err();
            assert!(matches!(err, EnvelopeError::BadVersion), "input {input:?}");
        }
    }

    #[test]
    fn rejects_bad_request_ids() {
        for input in [
            &br#"{"jsonrpc":"2.0","id":null,"method":"m"}"#[..],
            br#"{"jsonrpc":"2.0","id":1.5,"method":"m"}"#,
            br#"{"jsonrpc":"2.0","id":true,"method":"m"}"#,
            br#"{"jsonrpc":"2.0","id":[1],"method":"m"}"#,
            br#"{"jsonrpc":"2.0","id":18446744073709551615,"method":"m"}"#,
        ] {
            let err = parse(input).unwrap_err();
            assert!(matches!(err, EnvelopeError::BadId), "input {input:?}");
        }
    }

    #[test]
    fn rejects_shapeless_frames() {
        for input in [
            // Neither method nor id.
            &br#"{"jsonrpc":"2.0"}"#[..],
            // Response with both result and error.
            br#"{"jsonrpc":"2.0","id":1,"result":{},"error":{}}"#,
            // Response with neither result nor error.
            br#"{"jsonrpc":"2.0","id":1}"#,
            // Request carrying a result.
            br#"{"jsonrpc":"2.0","id":1,"method":"m","result":{}}"#,
            // Non-string method.
            br#"{"jsonrpc":"2.0","id":1,"method":42}"#,
            // A present-but-null method must not turn a frame into a
            // well-formed response (or notification): presence rules
            // apply to the null spelling too.
            br#"{"jsonrpc":"2.0","id":1,"method":null,"result":{}}"#,
            br#"{"jsonrpc":"2.0","id":1,"method":null,"error":{}}"#,
            br#"{"jsonrpc":"2.0","id":1,"method":null}"#,
            br#"{"jsonrpc":"2.0","method":null}"#,
        ] {
            let err = parse(input).unwrap_err();
            assert!(
                matches!(err, EnvelopeError::BadShape),
                "input {input:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn null_result_is_a_legal_success_response() {
        let msg = parse(br#"{"jsonrpc":"2.0","id":1,"result":null}"#).unwrap();
        match msg {
            Message::Response {
                result: Some(result),
                error: None,
                ..
            } => assert_eq!(result.get(), "null"),
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[test]
    fn null_error_member_alongside_result_is_shapeless() {
        // Strict boundary: `"error": null` is a *present* error member,
        // which must not coexist with `result`.
        let err = parse(br#"{"jsonrpc":"2.0","id":1,"result":{},"error":null}"#).unwrap_err();
        assert!(matches!(err, EnvelopeError::BadShape));
    }

    #[test]
    fn duplicate_members_are_rejected_as_invalid_json() {
        let err = parse(br#"{"jsonrpc":"2.0","id":1,"id":2,"method":"m"}"#).unwrap_err();
        assert!(matches!(err, EnvelopeError::InvalidJson(_)));
    }
}
