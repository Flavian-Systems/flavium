//! The transparent proxy core: one MCP client on one side, one upstream
//! MCP server on the other, bytes forwarded faithfully in both
//! directions.
//!
//! Every frame crosses a typed parse boundary before it is forwarded:
//! a frame from the client that does not parse as a single well-formed
//! JSON-RPC 2.0 object is answered with a JSON-RPC error and never
//! reaches the upstream; such a frame from the upstream is dropped and
//! never reaches the client. Frames that parse are forwarded as their
//! original bytes — the proxy never re-serializes, so unknown methods,
//! notifications, `_meta`, and unmodeled fields pass through unchanged.
//!
//! The proxy observes (but does not answer) the MCP `initialize`
//! handshake and records the negotiated protocol version in the session
//! summary and the log. Enforcement, tracing, and multi-upstream routing
//! arrive with later T1 milestones.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::envelope::{self, Message, RequestId, ResponseId};
use crate::framing::{write_frame, FrameReadError, FrameReader, DEFAULT_MAX_FRAME_BYTES};

/// How many client-bound frames may queue before the upstream pump
/// backpressures.
const CLIENT_QUEUE_FRAMES: usize = 64;

/// Proxy tuning knobs.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Per-frame size cap, both directions.
    pub max_frame_bytes: usize,
    /// Once one side of the session has ended, how long the other side's
    /// pump may keep draining before it is abandoned.
    pub shutdown_grace: Duration,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            shutdown_grace: Duration::from_secs(5),
        }
    }
}

/// Why the client→upstream pump stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientPumpEnd {
    /// The client closed its input — the normal MCP shutdown path.
    Eof,
    /// Reading from the client failed.
    ReadError,
    /// Writing to the upstream failed (the upstream likely exited).
    UpstreamWriteError,
    /// The upstream side ended first and the client pump was still
    /// blocked on client input after the grace period; abandoned.
    Abandoned,
}

/// Why the upstream→client pump stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamPumpEnd {
    /// The upstream closed its output.
    Eof,
    /// Reading from the upstream failed.
    ReadError,
    /// The client-side writer was gone.
    ClientGone,
    /// Still draining when the post-shutdown grace period expired;
    /// abandoned.
    Abandoned,
}

/// What the proxy observed of the MCP `initialize` handshake.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Handshake {
    /// `protocolVersion` offered by the client's `initialize` request.
    pub offered_protocol_version: Option<String>,
    /// `protocolVersion` in the upstream's `initialize` result — the
    /// version the session actually runs.
    pub negotiated_protocol_version: Option<String>,
    /// `clientInfo.name` from the `initialize` request.
    pub client_name: Option<String>,
    /// `clientInfo.version` from the `initialize` request.
    pub client_version: Option<String>,
    /// `serverInfo.name` from the `initialize` result.
    pub server_name: Option<String>,
    /// `serverInfo.version` from the `initialize` result.
    pub server_version: Option<String>,
}

/// End-of-session accounting returned by [`run`].
#[derive(Debug, Clone)]
pub struct ProxySummary {
    /// What the proxy observed of the `initialize` handshake.
    pub handshake: Handshake,
    /// Why the client→upstream pump stopped.
    pub client_end: ClientPumpEnd,
    /// Why the upstream→client pump stopped.
    pub upstream_end: UpstreamPumpEnd,
    /// True when writing to the client failed (or stalled past the
    /// grace period): frames were accepted but could not be delivered.
    pub client_delivery_failed: bool,
    /// Frames forwarded client→upstream.
    pub frames_to_upstream: u64,
    /// Frames delivered upstream→client (proxy-origin error replies
    /// included).
    pub frames_to_client: u64,
    /// Client-bound frames accepted but discarded because delivery to
    /// the client failed.
    pub frames_undelivered: u64,
    /// Client frames rejected at the parse boundary (answered with a
    /// JSON-RPC error, never forwarded).
    pub client_frames_rejected: u64,
    /// Upstream frames dropped at the parse boundary (never forwarded).
    pub upstream_frames_dropped: u64,
}

impl ProxySummary {
    /// True when the session ended by the client cleanly closing its
    /// input *and* every accepted client-bound frame was delivered —
    /// the only shutdown the proxy treats as success.
    pub fn clean_shutdown(&self) -> bool {
        self.client_end == ClientPumpEnd::Eof && !self.client_delivery_failed
    }
}

/// Errors from [`run`] itself. Protocol-level trouble never surfaces
/// here — it is answered, dropped, or recorded in the summary.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// An internal pump task panicked — a bug, not a protocol condition.
    #[error("internal proxy task failed")]
    TaskFailed,
}

