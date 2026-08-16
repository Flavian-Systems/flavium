//! Shared scaffolding for the router's integration tests: the scripted
//! peers on both sides, and the enforcement bundle the router now takes.
//!
//! The authorizer here is [`GrantEnvelope`] — the *reference* semantics
//! from `flavium-core`, not the Cedar engine. That is deliberate (M5's
//! D7): these tests are about wiring, so they check the wiring against the
//! specification, while the CLI's end-to-end tests exercise the real
//! engine. It also keeps Cedar's ~50 crates out of the proxy's tests.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flavium_core::{
    Authorizer, Constraint, Grant, GrantEnvelope, MemorySink, Principal, Timestamp, ToolName,
    TraceEvent,
};
use flavium_proxy_mcp::enforcement::{Clock, Enforcement, PathFlavors};
use flavium_proxy_mcp::router::{self, PreparedUpstream, ProxyConfig, RunError, SessionSummary};
use flavium_proxy_mcp::transport::{StdioTransport, Transport};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::task::JoinHandle;

/// Pinned protocol version — recorded live from the M1 Claude Desktop
/// demo on 2026-08-15 (docs/tasks/v0.1/T1-demo.md). Keep in sync with
/// proxy_e2e.rs and the scripted_upstream example.
pub const PINNED_PROTOCOL_VERSION: &str = "2025-11-25";

/// The principal every proxy test authorizes as.
pub const TEST_PRINCIPAL: &str = "test-bot";

pub fn test_config() -> ProxyConfig {
    ProxyConfig {
        shutdown_grace: Duration::from_millis(250),
        init_timeout: Duration::from_secs(5),
        list_timeout: Duration::from_secs(5),
        ..ProxyConfig::default()
    }
}

// ---- enforcement --------------------------------------------------------

/// A clock the test moves by hand — the reason [`Clock`] is a trait and
/// the core takes `now` as an argument instead of reading one.
#[derive(Debug)]
pub struct TestClock {
    now: Mutex<i64>,
}

impl TestClock {
    pub fn at(secs: i64) -> Self {
        Self {
            now: Mutex::new(secs),
        }
    }

    /// Moves the session's clock; the next decision and the next
    /// `tools/list` both see the new time, with no re-connect.
    pub fn set(&self, secs: i64) {
        *self.now.lock().unwrap() = secs;
    }
}

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_secs(*self.now.lock().unwrap())
    }
}

/// A grant for one tool with no constraints and no expiry.
pub fn grant(tool: &str) -> Grant {
    Grant {
        tool: ToolName::new(tool).unwrap(),
        constraints: BTreeMap::new(),
        expires: None,
    }
}

/// A grant with constraints and an optional expiry.
pub fn constrained(tool: &str, constraints: &[(&str, Constraint)], expires: Option<i64>) -> Grant {
    Grant {
        tool: ToolName::new(tool).unwrap(),
        constraints: constraints
            .iter()
            .map(|(name, c)| ((*name).to_owned(), c.clone()))
            .collect(),
        expires: expires.map(Timestamp::from_unix_secs),
    }
}

/// The envelope [`TEST_PRINCIPAL`] holds.
pub fn envelope(grants: Vec<Grant>) -> GrantEnvelope {
    GrantEnvelope {
        principal: Principal::new(TEST_PRINCIPAL).unwrap(),
        grants,
    }
}

/// An enforcement bundle plus the handles a test keeps hold of.
pub struct Wired {
    pub enforcement: Enforcement,
    pub sink: Arc<MemorySink>,
    pub clock: Arc<TestClock>,
}

/// An enforcement bundle over `envelope`, authorized by the reference
/// semantics, with a memory sink and a clock the test controls.
pub fn wire(envelope: GrantEnvelope, now: i64) -> Wired {
    let authorizer: Arc<dyn Authorizer> = Arc::new(envelope.clone());
    wire_parts(envelope, authorizer, now, PathFlavors::new())
}

/// As [`wire`], with a path-flavor map (M5's D4).
pub fn wire_paths(envelope: GrantEnvelope, now: i64, path_flavors: PathFlavors) -> Wired {
    let authorizer: Arc<dyn Authorizer> = Arc::new(envelope.clone());
    wire_parts(envelope, authorizer, now, path_flavors)
}

/// As [`wire`], with an authorizer of the caller's choosing — the only
/// way to reach an answer the reference semantics never produce.
pub fn wire_authorizer(
    envelope: GrantEnvelope,
    authorizer: Arc<dyn Authorizer>,
    now: i64,
) -> Wired {
    wire_parts(envelope, authorizer, now, PathFlavors::new())
}

fn wire_parts(
    envelope: GrantEnvelope,
    authorizer: Arc<dyn Authorizer>,
    now: i64,
    path_flavors: PathFlavors,
) -> Wired {
    let sink = Arc::new(MemorySink::new());
    let clock = Arc::new(TestClock::at(now));
    Wired {
        enforcement: Enforcement {
            envelope,
            authorizer,
            sink: sink.clone(),
            clock: clock.clone(),
            path_flavors,
        },
        sink,
        clock,
    }
}

/// The variant name of one event — enough to assert on a trace's shape
/// without re-deriving the whole vocabulary in every test.
pub fn variant_name(event: &TraceEvent) -> &'static str {
    match event {
        TraceEvent::SessionStarted { .. } => "SessionStarted",
        TraceEvent::HandshakeCompleted { .. } => "HandshakeCompleted",
        TraceEvent::ToolsListed { .. } => "ToolsListed",
        TraceEvent::CallRefused { .. } => "CallRefused",
        TraceEvent::CallDecided { .. } => "CallDecided",
        TraceEvent::CallCompleted { .. } => "CallCompleted",
        TraceEvent::FrameRejected { .. } => "FrameRejected",
        TraceEvent::FrameDiscarded { .. } => "FrameDiscarded",
        TraceEvent::UpstreamEnded { .. } => "UpstreamEnded",
        TraceEvent::SessionEnded { .. } => "SessionEnded",
    }
}

