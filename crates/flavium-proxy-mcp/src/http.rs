//! The streamable HTTP upstream client (MCP spec 2025-11-25, Streamable
//! HTTP transport).
//!
//! Shape of the transport:
//! - every outbound frame is its own `POST` to the MCP endpoint with
//!   `Accept: application/json, text/event-stream`;
//! - notifications and responses expect `202 Accepted`;
//! - a request's POST yields either one JSON body or an SSE stream that
//!   eventually carries the response (and may carry server messages
//!   before it);
//! - `MCP-Session-Id` is captured from the `initialize` response and
//!   echoed on everything after; a later `404` means the server ended
//!   the session — fatal in T1/M2 (no silent re-initialization);
//! - `MCP-Protocol-Version` is sent once the connection has negotiated;
//! - an optional long-lived `GET` stream carries unsolicited
//!   server→client messages; `405` means the server doesn't offer one.
//!
//! Deliberate M2 limits, stated where the guarantees are: SSE
//! resumability (`Last-Event-ID`) is not implemented — if a request's
//! stream dies before its response, the transport synthesizes a JSON-RPC
//! error response so the caller's accounting resolves instead of
//! hanging; redirects are refused (fail closed); TLS is rustls with
//! bundled roots.
//!
//! Failed POSTs for a single request are per-request failures, not
//! connection failures: hosted endpoints blip. The synthesized error
//! response is the only frame this transport ever originates, and it is
//! marked by its fixed message text.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::StatusCode;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::builder::{self, code};
use crate::envelope::{self, Message, RequestId, ResponseId};
use crate::sse::{SseItem, SseParser};
use crate::transport::{normalize_newlines, DropReason, Received, SendContext, TransportError};

/// Message text of the error response the transport synthesizes when a
/// request's POST fails; fixed and generic — details go to the log.
const UPSTREAM_FAILED_MSG: &str = "upstream request failed";

/// How long connection establishment may take before a POST/GET fails.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Best-effort session termination (DELETE) budget at close.
const DELETE_TIMEOUT: Duration = Duration::from_secs(2);

/// GET stream reconnect backoff bounds.
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Inbound frames queued between transport tasks and the reader.
const INBOUND_QUEUE: usize = 64;

/// Errors building an HTTP transport from configuration.
#[derive(Debug, thiserror::Error)]
pub enum HttpSetupError {
    /// The URL does not parse or is not http/https.
    #[error("invalid upstream url {url:?}")]
    BadUrl {
        /// The URL in redacted form (never the configured bytes, which
        /// may embed credentials).
        url: String,
    },

    /// A configured header name is not a legal HTTP header name.
    #[error("invalid header name {name:?}")]
    BadHeaderName {
        /// The name as configured.
        name: String,
    },

    /// A configured header value is not a legal HTTP header value.
    /// The value itself is never echoed — header values are secrets.
    #[error("invalid value for header {name:?}")]
    BadHeaderValue {
        /// The header whose value was rejected.
        name: String,
    },

    /// The HTTP client could not be constructed.
    #[error("failed to build HTTP client")]
    Client(#[source] reqwest::Error),
}

/// State shared with the POST and GET tasks.
struct Shared {
    client: reqwest::Client,
    url: reqwest::Url,
    /// Upstream name, for logs only.
    label: String,
    /// Operator-configured headers, sent on every request.
    extra: HeaderMap,
    max_frame: usize,
    session: Mutex<Option<HeaderValue>>,
    protocol_version: Mutex<Option<HeaderValue>>,
    inbound: mpsc::Sender<Inbound>,
    /// Last SSE `retry` hint in milliseconds; 0 = none.
    retry_hint_ms: AtomicU64,
}

enum Inbound {
    Frame(Vec<u8>),
    Dropped(DropReason),
    Fatal(TransportError),
}

/// Why one POST (or one GET attempt) failed.
enum HttpIssue {
    /// The server ended the MCP session (404 under a session id).
    SessionExpired,
    /// Anything else; the description goes to the log, never the peer.
    Failed(String),
}

/// A streamable-HTTP upstream connection.
pub struct HttpTransport {
    shared: Arc<Shared>,
    inbound_rx: mpsc::Receiver<Inbound>,
    tasks: JoinSet<()>,
    listening: bool,
}

impl HttpTransport {
    /// Builds the transport (no I/O yet — the first frame sent opens
    /// the first request).
    pub fn new(
        label: &str,
        url: &str,
        headers: &[(String, String)],
        max_frame: usize,
    ) -> Result<Self, HttpSetupError> {
        let parsed = reqwest::Url::parse(url).map_err(|_| HttpSetupError::BadUrl {
            url: crate::config::redact_url(url),
        })?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(HttpSetupError::BadUrl {
                url: crate::config::redact_url(url),
            });
        }
        let mut extra = HeaderMap::new();
        for (name, value) in headers {
            let header_name = HeaderName::try_from(name.as_str())
                .map_err(|_| HttpSetupError::BadHeaderName { name: name.clone() })?;
            let mut header_value = HeaderValue::try_from(value.as_str())
                .map_err(|_| HttpSetupError::BadHeaderValue { name: name.clone() })?;
            header_value.set_sensitive(true);
            extra.insert(header_name, header_value);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(HttpSetupError::Client)?;

        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_QUEUE);
        Ok(Self {
            shared: Arc::new(Shared {
                client,
                url: parsed,
                label: label.to_owned(),
                extra,
                max_frame,
                session: Mutex::new(None),
                protocol_version: Mutex::new(None),
                inbound: inbound_tx,
                retry_hint_ms: AtomicU64::new(0),
            }),
            inbound_rx,
            tasks: JoinSet::new(),
            listening: false,
        })
    }

