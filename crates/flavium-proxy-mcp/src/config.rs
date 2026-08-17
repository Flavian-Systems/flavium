//! Upstream configuration: what servers the proxy fronts and how to
//! reach them.
//!
//! These are plain, transport-agnostic data types; the CLI parses its
//! config file into them, tests construct them directly. Validation
//! here is structural (names, shapes, URL schemes); transport-level
//! validation (header syntax, spawnability) happens where the transport
//! is built, with its own typed errors.

use std::fmt;

/// One configured upstream MCP server.
#[derive(Debug, Clone)]
pub struct UpstreamSpec {
    /// Operator-chosen name, used in logs and errors only — tool names
    /// are never rewritten in T1/M2 (opt-in namespacing is T7).
    pub name: String,
    /// How to reach the server.
    pub transport: TransportSpec,
}

/// The transport an upstream is reached over.
#[derive(Clone)]
pub enum TransportSpec {
    /// Spawn a child process and speak MCP over its stdin/stdout.
    Stdio {
        /// The command line: program followed by its arguments.
        command: Vec<String>,
    },
    /// Speak streamable HTTP to an MCP endpoint URL.
    Http {
        /// The MCP endpoint (http or https).
        url: String,
        /// Extra headers sent on every request (e.g. `Authorization`).
        headers: Vec<(String, String)>,
    },
}

/// Header **values** are secrets (`Authorization` bearer tokens), and
/// URLs can embed credentials in userinfo or query strings; neither
/// must ever reach logs, so the derived representation is replaced with
/// one that redacts both.
impl fmt::Debug for TransportSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio { command } => f.debug_struct("Stdio").field("command", command).finish(),
            Self::Http { url, headers } => f
                .debug_struct("Http")
                .field("url", &redact_url(url))
                .field(
                    "headers",
                    &headers
                        .iter()
                        .map(|(name, _)| (name.as_str(), "<redacted>"))
                        .collect::<Vec<_>>(),
                )
                .finish(),
        }
    }
}

/// A display-safe rendering of a configured URL: scheme, host, port,
/// and path only. Userinfo and query strings — where hosted endpoints
/// put credentials — are stripped, so this form is the only one that
/// may appear in logs or error messages.
pub fn redact_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(parsed) => {
            let mut out = format!("{}://", parsed.scheme());
            out.push_str(parsed.host_str().unwrap_or(""));
            if let Some(port) = parsed.port() {
                out.push(':');
                out.push_str(&port.to_string());
            }
            out.push_str(parsed.path());
            out
        }
        // An unparseable URL could hide anything; identify the
        // upstream by name instead of echoing the string.
        Err(_) => "<unparseable url>".to_owned(),
    }
}

/// Structural configuration errors.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    /// No upstreams were configured at all.
    #[error("no upstreams configured")]
    NoUpstreams,

    /// An upstream has an empty name.
    #[error("upstream #{index} has an empty name")]
    EmptyName {
        /// Zero-based position in the config.
        index: usize,
    },

    /// Two upstreams share a name.
    #[error("duplicate upstream name {name:?}")]
    DuplicateName {
        /// The repeated name.
        name: String,
    },

    /// A stdio upstream has an empty command line.
    #[error("upstream {name:?} has an empty command")]
    EmptyCommand {
        /// The offending upstream.
        name: String,
    },

    /// An HTTP upstream's URL does not parse or is not http/https.
    #[error("upstream {name:?} has an invalid url {url:?}")]
    BadUrl {
        /// The offending upstream.
        name: String,
        /// The URL in redacted form (never the configured bytes, which
        /// may embed credentials).
        url: String,
    },
}

