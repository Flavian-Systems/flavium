//! What the gate decides, at the wire — T1's second acceptance criterion
//! (*a grant file denies out-of-envelope calls*) and its third (*denials
//! are logged*).
//!
//! The authorizer here is the reference semantics from `flavium-core`, so
//! these tests measure the **wiring** against the specification: which
//! bytes the client sees, whether the upstream ever saw the call, and what
//! reached the trace. The same rows run against the real Cedar engine in
//! the CLI's `proxy_e2e.rs`.
//!
//! Every denial row asserts three things, because two of them are easy to
//! get right while the third is wrong:
//!
//! 1. the upstream never saw the call,
//! 2. the exact client-visible answer,
//! 3. the trace event, with its principal, tool and reason.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use flavium_core::{
    ArgValue, Authorizer, CallOutcome, Constraint, Decision, DenialReason, DiscardKind, Principal,
    RefusalReason, SinkError, Timestamp, ToolCall, ToolName, TraceEvent, TraceSink,
};
use flavium_proxy_mcp::enforcement::{Enforcement, PathFlavors, SystemClock};
use flavium_proxy_mcp::normalize::PathFlavor;
use flavium_proxy_mcp::router::SessionEnd;
use serde_json::Value;
use support::{
    boot_with_tools, client_handshake, constrained, envelope, finish, finish_traced, grant, spawn,
    spawn_raw, test_config, text_of, wire, wire_authorizer, wire_paths, Harness, TEST_PRINCIPAL,
};

/// Every tool the fixture upstream declares. `ungranted_tool` exists
/// upstream and in no grant — the difference the client must not be able
/// to see.
const OFFERED: &[&str] = &[
    "read_file",
    "read_win",
    "send_mail",
    "expiring",
    "echo",
    "ungranted_tool",
];

/// The envelope the denial table is written against.
///
/// Constraints are already in the shape the loader produces: the path
/// prefixes are normalized, and their flavors are declared separately in
/// [`flavors`].
fn gate_envelope() -> flavium_core::GrantEnvelope {
    envelope(vec![
        constrained(
            "read_file",
            &[("path", Constraint::Prefix("/data/invoices/".into()))],
            None,
        ),
        // Folded, as `normalize_prefix` would leave it: the Windows
        // flavor case-folds both sides of the comparison.
        constrained(
            "read_win",
            &[("path", Constraint::Prefix("c:/users/me/desktop/".into()))],
            None,
        ),
        constrained(
            "send_mail",
            &[
                ("to", Constraint::Suffix("@yourco.com".into())),
                ("bcc", Constraint::Absent),
                (
                    "count",
                    Constraint::Range {
                        min: Some(1),
                        max: Some(10),
                    },
                ),
                (
                    "kind",
                    Constraint::OneOf(
                        ["invoice", "receipt"]
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                    ),
                ),
            ],
            None,
        ),
        constrained("expiring", &[], Some(2_000)),
        grant("echo"),
    ])
}

fn flavors() -> PathFlavors {
    let mut flavors = PathFlavors::new();
    flavors.insert("read_file", "path", PathFlavor::Posix);
    flavors.insert("read_win", "path", PathFlavor::Windows);
    flavors
}

/// The fixture session: one upstream offering [`OFFERED`], the client
/// handshaken, the clock at 1000.
async fn gate_session() -> Harness {
    let mut h = spawn(
        test_config(),
        1,
        Some(wire_paths(gate_envelope(), 1_000, flavors())),
    );
    boot_with_tools(&mut h.upstreams[0], "fixture", OFFERED).await;
    client_handshake(&mut h).await;
    h
}

/// The tool names one `tools/list` shows right now.
async fn list_names(h: &mut Harness) -> Vec<String> {
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}"#)
        .await;
    let reply = h.client.recv_json().await;
    reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

async fn call(h: &mut Harness, id: &str, name: &str, arguments: &str) -> Value {
    h.client
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": "{id}", "method": "tools/call", "params": {{"name": "{name}", "arguments": {arguments}}}}}"#
        ))
        .await;
    h.client.recv_json().await
}

