//! The session router: the proxy's MCP *server* face and the seam where
//! everything meets.
//!
//! Where M1 forwarded a session transparently, M2 terminates the
//! protocol: the router answers the client's `initialize` itself
//! (offering [`protocol::OFFERED_VERSION`], echoing a supported offer),
//! advertises exactly the capabilities the proxy honors
//! (`tools.listChanged` — nothing else), initializes every configured
//! upstream separately, merges their tool lists (pagination drained
//! internally, duplicate names rejected), and routes `tools/call` by
//! tool name. `params`/`result` bodies still cross the proxy
//! byte-faithfully; only ids are rewritten.
//!
//! Denial and error surface (T1 plan, "Denial surface"):
//! - a tool no upstream offers ⇒ `-32602`, the same shape a granted-but
//!   -denied tool will produce in M5;
//! - a `tools/list` cursor ⇒ `-32602` — the proxy never mints cursors,
//!   so any cursor is foreign;
//! - unknown methods ⇒ `-32601`: with multiple upstreams there is no
//!   faithful forwarding target, and the client was told exactly what
//!   this server can do;
//! - requests before `initialize` completes ⇒ `-32002`;
//! - a client id already in flight ⇒ `-32600`.
//!
//! Failure policy, stated plainly: any upstream ending — process exit,
//! pipe death, write stall, HTTP session expiry — ends the whole
//! session (abnormal exit). Supervision policies are T3 work; until
//! then the proxy prefers dying loudly over serving a session whose
//! tool surface silently shrank. A failed *re-list* after
//! `list_changed` keeps the previous table (availability may lag;
//! authority never depends on this table — grants gate calls in M5); a
//! re-list *collision* ends the session like a startup collision would.
//!
//! Backpressure policy: the serve loop never parks on an actor's
//! command queue (a saturated upstream answers `-32603 upstream busy`
//! instead — waiting would close a deadlock cycle between the command
//! and event queues), tool tables are byte-budgeted per upstream, and
//! everything client-bound funnels through one writer task.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, info, warn};

use crate::builder::{self, code};
use crate::config::ConfigError;
use crate::connection::{
    self, CallError, Command, ConnectError, Event, Handle, SendRefusal, UpstreamInfo,
};
use crate::envelope::{self, Message, RequestId};
use crate::framing::{write_frame, FrameReadError, FrameReader, DEFAULT_MAX_FRAME_BYTES};
use crate::idmap::ClientTable;
use crate::protocol;
use crate::splice;
use crate::toolset::{
    ListPage, ListPageError, ToolEntry, ToolSet, MAX_LIST_PAGES, MAX_TOOLS_PER_UPSTREAM,
};
use crate::transport::Transport;

/// Client-bound frames queued before backpressure.
const CLIENT_QUEUE_FRAMES: usize = 64;

/// Proxy tuning knobs.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Per-frame size cap, all transports, both directions.
    pub max_frame_bytes: usize,
    /// How long draining tasks get once the session is ending.
    pub shutdown_grace: Duration,
    /// Deadline for each upstream's initialize handshake.
    pub init_timeout: Duration,
    /// Deadline for each `tools/list` page.
    pub list_timeout: Duration,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            shutdown_grace: Duration::from_secs(5),
            // Generous: stdio upstreams are often `npx …` with a cold
            // cache.
            init_timeout: Duration::from_secs(60),
            list_timeout: Duration::from_secs(30),
        }
    }
}

/// One upstream ready to be connected: a name and an unopened transport.
pub struct PreparedUpstream {
    /// Operator-configured name (logs and errors only).
    pub name: String,
    /// The transport to run MCP over.
    pub transport: Transport,
}

/// Errors before the session starts serving the client.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    /// The upstream set was structurally invalid.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// An upstream failed its initialize handshake.
    #[error("upstream {name:?} failed to connect")]
    Connect {
        /// The upstream that failed.
        name: String,
        /// Why.
        #[source]
        source: ConnectError,
    },

    /// An upstream's tool list could not be drained.
    #[error("upstream {name:?} failed to list tools")]
    List {
        /// The upstream that failed.
        name: String,
        /// Why.
        #[source]
        source: ListError,
    },

    /// Two upstreams (or one, twice) claim the same tool name.
    #[error("tool {tool:?} is offered by both {first:?} and {second:?}")]
    Collision {
        /// The contested tool name.
        tool: String,
        /// Name of the upstream that declared it first.
        first: String,
        /// Name of the upstream that declared it again.
        second: String,
    },
}