/// Validates a full upstream set structurally.
///
/// Checks, in order and stopping at the first failure: at least one
/// upstream; every name non-empty and unique; every stdio command line
/// non-empty with a non-empty program; every HTTP URL parseable with an
/// `http` or `https` scheme. It does not touch the network or the
/// filesystem — spawnability and header syntax are checked where the
/// transport is built ([`crate::transport::SpawnError`],
/// [`crate::http::HttpSetupError`]).
///
/// # Errors
///
/// The corresponding [`ConfigError`]; URLs in errors are pre-redacted.
pub fn validate(specs: &[UpstreamSpec]) -> Result<(), ConfigError> {
    if specs.is_empty() {
        return Err(ConfigError::NoUpstreams);
    }
    let mut names = std::collections::HashSet::new();
    for (index, spec) in specs.iter().enumerate() {
        if spec.name.is_empty() {
            return Err(ConfigError::EmptyName { index });
        }
        if !names.insert(spec.name.as_str()) {
            return Err(ConfigError::DuplicateName {
                name: spec.name.clone(),
            });
        }
        match &spec.transport {
            TransportSpec::Stdio { command } => {
                if command.is_empty() || command[0].is_empty() {
                    return Err(ConfigError::EmptyCommand {
                        name: spec.name.clone(),
                    });
                }
            }
            TransportSpec::Http { url, .. } => {
                let parsed = reqwest::Url::parse(url);
                let scheme_ok =
                    matches!(&parsed, Ok(u) if u.scheme() == "http" || u.scheme() == "https");
                if !scheme_ok {
                    return Err(ConfigError::BadUrl {
                        name: spec.name.clone(),
                        url: redact_url(url),
                    });
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio(name: &str, cmd: &[&str]) -> UpstreamSpec {
        UpstreamSpec {
            name: name.into(),
            transport: TransportSpec::Stdio {
                command: cmd.iter().map(|s| (*s).to_owned()).collect(),
            },
        }
    }

    fn http(name: &str, url: &str) -> UpstreamSpec {
        UpstreamSpec {
            name: name.into(),
            transport: TransportSpec::Http {
                url: url.into(),
                headers: vec![],
            },
        }
    }

    #[test]
    fn accepts_a_mixed_valid_set() {
        let specs = [
            stdio("fs", &["npx", "server-filesystem"]),
            http("web", "https://example.com/mcp"),
            http("local", "http://127.0.0.1:8080/mcp"),
        ];
        assert_eq!(validate(&specs), Ok(()));
    }

    #[test]
    fn rejects_structural_problems() {
        assert_eq!(validate(&[]), Err(ConfigError::NoUpstreams));
        assert_eq!(
            validate(&[stdio("", &["x"])]),
            Err(ConfigError::EmptyName { index: 0 })
        );
        assert_eq!(
            validate(&[stdio("a", &["x"]), http("a", "https://e.com/")]),
            Err(ConfigError::DuplicateName { name: "a".into() })
        );
        assert_eq!(
            validate(&[stdio("a", &[])]),
            Err(ConfigError::EmptyCommand { name: "a".into() })
        );
        assert_eq!(
            validate(&[stdio("a", &[""])]),
            Err(ConfigError::EmptyCommand { name: "a".into() })
        );
    }

    #[test]
    fn rejects_non_http_urls() {
        for (bad, redacted) in [
            ("ftp://x/", "ftp://x/"),
            ("not a url", "<unparseable url>"),
            ("ws://x/", "ws://x/"),
        ] {
            assert_eq!(
                validate(&[http("a", bad)]),
                Err(ConfigError::BadUrl {
                    name: "a".into(),
                    url: redacted.into()
                }),
                "url {bad:?}"
            );
        }
    }

    #[test]
    fn redact_url_strips_userinfo_and_query() {
        assert_eq!(
            redact_url("https://user:tok3n@example.com:8443/mcp?api_key=SECRET#frag"),
            "https://example.com:8443/mcp"
        );
        assert_eq!(
            redact_url("http://127.0.0.1:8080/mcp"),
            "http://127.0.0.1:8080/mcp"
        );
        assert_eq!(redact_url("%%%"), "<unparseable url>");
    }

    #[test]
    fn debug_output_redacts_header_values_and_url_secrets() {
        let spec = UpstreamSpec {
            name: "web".into(),
            transport: TransportSpec::Http {
                url: "https://example.com/mcp?api_key=url-secret".into(),
                headers: vec![("Authorization".into(), "Bearer super-secret".into())],
            },
        };
        let debug = format!("{spec:?}");
        assert!(!debug.contains("super-secret"), "leaked: {debug}");
        assert!(!debug.contains("url-secret"), "leaked: {debug}");
        assert!(debug.contains("Authorization"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("https://example.com/mcp"));
    }
}
