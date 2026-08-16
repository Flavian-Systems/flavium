//! Path normalization: the one place a value is rewritten before a grant
//! decision is made about it.
//!
//! Flavium decides on the *spelling* of an argument; the upstream acts on
//! the *resource* that spelling resolves to. Every gap between the two is
//! a **false allow** or a **false denial**, and they are not equally bad:
//! a false denial announces itself, a false allow is silent. Without
//! normalizing, `Prefix("/data/invoices/")` is a byte prefix of
//! `"/data/invoices/../../etc/passwd"` — the reference semantics allow it
//! while the upstream reads `/etc/passwd`. That is the false allow this
//! module exists to close (T1/M5 plan, D4).
//!
//! Two properties make it safe to apply:
//!
//! - **Opt-in per argument.** Only the arguments a grant marks as paths
//!   (`path-prefix`, `windows-path-prefix`) are normalized. Normalizing
//!   an address, a pattern or document text would silently change what
//!   the decision was about.
//! - **The flavor is declared, never guessed.** Whether `\` separates
//!   cannot be inferred from the proxy's own host: an HTTP upstream is
//!   another machine, and a stdio child can be in WSL or a container. A
//!   grant names one tool, a tool belongs to one upstream, so the grant is
//!   exactly the scope at which an operator knows the answer.
//!
//! The normalizer is total, pure, byte level, and does no I/O: separators
//! unified, repeated separators collapsed — *except* a leading run of two
//! or more, which is a different root (see [`normalize`]) — `.` segments
//! dropped, `..` resolved against the previous segment, never escaping the
//! root of an absolute path, a leading `..` of a relative path kept,
//! trailing separator dropped from a value and kept on a prefix, and —
//! **under [`PathFlavor::Windows`] only** — ASCII case folded.
//!
//! Case folding follows the same reasoning as the separator: it is part of
//! the resolution rule the operator declared, not a guess about the host.
//! Windows resolves `c:\users\me\x` and `C:\Users\Me\X` to one file, so a
//! grant that admits one and refuses the other decides on the spelling
//! rather than the resource — the very thing this module exists to stop.
//! **Folding is ASCII only** (`A`–`Z` ↔ `a`–`z`), the set Windows is
//! guaranteed to fold: full Unicode lowercasing can merge characters
//! Windows keeps apart, and can even change a string's length (`İ`), and
//! either would be a false *allow*. A non-ASCII case difference therefore
//! still refuses a call the upstream would have served — a false denial,
//! which is the side this module always errs to. `PathFlavor::Posix` folds
//! nothing.
//!
//! Windows *does* fold beyond ASCII, and deliberately not matching it is
//! the point: it folds through an upcase table written **into the volume
//! at format time** — `$UpCase` on NTFS, the Up-case Table on exFAT — so
//! which non-ASCII characters are one name is a property of that volume
//! and of the Windows version that formatted it, not a constant. A proxy
//! cannot read it either: an upstream may be another machine. `A`–`Z` is
//! the subset every such table agrees on, so it is the subset that can be
//! folded without guessing.
//!
//! Folding does not promise that the upstream will *serve* a path, only
//! that flavium is not the one refusing it on spelling: an upstream may
//! run its own case-sensitive check (`@modelcontextprotocol/server-filesystem`
//! compares its allowed-directories root that way), and a path this
//! module admits can still come back as that upstream's own error.
//!
//! Still **no filesystem access and no symlink resolution** — symlinks and
//! hardlinks are outside what a proxy can see (DESIGN §7). Two residual
//! gaps, both named rather than hidden: a POSIX upstream wrongly declared
//! `windows-path-prefix` now folds case as well as separators (declaring
//! the flavor asserts the whole resolution rule, and that upstream needed
//! `path-prefix`), and an NTFS directory switched to case-sensitive
//! (`fsutil file setCaseSensitiveInfo`) holds files this normalizer treats
//! as one — rare, opt-in per directory, and a false allow only between two
//! files whose names differ solely by case.
//!
//! # Example
//!
//! ```
//! use flavium_proxy_mcp::normalize::{normalize, normalize_prefix, PathFlavor};
//!
//! // `..` is resolved, so the escape no longer matches the prefix.
//! assert_eq!(
//!     normalize("/data/invoices/../../etc/passwd", PathFlavor::Posix),
//!     "/etc/passwd"
//! );
//! // On POSIX a backslash is an ordinary filename byte, not a separator.
//! assert_eq!(normalize(r"\data\x", PathFlavor::Posix), r"\data\x");
//! assert_eq!(normalize(r"\data\x", PathFlavor::Windows), "/data/x");
//! // …but a UNC root stays distinct from the current drive's root.
//! assert_eq!(normalize(r"\\host\share\x", PathFlavor::Windows), "//host/share/x");
//! // The Windows flavor folds ASCII case, so one file has one spelling.
//! assert_eq!(normalize(r"C:\Users\Me\X", PathFlavor::Windows), "c:/users/me/x");
//! assert_eq!(normalize("/DATA/x", PathFlavor::Posix), "/DATA/x");
//!
//! // A prefix keeps the trailing separator its author wrote: dropping it
//! // would widen `/data/invoices/` to also admit `/data/invoices.bak`.
//! assert_eq!(
//!     normalize_prefix("/data/invoices/", PathFlavor::Posix),
//!     "/data/invoices/"
//! );
//! // And a prefix that reduces to nothing does not gain a root — `./`
//! // must not become "the whole filesystem".
//! assert_eq!(normalize_prefix("./", PathFlavor::Posix), "");
//! ```