    /// Sends one frame as its own POST. The POST runs as a task so slow
    /// upstream processing never serializes unrelated requests.
    pub async fn send(&mut self, frame: Vec<u8>, ctx: SendContext) -> Result<(), TransportError> {
        // Reap completed POST task handles so the set stays bounded.
        while self.tasks.try_join_next().is_some() {}
        let shared = Arc::clone(&self.shared);
        self.tasks.spawn(post_task(shared, frame, ctx));
        Ok(())
    }

    /// Sends one frame and awaits the server's acceptance of it — the
    /// ordering barrier for lifecycle-sensitive frames (the
    /// `initialized` notification MUST reach the server before any
    /// subsequent request). Failure is returned to the caller instead
    /// of being logged away.
    pub async fn send_ordered(
        &mut self,
        frame: Vec<u8>,
        ctx: SendContext,
    ) -> Result<(), TransportError> {
        match run_post(&self.shared, frame, ctx).await {
            Ok(()) => Ok(()),
            Err(HttpIssue::SessionExpired) => Err(TransportError::SessionExpired),
            Err(HttpIssue::Failed(reason)) => Err(TransportError::Http { reason }),
        }
    }

    /// Receives the next inbound item. This never reports a clean end
    /// of stream: an HTTP connection has no EOF — it ends by session
    /// teardown or a fatal error.
    pub async fn recv(&mut self) -> Result<Option<Received>, TransportError> {
        match self.inbound_rx.recv().await {
            Some(Inbound::Frame(frame)) => Ok(Some(Received::Frame(frame))),
            Some(Inbound::Dropped(reason)) => Ok(Some(Received::Dropped(reason))),
            Some(Inbound::Fatal(err)) => Err(err),
            None => Ok(None),
        }
    }

    /// Records the negotiated protocol version for the
    /// `MCP-Protocol-Version` header on all subsequent requests.
    pub fn set_protocol_version(&mut self, version: &str) {
        match HeaderValue::try_from(version) {
            Ok(value) => *lock(&self.shared.protocol_version) = Some(value),
            // Unreachable: negotiated versions come from the proxy's
            // own supported-version list.
            Err(_) => warn!(
                version,
                "negotiated version is not header-safe; not advertising it"
            ),
        }
    }

    /// Opens the long-lived GET stream for unsolicited server messages.
    /// Idempotent.
    pub fn start_listening(&mut self) {
        if !self.listening {
            self.listening = true;
            self.tasks.spawn(listen_task(Arc::clone(&self.shared)));
        }
    }

    /// Aborts transport tasks and best-effort terminates the session
    /// (`DELETE`; a 405 from servers that keep sessions is fine).
    pub async fn close(&mut self) {
        self.tasks.abort_all();
        let session = lock(&self.shared.session).clone();
        if let Some(session) = session {
            let mut headers = self.shared.extra.clone();
            headers.insert(mcp_session_id(), session);
            if let Some(version) = lock(&self.shared.protocol_version).clone() {
                headers.insert(mcp_protocol_version(), version);
            }
            let request = self
                .shared
                .client
                .delete(self.shared.url.clone())
                .headers(headers)
                .send();
            match tokio::time::timeout(DELETE_TIMEOUT, request).await {
                Ok(Ok(resp)) => debug!(status = %resp.status(), "session DELETE sent"),
                Ok(Err(err)) => debug!(error = %err, "session DELETE failed"),
                Err(_) => debug!("session DELETE timed out"),
            }
        }
    }
}