/// Errors draining one upstream's `tools/list`.
#[derive(Debug, thiserror::Error)]
pub enum ListError {
    /// The request itself failed.
    #[error(transparent)]
    Call(#[from] CallError),

    /// A page did not parse.
    #[error("unparseable tools/list page")]
    Page(#[from] ListPageError),

    /// The upstream paged past [`MAX_LIST_PAGES`] or repeated a cursor.
    #[error("upstream pagination never converged")]
    RunawayPagination,

    /// The upstream declared more than [`MAX_TOOLS_PER_UPSTREAM`] tools.
    #[error("upstream declared too many tools")]
    TooManyTools,

    /// The upstream's tools exceed the byte budget (one frame's worth
    /// per upstream) — page and count caps alone would still admit
    /// gigabytes.
    #[error("upstream tool list exceeds the byte budget")]
    TooManyBytes,
}

/// Errors from [`run`] itself.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// Startup failed; the client was never served.
    #[error(transparent)]
    Startup(#[from] StartupError),

    /// An internal task panicked — a bug, not a protocol condition.
    #[error("internal proxy task failed")]
    TaskFailed,
}

/// What the router observed of the client-side handshake.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientHandshake {
    /// `protocolVersion` the client offered.
    pub offered_protocol_version: Option<String>,
    /// The version the proxy answered with — what the session runs.
    pub negotiated_protocol_version: Option<String>,
    /// `clientInfo.name`.
    pub client_name: Option<String>,
    /// `clientInfo.version`.
    pub client_version: Option<String>,
}

/// Why the session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnd {
    /// The client closed its input — the normal MCP shutdown.
    ClientEof,
    /// Reading from the client failed.
    ClientReadError,
    /// The client write path died.
    ClientWriteFailed,
    /// An upstream connection ended or failed (index into the
    /// configured upstreams).
    UpstreamGone {
        /// Which upstream.
        upstream: usize,
    },
    /// A re-listed upstream introduced a tool-name collision.
    ToolCollision {
        /// The contested name.
        tool: String,
    },
    /// An internal invariant broke (a task died without reporting); a
    /// bug, surfaced as an abnormal end rather than a hang.
    Internal,
}

/// End-of-session accounting returned by [`run`].
#[derive(Debug)]
pub struct SessionSummary {
    /// The client-side handshake as negotiated by the proxy.
    pub handshake: ClientHandshake,
    /// Everything the upstream handshakes learned, in config order.
    pub upstreams: Vec<UpstreamInfo>,
    /// Why the session ended.
    pub end: SessionEnd,
    /// True when client-bound frames were accepted but not delivered.
    pub client_delivery_failed: bool,
    /// Frames forwarded to upstreams (requests and cancellations).
    pub frames_to_upstream: u64,
    /// Frames delivered to the client (proxy-origin included).
    pub frames_to_client: u64,
    /// Client-bound frames accepted but discarded on a dead write path.
    pub frames_undelivered: u64,
    /// Client frames rejected at the parse boundary.
    pub client_frames_rejected: u64,
    /// Client frames the router consumed without forwarding or
    /// answering (unroutable notifications, stray responses).
    pub client_frames_discarded: u64,
}

impl SessionSummary {
    /// The only shutdown treated as success: client EOF with every
    /// accepted frame delivered.
    pub fn clean_shutdown(&self) -> bool {
        self.end == SessionEnd::ClientEof && !self.client_delivery_failed
    }
}

#[derive(Default)]
struct WriterCounters {
    to_client: AtomicU64,
    undelivered: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriterEnd {
    Drained,
    WriteError,
}

/// Session phase on the client face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for `initialize`.
    PreInit,
    /// `initialize` answered; waiting for `notifications/initialized`.
    Initializing,
    /// Normal operation.
    Ready,
}