/// How a grant says separators work for one argument of one tool.
///
/// Declared per grant because neither fixed answer is safe: treating `\`
/// as a separator on a POSIX upstream turns `\data\invoices\x` — one
/// filename there — into an allow under `Prefix("/data/invoices/")`,
/// while *not* treating it as one means `..` never resolves in a Windows
/// path, so `C:\Users\me\Desktop\..\..\Administrator\secrets` sits inside
/// `C:\Users\me\Desktop\`. Each is a false allow on one platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PathFlavor {
    /// Only `/` separates; `\` is an ordinary filename byte.
    Posix,
    /// Both `/` and `\` separate, and `/` is the normalized spelling.
    Windows,
}

impl PathFlavor {
    /// Does this flavor treat `c` as a path separator?
    pub fn is_separator(self, c: char) -> bool {
        c == '/' || (self == PathFlavor::Windows && c == '\\')
    }
}

/// Normalizes one path *value* — the argument of a call, before the
/// decision is made about it.
///
/// Total: every input yields a string, and nothing here can panic. The
/// original bytes are still what the proxy forwards; only the decision
/// (and the trace of it) sees this form.
///
/// | Input | `Posix` | `Windows` |
/// |---|---|---|
/// | `/data/invoices/../../etc/passwd` | `/etc/passwd` | `/etc/passwd` |
/// | `/data//./invoices/` | `/data/invoices` | `/data/invoices` |
/// | `\data\x` | `\data\x` | `/data/x` |
/// | `C:\Users\me\..\other` | `C:\Users\me\..\other` | `c:/users/other` |
/// | `/..` | `/` | `/` |
/// | `../a` | `../a` | `../a` |
/// | `` (empty) | `` | `` |
/// | `\\server\share\x` | `\\server\share\x` | `//server/share/x` |
/// | `/DATA/x` | `/DATA/x` | `/data/x` |
///
/// The `Windows` column is ASCII lowercase throughout: that flavor folds
/// case (see the module docs), because Windows resolves paths that differ
/// only in case to the same file. `Posix` leaves case alone.
///
/// A Windows drive letter is just the first segment (`c:/users/…`), but a
/// **leading run of two or more separators is preserved as exactly two**
/// and is never collapsed into one. That distinction is load-bearing: on
/// Windows `\\data\share\x` is a UNC path to a share on a *host* called
/// `data`, while `\data\share\x` is a directory on the current drive —
/// two different machines. Collapsing them would let a grant over the
/// local `\data\` admit a write to a remote server. POSIX gets the same
/// treatment because the standard leaves a leading `//` implementation-
/// defined, and "I cannot tell" resolves to a denial here.
pub fn normalize(value: &str, flavor: PathFlavor) -> String {
    let root = match value
        .chars()
        .take_while(|c| flavor.is_separator(*c))
        .count()
    {
        0 => "",
        1 => "/",
        _ => "//",
    };
    let mut segments: Vec<&str> = Vec::new();
    let absolute = !root.is_empty();
    for segment in value.split(|c| flavor.is_separator(c)) {
        match segment {
            // Repeated separators, and a trailing one, produce empty
            // segments; `.` is the current directory. Both are dropped.
            "" | "." => {}
            ".." => match segments.last() {
                // A relative path may start with `..`, and further `..`
                // stack onto it — there is no known parent to cancel.
                Some(&"..") | None if !absolute => segments.push(".."),
                // An absolute path's root is its own parent: `/..` is `/`.
                None => {}
                Some(_) => {
                    segments.pop();
                }
            },
            other => segments.push(other),
        }
    }
    let joined = segments.join("/");
    let mut out = String::with_capacity(root.len() + joined.len());
    out.push_str(root);
    out.push_str(&joined);
    // ASCII only, and only for the flavor whose resolution rule says so:
    // `make_ascii_lowercase` cannot change the string's length or merge
    // characters Windows keeps apart, so it cannot widen a prefix by
    // accident. Non-ASCII is left exactly as it arrived.
    if flavor == PathFlavor::Windows {
        out.make_ascii_lowercase();
    }
    out
}

