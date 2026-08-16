//! Integration tests for the streamable-HTTP upstream transport against
//! a real (in-process) HTTP server.
//!
//! The server implements the streamable-HTTP contract the proxy relies
//! on: `MCP-Session-Id` assigned at initialize and *required* (400)
//! afterwards, `MCP-Protocol-Version` checked on post-negotiation
//! requests, 202 for notifications, JSON and SSE response bodies
//! (including multi-line SSE data, exercising newline normalization),
//! an unsolicited GET stream, DELETE at session end, and 404 once the
//! session is expired.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use flavium_proxy_mcp::http::HttpTransport;
use flavium_proxy_mcp::router::{self, PreparedUpstream, ProxyConfig, SessionEnd, SessionSummary};
use flavium_proxy_mcp::transport::Transport;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

const SESSION_ID: &str = "sess-0123456789abcdef";
const PINNED_PROTOCOL_VERSION: &str = "2025-11-25";

struct ServerState {
    /// Lifecycle gate: set once notifications/initialized arrives; any
    /// non-ping request before that is refused, like a stateful
    /// spec-following server would.
    initialized: AtomicBool,
    /// Kill switch: every request answers 404, as an expired session.
    expired: AtomicBool,
    /// Fail the next tools/call POST with a 500.
    fail_next_call: AtomicBool,
    /// Set when the client DELETEs the session.
    deleted: AtomicBool,
    /// Number of post-initialize requests that carried the negotiated
    /// MCP-Protocol-Version header.
    versioned_requests: AtomicUsize,
    /// Number of post-initialize requests total.
    later_requests: AtomicUsize,
    /// Frames pushed to the unsolicited GET stream.
    notify: broadcast::Sender<String>,
    /// The tools this server currently offers.
    tools: Mutex<Value>,
}

impl ServerState {
    fn new() -> Arc<Self> {
        let (notify, _) = broadcast::channel(16);
        Arc::new(Self {
            initialized: AtomicBool::new(false),
            expired: AtomicBool::new(false),
            fail_next_call: AtomicBool::new(false),
            deleted: AtomicBool::new(false),
            versioned_requests: AtomicUsize::new(0),
            later_requests: AtomicUsize::new(0),
            notify,
            tools: Mutex::new(json!([
                { "name": "http_echo", "inputSchema": { "type": "object" } },
                { "name": "sse_tool", "inputSchema": { "type": "object" } }
            ])),
        })
    }
}

/// One SSE event whose data is deliberately split across two lines: the
/// receiver must join them and cope with the embedded newline.
fn split_data_event(frame: &str) -> Event {
    let split_at = frame.len() / 2;
    // Split between tokens is what a pretty-printer would do; splitting
    // mid-token would corrupt the JSON, so find a comma to split after.
    let at = frame[..split_at]
        .rfind(',')
        .map(|i| i + 1)
        .unwrap_or(split_at);
    Event::default().data(format!("{}\n{}", &frame[..at], &frame[at..]))
}

async fn post_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if state.expired.load(Ordering::SeqCst) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let message: Value = serde_json::from_str(&body).expect("server received invalid JSON");
    let method = message.get("method").and_then(Value::as_str);
    let is_initialize = method == Some("initialize");

    if !is_initialize {
        // The streamable-HTTP session contract: no session header, no
        // service.
        if headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) != Some(SESSION_ID) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        state.later_requests.fetch_add(1, Ordering::SeqCst);
        if headers
            .get("mcp-protocol-version")
            .and_then(|v| v.to_str().ok())
            == Some(PINNED_PROTOCOL_VERSION)
        {
            state.versioned_requests.fetch_add(1, Ordering::SeqCst);
        }
    }

    let id = message.get("id").cloned();
    let Some(id) = id else {
        if method == Some("notifications/initialized") {
            state.initialized.store(true, Ordering::SeqCst);
        }
        // A notification (or response): accepted, no body.
        return StatusCode::ACCEPTED.into_response();
    };

    // The lifecycle gate the MCP spec implies for stateful servers: no
    // requests (pings aside) before initialized. If the proxy's
    // ordering ever regresses — e.g. the initialized POST racing the
    // first tools/list — this makes the whole suite fail loudly.
    if !is_initialize && method != Some("ping") && !state.initialized.load(Ordering::SeqCst) {
        let reply = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32002, "message": "Server not initialized" }
        });
        return (
            StatusCode::OK,
            [("Content-Type", "application/json")],
            reply.to_string(),
        )
            .into_response();
    }

    match method {
        Some("initialize") => {
            let reply = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": PINNED_PROTOCOL_VERSION,
                    "capabilities": { "tools": { "listChanged": true } },
                    "serverInfo": { "name": "http-upstream", "version": "1.0" },
                }
            });
            (
                StatusCode::OK,
                [
                    ("Mcp-Session-Id", SESSION_ID),
                    ("Content-Type", "application/json"),
                ],
                reply.to_string(),
            )
                .into_response()
        }
        Some("tools/list") => {
            // Answered over SSE with a priming event and split data
            // lines — the streaming shape hosted servers actually use.
            let tools = state.tools.lock().unwrap().clone();
            let reply = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tools }
            })
            .to_string();
            let events = futures_util::stream::iter(vec![
                Ok::<_, Infallible>(Event::default().id("prime-1")),
                Ok(split_data_event(&reply)),
            ]);
            Sse::new(events).into_response()
        }
        Some("tools/call") => {
            if state.fail_next_call.swap(false, Ordering::SeqCst) {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            let name = message["params"]["name"].as_str().unwrap_or_default();
            let reply = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "content": [{ "type": "text", "text": format!("called {name}") }] }
            })
            .to_string();
            if name == "sse_tool" {
                // Progress first, then the response, on one stream.
                let progress = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": { "progressToken": "http-tok", "progress": 0.5 }
                })
                .to_string();
                let events = futures_util::stream::iter(vec![
                    Ok::<_, Infallible>(Event::default().data(progress)),
                    Ok(Event::default().data(reply)),
                ]);
                Sse::new(events).into_response()
            } else {
                (
                    StatusCode::OK,
                    [("Content-Type", "application/json")],
                    reply,
                )
                    .into_response()
            }
        }
        _ => {
            let reply = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "Method not found" }
            });
            (
                StatusCode::OK,
                [("Content-Type", "application/json")],
                reply.to_string(),
            )
                .into_response()
        }
    }
}

