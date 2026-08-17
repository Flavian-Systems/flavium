//! Scripted-session integration tests for the T1 protocol-terminating
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
//! Since M5 every session here also runs **through the grant gate**, on a
//! permissive envelope: the assertions are unchanged from M2, so a
//! regression in byte identity shows up as a failure rather than as an
//! edited expectation. What the gate itself decides is
//! `enforcement_gate.rs`.
//!
//! Frame fixtures are ASCII-only raw strings; non-ASCII content is
//! injected via Rust escapes so no fixture can silently pick up
//! mis-encoded bytes from the source file.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::Duration;

use flavium_proxy_mcp::router::{self, ProxyConfig, RunError, SessionEnd, StartupError};
use serde_json::Value;
use support::{
    answer_tools_list, boot_upstream, client_handshake, envelope, finish, grant, test_config,
    text_of, wire, Harness, PINNED_PROTOCOL_VERSION,
};

/// Every tool the M1/M2 fixtures declare, granted without constraints.
///
/// These tests are about protocol wiring and byte identity, so the
/// envelope must not be what changes their answers — but the gate is in
/// the path on every call, which makes this suite the regression net M5
/// owes **W5**: normalization changes the decision, never the frame.
const FIXTURE_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "send_mail",
    "delete_mail",
    "echo",
    "slow",
    "alpha_tool",
    "beta_tool",
    "late_tool",
];

/// Spawns the router with `n` scripted stdio upstreams, enforcing
/// [`FIXTURE_TOOLS`].
fn spawn_router(config: ProxyConfig, n: usize) -> Harness {
    let wired = wire(
        envelope(FIXTURE_TOOLS.iter().map(|tool| grant(tool)).collect()),
        1_000,
    );
    support::spawn(config, n, Some(wired))
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

#[tokio::test]
async fn full_session_merges_routes_and_translates_byte_faithfully() {
    let mut h = spawn_router(test_config(), 2);
    boot_standard(&mut h).await;

    // The proxy answers initialize itself: its own identity, and exactly
    // the capabilities it honors.
    let init = client_handshake(&mut h).await;
    assert_eq!(init["id"], 0);
    assert_eq!(init["result"]["protocolVersion"], PINNED_PROTOCOL_VERSION);
    assert_eq!(init["result"]["serverInfo"]["name"], "flavium");
    assert_eq!(
        init["result"]["capabilities"],
        serde_json::json!({ "tools": { "listChanged": true } })
    );
    // Both upstreams here declare instructions, and this session is
    // enforced, so none of it crosses — see
    // `the_handshake_does_not_leak_ungranted_tools_through_instructions`
    // in `enforcement_gate.rs`. The merge itself is unit-tested.
    assert!(
        init["result"].get("instructions").is_none(),
        "instructions crossed an enforced handshake: {init}"
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
    let call_params = "{\"name\": \"read_file\", \"arguments\": {\"path\": \"/data/x\", \"n\": 2.50, \"u\": \"h\u{e9}llo\"}, \"_meta\": {\"progressToken\": \"tok-alpha\"}}";
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

    // While the call is in flight: progress with its token passes
    // through byte-identically; progress with the same token from the
    // *other* upstream (which was never sent it) is dropped.
    let progress = r#"{"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": "tok-alpha", "progress":  0.5}}"#;
    h.upstreams[0].send(progress).await;
    assert_eq!(h.client.recv().await, progress.as_bytes());
    h.upstreams[1]
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": "tok-alpha", "progress": 0.9, "message": "spoofed"}}"#)
        .await;

    // The response comes back under the client's original id bytes,
    // result body untouched — and it is the next thing the client
    // sees, proving the spoofed progress never crossed.
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
    // Deterministic drop proof, independent of delivery ordering: the
    // client was sent exactly the init reply and two pongs — the late
    // response is nowhere in the count.
    assert_eq!(summary.frames_to_client, 3);
    assert_eq!(summary.frames_to_upstream, 2, "one call + one cancel");
    assert_eq!(summary.client_frames_discarded, 1, "the unknown-id cancel");
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
    // Exactly the init reply and the pong reached the client; none of
    // the forged frames are in the count.
    assert_eq!(summary.frames_to_client, 2);
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

#[tokio::test]
async fn colliding_minted_ids_stay_scoped_to_their_upstreams() {
    // Both upstreams mint the same integer ids (init=0, list=1, first
    // call=2); responses arriving interleaved must resolve inside each
    // connection, never across.
    let mut h = spawn_router(test_config(), 2);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    boot_upstream(&mut h.upstreams[1], "beta-server", None).await;
    answer_tools_list(
        &mut h.upstreams[0],
        None,
        r#"{"tools": [{"name": "alpha_tool", "inputSchema": {"type": "object"}}]}"#,
    )
    .await;
    answer_tools_list(
        &mut h.upstreams[1],
        None,
        r#"{"tools": [{"name": "beta_tool", "inputSchema": {"type": "object"}}]}"#,
    )
    .await;
    client_handshake(&mut h).await;

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": "for-alpha", "method": "tools/call", "params": {"name": "alpha_tool", "arguments": {}}}"#)
        .await;
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": "for-beta", "method": "tools/call", "params": {"name": "beta_tool", "arguments": {}}}"#)
        .await;
    let alpha_seen = h.upstreams[0].recv_json().await;
    let beta_seen = h.upstreams[1].recv_json().await;
    let alpha_id = alpha_seen["id"].as_i64().unwrap();
    let beta_id = beta_seen["id"].as_i64().unwrap();
    assert_eq!(
        alpha_id, beta_id,
        "the fixture depends on the minted ids colliding; fix the setup if this fails"
    );

    // Answer in reverse order with distinguishable bodies.
    h.upstreams[1]
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": {beta_id}, "result": {{"content": [{{"type": "text", "text": "from-beta"}}]}}}}"#
        ))
        .await;
    let first = h.client.recv_json().await;
    assert_eq!(first["id"], "for-beta");
    assert_eq!(first["result"]["content"][0]["text"], "from-beta");

    h.upstreams[0]
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": {alpha_id}, "result": {{"content": [{{"type": "text", "text": "from-alpha"}}]}}}}"#
        ))
        .await;
    let second = h.client.recv_json().await;
    assert_eq!(second["id"], "for-alpha");
    assert_eq!(second["result"]["content"][0]["text"], "from-alpha");

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
}

