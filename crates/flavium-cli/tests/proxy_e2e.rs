//! End-to-end test: the real `flavium` binary proxying real child
//! processes (the `scripted_upstream` example from flavium-proxy-mcp).
//! Covers what the in-process tests cannot: child spawning, OS pipes,
//! config-file parsing, stdin-close shutdown, the process exit code —
//! and, since M5, the whole enforcement path with the **real Cedar
//! engine** behind it and a real JSONL trace file on disk.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Recorded live from the M1 Claude Desktop demo on 2026-08-15 — see
/// docs/tasks/v0.1/T1-demo.md; keep in sync with the
/// scripted_upstream example and the router-session tests.
const PINNED_PROTOCOL_VERSION: &str = "2025-11-25";

/// The scripted_upstream example binary, built by `cargo test` alongside
/// this test (test executables live in `target/<profile>/deps`).
fn scripted_upstream_path() -> PathBuf {
    let mut path = std::env::current_exe().expect("test executable path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("examples");
    path.push(format!("scripted_upstream{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "scripted_upstream example not found at {} — run via `cargo test` so examples are built",
        path.display()
    );
    path
}

/// A TOML config file in a per-test scratch directory.
fn write_config(name: &str, contents: &str) -> PathBuf {
    let path = scratch_dir(name).join("flavium.toml");
    std::fs::write(&path, contents).unwrap();
    path
}

/// A fresh scratch directory for one test.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("flavium-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// TOML-escapes a path (backslashes on Windows).
fn toml_path(path: &std::path::Path) -> String {
    path.to_str()
        .expect("test paths are UTF-8")
        .replace('\\', "\\\\")
}

/// The proxy under test, with line-oriented I/O helpers and a
/// kill-on-drop guard so a failing test cannot leak processes.
struct ProxyUnderTest {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    lines: mpsc::Receiver<std::io::Result<String>>,
}

impl ProxyUnderTest {
    /// The `-- <COMMAND>` shorthand, which only exists behind
    /// `--unenforced`: it cannot carry grants.
    fn spawn_legacy() -> Self {
        Self::spawn_with_args(&[
            OsString::from("--unenforced"),
            OsString::from("--"),
            scripted_upstream_path().into_os_string(),
        ])
    }

    fn spawn_with_config(config: &std::path::Path) -> Self {
        Self::spawn_with_args(&[OsString::from("--config"), config.as_os_str().to_owned()])
    }

    fn spawn_with_config_and_trace(config: &std::path::Path, trace: &std::path::Path) -> Self {
        Self::spawn_with_args(&[
            OsString::from("--config"),
            config.as_os_str().to_owned(),
            OsString::from("--trace"),
            trace.as_os_str().to_owned(),
        ])
    }

    fn spawn_with_args(args: &[OsString]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_flavium"))
            .arg("proxy")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn flavium");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("proxy stdout not piped");

        // Reads happen on a helper thread so every expectation below can
        // time out instead of hanging CI.
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        Self {
            child,
            stdin: Some(stdin.expect("proxy stdin not piped")),
            lines,
        }
    }

    fn send(&mut self, frame: &str) {
        let stdin = self.stdin.as_mut().expect("stdin already closed");
        stdin
            .write_all(frame.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush())
            .expect("failed to write to proxy stdin");
    }

    fn recv(&mut self) -> serde_json::Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(30))
            .expect("timed out waiting for a frame from the proxy")
            .expect("failed to read proxy stdout");
        serde_json::from_str(&line).expect("proxy emitted invalid JSON")
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    /// Waits for exit, polling so a wedged proxy fails the test instead
    /// of hanging it.
    fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().expect("failed to poll proxy") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "proxy did not exit after stdin closed"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for ProxyUnderTest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn handshake(proxy: &mut ProxyUnderTest) -> serde_json::Value {
    proxy.send(
        r#"{"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "e2e-test", "version": "0"}}}"#,
    );
    let init = proxy.recv();
    proxy.send(r#"{"jsonrpc": "2.0", "method": "notifications/initialized"}"#);
    init
}

#[test]
fn legacy_single_upstream_session_through_real_processes() {
    let mut proxy = ProxyUnderTest::spawn_legacy();

    // The proxy answers initialize itself now: flavium identity, the
    // pinned protocol version, tools-only capabilities.
    let init = handshake(&mut proxy);
    assert_eq!(init["id"], 0);
    assert_eq!(init["result"]["protocolVersion"], PINNED_PROTOCOL_VERSION);
    assert_eq!(init["result"]["serverInfo"]["name"], "flavium");
    assert_eq!(init["result"]["capabilities"]["tools"]["listChanged"], true);

    // tools/list reaches through to the child's declared tool.
    proxy.send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}"#);
    let list = proxy.recv();
    assert_eq!(list["id"], 1);
    assert_eq!(list["result"]["tools"][0]["name"], "echo");

    // tools/call round-trips through the child.
    proxy.send(
        r#"{"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "echo", "arguments": {"text": "hello"}}}"#,
    );
    let call = proxy.recv();
    assert_eq!(call["id"], 2);
    let echoed = call["result"]["content"][0]["text"]
        .as_str()
        .expect("echo text");
    assert!(
        echoed.contains("hello"),
        "echo lost the arguments: {echoed}"
    );

    // A garbage line is answered by the proxy itself and never kills
    // the session.
    proxy.send("{this is not json");
    let err = proxy.recv();
    assert_eq!(err["error"]["code"], -32700);
    assert!(err["id"].is_null());

    proxy.send(r#"{"jsonrpc": "2.0", "id": 3, "method": "tools/list"}"#);
    assert_eq!(proxy.recv()["id"], 3);

    // Closing stdin shuts the whole chain down cleanly.
    proxy.close_stdin();
    let status = proxy.wait_for_exit();
    assert!(status.success(), "proxy exited with {status}");
}

#[test]
fn config_file_merges_two_upstreams_and_routes_by_name() {
    let upstream = toml_path(&scripted_upstream_path());
    let config = write_config(
        "two",
        &format!(
            r#"
version = 1
principal = "e2e-bot"

[[upstream]]
name = "alpha"
command = ["{upstream}", "alpha_tool"]

[[upstream]]
name = "beta"
command = ["{upstream}", "beta_tool"]

[[grant]]
tool = "alpha_tool"

[[grant]]
tool = "beta_tool"
"#
        ),
    );
    let mut proxy = ProxyUnderTest::spawn_with_config(&config);

    let init = handshake(&mut proxy);
    assert_eq!(init["result"]["serverInfo"]["name"], "flavium");

    proxy.send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}"#);
    let list = proxy.recv();
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["alpha_tool", "beta_tool"]);

    // Calls route to the upstream that owns the name; the scripted
    // upstream echoes its params back, so the reply proves which child
    // served it.
    proxy.send(
        r#"{"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "beta_tool", "arguments": {"x": 1}}}"#,
    );
    let call = proxy.recv();
    assert_eq!(call["id"], 2);
    let echoed = call["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        echoed.contains("beta_tool"),
        "routed to the wrong upstream: {echoed}"
    );

    proxy.close_stdin();
    let status = proxy.wait_for_exit();
    assert!(status.success(), "proxy exited with {status}");
}

#[test]
fn colliding_upstreams_refuse_to_start() {
    let upstream = toml_path(&scripted_upstream_path());
    let config = write_config(
        "collide",
        &format!(
            r#"
version = 1
principal = "e2e-bot"

[[upstream]]
name = "one"
command = ["{upstream}", "same_tool"]

[[upstream]]
name = "two"
command = ["{upstream}", "same_tool"]

[[grant]]
tool = "same_tool"
"#
        ),
    );
    let mut proxy = ProxyUnderTest::spawn_with_config(&config);
    let status = proxy.wait_for_exit();
    assert!(
        !status.success(),
        "colliding tool names must refuse service, got {status}"
    );
}

/// The whole enforcement path, end to end, against the real Cedar
/// engine: the tool list is filtered to the granted tool, an in-envelope
/// call is forwarded, an out-of-envelope one is denied, an ungranted tool
/// is indistinguishable from one that does not exist — and every one of
/// those lands in the trace file.
#[test]
fn a_grant_file_filters_allows_denies_and_records() {
    let upstream = toml_path(&scripted_upstream_path());
    let dir = scratch_dir("enforced");
    let config = dir.join("flavium.toml");
    let trace = dir.join("trace.jsonl");
    std::fs::write(
        &config,
        format!(
            r#"
version = 1
principal = "invoice-bot"

[[upstream]]
name = "fs"
command = ["{upstream}", "read_file"]

[[upstream]]
name = "mail"
command = ["{upstream}", "send_mail"]

[[grant]]
tool = "read_file"
[grant.args]
path = {{ path-prefix = "/data/invoices/" }}
"#
        ),
    )
    .unwrap();

    let mut proxy = ProxyUnderTest::spawn_with_config_and_trace(&config, &trace);
    handshake(&mut proxy);

    // Visibility ⊆ authority: `send_mail` exists upstream and is simply
    // not shown.
    proxy.send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}"#);
    let list = proxy.recv();
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["read_file"], "only granted tools may be listed");

    // In the envelope: forwarded, and the child's echo proves it ran.
    proxy.send(
        r#"{"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "read_file", "arguments": {"path": "/data/invoices/2026-01.pdf"}}}"#,
    );
    let allowed = proxy.recv();
    assert_eq!(allowed["id"], 2);
    let echoed = allowed["result"]["content"][0]["text"].as_str().unwrap();
    assert!(echoed.contains("2026-01.pdf"), "not forwarded: {echoed}");

    // Outside the prefix: an agent-visible, recoverable tool error.
    proxy.send(
        r#"{"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "read_file", "arguments": {"path": "/etc/passwd"}}}"#,
    );
    let denied = proxy.recv();
    assert_eq!(denied["id"], 3);
    assert_eq!(denied["result"]["isError"], true);
    assert_eq!(denied["result"]["content"][0]["text"], "denied by policy");

    // The first row of the T1 denial table, which is the reason D4
    // exists: byte-prefix matching alone would have *allowed* this.
    proxy.send(
        r#"{"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "read_file", "arguments": {"path": "/data/invoices/../../etc/passwd"}}}"#,
    );
    let traversal = proxy.recv();
    assert_eq!(traversal["id"], 4);
    assert_eq!(traversal["result"]["isError"], true);

    // An ungranted tool answers exactly what an unknown tool answers.
    proxy.send(
        r#"{"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "send_mail", "arguments": {"to": "a@b.c"}}}"#,
    );
    let ungranted = proxy.recv();
    proxy.send(
        r#"{"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "no_such_tool", "arguments": {}}}"#,
    );
    let unknown = proxy.recv();
    assert_eq!(ungranted["error"]["code"], -32602);
    assert_eq!(
        ungranted["error"]["message"], "Unknown tool: send_mail",
        "a denied tool must read exactly like an absent one"
    );
    assert_eq!(unknown["error"]["message"], "Unknown tool: no_such_tool");

    proxy.close_stdin();
    let status = proxy.wait_for_exit();
    assert!(status.success(), "proxy exited with {status}");

    // The trace: one JSON object per line, dense seq, and the decisions
    // recorded as they were made.
    let text = std::fs::read_to_string(&trace).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("each trace line is one JSON object"))
        .collect();
    assert!(lines.len() >= 9, "trace was {}", text);
    let seqs: Vec<u64> = lines.iter().map(|l| l["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (1..=lines.len() as u64).collect::<Vec<_>>());
    assert!(lines.iter().all(|l| l["v"] == 1));

    let events: Vec<&str> = lines.iter().map(|l| l["event"].as_str().unwrap()).collect();
    assert_eq!(events.first(), Some(&"session_started"));
    assert_eq!(events.last(), Some(&"session_ended"));
    assert_eq!(lines[0]["principal"], "invoice-bot");
    assert_eq!(lines[0]["grants"][0]["tool"], "read_file");

    let listed = lines.iter().find(|l| l["event"] == "tools_listed").unwrap();
    assert_eq!(listed["offered"], 2);
    assert_eq!(listed["granted"], 1);

    let decided: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["event"] == "call_decided")
        .collect();
    assert_eq!(decided.len(), 4, "four calls named a tool in the envelope");
    assert_eq!(decided[0]["decision"]["kind"], "allow");
    assert_eq!(decided[1]["decision"]["reason"], "out_of_envelope");
    // The traversal is recorded in the form it was judged in — the
    // decision and the record cannot disagree.
    assert_eq!(
        decided[2]["args"]["path"]["value"], "/etc/passwd",
        "the trace records the normalized call"
    );
    assert_eq!(decided[3]["decision"]["reason"], "not_granted");

    // Only the allowed call has a completion.
    let completed: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["event"] == "call_completed")
        .collect();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0]["call_id"], decided[0]["call_id"]);
    assert_eq!(completed[0]["outcome"]["kind"], "result");

    // The unknown tool never reached a decision, only a refusal.
    let refused: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["event"] == "call_refused")
        .collect();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0]["tool"], "no_such_tool");
    assert_eq!(refused[0]["reason"], "unknown_tool");

    let ended = lines.last().unwrap();
    assert_eq!(ended["reason"]["kind"], "client_eof");
    assert_eq!(ended["delivery_failed"], false);
}