#[derive(Default)]
struct Counters {
    to_upstream: AtomicU64,
    to_client: AtomicU64,
    undelivered: AtomicU64,
    rejected: AtomicU64,
    dropped: AtomicU64,
}

/// How the client-side writer task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriterEnd {
    /// The queue drained and the client output was closed normally.
    Drained,
    /// A write to the client failed, or the writer stalled past the
    /// grace period; queued frames were discarded.
    WriteError,
}

#[derive(Default)]
struct HandshakeObserver {
    init_request_id: Option<RequestId>,
    handshake: Handshake,
}

/// Locks a mutex, recovering the guard even if a (bug-only) panic in
/// another task poisoned it — the observer state stays best-effort.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Runs one proxied MCP session over the four given byte streams and
/// returns when both directions have ended (or been abandoned after the
/// configured grace period).
///
/// The transports are generic so tests can drive the proxy over
/// in-memory pipes; production wiring lives in [`crate::stdio`].
pub async fn run<CR, CW, UR, UW>(
    config: ProxyConfig,
    client_rx: CR,
    client_tx: CW,
    upstream_rx: UR,
    upstream_tx: UW,
) -> Result<ProxySummary, ProxyError>
where
    CR: AsyncRead + Unpin + Send + 'static,
    CW: AsyncWrite + Unpin + Send + 'static,
    UR: AsyncRead + Unpin + Send + 'static,
    UW: AsyncWrite + Unpin + Send + 'static,
{
    let observer = Arc::new(Mutex::new(HandshakeObserver::default()));
    let counters = Arc::new(Counters::default());
    let (to_client, from_pumps) = mpsc::channel::<Vec<u8>>(CLIENT_QUEUE_FRAMES);

    let mut writer = tokio::spawn(client_writer(client_tx, from_pumps, Arc::clone(&counters)));
    let mut c2u = tokio::spawn(client_to_upstream(
        config.max_frame_bytes,
        client_rx,
        upstream_tx,
        to_client.clone(),
        Arc::clone(&observer),
        Arc::clone(&counters),
    ));
    let mut u2c = tokio::spawn(upstream_to_client(
        config.max_frame_bytes,
        upstream_rx,
        to_client,
        Arc::clone(&observer),
        Arc::clone(&counters),
    ));

    // Whichever task ends first decides the shutdown shape; the others
    // get the grace period to drain, then are abandoned. The writer is
    // a first-class shutdown signal: while either pump lives it holds a
    // channel sender, so the writer ending early means the client write
    // path is dead and the session cannot continue.
    let grace = config.shutdown_grace;
    let (client_end, upstream_end, writer_end) = tokio::select! {
        joined = &mut c2u => {
            let client_end = joined.map_err(|_| ProxyError::TaskFailed)?;
            let upstream_end =
                join_with_grace(&mut u2c, grace, UpstreamPumpEnd::Abandoned).await?;
            // Both senders are now dropped, so the writer drains to
            // completion — unless the client stopped reading, in which
            // case it is abandoned and the session was not delivered.
            let writer_end =
                join_with_grace(&mut writer, grace, WriterEnd::WriteError).await?;
            (client_end, upstream_end, writer_end)
        }
        joined = &mut u2c => {
            let upstream_end = joined.map_err(|_| ProxyError::TaskFailed)?;
            let client_end =
                join_with_grace(&mut c2u, grace, ClientPumpEnd::Abandoned).await?;
            let writer_end =
                join_with_grace(&mut writer, grace, WriterEnd::WriteError).await?;
            (client_end, upstream_end, writer_end)
        }
        joined = &mut writer => {
            let writer_end = joined.map_err(|_| ProxyError::TaskFailed)?;
            if writer_end == WriterEnd::WriteError {
                warn!("client write path is dead; shutting the session down");
            }
            let client_end =
                join_with_grace(&mut c2u, grace, ClientPumpEnd::Abandoned).await?;
            let upstream_end =
                join_with_grace(&mut u2c, grace, UpstreamPumpEnd::Abandoned).await?;
            (client_end, upstream_end, writer_end)
        }
    };

    let handshake = lock(&observer).handshake.clone();
    let summary = ProxySummary {
        handshake,
        client_end,
        upstream_end,
        client_delivery_failed: writer_end == WriterEnd::WriteError,
        frames_to_upstream: counters.to_upstream.load(Ordering::Relaxed),
        frames_to_client: counters.to_client.load(Ordering::Relaxed),
        frames_undelivered: counters.undelivered.load(Ordering::Relaxed),
        client_frames_rejected: counters.rejected.load(Ordering::Relaxed),
        upstream_frames_dropped: counters.dropped.load(Ordering::Relaxed),
    };
    info!(
        client_end = ?summary.client_end,
        upstream_end = ?summary.upstream_end,
        delivery_failed = summary.client_delivery_failed,
        frames_to_upstream = summary.frames_to_upstream,
        frames_to_client = summary.frames_to_client,
        undelivered = summary.frames_undelivered,
        rejected = summary.client_frames_rejected,
        dropped = summary.upstream_frames_dropped,
        "session ended"
    );
    Ok(summary)
}