#[tokio::test]
async fn out_of_policy_notifications_are_dropped_both_ways() {
    let mut h = spawn_router(test_config(), 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    answer_tools_list(
        &mut h.upstreams[0],
        None,
        r#"{"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}"#,
    )
    .await;
    client_handshake(&mut h).await;

    // Upstream notifications for capabilities the proxy never
    // advertised stop at the proxy.
    h.upstreams[0]
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "leak?"}}"#)
        .await;
    h.upstreams[0]
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/resources/updated", "params": {"uri": "file:///x"}}"#)
        .await;

    // Client notifications the proxy does not model reach no upstream.
    h.client
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}"#)
        .await;

    // Ordering proof in both directions: the next upstream-bound frame
    // is the call, the next client-bound frame is its response.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "echo", "arguments": {}}}"#)
        .await;
    let forwarded = h.upstreams[0].recv_json().await;
    assert_eq!(forwarded["method"], "tools/call");
    let uid = forwarded["id"].as_i64().unwrap();
    h.upstreams[0]
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": {uid}, "result": {{"content": []}}}}"#
        ))
        .await;
    assert_eq!(h.client.recv_json().await["id"], 1);

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
    assert_eq!(summary.client_frames_discarded, 1, "the roots notification");
    assert_eq!(summary.frames_to_upstream, 1, "only the call crossed");
    assert_eq!(
        summary.frames_to_client, 2,
        "only the init reply and the call response crossed"
    );
}