/// Runs one proxied session: connects and lists every upstream, then
/// serves the client until one side ends the session.
///
/// Transports are generic so tests can drive the router over in-memory
/// pipes; production wiring lives in [`crate::stdio`].
pub async fn run<CR, CW>(
    config: ProxyConfig,
    upstreams: Vec<PreparedUpstream>,
    client_rx: CR,
    client_tx: CW,
) -> Result<SessionSummary, RunError>
where
    CR: AsyncRead + Unpin + Send + 'static,
    CW: AsyncWrite + Unpin + Send + 'static,
{
    let (events_tx, events_rx) = mpsc::channel::<Event>(CLIENT_QUEUE_FRAMES);

    // Phase 1+2: bring every upstream up and drain its tool list before
    // the client is served at all — collisions are refused at startup,
    // not discovered mid-session.
    let startup = startup(&config, upstreams, events_tx).await;
    let (handles, actor_joins, infos, toolset) = match startup {
        Ok(parts) => parts,
        Err(err) => return Err(RunError::Startup(err)),
    };

    info!(
        upstreams = infos.len(),
        tools = toolset.len(),
        "all upstreams initialized; serving the client"
    );

    // Phase 3: serve.
    let counters = Arc::new(WriterCounters::default());
    let (to_client, from_router) = mpsc::channel::<Vec<u8>>(CLIENT_QUEUE_FRAMES);
    let writer = tokio::spawn(client_writer(client_tx, from_router, Arc::clone(&counters)));

    let mut session = Session {
        config: config.clone(),
        handles,
        infos,
        toolset,
        phase: Phase::PreInit,
        handshake: ClientHandshake::default(),
        in_flight: ClientTable::default(),
        relists: JoinSet::new(),
        relist_running: Vec::new(),
        relist_dirty: Vec::new(),
        pending_list_changed: false,
        to_client,
        frames_to_upstream: 0,
        rejected: 0,
        discarded: 0,
    };
    session.relist_running.resize(session.handles.len(), false);
    session.relist_dirty.resize(session.handles.len(), false);

    let end = session.serve(client_rx, events_rx).await;

    // Teardown: ask every actor to close (children reaped, HTTP
    // sessions DELETEd), then give them — and the writer — the grace
    // period to finish. try_send: a wedged actor's queue may be full,
    // and waiting on it would let a dead upstream hold run() open
    // forever; the graced join + abort below bounds it regardless.
    for handle in &session.handles {
        let _ = handle.try_send(Command::Shutdown);
    }
    // Relist tasks hold Handle clones; abort them first so dropping the
    // router's handles actually closes every command channel — the
    // fallback shutdown signal for an actor whose queue was full.
    session.relists.abort_all();
    drop(session.handles);
    for join in actor_joins {
        join_with_grace(join, config.shutdown_grace).await;
    }
    drop(session.to_client);
    let mut writer = writer;
    let writer_end = match tokio::time::timeout(config.shutdown_grace, &mut writer).await {
        Ok(Ok(end)) => end,
        Ok(Err(_)) => return Err(RunError::TaskFailed),
        Err(_) => {
            // The client stopped reading and the writer is parked on
            // the dead pipe; abandon it rather than leak it.
            writer.abort();
            WriterEnd::WriteError
        }
    };

    let summary = SessionSummary {
        handshake: session.handshake,
        upstreams: session.infos,
        end,
        client_delivery_failed: writer_end == WriterEnd::WriteError,
        frames_to_upstream: session.frames_to_upstream,
        frames_to_client: counters.to_client.load(Ordering::Relaxed),
        frames_undelivered: counters.undelivered.load(Ordering::Relaxed),
        client_frames_rejected: session.rejected,
        client_frames_discarded: session.discarded,
    };
    info!(
        end = ?summary.end,
        delivery_failed = summary.client_delivery_failed,
        frames_to_upstream = summary.frames_to_upstream,
        frames_to_client = summary.frames_to_client,
        undelivered = summary.frames_undelivered,
        rejected = summary.client_frames_rejected,
        discarded = summary.client_frames_discarded,
        "session ended"
    );
    Ok(summary)
}

type StartupParts = (Vec<Handle>, Vec<JoinHandle<()>>, Vec<UpstreamInfo>, ToolSet);

async fn startup(
    config: &ProxyConfig,
    upstreams: Vec<PreparedUpstream>,
    events_tx: mpsc::Sender<Event>,
) -> Result<StartupParts, StartupError> {
    let names: Vec<String> = upstreams.iter().map(|u| u.name.clone()).collect();

    let connects = upstreams.into_iter().enumerate().map(|(index, upstream)| {
        let events = events_tx.clone();
        let init_timeout = config.init_timeout;
        async move {
            let name = upstream.name;
            connection::connect(index, &name, upstream.transport, init_timeout, events)
                .await
                .map_err(|source| StartupError::Connect { name, source })
        }
    });
    let results = futures_util::future::join_all(connects).await;

    let mut handles = Vec::new();
    let mut joins = Vec::new();
    let mut infos = Vec::new();
    let mut first_error = None;
    for result in results {
        match result {
            Ok((handle, join, info)) => {
                handles.push(handle);
                joins.push(join);
                infos.push(info);
            }
            Err(err) => first_error = first_error.or(Some(err)),
        }
    }
    if let Some(err) = first_error {
        shutdown_all(handles, joins, config.shutdown_grace).await;
        return Err(err);
    }

    let lists = handles.iter().zip(&infos).map(|(handle, info)| {
        let handle = handle.clone();
        let declared = info.tools_declared;
        let name = info.name.clone();
        let list_timeout = config.list_timeout;
        let byte_budget = config.max_frame_bytes;
        async move {
            if !declared {
                // An upstream without the tools capability contributes
                // nothing; the proxy still fronts it in case later
                // milestones speak more than tools.
                warn!(upstream = %name, "upstream declares no tools capability");
                return Ok(Vec::new());
            }
            drain_tools(&handle, list_timeout, byte_budget)
                .await
                .map_err(|source| StartupError::List { name, source })
        }
    });
    let pages = futures_util::future::join_all(lists).await;

    let mut per_upstream = Vec::new();
    let mut first_error = None;
    for page in pages {
        match page {
            Ok(tools) => per_upstream.push(tools),
            Err(err) => first_error = first_error.or(Some(err)),
        }
    }
    if let Some(err) = first_error {
        shutdown_all(handles, joins, config.shutdown_grace).await;
        return Err(err);
    }

    match ToolSet::build(per_upstream) {
        Ok(toolset) => Ok((handles, joins, infos, toolset)),
        Err(collision) => {
            let name = |index: usize| names.get(index).cloned().unwrap_or_default();
            let err = StartupError::Collision {
                tool: collision.name,
                first: name(collision.first),
                second: name(collision.second),
            };
            shutdown_all(handles, joins, config.shutdown_grace).await;
            Err(err)
        }
    }
}

