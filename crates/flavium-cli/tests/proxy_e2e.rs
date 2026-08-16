//! End-to-end test: the real `flavium` binary proxying real child
//! processes (the `scripted_upstream` example from flavium-proxy-mcp).
//! Covers what the in-process tests cannot: child spawning, OS pipes,
//! config-file parsing, stdin-close shutdown, and the process exit code.

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
    let dir = std::env::temp_dir().join(format!("flavium-e2e-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("flavium.toml");
    std::fs::write(&path, contents).unwrap();
    path
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
    fn spawn_legacy() -> Self {
        Self::spawn_with_args(&[
            OsString::from("--"),
            scripted_upstream_path().into_os_string(),
        ])
    }

    fn spawn_with_config(config: &std::path::Path) -> Self {
        Self::spawn_with_args(&[OsString::from("--config"), config.as_os_str().to_owned()])
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
[[upstream]]
name = "alpha"
command = ["{upstream}", "alpha_tool"]

[[upstream]]
name = "beta"
command = ["{upstream}", "beta_tool"]
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
[[upstream]]
name = "one"
command = ["{upstream}", "same_tool"]

[[upstream]]
name = "two"
command = ["{upstream}", "same_tool"]
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

#[test]
fn upstream_exit_with_open_client_stdin_still_exits_nonzero() {
    // The upstream here is the flavium binary itself with no args: it
    // prints a banner (not an MCP handshake) and exits immediately — an
    // upstream that dies during startup. The client's stdin stays OPEN;
    // the proxy must fail startup rather than wedge.
    let mut proxy = ProxyUnderTest::spawn_with_args(&[
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
        .args(["proxy", "--", "flavium-no-such-upstream-binary"])
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