/// Joins a proxy task, aborting it if it outlives the grace period.
async fn join_with_grace<T>(
    handle: &mut JoinHandle<T>,
    grace: Duration,
    abandoned: T,
) -> Result<T, ProxyError> {
    match tokio::time::timeout(grace, &mut *handle).await {
        Ok(Ok(end)) => Ok(end),
        Ok(Err(_)) => Err(ProxyError::TaskFailed),
        Err(_) => {
            handle.abort();
            // The task may have completed between the timeout and the
            // abort; keep its real result rather than fabricating one.
            match (&mut *handle).await {
                Ok(end) => Ok(end),
                Err(err) if err.is_cancelled() => {
                    warn!("proxy task still running after grace period; abandoning it");
                    Ok(abandoned)
                }
                Err(_) => Err(ProxyError::TaskFailed),
            }
        }
    }
}

async fn client_to_upstream<R, W>(
    max_frame: usize,
    client_rx: R,
    mut upstream_tx: W,
    to_client: mpsc::Sender<Vec<u8>>,
    observer: Arc<Mutex<HandshakeObserver>>,
    counters: Arc<Counters>,
) -> ClientPumpEnd
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = FrameReader::new(client_rx, max_frame);
    let end = loop {
        match reader.read_frame().await {
            Ok(None) => break ClientPumpEnd::Eof,
            Ok(Some(frame)) => {
                if is_blank(&frame) {
                    debug!("skipping blank frame from client");
                    continue;
                }
                match envelope::parse(&frame) {
                    Ok(message) => {
                        observe_client_message(&observer, &message);
                        if let Err(err) = write_frame(&mut upstream_tx, &frame).await {
                            warn!(error = %err, "failed to write to upstream; shutting down");
                            break ClientPumpEnd::UpstreamWriteError;
                        }
                        counters.to_upstream.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(err) => {
                        counters.rejected.fetch_add(1, Ordering::Relaxed);
                        warn!(error = %err, "rejecting unparseable client frame");
                        reply_error(&to_client, err.is_parse_error()).await;
                    }
                }
            }
            Err(FrameReadError::Oversized { limit }) => {
                counters.rejected.fetch_add(1, Ordering::Relaxed);
                warn!(limit, "rejecting oversized client frame");
                reply_error(&to_client, true).await;
            }
            Err(FrameReadError::Io(err)) => {
                warn!(error = %err, "failed to read from client");
                break ClientPumpEnd::ReadError;
            }
        }
    };
    // Closing the upstream's input is the MCP stdio shutdown signal.
    let _ = upstream_tx.shutdown().await;
    end
}

async fn upstream_to_client<R>(
    max_frame: usize,
    upstream_rx: R,
    to_client: mpsc::Sender<Vec<u8>>,
    observer: Arc<Mutex<HandshakeObserver>>,
    counters: Arc<Counters>,
) -> UpstreamPumpEnd
where
    R: AsyncRead + Unpin,
{
    let mut reader = FrameReader::new(upstream_rx, max_frame);
    loop {
        match reader.read_frame().await {
            Ok(None) => return UpstreamPumpEnd::Eof,
            Ok(Some(frame)) => {
                if is_blank(&frame) {
                    debug!("skipping blank frame from upstream");
                    continue;
                }
                match envelope::parse(&frame) {
                    Ok(message) => {
                        observe_upstream_message(&observer, &message);
                        if to_client.send(frame).await.is_err() {
                            return UpstreamPumpEnd::ClientGone;
                        }
                    }
                    Err(err) => {
                        counters.dropped.fetch_add(1, Ordering::Relaxed);
                        warn!(error = %err, "dropping unparseable upstream frame");
                    }
                }
            }
            Err(FrameReadError::Oversized { limit }) => {
                counters.dropped.fetch_add(1, Ordering::Relaxed);
                warn!(limit, "dropping oversized upstream frame");
            }
            Err(FrameReadError::Io(err)) => {
                warn!(error = %err, "failed to read from upstream");
                return UpstreamPumpEnd::ReadError;
            }
        }
    }
}