async fn get_handler(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    if state.expired.load(Ordering::SeqCst) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) != Some(SESSION_ID) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let rx = state.notify.subscribe();
    let events = futures_util::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(frame) => Some((Ok::<_, Infallible>(Event::default().data(frame)), rx)),
            Err(_) => None,
        }
    });
    Sse::new(events).into_response()
}

async fn delete_handler(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    if headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) != Some(SESSION_ID) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    state.deleted.store(true, Ordering::SeqCst);
    StatusCode::OK.into_response()
}

/// Starts the scripted server; returns its state and MCP endpoint URL.
async fn start_server(with_get: bool) -> (Arc<ServerState>, String) {
    let state = ServerState::new();
    let mut app = Router::new()
        .route("/mcp", post(post_handler))
        .route("/mcp", delete(delete_handler));
    if with_get {
        app = app.route("/mcp", get(get_handler));
    }
    let app = app.with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (state, format!("http://{addr}/mcp"))
}

struct Client {
    tx: DuplexStream,
    rx: DuplexStream,
}

impl Client {
    async fn send(&mut self, frame: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            self.tx.write_all(frame.as_bytes()).await.unwrap();
            self.tx.write_all(b"\n").await.unwrap();
        })
        .await
        .expect("timed out writing frame");
    }

    async fn recv(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut frame = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = self.rx.read(&mut byte).await.unwrap();
                assert!(n != 0, "unexpected EOF while reading a frame");
                if byte[0] == b'\n' {
                    return serde_json::from_slice::<Value>(&frame)
                        .expect("client received invalid JSON");
                }
                frame.push(byte[0]);
            }
        })
        .await
        .expect("timed out reading frame")
    }
}

fn spawn_router_over_http(
    url: &str,
) -> (Client, JoinHandle<Result<SessionSummary, router::RunError>>) {
    let config = ProxyConfig {
        shutdown_grace: Duration::from_millis(500),
        init_timeout: Duration::from_secs(10),
        list_timeout: Duration::from_secs(10),
        ..ProxyConfig::default()
    };
    let (client_tx, router_rx) = tokio::io::duplex(1 << 16);
    let (router_tx, client_rx) = tokio::io::duplex(1 << 16);
    let transport = Transport::http(
        HttpTransport::new("http-upstream", url, &[], config.max_frame_bytes).unwrap(),
    );
    // The gate is in the path here too: an HTTP upstream's tools are
    // granted unconstrained, so these tests still measure the transport.
    let wired = support::wire(
        support::envelope(
            ["http_echo", "sse_tool"]
                .iter()
                .map(|tool| support::grant(tool))
                .collect(),
        ),
        1_000,
    );
    let join = tokio::spawn(router::run(
        config,
        vec![PreparedUpstream {
            name: "http-upstream".to_owned(),
            transport,
        }],
        Some(wired.enforcement),
        router_rx,
        router_tx,
    ));
    (
        Client {
            tx: client_tx,
            rx: client_rx,
        },
        join,
    )
}