/// A config with no grants is the failure mode worth engineering
/// against: an operator who believes they are protected and is not.
#[test]
fn a_config_without_grants_refuses_to_start() {
    let upstream = toml_path(&scripted_upstream_path());
    let config = write_config(
        "nogrants",
        &format!("version = 1\n[[upstream]]\nname = \"fs\"\ncommand = [\"{upstream}\"]\n"),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_flavium"))
        .args(["proxy", "--config"])
        .arg(&config)
        .output()
        .expect("failed to run flavium");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no [[grant]] entries"), "{stderr}");
    assert!(stderr.contains("--unenforced"), "{stderr}");

    // The same file runs when the operator asks for it on purpose.
    let mut proxy = ProxyUnderTest::spawn_with_args(&[
        OsString::from("--config"),
        config.as_os_str().to_owned(),
        OsString::from("--unenforced"),
    ]);
    handshake(&mut proxy);
    proxy.send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}"#);
    assert_eq!(proxy.recv()["result"]["tools"][0]["name"], "echo");
    proxy.close_stdin();
    assert!(proxy.wait_for_exit().success());
}

/// There is nothing honest to write for a session that allowed
/// everything, so the two flags are refused together.
#[test]
fn unenforced_and_trace_are_mutually_exclusive() {
    let output = Command::new(env!("CARGO_BIN_EXE_flavium"))
        .args([
            "proxy",
            "--unenforced",
            "--trace",
            "trace.jsonl",
            "--",
            "some-server",
        ])
        .output()
        .expect("failed to run flavium");
    // Clap rejects the combination before anything runs: exit code 2.
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--unenforced"), "{stderr}");
}

