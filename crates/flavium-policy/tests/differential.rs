//! **P1 (agreement)**: the Cedar engine decides exactly what the reference
//! semantics specify — the same [`Decision`], the same grant index, the same
//! [`DenialReason`] — for every envelope, call and time.
//!
//! This is the milestone's headline property and the reason the enforcement
//! story is not "we read the compiler and it looked right". The generators
//! mirror `flavium-core`'s property suite: the same deterministic SplitMix64
//! source, the same small universes (3 tools, 3 arguments, a short-string
//! alphabet that includes `*`, `\` and a multibyte character, the `i64`
//! sentinels, and times spanning every expiry), so the cases that make
//! `decide` interesting are the cases Cedar is asked about.
//!
//! A run that never allowed anything would pass vacuously, so every test
//! counts its non-vacuous outcomes and asserts a floor.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use cedar_policy::PolicyId;
use flavium_core::{
    decide, granted_tools, ArgValue, Authorizer, Constraint, Decision, Grant, GrantEnvelope,
    Principal, Timestamp, ToolCall, ToolName,
};
use flavium_policy::CedarAuthorizer;

// ---------------------------------------------------------------------------
// Deterministic randomness (SplitMix64, as in flavium-core's suite)
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        assert!(n > 0);
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, numerator: usize, denominator: usize) -> bool {
        self.below(denominator) < numerator
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

// ---------------------------------------------------------------------------
// Universes
// ---------------------------------------------------------------------------

/// The alphabet of `flavium-core`'s suite: separators, letters, an address
/// character, and the three that make Cedar interesting — the wildcard `*`,
/// the escape `\`, and a multibyte character.
const TOKENS: [&str; 9] = ["", "/", "a", "b", "@", ".", "*", "\\", "é"];

/// A second alphabet the core suite has no reason to carry: characters that
/// are hostile to *text*. Nothing here can change a policy's meaning, because
/// no policy is ever built from text (**P4**) — this run is what turns that
/// claim into a test.
const HOSTILE_TOKENS: [&str; 10] = ["\"", "\\", "*", "\n", "\u{0}", "::", "é", "😀", " ", "a"];

const ARGS: [&str; 3] = ["x", "y", "z"];
const INTS: [i64; 11] = [-4, -3, -2, -1, 0, 1, 2, 3, 4, i64::MIN, i64::MAX];
const TIMES: [i64; 7] = [0, 1, 2, 3, 4, 5, 6];

/// The tools, argument names and token alphabet one run draws from.
struct Universe {
    tools: Vec<&'static str>,
    ungranted: &'static str,
    args: Vec<&'static str>,
    tokens: Vec<&'static str>,
    times: Vec<Timestamp>,
}

impl Universe {
    /// The core suite's universe.
    fn plain() -> Self {
        Universe {
            tools: vec!["a", "b", "c"],
            ungranted: "d",
            args: ARGS.to_vec(),
            tokens: TOKENS.to_vec(),
            times: TIMES
                .iter()
                .map(|&t| Timestamp::from_unix_secs(t))
                .collect(),
        }
    }

    /// Names and values that are hostile as text: quotes, backslashes,
    /// wildcards, `::`, an argument name that is the empty string, and
    /// `__expr` — one of the three names Cedar's *JSON* value parser reserves
    /// as an escape. `__expr` is here because it is the one that actually bit:
    /// while the request context was built as JSON, a call whose only string
    /// argument was named `__expr` made Cedar reject the context outright, so
    /// the engine denied a call the specification allowed. Building the
    /// context with `RestrictedExpression` removed the whole class; this
    /// universe is the regression guard.
    fn hostile() -> Self {
        Universe {
            tools: vec!["a\"b", "a\\b", "Flavium::Tool::\"x\""],
            ungranted: "d*",
            args: vec!["", "a\"b", "__expr"],
            tokens: HOSTILE_TOKENS.to_vec(),
            times: TIMES
                .iter()
                .map(|&t| Timestamp::from_unix_secs(t))
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// A string of 0–3 tokens, biased short so that prefix and suffix relations
/// between generated strings are common rather than one in hundreds.
fn gen_string(rng: &mut Rng, u: &Universe) -> String {
    let count = [0, 1, 1, 1, 2, 2, 3, 3][rng.below(8)];
    let mut text = String::new();
    for _ in 0..count {
        text.push_str(rng.pick(&u.tokens));
    }
    text
}

fn gen_int(rng: &mut Rng) -> i64 {
    *rng.pick(&INTS)
}

fn gen_bound(rng: &mut Rng) -> Option<i64> {
    if rng.chance(1, 3) {
        None
    } else {
        Some(gen_int(rng))
    }
}

fn gen_constraint(rng: &mut Rng, u: &Universe) -> Constraint {
    match rng.below(5) {
        0 => Constraint::Prefix(gen_string(rng, u)),
        1 => Constraint::Suffix(gen_string(rng, u)),
        2 => {
            let n = rng.below(3);
            Constraint::OneOf((0..n).map(|_| gen_string(rng, u)).collect())
        }
        3 => Constraint::Range {
            min: gen_bound(rng),
            max: gen_bound(rng),
        },
        _ => Constraint::Absent,
    }
}

fn gen_grant(rng: &mut Rng, u: &Universe) -> Grant {
    let mut constraints = BTreeMap::new();
    for arg in &u.args {
        if rng.chance(1, 2) {
            constraints.insert((*arg).to_string(), gen_constraint(rng, u));
        }
    }
    let tool = *rng.pick(&u.tools);
    Grant {
        tool: ToolName::new(tool).unwrap(),
        constraints,
        expires: if rng.chance(1, 3) {
            None
        } else {
            Some(*rng.pick(&u.times))
        },
    }
}

fn gen_grants(rng: &mut Rng, u: &Universe) -> Vec<Grant> {
    let n = rng.below(4);
    (0..n).map(|_| gen_grant(rng, u)).collect()
}

fn gen_value(rng: &mut Rng, u: &Universe) -> ArgValue {
    match rng.below(10) {
        0..=4 => ArgValue::Str(gen_string(rng, u)),
        5..=7 => ArgValue::Int(gen_int(rng)),
        _ => ArgValue::Other,
    }
}

fn gen_call(rng: &mut Rng, u: &Universe) -> ToolCall {
    let tool = if rng.chance(1, 8) {
        u.ungranted
    } else {
        rng.pick(&u.tools)
    };
    let mut args = BTreeMap::new();
    for arg in &u.args {
        if rng.chance(3, 4) {
            args.insert((*arg).to_string(), gen_value(rng, u));
        }
    }
    ToolCall {
        tool: tool.to_string(),
        args,
    }
}

// ---------------------------------------------------------------------------
// The property
// ---------------------------------------------------------------------------

/// Everything a failure needs in order to be reproduced by hand.
fn report(
    envelope: &GrantEnvelope,
    engine: &CedarAuthorizer,
    call: &ToolCall,
    now: Timestamp,
) -> String {
    let mut compiled = String::new();
    for index in 0..envelope.grants.len() {
        let id = PolicyId::new(index.to_string());
        match engine.policies().policy(&id) {
            Some(policy) => compiled.push_str(&format!("  [{index}] {policy}\n")),
            None => compiled.push_str(&format!("  [{index}] <missing>\n")),
        }
    }
    format!(
        "envelope = {:#?}\ncall = {call:#?}\nnow = {now}\ncompiled =\n{compiled}",
        envelope
    )
}

/// How many non-vacuous outcomes a run produced, so a regression that makes
/// the property trivially true is caught by the floors instead of passing.
#[derive(Default, Debug)]
struct Coverage {
    allows: usize,
    out_of_envelope: usize,
    expired: usize,
    not_granted: usize,
    /// Allows naming a grant other than index 0 — the case where "lowest
    /// determining id" is doing real work.
    later_grant_allows: usize,
    /// Allows by a grant that is one of several matching — likewise.
    multi_match_allows: usize,
}

impl Coverage {
    fn record(&mut self, decision: &Decision, admitting: usize) {
        match decision {
            Decision::Allow { grant } => {
                self.allows += 1;
                if *grant > 0 {
                    self.later_grant_allows += 1;
                }
                if admitting > 1 {
                    self.multi_match_allows += 1;
                }
            }
            Decision::Deny(reason) => match reason {
                flavium_core::DenialReason::OutOfEnvelope => self.out_of_envelope += 1,
                flavium_core::DenialReason::Expired => self.expired += 1,
                flavium_core::DenialReason::NotGranted => self.not_granted += 1,
                flavium_core::DenialReason::EvaluationError { detail } => {
                    panic!("the reference semantics never produce an evaluation error: {detail}")
                }
            },
        }
    }
}

/// Runs the property over `envelopes` random envelopes, `calls` calls each,
/// at every time in the universe, and returns what it covered.
fn run(seed: u64, u: &Universe, envelopes: usize, calls: usize) -> Coverage {
    let mut rng = Rng::new(seed);
    let holder = Principal::new("bot").unwrap();
    let stranger = Principal::new("stranger").unwrap();
    let mut coverage = Coverage::default();

    for _ in 0..envelopes {
        let envelope = GrantEnvelope {
            principal: holder.clone(),
            grants: gen_grants(&mut rng, u),
        };
        let engine = CedarAuthorizer::new(envelope.clone())
            .unwrap_or_else(|error| panic!("compile failed: {error}\n{envelope:#?}"));

        let calls: Vec<ToolCall> = (0..calls).map(|_| gen_call(&mut rng, u)).collect();
        for call in &calls {
            for &now in &u.times {
                let specified = decide(&envelope.grants, call, now);
                let engine_says = engine.authorize(&holder, call, now);
                assert_eq!(
                    engine_says,
                    specified,
                    "P1 violated: Cedar says {engine_says:?}, the specification says {specified:?}\n{}",
                    report(&envelope, &engine, call, now)
                );
                coverage.record(&specified, envelope.admitting_grants(call, now).len());

                // A principal that is not the holder holds nothing, whatever
                // the call is (**P3**).
                assert_eq!(
                    engine.authorize(&stranger, call, now),
                    Decision::Deny(flavium_core::DenialReason::NotGranted),
                    "a stranger was answered as the holder\n{}",
                    report(&envelope, &engine, call, now)
                );
            }
        }

        // The tool axis agrees too (**INV-3**), for the holder and for
        // everyone else.
        for &now in &u.times {
            assert_eq!(
                Authorizer::granted_tools(&engine, &holder, now),
                granted_tools(&envelope.grants, now),
                "granted_tools disagreed at {now}\n{envelope:#?}"
            );
            assert_eq!(
                Authorizer::granted_tools(&engine, &stranger, now),
                BTreeSet::new(),
                "a stranger was shown tools at {now}"
            );
        }
    }
    coverage
}

#[test]
fn p1_cedar_agrees_with_the_specification() {
    let coverage = run(0xC0DE_0001, &Universe::plain(), 800, 32);
    // Floors, so the property cannot quietly become vacuous. Each is set at
    // roughly half of what the run produces today: they exist to catch a
    // collapse (a generator that stops producing allows, a universe whose
    // strings stop overlapping), not to pin exact counts. Multi-match allows
    // are genuinely rare here — 0–3 grants over 3 tools — which is why
    // `p1_holds_when_many_grants_compete_for_one_tool` carries that load.
    assert!(coverage.allows >= 4_000, "too few allows: {coverage:?}");
    assert!(
        coverage.out_of_envelope >= 15_000,
        "too few out-of-envelope denials: {coverage:?}"
    );
    assert!(
        coverage.expired >= 10_000,
        "too few expiry denials: {coverage:?}"
    );
    assert!(
        coverage.not_granted >= 50_000,
        "too few not-granted denials: {coverage:?}"
    );
    assert!(
        coverage.later_grant_allows >= 1_500,
        "too few allows by a grant other than the first: {coverage:?}"
    );
    assert!(
        coverage.multi_match_allows >= 50,
        "too few allows with several matching grants: {coverage:?}"
    );
}

/// The same property over names and values that are hostile as text: quotes,
/// backslashes, wildcards, `::`, NUL, a newline, an emoji, and an argument
/// whose name is the empty string. Cedar never sees any of it as syntax
/// (**P4**), so the answers must be the same ones the specification gives.
#[test]
fn p1_holds_for_names_and_values_that_are_hostile_as_text() {
    let coverage = run(0xC0DE_0002, &Universe::hostile(), 500, 24);
    assert!(coverage.allows >= 1_800, "too few allows: {coverage:?}");
    assert!(
        coverage.out_of_envelope >= 8_000,
        "too few out-of-envelope denials: {coverage:?}"
    );
    assert!(
        coverage.expired >= 4_000,
        "too few expiry denials: {coverage:?}"
    );
    assert!(
        coverage.not_granted >= 25_000,
        "too few not-granted denials: {coverage:?}"
    );
    assert!(
        coverage.later_grant_allows >= 800,
        "too few allows by a grant other than the first: {coverage:?}"
    );
}

/// Envelopes made only of grants for one tool, so that every call reaches
/// Cedar with several candidate policies and the lowest-index rule is under
/// constant pressure.
#[test]
fn p1_holds_when_many_grants_compete_for_one_tool() {
    let u = Universe {
        tools: vec!["a"],
        ungranted: "d",
        args: ARGS.to_vec(),
        tokens: TOKENS.to_vec(),
        times: TIMES
            .iter()
            .map(|&t| Timestamp::from_unix_secs(t))
            .collect(),
    };
    let mut rng = Rng::new(0xC0DE_0003);
    let holder = Principal::new("bot").unwrap();
    let mut multi = 0;

    for _ in 0..150 {
        // 1–12 grants: more than ten, so the ids span the lexical trap where
        // "10" sorts before "2".
        let count = 1 + rng.below(12);
        let envelope = GrantEnvelope {
            principal: holder.clone(),
            grants: (0..count).map(|_| gen_grant(&mut rng, &u)).collect(),
        };
        let engine = CedarAuthorizer::new(envelope.clone()).unwrap();
        for _ in 0..16 {
            let call = gen_call(&mut rng, &u);
            for &now in &u.times {
                let specified = decide(&envelope.grants, &call, now);
                assert_eq!(
                    engine.authorize(&holder, &call, now),
                    specified,
                    "{}",
                    report(&envelope, &engine, &call, now)
                );
                if envelope.admitting_grants(&call, now).len() > 1 {
                    multi += 1;
                }
            }
        }
    }
    assert!(multi >= 1_000, "too few multi-match calls: {multi}");
}
