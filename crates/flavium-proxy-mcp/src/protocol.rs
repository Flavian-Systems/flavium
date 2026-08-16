//! Protocol-level constants: what the proxy offers and accepts.
//!
//! The proxy terminates MCP on both faces, so it has exactly one
//! supported-version policy, used in both directions: it *offers*
//! [`OFFERED_VERSION`] and *speaks* anything in [`SUPPORTED_VERSIONS`].
//!
//! 2025-03-26 and older are deliberately outside the set: those
//! revisions permit JSON-RPC batching, which the envelope boundary
//! rejects, and offering them would promise a dialect the proxy will
//! not parse. The 2026-07-28 "modern era" revision is deferred T1 work
//! (see the T1 plan); a client offering it is answered with
//! [`OFFERED_VERSION`] per the spec's negotiation rule, and an upstream
//! insisting on it is refused at startup.

/// The newest protocol revision the proxy speaks, offered on both
/// faces. Matches the version pinned live in the M1 demo
/// (docs/tasks/v0.1/T1-demo.md).
pub const OFFERED_VERSION: &str = "2025-11-25";

/// Every protocol revision the proxy accepts a session in.
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-11-25"];

/// The name the proxy reports in `serverInfo` (to clients) and
/// `clientInfo` (to upstreams).
pub const PROXY_NAME: &str = "flavium";

/// Whether `version` is a revision the proxy will run a session in.
pub fn supported(version: &str) -> bool {
    SUPPORTED_VERSIONS.contains(&version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offered_version_is_supported() {
        assert!(supported(OFFERED_VERSION));
    }

    #[test]
    fn batching_era_and_unknown_versions_are_not() {
        assert!(!supported("2025-03-26"));
        assert!(!supported("2024-11-05"));
        assert!(!supported("2026-07-28"));
        assert!(!supported(""));
    }
}
