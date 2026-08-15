//! Property tests for the invariants stated in the crate docs.
//!
//! No test-support dependency (the crate is zero-dep, dev-deps included):
//! a SplitMix64 generator with a fixed seed and small-scope universes give
//! deterministic, reproducible cases. Positive properties derive the child
//! *from* the parent by random tightening steps, so `attenuates` is
//! exercised on real subsets rather than on the rare independent pair that
//! happens to attenuate; an independent-pairs run is kept with an asserted
//! floor of non-vacuous cases so a regression cannot trivialize it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use flavium_core::{
    admitting_grants, attenuates, decide, granted_tools, tool_status, ArgValue, Constraint,
    Decision, DenialReason, Grant, Timestamp, ToolCall, ToolName, ToolStatus,
};

// ---------------------------------------------------------------------------
// Deterministic randomness
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    /// SplitMix64.
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

const TOOLS: [&str; 3] = ["a", "b", "c"];
const UNGRANTED_TOOL: &str = "d";
const ARGS: [&str; 3] = ["x", "y", "z"];
const TOKENS: [&str; 9] = ["", "/", "a", "b", "@", ".", "*", "\\", "é"];
const INTS: [i64; 11] = [-4, -3, -2, -1, 0, 1, 2, 3, 4, i64::MIN, i64::MAX];
const TIMES: [i64; 7] = [0, 1, 2, 3, 4, 5, 6];

struct Universe {
    strings: Vec<String>,
    values: Vec<Option<ArgValue>>,
    times: Vec<Timestamp>,
}