/// Normalizes one path *prefix* — what a grant wrote.
///
/// The same normalization as [`normalize`] — including the Windows
/// flavor's ASCII case folding, so both sides of the comparison are folded
/// or neither is — except that a trailing separator survives. That
/// difference is load-bearing: `/data/invoices/`
/// normalized to `/data/invoices` would, as a byte prefix, also admit
/// `/data/invoices.bak/secret` — **normalization must never widen what an
/// operator wrote**, in either direction.
///
/// A prefix that reduces to nothing (`.`, `./`, `a/..`) comes back empty
/// rather than gaining a root: re-appending the separator there would turn
/// a *relative* prefix into the filesystem root, which is the widest
/// possible widening. An empty result is not a usable prefix — it admits
/// every string — so the grant loader refuses it rather than compiling it.
pub fn normalize_prefix(prefix: &str, flavor: PathFlavor) -> String {
    let mut out = normalize(prefix, flavor);
    if out.is_empty() {
        return out;
    }
    let trailing = prefix.ends_with(|c| flavor.is_separator(c));
    if trailing && !out.ends_with('/') {
        out.push('/');
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The D4 table, both flavors, plus the shapes a hostile client
    /// reaches for.
    #[test]
    fn normalization_table() {
        let rows: &[(&str, &str, &str)] = &[
            // value, posix, windows
            ("", "", ""),
            ("/", "/", "/"),
            ("a//b", "a/b", "a/b"),
            ("/a//b", "/a/b", "/a/b"),
            // A leading run of two or more separators is a different
            // root, not a doubled one — see `the_unc_root_is_not_a_local_root`.
            ("//", "//", "//"),
            ("///", "//", "//"),
            ("///a///b///", "//a/b", "//a/b"),
            (".", "", ""),
            ("..", "..", ".."),
            ("../a", "../a", "../a"),
            ("../../a", "../../a", "../../a"),
            ("a/../..", "..", ".."),
            ("/..", "/", "/"),
            ("/../..", "/", "/"),
            ("/a/../..", "/", "/"),
            ("/./a/./b/.", "/a/b", "/a/b"),
            ("/data/invoices/", "/data/invoices", "/data/invoices"),
            ("/data//./invoices/", "/data/invoices", "/data/invoices"),
            (
                "/data/invoices/../../etc/passwd",
                "/etc/passwd",
                "/etc/passwd",
            ),
            (r"\data\x", r"\data\x", "/data/x"),
            (r"/data/x\..\y", r"/data/x\..\y", "/data/y"),
            (
                r"C:\Users\me\Desktop\..\..\Administrator\secrets",
                r"C:\Users\me\Desktop\..\..\Administrator\secrets",
                "c:/users/administrator/secrets",
            ),
            (r"\\server\share\x", r"\\server\share\x", "//server/share/x"),
            (r"\server\share\x", r"\server\share\x", "/server/share/x"),
            // ASCII case folded under the Windows flavor, never under POSIX.
            ("/DATA/x", "/DATA/x", "/data/x"),
            (r"C:\Users\Me\X", r"C:\Users\Me\X", "c:/users/me/x"),
            // Not a path traversal: `...` and `..a` are ordinary names.
            ("/a/.../b", "/a/.../b", "/a/.../b"),
            ("/a/..b/c", "/a/..b/c", "/a/..b/c"),
            // Non-ASCII survives byte for byte.
            ("/data/f\u{e9}/../g", "/data/g", "/data/g"),
        ];
        for (value, posix, windows) in rows {
            assert_eq!(
                &normalize(value, PathFlavor::Posix),
                posix,
                "posix {value:?}"
            );
            assert_eq!(
                &normalize(value, PathFlavor::Windows),
                windows,
                "windows {value:?}"
            );
        }
    }

    /// The two false allows the flavor exists to prevent — each is the
    /// row that kills the "always translate" and "never translate"
    /// answers, kept as regression tests.
    #[test]
    fn the_two_false_allows_stay_closed() {
        // i — `/` only, always: on Windows no `..` ever resolves, so the
        // escape stays inside the granted prefix.
        let escape = r"C:\Users\me\Desktop\..\..\Administrator\secrets";
        let grant = normalize_prefix(r"C:\Users\me\Desktop\", PathFlavor::Windows);
        assert!(
            !normalize(escape, PathFlavor::Windows).starts_with(&grant),
            "the Windows flavor must resolve `..` across backslashes"
        );
        assert!(
            normalize(escape, PathFlavor::Posix).starts_with(&normalize_prefix(
                r"C:\Users\me\Desktop\",
                PathFlavor::Posix
            )),
            "the POSIX reading of a Windows path is exactly the false allow"
        );

        // ii — `/` and `\`, always: on POSIX `\data\invoices\x` is one
        // filename in the working directory, not a path under /data.
        let posix_filename = r"\data\invoices\x";
        assert!(
            !normalize(posix_filename, PathFlavor::Posix)
                .starts_with(&normalize_prefix("/data/invoices/", PathFlavor::Posix)),
            "a POSIX filename containing backslashes must not match a /data prefix"
        );
        assert!(
            normalize(posix_filename, PathFlavor::Windows)
                .starts_with(&normalize_prefix(r"\data\invoices\", PathFlavor::Windows)),
            "under the Windows flavor the same bytes are a path and do match"
        );
    }

    /// The Windows flavor folds ASCII case on both sides of the
    /// comparison — and folds nothing else.
    ///
    /// The allow rows are the live finding this behavior comes from: the
    /// T1/M5 run against `server-filesystem` denied
    /// `c:\users\flavi\desktop\flavium-demo\ok.txt` while allowing the
    /// same file spelled `C:\Users\…`, which is a decision about the
    /// spelling rather than the resource. The deny rows are what folding
    /// must *not* buy: a different directory stays outside, `..` still
    /// resolves before the comparison, and a non-ASCII case difference is
    /// still refused (the documented false denial).
    #[test]
    fn the_windows_flavor_folds_ascii_case_only() {
        let grant = normalize_prefix(r"C:\Users\flavi\Desktop\flavium-demo\", PathFlavor::Windows);
        assert_eq!(grant, "c:/users/flavi/desktop/flavium-demo/");
        for allowed in [
            r"C:\Users\flavi\Desktop\flavium-demo\ok.txt",
            r"c:\users\flavi\desktop\flavium-demo\ok.txt",
            r"C:\USERS\FLAVI\DESKTOP\FLAVIUM-DEMO\OK.TXT",
            "c:/Users/flavi/Desktop/flavium-demo/ok.txt",
            r"c:\users\flavi\desktop\other\..\flavium-demo\ok.txt",
        ] {
            assert!(
                normalize(allowed, PathFlavor::Windows).starts_with(&grant),
                "case-equal path was refused: {allowed:?}"
            );
        }
        for denied in [
            r"C:\Users\flavi\Desktop\outside.txt",
            r"c:\users\flavi\desktop\flavium-demo\..\outside.txt",
            r"C:\Users\flavi\Desktop\flavium-demo2\x",
            r"D:\Users\flavi\Desktop\flavium-demo\ok.txt",
            r"\\host\users\flavi\desktop\flavium-demo\ok.txt",
        ] {
            assert!(
                !normalize(denied, PathFlavor::Windows).starts_with(&grant),
                "folding case admitted {denied:?}"
            );
        }

        // Non-ASCII is left alone: `Ä` and `ä` stay distinct, so this is
        // refused even though Windows would serve it. A false denial —
        // the side this module errs to — and the reason folding is ASCII
        // only is that the Unicode answer can merge or resize.
        let accented = normalize_prefix("C:\\data\\\u{c4}\\", PathFlavor::Windows);
        assert_eq!(accented, "c:/data/\u{c4}/");
        assert!(!normalize("C:\\data\\\u{e4}\\x", PathFlavor::Windows).starts_with(&accented));
        assert!(normalize("C:\\DATA\\\u{c4}\\x", PathFlavor::Windows).starts_with(&accented));

        // POSIX folds nothing: the flavor difference is the whole point.
        let posix = normalize_prefix("/data/Invoices/", PathFlavor::Posix);
        assert_eq!(posix, "/data/Invoices/");
        assert!(!normalize("/data/invoices/x", PathFlavor::Posix).starts_with(&posix));
        assert!(normalize("/data/Invoices/x", PathFlavor::Posix).starts_with(&posix));
    }

    /// A prefix must never come out wider than it went in.
    #[test]
    fn prefix_keeps_its_trailing_separator() {
        for (raw, flavor, expected) in [
            ("/data/invoices/", PathFlavor::Posix, "/data/invoices/"),
            ("/data/invoices", PathFlavor::Posix, "/data/invoices"),
            ("/data/invoices//", PathFlavor::Posix, "/data/invoices/"),
            ("/data/./invoices/", PathFlavor::Posix, "/data/invoices/"),
            ("/", PathFlavor::Posix, "/"),
            ("//", PathFlavor::Posix, "//"),
            ("", PathFlavor::Posix, ""),
            ("/..", PathFlavor::Posix, "/"),
            (r"\data\", PathFlavor::Windows, "/data/"),
            (r"\\host\share\", PathFlavor::Windows, "//host/share/"),
            ("C:/Users/me/", PathFlavor::Windows, "c:/users/me/"),
            (r"C:\Users\me\", PathFlavor::Windows, "c:/users/me/"),
            (r"\data\", PathFlavor::Posix, r"\data\"),
        ] {
            assert_eq!(
                normalize_prefix(raw, flavor),
                expected,
                "{raw:?} {flavor:?}"
            );
        }

        // The widening a dropped separator would cause.
        let grant = normalize_prefix("/data/invoices/", PathFlavor::Posix);
        assert!(!normalize("/data/invoices.bak/secret", PathFlavor::Posix).starts_with(&grant));
        assert!(normalize("/data/invoices/2026-01.pdf", PathFlavor::Posix).starts_with(&grant));
    }

    /// A prefix that reduces to nothing must **not** grow a root.
    ///
    /// `./` normalizing to `/` would take a grant over the working
    /// directory and turn it into a grant over the whole filesystem — the
    /// widest widening there is, manufactured by the very step that
    /// exists to prevent widening. The empty result is not usable as a
    /// prefix either (it admits every string), which is why the grant
    /// loader refuses it outright; see `grants::ArgEntry::compile`.
    #[test]
    fn a_prefix_that_reduces_to_nothing_does_not_gain_a_root() {
        for raw in ["", ".", "./", "./.", "a/..", "a/../", "data/../", "x/./.."] {
            for flavor in [PathFlavor::Posix, PathFlavor::Windows] {
                let out = normalize_prefix(raw, flavor);
                assert_eq!(out, "", "{raw:?} {flavor:?} became {out:?}");
            }
        }
        for raw in [r".\", r"a\..", r"a\..\"] {
            assert_eq!(normalize_prefix(raw, PathFlavor::Windows), "", "{raw:?}");
        }
    }

    /// On Windows a leading `\\` is a UNC root — another *machine* — and
    /// a single `\` is the current drive. Folding them together would let
    /// a grant over a local directory admit a write to a remote share
    /// (handing that server the upstream's credentials on the way), and a
    /// grant over a share admit a local path.
    #[test]
    fn the_unc_root_is_not_a_local_root() {
        let local_grant = normalize_prefix(r"\data\", PathFlavor::Windows);
        assert_eq!(local_grant, "/data/");
        for remote in [
            r"\\data\share\loot.txt",
            "//data/share/loot.txt",
            r"\\\data\share\x",
        ] {
            assert!(
                !normalize(remote, PathFlavor::Windows).starts_with(&local_grant),
                "a local grant admitted the UNC path {remote:?}"
            );
        }
        assert!(normalize(r"\data\share\ok.txt", PathFlavor::Windows).starts_with(&local_grant));

        // …and the mirror: a share grant must not admit a local path.
        let share_grant = normalize_prefix(r"\\fileserver\invoices\", PathFlavor::Windows);
        assert_eq!(share_grant, "//fileserver/invoices/");
        assert!(
            !normalize(r"\fileserver\invoices\x", PathFlavor::Windows).starts_with(&share_grant)
        );
        assert!(
            normalize(r"\\fileserver\invoices\x", PathFlavor::Windows).starts_with(&share_grant)
        );

        // POSIX keeps the distinction too: the standard leaves a leading
        // `//` implementation-defined, so flavium cannot claim to know,
        // and "I cannot tell" resolves to a denial.
        let posix_grant = normalize_prefix("/data/", PathFlavor::Posix);
        assert!(!normalize("//data/x", PathFlavor::Posix).starts_with(&posix_grant));
        assert!(normalize("/data/x", PathFlavor::Posix).starts_with(&posix_grant));
    }

    /// Normalizing a value already in normal form changes nothing, and a
    /// second pass changes nothing either.
    #[test]
    fn normalization_is_idempotent() {
        for value in [
            "",
            "/",
            "..",
            "../a",
            "/a/b",
            r"\data\x",
            r"C:\a\b",
            "/data/invoices/../../etc/passwd",
            "///a///",
        ] {
            for flavor in [PathFlavor::Posix, PathFlavor::Windows] {
                let once = normalize(value, flavor);
                assert_eq!(normalize(&once, flavor), once, "{value:?} {flavor:?}");
            }
        }
    }

    #[test]
    fn separator_membership() {
        assert!(PathFlavor::Posix.is_separator('/'));
        assert!(!PathFlavor::Posix.is_separator('\\'));
        assert!(PathFlavor::Windows.is_separator('/'));
        assert!(PathFlavor::Windows.is_separator('\\'));
        assert!(!PathFlavor::Windows.is_separator('a'));
    }
}
