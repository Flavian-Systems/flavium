//! The merged tool table: which tools exist, which upstream serves each,
//! and the bytes the client is shown.
//!
//! Tool objects are kept as the raw bytes each upstream sent — the proxy
//! routes by `name` and forwards everything else (schemas, titles,
//! icons, `_meta`, fields not yet invented) untouched. Pagination is an
//! upstream-side concern only: the proxy drains every upstream's pages
//! internally and always presents the client one unpaginated list, so a
//! cursor from the client is by definition foreign and rejected by the
//! router.
//!
//! Duplicate tool names — across upstreams or within one — are rejected:
//! a name that routes to two places is ambiguous authority, and T1
//! resolves ambiguity by refusing to serve it (namespacing is the
//! documented follow-up, not silent precedence).

use serde::Deserialize;
use serde_json::value::RawValue;

/// Hard cap on tools/list pages drained per upstream; a server that
/// pages past this is treated as broken rather than followed forever.
pub const MAX_LIST_PAGES: usize = 1_000;

/// Hard cap on total tools accepted per upstream.
pub const MAX_TOOLS_PER_UPSTREAM: usize = 10_000;

/// One tool as an upstream declared it: the routing name plus the
/// original object bytes.
#[derive(Debug, Clone)]
pub struct ToolEntry {
    /// The tool's `name`, the routing key.
    pub name: String,
    /// The tool object exactly as the upstream sent it.
    pub raw: Box<RawValue>,
}

/// Errors reading a single `tools/list` result page.
#[derive(Debug, thiserror::Error)]
pub enum ListPageError {
    /// The result was not an object with a `tools` array.
    #[error("tools/list result is not an object with a \"tools\" array")]
    BadShape,

    /// A tool object had a missing, non-string, or empty `name`.
    #[error("a listed tool has a missing or empty name")]
    BadToolName,

    /// The `nextCursor` member was present but not a string.
    #[error("nextCursor is not a string")]
    BadCursor,
}

/// One parsed `tools/list` result page.
#[derive(Debug)]
pub struct ListPage {
    /// The tools on this page.
    pub tools: Vec<ToolEntry>,
    /// The upstream's continuation cursor, if it paginated.
    pub next_cursor: Option<String>,
}

impl ListPage {
    /// Parses a `tools/list` result, keeping each tool's raw bytes.
    pub fn parse(result_raw: &str) -> Result<Self, ListPageError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PageWire<'a> {
            #[serde(borrow)]
            tools: Vec<&'a RawValue>,
            #[serde(borrow, default)]
            next_cursor: Option<&'a RawValue>,
        }

        #[derive(Deserialize)]
        struct ToolNameWire {
            name: String,
        }

        let page: PageWire<'_> =
            serde_json::from_str(result_raw).map_err(|_| ListPageError::BadShape)?;
        // The cursor is validated separately so an absent cursor (fine)
        // is distinguishable from a present non-string one (typed
        // error, fail closed).
        let next_cursor = match page.next_cursor {
            None => None,
            Some(raw) => serde_json::from_str::<Option<String>>(raw.get())
                .map_err(|_| ListPageError::BadCursor)?,
        };

        let mut tools = Vec::with_capacity(page.tools.len());
        for raw in page.tools {
            let ToolNameWire { name } =
                serde_json::from_str(raw.get()).map_err(|_| ListPageError::BadToolName)?;
            if name.is_empty() {
                return Err(ListPageError::BadToolName);
            }
            tools.push(ToolEntry {
                name,
                raw: raw.to_owned(),
            });
        }
        Ok(Self { tools, next_cursor })
    }
}

/// A duplicate tool name, with the (upstream-index) claimants.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("tool {name:?} is offered by upstream #{first} and upstream #{second}")]
pub struct ToolCollision {
    /// The contested tool name.
    pub name: String,
    /// Index of the upstream that declared it first.
    pub first: usize,
    /// Index of the upstream that declared it again.
    pub second: usize,
}

/// The merged, collision-checked tool table for one session.
#[derive(Debug, Default)]
pub struct ToolSet {
    /// Tool lists per upstream, in upstream order.
    per_upstream: Vec<Vec<ToolEntry>>,
    /// name → upstream index.
    routes: std::collections::HashMap<String, usize>,
}

impl ToolSet {
    /// Builds the table, rejecting any duplicate name — within one
    /// upstream (`first == second`) or across upstreams.
    pub fn build(per_upstream: Vec<Vec<ToolEntry>>) -> Result<Self, ToolCollision> {
        let mut routes = std::collections::HashMap::new();
        for (index, tools) in per_upstream.iter().enumerate() {
            for tool in tools {
                if let Some(&first) = routes.get(&tool.name) {
                    return Err(ToolCollision {
                        name: tool.name.clone(),
                        first,
                        second: index,
                    });
                }
                routes.insert(tool.name.clone(), index);
            }
        }
        Ok(Self {
            per_upstream,
            routes,
        })
    }

