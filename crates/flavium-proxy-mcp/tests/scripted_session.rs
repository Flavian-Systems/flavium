//! Scripted-session integration tests for the T1/M1 transparent proxy.
//!
//! The proxy runs over four in-memory pipes (one per direction per
//! side), mirroring the four OS pipes of the real stdio deployment. The
//! test plays both the client and the upstream server, asserting that
//! forwarded frames are byte-identical to what was sent — the T1
//! acceptance criterion that `params`/`result` round-trip unmodified —
//! and that the parse boundary fails closed in both directions.
//!
//! Frame fixtures are ASCII-only raw strings; non-ASCII content is
//! injected via Rust escapes (see `call_req` below) so no fixture can
//! silently pick up mis-encoded bytes from the source file.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use flavium_proxy_mcp::proxy::{self, ClientPumpEnd, ProxyConfig, ProxySummary, UpstreamPumpEnd};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::task::JoinHandle;

/// The MCP protocol version the scripted session is pinned to —
/// recorded live from the M1 Claude Desktop demo on 2026-08-15 (see
/// docs/tasks/v0.1/T1-m1-demo.md). Update the pin, the demo doc,
/// proxy_e2e.rs, and the scripted_upstream example together if the
/// real client negotiates something newer.
const PINNED_PROTOCOL_VERSION: &str = "2025-11-25";

const INIT_REQUEST: &str = concat!(
    r#"{"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"#,
    r#""protocolVersion": "2025-11-25", "capabilities": {"roots": {"listChanged": true}}, "#,
    r#""clientInfo": {"name": "scripted-client", "version": "9.9.9"}, "#,
    r#""_meta": {"trace": "abc"}, "unknown_extension": [1, 2.5, null, {"deep": true}]}}"#
);

const INIT_RESPONSE: &str = concat!(
    r#"{"jsonrpc": "2.0", "id": 0, "result": {"protocolVersion": "2025-11-25", "#,
    r#""capabilities": {"tools": {"listChanged": true}, "experimental": {"future": 1}}, "#,
    r#""serverInfo": {"name": "scripted-upstream", "version": "0.0.0"}, "#,
    r#""unknown_result_field": {"nested": [true, null]}}}"#
);

/// A tools/call request whose arguments include raw multibyte UTF-8
/// (U+00E9) and a float — built with an escape so the bytes are exact.
fn call_req() -> String {
    let template = r#"{"jsonrpc": "2.0", "id": "call-1", "method": "tools/call", "params": {"name": "echo", "arguments": {"text": "h<E> llo", "n": 2.5}}}"#;
    template.replace("<E>", "\u{e9}")
}

/// A test harness: the proxy running over in-memory pipes, with the
/// test holding the client's and the upstream's ends.
struct Harness {
    client_in: DuplexStream,
    client_out: DuplexStream,
    upstream_in: DuplexStream,
    upstream_out: DuplexStream,
    proxy: JoinHandle<Result<ProxySummary, proxy::ProxyError>>,
}

fn spawn_proxy(config: ProxyConfig) -> Harness {
    // Four unidirectional pipes, exactly like the real deployment's
    // stdin/stdout pairs. The test writes client_in, reads client_out,
    // reads upstream_in (what the upstream would receive), writes
    // upstream_out (what the upstream would send).
    let (client_in, proxy_client_rx) = tokio::io::duplex(1 << 16);
    let (proxy_client_tx, client_out) = tokio::io::duplex(1 << 16);
    let (proxy_upstream_tx, upstream_in) = tokio::io::duplex(1 << 16);
    let (upstream_out, proxy_upstream_rx) = tokio::io::duplex(1 << 16);
    let proxy = tokio::spawn(proxy::run(
        config,
        proxy_client_rx,
        proxy_client_tx,
        proxy_upstream_rx,
        proxy_upstream_tx,
    ));
    Harness {
        client_in,
        client_out,
        upstream_in,
        upstream_out,
        proxy,
    }
}

fn test_config() -> ProxyConfig {
    ProxyConfig {
        shutdown_grace: Duration::from_millis(250),
        ..ProxyConfig::default()
    }
}

async fn send(writer: &mut DuplexStream, frame: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        writer.write_all(frame.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
    })
    .await
    .expect("timed out writing frame");
}

async fn send_raw(writer: &mut DuplexStream, bytes: &[u8]) {
    tokio::time::timeout(Duration::from_secs(5), async {
        writer.write_all(bytes).await.unwrap();
    })
    .await
    .expect("timed out writing bytes");
}

/// Reads one `\n`-terminated frame, byte-for-byte.
async fn recv(reader: &mut DuplexStream) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut frame = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = reader.read(&mut byte).await.unwrap();
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

