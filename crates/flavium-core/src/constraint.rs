//! Argument constraints: what a grant says about one argument of a call.
//!
//! A [`Constraint`] is attached to an argument *name* inside a
//! [`crate::Grant`] and is checked against the value the call supplies for
//! that name ([`ArgValue`], or `None` when the argument is missing). Two
//! operations are defined, and the second is sound with respect to the
//! first (invariant **L1** in the crate docs):
//!
//! - [`Constraint::admits`] — does this value satisfy the constraint?
//! - [`Constraint::includes`] — does *this* constraint admit every value
//!   *that* constraint admits? This is the per-argument piece of
//!   attenuation.
//!
//! Everything here is byte-wise and fail-closed. Nothing is normalized:
//! `Prefix("/data/inv")` admits `"/data/invalid"` (it is a byte prefix, not
//! a path-component prefix), `Suffix("yourco.com")` admits
//! `"x@evilyourco.com"` (write suffixes with the `@`), and a single string
//! that happens to contain several addresses is still one string. Grant
//! authors write constraints with that in mind; path normalization (`..`,
//! doubled and backslash separators) is done by the caller *before* a
//! value reaches this crate.

use std::collections::BTreeSet;

/// One argument value of a tool call, as the core models it.
///
/// Only strings and integers can be constrained in T1. Every other JSON
/// shape a client may send — floats, booleans, `null`, arrays, objects —
/// is [`ArgValue::Other`]: it is carried so a trace can show the argument
/// was present, but no constraint ever admits it. An argument that is
/// `Other` and unconstrained is simply not looked at.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArgValue {
    /// A JSON string, byte for byte.
    Str(String),
    /// A JSON integer that fits an `i64` (`-0`, `3.0`, `1e3` and values
    /// outside `i64` are `Other`).
    Int(i64),
    /// Any value the core does not model. Never admitted by a constraint.
    Other,
}

/// A constraint on one argument of a tool call.
///
/// The set of values each variant admits, where `v` is the call's value for
/// the constrained argument (`None` = the argument is missing):
///
/// | Variant | Admits `v` iff |
/// |---|---|
/// | `Prefix(p)` | `v` is `Str(s)` and `s` starts with `p` (bytes) |
/// | `Suffix(x)` | `v` is `Str(s)` and `s` ends with `x` (bytes) |
/// | `OneOf(set)` | `v` is `Str(s)` and `s ∈ set` |
/// | `Range{min, max}` | `v` is `Int(n)` and `min ≤ n ≤ max`, a `None` bound being no bound |
/// | `Absent` | `v` is `None` — the argument must not be supplied |
///
/// Consequently a constrained argument that is missing (except under
/// `Absent`), of the wrong type, or [`ArgValue::Other`] is **not** admitted.
/// `Prefix("")` and `Suffix("")` admit every string; `OneOf({})` admits
/// nothing; a `Range` with `min > max` admits nothing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Constraint {
    /// The value is a string starting with these bytes.
    Prefix(String),
    /// The value is a string ending with these bytes.
    Suffix(String),
    /// The value is one of these strings, exactly.
    OneOf(BTreeSet<String>),
    /// The value is an integer within these inclusive bounds; `None` means
    /// unbounded on that side.
    Range {
        /// Lower bound, inclusive.
        min: Option<i64>,
        /// Upper bound, inclusive.
        max: Option<i64>,
    },
    /// The argument must not be present at all.
    Absent,
}

impl Constraint {
    /// Does this constraint admit `value` (`None` = the argument is missing)?
    ///
    /// The table in the type documentation is the specification; this is
    /// its transcription. Total: every `(constraint, value)` pair yields a
    /// `bool`.
    pub fn admits(&self, value: Option<&ArgValue>) -> bool {
        match (self, value) {
            (Constraint::Absent, None) => true,
            (Constraint::Absent, Some(_)) => false,
            (_, None) => false,
            (Constraint::Prefix(prefix), Some(ArgValue::Str(s))) => s.starts_with(prefix.as_str()),
            (Constraint::Suffix(suffix), Some(ArgValue::Str(s))) => s.ends_with(suffix.as_str()),
            (Constraint::OneOf(set), Some(ArgValue::Str(s))) => set.contains(s),
            (Constraint::Range { min, max }, Some(ArgValue::Int(n))) => {
                within_bounds(*n, *min, *max)
            }
            // Type mismatch, or an unmodelled value: fail closed.
            (Constraint::Prefix(_), Some(_))
            | (Constraint::Suffix(_), Some(_))
            | (Constraint::OneOf(_), Some(_))
            | (Constraint::Range { .. }, Some(_)) => false,
        }
    }

