//! Scripted-session integration tests for the T1/M2 protocol-terminating
//! router.
//!
//! The router runs over in-memory pipes: the test plays the client on
//! one side and *two* scripted upstream servers on the other, asserting
//! the M2 contract — the proxy answers `initialize` itself, initializes
//! each upstream separately, merges tool lists with pagination drained
//! internally, routes `tools/call` by name with ids translated both
//! ways — and the T1 acceptance criterion that `params`/`result` bodies
//! round-trip byte-identically (ids are rewritten, so identity is
//! asserted at body level).
//!
//! Frame fixtures are ASCII-only raw strings; non-ASCII content is
//! injected via Rust escapes so no fixture can silently pick up
//! mis-encoded bytes from the source file.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use flavium_proxy_mcp::router::{
    self, PreparedUpstream, ProxyConfig, RunError, SessionEnd, SessionSummary, StartupError,
};
use flavium_proxy_mcp::transport::{StdioTransport, Transport};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::task::JoinHandle;

/// Pinned protocol version — recorded live from the M1 Claude Desktop
/// demo on 2026-08-15 (docs/tasks/v0.1/T1-m1-demo.md). Keep in sync
/// with proxy_e2e.rs and the scripted_upstream example.
const PINNED_PROTOCOL_VERSION: &str = "2025-11-25";

fn test_config() -> ProxyConfig {
    ProxyConfig {
        shutdown_grace: Duration::from_millis(250),
        init_timeout: Duration::from_secs(5),
        list_timeout: Duration::from_secs(5),
        ..ProxyConfig::default()
    }
}

/// One side of a scripted peer: what the test writes and reads.
struct Pipe {
    /// Test → proxy bytes.
    tx: DuplexStream,
    /// Proxy → test bytes.
    rx: DuplexStream,
}

impl Pipe {
    async fn send(&mut self, frame: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            self.tx.write_all(frame.as_bytes()).await.unwrap();
            self.tx.write_all(b"\n").await.unwrap();
        })
        .await
        .expect("timed out writing frame");
    }

    async fn send_raw(&mut self, bytes: &[u8]) {
        tokio::time::timeout(Duration::from_secs(5), async {
            self.tx.write_all(bytes).await.unwrap();
        })
        .await
        .expect("timed out writing bytes");
    }

    /// Reads one `\n`-terminated frame, byte-for-byte.
    async fn recv(&mut self) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut frame = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = self.rx.read(&mut byte).await.unwrap();
                assert!(n != 0, "unexpected EOF while reading a frame");
                if byte[0] == b'\n' {
                    return frame;
                }
                frame.push(byte[0]);
            }
        })
        .await
        .expect("timed out reading frame")
    }

    async fn recv_json(&mut self) -> Value {
        let frame = self.recv().await;
        serde_json::from_slice(&frame).expect("peer received invalid JSON")
    }

    async fn expect_eof(&mut self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut buf = [0u8; 64];
            let n = self.rx.read(&mut buf).await.unwrap();
            assert_eq!(
                n,
                0,
                "expected EOF, got {:?}",
                String::from_utf8_lossy(&buf[..n])
            );
        })
        .await
        .expect("timed out waiting for EOF");
    }
}

/// The test harness: the router over in-memory pipes, the test holding
/// the client end and both upstream ends.
struct Harness {
    client: Pipe,
    upstreams: Vec<Pipe>,
    router: JoinHandle<Result<SessionSummary, RunError>>,
}

/// Spawns the router with `n` scripted stdio upstreams named
/// `upstream-0`, `upstream-1`, …
fn spawn_router(config: ProxyConfig, n: usize) -> Harness {
    let (client_tx, router_client_rx) = tokio::io::duplex(1 << 16);
    let (router_client_tx, client_rx) = tokio::io::duplex(1 << 16);

    let mut upstreams = Vec::new();
    let mut prepared = Vec::new();
    for i in 0..n {
        let (test_tx, transport_rx) = tokio::io::duplex(1 << 16);
        let (transport_tx, test_rx) = tokio::io::duplex(1 << 16);
        upstreams.push(Pipe {
            tx: test_tx,
            rx: test_rx,
        });
        prepared.push(PreparedUpstream {
            name: format!("upstream-{i}"),
            transport: Transport::stdio(StdioTransport::from_streams(
                transport_rx,
                transport_tx,
                config.max_frame_bytes,
            )),
        });
    }

    let router = tokio::spawn(router::run(
        config,
        prepared,
        router_client_rx,
        router_client_tx,
    ));
    Harness {
        client: Pipe {
            tx: client_tx,
            rx: client_rx,
        },
        upstreams,
        router,
    }
}

