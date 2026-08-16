//! Trace events and the sink they go to.
//!
//! Every grant decision, refusal, budget tick, spawn and termination is a
//! trace event — if it is not traced, it is not done. This module fixes the
//! vocabulary ([`TraceEvent`]) and the seam ([`TraceSink`]); it does no I/O
//! and has no clock. The CLI supplies a JSONL sink (M5); T4 supplies the
//! hash-chained SQLite recorder with deterministic replay.
//!
//! Design points, so sinks and future variants stay consistent:
//!
//! - **Exhaustive on purpose.** [`TraceEvent`] is *not* `#[non_exhaustive]`:
//!   a sink must handle every variant at compile time, so adding one (T2a
//!   budget ticks, T2b model calls, T3 spawn/termination) is a wanted
//!   compile-error ripple through
//!   every sink, never a silently unserialized event.
//! - **Clock-free.** An event carries what enforcement computed, including
//!   the `now` a decision was made with (replaying the decision needs
//!   exactly envelope + call + now). Wall-clock time, sequence numbers,
//!   hashes and the session identifier are the recorder's to add.
//! - **Ordered.** The proxy emits from one task in causal order; a sink may
//!   therefore assign sequence numbers or chain hashes under its own lock.
//! - **Fallible.** [`TraceSink::record`] returns an error so the runtime can
//!   fail closed on audit: the recommended policy is that a sink failure
//!   ends the session.
//! - **Per session.** The trace begins when a session begins.
//!   [`TraceEvent::SessionStarted`] is emitted once startup has succeeded
//!   (every upstream connected and listed); a run that fails during
//!   startup never has a session and is reported through the process's
//!   logs and exit code, not the trace.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use crate::grant::{Decision, GrantEnvelope, ToolCall};
use crate::name::Principal;
use crate::time::Timestamp;

/// Correlates the events of one call within a session: refusal *or*
/// decision, then completion. Minted by the proxy, monotonic per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallId(pub u64);

impl fmt::Display for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Why a `tools/call` was refused before any grant decision was made — a
/// protocol-level refusal, not a policy one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// The request's params did not have the required shape (for instance
    /// `arguments` was not an object, or held duplicate keys).
    MalformedParams,
    /// No upstream offers a tool by that name.
    UnknownTool,
    /// The client reused a request id that is still in flight.
    DuplicateRequestId,
}

/// Why an allowed call never reached its upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotForwardedReason {
    /// The upstream's command queue was full.
    UpstreamBusy,
    /// The upstream's connection was already gone.
    UpstreamUnavailable,
    /// The request could not be rewritten unambiguously for the upstream.
    Untranslatable,
}

/// How an allowed call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOutcome {
    /// The upstream answered with a result; `is_error` mirrors the MCP
    /// `isError` flag when present.
    Result {
        /// The tool reported an error result.
        is_error: bool,
    },
    /// The client was answered with a JSON-RPC error: the upstream's own,
    /// or the proxy's substitute when the upstream's response could not be
    /// translated back.
    Error {
        /// The JSON-RPC error code.
        code: i64,
    },
    /// The call was allowed but never forwarded.
    NotForwarded(NotForwardedReason),
    /// The client cancelled the call before it was answered.
    Cancelled,
    /// The session ended while the call was in flight.
    Abandoned,
}

/// A frame the router or an upstream connection consumed without
/// forwarding or answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardKind {
    /// A response from the client (the proxy never sends it requests).
    StrayResponse,
    /// A client notification the proxy does not route.
    UnroutableNotification,
    /// An upstream response for a call no longer in flight toward it
    /// (cancelled, or answered by the wrong upstream).
    StaleResponse,
    /// A cancellation whose `requestId` could not be read (missing, `null`,
    /// or not an integer/string); there is nothing to look up.
    CancelUnreadable,
    /// A cancellation for a request that was not in flight.
    CancelNotInFlight,
    /// A cancellation the proxy could not forward (queue full); the late
    /// response is dropped instead.
    CancelNotForwarded,
    /// An upstream notification that arrived before the client was ready.
    NotificationBeforeReady,
    /// An upstream response with an id the connection never minted.
    UnknownResponseId,
    /// A progress notification for a token this connection did not send.
    OutOfScopeProgress,
}

