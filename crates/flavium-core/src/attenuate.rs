//! Attenuation: is a child grant set a subset of its parent's on every axis?
//!
//! [`attenuates`] is the check delegation runs at spawn (T3): a parent may
//! hand a child any grant set that is *covered* by its own, and nothing
//! else. The check is **sound and conservative** — it never accepts a child
//! that could do something the parent cannot (invariant **INV-1**), and it
//! may refuse a child that is semantically fine but not expressed grant by
//! grant (a child grant covered only by the union of two parent grants is
//! refused; write it as two child grants).
//!
//! The axes, checked in this order for each pair of grants
//! ([`Grant::covers`]):
//!
//! 1. **tool** — the same tool name;
//! 2. **expiry** — the child expires no later than the parent (`None` =
//!    never, so a never-expiring parent covers anything and a never-expiring
//!    child is covered only by a never-expiring parent);
//! 3. **constraints** — for every argument the parent constrains, the child
//!    constrains it too and the parent's constraint
//!    [`includes`](crate::Constraint::includes) the child's. The child may constrain further arguments; it may never
//!    drop or widen one.
//!
//! "Strictly attenuates" (DESIGN §3) means *always enforced ⊆*: a child
//! equal to its parent is fine (**INV-5**, reflexivity). What is forbidden
//! is any widening.

use std::fmt;

use crate::constraint::upper_bound_within;
use crate::grant::Grant;

/// The axis on which a child grant is not covered by a candidate parent
/// grant. Returned by [`Grant::covers`]; diagnostic only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Axis {
    /// The tools differ.
    Tool,
    /// The child outlives the parent.
    Expiry,
    /// The parent constrains this argument and the child either does not,
    /// or its constraint is not one [`Constraint::includes`](crate::Constraint::includes)
    /// recognises as within the parent's (structurally — see its table; a
    /// semantically tighter child written in another kind is refused too).
    Constraint {
        /// The argument name.
        argument: String,
    },
}

impl fmt::Display for Axis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Axis::Tool => f.write_str("tool"),
            Axis::Expiry => f.write_str("expiry"),
            Axis::Constraint { argument } => write!(f, "constraint on argument {argument:?}"),
        }
    }
}

impl std::error::Error for Axis {}

/// A child grant that no grant of the parent covers. Returned by
/// [`attenuates`].
///
/// Only the child's index is reported: with several parent grants naming
/// the same tool, each may fail on a different [`Axis`], so a single axis
/// would be misleading. Callers wanting detail can run [`Grant::covers`]
/// against each candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uncovered {
    /// Index of the uncovered grant in the child set.
    pub child: usize,
}

impl fmt::Display for Uncovered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "child grant #{} is not covered by any parent grant",
            self.child
        )
    }
}

impl std::error::Error for Uncovered {}