/// Answers one upstream's initialize + initialized exchange, asserting
/// the proxy's client face toward upstreams: fresh handshake, empty
/// (attenuated) capabilities, flavium identity.
async fn boot_upstream(pipe: &mut Pipe, server_name: &str, instructions: Option<&str>) {
    let init = pipe.recv_json().await;
    assert_eq!(init["method"], "initialize");
    assert_eq!(init["params"]["protocolVersion"], PINNED_PROTOCOL_VERSION);
    assert_eq!(
        init["params"]["capabilities"],
        serde_json::json!({}),
        "upstreams must see attenuated (empty) client capabilities"
    );
    assert_eq!(init["params"]["clientInfo"]["name"], "flavium");
    let id = init["id"].clone();

    let mut result = serde_json::json!({
        "protocolVersion": PINNED_PROTOCOL_VERSION,
        "capabilities": { "tools": { "listChanged": true } },
        "serverInfo": { "name": server_name, "version": "1.0" },
    });
    if let Some(text) = instructions {
        result["instructions"] = Value::String(text.to_owned());
    }
    let reply = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
    pipe.send(&reply.to_string()).await;

    let initialized = pipe.recv_json().await;
    assert_eq!(initialized["method"], "notifications/initialized");
    assert!(initialized.get("id").is_none());
}

/// Answers one `tools/list` request with a raw result payload (kept as
/// a string so tests control the exact bytes).
async fn answer_tools_list(pipe: &mut Pipe, expect_cursor: Option<&str>, result_raw: &str) {
    let list = pipe.recv_json().await;
    assert_eq!(list["method"], "tools/list");
    match expect_cursor {
        None => assert!(
            list.get("params").is_none() || list["params"].get("cursor").is_none(),
            "unexpected cursor in {list}"
        ),
        Some(cursor) => assert_eq!(list["params"]["cursor"], cursor),
    }
    let id = list["id"].as_i64().expect("proxy ids are integers");
    pipe.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{id},"result":{result_raw}}}"#
    ))
    .await;
}

/// Boots the standard two-upstream fixture:
/// - `upstream-0` (alpha): tools `read_file` + `write_file`, listed
///   across two pages, with instructions;
/// - `upstream-1` (beta): tool `send_mail`, with instructions.
async fn boot_standard(h: &mut Harness) {
    boot_upstream(&mut h.upstreams[0], "alpha-server", Some("Alpha rules.")).await;
    boot_upstream(&mut h.upstreams[1], "beta-server", Some("Beta rules.")).await;
    answer_tools_list(
        &mut h.upstreams[0],
        None,
        concat!(
            r#"{"tools": [{"name": "read_file", "description": "Reads.", "#,
            r#""inputSchema": {"type":  "object"}, "future_field": [1e2, null]}], "#,
            r#""nextCursor": "page-2"}"#
        ),
    )
    .await;
    answer_tools_list(
        &mut h.upstreams[0],
        Some("page-2"),
        r#"{"tools": [{"name": "write_file", "inputSchema": {"type": "object"}}]}"#,
    )
    .await;
    answer_tools_list(
        &mut h.upstreams[1],
        None,
        r#"{"tools": [{"name": "send_mail", "inputSchema": {"type": "object"}}]}"#,
    )
    .await;
}

/// Runs the client-side initialize + initialized handshake.
async fn client_handshake(h: &mut Harness) -> Value {
    h.client
        .send(concat!(
            r#"{"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"#,
            r#""protocolVersion": "2025-11-25", "capabilities": {"roots": {"listChanged": true}}, "#,
            r#""clientInfo": {"name": "scripted-client", "version": "9.9.9"}}}"#
        ))
        .await;
    let reply = h.client.recv_json().await;
    h.client
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/initialized"}"#)
        .await;
    reply
}