/// The decisions recorded so far, as `(tool, reason-or-allow)`.
fn decisions(events: &[TraceEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::CallDecided {
                principal,
                call,
                decision,
                ..
            } => {
                assert_eq!(principal.as_str(), TEST_PRINCIPAL);
                let verdict = match decision {
                    Decision::Allow { grant } => format!("allow:{grant}"),
                    Decision::Deny(DenialReason::NotGranted) => "not_granted".to_owned(),
                    Decision::Deny(DenialReason::Expired) => "expired".to_owned(),
                    Decision::Deny(DenialReason::OutOfEnvelope) => "out_of_envelope".to_owned(),
                    Decision::Deny(DenialReason::EvaluationError { detail }) => {
                        format!("evaluation_error:{detail}")
                    }
                };
                Some((call.tool.clone(), verdict))
            }
            _ => None,
        })
        .collect()
}

/// The `path` argument of the last decision, as the core judged it.
fn last_decided_path(events: &[TraceEvent]) -> String {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            TraceEvent::CallDecided { call, .. } => match call.args.get("path") {
                Some(ArgValue::Str(path)) => Some(path.clone()),
                other => panic!("last decision's path was {other:?}"),
            },
            _ => None,
        })
        .expect("no decision was recorded")
}

fn assert_denied_by_policy(reply: &Value, id: &str) {
    assert_eq!(reply["id"], id);
    assert!(
        reply.get("error").is_none(),
        "a policy denial is a tool error, not a protocol error: {reply}"
    );
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    assert_eq!(reply["result"]["content"][0]["type"], "text");
    assert_eq!(reply["result"]["content"][0]["text"], "denied by policy");
}

