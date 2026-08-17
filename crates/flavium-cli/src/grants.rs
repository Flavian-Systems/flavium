//! The config file: upstreams *and* grants, parsed as one document.
//!
//! Grants live in the same `flavium.toml` as upstreams because DESIGN §5
//! promises one config file, and because two files are two things to keep
//! in sync — with the same silent-forward hole when one of them is
//! missing. Enforcement is therefore not a flag: it follows from the file
//! having grants. A config with none makes `flavium proxy` refuse to
//! start; the transparent middlebox survives only behind an explicit
//! `--unenforced`.
//!
//! # The vocabulary, in full
//!
//! ```toml
//! version = 1                        # the file format's version, required
//! principal = "invoice-bot"
//!
//! [[upstream]]
//! name = "fs"
//! command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/data"]
//!
//! [[grant]]
//! tool = "read_file"
//! expires = 2026-09-01T00:00:00Z     # optional, TOML offset date-time
//! [grant.args]
//! path = { path-prefix = "/data/invoices/" }
//!
//! [[grant]]
//! tool = "send_mail"
//! [grant.args]
//! to    = { suffix = "@yourco.com" }
//! bcc   = { absent = true }
//! count = { range = { min = 1, max = 10 } }
//! kind  = { one-of = ["invoice", "receipt"] }
//! ```
//!
//! One constraint key per argument — `prefix`, `path-prefix`,
//! `windows-path-prefix`, `suffix`, `one-of`, `range`, `absent` — each
//! mapping one-to-one onto a [`Constraint`].
//!
//! # Fail closed, at startup, while an operator is watching
//!
//! Everything in this file is a security decision written by hand, so
//! every ambiguity becomes a startup error rather than a mid-session
//! denial that reads as policy. The split between *refusing* and
//! *warning* follows the direction of the mistake: a malformed file could
//! mean anything, so it stops the process; a grant that can only ever
//! deny costs availability, never authority, so it is reported and kept.
//!
//! Refusing an unknown key is also what makes `budget = 5` — the T2a axis
//! the grant vocabulary deliberately does not model yet — an error rather
//! than a lie.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use flavium_core::{Constraint, Grant, GrantEnvelope, Principal, Timestamp, ToolName};
use flavium_proxy_mcp::config::{TransportSpec, UpstreamSpec};
use flavium_proxy_mcp::enforcement::PathFlavors;
use flavium_proxy_mcp::normalize::{normalize_prefix, PathFlavor};
use serde::Deserialize;

/// The one config-file version this build implements.
///
/// A binary accepts the versions it implements **by exact match** and
/// refuses every other value by number. See [`version_error`] for why
/// that is the contract rather than a range.
pub const SUPPORTED_VERSION: i64 = 1;

/// The grant half of the config file, compiled.
#[derive(Debug, Clone)]
pub struct GrantConfig {
    /// The principal and its grants, in file order.
    pub envelope: GrantEnvelope,
    /// Which `(tool, argument)` pairs hold paths, and in which flavor.
    pub path_flavors: PathFlavors,
}

/// A parsed config file.
#[derive(Debug)]
pub struct LoadedConfig {
    /// The upstreams, in file order.
    pub upstreams: Vec<UpstreamSpec>,
    /// The grants, if the file declares any.
    pub grants: Option<GrantConfig>,
    /// Problems that cost availability but never authority: reported,
    /// not refused.
    pub warnings: Vec<String>,
}

/// Reads and parses a config file.
///
/// # Errors
///
/// A one-line message naming the file and what to fix.
pub fn load(path: &Path) -> Result<LoadedConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    parse(&text, &path.display().to_string())
}

/// Parses config-file text. `source` names the file in messages.
///
/// # Errors
///
/// A one-line message naming the file and what to fix.
pub fn parse(text: &str, source: &str) -> Result<LoadedConfig, String> {
    let file: ConfigFile =
        toml::from_str(text).map_err(|err| format!("cannot parse {source}: {err}"))?;

    check_version(file.version.as_ref()).map_err(|err| format!("{source}: {err}"))?;

    let upstreams = upstreams(file.upstream).map_err(|err| format!("{source}: {err}"))?;
    if upstreams.is_empty() {
        return Err(format!("{source}: no [[upstream]] entries"));
    }

    let mut warnings = Vec::new();
    let grants = if file.grant.is_empty() {
        if file.principal.is_some() {
            warnings.push(
                "`principal` is set but the file declares no [[grant]]; nothing is enforced"
                    .to_owned(),
            );
        }
        None
    } else {
        Some(
            grant_config(file.principal, file.grant, &mut warnings)
                .map_err(|err| format!("{source}: {err}"))?,
        )
    };

    Ok(LoadedConfig {
        upstreams,
        grants,
        warnings,
    })
}