/// Ends the session from the client side and returns the summary.
async fn finish(mut h: Harness) -> SessionSummary {
    h.client.tx.shutdown().await.unwrap();
    // The router closes each upstream child's stdin (EOF from the
    // upstream's point of view); scripted upstreams close their output
    // in response, completing the shutdown handshake.
    for pipe in &mut h.upstreams {
        pipe.expect_eof().await;
        pipe.tx.shutdown().await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), h.router)
        .await
        .expect("router did not shut down")
        .expect("router task panicked")
        .expect("router returned an error")
}

fn text_of(frame: &[u8]) -> &str {
    std::str::from_utf8(frame).expect("frame is not UTF-8")
}

#[tokio::test]
async fn full_session_merges_routes_and_translates_byte_faithfully() {
    let mut h = spawn_router(test_config(), 2);
    boot_standard(&mut h).await;

    // The proxy answers initialize itself: its own identity, exactly
    // the capabilities it honors, labeled merged instructions.
    let init = client_handshake(&mut h).await;
    assert_eq!(init["id"], 0);
    assert_eq!(init["result"]["protocolVersion"], PINNED_PROTOCOL_VERSION);
    assert_eq!(init["result"]["serverInfo"]["name"], "flavium");
    assert_eq!(
        init["result"]["capabilities"],
        serde_json::json!({ "tools": { "listChanged": true } })
    );
    assert_eq!(
        init["result"]["instructions"],
        "## upstream-0\n\nAlpha rules.\n\n## upstream-1\n\nBeta rules."
    );

    // tools/list: one unpaginated merged list, upstream order, tool
    // objects byte-identical to how their upstreams declared them.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}"#)
        .await;
    let list_frame = h.client.recv().await;
    let list_text = text_of(&list_frame).to_owned();
    assert!(
        list_text.contains(concat!(
            r#"{"name": "read_file", "description": "Reads.", "#,
            r#""inputSchema": {"type":  "object"}, "future_field": [1e2, null]}"#
        )),
        "tool bytes were not preserved: {list_text}"
    );
    let list: Value = serde_json::from_slice(&list_frame).unwrap();
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["read_file", "write_file", "send_mail"]);
    assert!(
        list["result"].get("nextCursor").is_none(),
        "the proxy must never mint a cursor"
    );

    // tools/call routed to upstream-0: id rewritten into the proxy's
    // minted space, params and unknown members byte-identical.
    let call_params = "{\"name\": \"read_file\", \"arguments\": {\"path\": \"/data/x\", \"n\": 2.50, \"u\": \"h\u{e9}llo\"}}";
    h.client
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": "call-1", "method": "tools/call", "params": {call_params}, "_meta": {{"trace": true}}}}"#
        ))
        .await;
    let forwarded = h.upstreams[0].recv().await;
    let forwarded_text = text_of(&forwarded).to_owned();
    assert!(
        forwarded_text.contains(&format!(r#""params":{call_params}"#)),
        "params bytes were not preserved: {forwarded_text}"
    );
    assert!(
        forwarded_text.contains(r#""_meta":{"trace": true}"#),
        "unknown members were not preserved: {forwarded_text}"
    );
    let forwarded_json: Value = serde_json::from_slice(&forwarded).unwrap();
    let upstream_id = forwarded_json["id"]
        .as_i64()
        .expect("forwarded id must be a proxy-minted integer");
    assert!(
        !forwarded_text.contains("call-1"),
        "the client's id must not leak upstream"
    );

    // The response comes back under the client's original id bytes,
    // result body untouched.
    let result_raw = r#"{"content": [{"type": "text", "text": "ok  spaced"}], "extra": [1e1]}"#;
    h.upstreams[0]
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": {upstream_id}, "result": {result_raw}}}"#
        ))
        .await;
    let answered = h.client.recv().await;
    let answered_text = text_of(&answered).to_owned();
    assert!(
        answered_text.contains(&format!(r#""result":{result_raw}"#)),
        "result bytes were not preserved: {answered_text}"
    );
    let answered_json: Value = serde_json::from_slice(&answered).unwrap();
    assert_eq!(answered_json["id"], "call-1");

    // A call to the other upstream routes independently, in beta's own
    // id space.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "send_mail", "arguments": {"to": "a@b.c"}}}"#)
        .await;
    let beta_forwarded = h.upstreams[1].recv_json().await;
    assert_eq!(beta_forwarded["params"]["name"], "send_mail");
    let beta_id = beta_forwarded["id"].as_i64().unwrap();
    h.upstreams[1]
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": {beta_id}, "result": {{"content": []}}}}"#
        ))
        .await;
    assert_eq!(h.client.recv_json().await["id"], 7);

    // Progress notifications pass through byte-identically.
    let progress = r#"{"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": "call-1-tok", "progress":  0.5}}"#;
    h.upstreams[0].send(progress).await;
    assert_eq!(h.client.recv().await, progress.as_bytes());

    // The denial surface, pinned: unknown tool, foreign cursor,
    // unadvertised method, and ping answered by the proxy itself.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": {"name": "missing_tool", "arguments": {}}}"#)
        .await;
    let unknown_tool = h.client.recv_json().await;
    assert_eq!(unknown_tool["error"]["code"], -32602);
    assert_eq!(unknown_tool["id"], 8);

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 9, "method": "tools/list", "params": {"cursor": "foreign"}}"#)
        .await;
    assert_eq!(h.client.recv_json().await["error"]["code"], -32602);

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 10, "method": "resources/read", "params": {"uri": "file:///x"}}"#)
        .await;
    assert_eq!(h.client.recv_json().await["error"]["code"], -32601);

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 11, "method": "ping"}"#)
        .await;
    let pong = h.client.recv_json().await;
    assert_eq!(pong["id"], 11);
    assert_eq!(pong["result"], serde_json::json!({}));

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
    assert_eq!(summary.end, SessionEnd::ClientEof);
    assert_eq!(summary.frames_to_upstream, 2);
    assert_eq!(summary.client_frames_rejected, 0);
    assert_eq!(
        summary.handshake.offered_protocol_version.as_deref(),
        Some(PINNED_PROTOCOL_VERSION)
    );
    assert_eq!(
        summary.handshake.negotiated_protocol_version.as_deref(),
        Some(PINNED_PROTOCOL_VERSION)
    );
    assert_eq!(
        summary.handshake.client_name.as_deref(),
        Some("scripted-client")
    );
    assert_eq!(summary.upstreams.len(), 2);
    assert_eq!(
        summary.upstreams[0].server_name.as_deref(),
        Some("alpha-server")
    );
}

