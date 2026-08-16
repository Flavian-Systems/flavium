//! The enforcement seam: everything the router needs to gate a call, and
//! nothing about how the answer is computed.
//!
//! [`Enforcement`] is what the CLI hands [`crate::router::run`]. It names
//! the principal, an [`Authorizer`], a [`TraceSink`], a [`Clock`], and the
//! per-argument path flavors. All five are traits or plain data from
//! `flavium-core`, which has no dependencies — so the proxy (and every
//! proxy test) reaches enforcement without knowing Cedar exists. The
//! dividend is immediate: the proxy's own tests use [`GrantEnvelope`], the
//! *reference* implementation, as their authorizer, so they test wiring
//! against the specification, while the CLI's end-to-end tests exercise
//! the real engine.
//!
//! [`GrantEnvelope`]: flavium_core::GrantEnvelope
//!
//! The [`Clock`] lives here rather than in the core on purpose: the core
//! is clock-free by rule (every decision takes `now` as an argument, which
//! is what makes it replayable), and a settable clock on the proxy side is
//! what makes expiry testable at all.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use flavium_core::{Authorizer, GrantEnvelope, Principal, Timestamp, TraceSink};

use crate::normalize::PathFlavor;

/// Where the proxy reads wall-clock time.
///
/// One read per call, used for both the decision and the trace event that
/// records it, so a replay of that event sees exactly the `now` the
/// decision was made with.
pub trait Clock: Send + Sync {
    /// The current time, in Unix seconds.
    fn now(&self) -> Timestamp;
}

/// The production clock: the host's wall clock, truncated to seconds.
///
/// Times before the epoch (a badly set clock) come out negative rather
/// than saturating; the core only ever compares timestamps, so a negative
/// one is harmless and, being far in the past, expires everything — the
/// fail-closed direction.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(since) => i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
            Err(before) => i64::try_from(before.duration().as_secs())
                .map(|secs| -secs)
                .unwrap_or(i64::MIN),
        };
        Timestamp::from_unix_secs(secs)
    }
}

impl<T: Clock + ?Sized> Clock for Arc<T> {
    fn now(&self) -> Timestamp {
        (**self).now()
    }
}

/// Which `(tool, argument)` pairs hold paths, and how their separators
/// are spelled.
///
/// Built by the grant loader from the `path-prefix` /
/// `windows-path-prefix` constraints in the grant file; consulted once per
/// call for the routed tool. An argument no grant marks as a path is
/// compared byte for byte, unchanged.
#[derive(Debug, Default, Clone)]
pub struct PathFlavors {
    by_tool: BTreeMap<String, BTreeMap<String, PathFlavor>>,
}

impl PathFlavors {
    /// An empty map: nothing is normalized.
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks `argument` of `tool` as a path in `flavor`.
    ///
    /// The loader has already refused a `(tool, argument)` claimed by two
    /// different flavors, or by a path-flavored and a non-path constraint,
    /// so a later insert can only repeat what an earlier one said.
    pub fn insert(&mut self, tool: &str, argument: &str, flavor: PathFlavor) {
        self.by_tool
            .entry(tool.to_owned())
            .or_default()
            .insert(argument.to_owned(), flavor);
    }

    /// The flavors declared for one tool's arguments, if any.
    pub fn for_tool(&self, tool: &str) -> Option<&BTreeMap<String, PathFlavor>> {
        self.by_tool.get(tool)
    }

    /// True when no argument anywhere is marked as a path.
    pub fn is_empty(&self) -> bool {
        self.by_tool.is_empty()
    }
}

/// Everything the router needs to enforce grants on a session.
///
/// [`crate::router::run`] takes this as an `Option`: `None` is the
/// deliberately unenforced middlebox behind `flavium proxy --unenforced`,
/// which forwards every call and records nothing — there is no honest
/// trace for a session that allowed everything.
pub struct Enforcement {
    /// The policy in force, recorded as the session's first trace event
    /// so every later `Allow { grant }` index can be interpreted.
    ///
    /// It is also the single source of the principal, which is why there
    /// is no separate field for one: the identity that authorizes and the
    /// identity the trace names cannot drift apart.
    pub envelope: GrantEnvelope,
    /// The engine that answers. `Arc<dyn>` so the CLI can wire Cedar
    /// while tests wire the reference semantics.
    pub authorizer: Arc<dyn Authorizer>,
    /// Where trace events go. Only the serve loop writes to it, in causal
    /// order, from one task.
    pub sink: Arc<dyn TraceSink>,
    /// Where `now` comes from.
    pub clock: Arc<dyn Clock>,
    /// Which arguments are paths (see [`PathFlavors`]).
    pub path_flavors: PathFlavors,
}