    /// Does this constraint admit every value that `other` admits?
    ///
    /// This is a *structural* check: it is sound (invariant **L1**: if it
    /// returns `true`, every value `other` admits, `self` admits) but not
    /// complete (some genuine inclusions return `false`, which only ever
    /// makes a delegation more explicit, never an authorization wider).
    ///
    /// The rows that return `true`, with `self` the (parent) constraint and
    /// `other` the (child) constraint:
    ///
    /// | `self` | `other` | `true` iff |
    /// |---|---|---|
    /// | `Absent` | `Absent` | always |
    /// | `Prefix("")` or `Suffix("")` | any `Prefix`/`Suffix`/`OneOf` | always — the parent admits every string, the child only strings |
    /// | `Prefix(p)` | `Prefix(c)` | `c` starts with `p` |
    /// | `Suffix(p)` | `Suffix(c)` | `c` ends with `p` |
    /// | `OneOf(P)` | `OneOf(C)` | `C ⊆ P` |
    /// | `Prefix(p)` | `OneOf(C)` | every element of `C` starts with `p` |
    /// | `Suffix(p)` | `OneOf(C)` | every element of `C` ends with `p` |
    /// | `Range{pmin, pmax}` | `Range{cmin, cmax}` | `pmin` is no bound or `cmin ≥ pmin`, and `pmax` is no bound or `cmax ≤ pmax` (a child bound of `None` under a parent bound is *not* included) |
    ///
    /// Every other combination is `false`, in particular anything involving
    /// `Absent` on one side only (a child that admits *missing* is wider
    /// than a parent that requires presence, and vice versa), a string kind
    /// against `Range`, and `OneOf` as parent of a `Prefix`/`Suffix` child.
    pub fn includes(&self, other: &Constraint) -> bool {
        match (self, other) {
            (Constraint::Absent, Constraint::Absent) => true,
            (Constraint::Absent, _) | (_, Constraint::Absent) => false,
            // The parent admits every string: any string-kind child is
            // included, whatever it says.
            (Constraint::Prefix(p), child) if p.is_empty() && child.is_string_kind() => true,
            (Constraint::Suffix(p), child) if p.is_empty() && child.is_string_kind() => true,
            (Constraint::Prefix(p), Constraint::Prefix(c)) => c.starts_with(p.as_str()),
            (Constraint::Suffix(p), Constraint::Suffix(c)) => c.ends_with(p.as_str()),
            (Constraint::OneOf(p), Constraint::OneOf(c)) => c.is_subset(p),
            (Constraint::Prefix(p), Constraint::OneOf(c)) => {
                c.iter().all(|v| v.starts_with(p.as_str()))
            }
            (Constraint::Suffix(p), Constraint::OneOf(c)) => {
                c.iter().all(|v| v.ends_with(p.as_str()))
            }
            (
                Constraint::Range {
                    min: pmin,
                    max: pmax,
                },
                Constraint::Range {
                    min: cmin,
                    max: cmax,
                },
            ) => lower_bound_within(*pmin, *cmin) && upper_bound_within(*pmax, *cmax),
            _ => false,
        }
    }

    /// True for the variants that admit only strings.
    fn is_string_kind(&self) -> bool {
        matches!(
            self,
            Constraint::Prefix(_) | Constraint::Suffix(_) | Constraint::OneOf(_)
        )
    }
}