/// The per-axis denial table: every way a call can fall outside its
/// envelope, and the two shapes a client is allowed to see.
#[tokio::test]
async fn the_denial_table() {
    let mut h = gate_session().await;

    // ---- out of envelope: an agent-visible, recoverable tool error ----
    //
    // The rows are written as (id, tool, arguments, what the core should
    // have judged) so that a normalization bug shows up as a wrong
    // *recorded* value, not only as a wrong verdict.
    let rows: &[(&str, &str, &str, &str)] = &[
        // The path axis, including the two traversals D4 exists for.
        (
            "d1",
            "read_file",
            r#"{"path": "/etc/passwd"}"#,
            "/etc/passwd",
        ),
        (
            "d2",
            "read_file",
            r#"{"path": "/data/invoices/../../etc/passwd"}"#,
            "/etc/passwd",
        ),
        (
            "d3",
            "read_file",
            r#"{"path": "/data/invoices/sub/../../secret"}"#,
            "/data/secret",
        ),
        // A backslash is an ordinary filename byte under the POSIX
        // flavor, so this is one file in the working directory.
        (
            "d4",
            "read_file",
            r#"{"path": "\\data\\invoices\\x"}"#,
            r"\data\invoices\x",
        ),
        // …and the Windows flavor resolves `..\` for the tool that
        // declared it.
        (
            "d5",
            "read_win",
            r#"{"path": "C:\\Users\\me\\Desktop\\..\\..\\Administrator\\secrets"}"#,
            "c:/users/administrator/secrets",
        ),
        // A constrained argument that is missing is not admitted.
        ("d6", "read_file", r#"{}"#, ""),
    ];
    for (id, tool, arguments, _) in rows {
        let reply = call(&mut h, id, tool, arguments).await;
        assert_denied_by_policy(&reply, id);
    }
    // The `path` each denial was decided on — normalized, and recorded in
    // exactly that form (D9: the record must reproduce the decision).
    let decided_paths: Vec<String> = h
        .events()
        .iter()
        .filter_map(|event| match event {
            TraceEvent::CallDecided { call, .. } => Some(match call.args.get("path") {
                Some(ArgValue::Str(path)) => path.clone(),
                None => String::new(),
                other => panic!("unexpected path {other:?}"),
            }),
            _ => None,
        })
        .collect();
    let expected_paths: Vec<&str> = rows.iter().map(|(_, _, _, path)| *path).collect();
    assert_eq!(decided_paths, expected_paths);

    // The non-path axes.
    for (id, arguments) in [
        ("m1", r#"{"to": "attacker@evil.com"}"#),
        ("m2", r#"{"to": "bob@yourco.com.evil"}"#),
        ("m3", r#"{"to": "a@yourco.com", "count": 99}"#),
        ("m4", r#"{"to": "a@yourco.com", "count": 0}"#),
        ("m5", r#"{"to": "a@yourco.com", "bcc": "leak@evil.com"}"#),
        ("m6", r#"{"to": "a@yourco.com", "kind": "other"}"#),
        // Wrong-typed and unrepresentable arguments fail closed.
        ("m7", r#"{"to": "a@yourco.com", "count": "3"}"#),
        ("m8", r#"{"to": "a@yourco.com", "count": 3.5}"#),
        ("m9", r#"{"to": "a@yourco.com", "count": -0}"#),
        ("m10", r#"{"to": 7}"#),
    ] {
        let reply = call(&mut h, id, "send_mail", arguments).await;
        assert_denied_by_policy(&reply, id);
    }

    // ---- absence: byte-identical to a tool that does not exist ----
    //
    // An expired grant is no grant, an ungranted tool is no tool, and a
    // tool no upstream offers is no tool. All three answer the same bytes
    // (**W6**), which is what makes the filtered list consistent rather
    // than an oracle.
    h.set_now(2_000);
    let expired = call(&mut h, "a1", "expiring", "{}").await;
    let ungranted = call(&mut h, "a2", "ungranted_tool", "{}").await;
    let absent = call(&mut h, "a3", "no_such_tool", "{}").await;
    for (reply, tool) in [
        (&expired, "expiring"),
        (&ungranted, "ungranted_tool"),
        (&absent, "no_such_tool"),
    ] {
        assert!(reply.get("result").is_none(), "{reply}");
        assert_eq!(reply["error"]["code"], -32602);
        assert_eq!(reply["error"]["message"], format!("Unknown tool: {tool}"));
    }

    // ---- the upstream saw none of it ----
    //
    // Proved deterministically: the *first* frame the upstream ever
    // receives after the handshake is this allowed call.
    h.set_now(1_000);
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": "ok", "method": "tools/call", "params": {"name": "read_file", "arguments": {"path": "/data/invoices/2026-01.pdf"}}}"#)
        .await;
    let forwarded = h.upstreams[0].recv_json().await;
    assert_eq!(forwarded["method"], "tools/call");
    assert_eq!(
        forwarded["params"]["arguments"]["path"],
        "/data/invoices/2026-01.pdf"
    );
    let upstream_id = forwarded["id"].as_i64().unwrap();
    h.upstreams[0]
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": {upstream_id}, "result": {{"content": []}}}}"#
        ))
        .await;
    assert_eq!(h.client.recv_json().await["id"], "ok");

    // ---- and the trace says the same thing ----
    let events = h.events();
    let verdicts = decisions(&events);
    let expected: Vec<(String, String)> = rows
        .iter()
        .map(|(_, tool, _, _)| ((*tool).to_owned(), "out_of_envelope".to_owned()))
        .chain((0..10).map(|_| ("send_mail".to_owned(), "out_of_envelope".to_owned())))
        .chain([
            ("expiring".to_owned(), "expired".to_owned()),
            ("ungranted_tool".to_owned(), "not_granted".to_owned()),
            ("read_file".to_owned(), "allow:0".to_owned()),
        ])
        .collect();
    assert_eq!(verdicts, expected);

    // `no_such_tool` never reached a decision: it was refused at routing,
    // which is exactly why the client cannot tell the two apart.
    let refusals: Vec<(Option<String>, RefusalReason)> = events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::CallRefused {
                principal,
                tool,
                reason,
                ..
            } => {
                assert_eq!(principal.as_str(), TEST_PRINCIPAL);
                Some((tool.clone(), *reason))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        refusals,
        vec![(Some("no_such_tool".to_owned()), RefusalReason::UnknownTool)]
    );

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
    assert_eq!(
        summary.frames_to_upstream, 1,
        "exactly one call crossed to the upstream"
    );
}

/// The path-flavored rows that must be **allowed** — normalization is
/// not only a way to deny.
#[tokio::test]
async fn normalized_paths_inside_the_prefix_are_allowed() {
    let mut h = gate_session().await;

    for (id, tool, arguments) in [
        (
            "p1",
            "read_file",
            r#"{"path": "/data/invoices/2026-01.pdf"}"#,
        ),
        (
            "p2",
            "read_file",
            r#"{"path": "/data/./invoices//2026-01.pdf"}"#,
        ),
        (
            "p3",
            "read_file",
            r#"{"path": "/data/invoices/sub/../2026-01.pdf"}"#,
        ),
        (
            "p4",
            "read_win",
            r#"{"path": "C:\\Users\\me\\Desktop\\notes.txt"}"#,
        ),
        (
            "p5",
            "read_win",
            r#"{"path": "C:/Users/me/Desktop/notes.txt"}"#,
        ),
        // Case-varied spellings of the same Windows file: one resource,
        // one decision.
        (
            "p6",
            "read_win",
            r#"{"path": "c:\\users\\me\\desktop\\notes.txt"}"#,
        ),
        (
            "p7",
            "read_win",
            r#"{"path": "C:\\USERS\\ME\\DESKTOP\\NOTES.TXT"}"#,
        ),
    ] {
        h.client
            .send(&format!(
                r#"{{"jsonrpc": "2.0", "id": "{id}", "method": "tools/call", "params": {{"name": "{tool}", "arguments": {arguments}}}}}"#
            ))
            .await;
        let forwarded = h.upstreams[0].recv_json().await;
        let upstream_id = forwarded["id"].as_i64().unwrap();
        h.upstreams[0]
            .send(&format!(
                r#"{{"jsonrpc": "2.0", "id": {upstream_id}, "result": {{"content": []}}}}"#
            ))
            .await;
        assert_eq!(h.client.recv_json().await["id"], id);
    }

    let events = h.events();
    assert!(
        decisions(&events)
            .iter()
            .all(|(_, v)| v.starts_with("allow")),
        "{:?}",
        decisions(&events)
    );

    // p1 was already in normal form, so nothing is duplicated for it; the
    // six that were rewritten each keep the spelling they arrived with.
    // The rows in order: p1 p2 p3 p4 p5 p6 p7.
    let sent: Vec<bool> = events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::CallDecided { args_as_sent, .. } => Some(!args_as_sent.is_empty()),
            _ => None,
        })
        .collect();
    assert_eq!(
        sent,
        [false, true, true, true, true, true, true],
        "only a value normalization actually changed gets an `args_as_sent` entry"
    );

    let summary = finish(h).await;
    assert_eq!(summary.frames_to_upstream, 7);
}

/// **W5** — the decision is made on the normalized value while the
/// client's own bytes are what cross to the upstream.
#[tokio::test]
async fn normalization_changes_the_decision_never_the_frame() {
    let mut h = gate_session().await;

    let params = r#"{"name": "read_file", "arguments": {"path": "/data/./invoices//sub/../2026-01.pdf", "note": "kept"}, "_meta": {"progressToken": "t"}}"#;
    h.client
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": "w5", "method": "tools/call", "params": {params}}}"#
        ))
        .await;
    let forwarded = h.upstreams[0].recv().await;
    let forwarded_text = text_of(&forwarded).to_owned();
    assert!(
        forwarded_text.contains(&format!(r#""params":{params}"#)),
        "the client's bytes were rewritten: {forwarded_text}"
    );

    // …while the decision, and the record of it, used the normalized
    // form.
    assert_eq!(last_decided_path(&h.events()), "/data/invoices/2026-01.pdf");

    // And because that form is lossy, the record also keeps the spelling
    // the client sent — otherwise this call and a plain read of the same
    // file would be one line in the log, and an auditor could not tell a
    // traversal attempt from an ordinary request.
    let sent = h
        .events()
        .iter()
        .rev()
        .find_map(|event| match event {
            TraceEvent::CallDecided { args_as_sent, .. } => Some(args_as_sent.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        sent.get("path").map(String::as_str),
        Some("/data/./invoices//sub/../2026-01.pdf")
    );
    assert!(
        !sent.contains_key("note"),
        "`note` is not path-flavored, so nothing about it changed"
    );

    let upstream_id: Value = serde_json::from_slice(&forwarded).unwrap();
    let upstream_id = upstream_id["id"].as_i64().unwrap();
    h.upstreams[0]
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": {upstream_id}, "result": {{"content": []}}}}"#
        ))
        .await;
    assert_eq!(h.client.recv_json().await["id"], "w5");
    assert!(finish(h).await.clean_shutdown());
}

/// **W2** — a tool is shown iff some live grant names it, and the tool
/// axis is the only axis: a tool whose every call would be denied is
/// still listed, because whether a call is in the envelope depends on
/// arguments that do not exist at list time.
#[tokio::test]
async fn the_list_shows_the_tool_axis_and_expiry_moves_it() {
    let mut h = gate_session().await;

    let before = list_names(&mut h).await;
    assert_eq!(
        before,
        ["read_file", "read_win", "send_mail", "expiring", "echo"],
        "ungranted_tool is offered upstream and must not be shown"
    );

    // The grant for `expiring` lapses mid-session: no re-connect, no
    // re-list — the same clock read that denies its calls removes it from
    // the list (core's INV-3, one fact with two faces).
    h.set_now(2_000);
    let after = list_names(&mut h).await;
    assert_eq!(after, ["read_file", "read_win", "send_mail", "echo"]);

    let listed: Vec<(u64, u64, i64)> = h
        .events()
        .iter()
        .filter_map(|event| match event {
            TraceEvent::ToolsListed {
                offered,
                granted,
                now,
                ..
            } => Some((*offered, *granted, now.unix_secs())),
            _ => None,
        })
        .collect();
    assert_eq!(listed, vec![(6, 5, 1_000), (6, 4, 2_000)]);
    assert!(finish(h).await.clean_shutdown());
}

/// Duplicate keys are refused rather than resolved: a JSON parser may
/// take either reading, the frame crosses the proxy byte-faithfully, and
/// the upstream runs its own parser — so deciding on one of them is a
/// guess about someone else's.
#[tokio::test]
async fn duplicate_argument_keys_are_refused_and_never_forwarded() {
    let mut h = gate_session().await;

    for (id, params) in [
        (
            "dup1",
            r#"{"name": "read_file", "arguments": {"path": "/etc/passwd", "path": "/data/invoices/ok.pdf"}}"#,
        ),
        (
            "dup2",
            r#"{"name": "read_file", "name": "echo", "arguments": {}}"#,
        ),
        ("bad1", r#"{"name": "read_file", "arguments": 42}"#),
        ("bad2", r#"{"name": "read_file", "arguments": [1]}"#),
        ("bad3", r#"{"name": 7}"#),
    ] {
        h.client
            .send(&format!(
                r#"{{"jsonrpc": "2.0", "id": "{id}", "method": "tools/call", "params": {params}}}"#
            ))
            .await;
        let reply = h.client.recv_json().await;
        assert_eq!(reply["id"], id);
        assert_eq!(reply["error"]["code"], -32602);
        assert_eq!(reply["error"]["message"], "Invalid params");
    }

    // Nothing was decided, everything was refused, and the tool name
    // survived into the trace wherever it could be read unambiguously.
    let events = h.events();
    assert!(decisions(&events).is_empty());
    let refusals: Vec<(Option<String>, RefusalReason)> = events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::CallRefused { tool, reason, .. } => Some((tool.clone(), *reason)),
            _ => None,
        })
        .collect();
    assert_eq!(
        refusals,
        vec![
            (Some("read_file".to_owned()), RefusalReason::MalformedParams),
            // Two `name` members: neither reading may be preferred, so
            // the trace records none.
            (None, RefusalReason::MalformedParams),
            (Some("read_file".to_owned()), RefusalReason::MalformedParams),
            (Some("read_file".to_owned()), RefusalReason::MalformedParams),
            (None, RefusalReason::MalformedParams),
        ]
    );

    // The upstream's first frame is still an ordinary call.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": "ok", "method": "tools/call", "params": {"name": "echo", "arguments": {}}}"#)
        .await;
    assert_eq!(h.upstreams[0].recv_json().await["method"], "tools/call");
    let summary = finish(h).await;
    assert_eq!(summary.frames_to_upstream, 1);
}

/// An engine that cannot evaluate denies, fail closed — and says so to
/// the operator, never to the client.
///
/// This answer cannot be reached from a grant file: `flavium-policy`'s P5
/// makes it unreachable by construction, and the reference semantics
/// never produce it at all. So the one test that pins its shape drives
/// the gate with an authorizer that returns it unconditionally.
#[tokio::test]
async fn an_evaluation_error_reads_like_a_policy_denial_and_does_not_end_the_session() {
    struct AlwaysFails;

    impl Authorizer for AlwaysFails {
        fn authorize(&self, _: &Principal, _: &ToolCall, _: Timestamp) -> Decision {
            Decision::Deny(DenialReason::EvaluationError {
                detail: "context build failed".into(),
            })
        }

        fn granted_tools(&self, _: &Principal, _: Timestamp) -> BTreeSet<ToolName> {
            BTreeSet::from([ToolName::new("echo").unwrap()])
        }
    }

    let mut h = spawn(
        test_config(),
        1,
        Some(wire_authorizer(
            envelope(vec![grant("echo")]),
            Arc::new(AlwaysFails),
            1_000,
        )),
    );
    boot_with_tools(&mut h.upstreams[0], "fixture", &["echo"]).await;
    client_handshake(&mut h).await;

    let failed = call(&mut h, "e1", "echo", "{}").await;
    // Byte-identical to an out-of-envelope denial: the client learns
    // nothing about engine internals, and answering `-32603` would tell
    // the agent the failure is not its fault, which invites a retry loop.
    assert_denied_by_policy(&failed, "e1");

    // The session continues: an engine failure denied the call, so
    // authority held. (Contrast a *sink* failure, which does end it.)
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": "ping", "method": "ping"}"#)
        .await;
    assert_eq!(h.client.recv_json().await["id"], "ping");

    // The detail reaches the operator through the trace, and only there.
    assert_eq!(
        decisions(&h.events()),
        vec![(
            "echo".to_owned(),
            "evaluation_error:context build failed".to_owned()
        )]
    );
    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
    assert_eq!(summary.frames_to_upstream, 0);
}

/// Accepts `accept` events, then fails every time — a full disk.
struct FailingSink {
    accept: usize,
    seen: AtomicUsize,
}

impl TraceSink for FailingSink {
    fn record(&self, _event: &TraceEvent) -> Result<(), SinkError> {
        if self.seen.fetch_add(1, Ordering::SeqCst) < self.accept {
            Ok(())
        } else {
            Err(Box::<dyn std::error::Error + Send + Sync>::from(
                "no space left on device",
            ))
        }
    }
}

/// A session whose sink accepts exactly `accept` events, over one
/// upstream offering `echo`.
fn failing_sink_session(accept: usize) -> Harness {
    let enforcement = Enforcement {
        envelope: envelope(vec![grant("echo")]),
        authorizer: Arc::new(envelope(vec![grant("echo")])),
        sink: Arc::new(FailingSink {
            accept,
            seen: AtomicUsize::new(0),
        }),
        clock: Arc::new(SystemClock),
        path_flavors: PathFlavors::new(),
    };
    spawn_raw(test_config(), 1, Some(enforcement))
}

async fn expect_trace_failed(h: Harness) {
    let mut h = h;
    // No further client frame is answered, and the session ends.
    h.client.expect_eof().await;
    let summary = tokio::time::timeout(std::time::Duration::from_secs(5), h.router)
        .await
        .expect("router did not end after the sink failed")
        .expect("router task panicked")
        .expect("router returned an error");
    assert_eq!(summary.end, SessionEnd::TraceFailed);
    assert!(
        !summary.clean_shutdown(),
        "a session that could not be recorded is not a clean one"
    );
    assert_eq!(
        summary.frames_to_upstream, 0,
        "nothing may cross after the audit record failed"
    );
}

/// **W3** — a sink that refuses an event stops the session. A full disk
/// should stop the agent, not run it unrecorded.
#[tokio::test]
async fn a_sink_failure_on_a_list_ends_the_session() {
    // SessionStarted and HandshakeCompleted get through; the ToolsListed
    // of the first tools/list does not.
    let mut h = failing_sink_session(2);
    boot_with_tools(&mut h.upstreams[0], "fixture", &["echo"]).await;
    client_handshake(&mut h).await;
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}"#)
        .await;
    expect_trace_failed(h).await;
}

/// The same, at the one moment it matters most: the sink refuses the
/// `CallDecided { Allow }`, so the call must **not** be forwarded. This
/// is what makes "trace, then act" more than a comment.
#[tokio::test]
async fn a_sink_failure_on_an_allow_forwards_nothing() {
    let mut h = failing_sink_session(2);
    boot_with_tools(&mut h.upstreams[0], "fixture", &["echo"]).await;
    client_handshake(&mut h).await;
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "echo", "arguments": {}}}"#)
        .await;
    expect_trace_failed(h).await;
}

/// **W6, the other half.** A reused request id is refused *before* the
/// tool table is consulted, so `-32600` is the answer whatever the call
/// names. Otherwise the pair of codes would tell a client which tools
/// exist upstream — precisely the set the filtered list hides.
#[tokio::test]
async fn a_reused_id_answers_the_same_whatever_tool_it_names() {
    let mut h = gate_session().await;

    // Park a call on id "x" so the id is in flight.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": "x", "method": "tools/call", "params": {"name": "echo", "arguments": {}}}"#)
        .await;
    h.upstreams[0].recv_json().await;

    // Three second calls on the same id: a granted tool, a tool the
    // upstream offers but no grant names, and a tool nobody offers.
    let mut replies = Vec::new();
    for tool in ["echo", "ungranted_tool", "no_such_tool"] {
        h.client
            .send(&format!(
                r#"{{"jsonrpc": "2.0", "id": "x", "method": "tools/call", "params": {{"name": "{tool}", "arguments": {{}}}}}}"#
            ))
            .await;
        replies.push(h.client.recv_json().await);
    }
    for reply in &replies {
        assert_eq!(reply["error"]["code"], -32600, "{reply}");
        assert_eq!(reply["error"]["message"], "Request id is already in flight");
    }
    assert_eq!(replies[0], replies[1]);
    assert_eq!(replies[1], replies[2]);

    // All three are refusals, none is a decision — the tool table was
    // never consulted, so nothing about it leaked.
    let events = h.events();
    let refusals: Vec<(Option<String>, RefusalReason)> = events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::CallRefused { tool, reason, .. } => Some((tool.clone(), *reason)),
            _ => None,
        })
        .collect();
    assert_eq!(
        refusals,
        vec![
            (Some("echo".to_owned()), RefusalReason::DuplicateRequestId),
            (
                Some("ungranted_tool".to_owned()),
                RefusalReason::DuplicateRequestId
            ),
            (
                Some("no_such_tool".to_owned()),
                RefusalReason::DuplicateRequestId
            ),
        ]
    );
    assert_eq!(decisions(&events).len(), 1, "only the parked call decided");
    assert_eq!(finish(h).await.frames_to_upstream, 1);
}

/// The trace of one ordinary session, end to end: the envelope, the
/// handshake, a list, an allow with its completion, a denial, a refusal,
/// and the ending — in causal order, from one task.
#[tokio::test]
async fn a_session_records_a_complete_causal_transcript() {
    let mut h = gate_session().await;

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}"#)
        .await;
    h.client.recv_json().await;

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "echo", "arguments": {"text": "hi"}}}"#)
        .await;
    let forwarded = h.upstreams[0].recv_json().await;
    let upstream_id = forwarded["id"].as_i64().unwrap();
    h.upstreams[0]
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": {upstream_id}, "result": {{"content": [], "isError": true}}}}"#
        ))
        .await;
    assert_eq!(h.client.recv_json().await["id"], 2);

    call(&mut h, "3", "read_file", r#"{"path": "/etc/passwd"}"#).await;
    call(&mut h, "4", "no_such_tool", "{}").await;
    // A frame that never parses, and a notification with nowhere to go.
    h.client.send_raw(b"{not json\n").await;
    h.client.recv_json().await;
    h.client
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}"#)
        .await;
    // Ordering proof: the ping's answer cannot arrive before the
    // notification has been consumed.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 5, "method": "ping"}"#)
        .await;
    assert_eq!(h.client.recv_json().await["id"], 5);

    let (summary, events) = finish_traced(h).await;
    assert!(summary.clean_shutdown());
    assert_eq!(
        support::event_names(&events),
        vec![
            "SessionStarted",
            "HandshakeCompleted",
            "ToolsListed",
            "CallDecided",
            "CallCompleted",
            "CallDecided",
            "CallRefused",
            "FrameRejected",
            "FrameDiscarded",
            "SessionEnded",
        ]
    );

    match &events[0] {
        TraceEvent::SessionStarted { envelope } => {
            assert_eq!(envelope.principal.as_str(), TEST_PRINCIPAL);
            assert_eq!(envelope.grants.len(), 5);
            assert_eq!(envelope.grants[0].tool.as_str(), "read_file");
        }
        other => panic!("expected SessionStarted, got {other:?}"),
    }
    match &events[1] {
        TraceEvent::HandshakeCompleted {
            offered_protocol_version,
            protocol_version,
            client_name,
            ..
        } => {
            assert_eq!(offered_protocol_version, support::PINNED_PROTOCOL_VERSION);
            assert_eq!(protocol_version, support::PINNED_PROTOCOL_VERSION);
            assert_eq!(client_name.as_deref(), Some("scripted-client"));
        }
        other => panic!("expected HandshakeCompleted, got {other:?}"),
    }
    // The allow and its completion share one call id; the denial gets a
    // fresh one, and no completion.
    let (allow_id, completed_id) = match (&events[3], &events[4]) {
        (
            TraceEvent::CallDecided {
                call_id, decision, ..
            },
            TraceEvent::CallCompleted {
                call_id: completed,
                outcome,
                ..
            },
        ) => {
            assert!(decision.is_allow());
            assert_eq!(*outcome, CallOutcome::Result { is_error: true });
            (*call_id, *completed)
        }
        other => panic!("expected a decision and its completion, got {other:?}"),
    };
    assert_eq!(allow_id, completed_id);
    match &events[8] {
        TraceEvent::FrameDiscarded { kind } => {
            assert_eq!(*kind, DiscardKind::UnroutableNotification)
        }
        other => panic!("expected FrameDiscarded, got {other:?}"),
    }
    match &events[9] {
        TraceEvent::SessionEnded {
            reason,
            undelivered,
            delivery_failed,
        } => {
            assert_eq!(*reason, flavium_core::SessionEndReason::ClientEof);
            assert_eq!(*undelivered, 0);
            assert!(!delivery_failed);
        }
        other => panic!("expected SessionEnded, got {other:?}"),
    }
}

