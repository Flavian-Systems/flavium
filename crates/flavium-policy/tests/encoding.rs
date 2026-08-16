//! What gets *denied*, and what a grant actually compiles to.
//!
//! The differential test proves the engine and the specification agree; this
//! one pins the individual rows a reader wants to check by eye — the per-axis
//! denial table that is T1's acceptance criterion, the Cedar encodings that
//! could silently drift, and the rendered text of a representative grant so
//! that a change to the compiler is visible in a diff.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use cedar_policy::PolicyId;
use flavium_core::{
    decide, ArgValue, Authorizer, Constraint, Decision, DenialReason, Grant, GrantEnvelope,
    Principal, Timestamp, ToolCall, ToolName,
};
use flavium_policy::{compile, CedarAuthorizer};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn t(secs: i64) -> Timestamp {
    Timestamp::from_unix_secs(secs)
}

fn bot() -> Principal {
    Principal::new("bot").unwrap()
}

fn grant(tool: &str, constraints: &[(&str, Constraint)], expires: Option<i64>) -> Grant {
    Grant {
        tool: ToolName::new(tool).unwrap(),
        constraints: constraints
            .iter()
            .map(|(name, constraint)| (name.to_string(), constraint.clone()))
            .collect(),
        expires: expires.map(t),
    }
}

fn envelope(grants: Vec<Grant>) -> GrantEnvelope {
    GrantEnvelope {
        principal: bot(),
        grants,
    }
}

fn engine(grants: Vec<Grant>) -> CedarAuthorizer {
    CedarAuthorizer::new(envelope(grants)).unwrap()
}

fn call(tool: &str, args: &[(&str, ArgValue)]) -> ToolCall {
    ToolCall {
        tool: tool.to_string(),
        args: args
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect(),
    }
}

fn s(text: &str) -> ArgValue {
    ArgValue::Str(text.to_string())
}

fn one_of(members: &[&str]) -> Constraint {
    Constraint::OneOf(members.iter().map(|m| (*m).to_string()).collect())
}

/// Asserts the engine's answer, and that the specification says the same
/// thing — every row of every table below is a differential case too.
#[track_caller]
fn assert_decision(grants: Vec<Grant>, call: &ToolCall, now: Timestamp, expected: Decision) {
    let envelope = envelope(grants);
    let engine = CedarAuthorizer::new(envelope.clone()).unwrap();
    let engine_says = engine.authorize(&bot(), call, now);
    assert_eq!(
        engine_says, expected,
        "engine disagreed\nenvelope = {envelope:#?}\ncall = {call:#?}\nnow = {now}"
    );
    assert_eq!(
        decide(&envelope.grants, call, now),
        expected,
        "the specification disagreed\nenvelope = {envelope:#?}\ncall = {call:#?}\nnow = {now}"
    );
}

#[track_caller]
fn assert_denied(grants: Vec<Grant>, call: &ToolCall, now: Timestamp, reason: DenialReason) {
    assert_decision(grants, call, now, Decision::Deny(reason));
}

// ---------------------------------------------------------------------------
// The per-axis denial table — T1's acceptance criterion at the policy layer
// ---------------------------------------------------------------------------