#[test]
fn upstream_exit_with_open_client_stdin_still_exits_nonzero() {
    // The upstream here is the flavium binary itself with no args: it
    // prints a banner (not an MCP handshake) and exits immediately — an
    // upstream that dies during startup. The client's stdin stays OPEN;
    // the proxy must fail startup rather than wedge.
    let mut proxy = ProxyUnderTest::spawn_with_args(&[
        OsString::from("--unenforced"),
        OsString::from("--"),
        OsString::from(env!("CARGO_BIN_EXE_flavium")),
    ]);
    let status = proxy.wait_for_exit();
    assert!(
        !status.success(),
        "upstream death with an open client must end abnormally, got {status}"
    );
}

#[test]
fn banner_still_prints_without_a_subcommand() {
    let output = Command::new(env!("CARGO_BIN_EXE_flavium"))
        .output()
        .expect("failed to run flavium");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("banner is UTF-8");
    assert!(stdout.contains("flavium"), "unexpected banner: {stdout}");
}

#[test]
fn proxy_without_upstreams_is_a_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_flavium"))
        .arg("proxy")
        .output()
        .expect("failed to run flavium");
    assert!(!output.status.success());
}

#[test]
fn unspawnable_upstream_fails_fast_with_nonzero_exit() {
    let output = Command::new(env!("CARGO_BIN_EXE_flavium"))
        .args([
            "proxy",
            "--unenforced",
            "--",
            "flavium-no-such-upstream-binary",
        ])
        .output()
        .expect("failed to run flavium");
    assert!(!output.status.success());
}

#[test]
fn unreadable_config_fails_fast() {
    let output = Command::new(env!("CARGO_BIN_EXE_flavium"))
        .args(["proxy", "--config", "no-such-flavium-config.toml"])
        .output()
        .expect("failed to run flavium");
    assert!(!output.status.success());
}