impl Grant {
    /// Does this (parent) grant cover `child` — is everything `child`
    /// authorizes also authorized by `self`?
    ///
    /// Checks the axes in the order tool, expiry, constraints (constraints
    /// in argument-name order) and reports the first that fails. Sound
    /// (**L2**): `Ok` implies that whenever `child` is live and admits a
    /// call, `self` is live and admits it too. Conservative: `Err` does not
    /// prove the child is wider.
    pub fn covers(&self, child: &Grant) -> Result<(), Axis> {
        if self.tool != child.tool {
            return Err(Axis::Tool);
        }
        if !upper_bound_within(self.expires, child.expires) {
            return Err(Axis::Expiry);
        }
        for (argument, parent_constraint) in &self.constraints {
            let covered = match child.constraints.get(argument) {
                Some(child_constraint) => parent_constraint.includes(child_constraint),
                None => false,
            };
            if !covered {
                return Err(Axis::Constraint {
                    argument: argument.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Is `child` a subset of `parent` on every axis — may `parent` delegate
/// exactly `child`?
///
/// `Ok(())` iff every grant in `child` is covered ([`Grant::covers`]) by at
/// least one grant in `parent`. An empty child is always covered; nothing
/// but an empty child is covered by an empty parent.
///
/// Maintains **INV-1** (soundness: `Ok` ⇒ every call the child allows, the
/// parent allows, at every time), **INV-5** (reflexive, transitive) and
/// **INV-6** (monotone). Pure and total (**INV-4**).
pub fn attenuates(parent: &[Grant], child: &[Grant]) -> Result<(), Uncovered> {
    for (index, child_grant) in child.iter().enumerate() {
        let covered = parent
            .iter()
            .any(|parent_grant| parent_grant.covers(child_grant).is_ok());
        if !covered {
            return Err(Uncovered { child: index });
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::constraint::Constraint;
    use crate::name::ToolName;
    use crate::time::Timestamp;
    use std::collections::BTreeSet;

    fn t(secs: i64) -> Timestamp {
        Timestamp::from_unix_secs(secs)
    }
    fn grant(name: &str, constraints: &[(&str, Constraint)], expires: Option<i64>) -> Grant {
        Grant {
            tool: ToolName::new(name).unwrap(),
            constraints: constraints
                .iter()
                .map(|(k, c)| (k.to_string(), c.clone()))
                .collect(),
            expires: expires.map(t),
        }
    }
    fn prefix(p: &str) -> Constraint {
        Constraint::Prefix(p.to_string())
    }
    fn suffix(p: &str) -> Constraint {
        Constraint::Suffix(p.to_string())
    }
    fn one_of(items: &[&str]) -> Constraint {
        Constraint::OneOf(items.iter().map(|i| i.to_string()).collect::<BTreeSet<_>>())
    }
    fn range(min: Option<i64>, max: Option<i64>) -> Constraint {
        Constraint::Range { min, max }
    }
    fn arg(argument: &str) -> Axis {
        Axis::Constraint {
            argument: argument.to_string(),
        }
    }

    // ---- covers: every axis, tightening allowed ---------------------------

    #[test]
    fn covers_itself_and_tightenings() {
        let parent = grant(
            "send",
            &[
                ("to", suffix("@yourco.com")),
                ("n", range(Some(1), Some(10))),
            ],
            Some(100),
        );
        assert_eq!(parent.covers(&parent), Ok(()));
        let child = grant(
            "send",
            &[
                ("to", suffix("bob@yourco.com")),
                ("n", range(Some(2), Some(3))),
                ("cc", Constraint::Absent),
                ("subject", prefix("Re:")),
            ],
            Some(50),
        );
        assert_eq!(parent.covers(&child), Ok(()));
        // Parent never expires: any child expiry is fine.
        let eternal = grant("send", &[], None);
        assert_eq!(eternal.covers(&grant("send", &[], Some(1))), Ok(()));
        assert_eq!(eternal.covers(&grant("send", &[], None)), Ok(()));
        // Parent unconstrained on an argument: a child may add anything.
        assert_eq!(
            eternal.covers(&grant("send", &[("x", Constraint::Absent)], None)),
            Ok(())
        );
        assert_eq!(
            eternal.covers(&grant("send", &[("x", range(None, None))], None)),
            Ok(())
        );
    }

    // ---- covers: every axis, loosening past the parent is refused ---------

    #[test]
    fn covers_refuses_other_tool() {
        let parent = grant("read", &[], None);
        assert_eq!(parent.covers(&grant("write", &[], None)), Err(Axis::Tool));
    }

    #[test]
    fn covers_refuses_longer_life() {
        let parent = grant("read", &[], Some(10));
        assert_eq!(parent.covers(&grant("read", &[], Some(10))), Ok(()));
        assert_eq!(
            parent.covers(&grant("read", &[], Some(11))),
            Err(Axis::Expiry)
        );
        assert_eq!(parent.covers(&grant("read", &[], None)), Err(Axis::Expiry));
    }

    #[test]
    fn covers_refuses_dropped_or_widened_constraints() {
        let parent = grant(
            "read",
            &[("path", prefix("/data/")), ("n", range(Some(0), Some(5)))],
            None,
        );
        // Dropped.
        assert_eq!(
            parent.covers(&grant("read", &[("path", prefix("/data/x"))], None)),
            Err(arg("n"))
        );
        // Widened prefix.
        assert_eq!(
            parent.covers(&grant(
                "read",
                &[("path", prefix("/")), ("n", range(Some(0), Some(5)))],
                None
            )),
            Err(arg("path"))
        );
        // Widened range: bound raised.
        assert_eq!(
            parent.covers(&grant(
                "read",
                &[("path", prefix("/data/")), ("n", range(Some(0), Some(6)))],
                None
            )),
            Err(arg("n"))
        );
        // Widened range: bound dropped to None.
        assert_eq!(
            parent.covers(&grant(
                "read",
                &[("path", prefix("/data/")), ("n", range(None, Some(5)))],
                None
            )),
            Err(arg("n"))
        );
        // Kind changed: a suffix is not within a prefix.
        assert_eq!(
            parent.covers(&grant(
                "read",
                &[("path", suffix("/data/")), ("n", range(Some(0), Some(5)))],
                None
            )),
            Err(arg("path"))
        );
        // Absent under a presence-requiring parent, and the reverse.
        assert_eq!(
            parent.covers(&grant(
                "read",
                &[("path", Constraint::Absent), ("n", range(Some(0), Some(5)))],
                None
            )),
            Err(arg("path"))
        );
        let absent_parent = grant("read", &[("cc", Constraint::Absent)], None);
        assert_eq!(
            absent_parent.covers(&grant("read", &[("cc", prefix(""))], None)),
            Err(arg("cc"))
        );
        assert_eq!(
            absent_parent.covers(&grant("read", &[], None)),
            Err(arg("cc"))
        );
    }

    #[test]
    fn covers_reports_axes_in_fixed_order() {
        let parent = grant("read", &[("a", prefix("/x")), ("b", prefix("/y"))], Some(1));
        // Everything wrong: tool first.
        assert_eq!(parent.covers(&grant("write", &[], None)), Err(Axis::Tool));
        // Tool right: expiry next.
        assert_eq!(parent.covers(&grant("read", &[], None)), Err(Axis::Expiry));
        // Expiry right: constraints in argument order.
        assert_eq!(parent.covers(&grant("read", &[], Some(1))), Err(arg("a")));
        assert_eq!(
            parent.covers(&grant("read", &[("a", prefix("/x/1"))], Some(1))),
            Err(arg("b"))
        );
    }

    #[test]
    fn covers_one_of_rows() {
        let parent = grant("send", &[("to", one_of(&["a@x", "b@x"]))], None);
        assert_eq!(
            parent.covers(&grant("send", &[("to", one_of(&["a@x"]))], None)),
            Ok(())
        );
        assert_eq!(
            parent.covers(&grant("send", &[("to", one_of(&["a@x", "c@x"]))], None)),
            Err(arg("to"))
        );
        let parent = grant("send", &[("to", suffix("@x"))], None);
        assert_eq!(
            parent.covers(&grant("send", &[("to", one_of(&["a@x"]))], None)),
            Ok(())
        );
        assert_eq!(
            parent.covers(&grant("send", &[("to", one_of(&["a@x", "a@y"]))], None)),
            Err(arg("to"))
        );
    }

    // ---- attenuates: set level -------------------------------------------

    #[test]
    fn empty_child_is_always_covered_and_only_by_empty_parent() {
        assert_eq!(attenuates(&[], &[]), Ok(()));
        assert_eq!(attenuates(&[grant("a", &[], None)], &[]), Ok(()));
        assert_eq!(
            attenuates(&[], &[grant("a", &[], None)]),
            Err(Uncovered { child: 0 })
        );
    }

    #[test]
    fn child_may_pick_and_narrow_and_duplicate() {
        let parent = vec![
            grant("read", &[("path", prefix("/data/"))], Some(100)),
            grant("send", &[("to", suffix("@yourco.com"))], None),
        ];
        let child = vec![
            grant("send", &[("to", suffix("bob@yourco.com"))], Some(5)),
            grant("read", &[("path", prefix("/data/2026-"))], Some(100)),
            grant("read", &[("path", prefix("/data/2026-"))], Some(100)),
        ];
        assert_eq!(attenuates(&parent, &child), Ok(()));
        assert_eq!(attenuates(&parent, &parent), Ok(()));
    }

    #[test]
    fn child_may_not_mint_authority() {
        let parent = vec![grant("read", &[("path", prefix("/data/"))], Some(100))];
        // A tool no parent grant names.
        assert_eq!(
            attenuates(
                &parent,
                &[grant("write", &[("path", prefix("/data/x"))], Some(1))]
            ),
            Err(Uncovered { child: 0 })
        );
        // Reports the first uncovered child, by index.
        let child = vec![
            grant("read", &[("path", prefix("/data/x"))], Some(1)),
            grant("read", &[("path", prefix("/"))], Some(1)),
            grant("write", &[], None),
        ];
        assert_eq!(attenuates(&parent, &child), Err(Uncovered { child: 1 }));
    }

    #[test]
    fn union_of_parents_is_conservatively_refused() {
        // Semantically this child is within the union of the two parents,
        // but a child grant must be covered by ONE parent grant, so it is
        // refused — the documented incompleteness.
        let parent = vec![
            grant("read", &[("path", prefix("/a"))], None),
            grant("read", &[("path", prefix("/b"))], None),
        ];
        let child = vec![grant("read", &[("path", one_of(&["/a/1", "/b/1"]))], None)];
        assert_eq!(attenuates(&parent, &child), Err(Uncovered { child: 0 }));
        // Written grant by grant it passes.
        let child = vec![
            grant("read", &[("path", one_of(&["/a/1"]))], None),
            grant("read", &[("path", one_of(&["/b/1"]))], None),
        ];
        assert_eq!(attenuates(&parent, &child), Ok(()));
    }

    #[test]
    fn display() {
        assert_eq!(
            Uncovered { child: 2 }.to_string(),
            "child grant #2 is not covered by any parent grant"
        );
        assert_eq!(arg("to").to_string(), "constraint on argument \"to\"");
        assert_eq!(Axis::Expiry.to_string(), "expiry");
    }
}