async fn shutdown_all(handles: Vec<Handle>, joins: Vec<JoinHandle<()>>, grace: Duration) {
    for handle in &handles {
        let _ = handle.try_send(Command::Shutdown);
    }
    // Dropping the handles closes the command channels — the fallback
    // shutdown signal; the graced join + abort bounds a wedged actor.
    drop(handles);
    for join in joins {
        join_with_grace(join, grace).await;
    }
}

async fn join_with_grace(mut join: JoinHandle<()>, grace: Duration) {
    if tokio::time::timeout(grace, &mut join).await.is_err() {
        warn!("connection actor still running after grace period; aborting it");
        join.abort();
    }
}

/// Drains one upstream's full tool list through its pagination,
/// bounding pages, tool count, and total retained bytes.
async fn drain_tools(
    handle: &Handle,
    list_timeout: Duration,
    byte_budget: usize,
) -> Result<Vec<ToolEntry>, ListError> {
    let mut tools = Vec::new();
    let mut bytes = 0usize;
    let mut cursor: Option<String> = None;
    for _page in 0..MAX_LIST_PAGES {
        let params = cursor
            .as_ref()
            .map(|c| serde_json::json!({ "cursor": c }).to_string());
        let result = handle.call("tools/list", params, list_timeout).await?;
        let page = ListPage::parse(result.get())?;
        for tool in &page.tools {
            bytes = bytes.saturating_add(tool.raw.get().len());
        }
        if bytes > byte_budget {
            return Err(ListError::TooManyBytes);
        }
        tools.extend(page.tools);
        if tools.len() > MAX_TOOLS_PER_UPSTREAM {
            return Err(ListError::TooManyTools);
        }
        match page.next_cursor {
            None => return Ok(tools),
            // A cursor identical to the one just used would loop
            // forever; refuse it.
            Some(next) if cursor.as_deref() == Some(next.as_str()) => {
                return Err(ListError::RunawayPagination)
            }
            Some(next) => cursor = Some(next),
        }
    }
    Err(ListError::RunawayPagination)
}

/// Owns the client-side writer: everything client-bound funnels through
/// one task so frames never interleave mid-write.
async fn client_writer<W>(
    mut client_tx: W,
    mut from_router: mpsc::Receiver<Vec<u8>>,
    counters: Arc<WriterCounters>,
) -> WriterEnd
where
    W: AsyncWrite + Unpin,
{
    let mut end = WriterEnd::Drained;
    while let Some(frame) = from_router.recv().await {
        match write_frame(&mut client_tx, &frame).await {
            Ok(()) => {
                counters.to_client.fetch_add(1, Ordering::Relaxed);
            }
            Err(err) => {
                warn!(error = %err, "failed to write to client; discarding remaining output");
                end = WriterEnd::WriteError;
                counters.undelivered.fetch_add(1, Ordering::Relaxed);
                from_router.close();
                while from_router.recv().await.is_some() {
                    counters.undelivered.fetch_add(1, Ordering::Relaxed);
                }
                break;
            }
        }
    }
    let _ = client_tx.shutdown().await;
    end
}

/// The outcome of one background re-list.
struct RelistOutcome {
    upstream: usize,
    result: Result<Vec<ToolEntry>, ListError>,
}

struct Session {
    config: ProxyConfig,
    handles: Vec<Handle>,
    infos: Vec<UpstreamInfo>,
    toolset: ToolSet,
    phase: Phase,
    handshake: ClientHandshake,
    in_flight: ClientTable,
    relists: JoinSet<RelistOutcome>,
    relist_running: Vec<bool>,
    relist_dirty: Vec<bool>,
    /// A tool-table change happened before the client face reached
    /// Ready; one list_changed is flushed at the transition.
    pending_list_changed: bool,
    to_client: mpsc::Sender<Vec<u8>>,
    frames_to_upstream: u64,
    rejected: u64,
    discarded: u64,
}

impl Session {
    /// The serve loop; returns why the session ended.
    async fn serve<CR>(&mut self, client_rx: CR, mut events: mpsc::Receiver<Event>) -> SessionEnd
    where
        CR: AsyncRead + Unpin,
    {
        let mut reader = FrameReader::new(client_rx, self.config.max_frame_bytes);
        loop {
            tokio::select! {
                frame = reader.read_frame() => match frame {
                    Ok(None) => return SessionEnd::ClientEof,
                    Ok(Some(frame)) => {
                        if let Some(end) = self.on_client_frame(frame).await {
                            return end;
                        }
                    }
                    Err(FrameReadError::Oversized { limit }) => {
                        self.rejected += 1;
                        warn!(limit, "rejecting oversized client frame");
                        if !self
                            .deliver(builder::error_frame_null_id(code::PARSE_ERROR, "Parse error"))
                            .await
                        {
                            return SessionEnd::ClientWriteFailed;
                        }
                    }
                    Err(FrameReadError::Io(err)) => {
                        warn!(error = %err, "failed to read from client");
                        return SessionEnd::ClientReadError;
                    }
                },
                event = events.recv() => match event {
                    // Every live actor holds a sender and reports its
                    // own end before dropping it; the channel closing
                    // without such a report means an actor died silently.
                    None => return SessionEnd::Internal,
                    Some(event) => {
                        if let Some(end) = self.on_upstream_event(event).await {
                            return end;
                        }
                    }
                },
                Some(joined) = self.relists.join_next(), if !self.relists.is_empty() => {
                    match joined {
                        Ok(outcome) => {
                            if let Some(end) = self.on_relist(outcome).await {
                                return end;
                            }
                        }
                        Err(err) => warn!(error = %err, "re-list task failed"),
                    }
                }
            }
        }
    }