#[tokio::test]
async fn requests_before_initialization_are_refused() {
    let mut h = spawn_router(test_config(), 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    answer_tools_list(&mut h.upstreams[0], None, r#"{"tools": []}"#).await;

    // Before initialize: tools are refused, ping works.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}"#)
        .await;
    let refused = h.client.recv_json().await;
    assert_eq!(refused["error"]["code"], -32002);
    assert_eq!(refused["id"], 1);

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 2, "method": "ping"}"#)
        .await;
    assert_eq!(h.client.recv_json().await["id"], 2);

    // After initialize but before the initialized notification: still
    // refused (the spec says clients SHOULD NOT do this; the proxy
    // holds it to that).
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 3, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "c", "version": "0"}}}"#)
        .await;
    assert_eq!(h.client.recv_json().await["id"], 3);
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 4, "method": "tools/list"}"#)
        .await;
    assert_eq!(h.client.recv_json().await["error"]["code"], -32002);

    // A second initialize is illegal.
    h.client
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/initialized"}"#)
        .await;
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 5, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "c", "version": "0"}}}"#)
        .await;
    assert_eq!(h.client.recv_json().await["error"]["code"], -32600);

    // And the session is functional afterwards.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 6, "method": "tools/list"}"#)
        .await;
    assert_eq!(h.client.recv_json().await["id"], 6);

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
}

