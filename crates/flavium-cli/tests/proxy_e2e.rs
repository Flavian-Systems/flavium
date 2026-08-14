//! End-to-end test: the real `flavium` binary proxying a real child
//! process (the `scripted_upstream` example from flavium-proxy-mcp).
//! Covers what the in-process tests cannot: child spawning, OS pipes,
//! stdin-close shutdown, and the process exit code.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// See docs/tasks/v0.1/T1-m1-demo.md; keep in sync with the
/// scripted_upstream example and the scripted-session tests.
const PINNED_PROTOCOL_VERSION: &str = "2025-06-18";

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

/// The proxy under test, with line-oriented I/O helpers and a
/// kill-on-drop guard so a failing test cannot leak processes.
struct ProxyUnderTest {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    lines: mpsc::Receiver<std::io::Result<String>>,
}

impl ProxyUnderTest {
    fn spawn() -> Self {
        Self::spawn_with_upstream(&scripted_upstream_path().into_os_string())
    }

    fn spawn_with_upstream(upstream: &std::ffi::OsStr) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_flavium"))
            .arg("proxy")
            .arg("--")
            .arg(upstream)
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

#[test]
fn full_session_through_real_processes() {
    let mut proxy = ProxyUnderTest::spawn();

    // initialize
    proxy.send(
        r#"{"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e-test", "version": "0"}}}"#,
    );
    let init = proxy.recv();
    assert_eq!(init["id"], 0);
    assert_eq!(init["result"]["protocolVersion"], PINNED_PROTOCOL_VERSION);
    assert_eq!(init["result"]["serverInfo"]["name"], "scripted-upstream");

    proxy.send(r#"{"jsonrpc": "2.0", "method": "notifications/initialized"}"#);

    // tools/list
    proxy.send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}"#);
    let list = proxy.recv();
    assert_eq!(list["id"], 1);
    assert_eq!(list["result"]["tools"][0]["name"], "echo");

    // tools/call
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
fn upstream_exit_with_open_client_stdin_still_exits_nonzero() {
    // The upstream here is the flavium binary itself with no args: it
    // prints a (non-JSON, dropped-at-the-parse-boundary) banner and
    // exits immediately — an upstream that dies mid-session. The
    // client's stdin stays OPEN; the proxy must still notice, release
    // its blocked stdin read, and exit abnormally rather than wedge —
    // this pins the runtime.shutdown_background() path in main.rs.
    let mut proxy =
        ProxyUnderTest::spawn_with_upstream(std::ffi::OsStr::new(env!("CARGO_BIN_EXE_flavium")));
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
fn proxy_without_upstream_command_is_a_usage_error() {
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