fn mcp_session_id() -> HeaderName {
    HeaderName::from_static("mcp-session-id")
}

fn mcp_protocol_version() -> HeaderName {
    HeaderName::from_static("mcp-protocol-version")
}

/// Locks a mutex, recovering from (bug-only) poisoning.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Headers common to every request: operator extras first, then the
/// proxy-controlled ones so config cannot shadow protocol headers.
fn base_headers(shared: &Shared) -> HeaderMap {
    let mut headers = shared.extra.clone();
    if let Some(session) = lock(&shared.session).clone() {
        headers.insert(mcp_session_id(), session);
    }
    if let Some(version) = lock(&shared.protocol_version).clone() {
        headers.insert(mcp_protocol_version(), version);
    }
    headers
}

fn capture_session(shared: &Shared, resp: &reqwest::Response) {
    let mut slot = lock(&shared.session);
    if slot.is_none() {
        if let Some(value) = resp.headers().get(mcp_session_id()) {
            let mut value = value.clone();
            value.set_sensitive(true);
            info!(upstream = %shared.label, "upstream assigned an MCP session id");
            *slot = Some(value);
        }
    }
}

fn has_session(shared: &Shared) -> bool {
    lock(&shared.session).is_some()
}

/// The `Content-Type` essence (before any `;`), lowercased.
fn content_type(resp: &reqwest::Response) -> Option<String> {
    resp.headers().get(CONTENT_TYPE)?.to_str().ok().map(|v| {
        v.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    })
}

/// Whether `frame` is a JSON-RPC response answering the proxy-minted
/// integer id `upstream_id`.
fn answers(frame: &[u8], upstream_id: u64) -> bool {
    matches!(
        envelope::parse(frame),
        Ok(Message::Response {
            id: ResponseId::Id(RequestId::Number(n)),
            ..
        }) if n >= 0 && n as u64 == upstream_id
    )
}

async fn push(shared: &Shared, item: Inbound) {
    if shared.inbound.send(item).await.is_err() {
        debug!(upstream = %shared.label, "transport reader gone; dropping inbound item");
    }
}

/// Runs one POST to completion, then resolves its failure mode.
async fn post_task(shared: Arc<Shared>, frame: Vec<u8>, ctx: SendContext) {
    match run_post(&shared, frame, ctx).await {
        Ok(()) => {}
        Err(HttpIssue::SessionExpired) => {
            warn!(upstream = %shared.label, "upstream ended the MCP session (404)");
            push(&shared, Inbound::Fatal(TransportError::SessionExpired)).await;
        }
        Err(HttpIssue::Failed(reason)) => {
            warn!(upstream = %shared.label, %reason, "upstream POST failed");
            if let SendContext::Request { upstream_id } = ctx {
                let synth = builder::error_frame(
                    &upstream_id.to_string(),
                    code::INTERNAL_ERROR,
                    UPSTREAM_FAILED_MSG,
                );
                push(&shared, Inbound::Frame(synth)).await;
            }
        }
    }
}

async fn run_post(shared: &Shared, frame: Vec<u8>, ctx: SendContext) -> Result<(), HttpIssue> {
    let mut headers = base_headers(shared);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    let resp = shared
        .client
        .post(shared.url.clone())
        .headers(headers)
        .body(frame)
        .send()
        .await
        .map_err(|err| HttpIssue::Failed(err.without_url().to_string()))?;

    capture_session(shared, &resp);
    let status = resp.status();
    if status == StatusCode::NOT_FOUND && has_session(shared) {
        return Err(HttpIssue::SessionExpired);
    }
    if status == StatusCode::ACCEPTED {
        return Ok(());
    }
    if !status.is_success() {
        return Err(HttpIssue::Failed(format!("HTTP {status}")));
    }

    match content_type(&resp).as_deref() {
        Some("application/json") => {
            let Some(body) = read_capped(shared, resp).await? else {
                push(
                    shared,
                    Inbound::Dropped(DropReason::Oversized {
                        limit: shared.max_frame,
                    }),
                )
                .await;
                return Err(HttpIssue::Failed("response body exceeded frame cap".into()));
            };
            let Ok(body) = normalize_newlines(body) else {
                // A raw newline inside a string literal: never valid
                // JSON; refusing beats silently repairing.
                push(shared, Inbound::Dropped(DropReason::InvalidEncoding)).await;
                return Err(HttpIssue::Failed(
                    "response body has raw newlines inside strings".into(),
                ));
            };
            let answered = match ctx {
                SendContext::Request { upstream_id } => answers(&body, upstream_id),
                SendContext::FireAndForget => true,
            };
            push(shared, Inbound::Frame(body)).await;
            if answered {
                Ok(())
            } else {
                Err(HttpIssue::Failed(
                    "JSON body did not answer the request".into(),
                ))
            }
        }
        Some("text/event-stream") => {
            let answered = read_sse(shared, resp).await?;
            match ctx {
                SendContext::Request { upstream_id } if !answered.contains(&upstream_id) => Err(
                    HttpIssue::Failed("SSE stream ended before the response".into()),
                ),
                _ => Ok(()),
            }
        }
        other => Err(HttpIssue::Failed(format!(
            "unexpected content-type {other:?}"
        ))),
    }
}