/// Reads until EOF, asserting nothing arrives first.
async fn expect_eof(reader: &mut DuplexStream) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = [0u8; 64];
        let n = reader.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "expected EOF, got {:?}", &buf[..n]);
    })
    .await
    .expect("timed out waiting for EOF");
}

async fn finish(harness: Harness) -> ProxySummary {
    let Harness {
        mut client_in,
        client_out,
        upstream_in,
        mut upstream_out,
        proxy,
    } = harness;
    // Client closes its input: the MCP stdio shutdown signal.
    client_in.shutdown().await.unwrap();
    // The proxy closes the upstream's input in response; the upstream
    // then closes its output.
    drop(upstream_in);
    upstream_out.shutdown().await.unwrap();
    drop(upstream_out);
    drop(client_out);
    tokio::time::timeout(Duration::from_secs(5), proxy)
        .await
        .expect("proxy did not shut down")
        .expect("proxy task panicked")
        .expect("proxy returned an error")
}

#[tokio::test]
async fn session_round_trips_byte_faithfully_and_pins_protocol_version() {
    let mut h = spawn_proxy(test_config());

    // initialize: request and response forwarded byte-identically,
    // unknown fields, `_meta`, and odd spacing included.
    send(&mut h.client_in, INIT_REQUEST).await;
    assert_eq!(recv(&mut h.upstream_in).await, INIT_REQUEST.as_bytes());
    send(&mut h.upstream_out, INIT_RESPONSE).await;
    assert_eq!(recv(&mut h.client_out).await, INIT_RESPONSE.as_bytes());

    // initialized notification (no id) passes through.
    let initialized = r#"{"jsonrpc": "2.0", "method": "notifications/initialized"}"#;
    send(&mut h.client_in, initialized).await;
    assert_eq!(recv(&mut h.upstream_in).await, initialized.as_bytes());

    // tools/list and tools/call round-trip.
    let list_req = r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}"#;
    send(&mut h.client_in, list_req).await;
    assert_eq!(recv(&mut h.upstream_in).await, list_req.as_bytes());
    let list_resp = r#"{"jsonrpc": "2.0", "id": 1, "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}}"#;
    send(&mut h.upstream_out, list_resp).await;
    assert_eq!(recv(&mut h.client_out).await, list_resp.as_bytes());

    let call_req = call_req();
    send(&mut h.client_in, &call_req).await;
    assert_eq!(recv(&mut h.upstream_in).await, call_req.as_bytes());
    let call_resp = r#"{"jsonrpc": "2.0", "id": "call-1", "result": {"content": [{"type": "text", "text": "ok"}], "isError": false}}"#;
    send(&mut h.upstream_out, call_resp).await;
    assert_eq!(recv(&mut h.client_out).await, call_resp.as_bytes());

    // Methods the proxy has never heard of pass through both ways, as
    // do server-initiated notifications.
    let unknown_req = r#"{"jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": {"uri": "file:///x"}}"#;
    send(&mut h.client_in, unknown_req).await;
    assert_eq!(recv(&mut h.upstream_in).await, unknown_req.as_bytes());
    let server_notification = r#"{"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}"#;
    send(&mut h.upstream_out, server_notification).await;
    assert_eq!(
        recv(&mut h.client_out).await,
        server_notification.as_bytes()
    );

    // Cancellation notifications pass through untouched (no id
    // translation exists in M1 to rewrite them).
    let cancel = r#"{"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 2, "reason": "user"}}"#;
    send(&mut h.client_in, cancel).await;
    assert_eq!(recv(&mut h.upstream_in).await, cancel.as_bytes());

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
    assert_eq!(summary.client_end, ClientPumpEnd::Eof);
    assert_eq!(summary.upstream_end, UpstreamPumpEnd::Eof);
    assert_eq!(summary.frames_to_upstream, 6);
    assert_eq!(summary.frames_to_client, 4);
    assert_eq!(summary.client_frames_rejected, 0);
    assert_eq!(summary.upstream_frames_dropped, 0);

    // The handshake observation pins the negotiated protocol version.
    let handshake = &summary.handshake;
    assert_eq!(
        handshake.offered_protocol_version.as_deref(),
        Some(PINNED_PROTOCOL_VERSION)
    );
    assert_eq!(
        handshake.negotiated_protocol_version.as_deref(),
        Some(PINNED_PROTOCOL_VERSION)
    );
    assert_eq!(handshake.client_name.as_deref(), Some("scripted-client"));
    assert_eq!(handshake.client_version.as_deref(), Some("9.9.9"));
    assert_eq!(handshake.server_name.as_deref(), Some("scripted-upstream"));
    assert_eq!(handshake.server_version.as_deref(), Some("0.0.0"));
}