async fn handshake(client: &mut Client) {
    client
        .send(r#"{"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}"#)
        .await;
    let init = client.recv().await;
    assert_eq!(init["result"]["serverInfo"]["name"], "flavium");
    client
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/initialized"}"#)
        .await;
}

#[tokio::test]
async fn full_session_over_streamable_http() {
    let (state, url) = start_server(true).await;
    let (mut client, join) = spawn_router_over_http(&url);
    handshake(&mut client).await;

    // The SSE-delivered, line-split tools/list made it through intact.
    client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}"#)
        .await;
    let list = client.recv().await;
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["http_echo", "sse_tool"]);

    // Plain-JSON tools/call.
    client
        .send(r#"{"jsonrpc": "2.0", "id": "c1", "method": "tools/call", "params": {"name": "http_echo", "arguments": {}}}"#)
        .await;
    let reply = client.recv().await;
    assert_eq!(reply["id"], "c1");
    assert_eq!(reply["result"]["content"][0]["text"], "called http_echo");

    // SSE tools/call: progress passes through first, then the result.
    // The call carries the progress token the server will reference —
    // the proxy forwards progress only for tokens it actually sent.
    client
        .send(r#"{"jsonrpc": "2.0", "id": "c2", "method": "tools/call", "params": {"name": "sse_tool", "arguments": {}, "_meta": {"progressToken": "http-tok"}}}"#)
        .await;
    let progress = client.recv().await;
    assert_eq!(progress["method"], "notifications/progress");
    assert_eq!(progress["params"]["progressToken"], "http-tok");
    let reply = client.recv().await;
    assert_eq!(reply["id"], "c2");
    assert_eq!(reply["result"]["content"][0]["text"], "called sse_tool");

    // An unsolicited list_changed on the GET stream triggers a re-list
    // and a proxy-origin list_changed to the client. Wait for the GET
    // stream to be connected before pushing.
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.notify.receiver_count() == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("proxy never opened the GET stream");
    *state.tools.lock().unwrap() = json!([
        { "name": "http_echo", "inputSchema": { "type": "object" } }
    ]);
    state
        .notify
        .send(json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }).to_string())
        .unwrap();
    let note = client.recv().await;
    assert_eq!(note["method"], "notifications/tools/list_changed");
    client
        .send(r#"{"jsonrpc": "2.0", "id": 2, "method": "tools/list"}"#)
        .await;
    let relisted = client.recv().await;
    assert_eq!(relisted["result"]["tools"].as_array().unwrap().len(), 1);

    // Clean shutdown DELETEs the session, and every post-initialize
    // request carried both session and protocol-version headers.
    client.tx.shutdown().await.unwrap();
    let summary = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("router did not shut down")
        .expect("router task panicked")
        .expect("router returned an error");
    assert!(summary.clean_shutdown());
    assert_eq!(
        summary.upstreams[0].server_name.as_deref(),
        Some("http-upstream")
    );

    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.deleted.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("session was never DELETEd");
    let later = state.later_requests.load(Ordering::SeqCst);
    let versioned = state.versioned_requests.load(Ordering::SeqCst);
    assert!(later > 0);
    assert_eq!(
        later, versioned,
        "every post-initialize request must carry MCP-Protocol-Version"
    );
}

#[tokio::test]
async fn per_request_http_failure_is_isolated_not_fatal() {
    // This server offers no GET route: the proxy must tolerate the 405
    // and keep working without a listening stream.
    let (state, url) = start_server(false).await;
    let (mut client, join) = spawn_router_over_http(&url);
    handshake(&mut client).await;

    state.fail_next_call.store(true, Ordering::SeqCst);
    client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "http_echo", "arguments": {}}}"#)
        .await;
    let failed = client.recv().await;
    assert_eq!(failed["id"], 1);
    assert_eq!(failed["error"]["code"], -32603);

    // The connection survives; the next call succeeds.
    client
        .send(r#"{"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "http_echo", "arguments": {}}}"#)
        .await;
    let ok = client.recv().await;
    assert_eq!(ok["id"], 2);
    assert!(ok.get("error").is_none());

    client.tx.shutdown().await.unwrap();
    let summary = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("router did not shut down")
        .expect("router task panicked")
        .expect("router returned an error");
    assert!(summary.clean_shutdown());
}

#[tokio::test]
async fn session_expiry_ends_the_session() {
    let (state, url) = start_server(false).await;
    let (mut client, join) = spawn_router_over_http(&url);
    handshake(&mut client).await;

    state.expired.store(true, Ordering::SeqCst);
    client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "http_echo", "arguments": {}}}"#)
        .await;

    let summary = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("router did not end after session expiry")
        .expect("router task panicked")
        .expect("router returned an error");
    assert_eq!(summary.end, SessionEnd::UpstreamGone { upstream: 0 });
    assert!(!summary.clean_shutdown());
}