/// The names of every recorded event, in order.
pub fn event_names(events: &[TraceEvent]) -> Vec<&'static str> {
    events.iter().map(variant_name).collect()
}

// ---- scripted peers -----------------------------------------------------

/// One side of a scripted peer: what the test writes and reads.
pub struct Pipe {
    /// Test → proxy bytes.
    pub tx: DuplexStream,
    /// Proxy → test bytes.
    pub rx: DuplexStream,
}

impl Pipe {
    pub async fn send(&mut self, frame: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            self.tx.write_all(frame.as_bytes()).await.unwrap();
            self.tx.write_all(b"\n").await.unwrap();
        })
        .await
        .expect("timed out writing frame");
    }

    pub async fn send_raw(&mut self, bytes: &[u8]) {
        tokio::time::timeout(Duration::from_secs(5), async {
            self.tx.write_all(bytes).await.unwrap();
        })
        .await
        .expect("timed out writing bytes");
    }

    /// Reads one `\n`-terminated frame, byte-for-byte.
    pub async fn recv(&mut self) -> Vec<u8> {
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

    pub async fn recv_json(&mut self) -> Value {
        let frame = self.recv().await;
        serde_json::from_slice(&frame).expect("peer received invalid JSON")
    }

    pub async fn expect_eof(&mut self) {
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
/// the client end and every upstream end.
pub struct Harness {
    pub client: Pipe,
    pub upstreams: Vec<Pipe>,
    pub router: JoinHandle<Result<SessionSummary, RunError>>,
    /// The session's trace, when it was enforced.
    pub sink: Option<Arc<MemorySink>>,
    /// The session's clock, when it was enforced.
    pub clock: Option<Arc<TestClock>>,
}

impl Harness {
    /// Everything recorded so far, oldest first.
    pub fn events(&self) -> Vec<TraceEvent> {
        self.sink.as_ref().expect("session is unenforced").events()
    }

    /// The names of everything recorded so far.
    pub fn event_names(&self) -> Vec<&'static str> {
        self.events().iter().map(variant_name).collect()
    }

    /// The last recorded event of one kind.
    pub fn last_event(&self, kind: &str) -> TraceEvent {
        self.events()
            .into_iter()
            .rfind(|event| variant_name(event) == kind)
            .unwrap_or_else(|| panic!("no {kind} was recorded"))
    }

    /// Moves the session's clock.
    pub fn set_now(&self, secs: i64) {
        self.clock
            .as_ref()
            .expect("session is unenforced")
            .set(secs);
    }
}

/// Spawns the router with `n` scripted stdio upstreams named
/// `upstream-0`, `upstream-1`, …; `wired` of `None` is the unenforced
/// middlebox.
pub fn spawn(config: ProxyConfig, n: usize, wired: Option<Wired>) -> Harness {
    let (enforcement, sink, clock) = match wired {
        None => (None, None, None),
        Some(wired) => (Some(wired.enforcement), Some(wired.sink), Some(wired.clock)),
    };
    let mut harness = spawn_raw(config, n, enforcement);
    harness.sink = sink;
    harness.clock = clock;
    harness
}

/// As [`spawn`], for an enforcement bundle the test built itself — the
/// only way in for a sink or an authorizer this module does not provide.
pub fn spawn_raw(config: ProxyConfig, n: usize, enforcement: Option<Enforcement>) -> Harness {
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
        enforcement,
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
        sink: None,
        clock: None,
    }
}

/// Answers one upstream's initialize + initialized exchange, asserting
/// the proxy's client face toward upstreams: fresh handshake, empty
/// (attenuated) capabilities, flavium identity.
pub async fn boot_upstream(pipe: &mut Pipe, server_name: &str, instructions: Option<&str>) {
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

/// Answers one `tools/list` request with a raw result payload (kept as a
/// string so tests control the exact bytes).
pub async fn answer_tools_list(pipe: &mut Pipe, expect_cursor: Option<&str>, result_raw: &str) {
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

/// Boots one upstream that declares exactly `tools`.
pub async fn boot_with_tools(pipe: &mut Pipe, server_name: &str, tools: &[&str]) {
    boot_upstream(pipe, server_name, None).await;
    let declared: Vec<String> = tools
        .iter()
        .map(|name| format!(r#"{{"name": "{name}", "inputSchema": {{"type": "object"}}}}"#))
        .collect();
    answer_tools_list(
        pipe,
        None,
        &format!(r#"{{"tools": [{}]}}"#, declared.join(", ")),
    )
    .await;
}

/// Runs the client-side initialize + initialized handshake.
pub async fn client_handshake(h: &mut Harness) -> Value {
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
pub async fn finish(mut h: Harness) -> SessionSummary {
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

/// Ends the session and returns the summary *and* the trace, since a
/// session's last events are written during teardown.
pub async fn finish_traced(h: Harness) -> (SessionSummary, Vec<TraceEvent>) {
    let sink = h.sink.clone().expect("session is unenforced");
    let summary = finish(h).await;
    (summary, sink.events())
}

pub fn text_of(frame: &[u8]) -> &str {
    std::str::from_utf8(frame).expect("frame is not UTF-8")
}
