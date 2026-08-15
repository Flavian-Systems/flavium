//! Flavium core types: grants, principals, budgets, trace events.
//!
//! This crate is the future formal-verification target. It is kept small and
//! dependency-light on purpose; see DESIGN.md §6.

/// A placeholder for the grant tuple: (principal, tool, constraints, expiry, budget).
/// Real types land with the first proxy milestone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Who holds the grant.
    pub principal: String,
    /// The tool the grant authorizes.
    pub tool: String,
}

/// Attenuation invariant, stated as code from day one:
/// a child's grant set must be a subset of its parent's.
pub fn attenuates(parent: &[Grant], child: &[Grant]) -> bool {
    child.iter().all(|g| parent.contains(g))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(p: &str, t: &str) -> Grant {
        Grant {
            principal: p.into(),
            tool: t.into(),
        }
    }

    #[test]
    fn child_subset_attenuates() {
        let parent = vec![g("bot", "fs.read"), g("bot", "email.send")];
        let child = vec![g("bot", "fs.read")];
        assert!(attenuates(&parent, &child));
    }

    #[test]
    fn child_excess_rejected() {
        let parent = vec![g("bot", "fs.read")];
        let child = vec![g("bot", "email.send")];
        assert!(!attenuates(&parent, &child));
    }
}