/// Every allowed call gets exactly one terminal event, including the ones
/// that never come back: a cancellation, and a session that ends while a
/// call is in flight.
#[tokio::test]
async fn cancelled_and_abandoned_calls_are_completed_too() {
    let mut h = gate_session().await;

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": "cancel-me", "method": "tools/call", "params": {"name": "echo", "arguments": {}}}"#)
        .await;
    h.upstreams[0].recv_json().await;
    h.client
        .send(r#"{"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": "cancel-me"}}"#)
        .await;
    h.upstreams[0].recv_json().await;

    // A second call is left in flight when the client hangs up.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": "abandon-me", "method": "tools/call", "params": {"name": "echo", "arguments": {}}}"#)
        .await;
    h.upstreams[0].recv_json().await;

    let (_, events) = finish_traced(h).await;
    let outcomes: Vec<CallOutcome> = events
        .iter()
        .filter_map(|event| match event {
            TraceEvent::CallCompleted { outcome, .. } => Some(*outcome),
            _ => None,
        })
        .collect();
    assert_eq!(
        outcomes,
        vec![CallOutcome::Cancelled, CallOutcome::Abandoned]
    );
    // …and every decision has one.
    let decided = events
        .iter()
        .filter(|e| matches!(e, TraceEvent::CallDecided { .. }))
        .count();
    assert_eq!(decided, outcomes.len());
}

/// `--unenforced` is the M1/M2 middlebox: no filter, no gate, no trace.
/// It exists so that "transparent" is something an operator asks for
/// rather than something a missing config section produces.
#[tokio::test]
async fn an_unenforced_session_forwards_everything_and_records_nothing() {
    let mut h = spawn(test_config(), 1, None);
    boot_with_tools(&mut h.upstreams[0], "fixture", OFFERED).await;
    client_handshake(&mut h).await;

    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}"#)
        .await;
    let list = h.client.recv_json().await;
    assert_eq!(
        list["result"]["tools"].as_array().unwrap().len(),
        OFFERED.len(),
        "an unenforced session hides nothing"
    );

    // A call that every envelope above would deny is simply forwarded.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "read_file", "arguments": {"path": "/etc/passwd"}}}"#)
        .await;
    let forwarded = h.upstreams[0].recv_json().await;
    assert_eq!(forwarded["params"]["arguments"]["path"], "/etc/passwd");
    let upstream_id = forwarded["id"].as_i64().unwrap();
    h.upstreams[0]
        .send(&format!(
            r#"{{"jsonrpc": "2.0", "id": {upstream_id}, "result": {{"content": []}}}}"#
        ))
        .await;
    assert_eq!(h.client.recv_json().await["id"], 2);

    // Malformed params are still refused — enforcement is about
    // authority, not about tolerating an ambiguous frame.
    h.client
        .send(r#"{"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "read_file", "arguments": {"path": "/a", "path": "/b"}}}"#)
        .await;
    assert_eq!(h.client.recv_json().await["error"]["code"], -32602);

    let summary = finish(h).await;
    assert!(summary.clean_shutdown());
    assert_eq!(summary.frames_to_upstream, 1);
}

/// A grant naming a tool no upstream offers is kept and reported: it
/// admits nothing, which costs availability and never authority.
#[tokio::test]
async fn a_grant_for_an_unoffered_tool_does_not_stop_the_session() {
    let mut h = spawn(
        test_config(),
        1,
        Some(wire(
            envelope(vec![grant("echo"), grant("typo_tool")]),
            1_000,
        )),
    );
    boot_with_tools(&mut h.upstreams[0], "fixture", &["echo"]).await;
    client_handshake(&mut h).await;

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
    assert_eq!(names, ["echo"], "a granted tool nobody offers is not shown");

    // Calling it is an ordinary unknown tool.
    let reply = call(&mut h, "1", "typo_tool", "{}").await;
    assert_eq!(reply["error"]["message"], "Unknown tool: typo_tool");
    assert!(finish(h).await.clean_shutdown());
}