impl Enforcement {
    /// Whose grants gate this session. Static per process in T1; MCP
    /// `clientInfo` is untrusted data and never identity.
    pub fn principal(&self) -> &Principal {
        &self.envelope.principal
    }
}

impl std::fmt::Debug for Enforcement {
    /// Hand-written because none of the three trait objects is `Debug`,
    /// and because an envelope's grants have no business in a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Enforcement")
            .field("principal", self.principal())
            .field("grants", &self.envelope.grants.len())
            .field("path_flavors", &self.path_flavors)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A clock the tests move by hand — the reason [`Clock`] is a trait.
    #[derive(Debug, Default)]
    struct TestClock {
        now: Mutex<i64>,
    }

    impl TestClock {
        fn at(secs: i64) -> Self {
            Self {
                now: Mutex::new(secs),
            }
        }
        fn set(&self, secs: i64) {
            *self.now.lock().unwrap() = secs;
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> Timestamp {
            Timestamp::from_unix_secs(*self.now.lock().unwrap())
        }
    }

    #[test]
    fn system_clock_is_near_the_epoch_in_seconds() {
        let now = SystemClock.now().unix_secs();
        // Any plausible build machine: after 2020, before 2100.
        assert!(now > 1_577_836_800, "clock reads {now}");
        assert!(now < 4_102_444_800, "clock reads {now}");
    }

    #[test]
    fn a_settable_clock_works_behind_arc_dyn() {
        let clock = Arc::new(TestClock::at(5));
        let shared: Arc<dyn Clock> = clock.clone();
        assert_eq!(shared.now(), Timestamp::from_unix_secs(5));
        clock.set(9);
        assert_eq!(shared.now(), Timestamp::from_unix_secs(9));
    }

    #[test]
    fn path_flavors_are_looked_up_by_tool_then_argument() {
        let mut flavors = PathFlavors::new();
        assert!(flavors.is_empty());
        flavors.insert("read_file", "path", PathFlavor::Posix);
        flavors.insert("read_file", "backup", PathFlavor::Windows);
        flavors.insert("write_file", "path", PathFlavor::Windows);
        assert!(!flavors.is_empty());

        let read = flavors.for_tool("read_file").unwrap();
        assert_eq!(read.get("path"), Some(&PathFlavor::Posix));
        assert_eq!(read.get("backup"), Some(&PathFlavor::Windows));
        assert_eq!(read.get("other"), None);
        assert_eq!(
            flavors.for_tool("write_file").unwrap().get("path"),
            Some(&PathFlavor::Windows)
        );
        assert!(flavors.for_tool("send_mail").is_none());
    }

    #[test]
    fn enforcement_debug_names_the_principal_but_no_grant() {
        use flavium_core::{Constraint, Grant, NullSink, ToolName};
        let envelope = GrantEnvelope {
            principal: Principal::new("bot").unwrap(),
            grants: vec![Grant {
                tool: ToolName::new("read_file").unwrap(),
                constraints: BTreeMap::from([(
                    "path".to_owned(),
                    Constraint::Prefix("/data/secret-project/".into()),
                )]),
                expires: None,
            }],
        };
        let enforcement = Enforcement {
            envelope: envelope.clone(),
            authorizer: Arc::new(envelope),
            sink: Arc::new(NullSink),
            clock: Arc::new(SystemClock),
            path_flavors: PathFlavors::new(),
        };
        assert_eq!(enforcement.principal().as_str(), "bot");
        let debug = format!("{enforcement:?}");
        assert!(debug.contains("bot"));
        assert!(!debug.contains("secret-project"), "leaked: {debug}");
    }
}