/// One row per way a call can fall outside its envelope, each with the exact
/// [`DenialReason`] the client will be answered with.
#[test]
fn every_axis_denies_with_the_right_reason() {
    let mail = || {
        grant(
            "send_mail",
            &[
                ("to", Constraint::Suffix("@yourco.com".into())),
                ("bcc", Constraint::Absent),
            ],
            None,
        )
    };
    let read = || {
        grant(
            "read_file",
            &[("path", Constraint::Prefix("/data/invoices/".into()))],
            None,
        )
    };
    let page = || {
        grant(
            "list_page",
            &[(
                "n",
                Constraint::Range {
                    min: Some(1),
                    max: Some(10),
                },
            )],
            None,
        )
    };

    // Path outside its prefix.
    assert_denied(
        vec![read()],
        &call("read_file", &[("path", s("/etc/passwd"))]),
        t(0),
        DenialReason::OutOfEnvelope,
    );
    // A prefix is bytes, not path components — the documented sharp edge.
    assert_decision(
        vec![read()],
        &call(
            "read_file",
            &[("path", s("/data/invoices/../../etc/passwd"))],
        ),
        t(0),
        Decision::Allow { grant: 0 },
    );

    // Off-pattern recipient, and the near-miss that a careless suffix admits.
    assert_denied(
        vec![mail()],
        &call("send_mail", &[("to", s("attacker@evil.com"))]),
        t(0),
        DenialReason::OutOfEnvelope,
    );
    assert_denied(
        vec![mail()],
        &call("send_mail", &[("to", s("alice@yourco.com.evil"))]),
        t(0),
        DenialReason::OutOfEnvelope,
    );

    // Out-of-range number, both ends.
    for n in [0, 11, i64::MIN, i64::MAX] {
        assert_denied(
            vec![page()],
            &call("list_page", &[("n", ArgValue::Int(n))]),
            t(0),
            DenialReason::OutOfEnvelope,
        );
    }

    // Expired grant — whatever the arguments are.
    assert_denied(
        vec![grant(
            "send_mail",
            &[("to", Constraint::Suffix("@yourco.com".into()))],
            Some(5),
        )],
        &call("send_mail", &[("to", s("alice@yourco.com"))]),
        t(5),
        DenialReason::Expired,
    );

    // Ungranted tool.
    assert_denied(
        vec![read()],
        &call("delete_file", &[("path", s("/data/invoices/1"))]),
        t(0),
        DenialReason::NotGranted,
    );

    // An argument of a type the core does not model.
    assert_denied(
        vec![read()],
        &call("read_file", &[("path", ArgValue::Other)]),
        t(0),
        DenialReason::OutOfEnvelope,
    );

    // A missing constrained argument.
    assert_denied(
        vec![read()],
        &call("read_file", &[]),
        t(0),
        DenialReason::OutOfEnvelope,
    );

    // `Absent` violated — the constraint that closes the `bcc` hole.
    assert_denied(
        vec![mail()],
        &call(
            "send_mail",
            &[("to", s("alice@yourco.com")), ("bcc", s("x@evil.com"))],
        ),
        t(0),
        DenialReason::OutOfEnvelope,
    );
    // … including when the smuggled value is of an unmodelled type.
    assert_denied(
        vec![mail()],
        &call(
            "send_mail",
            &[("to", s("alice@yourco.com")), ("bcc", ArgValue::Other)],
        ),
        t(0),
        DenialReason::OutOfEnvelope,
    );
    // The same call without `bcc` is allowed, so the row above is about
    // `bcc` and not about something else being wrong.
    assert_decision(
        vec![mail()],
        &call("send_mail", &[("to", s("alice@yourco.com"))]),
        t(0),
        Decision::Allow { grant: 0 },
    );

    // An unconstrained argument is not examined — the authoring pitfall.
    assert_decision(
        vec![mail()],
        &call(
            "send_mail",
            &[("to", s("alice@yourco.com")), ("cc", s("x@evil.com"))],
        ),
        t(0),
        Decision::Allow { grant: 0 },
    );
}