/// Reads a proxy-origin error reply and returns its JSON-RPC error code.
async fn recv_error_code(reader: &mut DuplexStream) -> i64 {
    let frame = recv(reader).await;
    let value: serde_json::Value = serde_json::from_slice(&frame).unwrap();
    assert_eq!(value["jsonrpc"], "2.0");
    assert!(value["id"].is_null(), "error reply id must be null");
    value["error"]["code"].as_i64().unwrap()
}

#[tokio::test]
async fn unparseable_client_frames_are_answered_never_forwarded() {
    let mut h = spawn_proxy(test_config());

    // (frame bytes, expected JSON-RPC error code)
    let cases: &[(&[u8], i64)] = &[
        (b"{not json", -32700),
        (b"\xff\xfe{\"jsonrpc\":\"2.0\"}", -32700),
        // Raw control character inside a JSON string.
        (
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"m\x01\"}",
            -32700,
        ),
        (br#"[{"jsonrpc":"2.0","id":1,"method":"m"}]"#, -32600),
        (b"42", -32600),
        (br#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#, -32600),
        (br#"{"id":1,"method":"no-version"}"#, -32600),
        (
            br#"{"jsonrpc":"2.0","id":1,"method":"m","result":{}}"#,
            -32600,
        ),
    ];
    for (frame, expected_code) in cases {
        send_raw(&mut h.client_in, frame).await;
        send_raw(&mut h.client_in, b"\n").await;
        let code = recv_error_code(&mut h.client_out).await;
        assert_eq!(code, *expected_code, "frame {frame:?}");
    }

    // The session survives: a valid request still goes through, and it
    // is the *next* thing the upstream sees — none of the garbage leaked.
    let valid = r#"{"jsonrpc": "2.0", "id": 3, "method": "ping"}"#;
    send(&mut h.client_in, valid).await;
    assert_eq!(recv(&mut h.upstream_in).await, valid.as_bytes());

    let summary = finish(h).await;
    assert_eq!(summary.client_frames_rejected, cases.len() as u64);
    assert_eq!(summary.frames_to_upstream, 1);
    assert!(summary.clean_shutdown());
}

#[tokio::test]
async fn oversized_client_frame_is_rejected_and_stream_resyncs() {
    let config = ProxyConfig {
        max_frame_bytes: 1024,
        ..test_config()
    };
    let mut h = spawn_proxy(config);

    let mut oversized = format!(
        r#"{{"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {{"pad": "{}"#,
        "x".repeat(4096)
    )
    .into_bytes();
    oversized.extend_from_slice(br#""}}"#);
    send_raw(&mut h.client_in, &oversized).await;
    send_raw(&mut h.client_in, b"\n").await;
    assert_eq!(recv_error_code(&mut h.client_out).await, -32700);

    let valid = r#"{"jsonrpc": "2.0", "id": 10, "method": "ping"}"#;
    send(&mut h.client_in, valid).await;
    assert_eq!(recv(&mut h.upstream_in).await, valid.as_bytes());

    let summary = finish(h).await;
    assert_eq!(summary.client_frames_rejected, 1);
    assert_eq!(summary.frames_to_upstream, 1);
}

#[tokio::test]
async fn unparseable_upstream_frames_are_dropped_never_forwarded() {
    let mut h = spawn_proxy(test_config());

    send_raw(&mut h.upstream_out, b"\xff\xfegarbage\n").await;
    send_raw(&mut h.upstream_out, b"{broken\n").await;
    send_raw(&mut h.upstream_out, b"[]\n").await;

    // The next thing the client receives is the first valid frame —
    // nothing upstream sent before it leaked through.
    let notification = r#"{"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}"#;
    send(&mut h.upstream_out, notification).await;
    assert_eq!(recv(&mut h.client_out).await, notification.as_bytes());

    let summary = finish(h).await;
    assert_eq!(summary.upstream_frames_dropped, 3);
    assert_eq!(summary.frames_to_client, 1);
    assert!(summary.clean_shutdown());
}

#[tokio::test]
async fn oversized_upstream_frame_is_dropped_and_session_survives() {
    let config = ProxyConfig {
        max_frame_bytes: 1024,
        ..test_config()
    };
    let mut h = spawn_proxy(config);

    let mut oversized = vec![b'x'; 4096];
    oversized.push(b'\n');
    send_raw(&mut h.upstream_out, &oversized).await;

    // The next thing the client receives is the next valid frame — the
    // oversized line neither reached it nor killed the session.
    let notification = r#"{"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}"#;
    send(&mut h.upstream_out, notification).await;
    assert_eq!(recv(&mut h.client_out).await, notification.as_bytes());

    let summary = finish(h).await;
    assert_eq!(summary.upstream_frames_dropped, 1);
    assert_eq!(summary.frames_to_client, 1);
    assert!(summary.clean_shutdown());
}

#[tokio::test]
async fn dead_client_writer_ends_the_session_and_is_not_clean() {
    let mut h = spawn_proxy(test_config());

    send(&mut h.client_in, INIT_REQUEST).await;
    assert_eq!(recv(&mut h.upstream_in).await, INIT_REQUEST.as_bytes());

    // The client stops reading its output mid-session while keeping its
    // input open; the next client-bound frame hits the dead pipe.
    drop(h.client_out);
    send(&mut h.upstream_out, INIT_RESPONSE).await;

    // The upstream then goes silent. The proxy must still notice the
    // dead write path and end the session within the grace period —
    // and must not report it as clean.
    let summary = tokio::time::timeout(Duration::from_secs(5), h.proxy)
        .await
        .expect("proxy did not shut down after the client writer died")
        .expect("proxy task panicked")
        .expect("proxy returned an error");

    assert!(summary.client_delivery_failed);
    assert_eq!(summary.frames_undelivered, 1);
    assert!(!summary.clean_shutdown());
}

#[tokio::test]
async fn hung_upstream_is_abandoned_after_grace() {
    let mut h = spawn_proxy(test_config());

    send(&mut h.client_in, INIT_REQUEST).await;
    assert_eq!(recv(&mut h.upstream_in).await, INIT_REQUEST.as_bytes());

    // Client shuts down; the upstream ignores its closed stdin and
    // neither sends nor closes anything.
    h.client_in.shutdown().await.unwrap();
    let summary = tokio::time::timeout(Duration::from_secs(5), h.proxy)
        .await
        .expect("proxy did not give up on the hung upstream")
        .expect("proxy task panicked")
        .expect("proxy returned an error");

    assert_eq!(summary.client_end, ClientPumpEnd::Eof);
    assert_eq!(summary.upstream_end, UpstreamPumpEnd::Abandoned);
    assert!(summary.clean_shutdown());
}

#[tokio::test]
async fn upstream_write_failure_ends_abnormally() {
    let mut h = spawn_proxy(test_config());

    send(&mut h.client_in, INIT_REQUEST).await;
    assert_eq!(recv(&mut h.upstream_in).await, INIT_REQUEST.as_bytes());

    // The upstream stops reading its input (pipe closed) while its
    // output stays open; the client's next frame cannot be delivered.
    drop(h.upstream_in);
    let ping = r#"{"jsonrpc": "2.0", "id": 4, "method": "ping"}"#;
    send(&mut h.client_in, ping).await;

    // The proxy shuts the client side down once the write fails.
    expect_eof(&mut h.client_out).await;
    let summary = tokio::time::timeout(Duration::from_secs(5), h.proxy)
        .await
        .expect("proxy did not shut down after upstream write failure")
        .expect("proxy task panicked")
        .expect("proxy returned an error");

    assert_eq!(summary.client_end, ClientPumpEnd::UpstreamWriteError);
    assert_eq!(summary.upstream_end, UpstreamPumpEnd::Abandoned);
    assert!(!summary.clean_shutdown());
}

#[tokio::test]
async fn upstream_death_with_idle_client_still_exits() {
    let mut h = spawn_proxy(test_config());

    send(&mut h.client_in, INIT_REQUEST).await;
    assert_eq!(recv(&mut h.upstream_in).await, INIT_REQUEST.as_bytes());

    // The upstream dies outright: both its pipes close. The client
    // stays idle — the proxy must still notice and exit rather than
    // linger as a zombie.
    drop(h.upstream_in);
    let _ = h.upstream_out.shutdown().await;
    drop(h.upstream_out);

    let summary = tokio::time::timeout(Duration::from_secs(5), h.proxy)
        .await
        .expect("proxy did not shut down after upstream death")
        .expect("proxy task panicked")
        .expect("proxy returned an error");

    assert_eq!(summary.upstream_end, UpstreamPumpEnd::Eof);
    assert_eq!(summary.client_end, ClientPumpEnd::Abandoned);
    assert!(!summary.clean_shutdown());
}

#[tokio::test]
async fn blank_frames_are_skipped_silently() {
    let mut h = spawn_proxy(test_config());

    send_raw(&mut h.client_in, b"\n \t\r\n").await;
    let valid = r#"{"jsonrpc": "2.0", "id": 5, "method": "ping"}"#;
    send(&mut h.client_in, valid).await;
    assert_eq!(recv(&mut h.upstream_in).await, valid.as_bytes());

    let summary = finish(h).await;
    assert_eq!(summary.client_frames_rejected, 0);
    assert_eq!(summary.frames_to_upstream, 1);
}