#[tokio::test]
async fn unsupported_client_offer_is_answered_with_latest_supported() {
    let mut h = spawn_router(test_config(), 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    answer_tools_list(&mut h.upstreams[0], None, r#"{"tools": []}"#).await;

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2026-07-28", "capabilities": {}, "clientInfo": {"name": "c", "version": "0"}}}"#)
        .await;
    let init = h.client.recv_json().await;
    assert_eq!(init["result"]["protocolVersion"], PINNED_PROTOCOL_VERSION);

    h.client
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/initialized"}"#)
        .await;
    let summary = finish(h).await;
    assert_eq!(
        summary.handshake.offered_protocol_version.as_deref(),
        Some("2026-07-28")
    );
    assert_eq!(
        summary.handshake.negotiated_protocol_version.as_deref(),
        Some(PINNED_PROTOCOL_VERSION)
    );
}

#[tokio::test]
async fn supported_older_client_offer_is_echoed() {
    let mut h = spawn_router(test_config(), 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    answer_tools_list(&mut h.upstreams[0], None, r#"{"tools": []}"#).await;

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "c", "version": "0"}}}"#)
        .await;
    assert_eq!(
        h.client.recv_json().await["result"]["protocolVersion"],
        "2025-06-18"
    );
    h.client
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/initialized"}"#)
        .await;
    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
}

#[tokio::test]
async fn duplicate_in_flight_id_is_rejected_and_reusable_after_completion() {
    let mut h = spawn_router(test_config(), 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    answer_tools_list(
        &mut h.upstreams[0],
        None,
        r#"{"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}"#,
    )
    .await;
    client_handshake(&mut h).await;

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "echo", "arguments": {}}}"#)
        .await;
    let first = h.upstreams[0].recv_json().await;
    let upstream_id = first["id"].as_i64().unwrap();

    // Same id again while in flight: rejected without reaching the
    // upstream.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "echo", "arguments": {"again": true}}}"#)
        .await;
    let rejected = h.client.recv_json().await;
    assert_eq!(rejected["error"]["code"], -32600);

    // The original completes normally…
    h.upstreams[0]
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": {upstream_id}, "result": {{"content": []}}}}"#
        ))
        .await;
    assert_eq!(h.client.recv_json().await["id"], 5);

    // …and the id becomes usable again.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "echo", "arguments": {}}}"#)
        .await;
    let second = h.upstreams[0].recv_json().await;
    let second_id = second["id"].as_i64().unwrap();
    assert_ne!(second_id, upstream_id, "minted ids must not be reused");
    h.upstreams[0]
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": {second_id}, "result": {{"content": []}}}}"#
        ))
        .await;
    assert_eq!(h.client.recv_json().await["id"], 5);

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
}

#[tokio::test]
async fn cancellation_is_translated_and_late_responses_are_dropped() {
    let mut h = spawn_router(test_config(), 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    answer_tools_list(
        &mut h.upstreams[0],
        None,
        r#"{"tools": [{"name": "slow", "inputSchema": {"type": "object"}}]}"#,
    )
    .await;
    client_handshake(&mut h).await;

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": "req-9", "method": "tools/call", "params": {"name": "slow", "arguments": {}}}"#)
        .await;
    let forwarded = h.upstreams[0].recv_json().await;
    let upstream_id = forwarded["id"].as_i64().unwrap();

    // The cancellation crosses with its requestId rewritten and its
    // other members preserved.
    h.client
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": "req-9", "reason": "user  asked"}}"#)
        .await;
    let cancel = h.upstreams[0].recv().await;
    let cancel_text = text_of(&cancel).to_owned();
    assert!(
        cancel_text.contains(&format!(r#""requestId":{upstream_id}"#)),
        "requestId was not translated: {cancel_text}"
    );
    assert!(
        cancel_text.contains(r#""reason":"user  asked""#),
        "reason was not preserved: {cancel_text}"
    );
    assert!(!cancel_text.contains("req-9"));

    // The upstream answers anyway; the response must be dropped — the
    // next thing the client sees is the answer to a later ping.
    h.upstreams[0]
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": {upstream_id}, "result": {{"content": [{{"type": "text", "text": "too late"}}]}}}}"#
        ))
        .await;
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 100, "method": "ping"}"#)
        .await;
    let next = h.client.recv_json().await;
    assert_eq!(
        next["id"], 100,
        "late response leaked to the client: {next}"
    );

    // A cancellation for something unknown is dropped silently.
    h.client
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 424242}}"#)
        .await;
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 101, "method": "ping"}"#)
        .await;
    assert_eq!(h.client.recv_json().await["id"], 101);

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
}