/// Owns the client-side writer: everything client-bound funnels through
/// one task so forwarded frames and proxy-origin replies never
/// interleave mid-frame. Returns [`WriterEnd::WriteError`] when the
/// client write path died — the signal [`run`] uses to end the session.
async fn client_writer<W>(
    mut client_tx: W,
    mut from_pumps: mpsc::Receiver<Vec<u8>>,
    counters: Arc<Counters>,
) -> WriterEnd
where
    W: AsyncWrite + Unpin,
{
    let mut end = WriterEnd::Drained;
    while let Some(frame) = from_pumps.recv().await {
        match write_frame(&mut client_tx, &frame).await {
            Ok(()) => {
                counters.to_client.fetch_add(1, Ordering::Relaxed);
            }
            Err(err) => {
                warn!(error = %err, "failed to write to client; discarding remaining output");
                end = WriterEnd::WriteError;
                // Account for the failed frame and everything queued
                // behind it: accepted but never delivered.
                counters.undelivered.fetch_add(1, Ordering::Relaxed);
                from_pumps.close();
                while from_pumps.recv().await.is_some() {
                    counters.undelivered.fetch_add(1, Ordering::Relaxed);
                }
                break;
            }
        }
    }
    let _ = client_tx.shutdown().await;
    end
}

/// Sends a proxy-origin JSON-RPC error to the client. The wording is
/// fixed and generic: rejected input is never echoed back.
async fn reply_error(to_client: &mpsc::Sender<Vec<u8>>, parse_error: bool) {
    let (code, message) = if parse_error {
        (-32700, "Parse error")
    } else {
        (-32600, "Invalid Request")
    };
    let reply = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": code, "message": message }
    }))
    .unwrap_or_else(|_| FALLBACK_ERROR_REPLY.to_vec());
    if to_client.send(reply).await.is_err() {
        debug!("client writer gone while replying with an error");
    }
}

/// Used only if serializing the ordinary error reply somehow fails.
const FALLBACK_ERROR_REPLY: &[u8] =
    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"Internal error"}}"#;

fn is_blank(frame: &[u8]) -> bool {
    frame.iter().all(u8::is_ascii_whitespace)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    protocol_version: Option<String>,
    client_info: Option<PeerInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeResult {
    protocol_version: Option<String>,
    server_info: Option<PeerInfo>,
}

#[derive(Deserialize)]
struct PeerInfo {
    name: Option<String>,
    version: Option<String>,
}

/// Records the client's `initialize` request. Observation is
/// best-effort and never blocks forwarding: unparseable params are
/// logged and ignored.
fn observe_client_message(observer: &Mutex<HandshakeObserver>, message: &Message<'_>) {
    let Message::Request { id, method, params } = message else {
        return;
    };
    if method != "initialize" {
        debug!(method, "forwarding client request");
        return;
    }
    let parsed: InitializeParams = params
        .and_then(|p| serde_json::from_str(p.get()).ok())
        .unwrap_or_default();
    let mut guard = lock(observer);
    guard.init_request_id = Some(id.clone());
    guard.handshake.offered_protocol_version = parsed.protocol_version;
    if let Some(info) = parsed.client_info {
        guard.handshake.client_name = info.name;
        guard.handshake.client_version = info.version;
    }
    info!(
        offered_protocol_version = guard.handshake.offered_protocol_version.as_deref(),
        client_name = guard.handshake.client_name.as_deref(),
        client_version = guard.handshake.client_version.as_deref(),
        "observed initialize request"
    );
}

/// Records the upstream's `initialize` result, completing the observed
/// handshake — this is where the negotiated protocol version is pinned
/// down and logged.
fn observe_upstream_message(observer: &Mutex<HandshakeObserver>, message: &Message<'_>) {
    let Message::Response {
        id: ResponseId::Id(id),
        result: Some(result),
        ..
    } = message
    else {
        return;
    };
    let mut guard = lock(observer);
    if guard.init_request_id.as_ref() != Some(id)
        || guard.handshake.negotiated_protocol_version.is_some()
    {
        return;
    }
    let Ok(parsed) = serde_json::from_str::<InitializeResult>(result.get()) else {
        debug!("initialize result did not parse; handshake left unrecorded");
        return;
    };
    guard.handshake.negotiated_protocol_version = parsed.protocol_version;
    if let Some(info) = parsed.server_info {
        guard.handshake.server_name = info.name;
        guard.handshake.server_version = info.version;
    }
    info!(
        negotiated_protocol_version = guard.handshake.negotiated_protocol_version.as_deref(),
        server_name = guard.handshake.server_name.as_deref(),
        server_version = guard.handshake.server_version.as_deref(),
        "MCP handshake complete"
    );
}