impl Universe {
    fn build() -> Self {
        // Every concatenation of up to three tokens, deduplicated.
        let mut set: BTreeSet<String> = BTreeSet::new();
        set.insert(String::new());
        for a in TOKENS {
            set.insert(a.to_string());
            for b in TOKENS {
                set.insert(format!("{a}{b}"));
                for c in TOKENS {
                    set.insert(format!("{a}{b}{c}"));
                }
            }
        }
        let strings: Vec<String> = set.into_iter().collect();
        let mut values: Vec<Option<ArgValue>> = vec![None, Some(ArgValue::Other)];
        values.extend(INTS.iter().map(|&i| Some(ArgValue::Int(i))));
        values.extend(strings.iter().map(|s| Some(ArgValue::Str(s.clone()))));
        let times = TIMES
            .iter()
            .map(|&t| Timestamp::from_unix_secs(t))
            .collect();
        Universe {
            strings,
            values,
            times,
        }
    }
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// A universe string, biased toward short ones (0–3 tokens with weights
/// 1:3:2:2) so that prefix/suffix relations between generated strings are
/// common rather than one-in-hundreds; a uniform draw over the 585 universe
/// strings would almost always yield three-token strings that are prefixes
/// of nothing else.
fn gen_string(rng: &mut Rng, _u: &Universe) -> String {
    let count = [0, 1, 1, 1, 2, 2, 3, 3][rng.below(8)];
    let mut s = String::new();
    for _ in 0..count {
        let token = *rng.pick(&TOKENS);
        s.push_str(token);
    }
    s
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

fn gen_expiry(rng: &mut Rng, u: &Universe) -> Option<Timestamp> {
    if rng.chance(1, 3) {
        None
    } else {
        Some(*rng.pick(&u.times))
    }
}

fn gen_grant(rng: &mut Rng, u: &Universe) -> Grant {
    let mut constraints = BTreeMap::new();
    for arg in ARGS {
        if rng.chance(1, 2) {
            constraints.insert(arg.to_string(), gen_constraint(rng, u));
        }
    }
    // Deref explicitly: `pick` yields `&&str`, and leaving the coercion to
    // the `&str` parameter confuses rust-analyzer's inference (it guesses
    // `T = str` first), though rustc is fine with it.
    let tool = *rng.pick(&TOOLS);
    Grant {
        tool: ToolName::new(tool).unwrap(),
        constraints,
        expires: gen_expiry(rng, u),
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
        UNGRANTED_TOOL
    } else {
        rng.pick(&TOOLS)
    };
    let mut args = BTreeMap::new();
    for arg in ARGS {
        if rng.chance(3, 4) {
            args.insert(arg.to_string(), gen_value(rng, u));
        }
    }
    ToolCall {
        tool: tool.to_string(),
        args,
    }
}

fn gen_calls(rng: &mut Rng, u: &Universe, n: usize) -> Vec<ToolCall> {
    (0..n).map(|_| gen_call(rng, u)).collect()
}

// ---------------------------------------------------------------------------
// Tightening: derive a child that is a subset by construction
// ---------------------------------------------------------------------------

/// A non-empty-or-empty `OneOf` of universe strings satisfying `keep`
/// (one or two of them; empty only if none qualifies).
fn one_of_matching(rng: &mut Rng, u: &Universe, keep: impl Fn(&str) -> bool) -> Constraint {
    let candidates: Vec<&String> = u.strings.iter().filter(|s| keep(s)).collect();
    let mut set = BTreeSet::new();
    if !candidates.is_empty() {
        for _ in 0..(1 + rng.below(2)) {
            set.insert((*rng.pick(&candidates)).clone());
        }
    }
    Constraint::OneOf(set)
}

/// A random string-kind constraint (what an "admit every string" parent
/// may be tightened to).
fn gen_string_kind(rng: &mut Rng, u: &Universe) -> Constraint {
    match rng.below(3) {
        0 => Constraint::Prefix(gen_string(rng, u)),
        1 => Constraint::Suffix(gen_string(rng, u)),
        _ => one_of_matching(rng, u, |_| true),
    }
}

/// One tightening step on a constraint. Every arm produces a constraint
/// that the input `includes` — same-kind narrowing, or one of the
/// documented cross-kind rows (`Prefix`/`Suffix` ⊇ `OneOf` of matching
/// strings; an empty `Prefix`/`Suffix` ⊇ any string kind).
fn tighten_constraint(rng: &mut Rng, u: &Universe, c: &Constraint) -> Constraint {
    match c {
        Constraint::Prefix(p) => match rng.below(3) {
            0 => Constraint::Prefix(format!("{p}{}", rng.pick(&TOKENS))),
            1 => {
                let p = p.clone();
                one_of_matching(rng, u, |s| s.starts_with(p.as_str()))
            }
            _ if p.is_empty() => gen_string_kind(rng, u),
            _ => Constraint::Prefix(format!("{p}{}", rng.pick(&TOKENS))),
        },
        Constraint::Suffix(s) => match rng.below(3) {
            0 => Constraint::Suffix(format!("{}{s}", rng.pick(&TOKENS))),
            1 => {
                let s = s.clone();
                one_of_matching(rng, u, |v| v.ends_with(s.as_str()))
            }
            _ if s.is_empty() => gen_string_kind(rng, u),
            _ => Constraint::Suffix(format!("{}{s}", rng.pick(&TOKENS))),
        },
        Constraint::OneOf(set) => {
            let mut set = set.clone();
            if !set.is_empty() {
                let victim = set.iter().nth(rng.below(set.len())).unwrap().clone();
                set.remove(&victim);
            }
            Constraint::OneOf(set)
        }
        Constraint::Range { min, max } => {
            let r = gen_int(rng);
            if rng.chance(1, 2) {
                let min = Some(match min {
                    None => r,
                    Some(m) => std::cmp::max(*m, r),
                });
                Constraint::Range { min, max: *max }
            } else {
                let max = Some(match max {
                    None => r,
                    Some(m) => std::cmp::min(*m, r),
                });
                Constraint::Range { min: *min, max }
            }
        }
        Constraint::Absent => Constraint::Absent,
    }
}

fn tighten_grant(rng: &mut Rng, u: &Universe, g: &Grant) -> Grant {
    let mut g = g.clone();
    match rng.below(3) {
        0 => {
            // Constrain (further) one argument.
            let arg = rng.pick(&ARGS).to_string();
            let new = match g.constraints.get(&arg) {
                Some(existing) => tighten_constraint(rng, u, existing),
                None => gen_constraint(rng, u),
            };
            g.constraints.insert(arg, new);
        }
        1 => {
            // Expire no later than before.
            let t = *rng.pick(&u.times);
            g.expires = Some(match g.expires {
                None => t,
                Some(e) => std::cmp::min(e, t),
            });
        }
        _ => {}
    }
    g
}

fn tighten_grants(rng: &mut Rng, u: &Universe, parent: &[Grant]) -> Vec<Grant> {
    let mut child: Vec<Grant> = parent.to_vec();
    let steps = rng.below(5);
    for _ in 0..steps {
        if child.is_empty() {
            break;
        }
        match rng.below(4) {
            0 => {
                let i = rng.below(child.len());
                child.remove(i);
            }
            1 => {
                let i = rng.below(child.len());
                let dup = child[i].clone();
                child.push(dup);
            }
            _ => {
                let i = rng.below(child.len());
                child[i] = tighten_grant(rng, u, &child[i]);
            }
        }
    }
    child
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_live_and_admits(g: &Grant, call: &ToolCall, now: Timestamp) -> bool {
    g.is_live(now) && g.admits(call)
}

/// INV-1 for one (parent, child) pair over sampled calls and every time.
/// Returns how many child-allowed calls were observed (0 = vacuous).
fn check_inv1(parent: &[Grant], child: &[Grant], calls: &[ToolCall], u: &Universe) -> usize {
    let mut allowed = 0;
    for call in calls {
        for &now in &u.times {
            if decide(child, call, now).is_allow() {
                allowed += 1;
                assert!(
                    decide(parent, call, now).is_allow(),
                    "INV-1 violated: child allows {call:?} at {now} but parent denies\nparent = {parent:#?}\nchild = {child:#?}"
                );
            }
        }
    }
    for &now in &u.times {
        let c = granted_tools(child, now);
        let p = granted_tools(parent, now);
        assert!(
            c.is_subset(&p),
            "INV-1b violated at {now}: child tools {c:?} not within parent tools {p:?}"
        );
    }
    allowed
}

// ---------------------------------------------------------------------------
// L1 — constraint inclusion is sound
// ---------------------------------------------------------------------------

/// Which documented row of the `includes` table a `(parent, child)` pair
/// exercises; `"other"` for pairs no row makes true.
fn includes_row(p: &Constraint, c: &Constraint) -> &'static str {
    let string_kind = |k: &Constraint| {
        matches!(
            k,
            Constraint::Prefix(_) | Constraint::Suffix(_) | Constraint::OneOf(_)
        )
    };
    match (p, c) {
        (Constraint::Absent, Constraint::Absent) => "absent",
        (Constraint::Prefix(s), c) if s.is_empty() && string_kind(c) => "all-strings",
        (Constraint::Suffix(s), c) if s.is_empty() && string_kind(c) => "all-strings",
        (Constraint::Prefix(_), Constraint::Prefix(_)) => "prefix",
        (Constraint::Suffix(_), Constraint::Suffix(_)) => "suffix",
        (Constraint::OneOf(_), Constraint::OneOf(_)) => "one-of",
        (Constraint::Prefix(_), Constraint::OneOf(_)) => "prefix-one-of",
        (Constraint::Suffix(_), Constraint::OneOf(_)) => "suffix-one-of",
        (Constraint::Range { .. }, Constraint::Range { .. }) => "range",
        _ => "other",
    }
}

#[test]
fn l1_inclusion_is_sound_over_the_whole_value_universe() {
    let u = Universe::build();
    let mut rng = Rng::new(0x1001);
    // The pool: random constraints plus the two "admit every string"
    // parents, which a random draw would almost never produce.
    let mut pool: Vec<Constraint> = (0..300).map(|_| gen_constraint(&mut rng, &u)).collect();
    pool.push(Constraint::Prefix(String::new()));
    pool.push(Constraint::Suffix(String::new()));

    // Non-trivial inclusions seen per row: child differs from parent and
    // admits at least one universe value. A row whose counter stays low is
    // a row this test does not really exercise.
    let mut rows: BTreeMap<&'static str, usize> = BTreeMap::new();
    let check = |p: &Constraint, c: &Constraint, rows: &mut BTreeMap<&'static str, usize>| {
        if !p.includes(c) {
            return;
        }
        let mut child_admits_something = false;
        for v in &u.values {
            if c.admits(v.as_ref()) {
                child_admits_something = true;
                assert!(
                    p.admits(v.as_ref()),
                    "L1 violated: {p:?} includes {c:?} but only the child admits {v:?}"
                );
            }
        }
        if child_admits_something && p != c {
            *rows.entry(includes_row(p, c)).or_insert(0) += 1;
        }
    };
    for i in 0..6000 {
        let p = rng.pick(&pool).clone();
        // Half the children are derived from the parent by tightening (so
        // every true row of the table is reached with real, unequal
        // operands), half are independent pool picks (so the false rows
        // and whatever happens to be included get their turn too).
        let c = if i % 2 == 0 {
            let mut c = p.clone();
            for _ in 0..(1 + rng.below(3)) {
                c = tighten_constraint(&mut rng, &u, &c);
            }
            c
        } else {
            rng.pick(&pool).clone()
        };
        check(&p, &c, &mut rows);
        // The mirror-image pair exercises the false rows in reverse.
        check(&c, &p, &mut rows);
    }
    // Reflexive on the pool, too.
    for c in &pool {
        assert!(c.includes(c), "includes must be reflexive: {c:?}");
    }
    for row in [
        "prefix",
        "suffix",
        "one-of",
        "prefix-one-of",
        "suffix-one-of",
        "all-strings",
        "range",
    ] {
        let n = rows.get(row).copied().unwrap_or(0);
        assert!(
            n >= 20,
            "row {row:?} exercised only {n} times non-trivially: {rows:?}"
        );
    }
    assert_eq!(
        rows.get("other"),
        None,
        "a pair outside the table was included: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// L2 — grant coverage is sound
// ---------------------------------------------------------------------------

#[test]
fn l2_covers_is_sound() {
    let u = Universe::build();
    let mut rng = Rng::new(0x2002);
    let mut independent_covered = 0;
    for i in 0..3000 {
        let parent = gen_grant(&mut rng, &u);
        // Half derived (always covered), half independent (rarely covered).
        let derived = i % 2 == 0;
        let child = if derived {
            let mut c = parent.clone();
            for _ in 0..rng.below(4) {
                c = tighten_grant(&mut rng, &u, &c);
            }
            c
        } else {
            gen_grant(&mut rng, &u)
        };
        if parent.covers(&child).is_ok() {
            if !derived {
                independent_covered += 1;
            }
            for call in gen_calls(&mut rng, &u, 32) {
                for &now in &u.times {
                    if is_live_and_admits(&child, &call, now) {
                        assert!(
                            is_live_and_admits(&parent, &call, now),
                            "L2 violated for {call:?} at {now}\nparent = {parent:#?}\nchild = {child:#?}"
                        );
                    }
                }
            }
        } else if derived {
            panic!("a derived child must be covered\nparent = {parent:#?}\nchild = {child:#?}");
        }
    }
    assert!(
        independent_covered >= 20,
        "too few independent covered pairs: {independent_covered}"
    );
}

// ---------------------------------------------------------------------------
// INV-1 — attenuation is sound (derived children: never vacuous)
// ---------------------------------------------------------------------------

#[test]
fn inv1_holds_for_derived_children_and_derivation_is_accepted() {
    let u = Universe::build();
    let mut rng = Rng::new(0x3003);
    let mut allowed_total = 0;
    for _ in 0..2000 {
        let parent = gen_grants(&mut rng, &u);
        let child = tighten_grants(&mut rng, &u, &parent);
        assert_eq!(
            attenuates(&parent, &child),
            Ok(()),
            "a child derived by tightening must attenuate\nparent = {parent:#?}\nchild = {child:#?}"
        );
        let calls = gen_calls(&mut rng, &u, 48);
        allowed_total += check_inv1(&parent, &child, &calls, &u);
    }
    assert!(
        allowed_total >= 5000,
        "too few child-allowed calls observed: {allowed_total}"
    );
}

// ---------------------------------------------------------------------------
// INV-1 — attenuation is sound (independent pairs: whatever passes)
// ---------------------------------------------------------------------------

#[test]
fn inv1_holds_for_independent_pairs_that_attenuate() {
    let u = Universe::build();
    let mut rng = Rng::new(0x4004);
    let mut non_vacuous = 0;
    for _ in 0..20_000 {
        let parent = gen_grants(&mut rng, &u);
        let child = gen_grants(&mut rng, &u);
        if attenuates(&parent, &child).is_ok() && !child.is_empty() {
            let calls = gen_calls(&mut rng, &u, 48);
            if check_inv1(&parent, &child, &calls, &u) > 0 {
                non_vacuous += 1;
            }
        }
    }
    assert!(
        non_vacuous >= 50,
        "too few non-vacuous independent cases: {non_vacuous}"
    );
}

// ---------------------------------------------------------------------------
// INV-5 — reflexive and transitive
// ---------------------------------------------------------------------------

#[test]
fn inv5_attenuation_is_reflexive_and_transitive() {
    let u = Universe::build();
    let mut rng = Rng::new(0x5005);
    for _ in 0..3000 {
        let a = gen_grants(&mut rng, &u);
        assert_eq!(attenuates(&a, &a), Ok(()), "reflexivity: {a:#?}");
        let b = tighten_grants(&mut rng, &u, &a);
        let c = tighten_grants(&mut rng, &u, &b);
        assert_eq!(attenuates(&a, &b), Ok(()));
        assert_eq!(attenuates(&b, &c), Ok(()));
        assert_eq!(
            attenuates(&a, &c),
            Ok(()),
            "transitivity over a chain\na = {a:#?}\nc = {c:#?}"
        );
    }
    // Mixed chains: an independent link a ⊇ b (whenever it happens to
    // hold) followed by a derived link b ⊇ c must span a ⊇ c.
    let mut spans = 0;
    for _ in 0..20_000 {
        let a = gen_grants(&mut rng, &u);
        let b = gen_grants(&mut rng, &u);
        if attenuates(&a, &b).is_ok() && !b.is_empty() {
            let c = tighten_grants(&mut rng, &u, &b);
            assert_eq!(attenuates(&b, &c), Ok(()));
            assert_eq!(
                attenuates(&a, &c),
                Ok(()),
                "transitivity\na = {a:#?}\nb = {b:#?}\nc = {c:#?}"
            );
            spans += 1;
        }
    }
    // Fully independent triples: rare, but whenever both links hold, so
    // must the span (no floor — this is a pure consistency check).
    for _ in 0..20_000 {
        let a = gen_grants(&mut rng, &u);
        let b = gen_grants(&mut rng, &u);
        let c = gen_grants(&mut rng, &u);
        if attenuates(&a, &b).is_ok() && attenuates(&b, &c).is_ok() {
            assert_eq!(
                attenuates(&a, &c),
                Ok(()),
                "transitivity\na = {a:#?}\nb = {b:#?}\nc = {c:#?}"
            );
        }
    }
    assert!(spans >= 100, "too few mixed chains: {spans}");
}

// ---------------------------------------------------------------------------
// INV-6 — monotone
// ---------------------------------------------------------------------------

#[test]
fn inv6_attenuation_is_monotone() {
    let u = Universe::build();
    let mut rng = Rng::new(0x6006);
    for _ in 0..3000 {
        let parent = gen_grants(&mut rng, &u);
        let child = tighten_grants(&mut rng, &u, &parent);
        assert_eq!(attenuates(&parent, &child), Ok(()));
        // Adding a grant to the parent keeps the child covered.
        let mut wider_parent = parent.clone();
        wider_parent.push(gen_grant(&mut rng, &u));
        assert_eq!(attenuates(&wider_parent, &child), Ok(()));
        // Removing a grant from the child keeps it covered.
        if !child.is_empty() {
            let mut smaller_child = child.clone();
            smaller_child.remove(rng.below(child.len()));
            assert_eq!(attenuates(&parent, &smaller_child), Ok(()));
        }
        // Tightening the child further keeps it covered.
        let tighter_child = tighten_grants(&mut rng, &u, &child);
        assert_eq!(attenuates(&parent, &tighter_child), Ok(()));
    }
}

// ---------------------------------------------------------------------------
// INV-2 / INV-3 — deny by default; expired is absent; decide's index
// ---------------------------------------------------------------------------

#[test]
fn inv2_empty_grants_and_unnamed_tools_deny() {
    let u = Universe::build();
    let mut rng = Rng::new(0x7007);
    for _ in 0..500 {
        let call = gen_call(&mut rng, &u);
        for &now in &u.times {
            assert_eq!(
                decide(&[], &call, now),
                Decision::Deny(DenialReason::NotGranted)
            );
            assert!(granted_tools(&[], now).is_empty());
        }
        let grants = gen_grants(&mut rng, &u);
        let mut unnamed = call.clone();
        unnamed.tool = UNGRANTED_TOOL.to_string();
        for &now in &u.times {
            assert_eq!(
                decide(&grants, &unnamed, now),
                Decision::Deny(DenialReason::NotGranted)
            );
        }
    }
}

#[test]
fn inv3_tool_status_granted_tools_and_decide_agree() {
    let u = Universe::build();
    let mut rng = Rng::new(0x8008);
    for _ in 0..3000 {
        let grants = gen_grants(&mut rng, &u);
        let calls = gen_calls(&mut rng, &u, 16);
        for &now in &u.times {
            let listed = granted_tools(&grants, now);
            for tool in TOOLS.iter().chain([UNGRANTED_TOOL].iter()) {
                let status = tool_status(&grants, tool, now);
                assert_eq!(
                    listed.contains(*tool),
                    status == ToolStatus::Live,
                    "{grants:#?} {tool} {now}"
                );
                for call in calls.iter().filter(|c| c.tool == *tool) {
                    let d = decide(&grants, call, now);
                    match status {
                        ToolStatus::NotGranted => {
                            assert_eq!(d, Decision::Deny(DenialReason::NotGranted))
                        }
                        ToolStatus::Expired => assert_eq!(d, Decision::Deny(DenialReason::Expired)),
                        ToolStatus::Live => assert!(matches!(
                            d,
                            Decision::Allow { .. } | Decision::Deny(DenialReason::OutOfEnvelope)
                        )),
                    }
                    if let Decision::Allow { grant } = d {
                        // The allowing grant names the called tool — checked
                        // directly, not through `Grant::admits`.
                        assert_eq!(grants[grant].tool.as_str(), call.tool);
                        let admitting = admitting_grants(&grants, call, now);
                        assert_eq!(admitting.first(), Some(&grant));
                        assert!(is_live_and_admits(&grants[grant], call, now));
                        for &i in &admitting {
                            assert_eq!(grants[i].tool.as_str(), call.tool);
                            assert!(is_live_and_admits(&grants[i], call, now));
                        }
                    }
                }
            }
        }
    }
}