#[tokio::test]
async fn list_changed_relists_and_reemits() {
    let mut h = spawn_router(test_config(), 2);
    boot_standard(&mut h).await;
    client_handshake(&mut h).await;

    // Upstream 1 announces a change; the proxy re-lists it internally
    // and emits its own list_changed to the client.
    h.upstreams[1]
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}"#)
        .await;
    answer_tools_list(
        &mut h.upstreams[1],
        None,
        r#"{"tools": [{"name": "send_mail", "inputSchema": {"type": "object"}}, {"name": "delete_mail", "inputSchema": {"type": "object"}}]}"#,
    )
    .await;
    let note = h.client.recv_json().await;
    assert_eq!(note["method"], "notifications/tools/list_changed");

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}"#)
        .await;
    let list = h.client.recv_json().await;
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["read_file", "write_file", "send_mail", "delete_mail"]
    );

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
}

#[tokio::test]
async fn relist_collision_ends_the_session() {
    let mut h = spawn_router(test_config(), 2);
    boot_standard(&mut h).await;
    client_handshake(&mut h).await;

    // Upstream 1 re-lists into a name upstream 0 already owns.
    h.upstreams[1]
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}"#)
        .await;
    answer_tools_list(
        &mut h.upstreams[1],
        None,
        r#"{"tools": [{"name": "read_file", "inputSchema": {"type": "object"}}]}"#,
    )
    .await;

    let summary = tokio::time::timeout(Duration::from_secs(5), h.router)
        .await
        .expect("router did not end after the collision")
        .expect("router task panicked")
        .expect("router returned an error");
    assert_eq!(
        summary.end,
        SessionEnd::ToolCollision {
            tool: "read_file".to_owned()
        }
    );
    assert!(!summary.clean_shutdown());
}

#[tokio::test]
async fn upstream_death_ends_the_session_abnormally() {
    let mut h = spawn_router(test_config(), 2);
    boot_standard(&mut h).await;
    client_handshake(&mut h).await;

    // Upstream 0 dies outright.
    let dead = h.upstreams.remove(0);
    drop(dead);

    let summary = tokio::time::timeout(Duration::from_secs(5), h.router)
        .await
        .expect("router did not notice the dead upstream")
        .expect("router task panicked")
        .expect("router returned an error");
    assert_eq!(summary.end, SessionEnd::UpstreamGone { upstream: 0 });
    assert!(!summary.clean_shutdown());
}

#[tokio::test]
async fn startup_collision_refuses_service() {
    let mut h = spawn_router(test_config(), 2);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    boot_upstream(&mut h.upstreams[1], "beta-server", None).await;
    let same = r#"{"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}"#;
    answer_tools_list(&mut h.upstreams[0], None, same).await;
    answer_tools_list(&mut h.upstreams[1], None, same).await;

    let result = tokio::time::timeout(Duration::from_secs(5), h.router)
        .await
        .expect("router did not fail startup")
        .expect("router task panicked");
    match result {
        Err(RunError::Startup(StartupError::Collision {
            tool,
            first,
            second,
        })) => {
            assert_eq!(tool, "echo");
            assert_eq!(first, "upstream-0");
            assert_eq!(second, "upstream-1");
        }
        other => panic!("expected a collision startup error, got {other:?}"),
    }
}