/// Reads a response body with the frame cap; `None` means oversized.
async fn read_capped(
    shared: &Shared,
    resp: reqwest::Response,
) -> Result<Option<Vec<u8>>, HttpIssue> {
    let mut body: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| HttpIssue::Failed(err.to_string()))?;
        if body.len() + chunk.len() > shared.max_frame {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

/// Drains one SSE body, pushing every default-type event as a frame.
/// Returns the set of proxy-minted ids the stream answered.
async fn read_sse(
    shared: &Shared,
    resp: reqwest::Response,
) -> Result<std::collections::HashSet<u64>, HttpIssue> {
    let mut answered = std::collections::HashSet::new();
    let mut parser = SseParser::new(shared.max_frame);
    let mut stream = resp.bytes_stream();
    loop {
        let chunk = match stream.next().await {
            Some(Ok(chunk)) => chunk,
            Some(Err(err)) => return Err(HttpIssue::Failed(err.without_url().to_string())),
            None => break,
        };
        for item in parser.push(&chunk) {
            handle_sse_item(shared, item, &mut answered).await;
        }
    }
    if parser.finish() {
        debug!(upstream = %shared.label, "SSE stream ended mid-event; partial event discarded");
    }
    Ok(answered)
}

async fn handle_sse_item(
    shared: &Shared,
    item: SseItem,
    answered: &mut std::collections::HashSet<u64>,
) {
    match item {
        // An absent event type and the explicit default type are the
        // same thing in SSE.
        SseItem::Event { event_type, data }
            if event_type.is_none() || event_type.as_deref() == Some("message") =>
        {
            let Ok(frame) = normalize_newlines(data.into_bytes()) else {
                debug!(upstream = %shared.label, "SSE event has raw newlines inside strings; dropped");
                push(shared, Inbound::Dropped(DropReason::InvalidEncoding)).await;
                return;
            };
            if let Ok(Message::Response {
                id: ResponseId::Id(RequestId::Number(n)),
                ..
            }) = envelope::parse(&frame)
            {
                if n >= 0 {
                    answered.insert(n as u64);
                }
            }
            push(shared, Inbound::Frame(frame)).await;
        }
        SseItem::Event { event_type, .. } => {
            let event_type = event_type.unwrap_or_default();
            debug!(upstream = %shared.label, event_type, "ignoring non-message SSE event");
            push(
                shared,
                Inbound::Dropped(DropReason::UnexpectedSseEvent { event_type }),
            )
            .await;
        }
        SseItem::Oversized => {
            push(
                shared,
                Inbound::Dropped(DropReason::Oversized {
                    limit: shared.max_frame,
                }),
            )
            .await;
        }
        SseItem::InvalidUtf8 => {
            push(shared, Inbound::Dropped(DropReason::InvalidEncoding)).await;
        }
        SseItem::Retry(delay) => {
            shared.retry_hint_ms.store(
                delay.as_millis().min(u128::from(u64::MAX)) as u64,
                Ordering::Relaxed,
            );
        }
    }
}

/// The long-lived GET listening loop: reconnects with backoff, honours
/// `retry` hints, stops on 405 (server offers no stream), and reports
/// session expiry as fatal.
async fn listen_task(shared: Arc<Shared>) {
    let mut delay = RECONNECT_MIN;
    let mut reported = false;
    loop {
        match run_get(&shared).await {
            Ok(had_events) => {
                if had_events {
                    delay = RECONNECT_MIN;
                }
                debug!(upstream = %shared.label, "GET stream ended; reconnecting");
            }
            Err(GetEnd::NotSupported(reason)) => {
                info!(upstream = %shared.label, %reason, "upstream offers no GET stream");
                return;
            }
            Err(GetEnd::SessionExpired) => {
                push(&shared, Inbound::Fatal(TransportError::SessionExpired)).await;
                return;
            }
            Err(GetEnd::Failed(reason)) => {
                if !reported {
                    warn!(upstream = %shared.label, %reason, "GET stream failed; will keep retrying");
                    reported = true;
                } else {
                    debug!(upstream = %shared.label, %reason, "GET stream failed again");
                }
            }
        }
        let hint = shared.retry_hint_ms.swap(0, Ordering::Relaxed);
        let wait = if hint > 0 {
            Duration::from_millis(hint).clamp(RECONNECT_MIN, RECONNECT_MAX)
        } else {
            delay
        };
        tokio::time::sleep(wait).await;
        delay = (delay * 2).min(RECONNECT_MAX);
    }
}

enum GetEnd {
    /// The server does not offer a GET stream; stop asking.
    NotSupported(String),
    /// 404 under a session id: the session is gone.
    SessionExpired,
    /// Transient failure; retry with backoff.
    Failed(String),
}

/// One GET attempt. `Ok(had_events)` when the stream ended normally.
async fn run_get(shared: &Shared) -> Result<bool, GetEnd> {
    let mut headers = base_headers(shared);
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    let resp = shared
        .client
        .get(shared.url.clone())
        .headers(headers)
        .send()
        .await
        .map_err(|err| GetEnd::Failed(err.without_url().to_string()))?;

    let status = resp.status();
    if status == StatusCode::NOT_FOUND && has_session(shared) {
        return Err(GetEnd::SessionExpired);
    }
    // The spec reserves exactly one "no stream offered" signal: 405.
    // (A 404 without session management gets the same treatment — an
    // endpoint that has no GET route.) Everything else, 4xx included,
    // may be transient — a rate limiter, a gateway hiccup — and
    // permanently abandoning the stream over it would silently lose
    // every future list_changed; retry with backoff instead.
    if status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::NOT_FOUND {
        return Err(GetEnd::NotSupported(format!("HTTP {status}")));
    }
    if !status.is_success() {
        return Err(GetEnd::Failed(format!("HTTP {status}")));
    }
    if content_type(&resp).as_deref() != Some("text/event-stream") {
        return Err(GetEnd::NotSupported(
            "GET did not return an event stream".into(),
        ));
    }

    let mut answered = std::collections::HashSet::new();
    let mut had_events = false;
    let mut parser = SseParser::new(shared.max_frame);
    let mut stream = resp.bytes_stream();
    loop {
        let chunk = match stream.next().await {
            Some(Ok(chunk)) => chunk,
            Some(Err(err)) => return Err(GetEnd::Failed(err.without_url().to_string())),
            None => break,
        };
        for item in parser.push(&chunk) {
            had_events = true;
            handle_sse_item(shared, item, &mut answered).await;
        }
    }
    let _ = parser.finish();
    Ok(had_events)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn setup_error(url: &str, headers: &[(String, String)]) -> HttpSetupError {
        match HttpTransport::new("u", url, headers, 1024) {
            Err(err) => err,
            Ok(_) => panic!("expected setup to fail"),
        }
    }

    #[test]
    fn setup_rejects_bad_urls_and_headers_without_leaking_values() {
        assert!(matches!(
            setup_error("ftp://x/", &[]),
            HttpSetupError::BadUrl { .. }
        ));
        assert!(matches!(
            setup_error("not a url", &[]),
            HttpSetupError::BadUrl { .. }
        ));
        assert!(matches!(
            setup_error(
                "https://example.com/mcp",
                &[("bad name".to_owned(), "v".to_owned())]
            ),
            HttpSetupError::BadHeaderName { .. }
        ));

        let err = setup_error(
            "https://example.com/mcp",
            &[("Authorization".to_owned(), "secret\nvalue".to_owned())],
        );
        let shown = format!("{err} {err:?}");
        assert!(matches!(err, HttpSetupError::BadHeaderValue { .. }));
        assert!(!shown.contains("secret"), "leaked header value: {shown}");
    }

    #[test]
    fn answers_matches_only_the_minted_numeric_id() {
        assert!(answers(br#"{"jsonrpc":"2.0","id":7,"result":{}}"#, 7));
        assert!(answers(
            br#"{"jsonrpc":"2.0","id":7,"error":{"code":1,"message":"m"}}"#,
            7
        ));
        assert!(!answers(br#"{"jsonrpc":"2.0","id":8,"result":{}}"#, 7));
        assert!(!answers(br#"{"jsonrpc":"2.0","id":"7","result":{}}"#, 7));
        assert!(!answers(br#"{"jsonrpc":"2.0","id":-1,"result":{}}"#, 7));
        assert!(!answers(br#"{"jsonrpc":"2.0","id":7,"method":"m"}"#, 7));
        assert!(!answers(b"not json", 7));
    }
}