/// `min ≤ n ≤ max`, where a `None` bound is no bound.
fn within_bounds(n: i64, min: Option<i64>, max: Option<i64>) -> bool {
    let above_min = match min {
        None => true,
        Some(m) => n >= m,
    };
    let below_max = match max {
        None => true,
        Some(m) => n <= m,
    };
    above_min && below_max
}

/// Is a child's lower bound at least as tight as the parent's?
///
/// `None` is "no bound" (−∞). Written as an explicit table rather than an
/// `Option` comparison on purpose: the derived ordering on `Option` puts
/// `None` *below* every `Some`, which would call an unbounded child tighter
/// than a bounded parent.
///
/// | parent | child | result |
/// |---|---|---|
/// | `None` | anything | `true` |
/// | `Some(p)` | `None` | `false` — the child reaches below `p` |
/// | `Some(p)` | `Some(c)` | `c >= p` |
pub(crate) fn lower_bound_within<T: Ord + Copy>(parent: Option<T>, child: Option<T>) -> bool {
    match (parent, child) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(p), Some(c)) => c >= p,
    }
}

/// Is a child's upper bound at least as tight as the parent's?
///
/// `None` is "no bound" (+∞). Same rationale as [`lower_bound_within`];
/// also used for grant expiry (`None` = never expires).
///
/// | parent | child | result |
/// |---|---|---|
/// | `None` | anything | `true` |
/// | `Some(p)` | `None` | `false` — the child reaches above `p` |
/// | `Some(p)` | `Some(c)` | `c <= p` |
pub(crate) fn upper_bound_within<T: Ord + Copy>(parent: Option<T>, child: Option<T>) -> bool {
    match (parent, child) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(p), Some(c)) => c <= p,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn s(v: &str) -> ArgValue {
        ArgValue::Str(v.to_string())
    }
    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|i| i.to_string()).collect()
    }
    fn range(min: Option<i64>, max: Option<i64>) -> Constraint {
        Constraint::Range { min, max }
    }

    // ---- admits ---------------------------------------------------------

    #[test]
    fn prefix_admits_byte_prefixes_only() {
        let c = Constraint::Prefix("/data/inv".into());
        assert!(c.admits(Some(&s("/data/inv"))));
        assert!(c.admits(Some(&s("/data/invoices/1"))));
        assert!(
            c.admits(Some(&s("/data/invalid"))),
            "byte prefix, not path component"
        );
        assert!(!c.admits(Some(&s("/data/in"))));
        assert!(!c.admits(Some(&s("/DATA/inv"))), "byte-wise, case matters");
        assert!(!c.admits(Some(&ArgValue::Int(1))));
        assert!(!c.admits(Some(&ArgValue::Other)));
        assert!(!c.admits(None), "missing constrained argument is denied");
        assert!(Constraint::Prefix(String::new()).admits(Some(&s(""))));
        assert!(Constraint::Prefix(String::new()).admits(Some(&s("anything"))));
        assert!(!Constraint::Prefix(String::new()).admits(Some(&ArgValue::Int(0))));
    }

    #[test]
    fn suffix_admits_byte_suffixes_only() {
        let c = Constraint::Suffix("@yourco.com".into());
        assert!(c.admits(Some(&s("bob@yourco.com"))));
        assert!(!c.admits(Some(&s("bob@yourco.com.evil"))));
        assert!(!c.admits(Some(&s("bob@YourCo.com"))));
        assert!(Constraint::Suffix("yourco.com".into()).admits(Some(&s("x@evilyourco.com"))));
        assert!(!c.admits(Some(&ArgValue::Other)));
        assert!(!c.admits(None));
        assert!(Constraint::Suffix(String::new()).admits(Some(&s(""))));
        assert!(Constraint::Suffix(String::new()).admits(Some(&s("anything"))));
        assert!(!Constraint::Suffix(String::new()).admits(Some(&ArgValue::Int(0))));
        assert!(!Constraint::Suffix(String::new()).admits(None));
    }

    #[test]
    fn one_of_is_exact() {
        let c = Constraint::OneOf(set(&["a", "b"]));
        assert!(c.admits(Some(&s("a"))));
        assert!(!c.admits(Some(&s("ab"))));
        assert!(!c.admits(Some(&s(""))));
        assert!(
            !Constraint::OneOf(set(&[])).admits(Some(&s(""))),
            "empty set admits nothing"
        );
        assert!(!c.admits(None));
        assert!(!c.admits(Some(&ArgValue::Int(1))));
    }

    #[test]
    fn range_is_inclusive_with_open_ends() {
        let c = range(Some(1), Some(10));
        assert!(c.admits(Some(&ArgValue::Int(1))));
        assert!(c.admits(Some(&ArgValue::Int(10))));
        assert!(!c.admits(Some(&ArgValue::Int(0))));
        assert!(!c.admits(Some(&ArgValue::Int(11))));
        assert!(range(None, Some(0)).admits(Some(&ArgValue::Int(i64::MIN))));
        assert!(range(Some(0), None).admits(Some(&ArgValue::Int(i64::MAX))));
        assert!(range(None, None).admits(Some(&ArgValue::Int(-5))));
        assert!(
            !range(Some(5), Some(3)).admits(Some(&ArgValue::Int(4))),
            "min > max admits nothing"
        );
        assert!(!c.admits(Some(&s("5"))), "strings are not integers");
        assert!(!c.admits(Some(&ArgValue::Other)));
        assert!(!c.admits(None));
    }

    #[test]
    fn absent_admits_only_missing() {
        assert!(Constraint::Absent.admits(None));
        assert!(!Constraint::Absent.admits(Some(&s(""))));
        assert!(!Constraint::Absent.admits(Some(&ArgValue::Other)));
        assert!(!Constraint::Absent.admits(Some(&ArgValue::Int(0))));
    }

    // ---- includes -------------------------------------------------------

    #[test]
    fn prefix_includes_longer_prefix() {
        let p = Constraint::Prefix("/a".into());
        assert!(p.includes(&Constraint::Prefix("/a".into())));
        assert!(p.includes(&Constraint::Prefix("/a/b".into())));
        assert!(!p.includes(&Constraint::Prefix("/".into())));
        assert!(!p.includes(&Constraint::Prefix("/b".into())));
        assert!(!p.includes(&Constraint::Suffix("/a".into())));
        assert!(!p.includes(&range(None, None)));
        assert!(!p.includes(&Constraint::Absent));
    }

    #[test]
    fn suffix_includes_longer_suffix() {
        let p = Constraint::Suffix("@yourco.com".into());
        assert!(p.includes(&Constraint::Suffix("bob@yourco.com".into())));
        assert!(!p.includes(&Constraint::Suffix("yourco.com".into())));
        assert!(!p.includes(&Constraint::Prefix("@yourco.com".into())));
    }

    #[test]
    fn empty_prefix_or_suffix_includes_any_string_kind() {
        for all in [
            Constraint::Prefix(String::new()),
            Constraint::Suffix(String::new()),
        ] {
            assert!(all.includes(&Constraint::Prefix("x".into())));
            assert!(all.includes(&Constraint::Suffix("x".into())));
            assert!(all.includes(&Constraint::OneOf(set(&["q"]))));
            assert!(all.includes(&Constraint::OneOf(set(&[]))));
            assert!(
                !all.includes(&range(None, None)),
                "integers are not strings"
            );
            assert!(
                !all.includes(&Constraint::Absent),
                "missing is not a string"
            );
        }
    }

    #[test]
    fn one_of_inclusion_rows() {
        let p = Constraint::OneOf(set(&["a", "b", "/x/y"]));
        assert!(p.includes(&Constraint::OneOf(set(&["a"]))));
        assert!(p.includes(&Constraint::OneOf(set(&[]))));
        assert!(!p.includes(&Constraint::OneOf(set(&["a", "c"]))));
        assert!(
            !p.includes(&Constraint::Prefix("a".into())),
            "finite set never includes a prefix"
        );
        assert!(Constraint::Prefix("/x".into()).includes(&Constraint::OneOf(set(&["/x/y", "/x"]))));
        assert!(!Constraint::Prefix("/x".into()).includes(&Constraint::OneOf(set(&["/x/y", "/z"]))));
        assert!(Constraint::Suffix("b".into()).includes(&Constraint::OneOf(set(&["ab", "b"]))));
        assert!(!Constraint::Suffix("b".into()).includes(&Constraint::OneOf(set(&["ba"]))));
    }

    #[test]
    fn range_inclusion_never_trusts_option_ordering() {
        let p = range(Some(0), Some(10));
        assert!(p.includes(&range(Some(0), Some(10))));
        assert!(p.includes(&range(Some(3), Some(4))));
        assert!(!p.includes(&range(Some(-1), Some(4))));
        assert!(!p.includes(&range(Some(3), Some(11))));
        assert!(
            !p.includes(&range(None, Some(4))),
            "child min None reaches below the parent"
        );
        assert!(
            !p.includes(&range(Some(3), None)),
            "child max None reaches above the parent"
        );
        assert!(range(None, None).includes(&range(None, None)));
        assert!(range(None, Some(10)).includes(&range(None, Some(3))));
        assert!(range(Some(0), None).includes(&range(Some(0), None)));
        assert!(!range(Some(0), None).includes(&range(None, None)));
        assert!(
            p.includes(&range(Some(9), Some(2))),
            "an empty child range is within anything it sits in"
        );
        assert!(!p.includes(&Constraint::Prefix(String::new())));
        assert!(!p.includes(&Constraint::Absent));
    }

    #[test]
    fn absent_inclusion_rows() {
        assert!(Constraint::Absent.includes(&Constraint::Absent));
        assert!(!Constraint::Absent.includes(&Constraint::Prefix(String::new())));
        assert!(!Constraint::Absent.includes(&range(None, None)));
        assert!(!Constraint::Prefix(String::new()).includes(&Constraint::Absent));
        assert!(!range(None, None).includes(&Constraint::Absent));
    }

    /// Every ordered pair of one representative per kind (plus the two
    /// "admit every string" parents): `includes` is true exactly on the
    /// documented rows and false everywhere else.
    #[test]
    fn includes_is_true_only_on_documented_rows() {
        let prefix = Constraint::Prefix("x".into());
        let suffix = Constraint::Suffix("x".into());
        let one_of = Constraint::OneOf(set(&["x"]));
        let rng = range(Some(0), Some(1));
        let absent = Constraint::Absent;
        let all_p = Constraint::Prefix(String::new());
        let all_s = Constraint::Suffix(String::new());
        let reps = [&prefix, &suffix, &one_of, &rng, &absent, &all_p, &all_s];
        for parent in reps {
            for child in reps {
                let expected = match (parent, child) {
                    // Same kind, identical operands.
                    (p, c) if p == c => true,
                    // The empty-prefix/suffix parent admits every string.
                    (Constraint::Prefix(p), c) if p.is_empty() && c.is_string_kind() => true,
                    (Constraint::Suffix(p), c) if p.is_empty() && c.is_string_kind() => true,
                    // "x" is both a prefix and a suffix of "x".
                    (Constraint::Prefix(_), Constraint::OneOf(_)) => true,
                    (Constraint::Suffix(_), Constraint::OneOf(_)) => true,
                    _ => false,
                };
                assert_eq!(
                    parent.includes(child),
                    expected,
                    "parent {parent:?} child {child:?}"
                );
            }
        }
    }

    #[test]
    fn bound_helper_tables() {
        // Monomorphic aliases: the helpers are generic over `T: Ord`, and
        // bare integer literals would leave `T` to fallback inference.
        let lower = lower_bound_within::<i64>;
        let upper = upper_bound_within::<i64>;

        assert!(lower(None, None));
        assert!(lower(None, Some(-100)));
        assert!(!lower(Some(0), None));
        assert!(lower(Some(0), Some(0)));
        assert!(lower(Some(0), Some(1)));
        assert!(!lower(Some(0), Some(-1)));

        assert!(upper(None, None));
        assert!(upper(None, Some(100)));
        assert!(!upper(Some(0), None));
        assert!(upper(Some(0), Some(0)));
        assert!(upper(Some(0), Some(-1)));
        assert!(!upper(Some(0), Some(1)));
    }
}