/// The config file's shape. `deny_unknown_fields` everywhere: a typo in a
/// policy file is a security decision nobody made, and a key from a
/// richer future flavium read by this binary must fail closed rather than
/// be ignored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    /// Read as a raw value so that a string or a float is refused with
    /// the same message as an unsupported number, rather than as a serde
    /// type error.
    #[serde(default)]
    version: Option<toml::Value>,
    #[serde(default)]
    principal: Option<String>,
    #[serde(default)]
    upstream: Vec<UpstreamEntry>,
    #[serde(default)]
    grant: Vec<GrantEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpstreamEntry {
    name: String,
    #[serde(default)]
    command: Option<Vec<String>>,
    #[serde(default)]
    url: Option<String>,
    /// Extra headers for an HTTP upstream. Values are secrets; they are
    /// never logged.
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantEntry {
    tool: String,
    #[serde(default)]
    expires: Option<toml::value::Datetime>,
    #[serde(default)]
    args: BTreeMap<String, ArgEntry>,
}

/// One argument's constraint. Every field is optional here so that "zero
/// keys" and "two keys" become *our* errors with *our* message; TOML
/// accepts both shapes happily.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArgEntry {
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default, rename = "path-prefix")]
    path_prefix: Option<String>,
    #[serde(default, rename = "windows-path-prefix")]
    windows_path_prefix: Option<String>,
    #[serde(default)]
    suffix: Option<String>,
    #[serde(default, rename = "one-of")]
    one_of: Option<Vec<String>>,
    #[serde(default)]
    range: Option<RangeEntry>,
    #[serde(default)]
    absent: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeEntry {
    #[serde(default)]
    min: Option<i64>,
    #[serde(default)]
    max: Option<i64>,
}

/// Checks the file-format version.
///
/// The direction `deny_unknown_fields` already covers is a *newer* file
/// read by an *older* binary: the new key is an unknown key and the
/// process refuses to start. `version` exists for the direction nothing
/// else can catch — an **old file whose keys still parse but no longer
/// mean what its author meant**. In a policy file, re-interpreted can
/// mean widened, so the contract is exact match, and a bump means the new
/// binary refuses the old dialect rather than interpreting it.
fn check_version(version: Option<&toml::Value>) -> Result<(), String> {
    match version.map(|v| v.as_integer()) {
        Some(Some(SUPPORTED_VERSION)) => Ok(()),
        Some(Some(other)) => Err(version_error(&other.to_string())),
        Some(None) => Err(version_error("not an integer")),
        None => Err(format!(
            "`version` is required; this build supports version {SUPPORTED_VERSION}"
        )),
    }
}

fn version_error(got: &str) -> String {
    format!("unsupported config `version` ({got}); this build supports version {SUPPORTED_VERSION}")
}

/// The upstream half — unchanged from M2 except that it now lives beside
/// the grants it is validated with.
fn upstreams(entries: Vec<UpstreamEntry>) -> Result<Vec<UpstreamSpec>, String> {
    entries
        .into_iter()
        .map(|entry| {
            let name = entry.name;
            let transport = match (entry.command, entry.url) {
                (Some(command), None) => {
                    if entry.headers.is_some() {
                        return Err(format!(
                            "upstream {name:?}: `headers` only applies to `url` upstreams"
                        ));
                    }
                    TransportSpec::Stdio { command }
                }
                (None, Some(url)) => TransportSpec::Http {
                    url,
                    headers: entry.headers.unwrap_or_default().into_iter().collect(),
                },
                _ => {
                    return Err(format!(
                        "upstream {name:?}: exactly one of `command` or `url` is required"
                    ))
                }
            };
            Ok(UpstreamSpec { name, transport })
        })
        .collect()
}