#[tokio::test]
async fn runaway_pagination_refuses_service() {
    let mut h = spawn_router(test_config(), 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    // The upstream echoes the same cursor forever.
    answer_tools_list(
        &mut h.upstreams[0],
        None,
        r#"{"tools": [], "nextCursor": "loop"}"#,
    )
    .await;
    answer_tools_list(
        &mut h.upstreams[0],
        Some("loop"),
        r#"{"tools": [], "nextCursor": "loop"}"#,
    )
    .await;

    let result = tokio::time::timeout(Duration::from_secs(5), h.router)
        .await
        .expect("router did not fail startup")
        .expect("router task panicked");
    assert!(
        matches!(
            result,
            Err(RunError::Startup(StartupError::List {
                source: router::ListError::RunawayPagination,
                ..
            }))
        ),
        "expected runaway pagination, got {result:?}"
    );
}

#[tokio::test]
async fn oversized_tool_bytes_refuse_service() {
    let config = ProxyConfig {
        max_frame_bytes: 1024,
        ..test_config()
    };
    let mut h = spawn_router(config, 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    // Two pages, each under the frame cap, together over the byte
    // budget: page/count caps alone would admit this.
    let fat_tool = |name: &str| {
        format!(
            r#"{{"tools": [{{"name": "{name}", "description": "{}", "inputSchema": {{"type": "object"}}}}], "nextCursor": "p2"}}"#,
            "x".repeat(600)
        )
    };
    let page_one = fat_tool("a");
    answer_tools_list(&mut h.upstreams[0], None, &page_one).await;
    let page_two = fat_tool("b").replace(r#", "nextCursor": "p2""#, "");
    answer_tools_list(&mut h.upstreams[0], Some("p2"), &page_two).await;

    let result = tokio::time::timeout(Duration::from_secs(5), h.router)
        .await
        .expect("router did not fail startup")
        .expect("router task panicked");
    assert!(
        matches!(
            result,
            Err(RunError::Startup(StartupError::List {
                source: router::ListError::TooManyBytes,
                ..
            }))
        ),
        "expected a byte-budget refusal, got {result:?}"
    );
}

#[tokio::test]
async fn relist_failure_keeps_the_previous_table() {
    let mut h = spawn_router(test_config(), 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    answer_tools_list(
        &mut h.upstreams[0],
        None,
        r#"{"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}"#,
    )
    .await;
    client_handshake(&mut h).await;

    // The upstream announces a change but errors the re-list.
    h.upstreams[0]
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}"#)
        .await;
    let relist = h.upstreams[0].recv_json().await;
    assert_eq!(relist["method"], "tools/list");
    let relist_id = relist["id"].as_i64().unwrap();
    h.upstreams[0]
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": {relist_id}, "error": {{"code": -32603, "message": "registry reloading"}}}}"#
        ))
        .await;

    // The session survives and the old table still serves; the first
    // frame after the failure is the pong — no list_changed was
    // emitted for a change the proxy could not verify.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "ping"}"#)
        .await;
    assert_eq!(h.client.recv_json().await["id"], 1);
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 2, "method": "tools/list"}"#)
        .await;
    let list = h.client.recv_json().await;
    assert_eq!(list["result"]["tools"][0]["name"], "echo");

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
    assert_eq!(
        summary.frames_to_client, 3,
        "init reply, pong, list — and no list_changed"
    );
}

#[tokio::test]
async fn oversized_client_frame_is_answered_and_session_survives() {
    let config = ProxyConfig {
        max_frame_bytes: 1024,
        ..test_config()
    };
    let mut h = spawn_router(config, 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    answer_tools_list(&mut h.upstreams[0], None, r#"{"tools": []}"#).await;
    client_handshake(&mut h).await;

    let mut oversized = vec![b'x'; 4096];
    oversized.push(b'\n');
    h.client.send_raw(&oversized).await;
    let reply = h.client.recv_json().await;
    assert!(reply["id"].is_null());
    assert_eq!(reply["error"]["code"], -32700);

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "ping"}"#)
        .await;
    assert_eq!(h.client.recv_json().await["id"], 1);

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
    assert_eq!(summary.client_frames_rejected, 1);
}

#[tokio::test]
async fn invalid_params_denials_are_pinned() {
    let mut h = spawn_router(test_config(), 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    answer_tools_list(
        &mut h.upstreams[0],
        None,
        r#"{"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}"#,
    )
    .await;

    // initialize with unreadable params, then without protocolVersion:
    // each refused with -32602, neither advances the handshake.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": 42}"#)
        .await;
    let bad = h.client.recv_json().await;
    assert_eq!(bad["id"], 1);
    assert_eq!(bad["error"]["code"], -32602);

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {"capabilities": {}, "clientInfo": {"name": "c", "version": "0"}}}"#)
        .await;
    let missing = h.client.recv_json().await;
    assert_eq!(missing["id"], 2);
    assert_eq!(missing["error"]["code"], -32602);

    // The real handshake still works afterwards.
    client_handshake(&mut h).await;

    // tools/call without a name is refused with the same shape.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"arguments": {}}}"#)
        .await;
    let nameless = h.client.recv_json().await;
    assert_eq!(nameless["id"], 3);
    assert_eq!(nameless["error"]["code"], -32602);

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
}

#[tokio::test]
async fn pre_ready_list_changed_is_flushed_after_initialized() {
    let mut h = spawn_router(test_config(), 1);
    boot_upstream(&mut h.upstreams[0], "alpha-server", None).await;
    answer_tools_list(
        &mut h.upstreams[0],
        None,
        r#"{"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}"#,
    )
    .await;

    // The upstream's tools change before the client has even sent
    // initialize; no server-originated frame may cross yet.
    h.upstreams[0]
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}"#)
        .await;
    answer_tools_list(
        &mut h.upstreams[0],
        None,
        r#"{"tools": [{"name": "echo", "inputSchema": {"type": "object"}}, {"name": "late_tool", "inputSchema": {"type": "object"}}]}"#,
    )
    .await;

    // The first client-bound frame is the initialize reply, the second
    // is the flushed list_changed after initialized.
    let init = client_handshake(&mut h).await;
    assert!(init.get("result").is_some());
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
    assert_eq!(names, ["echo", "late_tool"]);

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
}
