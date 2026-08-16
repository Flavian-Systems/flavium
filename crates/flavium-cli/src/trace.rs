//! The JSONL trace sink: one JSON object per line, appended, flushed per
//! event.
//!
//! The trace has to answer *why was this allowed?*, which needs the
//! values the decision was made on — not the ones on the wire. So
//! [`TraceEvent::CallDecided`] records the call **as evaluated**:
//! normalized (M5's D4), with unmodelled values as a type tag carrying no
//! payload. Replaying a decision needs exactly envelope + call + `now`,
//! and all three are here.
//!
//! It also has to answer *what did the agent ask for?*, and the evaluated
//! form cannot: normalization is lossy, so `…/a/../b` and `…/b`, or two
//! case spellings of one Windows path, are recorded identically while
//! being different requests. So a line carries `args_as_sent` beside the
//! evaluated `args` — the caller's own spelling, for the arguments where
//! the two differ and only those. No key means normalization changed
//! nothing.
//!
//! # The shape of a line
//!
//! Every line carries `v`, a monotonic `seq`, a wall-clock `ts` in Unix
//! milliseconds, and a session id — the four things the core deliberately
//! leaves to the recorder, since an event itself is clock-free. Then
//! `event`, the variant's name, and its fields:
//!
//! ```jsonl
//! {"v":1,"seq":1,"ts":1755300000123,"session":"1755300000-4242","event":"session_started","principal":"invoice-bot","grants":[…]}
//! {"v":1,"seq":4,"ts":1755300001456,"session":"1755300000-4242","event":"call_decided","principal":"invoice-bot","call_id":0,"now":1755300001,"tool":"read_file","args":{"path":{"kind":"str","value":"/etc/passwd"}},"decision":{"kind":"deny","reason":"out_of_envelope"}}
//! ```
//!
//! `"v": 1` because T4 publishes this as a versioned specification and
//! will change it; until then the format is **unstable** and nothing
//! should parse it as a contract.
//!
//! # Two deliberate limits
//!
//! - **The file is created `0600` on unix.** It now contains the
//!   arguments of every call. Note *created*: a mode applies at creation,
//!   so an existing file keeps whatever permissions it already had —
//!   point `--trace` at a fresh path, or at a directory that is already
//!   protected.
//! - **A string argument longer than [`VALUE_CAP`] is truncated**, and
//!   then — and only then — carries its full byte length and the SHA-256
//!   of the whole value. An argument can be a megabyte of document text,
//!   and an audit log must not become a copy of the data plane. Below the
//!   cap the plaintext is already there, so a digest would be noise, and
//!   worse than noise for privacy: a short low-entropy value (an address,
//!   an identifier, a token) is enumerable from its hash while a kilobyte
//!   of text is not. The rule that keeps the log small and the rule that
//!   leaks least are the same rule.
//!
//!   Truncation is a *sink* concern only. The decision was made on the
//!   complete value, never on the prefix.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use flavium_core::{
    ArgValue, CallOutcome, Constraint, Decision, DenialReason, DiscardKind, Grant, GrantEnvelope,
    NotForwardedReason, RefusalReason, SessionEndReason, SinkError, ToolCall, TraceEvent,
    TraceSink,
};
use sha2::{Digest, Sha256};

/// The format version every line carries.
pub const FORMAT_VERSION: u32 = 1;

/// Longest string value recorded whole, in bytes.
///
/// 4 KiB is `PATH_MAX`: the longest path Linux can express is 4096 bytes
/// including its terminator, so the cap is a boundary with a meaning
/// rather than a taste — **a path argument is never truncated**. "Which
/// file did it read?" is the question this record exists to answer, and a
/// digest does not answer it. It also closes a small hole: below
/// `PATH_MAX`, an attacker who learned where the cap sat could pad a path
/// until the interesting tail fell off the record. Authority is
/// unaffected either way, but the audit line would be deliberately less
/// legible.
///
/// Windows long paths (`\\?\`, up to ~32 K UTF-16 units) can still exceed
/// it and will truncate; the length and digest still identify them.
pub const VALUE_CAP: usize = 4096;

/// A sink that appends one JSON object per line.
#[derive(Debug)]
pub struct JsonlSink {
    /// `<start-secs>-<pid>`: enough to tell two runs apart in one file.
    session: String,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    file: File,
    seq: u64,
    /// Once a write fails the session is ending anyway; refusing every
    /// later event keeps the file from gaining a second half-written line
    /// after the first.
    failed: bool,
}

impl JsonlSink {
    /// Opens (or creates) the trace file for appending.
    ///
    /// Called at startup so an unusable path fails while an operator is
    /// watching, rather than mid-session as a denial. On unix a file this
    /// call *creates* is `0600`; one that already exists keeps its own
    /// permissions, because a mode is a creation argument and silently
    /// tightening someone else's file would be its own surprise.
    ///
    /// # Errors
    ///
    /// The underlying I/O error.
    pub fn create(path: &Path) -> std::io::Result<Self> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        Ok(Self {
            session: session_id(),
            state: Mutex::new(State {
                file,
                seq: 0,
                failed: false,
            }),
        })
    }

    /// The session identifier every line of this run carries.
    pub fn session_id(&self) -> &str {
        &self.session
    }
}