    /// Which upstream serves `name`, if any.
    pub fn route(&self, name: &str) -> Option<usize> {
        self.routes.get(name).copied()
    }

    /// Total number of tools across all upstreams.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// True when no upstream offers any tool.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// One upstream's current entries, cloned out of the table (used
    /// when re-merging after a `list_changed`).
    pub fn tools_of(&self, upstream: usize) -> Vec<ToolEntry> {
        self.per_upstream.get(upstream).cloned().unwrap_or_default()
    }

    /// The unpaginated merged `tools/list` result: every upstream's
    /// tools in upstream order, each tool byte-identical to how its
    /// upstream declared it, and no cursor — the proxy never mints one.
    pub fn merged_result(&self) -> String {
        let mut out = String::from(r#"{"tools":["#);
        let mut first = true;
        for tools in &self.per_upstream {
            for tool in tools {
                if !first {
                    out.push(',');
                }
                first = false;
                out.push_str(tool.raw.get());
            }
        }
        out.push_str("]}");
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn entry(name: &str, raw: &str) -> ToolEntry {
        ToolEntry {
            name: name.to_owned(),
            raw: serde_json::from_str(raw).unwrap(),
        }
    }

    #[test]
    fn parses_a_page_keeping_tool_bytes() {
        let page = ListPage::parse(
            r#"{"tools": [{"name": "echo", "inputSchema": { "type":  "object" }, "future": [1e2]}], "nextCursor": "p2"}"#,
        )
        .unwrap();
        assert_eq!(page.tools.len(), 1);
        assert_eq!(page.tools[0].name, "echo");
        assert_eq!(
            page.tools[0].raw.get(),
            r#"{"name": "echo", "inputSchema": { "type":  "object" }, "future": [1e2]}"#
        );
        assert_eq!(page.next_cursor.as_deref(), Some("p2"));
    }

    #[test]
    fn absent_and_null_cursors_both_mean_done() {
        let page = ListPage::parse(r#"{"tools": []}"#).unwrap();
        assert!(page.next_cursor.is_none());
        let page = ListPage::parse(r#"{"tools": [], "nextCursor": null}"#).unwrap();
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn malformed_pages_are_typed_errors() {
        assert!(matches!(
            ListPage::parse(r#"{"tools": [], "nextCursor": 7}"#),
            Err(ListPageError::BadCursor)
        ));
        for bad in [
            r#"{"no_tools": []}"#,
            r#"{"tools": {}}"#,
            "[]",
            "42",
            "{broken",
        ] {
            assert!(
                matches!(ListPage::parse(bad), Err(ListPageError::BadShape)),
                "input {bad:?}"
            );
        }
        for bad_tool in [
            r#"{"tools": [{"inputSchema": {}}]}"#,
            r#"{"tools": [{"name": 7}]}"#,
            r#"{"tools": [{"name": ""}]}"#,
            r#"{"tools": [42]}"#,
        ] {
            assert!(
                matches!(ListPage::parse(bad_tool), Err(ListPageError::BadToolName)),
                "input {bad_tool:?}"
            );
        }
    }

    #[test]
    fn routes_and_merges_across_upstreams_in_order() {
        let set = ToolSet::build(vec![
            vec![entry("read", r#"{"name":"read", "x": 1}"#)],
            vec![
                entry("write", r#"{"name":"write"}"#),
                entry("send", r#"{"name":"send",  "y": [2.50]}"#),
            ],
        ])
        .unwrap();
        assert_eq!(set.route("read"), Some(0));
        assert_eq!(set.route("send"), Some(1));
        assert_eq!(set.route("missing"), None);
        assert_eq!(set.len(), 3);
        assert_eq!(
            set.merged_result(),
            r#"{"tools":[{"name":"read", "x": 1},{"name":"write"},{"name":"send",  "y": [2.50]}]}"#
        );
    }

    #[test]
    fn collisions_across_upstreams_are_rejected() {
        let err = ToolSet::build(vec![
            vec![entry("echo", r#"{"name":"echo"}"#)],
            vec![entry("echo", r#"{"name":"echo"}"#)],
        ])
        .unwrap_err();
        assert_eq!(
            err,
            ToolCollision {
                name: "echo".into(),
                first: 0,
                second: 1
            }
        );
    }

    #[test]
    fn collisions_within_one_upstream_are_rejected() {
        let err = ToolSet::build(vec![vec![
            entry("dup", r#"{"name":"dup"}"#),
            entry("dup", r#"{"name":"dup"}"#),
        ]])
        .unwrap_err();
        assert_eq!(err.first, 0);
        assert_eq!(err.second, 0);
        assert_eq!(err.name, "dup");
    }

    #[test]
    fn empty_toolset_serves_an_empty_list() {
        let set = ToolSet::build(vec![vec![], vec![]]).unwrap();
        assert!(set.is_empty());
        assert_eq!(set.merged_result(), r#"{"tools":[]}"#);
    }
}