/// Why a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEndReason {
    /// The client closed its input — the normal shutdown.
    ClientEof,
    /// Reading from the client failed.
    ClientReadError,
    /// Writing to the client failed.
    ClientWriteFailed,
    /// An upstream connection ended or failed (until T3 supervision, any
    /// upstream ending ends the session).
    UpstreamGone {
        /// The upstream's configured name.
        upstream: String,
    },
    /// A re-listed upstream introduced a tool-name collision.
    ToolCollision {
        /// The contested tool name.
        tool: String,
    },
    /// An internal invariant broke; surfaced loudly rather than hung.
    Internal,
}

/// Everything the runtime records. One session is a sequence of these,
/// beginning with [`TraceEvent::SessionStarted`] and ending with
/// [`TraceEvent::SessionEnded`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEvent {
    /// The session began under this envelope — the policy in force, so
    /// every later `Allow { grant }` index can be interpreted.
    SessionStarted {
        /// The principal and its grants, in file order.
        envelope: GrantEnvelope,
    },
    /// The client handshake completed. `offered_protocol_version` and
    /// `client_*` come from the client's `initialize`: untrusted,
    /// informational, never identity — recorded so protocol drift is
    /// observable.
    HandshakeCompleted {
        /// The protocol revision the client asked for.
        offered_protocol_version: String,
        /// The negotiated protocol revision the session runs.
        protocol_version: String,
        /// The client's self-reported name, if any.
        client_name: Option<String>,
        /// The client's self-reported version, if any.
        client_version: Option<String>,
    },
    /// The client asked for the tool list and was shown the granted subset.
    ToolsListed {
        /// Whose grants filtered the list.
        principal: Principal,
        /// The time the filter was evaluated at.
        now: Timestamp,
        /// Tools the upstreams offer.
        offered: u64,
        /// Tools shown to the client — those with a live grant.
        granted: u64,
    },
    /// A `tools/call` was refused before any grant decision.
    CallRefused {
        /// The principal the call would have been attributed to.
        principal: Principal,
        /// Correlation id.
        call_id: CallId,
        /// The requested tool name, when it could be read.
        tool: Option<String>,
        /// Why.
        reason: RefusalReason,
    },
    /// A `tools/call` was authorized: allowed or denied by policy.
    CallDecided {
        /// Who asked.
        principal: Principal,
        /// Correlation id.
        call_id: CallId,
        /// The call as the core evaluated it (tool and arguments).
        call: ToolCall,
        /// The string arguments whose spelling the caller sent differently
        /// from the form in `call`, by argument name — empty when the two
        /// agree, which is the common case.
        ///
        /// `call` answers *why was this decided so*, and it must stay the
        /// evaluated form for that answer to reproduce. This answers the
        /// other question an auditor asks — *what did the agent ask for* —
        /// which the evaluated form cannot: normalization is lossy, so two
        /// different requests (`…/a/../b` and `…/b`, or two spellings of
        /// one Windows path) reach the same decision and were until now
        /// recorded identically. Only differing arguments appear, so the
        /// key's presence means normalization changed something.
        ///
        /// Only [`ArgValue::Str`](crate::ArgValue::Str) values are ever
        /// normalized, so the pre-image is always a string.
        args_as_sent: BTreeMap<String, String>,
        /// The time the decision was made with.
        now: Timestamp,
        /// The decision.
        decision: Decision,
    },
    /// An allowed call ended.
    CallCompleted {
        /// Who asked.
        principal: Principal,
        /// Correlation id.
        call_id: CallId,
        /// How it ended.
        outcome: CallOutcome,
    },
    /// A client frame was rejected at the parse boundary and answered with
    /// this JSON-RPC error code.
    FrameRejected {
        /// The error code sent back (`-32700`, `-32600`, …).
        code: i64,
    },
    /// A frame was consumed without being forwarded or answered.
    FrameDiscarded {
        /// What kind of frame, and why.
        kind: DiscardKind,
    },
    /// An upstream connection ended.
    UpstreamEnded {
        /// The upstream's configured name.
        upstream: String,
        /// The failure, if it was one (secrets already redacted by the
        /// emitter).
        error: Option<String>,
    },
    /// The session ended. Clean iff `reason` is `ClientEof` and
    /// `delivery_failed` is false.
    SessionEnded {
        /// Why.
        reason: SessionEndReason,
        /// Client-bound frames accepted but never delivered; a lower bound
        /// when `delivery_failed` is set (frames abandoned mid-write at
        /// teardown may not be counted).
        undelivered: u64,
        /// True when not every accepted client-bound frame was delivered —
        /// the writer failed or was abandoned at teardown. The exit-code
        /// criterion, recorded even when `undelivered` cannot count it.
        delivery_failed: bool,
    },
}