/// Compiles the `[[grant]]` tables into an envelope and a flavor map.
fn grant_config(
    principal: Option<String>,
    entries: Vec<GrantEntry>,
    warnings: &mut Vec<String>,
) -> Result<GrantConfig, String> {
    let Some(principal) = principal else {
        return Err("`principal` is required when the file declares grants".to_owned());
    };
    let principal = Principal::new(&principal)
        .map_err(|err| format!("invalid `principal` {principal:?}: {err}"))?;

    let mut grants = Vec::with_capacity(entries.len());
    let mut path_flavors = PathFlavors::new();
    // Every `(tool, argument)` seen so far and how it was constrained:
    // `Some(flavor)` for a path-flavored constraint, `None` for a
    // byte-wise one. Two different answers anywhere in the file mean the
    // proxy would have to normalize an argument for one grant and not for
    // another — an ambiguity an operator must resolve, not the runtime.
    let mut declared: BTreeMap<(String, String), Option<PathFlavor>> = BTreeMap::new();

    for entry in entries {
        let tool = ToolName::new(&entry.tool)
            .map_err(|err| format!("invalid grant `tool` {:?}: {err}", entry.tool))?;
        let expires = match entry.expires {
            None => None,
            Some(datetime) => Some(
                to_timestamp(&datetime)
                    .map_err(|err| format!("grant for tool {:?}: {err}", entry.tool))?,
            ),
        };

        let mut constraints = BTreeMap::new();
        for (argument, spec) in entry.args {
            let (constraint, flavor) = spec
                .compile(&argument, warnings)
                .map_err(|err| format!("grant for tool {:?}: {err}", entry.tool))?;
            let key = (entry.tool.clone(), argument.clone());
            match declared.get(&key) {
                Some(previous) if *previous != flavor => {
                    return Err(format!(
                        "argument {argument:?} of tool {:?} is constrained both as a path and not \
                         (or in two path flavors); pick one",
                        entry.tool
                    ))
                }
                _ => {
                    declared.insert(key, flavor);
                }
            }
            if let Some(flavor) = flavor {
                path_flavors.insert(&entry.tool, &argument, flavor);
            }
            constraints.insert(argument, constraint);
        }

        grants.push(Grant {
            tool,
            constraints,
            expires,
        });
    }

    Ok(GrantConfig {
        envelope: GrantEnvelope { principal, grants },
        path_flavors,
    })
}