    /// Handles one parsed-or-not client frame. `Some(end)` ends the
    /// session.
    async fn on_client_frame(&mut self, frame: Vec<u8>) -> Option<SessionEnd> {
        if frame.iter().all(u8::is_ascii_whitespace) {
            debug!("skipping blank frame from client");
            return None;
        }
        let parsed = envelope::parse(&frame);
        match parsed {
            Err(err) => {
                self.rejected += 1;
                warn!(error = %err, "rejecting unparseable client frame");
                let (code, message) = if err.is_parse_error() {
                    (code::PARSE_ERROR, "Parse error")
                } else {
                    (code::INVALID_REQUEST, "Invalid Request")
                };
                if !self
                    .deliver(builder::error_frame_null_id(code, message))
                    .await
                {
                    return Some(SessionEnd::ClientWriteFailed);
                }
                None
            }
            Ok(Message::Request { id, method, params }) => {
                let params = params.map(|p| p.get().to_owned());
                let id_raw = raw_id_bytes(&frame, &id);
                self.on_client_request(frame, id, id_raw, method, params)
                    .await
            }
            Ok(Message::Notification { method, params }) => {
                let params = params.map(|p| p.get().to_owned());
                self.on_client_notification(frame, method, params).await
            }
            Ok(Message::Response { .. }) => {
                // The proxy never sends the client a request, so there
                // is nothing a client response can answer.
                self.discarded += 1;
                debug!("discarding unsolicited response from client");
                None
            }
        }
    }

    async fn on_client_request(
        &mut self,
        frame: Vec<u8>,
        id: RequestId,
        id_raw: String,
        method: String,
        params: Option<String>,
    ) -> Option<SessionEnd> {
        let reply = match (self.phase, method.as_str()) {
            (_, "ping") => builder::result_frame(&id_raw, "{}"),
            (Phase::PreInit, "initialize") => self.on_initialize(&id_raw, params.as_deref()),
            (_, "initialize") => {
                builder::error_frame(&id_raw, code::INVALID_REQUEST, "Already initialized")
            }
            (Phase::PreInit | Phase::Initializing, _) => builder::error_frame(
                &id_raw,
                code::SERVER_NOT_INITIALIZED,
                "Server not initialized",
            ),
            (Phase::Ready, "tools/list") => self.on_tools_list(&id_raw, params.as_deref()),
            (Phase::Ready, "tools/call") => {
                return self
                    .on_tools_call(frame, id, id_raw, params.as_deref())
                    .await
            }
            (Phase::Ready, _) => {
                debug!(method, "refusing method outside advertised capabilities");
                builder::error_frame(&id_raw, code::METHOD_NOT_FOUND, "Method not found")
            }
        };
        if !self.deliver(reply).await {
            return Some(SessionEnd::ClientWriteFailed);
        }
        None
    }

    fn on_initialize(&mut self, id_raw: &str, params: Option<&str>) -> Vec<u8> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct InitParamsWire {
            protocol_version: Option<String>,
            client_info: Option<PeerInfoWire>,
        }
        #[derive(Deserialize)]
        struct PeerInfoWire {
            name: Option<String>,
            version: Option<String>,
        }

        let parsed: Option<InitParamsWire> = params.and_then(|p| serde_json::from_str(p).ok());
        let Some(parsed) = parsed else {
            return builder::error_frame(id_raw, code::INVALID_PARAMS, "Invalid params");
        };
        let Some(offered) = parsed.protocol_version else {
            return builder::error_frame(
                id_raw,
                code::INVALID_PARAMS,
                "protocolVersion is required",
            );
        };

        // Spec negotiation: echo a supported offer; otherwise answer
        // with the newest version the proxy speaks and let the client
        // decide whether it can follow.
        let negotiated = if protocol::supported(&offered) {
            offered.clone()
        } else {
            protocol::OFFERED_VERSION.to_owned()
        };

        self.handshake.offered_protocol_version = Some(offered);
        self.handshake.negotiated_protocol_version = Some(negotiated.clone());
        if let Some(info) = parsed.client_info {
            self.handshake.client_name = info.name;
            self.handshake.client_version = info.version;
        }
        info!(
            offered_protocol_version = self.handshake.offered_protocol_version.as_deref(),
            negotiated_protocol_version = negotiated,
            client_name = self.handshake.client_name.as_deref(),
            client_version = self.handshake.client_version.as_deref(),
            "answered client initialize"
        );