#[tokio::test]
async fn unsupported_upstream_version_refuses_service() {
    let mut h = spawn_router(test_config(), 1);
    let init = h.upstreams[0].recv_json().await;
    let id = init["id"].clone();
    h.upstreams[0]
        .send(
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "serverInfo": { "name": "old-server", "version": "0" }
                }
            })
            .to_string(),
        )
        .await;

    let result = tokio::time::timeout(Duration::from_secs(5), h.router)
        .await
        .expect("router did not fail startup")
        .expect("router task panicked");
    assert!(
        matches!(
            result,
            Err(RunError::Startup(StartupError::Connect { ref name, .. })) if name == "upstream-0"
        ),
        "expected a connect startup error, got {result:?}"
    );
}

#[tokio::test]
async fn unparseable_client_frames_are_answered_never_forwarded() {
    let mut h = spawn_router(test_config(), 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    answer_tools_list(
        &mut h.upstreams[0],
        None,
        r#"{"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}"#,
    )
    .await;
    client_handshake(&mut h).await;

    let cases: &[(&[u8], i64)] = &[
        (b"{not json", -32700),
        (b"\xff\xfe{\"jsonrpc\":\"2.0\"}", -32700),
        (br#"[{"jsonrpc":"2.0","id":1,"method":"m"}]"#, -32600),
        (b"42", -32600),
        (br#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#, -32600),
        (br#"{"id":1,"method":"no-version"}"#, -32600),
    ];
    for (frame, expected_code) in cases {
        h.client.send_raw(frame).await;
        h.client.send_raw(b"\n").await;
        let reply = h.client.recv_json().await;
        assert!(reply["id"].is_null());
        assert_eq!(reply["error"]["code"].as_i64().unwrap(), *expected_code);
    }

    // The session survives, and the next thing the upstream sees is a
    // valid forwarded call — none of the garbage leaked.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "echo", "arguments": {}}}"#)
        .await;
    let forwarded = h.upstreams[0].recv_json().await;
    assert_eq!(forwarded["method"], "tools/call");
    let uid = forwarded["id"].as_i64().unwrap();
    h.upstreams[0]
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": {uid}, "result": {{"content": []}}}}"#
        ))
        .await;
    assert_eq!(h.client.recv_json().await["id"], 3);

    let summary = finish(h).await;
    assert_eq!(summary.client_frames_rejected, cases.len() as u64);
    assert_eq!(summary.frames_to_upstream, 1);
    assert!(summary.clean_shutdown());
}

#[tokio::test]
async fn upstream_responses_with_unknown_ids_are_dropped() {
    let mut h = spawn_router(test_config(), 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    answer_tools_list(&mut h.upstreams[0], None, r#"{"tools": []}"#).await;
    client_handshake(&mut h).await;

    // Responses to ids the proxy never minted (or minted for internal
    // requests long completed), and unparseable frames: all dropped.
    h.upstreams[0]
        .send(r#"{"jsonrpc": "2.0", "id": 424242, "result": {"forged": true}}"#)
        .await;
    h.upstreams[0]
        .send(r#"{"jsonrpc": "2.0", "id": "string-id", "result": {}}"#)
        .await;
    h.upstreams[0].send_raw(b"{broken\n").await;

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "ping"}"#)
        .await;
    assert_eq!(h.client.recv_json().await["id"], 1);

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
}

#[tokio::test]
async fn upstream_requests_are_refused_and_pings_answered() {
    let mut h = spawn_router(test_config(), 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    answer_tools_list(&mut h.upstreams[0], None, r#"{"tools": []}"#).await;
    client_handshake(&mut h).await;

    // The server→client request channel is closed: sampling is refused
    // at the proxy, never surfaced to the client.
    h.upstreams[0]
        .send(r#"{"jsonrpc": "2.0", "id": 900, "method": "sampling/createMessage", "params": {}}"#)
        .await;
    let refused = h.upstreams[0].recv_json().await;
    assert_eq!(refused["id"], 900);
    assert_eq!(refused["error"]["code"], -32601);

    // Upstream pings are a liveness courtesy the proxy answers itself.
    h.upstreams[0]
        .send(r#"{"jsonrpc": "2.0", "id": 901, "method": "ping"}"#)
        .await;
    let pong = h.upstreams[0].recv_json().await;
    assert_eq!(pong["id"], 901);
    assert_eq!(pong["result"], serde_json::json!({}));

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
}