impl ArgEntry {
    /// This argument's constraint, and its path flavor if it has one.
    ///
    /// Exactly one key must be set: zero says nothing and two say two
    /// things, and TOML accepts both — so this is the loader's own check.
    fn compile(
        &self,
        argument: &str,
        warnings: &mut Vec<String>,
    ) -> Result<(Constraint, Option<PathFlavor>), String> {
        let mut found: Vec<(Constraint, Option<PathFlavor>)> = Vec::new();

        if let Some(prefix) = &self.prefix {
            if prefix.is_empty() {
                warnings.push(format!(
                    "argument {argument:?} has an empty `prefix`; it admits every string value"
                ));
            }
            found.push((Constraint::Prefix(prefix.clone()), None));
        }
        for (raw, flavor, key) in [
            (&self.path_prefix, PathFlavor::Posix, "path-prefix"),
            (
                &self.windows_path_prefix,
                PathFlavor::Windows,
                "windows-path-prefix",
            ),
        ] {
            let Some(raw) = raw else { continue };
            let normalized = normalize_prefix(raw, flavor);
            // A path prefix is enforced as a *byte* prefix, so it is only
            // a statement about a directory while byte containment and
            // path containment agree. Three spellings break that, each
            // producing a constraint far wider than the one written, and
            // each is refused rather than warned about: the design says
            // normalization must never widen, and these widen at the
            // moment of compiling.
            let degenerate = if normalized.is_empty() {
                // `.`, `./`, `a/..` — reduce to nothing, and an empty
                // prefix admits every string.
                Some("normalizes to nothing and would therefore admit every path")
            } else if normalized == "/" {
                // The local root is a byte prefix of the *other* root:
                // `"//host/share/x".starts_with("/")`. So a grant on the
                // current drive would admit a write to an arbitrary SMB
                // host — two machines, one string. There is no byte
                // prefix that means "this root and not that one".
                Some(
                    "is the bare root, which as a byte prefix also admits every `//`-rooted \
                     (UNC) path on any host",
                )
            } else if normalized.split('/').any(|segment| segment == "..")
                && normalized
                    .split('/')
                    .all(|segment| segment.is_empty() || segment == "..")
            {
                // `..`, `../`, `a/../..` — every segment climbs. Further
                // `..` *stack* rather than cancel, so `"../../etc/passwd"`
                // byte-starts-with `"../"` while sitting a level higher
                // than the prefix names; the grant reaches every ancestor.
                Some(
                    "names only ancestors, and further `..` stack rather than cancel, so it \
                     admits paths above the one it names",
                )
            } else {
                None
            };
            if let Some(why) = degenerate {
                return Err(format!(
                    "argument {argument:?} has a `{key}` of {raw:?}, which {why}; write the \
                     directory you mean"
                ));
            }
            // A NUL cannot appear in a pathname on any platform flavium
            // targets, and a consumer that stops at one would act on a
            // different resource than the one compared here.
            if normalized.contains('\0') {
                return Err(format!(
                    "argument {argument:?} has a `{key}` of {raw:?}, which contains a NUL byte; \
                     that cannot name a path on any supported platform"
                ));
            }
            found.push((Constraint::Prefix(normalized), Some(flavor)));
        }
        if let Some(suffix) = &self.suffix {
            if suffix.is_empty() {
                warnings.push(format!(
                    "argument {argument:?} has an empty `suffix`; it admits every string value"
                ));
            }
            found.push((Constraint::Suffix(suffix.clone()), None));
        }
        if let Some(values) = &self.one_of {
            if values.is_empty() {
                warnings.push(format!(
                    "argument {argument:?} has an empty `one-of`; it admits nothing"
                ));
            }
            let set: BTreeSet<String> = values.iter().cloned().collect();
            found.push((Constraint::OneOf(set), None));
        }
        if let Some(range) = &self.range {
            if range.min.is_none() && range.max.is_none() {
                return Err(format!(
                    "argument {argument:?} has a `range` with neither `min` nor `max`"
                ));
            }
            if let (Some(min), Some(max)) = (range.min, range.max) {
                if min > max {
                    warnings.push(format!(
                        "argument {argument:?} has a `range` with min {min} above max {max}; \
                         it admits nothing"
                    ));
                }
            }
            found.push((
                Constraint::Range {
                    min: range.min,
                    max: range.max,
                },
                None,
            ));
        }
        if let Some(absent) = self.absent {
            if !absent {
                // There is no "must be present, anything goes"
                // constraint, so `absent = false` cannot mean what it
                // looks like it means. Refuse rather than silently treat
                // it as `Absent` — the opposite of the author's intent.
                return Err(format!(
                    "argument {argument:?} has `absent = false`; only `absent = true` is a \
                     constraint (omit the argument entirely to leave it unconstrained)"
                ));
            }
            found.push((Constraint::Absent, None));
        }

        match found.len() {
            1 => Ok(found.remove(0)),
            0 => Err(format!(
                "argument {argument:?} has no constraint key (one of `prefix`, `path-prefix`, \
                 `windows-path-prefix`, `suffix`, `one-of`, `range`, `absent`)"
            )),
            n => Err(format!(
                "argument {argument:?} has {n} constraint keys; exactly one is allowed"
            )),
        }
    }
}