        let mut result = serde_json::json!({
            "protocolVersion": negotiated,
            // Exactly what the proxy honors: it re-lists and re-emits
            // list_changed. No prompts, resources, logging, or
            // completions are advertised, so a compliant client will
            // not ask for them.
            "capabilities": { "tools": { "listChanged": true } },
            "serverInfo": {
                "name": protocol::PROXY_NAME,
                "title": "Flavium MCP proxy",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        if let Some(instructions) = merged_instructions(&self.infos) {
            result["instructions"] = serde_json::Value::String(instructions);
        }
        self.phase = Phase::Initializing;
        builder::result_frame(id_raw, &result.to_string())
    }

    fn on_tools_list(&mut self, id_raw: &str, params: Option<&str>) -> Vec<u8> {
        // The proxy returns unpaginated lists and never mints a cursor,
        // so any non-null cursor is foreign by definition. A `null`
        // params member is treated as absent params.
        if let Some(params) = params.filter(|p| p.trim() != "null") {
            match splice::member_value(params, "cursor") {
                Ok(None) => {}
                Ok(Some(value)) if value == "null" => {}
                Ok(Some(_)) => {
                    return builder::error_frame(id_raw, code::INVALID_PARAMS, "Unknown cursor")
                }
                Err(_) => {
                    return builder::error_frame(id_raw, code::INVALID_PARAMS, "Invalid params")
                }
            }
        }
        let merged = self.toolset.merged_result();
        // Per-upstream byte budgets bound each upstream at one frame's
        // worth, but several upstreams can still sum past what one
        // frame may carry; refuse rather than emit a frame the proxy
        // itself would reject on any read path.
        if merged.len().saturating_add(64) > self.config.max_frame_bytes {
            warn!(
                bytes = merged.len(),
                "merged tool list exceeds the frame cap; refusing to serve it"
            );
            return builder::error_frame(
                id_raw,
                code::INTERNAL_ERROR,
                "merged tool list exceeds the frame limit",
            );
        }
        builder::result_frame(id_raw, &merged)
    }

    async fn on_tools_call(
        &mut self,
        frame: Vec<u8>,
        id: RequestId,
        id_raw: String,
        params: Option<&str>,
    ) -> Option<SessionEnd> {
        #[derive(Deserialize)]
        struct CallParamsWire {
            name: Option<String>,
        }

        let name = params
            .and_then(|p| serde_json::from_str::<CallParamsWire>(p).ok())
            .and_then(|p| p.name);
        let Some(name) = name else {
            let reply = builder::error_frame(&id_raw, code::INVALID_PARAMS, "Invalid params");
            if !self.deliver(reply).await {
                return Some(SessionEnd::ClientWriteFailed);
            }
            return None;
        };

        let Some(upstream) = self.toolset.route(&name) else {
            // Indistinguishable from what M5 will answer for a tool
            // outside the grant envelope.
            let reply = builder::error_frame(
                &id_raw,
                code::INVALID_PARAMS,
                format!("Unknown tool: {name}").as_str(),
            );
            if !self.deliver(reply).await {
                return Some(SessionEnd::ClientWriteFailed);
            }
            return None;
        };

        if self.in_flight.insert(id.clone(), upstream).is_err() {
            warn!("client reused an in-flight request id; rejecting");
            let reply = builder::error_frame(
                &id_raw,
                code::INVALID_REQUEST,
                "Request id is already in flight",
            );
            if !self.deliver(reply).await {
                return Some(SessionEnd::ClientWriteFailed);
            }
            return None;
        }

        let command = Command::Forward {
            client_id: id.clone(),
            client_id_raw: id_raw.clone().into_boxed_str(),
            frame,
        };
        // try_send, never send: the serve loop must not park on a full
        // actor queue while that actor may be parked on the event queue
        // the serve loop drains — that cycle is a deadlock.
        match self.handles[upstream].try_send(command) {
            Ok(()) => {
                self.frames_to_upstream += 1;
                None
            }
            Err(refusal) => {
                self.in_flight.remove(&id);
                let message = match refusal {
                    SendRefusal::Busy => "upstream busy",
                    // The actor is gone; its Fatal/Ended event will end
                    // the session. Answer this call meanwhile.
                    SendRefusal::Closed => "upstream unavailable",
                };
                let reply = builder::error_frame(&id_raw, code::INTERNAL_ERROR, message);
                if !self.deliver(reply).await {
                    return Some(SessionEnd::ClientWriteFailed);
                }
                None
            }
        }
    }

    async fn on_client_notification(
        &mut self,
        frame: Vec<u8>,
        method: String,
        params: Option<String>,
    ) -> Option<SessionEnd> {
        match method.as_str() {
            "notifications/initialized" => {
                if self.phase == Phase::Initializing {
                    self.phase = Phase::Ready;
                    info!("client session initialized");
                    if self.pending_list_changed {
                        self.pending_list_changed = false;
                        let note =
                            builder::notification_frame("notifications/tools/list_changed", None);
                        if !self.deliver(note).await {
                            return Some(SessionEnd::ClientWriteFailed);
                        }
                    }
                } else {
                    debug!("ignoring redundant initialized notification");
                }
                None
            }
            "notifications/cancelled" => {
                let request_id = params.as_deref().and_then(cancelled_request_id);
                let Some(request_id) = request_id else {
                    self.discarded += 1;
                    debug!("cancellation without a readable requestId; dropped");
                    return None;
                };
                let Some(upstream) = self.in_flight.remove(&request_id) else {
                    self.discarded += 1;
                    debug!("cancellation for a request not in flight; dropped");
                    return None;
                };
                let command = Command::Cancel {
                    client_id: request_id,
                    frame,
                };
                // Cancellation is best-effort by specification; if the
                // actor's queue is full the cancel is dropped, and the
                // in-flight entry is already removed, so the eventual
                // response is dropped by the route check instead.
                match self.handles[upstream].try_send(command) {
                    Ok(()) => self.frames_to_upstream += 1,
                    Err(refusal) => {
                        self.discarded += 1;
                        debug!(?refusal, "cancellation not forwarded");
                    }
                }
                None
            }
            other => {
                // With multiple upstreams there is no faithful
                // forwarding target for notifications the proxy does
                // not model; they stop here, visibly in the count.
                self.discarded += 1;
                debug!(method = other, "discarding unroutable client notification");
                None
            }
        }
    }

    async fn on_upstream_event(&mut self, event: Event) -> Option<SessionEnd> {
        match event {
            Event::Response {
                upstream,
                client_id,
                frame,
            } => {
                // The response must answer a request that is (a) still
                // in flight and (b) routed to the upstream it came
                // from. (a) fails for responses that raced a
                // cancellation — the plan's "late responses after
                // cancel dropped"; (b) fails when the client has since
                // reused the id toward a different upstream, which a
                // stale response must not hijack.
                match self.in_flight.route(&client_id) {
                    Some(routed) if routed == upstream => {
                        self.in_flight.remove(&client_id);
                        if !self.deliver(frame).await {
                            return Some(SessionEnd::ClientWriteFailed);
                        }
                    }
                    Some(_) => {
                        self.discarded += 1;
                        debug!(
                            upstream,
                            "stale response from a different upstream; dropped"
                        );
                    }
                    None => {
                        self.discarded += 1;
                        debug!(
                            upstream,
                            "response for a request no longer in flight; dropped"
                        );
                    }
                }
                None
            }
            Event::Notification { frame, .. } => {
                // Server-originated traffic before the client-face
                // handshake completes would violate the lifecycle;
                // progress before Ready references requests that cannot
                // exist yet.
                if self.phase != Phase::Ready {
                    self.discarded += 1;
                    debug!("upstream notification before client is ready; dropped");
                    return None;
                }
                if !self.deliver(frame).await {
                    return Some(SessionEnd::ClientWriteFailed);
                }
                None
            }
            Event::ListChanged { upstream } => {
                self.schedule_relist(upstream);
                None
            }
            Event::Ended { upstream } => {
                warn!(
                    upstream = %self.infos[upstream].name,
                    "upstream ended; ending the session (supervision is T3)"
                );
                Some(SessionEnd::UpstreamGone { upstream })
            }
            Event::Fatal { upstream, error } => {
                warn!(
                    upstream = %self.infos[upstream].name,
                    error = %error,
                    "upstream failed; ending the session"
                );
                Some(SessionEnd::UpstreamGone { upstream })
            }
        }
    }

    fn schedule_relist(&mut self, upstream: usize) {
        if self.relist_running[upstream] {
            self.relist_dirty[upstream] = true;
            return;
        }
        self.relist_running[upstream] = true;
        let handle = self.handles[upstream].clone();
        let list_timeout = self.config.list_timeout;
        let byte_budget = self.config.max_frame_bytes;
        self.relists.spawn(async move {
            RelistOutcome {
                upstream,
                result: drain_tools(&handle, list_timeout, byte_budget).await,
            }
        });
    }

    async fn on_relist(&mut self, outcome: RelistOutcome) -> Option<SessionEnd> {
        let upstream = outcome.upstream;
        self.relist_running[upstream] = false;
        if self.relist_dirty[upstream] {
            self.relist_dirty[upstream] = false;
            self.schedule_relist(upstream);
        }

        match outcome.result {
            Ok(tools) => {
                let mut per_upstream: Vec<Vec<ToolEntry>> = Vec::with_capacity(self.handles.len());
                for index in 0..self.handles.len() {
                    if index == upstream {
                        per_upstream.push(tools.clone());
                    } else {
                        per_upstream.push(self.toolset_tools(index));
                    }
                }
                match ToolSet::build(per_upstream) {
                    Ok(new_set) => {
                        self.toolset = new_set;
                        info!(
                            upstream = %self.infos[upstream].name,
                            tools = self.toolset.len(),
                            "tool list re-merged after list_changed"
                        );
                        // Before the client face is Ready no
                        // server-originated frame may cross; remember
                        // the change and flush one notification at the
                        // transition.
                        if self.phase != Phase::Ready {
                            self.pending_list_changed = true;
                            return None;
                        }
                        let note =
                            builder::notification_frame("notifications/tools/list_changed", None);
                        if !self.deliver(note).await {
                            return Some(SessionEnd::ClientWriteFailed);
                        }
                        None
                    }
                    Err(collision) => {
                        warn!(
                            tool = %collision.name,
                            "re-list introduced a tool collision; ending the session"
                        );
                        Some(SessionEnd::ToolCollision {
                            tool: collision.name,
                        })
                    }
                }
            }
            Err(err) => {
                // Availability may lag behind the upstream; authority
                // never depends on this table (grants gate calls from
                // M5 on), so a stale list is the lesser evil next to
                // killing a live session over a transient failure.
                warn!(
                    upstream = %self.infos[upstream].name,
                    error = %err,
                    "re-list failed; keeping the previous tool table"
                );
                None
            }
        }
    }

    /// The current tool entries of one upstream (cloned out of the
    /// live table).
    fn toolset_tools(&self, upstream: usize) -> Vec<ToolEntry> {
        self.toolset.tools_of(upstream)
    }

    /// Queues a frame for the client. `false` means the write path is
    /// dead and the session must end.
    async fn deliver(&mut self, frame: Vec<u8>) -> bool {
        self.to_client.send(frame).await.is_ok()
    }
}

/// The exact id bytes of a request frame, falling back to canonical
/// encoding when the frame resists member capture (never expected for
/// frames that passed the envelope boundary).
fn raw_id_bytes(frame: &[u8], id: &RequestId) -> String {
    std::str::from_utf8(frame)
        .ok()
        .and_then(|text| splice::member_value(text, "id").ok().flatten())
        .unwrap_or_else(|| builder::encode_id(id))
}

/// The `requestId` of a cancellation, as a typed id.
fn cancelled_request_id(params: &str) -> Option<RequestId> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct CancelledWire {
        request_id: Option<serde_json::Value>,
    }
    let parsed: CancelledWire = serde_json::from_str(params).ok()?;
    match parsed.request_id? {
        serde_json::Value::Number(n) => n.as_i64().map(RequestId::Number),
        serde_json::Value::String(s) => Some(RequestId::String(s)),
        _ => None,
    }
}

/// The client-facing `instructions`: verbatim for a single upstream,
/// labeled sections when several contribute.
fn merged_instructions(infos: &[UpstreamInfo]) -> Option<String> {
    let with_instructions: Vec<&UpstreamInfo> = infos
        .iter()
        .filter(|info| info.instructions.as_deref().is_some_and(|i| !i.is_empty()))
        .collect();
    match with_instructions.as_slice() {
        [] => None,
        [only] if infos.len() == 1 => only.instructions.clone(),
        several => Some(
            several
                .iter()
                .filter_map(|info| {
                    info.instructions
                        .as_deref()
                        .map(|text| format!("## {}\n\n{}", info.name, text))
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn info(name: &str, instructions: Option<&str>) -> UpstreamInfo {
        UpstreamInfo {
            name: name.into(),
            negotiated_version: protocol::OFFERED_VERSION.into(),
            server_name: None,
            server_version: None,
            instructions: instructions.map(str::to_owned),
            tools_declared: true,
            tools_list_changed: false,
        }
    }

    #[test]
    fn single_upstream_instructions_pass_verbatim() {
        let infos = vec![info("fs", Some("Use the read tool."))];
        assert_eq!(
            merged_instructions(&infos).as_deref(),
            Some("Use the read tool.")
        );
    }

    #[test]
    fn multiple_upstreams_get_labeled_sections() {
        let infos = vec![
            info("fs", Some("Files here.")),
            info("mail", None),
            info("web", Some("Web there.")),
        ];
        assert_eq!(
            merged_instructions(&infos).as_deref(),
            Some("## fs\n\nFiles here.\n\n## web\n\nWeb there.")
        );
    }

    #[test]
    fn no_instructions_means_no_field() {
        assert_eq!(merged_instructions(&[info("fs", None)]), None);
        assert_eq!(merged_instructions(&[info("fs", Some(""))]), None);
        assert_eq!(merged_instructions(&[]), None);
    }

    #[test]
    fn cancelled_request_ids_parse_strictly() {
        assert_eq!(
            cancelled_request_id(r#"{"requestId": 7, "reason": "x"}"#),
            Some(RequestId::Number(7))
        );
        assert_eq!(
            cancelled_request_id(r#"{"requestId": "abc"}"#),
            Some(RequestId::String("abc".into()))
        );
        for bad in [
            r#"{"requestId": 1.5}"#,
            r#"{"requestId": null}"#,
            r#"{"requestId": [1]}"#,
            r#"{"reason": "no id"}"#,
            "not json",
        ] {
            assert_eq!(cancelled_request_id(bad), None, "input {bad:?}");
        }
    }

    #[test]
    fn raw_id_bytes_prefers_source_bytes() {
        let frame = br#"{"jsonrpc":"2.0","id": "cafe" ,"method":"m"}"#;
        let id = RequestId::String("cafe".into());
        assert_eq!(raw_id_bytes(frame, &id), r#""cafe""#);
    }
}
