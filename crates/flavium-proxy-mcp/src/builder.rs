//! Construction of proxy-origin JSON-RPC frames.
//!
//! The M2 proxy answers some methods itself (`initialize`, `ping`,
//! `tools/list`), speaks to upstreams as a first-class MCP client, and
//! replies with errors it originates. Every such frame is built here.
//!
//! Two id spellings appear on purpose:
//! - **raw ids** (`id_raw`) are the exact bytes of an id captured from a
//!   parsed, validated frame — used when answering the peer that sent
//!   them, so its id round-trips byte-identically;
//! - **typed ids** ([`RequestId`]) are used where the proxy owns the id
//!   space (requests it mints) or where canonical re-encoding of a peer
//!   id is acceptable (error replies to malformed traffic).
//!
//! `result_raw`/`params_raw` arguments must be self-contained valid JSON;
//! callers only pass values captured from validated frames or serialized
//! with `serde_json`.

use crate::envelope::RequestId;

/// Fixed JSON-RPC error codes the proxy emits.
pub mod code {
    /// Parse error: the frame was not readable JSON-RPC at all.
    pub const PARSE_ERROR: i64 = -32700;
    /// Invalid request: readable but not a legal JSON-RPC frame, or an
    /// illegal request (duplicate in-flight id, re-initialize).
    pub const INVALID_REQUEST: i64 = -32600;
    /// Method not found.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid params: unknown tool, foreign cursor, malformed params.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal error: the upstream failed out from under a request.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// The client sent a request before the session was initialized.
    pub const SERVER_NOT_INITIALIZED: i64 = -32002;
}

/// A success response answering the raw id bytes of a validated request.
pub fn result_frame(id_raw: &str, result_raw: &str) -> Vec<u8> {
    format!(r#"{{"jsonrpc":"2.0","id":{id_raw},"result":{result_raw}}}"#).into_bytes()
}

/// An error response answering the raw id bytes of a validated request.
pub fn error_frame(id_raw: &str, code: i64, message: &str) -> Vec<u8> {
    let message = escape(message);
    format!(r#"{{"jsonrpc":"2.0","id":{id_raw},"error":{{"code":{code},"message":{message}}}}}"#)
        .into_bytes()
}

/// An error response with a typed id, canonically encoded.
pub fn error_frame_for(id: &RequestId, code: i64, message: &str) -> Vec<u8> {
    error_frame(&encode_id(id), code, message)
}

/// An error response with a `null` id, for frames whose id could not be
/// read at all.
pub fn error_frame_null_id(code: i64, message: &str) -> Vec<u8> {
    error_frame("null", code, message)
}

/// A request the proxy originates toward an upstream, in the proxy's
/// integer id space.
pub fn request_frame(id: u64, method: &str, params_raw: Option<&str>) -> Vec<u8> {
    let method = escape(method);
    match params_raw {
        Some(params) => {
            format!(r#"{{"jsonrpc":"2.0","id":{id},"method":{method},"params":{params}}}"#)
        }
        None => format!(r#"{{"jsonrpc":"2.0","id":{id},"method":{method}}}"#),
    }
    .into_bytes()
}

/// A notification the proxy originates.
pub fn notification_frame(method: &str, params_raw: Option<&str>) -> Vec<u8> {
    let method = escape(method);
    match params_raw {
        Some(params) => format!(r#"{{"jsonrpc":"2.0","method":{method},"params":{params}}}"#),
        None => format!(r#"{{"jsonrpc":"2.0","method":{method}}}"#),
    }
    .into_bytes()
}

/// The canonical JSON encoding of a typed request id.
pub fn encode_id(id: &RequestId) -> String {
    match id {
        RequestId::Number(n) => n.to_string(),
        RequestId::String(s) => escape(s),
    }
}

/// JSON-encodes a string. `serde_json::to_string` on a `&str` cannot
/// fail; the impossible error collapses to a plain (never-taken)
/// fallback rather than an `unwrap`.
fn escape(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::envelope::{self, Message};

    fn assert_parses(frame: &[u8]) {
        envelope::parse(frame).unwrap();
    }

    #[test]
    fn result_frame_round_trips_raw_id_bytes() {
        // Raw id bytes are spliced verbatim — an exotic escape spelling
        // the client chose survives into the response untouched.
        let id_raw = "\"call\\u002d1\"";
        let frame = result_frame(id_raw, r#"{"tools":[]}"#);
        let expected = format!(r#"{{"jsonrpc":"2.0","id":{id_raw},"result":{{"tools":[]}}}}"#);
        assert_eq!(frame, expected.into_bytes());
        assert_parses(&frame);
        // Semantically the escaped spelling is still the same id.
        let parsed: serde_json::Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(parsed["id"], "call-1");
    }

    #[test]
    fn error_frames_escape_messages() {
        let frame = error_frame("7", code::INVALID_PARAMS, "bad \"name\"\n");
        let parsed: serde_json::Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(parsed["error"]["code"], -32602);
        assert_eq!(parsed["error"]["message"], "bad \"name\"\n");
        assert_eq!(parsed["id"], 7);
        assert_parses(&frame);
    }

    #[test]
    fn typed_ids_encode_canonically() {
        assert_eq!(encode_id(&RequestId::Number(-3)), "-3");
        assert_eq!(encode_id(&RequestId::String("a\"b".into())), r#""a\"b""#);
        let frame = error_frame_for(
            &RequestId::String("x".into()),
            code::METHOD_NOT_FOUND,
            "Method not found",
        );
        assert_parses(&frame);
    }

    #[test]
    fn request_and_notification_frames_parse_as_their_kind() {
        let req = request_frame(0, "initialize", Some(r#"{"protocolVersion":"2025-11-25"}"#));
        match envelope::parse(&req).unwrap() {
            Message::Request { id, method, params } => {
                assert_eq!(id, RequestId::Number(0));
                assert_eq!(method, "initialize");
                assert_eq!(params.unwrap().get(), r#"{"protocolVersion":"2025-11-25"}"#);
            }
            other => panic!("expected request, got {other:?}"),
        }

        let note = notification_frame("notifications/tools/list_changed", None);
        assert!(matches!(
            envelope::parse(&note).unwrap(),
            Message::Notification { .. }
        ));
        let bare = request_frame(1, "tools/list", None);
        assert_parses(&bare);
    }

    #[test]
    fn null_id_error_frame_is_a_legal_response() {
        let frame = error_frame_null_id(code::PARSE_ERROR, "Parse error");
        match envelope::parse(&frame).unwrap() {
            Message::Response { id, error, .. } => {
                assert!(matches!(id, envelope::ResponseId::Null));
                assert!(error.is_some());
            }
            other => panic!("expected response, got {other:?}"),
        }
    }
}