/// The error a sink may report from [`TraceSink::record`].
pub type SinkError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Where trace events go. Implementations own their I/O; the core only
/// hands them events, in causal order, from one task at a time per session.
pub trait TraceSink: Send + Sync {
    /// Record one event. An `Err` means the event may not have been
    /// durably recorded; the runtime is expected to fail closed on it.
    fn record(&self, event: &TraceEvent) -> Result<(), SinkError>;
}

impl<T: TraceSink + ?Sized> TraceSink for Arc<T> {
    fn record(&self, event: &TraceEvent) -> Result<(), SinkError> {
        (**self).record(event)
    }
}

/// A sink that discards everything. For tests and for explicitly untraced
/// runs.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl TraceSink for NullSink {
    fn record(&self, _event: &TraceEvent) -> Result<(), SinkError> {
        Ok(())
    }
}

/// A sink that keeps every event in memory, in order. For tests.
#[derive(Debug, Default)]
pub struct MemorySink {
    events: Mutex<Vec<TraceEvent>>,
}

impl MemorySink {
    /// An empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of every event recorded so far, oldest first.
    pub fn events(&self) -> Vec<TraceEvent> {
        self.lock().clone()
    }

    /// How many events have been recorded.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// True if nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// The events, tolerating a poisoned lock: a panic on another thread
    /// while holding it cannot have left the `Vec` inconsistent (pushes are
    /// atomic with respect to what a reader can observe), so the data is
    /// still good.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<TraceEvent>> {
        self.events.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl TraceSink for MemorySink {
    fn record(&self, event: &TraceEvent) -> Result<(), SinkError> {
        self.lock().push(event.clone());
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::grant::DenialReason;

    fn principal() -> Principal {
        Principal::new("bot").unwrap()
    }

    fn sample_events() -> Vec<TraceEvent> {
        vec![
            TraceEvent::SessionStarted {
                envelope: GrantEnvelope {
                    principal: principal(),
                    grants: vec![],
                },
            },
            TraceEvent::CallDecided {
                principal: principal(),
                call_id: CallId(1),
                call: ToolCall {
                    tool: "read_file".into(),
                    args: Default::default(),
                },
                args_as_sent: BTreeMap::new(),
                now: Timestamp::from_unix_secs(0),
                decision: Decision::Deny(DenialReason::NotGranted),
            },
            TraceEvent::SessionEnded {
                reason: SessionEndReason::ClientEof,
                undelivered: 0,
                delivery_failed: false,
            },
        ]
    }

    #[test]
    fn memory_sink_records_in_order() {
        let sink = MemorySink::new();
        assert!(sink.is_empty());
        for event in sample_events() {
            sink.record(&event).unwrap();
        }
        assert_eq!(sink.len(), 3);
        assert_eq!(sink.events(), sample_events());
    }

    #[test]
    fn null_sink_accepts_everything() {
        for event in sample_events() {
            NullSink.record(&event).unwrap();
        }
    }

    #[test]
    fn works_behind_arc_dyn() {
        let sink: Arc<dyn TraceSink> = Arc::new(MemorySink::new());
        sink.record(&sample_events()[0]).unwrap();
        // The Arc blanket impl lets `Arc<MemorySink>` itself be a sink too.
        let concrete = Arc::new(MemorySink::new());
        let as_sink: &dyn TraceSink = &concrete;
        as_sink.record(&sample_events()[2]).unwrap();
        assert_eq!(concrete.len(), 1);
    }

    #[test]
    fn memory_sink_survives_poisoning() {
        let sink = Arc::new(MemorySink::new());
        sink.record(&sample_events()[0]).unwrap();
        let poisoner = Arc::clone(&sink);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.events.lock().unwrap();
            panic!("poison the lock on purpose");
        })
        .join();
        assert!(sink.events.is_poisoned());
        sink.record(&sample_events()[2]).unwrap();
        assert_eq!(sink.len(), 2);
        assert!(!sink.is_empty());
        // The pre-poison event survived, in order.
        assert_eq!(
            sink.events(),
            vec![sample_events()[0].clone(), sample_events()[2].clone()]
        );
    }

    #[test]
    fn call_id_displays_as_number() {
        assert_eq!(CallId(7).to_string(), "7");
    }
}