/// A TOML offset date-time as Unix seconds.
///
/// A grant's expiry is the one field where "what did the operator mean?"
/// must have exactly one answer: `2026-09-01T00:00:00` is a different
/// instant in different places, and a security artifact may not have an
/// ambiguous field. TOML's own parser separates the offset-bearing form
/// from the rest, so refusing the others is a one-line check.
///
/// Sub-second precision is dropped, which moves an expiry at most one
/// second *earlier* — the fail-closed direction.
fn to_timestamp(datetime: &toml::value::Datetime) -> Result<Timestamp, String> {
    let (Some(date), Some(time)) = (datetime.date, datetime.time) else {
        return Err(format!(
            "`expires` must be a date *and* a time with a UTC offset \
             (for example 2026-09-01T00:00:00Z), got {datetime}"
        ));
    };
    let Some(offset) = datetime.offset else {
        return Err(format!(
            "`expires` has no UTC offset, so it means a different instant in every time zone; \
             write {datetime}Z or give an explicit offset such as {datetime}+02:00"
        ));
    };
    let offset_minutes = match offset {
        toml::value::Offset::Z => 0i64,
        toml::value::Offset::Custom { minutes } => i64::from(minutes),
    };
    let days = days_from_civil(i64::from(date.year), date.month, date.day);
    let seconds_of_day = i64::from(time.hour) * 3_600
        + i64::from(time.minute) * 60
        + i64::from(time.second.unwrap_or(0));
    Ok(Timestamp::from_unix_secs(
        days * 86_400 + seconds_of_day - offset_minutes * 60,
    ))
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date.
///
/// Howard Hinnant's `days_from_civil`, which is correct for every year an
/// `i64` can hold and uses only truncating division — no date crate for
/// fifteen lines that a table of reference instants pins exactly.
fn days_from_civil(year: i64, month: u8, day: u8) -> i64 {
    let month = i64::from(month);
    let day = i64::from(day);
    // The year starts in March, so a leap day lands at the end of it.
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400; // [0, 399]
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ok(text: &str) -> LoadedConfig {
        parse(text, "test.toml").unwrap_or_else(|err| panic!("expected Ok, got {err}"))
    }

    fn err(text: &str) -> String {
        parse(text, "test.toml").expect_err("expected a refusal")
    }

    const UPSTREAM: &str = "[[upstream]]\nname = \"fs\"\ncommand = [\"srv\"]\n";

    fn with_grants(body: &str) -> String {
        format!("version = 1\nprincipal = \"bot\"\n{UPSTREAM}{body}")
    }

    #[test]
    fn the_reference_file_parses_into_an_envelope_and_a_flavor_map() {
        let config = ok(&with_grants(
            r#"
[[grant]]
tool = "read_file"
expires = 2026-09-01T00:00:00Z
[grant.args]
path = { path-prefix = "/data/invoices/" }

[[grant]]
tool = "send_mail"
[grant.args]
to    = { suffix = "@yourco.com" }
cc    = { absent = true }
bcc   = { absent = true }
count = { range = { min = 1, max = 10 } }
kind  = { one-of = ["invoice", "receipt"] }
"#,
        ));
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        let grants = config.grants.unwrap();
        assert_eq!(grants.envelope.principal.as_str(), "bot");
        assert_eq!(grants.envelope.grants.len(), 2);

        let read = &grants.envelope.grants[0];
        assert_eq!(read.tool.as_str(), "read_file");
        assert_eq!(
            read.constraints.get("path"),
            Some(&Constraint::Prefix("/data/invoices/".into()))
        );
        assert_eq!(
            read.expires,
            Some(Timestamp::from_unix_secs(1_788_220_800)),
            "2026-09-01T00:00:00Z"
        );

        let mail = &grants.envelope.grants[1];
        assert_eq!(
            mail.constraints.get("to"),
            Some(&Constraint::Suffix("@yourco.com".into()))
        );
        assert_eq!(mail.constraints.get("cc"), Some(&Constraint::Absent));
        assert_eq!(
            mail.constraints.get("count"),
            Some(&Constraint::Range {
                min: Some(1),
                max: Some(10)
            })
        );
        assert_eq!(
            mail.constraints.get("kind"),
            Some(&Constraint::OneOf(
                ["invoice", "receipt"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            ))
        );
        assert!(mail.expires.is_none());

        // Only the path-flavored argument is marked for normalization.
        let flavors = grants.path_flavors.for_tool("read_file").unwrap();
        assert_eq!(flavors.get("path"), Some(&PathFlavor::Posix));
        assert!(grants.path_flavors.for_tool("send_mail").is_none());
    }

    /// The path-flavored constraints normalize the prefix an operator
    /// wrote, so a call's normalized value is compared with a prefix in
    /// the same shape — and never a wider one.
    #[test]
    fn path_prefixes_are_normalized_without_widening() {
        let config = ok(&with_grants(
            r#"
[[grant]]
tool = "read_file"
[grant.args]
path = { path-prefix = "/data/./invoices//" }

[[grant]]
tool = "read_win"
[grant.args]
path = { windows-path-prefix = 'C:\Users\me\Desktop\' }
"#,
        ));
        let grants = config.grants.unwrap();
        assert_eq!(
            grants.envelope.grants[0].constraints.get("path"),
            Some(&Constraint::Prefix("/data/invoices/".into()))
        );
        // ASCII case folded: the Windows flavor decides on the resource,
        // and Windows resolves `C:\Users\Me\` and `c:\users\me\` to one
        // directory. The call's value is folded the same way.
        assert_eq!(
            grants.envelope.grants[1].constraints.get("path"),
            Some(&Constraint::Prefix("c:/users/me/desktop/".into()))
        );
        assert_eq!(
            grants
                .path_flavors
                .for_tool("read_win")
                .unwrap()
                .get("path"),
            Some(&PathFlavor::Windows)
        );
    }

    /// Every row here is a startup error, which is the point: an
    /// ambiguity in a policy file must stop the process while an operator
    /// is watching, never surface mid-session as something that reads
    /// like policy.
    /// `version` is the only guard against a future flavium silently
    /// re-interpreting the words in today's file, so every way of not
    /// saying `version = 1` refuses by number and names what this build
    /// does support.
    #[test]
    fn the_version_table() {
        let rows: Vec<(&str, String)> = vec![
            ("absent", format!("principal = \"b\"\n{UPSTREAM}")),
            ("zero", format!("version = 0\n{UPSTREAM}")),
            ("two", format!("version = 2\n{UPSTREAM}")),
            ("string", format!("version = \"1\"\n{UPSTREAM}")),
            ("float", format!("version = 1.5\n{UPSTREAM}")),
            ("boolean", format!("version = true\n{UPSTREAM}")),
        ];
        for (label, text) in rows {
            let message = err(&text);
            assert!(
                message.contains("version") && message.contains("supports version 1"),
                "version row {label}: {message}"
            );
        }
        // …and the one spelling that works.
        assert!(ok(&format!("version = 1\n{UPSTREAM}")).grants.is_none());
    }

    #[test]
    fn unknown_keys_and_missing_pieces_refuse() {
        // An unknown key anywhere: the T2a axis that is not modelled yet
        // is an error, not a lie.
        assert!(err(&with_grants("[[grant]]\ntool = \"t\"\nbudget = 5\n")).contains("budget"));
        assert!(err(&format!("version = 1\nnope = true\n{UPSTREAM}")).contains("nope"));
        assert!(err(&with_grants(
            "[[grant]]\ntool = \"t\"\n[grant.args]\np = { prefx = \"a\" }\n"
        ))
        .contains("prefx"));

        // Grants without a principal, and names the core would reject.
        assert!(
            err(&format!("version = 1\n{UPSTREAM}[[grant]]\ntool = \"t\"\n"))
                .contains("`principal` is required")
        );
        assert!(err(&format!(
            "version = 1\nprincipal = \"\"\n{UPSTREAM}[[grant]]\ntool = \"t\"\n"
        ))
        .contains("invalid `principal`"));
        assert!(err(&with_grants("[[grant]]\ntool = \"a\\nb\"\n")).contains("invalid grant `tool`"));
        assert!(err(&with_grants("[[grant]]\ntool = \"\"\n")).contains("invalid grant `tool`"));

        // No upstreams at all.
        assert!(err("version = 1\n").contains("no [[upstream]] entries"));
    }

    #[test]
    fn constraint_key_arity_and_absent_false() {
        let zero = err(&with_grants(
            "[[grant]]\ntool = \"t\"\n[grant.args]\np = {}\n",
        ));
        assert!(zero.contains("no constraint key"), "{zero}");

        let two = err(&with_grants(
            "[[grant]]\ntool = \"t\"\n[grant.args]\np = { prefix = \"a\", suffix = \"b\" }\n",
        ));
        assert!(two.contains("2 constraint keys"), "{two}");

        // A path prefix that normalizes to nothing would admit every
        // path — the widest possible constraint, and one the operator
        // never wrote. It stops the process.
        for degenerate in [".", "./", "", "a/..", "data/../", "x/./.."] {
            let message = err(&with_grants(&format!(
                "[[grant]]\ntool = \"t\"\n[grant.args]\np = {{ path-prefix = \"{degenerate}\" }}\n"
            )));
            assert!(
                message.contains("normalizes to nothing"),
                "path-prefix {degenerate:?}: {message}"
            );
        }
        let windows = err(&with_grants(
            "[[grant]]\ntool = \"t\"\n[grant.args]\np = { windows-path-prefix = '.\\' }\n",
        ));
        assert!(windows.contains("normalizes to nothing"), "{windows}");

        // The bare root *is* degenerate, and this reverses an earlier
        // reading of it ("an operator who writes `/` has said what they
        // meant"). What they mean cannot be enforced: the constraint is a
        // byte prefix, and `"//host/share/x".starts_with("/")`, so a grant
        // on the local root also admits every UNC path — a write to an
        // arbitrary SMB host under a grant that named this machine. No
        // byte prefix separates the two roots, so the spelling is refused
        // instead of compiled into a wider grant than it looks.
        for root in ["/", "\\"] {
            let message = err(&with_grants(&format!(
                "[[grant]]\ntool = \"t\"\n[grant.args]\np = {{ windows-path-prefix = '{root}' }}\n"
            )));
            assert!(message.contains("bare root"), "{root:?}: {message}");
        }
        let posix_root = err(&with_grants(
            "[[grant]]\ntool = \"t\"\n[grant.args]\np = { path-prefix = \"/\" }\n",
        ));
        assert!(posix_root.contains("bare root"), "{posix_root}");
        // The UNC root is not refused: it admits exactly what it says,
        // every `//`-rooted path, and no other root hides inside it.
        let unc = ok(&with_grants(
            "[[grant]]\ntool = \"t\"\n[grant.args]\np = { path-prefix = \"//\" }\n",
        ));
        assert_eq!(
            unc.grants.unwrap().envelope.grants[0].constraints.get("p"),
            Some(&Constraint::Prefix("//".into()))
        );

        // A prefix of nothing but `..` reaches above what it names,
        // because further `..` stack rather than cancel: `"../../etc"`
        // byte-starts-with `"../"`. Every ancestor is in the grant.
        for ancestors in ["..", "../", "../../", "a/../..", "..\\"] {
            let message = err(&with_grants(&format!(
                "[[grant]]\ntool = \"t\"\n[grant.args]\np = {{ windows-path-prefix = '{ancestors}' }}\n"
            )));
            assert!(
                message.contains("names only ancestors"),
                "{ancestors:?}: {message}"
            );
        }
        // A `..` that leads somewhere specific is still a real directory.
        let relative = ok(&with_grants(
            "[[grant]]\ntool = \"t\"\n[grant.args]\np = { path-prefix = \"../data/\" }\n",
        ));
        assert_eq!(
            relative.grants.unwrap().envelope.grants[0]
                .constraints
                .get("p"),
            Some(&Constraint::Prefix("../data/".into()))
        );

        // A path flavor and a byte prefix on the same argument are two
        // keys as well — the ambiguity is refused before it can differ.
        let both = err(&with_grants(
            "[[grant]]\ntool = \"t\"\n[grant.args]\np = { prefix = \"/a\", path-prefix = \"/a\" }\n",
        ));
        assert!(both.contains("2 constraint keys"), "{both}");

        let absent_false = err(&with_grants(
            "[[grant]]\ntool = \"t\"\n[grant.args]\np = { absent = false }\n",
        ));
        assert!(absent_false.contains("absent = false"), "{absent_false}");

        let unbounded = err(&with_grants(
            "[[grant]]\ntool = \"t\"\n[grant.args]\nn = { range = {} }\n",
        ));
        assert!(unbounded.contains("neither `min` nor `max`"), "{unbounded}");
    }

    /// The same `(tool, argument)` may not be a path in one grant and
    /// bytes in another: the proxy would have to normalize the argument
    /// and not normalize it, for the same call.
    #[test]
    fn one_argument_cannot_be_both_a_path_and_not() {
        let mixed = err(&with_grants(
            r#"
[[grant]]
tool = "read_file"
[grant.args]
path = { path-prefix = "/data/" }

[[grant]]
tool = "read_file"
[grant.args]
path = { prefix = "/tmp/" }
"#,
        ));
        assert!(mixed.contains("both as a path and not"), "{mixed}");

        let two_flavors = err(&with_grants(
            r#"
[[grant]]
tool = "read_file"
[grant.args]
path = { path-prefix = "/data/" }

[[grant]]
tool = "read_file"
[grant.args]
path = { windows-path-prefix = 'C:\data\' }
"#,
        ));
        assert!(two_flavors.contains("two path flavors"), "{two_flavors}");

        // The same flavor twice is consistent, so it is fine.
        let repeated = ok(&with_grants(
            r#"
[[grant]]
tool = "read_file"
[grant.args]
path = { path-prefix = "/data/" }

[[grant]]
tool = "read_file"
[grant.args]
path = { path-prefix = "/other/" }
"#,
        ));
        assert_eq!(repeated.grants.unwrap().envelope.grants.len(), 2);
    }

    /// Warnings never stop the process: each of these grants can only
    /// deny, which costs availability and never authority.
    #[test]
    fn the_warning_table_does_not_refuse() {
        let config = ok(&with_grants(
            r#"
[[grant]]
tool = "t"
[grant.args]
empty = { one-of = [] }
backwards = { range = { min = 10, max = 1 } }
"#,
        ));
        assert!(config.grants.is_some());
        let warnings = config.warnings.join("; ");
        assert!(warnings.contains("empty `one-of`"), "{warnings}");
        assert!(warnings.contains("above max"), "{warnings}");

        // The mirror of those two: a constraint that admits *everything*
        // is reported as well. It is the operator's own bytes, so it is
        // not refused — but the loader had been silent about the only
        // direction that costs authority.
        let wide = ok(&with_grants(
            r#"
[[grant]]
tool = "t"
[grant.args]
p = { prefix = "" }
s = { suffix = "" }
"#,
        ));
        let warnings = wide.warnings.join("; ");
        assert!(warnings.contains("empty `prefix`"), "{warnings}");
        assert!(warnings.contains("empty `suffix`"), "{warnings}");

        // A principal with no grants is not enforcement, and says so.
        let stray = ok(&format!("version = 1\nprincipal = \"bot\"\n{UPSTREAM}"));
        assert!(stray.grants.is_none());
        assert!(stray.warnings.join("; ").contains("no [[grant]]"));
    }

    /// `expires` must name one instant. Every offset-less TOML form is
    /// refused, and the accepted ones convert exactly.
    #[test]
    fn expiry_conversion_and_its_refusals() {
        for bad in ["2026-09-01T00:00:00", "2026-09-01", "00:32:00"] {
            let message = err(&with_grants(&format!(
                "[[grant]]\ntool = \"t\"\nexpires = {bad}\n"
            )));
            assert!(
                message.contains("offset") || message.contains("date *and* a time"),
                "{bad}: {message}"
            );
        }

        let expires = |literal: &str| -> i64 {
            let config = ok(&with_grants(&format!(
                "[[grant]]\ntool = \"t\"\nexpires = {literal}\n"
            )));
            config.grants.unwrap().envelope.grants[0]
                .expires
                .unwrap()
                .unix_secs()
        };
        // The eight reference instants: epoch, a leap day, the 2100
        // non-leap century boundary, a pre-epoch negative, the 2038
        // boundary, and an offset agreeing with its Z equivalent.
        assert_eq!(expires("1970-01-01T00:00:00Z"), 0);
        assert_eq!(expires("1970-01-01T00:00:01Z"), 1);
        assert_eq!(expires("2000-02-29T12:00:00Z"), 951_825_600);
        assert_eq!(expires("2100-03-01T00:00:00Z"), 4_107_542_400);
        assert_eq!(expires("1969-12-31T23:59:59Z"), -1);
        assert_eq!(expires("1900-01-01T00:00:00Z"), -2_208_988_800);
        assert_eq!(expires("2038-01-19T03:14:07Z"), 2_147_483_647);
        assert_eq!(
            expires("2026-09-01T02:00:00+02:00"),
            expires("2026-09-01T00:00:00Z")
        );
        assert_eq!(
            expires("2026-08-31T22:00:00-02:00"),
            expires("2026-09-01T00:00:00Z")
        );
        // Sub-second precision is dropped downward.
        assert_eq!(
            expires("2026-09-01T00:00:00.999Z"),
            expires("2026-09-01T00:00:00Z")
        );
    }

    #[test]
    fn upstreams_still_parse_both_transport_kinds() {
        let config = ok(&format!(
            r#"
version = 1
{UPSTREAM}
[[upstream]]
name = "web"
url = "https://example.com/mcp"
headers = {{ Authorization = "Bearer token" }}
"#
        ));
        assert_eq!(config.upstreams.len(), 2);
        assert!(config.grants.is_none());
        assert!(matches!(
            &config.upstreams[0].transport,
            TransportSpec::Stdio { .. }
        ));
        match &config.upstreams[1].transport {
            TransportSpec::Http { url, headers } => {
                assert_eq!(url, "https://example.com/mcp");
                assert_eq!(headers[0].0, "Authorization");
            }
            other => panic!("expected http, got {other:?}"),
        }
    }

    #[test]
    fn upstream_shape_errors_are_unchanged_from_m2() {
        assert!(err("version = 1\n[[upstream]]\nname = \"x\"\ncommand = [\"a\"]\nurl = \"https://e.com/\"\n")
            .contains("exactly one of"));
        assert!(err("version = 1\n[[upstream]]\nname = \"x\"\n").contains("exactly one of"));
        assert!(err(
            "version = 1\n[[upstream]]\nname = \"x\"\ncommand = [\"a\"]\nheaders = { A = \"b\" }\n"
        )
        .contains("only applies to `url`"));
        assert!(err("").contains("version"));
    }

    #[test]
    fn days_from_civil_matches_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2000, 2, 29), 11_016);
        assert_eq!(days_from_civil(1900, 3, 1), -25_508);
        assert_eq!(days_from_civil(2100, 2, 28), 47_540);
        // 1900 and 2100 are not leap years; 2000 is.
        assert_eq!(
            days_from_civil(2100, 3, 1) - days_from_civil(2100, 2, 28),
            1
        );
        assert_eq!(
            days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 28),
            2
        );
    }
}