/// A wrongly-typed argument must deny *and* leave Cedar with nothing to
/// complain about: an evaluation error would be a decision flavium has to
/// make without the engine, and the two implementations would part company on
/// exactly the inputs an attacker controls.
#[test]
fn a_wrongly_typed_argument_denies_with_no_cedar_error() {
    let string_constrained = grant("t", &[("x", Constraint::Prefix("/a".into()))], None);
    let int_constrained = grant(
        "t",
        &[(
            "x",
            Constraint::Range {
                min: Some(0),
                max: Some(10),
            },
        )],
        None,
    );
    let wrong_for_strings = [ArgValue::Int(5), ArgValue::Other];
    let wrong_for_ints = [s("5"), ArgValue::Other];

    for (grants, wrong) in [
        (vec![string_constrained], wrong_for_strings.to_vec()),
        (vec![int_constrained], wrong_for_ints.to_vec()),
    ] {
        for value in wrong {
            let engine = engine(grants.clone());
            let decision = engine.authorize(&bot(), &call("t", &[("x", value.clone())]), t(0));
            assert_eq!(
                decision,
                Decision::Deny(DenialReason::OutOfEnvelope),
                "value {value:?} must deny as out-of-envelope, not as an engine failure"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Cedar-specific rows
// ---------------------------------------------------------------------------

/// A `*` or a `\` inside a grant is a plain character, not a wildcard and not
/// an escape. This is the property the structured `like` pattern buys, and
/// the one a text-built policy would get wrong.
#[test]
fn wildcards_and_backslashes_in_a_grant_match_literally() {
    let prefixed = vec![grant(
        "t",
        &[("x", Constraint::Prefix("/a*b\\c".into()))],
        None,
    )];
    assert_decision(
        prefixed.clone(),
        &call("t", &[("x", s("/a*b\\c/d"))]),
        t(0),
        Decision::Allow { grant: 0 },
    );
    for miss in ["/aQQb\\c/d", "/ab\\c/d", "/a*bQc/d", "/a*b/c"] {
        assert_denied(
            prefixed.clone(),
            &call("t", &[("x", s(miss))]),
            t(0),
            DenialReason::OutOfEnvelope,
        );
    }

    let suffixed = vec![grant(
        "t",
        &[("x", Constraint::Suffix("*.txt".into()))],
        None,
    )];
    assert_decision(
        suffixed.clone(),
        &call("t", &[("x", s("report*.txt"))]),
        t(0),
        Decision::Allow { grant: 0 },
    );
    assert_denied(
        suffixed,
        &call("t", &[("x", s("report.txt"))]),
        t(0),
        DenialReason::OutOfEnvelope,
    );

    // A grant that is *only* a wildcard still matches only a literal `*`.
    let star = vec![grant("t", &[("x", one_of(&["*"]))], None)];
    assert_decision(
        star.clone(),
        &call("t", &[("x", s("*"))]),
        t(0),
        Decision::Allow { grant: 0 },
    );
    assert_denied(
        star,
        &call("t", &[("x", s("anything"))]),
        t(0),
        DenialReason::OutOfEnvelope,
    );
}

/// The empty prefix and suffix admit every *string* — and nothing else.
#[test]
fn empty_prefix_and_suffix_admit_every_string_only() {
    for constraint in [
        Constraint::Prefix(String::new()),
        Constraint::Suffix(String::new()),
    ] {
        let grants = vec![grant("t", &[("x", constraint.clone())], None)];
        for text in ["", "anything", "\\*", "é"] {
            assert_decision(
                grants.clone(),
                &call("t", &[("x", s(text))]),
                t(0),
                Decision::Allow { grant: 0 },
            );
        }
        for other in [ArgValue::Int(0), ArgValue::Other] {
            assert_denied(
                grants.clone(),
                &call("t", &[("x", other)]),
                t(0),
                DenialReason::OutOfEnvelope,
            );
        }
        // Missing is not a string either.
        assert_denied(grants, &call("t", &[]), t(0), DenialReason::OutOfEnvelope);
    }
}

#[test]
fn an_empty_one_of_admits_nothing() {
    let grants = vec![grant(
        "t",
        &[("x", Constraint::OneOf(BTreeSet::new()))],
        None,
    )];
    for value in [s(""), s("anything"), ArgValue::Int(0), ArgValue::Other] {
        assert_denied(
            grants.clone(),
            &call("t", &[("x", value)]),
            t(0),
            DenialReason::OutOfEnvelope,
        );
    }
    assert_denied(grants, &call("t", &[]), t(0), DenialReason::OutOfEnvelope);
}

#[test]
fn one_of_is_exact_and_carries_hostile_members() {
    let members = ["a", "", "\"", "\\", "*", "é", "a\nb"];
    let grants = vec![grant("t", &[("x", one_of(&members))], None)];
    for member in members {
        assert_decision(
            grants.clone(),
            &call("t", &[("x", s(member))]),
            t(0),
            Decision::Allow { grant: 0 },
        );
    }
    for miss in ["aa", "b", "\\\\", "**", "a\nb\n"] {
        assert_denied(
            grants.clone(),
            &call("t", &[("x", s(miss))]),
            t(0),
            DenialReason::OutOfEnvelope,
        );
    }
}

#[test]
fn ranges_cover_the_i64_extremes_and_the_empty_case() {
    let unbounded = vec![grant(
        "t",
        &[(
            "x",
            Constraint::Range {
                min: None,
                max: None,
            },
        )],
        None,
    )];
    for n in [i64::MIN, -1, 0, 1, i64::MAX] {
        assert_decision(
            unbounded.clone(),
            &call("t", &[("x", ArgValue::Int(n))]),
            t(0),
            Decision::Allow { grant: 0 },
        );
    }
    // An unbounded range is still a *type* constraint: only integers.
    assert_denied(
        unbounded.clone(),
        &call("t", &[("x", s("0"))]),
        t(0),
        DenialReason::OutOfEnvelope,
    );
    assert_denied(
        unbounded,
        &call("t", &[]),
        t(0),
        DenialReason::OutOfEnvelope,
    );

    // One-sided bounds, at the extremes.
    let at_least_max = vec![grant(
        "t",
        &[(
            "x",
            Constraint::Range {
                min: Some(i64::MAX),
                max: None,
            },
        )],
        None,
    )];
    assert_decision(
        at_least_max.clone(),
        &call("t", &[("x", ArgValue::Int(i64::MAX))]),
        t(0),
        Decision::Allow { grant: 0 },
    );
    assert_denied(
        at_least_max,
        &call("t", &[("x", ArgValue::Int(i64::MAX - 1))]),
        t(0),
        DenialReason::OutOfEnvelope,
    );

    // `min > max` admits nothing.
    let empty = vec![grant(
        "t",
        &[(
            "x",
            Constraint::Range {
                min: Some(5),
                max: Some(3),
            },
        )],
        None,
    )];
    for n in [2, 3, 4, 5, 6] {
        assert_denied(
            empty.clone(),
            &call("t", &[("x", ArgValue::Int(n))]),
            t(0),
            DenialReason::OutOfEnvelope,
        );
    }
}

/// The expiry boundary is exclusive: at `now == expires` the grant is gone
/// (**INV-3**), and Cedar's strict `<` must agree with the core's.
#[test]
fn the_expiry_boundary_is_exclusive_on_both_sides() {
    let grants = vec![grant("t", &[], Some(10))];
    assert_decision(
        grants.clone(),
        &call("t", &[]),
        t(9),
        Decision::Allow { grant: 0 },
    );
    for now in [10, 11] {
        assert_denied(
            grants.clone(),
            &call("t", &[]),
            t(now),
            DenialReason::Expired,
        );
    }
    // The extremes of the time axis.
    assert_decision(
        vec![grant("t", &[], None)],
        &call("t", &[]),
        t(i64::MAX),
        Decision::Allow { grant: 0 },
    );
    assert_decision(
        vec![grant("t", &[], Some(i64::MAX))],
        &call("t", &[]),
        t(i64::MAX - 1),
        Decision::Allow { grant: 0 },
    );
    assert_denied(
        vec![grant("t", &[], Some(i64::MIN))],
        &call("t", &[]),
        t(i64::MIN),
        DenialReason::Expired,
    );
}

/// A tool name containing `"` or `\` compiles and matches — the `from_json`
/// path. `EntityUid::from_str` fails outright on these, which is why no part
/// of the compiler goes through text.
#[test]
fn tool_and_principal_names_that_are_hostile_as_text_still_work() {
    for name in ["a\"b", "a\\b", "Flavium::Tool::\"x\"", "*", "é", "a b"] {
        let holder = Principal::new(name).unwrap();
        let envelope = GrantEnvelope {
            principal: holder.clone(),
            grants: vec![grant(name, &[("x", Constraint::Prefix("/p".into()))], None)],
        };
        let engine = CedarAuthorizer::new(envelope.clone()).unwrap();

        assert_eq!(
            engine.authorize(&holder, &call(name, &[("x", s("/pq"))]), t(0)),
            Decision::Allow { grant: 0 },
            "name {name:?}"
        );
        assert_eq!(
            engine.authorize(&holder, &call(name, &[("x", s("/zz"))]), t(0)),
            Decision::Deny(DenialReason::OutOfEnvelope),
            "name {name:?}"
        );
        // A different tool is not granted, however similar it looks.
        assert_eq!(
            engine.authorize(&holder, &call("other", &[("x", s("/pq"))]), t(0)),
            Decision::Deny(DenialReason::NotGranted),
            "name {name:?}"
        );
        // And a different principal holds nothing.
        assert_eq!(
            engine.authorize(&bot(), &call(name, &[("x", s("/pq"))]), t(0)),
            Decision::Deny(DenialReason::NotGranted),
            "name {name:?}"
        );
    }
}

/// Argument names are map keys, not identifiers: a grant may constrain an
/// argument called `""`, `"context"` or `"a\"b"` and Cedar must read it as
/// data.
///
/// `__expr`, `__entity` and `__extn` are the three names Cedar's *JSON* value
/// parser reserves as escapes, and they are the reason the request context is
/// not built as JSON: a call whose only string argument was named `__expr`
/// used to make Cedar reject the whole context, denying a call the
/// specification allows. Each name below is tested as the call's *only*
/// argument, which is exactly the shape that triggered it.
#[test]
fn argument_names_that_are_not_identifiers_still_compile_and_match() {
    for name in [
        "", "a b", "a\"b", "context", "if", "0", "é", "\n", "__expr", "__entity", "__extn", "str",
        "int", "present", "now",
    ] {
        let grants = vec![grant("t", &[(name, Constraint::Prefix("/p".into()))], None)];
        assert_decision(
            grants.clone(),
            &call("t", &[(name, s("/pq"))]),
            t(0),
            Decision::Allow { grant: 0 },
        );
        assert_denied(
            grants.clone(),
            &call("t", &[(name, s("/zz"))]),
            t(0),
            DenialReason::OutOfEnvelope,
        );
        assert_denied(grants, &call("t", &[]), t(0), DenialReason::OutOfEnvelope);
    }
}

/// Several grants may admit one call; the answer names the lowest index, so
/// the trace points an auditor at the grant that actually authorized it.
#[test]
fn several_matching_grants_answer_with_the_lowest_index() {
    let grants = vec![
        grant("t", &[("x", Constraint::Prefix("/a".into()))], Some(5)),
        grant("t", &[("x", Constraint::Prefix("/".into()))], None),
        grant("t", &[], None),
    ];
    // All three admit at t(0) …
    assert_decision(
        grants.clone(),
        &call("t", &[("x", s("/ab"))]),
        t(0),
        Decision::Allow { grant: 0 },
    );
    // … but grant 0 has expired by t(5), so the next one answers.
    assert_decision(
        grants.clone(),
        &call("t", &[("x", s("/ab"))]),
        t(5),
        Decision::Allow { grant: 1 },
    );
    // A value only the unconstrained grant admits.
    assert_decision(
        grants,
        &call("t", &[("x", ArgValue::Int(1))]),
        t(0),
        Decision::Allow { grant: 2 },
    );
}

/// A grant for one tool lends no authority over another, however permissive
/// it is.
#[test]
fn authority_does_not_leak_across_tools() {
    let grants = vec![
        grant("open", &[], None),
        grant(
            "send",
            &[("to", Constraint::Suffix("@yourco.com".into()))],
            None,
        ),
    ];
    assert_decision(
        grants.clone(),
        &call("open", &[("to", s("attacker@evil.com"))]),
        t(0),
        Decision::Allow { grant: 0 },
    );
    assert_denied(
        grants,
        &call("send", &[("to", s("attacker@evil.com"))]),
        t(0),
        DenialReason::OutOfEnvelope,
    );
}

// ---------------------------------------------------------------------------
// Compile-time behaviour
// ---------------------------------------------------------------------------

#[test]
fn compiling_produces_one_policy_per_grant_named_by_its_index() {
    let envelope = envelope(vec![
        grant("a", &[], None),
        grant("b", &[("x", Constraint::Absent)], Some(1)),
        grant("a", &[("y", one_of(&["q"]))], None),
    ]);
    let policies = compile(&envelope).unwrap();
    assert_eq!(policies.policies().count(), 3);
    for index in 0..3 {
        assert!(
            policies.policy(&PolicyId::new(index.to_string())).is_some(),
            "no policy named {index}"
        );
    }
    // P2: an envelope with no grants compiles to a set that allows nothing.
    assert_eq!(compile(&envelope0()).unwrap().policies().count(), 0);
}

fn envelope0() -> GrantEnvelope {
    envelope(vec![])
}

/// A grant with many constrained arguments compiles and decides correctly.
///
/// Sixteen is an ordinary tool signature, and it used to abort the process:
/// the `when` condition was folded into a left spine of `&&`, Cedar's parse of
/// that JSON is recursive, and the stack overflowed — not a panic that could
/// be caught and denied, but a `STATUS_STACK_OVERFLOW` that takes the proxy
/// down. Splitting the conjunction down the middle makes the depth `log2(N)`;
/// 4096 constrained arguments is well past anything a real tool has and is
/// here to show the failure mode is gone rather than pushed out.
#[test]
fn a_grant_with_many_constrained_arguments_compiles_and_decides() {
    for count in [16usize, 64, 4096] {
        let mut constraints = BTreeMap::new();
        for i in 0..count {
            constraints.insert(format!("a{i:05}"), Constraint::Prefix("/p".into()));
        }
        let grants = vec![Grant {
            tool: ToolName::new("t").unwrap(),
            constraints,
            expires: Some(t(9)),
        }];
        let engine = CedarAuthorizer::new(envelope(grants.clone())).unwrap();

        let mut args = BTreeMap::new();
        for i in 0..count {
            args.insert(format!("a{i:05}"), ArgValue::Str("/p/ok".into()));
        }
        let all_good = ToolCall {
            tool: "t".into(),
            args,
        };
        assert_eq!(
            engine.authorize(&bot(), &all_good, t(0)),
            Decision::Allow { grant: 0 },
            "{count} constraints"
        );
        assert_eq!(
            engine.authorize(&bot(), &all_good, t(0)),
            decide(&grants, &all_good, t(0))
        );

        // One argument off its prefix is enough to deny.
        let mut one_bad = all_good.clone();
        one_bad
            .args
            .insert("a00000".to_string(), ArgValue::Str("/nope".into()));
        assert_eq!(
            engine.authorize(&bot(), &one_bad, t(0)),
            Decision::Deny(DenialReason::OutOfEnvelope),
            "{count} constraints"
        );
        assert_eq!(
            engine.authorize(&bot(), &one_bad, t(0)),
            decide(&grants, &one_bad, t(0))
        );
    }
}

/// Any envelope the core can build compiles: the constraint kinds are a
/// closed vocabulary, and every value in them is data.
#[test]
fn every_constraint_kind_compiles() {
    let all_kinds = grant(
        "t",
        &[
            ("p", Constraint::Prefix("/a*b\\c".into())),
            ("s", Constraint::Suffix("\"@x.com".into())),
            ("o", one_of(&["a", "", "\\"])),
            ("e", Constraint::OneOf(BTreeSet::new())),
            (
                "r",
                Constraint::Range {
                    min: Some(i64::MIN),
                    max: Some(i64::MAX),
                },
            ),
            (
                "u",
                Constraint::Range {
                    min: None,
                    max: None,
                },
            ),
            ("a", Constraint::Absent),
        ],
        Some(i64::MAX),
    );
    assert!(compile(&envelope(vec![all_kinds])).is_ok());
}

/// The rendered Cedar text of a representative grant, so a reviewer can see
/// what a grant becomes and any change to the encoding shows up in the diff.
///
/// Read it as: the scope pins principal, action and tool; then one guarded
/// conjunct per constraint in argument-name order, then the expiry. The six
/// conjuncts — `bcc`, `depth`, `kind`, `name`, `path`, expiry — are grouped
/// as a balanced tree rather than a left spine, which is what keeps a grant
/// with many constrained arguments from overflowing Cedar's recursive parse:
/// `(bcc && (depth && kind)) && (name && (path && expiry))`.
#[test]
fn a_representative_grant_renders_as_expected() {
    let envelope = envelope(vec![grant(
        "read_file",
        &[
            (
                "depth",
                Constraint::Range {
                    min: Some(1),
                    max: Some(9),
                },
            ),
            ("bcc", Constraint::Absent),
            ("path", Constraint::Prefix("/data/".into())),
            ("kind", one_of(&["pdf", "csv"])),
            ("name", Constraint::Suffix(".txt".into())),
        ],
        Some(1_800_000_000),
    )]);
    let policies = compile(&envelope).unwrap();
    let rendered = policies.policy(&PolicyId::new("0")).unwrap().to_string();
    assert_eq!(
        rendered,
        concat!(
            "permit(",
            "principal == Flavium::Principal::\"bot\", ",
            "action == Flavium::Action::\"call\", ",
            "resource == Flavium::Tool::\"read_file\"",
            ") when { ",
            // bcc && (depth && kind)
            "((!((context.present).contains(\"bcc\"))) && ",
            "((((context.int) has depth) && ((1 <= ((context.int).depth)) && (((context.int).depth) <= 9))) && ",
            "(((context.str) has kind) && ([\"csv\", \"pdf\"].contains((context.str).kind))))) && ",
            // name && (path && expiry)
            "((((context.str) has name) && (((context.str).name) like \"*.txt\")) && ",
            "((((context.str) has path) && (((context.str).path) like \"/data/*\")) && ",
            "((context.now) < 1800000000)))",
            " };"
        ),
        "the compiled form changed — check that the change is intended:\n{rendered}"
    );
}

/// A grant with no constraints and no expiry is the literal `true`: it admits
/// every call on its tool, which is what the specification says too.
#[test]
fn an_unconstrained_grant_renders_as_true() {
    let policies = compile(&envelope(vec![grant("t", &[], None)])).unwrap();
    let rendered = policies.policy(&PolicyId::new("0")).unwrap().to_string();
    assert_eq!(
        rendered,
        "permit(principal == Flavium::Principal::\"bot\", \
         action == Flavium::Action::\"call\", \
         resource == Flavium::Tool::\"t\") when { true };"
    );
}

/// Cedar never sees the client's tool string: an envelope compiled for one
/// tool answers `NotGranted` for every other spelling of it, including ones
/// that could never be a valid [`ToolName`].
#[test]
fn a_clients_arbitrary_tool_string_never_reaches_the_engine() {
    let engine = engine(vec![grant("read_file", &[], None)]);
    for tool in [
        "read_file ",
        " read_file",
        "READ_FILE",
        "read\nfile",
        "",
        "*",
    ] {
        assert_eq!(
            engine.authorize(&bot(), &call(tool, &[]), t(0)),
            Decision::Deny(DenialReason::NotGranted),
            "tool {tool:?}"
        );
    }
    assert_eq!(
        engine.authorize(&bot(), &call("read_file", &[]), t(0)),
        Decision::Allow { grant: 0 }
    );
}

/// `granted_tools` is what a `tools/list` may show, and it must agree with
/// `authorize` on the tool axis (**INV-3**).
#[test]
fn granted_tools_lists_live_tools_and_agrees_with_authorize() {
    let engine = engine(vec![
        grant("a", &[], Some(5)),
        grant("a", &[], None),
        grant("b", &[], Some(5)),
        grant("c", &[("x", Constraint::Absent)], None),
    ]);
    let names = |now: i64| -> Vec<String> {
        Authorizer::granted_tools(&engine, &bot(), t(now))
            .into_iter()
            .map(|tool| tool.to_string())
            .collect()
    };
    assert_eq!(names(0), vec!["a", "b", "c"]);
    assert_eq!(names(7), vec!["a", "c"]);

    // A tool outside the list is NotGranted or Expired for every call; a tool
    // inside it is never either — even when the call is denied.
    for now in [0, 7] {
        let listed = Authorizer::granted_tools(&engine, &bot(), t(now));
        for tool in ["a", "b", "c", "d"] {
            let decision = engine.authorize(&bot(), &call(tool, &[("x", s("supplied"))]), t(now));
            let hard_denial = matches!(
                decision,
                Decision::Deny(DenialReason::NotGranted | DenialReason::Expired)
            );
            assert_eq!(
                listed.contains(tool),
                !hard_denial,
                "tool {tool} at {now}: listed={:?} decision={decision:?}",
                listed.contains(tool)
            );
        }
    }
    // `c` is listed, yet this call is denied — visibility is not permission.
    assert_eq!(
        engine.authorize(&bot(), &call("c", &[("x", s("supplied"))]), t(0)),
        Decision::Deny(DenialReason::OutOfEnvelope)
    );
}

/// The engine holds its envelope, so M5 can trace the grant an allow names.
#[test]
fn the_envelope_is_reachable_and_allow_indexes_into_it() {
    let grants = vec![
        grant("a", &[("x", Constraint::Prefix("/deny".into()))], None),
        grant("a", &[], None),
    ];
    let engine = engine(grants);
    match engine.authorize(&bot(), &call("a", &[("x", s("/other"))]), t(0)) {
        Decision::Allow { grant } => {
            assert_eq!(engine.envelope().grants[grant].tool.as_str(), "a");
            assert_eq!(grant, 1);
        }
        other => panic!("expected an allow, got {other:?}"),
    }
}

/// Grants whose argument maps differ only in ordering compile to the same
/// policy: `BTreeMap` fixes the iteration order, so the compiled form is a
/// function of the grant and nothing else (**INV-4**).
#[test]
fn compilation_is_deterministic() {
    let mut forward = BTreeMap::new();
    forward.insert("a".to_string(), Constraint::Prefix("/1".into()));
    forward.insert("b".to_string(), Constraint::Suffix("/2".into()));
    let mut backward = BTreeMap::new();
    backward.insert("b".to_string(), Constraint::Suffix("/2".into()));
    backward.insert("a".to_string(), Constraint::Prefix("/1".into()));

    let render = |constraints: BTreeMap<String, Constraint>| {
        let envelope = envelope(vec![Grant {
            tool: ToolName::new("t").unwrap(),
            constraints,
            expires: None,
        }]);
        compile(&envelope)
            .unwrap()
            .policy(&PolicyId::new("0"))
            .unwrap()
            .to_string()
    };
    assert_eq!(render(forward.clone()), render(backward));
    assert_eq!(render(forward.clone()), render(forward));
}