impl TraceSink for JsonlSink {
    fn record(&self, event: &TraceEvent) -> Result<(), SinkError> {
        // A poisoned lock cannot have left the file handle inconsistent:
        // every write is one complete line and nothing panics while
        // holding the guard.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.failed {
            return Err(Box::<dyn std::error::Error + Send + Sync>::from(
                "trace sink already failed",
            ));
        }
        state.seq += 1;
        let mut line = String::with_capacity(256);
        line.push_str(r#"{"v":"#);
        line.push_str(&FORMAT_VERSION.to_string());
        line.push_str(r#","seq":"#);
        line.push_str(&state.seq.to_string());
        line.push_str(r#","ts":"#);
        line.push_str(&unix_millis().to_string());
        line.push_str(r#","session":"#);
        write_str(&mut line, &self.session);
        line.push(',');
        write_event(&mut line, event);
        line.push_str("}\n");

        match state
            .file
            .write_all(line.as_bytes())
            .and_then(|()| state.file.flush())
        {
            Ok(()) => Ok(()),
            Err(err) => {
                state.failed = true;
                Err(Box::new(err))
            }
        }
    }
}

/// `<start-secs>-<pid>`.
fn session_id() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    format!("{secs}-{}", std::process::id())
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0)
}

// ---- the event encodings ------------------------------------------------
//
// Hand-written, so that `flavium-core` stays serde-free: the verification
// target should carry the vocabulary and nothing else. Adding a
// `TraceEvent` variant breaks this match at compile time, which is why the
// enum is deliberately not `#[non_exhaustive]`.

fn write_event(out: &mut String, event: &TraceEvent) {
    match event {
        TraceEvent::SessionStarted { envelope } => {
            out.push_str(r#""event":"session_started","#);
            write_envelope(out, envelope);
        }
        TraceEvent::HandshakeCompleted {
            offered_protocol_version,
            protocol_version,
            client_name,
            client_version,
        } => {
            out.push_str(r#""event":"handshake_completed","offered_protocol_version":"#);
            write_capped(out, offered_protocol_version);
            out.push_str(r#","protocol_version":"#);
            write_capped(out, protocol_version);
            out.push_str(r#","client_name":"#);
            write_optional(out, client_name.as_deref());
            out.push_str(r#","client_version":"#);
            write_optional(out, client_version.as_deref());
        }
        TraceEvent::ToolsListed {
            principal,
            now,
            offered,
            granted,
        } => {
            out.push_str(r#""event":"tools_listed","principal":"#);
            write_str(out, principal.as_str());
            out.push_str(r#","now":"#);
            out.push_str(&now.unix_secs().to_string());
            out.push_str(r#","offered":"#);
            out.push_str(&offered.to_string());
            out.push_str(r#","granted":"#);
            out.push_str(&granted.to_string());
        }
        TraceEvent::CallRefused {
            principal,
            call_id,
            tool,
            reason,
        } => {
            out.push_str(r#""event":"call_refused","principal":"#);
            write_str(out, principal.as_str());
            out.push_str(r#","call_id":"#);
            out.push_str(&call_id.0.to_string());
            out.push_str(r#","tool":"#);
            match tool {
                None => out.push_str("null"),
                Some(tool) => write_capped(out, tool),
            }
            out.push_str(r#","reason":"#);
            write_str(
                out,
                match reason {
                    RefusalReason::MalformedParams => "malformed_params",
                    RefusalReason::UnknownTool => "unknown_tool",
                    RefusalReason::DuplicateRequestId => "duplicate_request_id",
                },
            );
        }
        TraceEvent::CallDecided {
            principal,
            call_id,
            call,
            args_as_sent,
            now,
            decision,
        } => {
            out.push_str(r#""event":"call_decided","principal":"#);
            write_str(out, principal.as_str());
            out.push_str(r#","call_id":"#);
            out.push_str(&call_id.0.to_string());
            out.push_str(r#","now":"#);
            out.push_str(&now.unix_secs().to_string());
            out.push(',');
            write_call(out, call);
            // Absent, not empty, when normalization changed nothing: the
            // key's presence is the signal that it did.
            if !args_as_sent.is_empty() {
                out.push_str(r#","args_as_sent":{"#);
                for (index, (argument, sent)) in args_as_sent.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_str(out, argument);
                    out.push(':');
                    write_capped(out, sent);
                }
                out.push('}');
            }
            out.push_str(r#","decision":"#);
            write_decision(out, decision);
        }
        TraceEvent::CallCompleted {
            principal,
            call_id,
            outcome,
        } => {
            out.push_str(r#""event":"call_completed","principal":"#);
            write_str(out, principal.as_str());
            out.push_str(r#","call_id":"#);
            out.push_str(&call_id.0.to_string());
            out.push_str(r#","outcome":"#);
            write_outcome(out, outcome);
        }
        TraceEvent::FrameRejected { code } => {
            out.push_str(r#""event":"frame_rejected","code":"#);
            out.push_str(&code.to_string());
        }
        TraceEvent::FrameDiscarded { kind } => {
            out.push_str(r#""event":"frame_discarded","kind":"#);
            write_str(
                out,
                match kind {
                    DiscardKind::StrayResponse => "stray_response",
                    DiscardKind::UnroutableNotification => "unroutable_notification",
                    DiscardKind::StaleResponse => "stale_response",
                    DiscardKind::CancelUnreadable => "cancel_unreadable",
                    DiscardKind::CancelNotInFlight => "cancel_not_in_flight",
                    DiscardKind::CancelNotForwarded => "cancel_not_forwarded",
                    DiscardKind::NotificationBeforeReady => "notification_before_ready",
                    DiscardKind::UnknownResponseId => "unknown_response_id",
                    DiscardKind::OutOfScopeProgress => "out_of_scope_progress",
                },
            );
        }
        TraceEvent::UpstreamEnded { upstream, error } => {
            out.push_str(r#""event":"upstream_ended","upstream":"#);
            write_capped(out, upstream);
            out.push_str(r#","error":"#);
            write_optional(out, error.as_deref());
        }
        TraceEvent::SessionEnded {
            reason,
            undelivered,
            delivery_failed,
        } => {
            out.push_str(r#""event":"session_ended","reason":"#);
            write_end_reason(out, reason);
            out.push_str(r#","undelivered":"#);
            out.push_str(&undelivered.to_string());
            out.push_str(r#","delivery_failed":"#);
            out.push_str(if *delivery_failed { "true" } else { "false" });
        }
    }
}

fn write_envelope(out: &mut String, envelope: &GrantEnvelope) {
    out.push_str(r#""principal":"#);
    write_str(out, envelope.principal.as_str());
    out.push_str(r#","grants":["#);
    for (index, grant) in envelope.grants.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_grant(out, grant);
    }
    out.push(']');
}

fn write_grant(out: &mut String, grant: &Grant) {
    out.push_str(r#"{"tool":"#);
    write_str(out, grant.tool.as_str());
    out.push_str(r#","expires":"#);
    match grant.expires {
        None => out.push_str("null"),
        Some(expires) => out.push_str(&expires.unix_secs().to_string()),
    }
    out.push_str(r#","args":{"#);
    for (index, (argument, constraint)) in grant.constraints.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_str(out, argument);
        out.push(':');
        write_constraint(out, constraint);
    }
    out.push_str("}}");
}

fn write_constraint(out: &mut String, constraint: &Constraint) {
    match constraint {
        Constraint::Prefix(value) => {
            out.push_str(r#"{"kind":"prefix","value":"#);
            write_capped(out, value);
            out.push('}');
        }
        Constraint::Suffix(value) => {
            out.push_str(r#"{"kind":"suffix","value":"#);
            write_capped(out, value);
            out.push('}');
        }
        Constraint::OneOf(values) => {
            out.push_str(r#"{"kind":"one_of","values":["#);
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_capped(out, value);
            }
            out.push_str("]}");
        }
        Constraint::Range { min, max } => {
            out.push_str(r#"{"kind":"range","min":"#);
            write_optional_int(out, *min);
            out.push_str(r#","max":"#);
            write_optional_int(out, *max);
            out.push('}');
        }
        Constraint::Absent => out.push_str(r#"{"kind":"absent"}"#),
    }
}

/// The call as evaluated: the tool, and every argument the core saw.
fn write_call(out: &mut String, call: &ToolCall) {
    out.push_str(r#""tool":"#);
    write_capped(out, &call.tool);
    out.push_str(r#","args":{"#);
    for (index, (argument, value)) in call.args.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_str(out, argument);
        out.push(':');
        match value {
            ArgValue::Str(text) => {
                out.push_str(r#"{"kind":"str","value":"#);
                write_capped(out, text);
                out.push('}');
            }
            ArgValue::Int(n) => {
                out.push_str(r#"{"kind":"int","value":"#);
                out.push_str(&n.to_string());
                out.push('}');
            }
            // Carried so the record shows the argument was present; no
            // constraint ever admits it, so there is nothing to compare.
            ArgValue::Other => out.push_str(r#"{"kind":"other"}"#),
        }
    }
    out.push('}');
}

fn write_decision(out: &mut String, decision: &Decision) {
    match decision {
        Decision::Allow { grant } => {
            out.push_str(r#"{"kind":"allow","grant":"#);
            out.push_str(&grant.to_string());
            out.push('}');
        }
        Decision::Deny(reason) => {
            out.push_str(r#"{"kind":"deny","reason":"#);
            write_str(
                out,
                match reason {
                    DenialReason::NotGranted => "not_granted",
                    DenialReason::Expired => "expired",
                    DenialReason::OutOfEnvelope => "out_of_envelope",
                    DenialReason::EvaluationError { .. } => "evaluation_error",
                },
            );
            if let DenialReason::EvaluationError { detail } = reason {
                out.push_str(r#","detail":"#);
                write_capped(out, detail);
            }
            out.push('}');
        }
    }
}

fn write_outcome(out: &mut String, outcome: &CallOutcome) {
    match outcome {
        CallOutcome::Result { is_error } => {
            out.push_str(r#"{"kind":"result","is_error":"#);
            out.push_str(if *is_error { "true" } else { "false" });
            out.push('}');
        }
        CallOutcome::Error { code } => {
            out.push_str(r#"{"kind":"error","code":"#);
            out.push_str(&code.to_string());
            out.push('}');
        }
        CallOutcome::NotForwarded(reason) => {
            out.push_str(r#"{"kind":"not_forwarded","reason":"#);
            write_str(
                out,
                match reason {
                    NotForwardedReason::UpstreamBusy => "upstream_busy",
                    NotForwardedReason::UpstreamUnavailable => "upstream_unavailable",
                    NotForwardedReason::Untranslatable => "untranslatable",
                },
            );
            out.push('}');
        }
        CallOutcome::Cancelled => out.push_str(r#"{"kind":"cancelled"}"#),
        CallOutcome::Abandoned => out.push_str(r#"{"kind":"abandoned"}"#),
    }
}

fn write_end_reason(out: &mut String, reason: &SessionEndReason) {
    match reason {
        SessionEndReason::ClientEof => out.push_str(r#"{"kind":"client_eof"}"#),
        SessionEndReason::ClientReadError => out.push_str(r#"{"kind":"client_read_error"}"#),
        SessionEndReason::ClientWriteFailed => out.push_str(r#"{"kind":"client_write_failed"}"#),
        SessionEndReason::UpstreamGone { upstream } => {
            out.push_str(r#"{"kind":"upstream_gone","upstream":"#);
            write_capped(out, upstream);
            out.push('}');
        }
        SessionEndReason::ToolCollision { tool } => {
            out.push_str(r#"{"kind":"tool_collision","tool":"#);
            write_capped(out, tool);
            out.push('}');
        }
        SessionEndReason::Internal => out.push_str(r#"{"kind":"internal"}"#),
    }
}

fn write_optional(out: &mut String, value: Option<&str>) {
    match value {
        None => out.push_str("null"),
        Some(value) => write_capped(out, value),
    }
}

fn write_optional_int(out: &mut String, value: Option<i64>) {
    match value {
        None => out.push_str("null"),
        Some(value) => out.push_str(&value.to_string()),
    }
}

/// A string, whole if it fits under [`VALUE_CAP`] and otherwise as its
/// prefix plus what it takes to identify the rest.
///
/// The digest covers exactly the bytes the record is derived from — the
/// evaluated value — so the line is internally consistent, and two calls
/// carrying the same oversized payload are visibly the same payload.
fn write_capped(out: &mut String, value: &str) {
    if value.len() <= VALUE_CAP {
        write_str(out, value);
        return;
    }
    let cut = floor_char_boundary(value, VALUE_CAP);
    out.push_str(r#"{"truncated":"#);
    write_str(out, &value[..cut]);
    out.push_str(r#","bytes":"#);
    out.push_str(&value.len().to_string());
    out.push_str(r#","sha256":"#);
    write_str(out, &sha256_hex(value.as_bytes()));
    out.push('}');
}

/// The largest index at or below `max` that splits `s` between characters.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if s.len() <= max {
        return s.len();
    }
    let mut index = max;
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Two lowercase hex digits, without a formatting dependency.
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        hex.push(DIGITS[usize::from(byte >> 4)] as char);
        hex.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    hex
}

/// A JSON string literal.
///
/// Hand-written so the CLI needs no JSON dependency for the one thing it
/// writes. Escapes exactly what RFC 8259 requires: the quote, the
/// backslash, and every control character below `0x20`.
fn write_str(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u");
                let code = c as u32;
                const DIGITS: &[u8; 16] = b"0123456789abcdef";
                for shift in [12, 8, 4, 0] {
                    out.push(DIGITS[((code >> shift) & 0xf) as usize] as char);
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use flavium_core::{CallId, Principal, Timestamp, ToolName};
    use std::collections::BTreeMap;

    /// A fresh path per call: these tests run in parallel in one process,
    /// and the sink appends.
    fn scratch(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("flavium-trace-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{name}-{unique}.jsonl"))
    }

    fn line_of(event: &TraceEvent) -> serde_json::Value {
        let path = scratch("line");
        let sink = JsonlSink::create(&path).unwrap();
        sink.record(event).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        serde_json::from_str(text.trim_end()).expect("each line is one JSON object")
    }

    fn principal() -> Principal {
        Principal::new("invoice-bot").unwrap()
    }

    fn call(args: &[(&str, ArgValue)]) -> ToolCall {
        ToolCall {
            tool: "read_file".into(),
            args: args
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        }
    }

    #[test]
    fn every_line_carries_the_recorder_fields() {
        let line = line_of(&TraceEvent::FrameRejected { code: -32700 });
        assert_eq!(line["v"], 1);
        assert_eq!(line["seq"], 1);
        assert!(line["ts"].as_u64().unwrap() > 1_600_000_000_000);
        assert!(line["session"].as_str().unwrap().contains('-'));
        assert_eq!(line["event"], "frame_rejected");
        assert_eq!(line["code"], -32700);
    }

    #[test]
    fn seq_is_dense_and_monotonic() {
        let path = scratch("seq");
        let sink = JsonlSink::create(&path).unwrap();
        for code in 0..5 {
            sink.record(&TraceEvent::FrameRejected { code }).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let seqs: Vec<u64> = text
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["seq"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
        // Every line shares one session id.
        let sessions: std::collections::BTreeSet<String> = text
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["session"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions.iter().next().unwrap(), sink.session_id());
    }

    #[test]
    fn the_envelope_is_recorded_so_grant_indices_can_be_read() {
        let envelope = GrantEnvelope {
            principal: principal(),
            grants: vec![
                Grant {
                    tool: ToolName::new("read_file").unwrap(),
                    constraints: BTreeMap::from([(
                        "path".to_owned(),
                        Constraint::Prefix("/data/invoices/".into()),
                    )]),
                    expires: Some(Timestamp::from_unix_secs(1_756_684_800)),
                },
                Grant {
                    tool: ToolName::new("send_mail").unwrap(),
                    constraints: BTreeMap::from([
                        ("bcc".to_owned(), Constraint::Absent),
                        (
                            "count".to_owned(),
                            Constraint::Range {
                                min: Some(1),
                                max: None,
                            },
                        ),
                        (
                            "kind".to_owned(),
                            Constraint::OneOf(["a", "b"].iter().map(|s| s.to_string()).collect()),
                        ),
                        ("to".to_owned(), Constraint::Suffix("@yourco.com".into())),
                    ]),
                    expires: None,
                },
            ],
        };
        let line = line_of(&TraceEvent::SessionStarted { envelope });
        assert_eq!(line["event"], "session_started");
        assert_eq!(line["principal"], "invoice-bot");
        assert_eq!(line["grants"][0]["tool"], "read_file");
        assert_eq!(line["grants"][0]["expires"], 1_756_684_800_i64);
        assert_eq!(
            line["grants"][0]["args"]["path"],
            serde_json::json!({"kind": "prefix", "value": "/data/invoices/"})
        );
        assert!(line["grants"][1]["expires"].is_null());
        assert_eq!(
            line["grants"][1]["args"]["bcc"],
            serde_json::json!({"kind": "absent"})
        );
        assert_eq!(
            line["grants"][1]["args"]["count"],
            serde_json::json!({"kind": "range", "min": 1, "max": null})
        );
        assert_eq!(
            line["grants"][1]["args"]["kind"],
            serde_json::json!({"kind": "one_of", "values": ["a", "b"]})
        );
        assert_eq!(
            line["grants"][1]["args"]["to"],
            serde_json::json!({"kind": "suffix", "value": "@yourco.com"})
        );
    }

    #[test]
    fn a_decision_records_the_call_the_core_judged() {
        let line = line_of(&TraceEvent::CallDecided {
            principal: principal(),
            call_id: CallId(3),
            call: call(&[
                ("path", ArgValue::Str("/etc/passwd".into())),
                ("count", ArgValue::Int(-7)),
                ("blob", ArgValue::Other),
            ]),
            args_as_sent: Default::default(),
            now: Timestamp::from_unix_secs(1_755_300_001),
            decision: Decision::Deny(DenialReason::OutOfEnvelope),
        });
        assert_eq!(line["event"], "call_decided");
        assert_eq!(line["call_id"], 3);
        assert_eq!(line["now"], 1_755_300_001_i64);
        assert_eq!(line["tool"], "read_file");
        assert_eq!(
            line["args"]["path"],
            serde_json::json!({"kind": "str", "value": "/etc/passwd"})
        );
        assert_eq!(
            line["args"]["count"],
            serde_json::json!({"kind": "int", "value": -7})
        );
        assert_eq!(line["args"]["blob"], serde_json::json!({"kind": "other"}));
        assert_eq!(
            line["decision"],
            serde_json::json!({"kind": "deny", "reason": "out_of_envelope"})
        );

        let allowed = line_of(&TraceEvent::CallDecided {
            principal: principal(),
            call_id: CallId(0),
            call: call(&[]),
            args_as_sent: Default::default(),
            now: Timestamp::from_unix_secs(0),
            decision: Decision::Allow { grant: 2 },
        });
        assert_eq!(
            allowed["decision"],
            serde_json::json!({"kind": "allow", "grant": 2})
        );
        assert_eq!(allowed["args"], serde_json::json!({}));

        assert!(
            allowed.get("args_as_sent").is_none(),
            "nothing was normalized, so the key must not appear at all"
        );

        // An engine failure reaches the operator here and nowhere else.
        let failed = line_of(&TraceEvent::CallDecided {
            principal: principal(),
            call_id: CallId(1),
            call: call(&[]),
            args_as_sent: Default::default(),
            now: Timestamp::from_unix_secs(0),
            decision: Decision::Deny(DenialReason::EvaluationError {
                detail: "context build failed".into(),
            }),
        });
        assert_eq!(
            failed["decision"],
            serde_json::json!({
                "kind": "deny",
                "reason": "evaluation_error",
                "detail": "context build failed"
            })
        );
    }

    /// Normalization is lossy, so the line carries the caller's own
    /// spelling beside the evaluated one — for the arguments where they
    /// differ, and no others.
    #[test]
    fn a_decision_also_records_what_was_asked_for() {
        let line = line_of(&TraceEvent::CallDecided {
            principal: principal(),
            call_id: CallId(4),
            call: call(&[
                ("path", ArgValue::Str("c:/users/me/x".into())),
                ("note", ArgValue::Str("untouched".into())),
            ]),
            args_as_sent: [("path".to_string(), r"C:\Users\Me\other\..\x".to_string())]
                .into_iter()
                .collect(),
            now: Timestamp::from_unix_secs(0),
            decision: Decision::Allow { grant: 0 },
        });
        // The decision was made on the evaluated value, and that is still
        // what `args` holds — replay reads this, not the spelling.
        assert_eq!(
            line["args"]["path"],
            serde_json::json!({"kind": "str", "value": "c:/users/me/x"})
        );
        // …and the record can now answer what the agent asked for, which
        // `args` alone cannot: `..` resolved and case folded away.
        assert_eq!(line["args_as_sent"]["path"], r"C:\Users\Me\other\..\x");
        // An argument that normalization left alone is not duplicated.
        assert!(line["args_as_sent"].get("note").is_none());

        // Two calls whose evaluated form is identical stay distinguishable.
        let other = line_of(&TraceEvent::CallDecided {
            principal: principal(),
            call_id: CallId(5),
            call: call(&[("path", ArgValue::Str("c:/users/me/x".into()))]),
            args_as_sent: [("path".to_string(), r"C:\USERS\ME\X".to_string())]
                .into_iter()
                .collect(),
            now: Timestamp::from_unix_secs(0),
            decision: Decision::Allow { grant: 0 },
        });
        assert_eq!(line["args"]["path"], other["args"]["path"]);
        assert_ne!(line["args_as_sent"], other["args_as_sent"]);
    }

    /// A spelling is an argument value like any other, so the cap that
    /// keeps the log from becoming a copy of the data plane applies to it
    /// too — otherwise a megabyte path would arrive through the back door.
    #[test]
    fn an_oversized_spelling_is_capped_like_any_other_value() {
        let over = "a".repeat(VALUE_CAP + 1);
        let line = line_of(&TraceEvent::CallDecided {
            principal: principal(),
            call_id: CallId(0),
            call: call(&[("path", ArgValue::Str("/a".into()))]),
            args_as_sent: [("path".to_string(), over.clone())].into_iter().collect(),
            now: Timestamp::from_unix_secs(0),
            decision: Decision::Allow { grant: 0 },
        });
        let value = &line["args_as_sent"]["path"];
        assert_eq!(value["truncated"].as_str().unwrap().len(), VALUE_CAP);
        assert_eq!(value["bytes"], (VALUE_CAP + 1) as u64);
        assert!(value["sha256"].as_str().is_some());
    }

    #[test]
    fn every_outcome_and_ending_has_a_spelling() {
        let outcome = |outcome: CallOutcome| -> serde_json::Value {
            line_of(&TraceEvent::CallCompleted {
                principal: principal(),
                call_id: CallId(0),
                outcome,
            })["outcome"]
                .clone()
        };
        assert_eq!(
            outcome(CallOutcome::Result { is_error: true }),
            serde_json::json!({"kind": "result", "is_error": true})
        );
        assert_eq!(
            outcome(CallOutcome::Error { code: -32603 }),
            serde_json::json!({"kind": "error", "code": -32603})
        );
        assert_eq!(
            outcome(CallOutcome::NotForwarded(NotForwardedReason::UpstreamBusy)),
            serde_json::json!({"kind": "not_forwarded", "reason": "upstream_busy"})
        );
        assert_eq!(
            outcome(CallOutcome::NotForwarded(
                NotForwardedReason::Untranslatable
            )),
            serde_json::json!({"kind": "not_forwarded", "reason": "untranslatable"})
        );
        assert_eq!(
            outcome(CallOutcome::Cancelled),
            serde_json::json!({"kind": "cancelled"})
        );
        assert_eq!(
            outcome(CallOutcome::Abandoned),
            serde_json::json!({"kind": "abandoned"})
        );

        let ended = |reason: SessionEndReason| -> serde_json::Value {
            line_of(&TraceEvent::SessionEnded {
                reason,
                undelivered: 2,
                delivery_failed: true,
            })
        };
        let clean = ended(SessionEndReason::ClientEof);
        assert_eq!(clean["reason"], serde_json::json!({"kind": "client_eof"}));
        assert_eq!(clean["undelivered"], 2);
        assert_eq!(clean["delivery_failed"], true);
        assert_eq!(
            ended(SessionEndReason::UpstreamGone {
                upstream: "fs".into()
            })["reason"],
            serde_json::json!({"kind": "upstream_gone", "upstream": "fs"})
        );
        assert_eq!(
            ended(SessionEndReason::ToolCollision {
                tool: "read_file".into()
            })["reason"],
            serde_json::json!({"kind": "tool_collision", "tool": "read_file"})
        );
        assert_eq!(
            ended(SessionEndReason::Internal)["reason"],
            serde_json::json!({"kind": "internal"})
        );

        for (kind, spelling) in [
            (DiscardKind::StrayResponse, "stray_response"),
            (
                DiscardKind::UnroutableNotification,
                "unroutable_notification",
            ),
            (DiscardKind::StaleResponse, "stale_response"),
            (DiscardKind::CancelUnreadable, "cancel_unreadable"),
            (DiscardKind::CancelNotInFlight, "cancel_not_in_flight"),
            (DiscardKind::CancelNotForwarded, "cancel_not_forwarded"),
            (
                DiscardKind::NotificationBeforeReady,
                "notification_before_ready",
            ),
            (DiscardKind::UnknownResponseId, "unknown_response_id"),
            (DiscardKind::OutOfScopeProgress, "out_of_scope_progress"),
        ] {
            assert_eq!(
                line_of(&TraceEvent::FrameDiscarded { kind })["kind"],
                spelling
            );
        }

        let listed = line_of(&TraceEvent::ToolsListed {
            principal: principal(),
            now: Timestamp::from_unix_secs(7),
            offered: 9,
            granted: 2,
        });
        assert_eq!(listed["offered"], 9);
        assert_eq!(listed["granted"], 2);
        assert_eq!(listed["now"], 7);

        let refused = line_of(&TraceEvent::CallRefused {
            principal: principal(),
            call_id: CallId(4),
            tool: None,
            reason: RefusalReason::MalformedParams,
        });
        assert!(refused["tool"].is_null());
        assert_eq!(refused["reason"], "malformed_params");

        let handshake = line_of(&TraceEvent::HandshakeCompleted {
            offered_protocol_version: "2025-11-25".into(),
            protocol_version: "2025-11-25".into(),
            client_name: Some("claude".into()),
            client_version: None,
        });
        assert_eq!(handshake["client_name"], "claude");
        assert!(handshake["client_version"].is_null());

        let upstream = line_of(&TraceEvent::UpstreamEnded {
            upstream: "fs".into(),
            error: Some("pipe closed".into()),
        });
        assert_eq!(upstream["upstream"], "fs");
        assert_eq!(upstream["error"], "pipe closed");
    }

    /// The cap, both sides of it — and the digest is the reason it is
    /// safe to lose the tail.
    #[test]
    fn oversized_values_carry_their_length_and_digest() {
        let under = "a".repeat(VALUE_CAP);
        let line = line_of(&TraceEvent::CallDecided {
            principal: principal(),
            call_id: CallId(0),
            call: call(&[("v", ArgValue::Str(under.clone()))]),
            args_as_sent: Default::default(),
            now: Timestamp::from_unix_secs(0),
            decision: Decision::Allow { grant: 0 },
        });
        assert_eq!(
            line["args"]["v"],
            serde_json::json!({"kind": "str", "value": under}),
            "a value at the cap is recorded whole and unhashed"
        );

        let over = "a".repeat(VALUE_CAP + 1);
        let line = line_of(&TraceEvent::CallDecided {
            principal: principal(),
            call_id: CallId(0),
            call: call(&[("v", ArgValue::Str(over.clone()))]),
            args_as_sent: Default::default(),
            now: Timestamp::from_unix_secs(0),
            decision: Decision::Allow { grant: 0 },
        });
        let value = &line["args"]["v"]["value"];
        assert_eq!(value["truncated"].as_str().unwrap().len(), VALUE_CAP);
        assert_eq!(value["bytes"], (VALUE_CAP + 1) as u64);
        let digest = value["sha256"].as_str().unwrap();
        assert_eq!(digest.len(), 64);
        assert_eq!(digest, sha256_hex(over.as_bytes()));

        // Two calls carrying the same oversized payload are visibly the
        // same payload — the property the digest exists for.
        let again = line_of(&TraceEvent::CallDecided {
            principal: principal(),
            call_id: CallId(1),
            call: call(&[("w", ArgValue::Str(over))]),
            args_as_sent: Default::default(),
            now: Timestamp::from_unix_secs(0),
            decision: Decision::Allow { grant: 0 },
        });
        assert_eq!(again["args"]["w"]["value"]["sha256"], digest);

        // The row the cap was chosen for: the longest path Linux can
        // express is recorded whole.
        let path_max = format!("/{}", "p".repeat(4094));
        assert_eq!(path_max.len(), 4095);
        let line = line_of(&TraceEvent::CallDecided {
            principal: principal(),
            call_id: CallId(0),
            call: call(&[("path", ArgValue::Str(path_max.clone()))]),
            args_as_sent: Default::default(),
            now: Timestamp::from_unix_secs(0),
            decision: Decision::Allow { grant: 0 },
        });
        assert_eq!(line["args"]["path"]["value"], path_max);
    }

    /// A multi-byte character must never be split by the cap: the line
    /// has to stay valid UTF-8 and valid JSON.
    #[test]
    fn truncation_lands_on_a_character_boundary() {
        // 'é' is two bytes; starting one byte before the cap puts a
        // character across it.
        let value = format!("{}{}", "a".repeat(VALUE_CAP - 1), "é".repeat(10));
        assert!(value.len() > VALUE_CAP);
        let line = line_of(&TraceEvent::CallDecided {
            principal: principal(),
            call_id: CallId(0),
            call: call(&[("v", ArgValue::Str(value.clone()))]),
            args_as_sent: Default::default(),
            now: Timestamp::from_unix_secs(0),
            decision: Decision::Allow { grant: 0 },
        });
        let kept = line["args"]["v"]["value"]["truncated"].as_str().unwrap();
        assert_eq!(kept.len(), VALUE_CAP - 1);
        assert_eq!(line["args"]["v"]["value"]["bytes"], value.len() as u64);
        assert_eq!(floor_char_boundary("éé", 1), 0);
        assert_eq!(floor_char_boundary("ab", 5), 2);
    }

    /// Nothing a client sends can break a line: control characters,
    /// quotes and backslashes are escaped, so the file stays one JSON
    /// object per line.
    #[test]
    fn hostile_strings_stay_inside_their_json_string() {
        let nasty = "a\"b\\c\nd\re\tf\u{8}g\u{c}h\u{1}i\u{1f}j";
        let line = line_of(&TraceEvent::CallDecided {
            principal: principal(),
            call_id: CallId(0),
            call: call(&[("v", ArgValue::Str(nasty.to_owned()))]),
            args_as_sent: Default::default(),
            now: Timestamp::from_unix_secs(0),
            decision: Decision::Allow { grant: 0 },
        });
        assert_eq!(line["args"]["v"]["value"], nasty);

        let mut out = String::new();
        write_str(&mut out, nasty);
        assert!(out.contains("\\u0001"));
        assert!(out.contains("\\u001f"));
        assert!(out.contains("\\b"));
        assert!(out.contains("\\f"));
        assert!(!out.contains('\n'));

        // A tool name a grant could never carry still round-trips.
        let line = line_of(&TraceEvent::CallRefused {
            principal: principal(),
            call_id: CallId(0),
            tool: Some("read\nfile".to_owned()),
            reason: RefusalReason::UnknownTool,
        });
        assert_eq!(line["tool"], "read\nfile");
    }

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn a_failed_sink_stays_failed() {
        let path = scratch("failed");
        let sink = JsonlSink::create(&path).unwrap();
        sink.record(&TraceEvent::FrameRejected { code: -1 })
            .unwrap();
        // Simulate the write path dying the way a full disk would.
        sink.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .failed = true;
        assert!(sink
            .record(&TraceEvent::FrameRejected { code: -2 })
            .is_err());
        assert!(sink
            .record(&TraceEvent::FrameRejected { code: -3 })
            .is_err());
        // Only the first event ever reached the file.
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
    }

    /// The file holds the arguments of every call, so a file this run
    /// creates is readable only by its owner. (Windows has no mode; the
    /// file inherits the directory's ACL, which `docs/cli.md` says.)
    #[cfg(unix)]
    #[test]
    fn a_created_trace_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = scratch("mode");
        let _sink = JsonlSink::create(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "trace file mode was {:o}",
            mode & 0o777
        );
    }

    #[test]
    fn creating_the_sink_fails_loudly_on_an_unusable_path() {
        let dir = std::env::temp_dir().join(format!("flavium-trace-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A directory is not a file to append to.
        assert!(JsonlSink::create(&dir).is_err());
    }
}
