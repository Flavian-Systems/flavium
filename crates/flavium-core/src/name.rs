//! Validated names: [`Principal`] and [`ToolName`].
//!
//! Both are thin newtypes over `String` whose only invariant is *well-formed
//! for everywhere a name travels*: non-empty and free of ASCII control
//! characters (so a name can never break a log line or a JSONL record, and
//! is a valid Cedar entity id). They are used on the **grant** side. Names
//! that arrive from clients or upstreams (`ToolCall::tool`, trace fields)
//! stay plain `String`s: a name that would fail validation can never equal
//! a grant's name and therefore falls out as `NotGranted` — fail closed with
//! no special path.

use std::borrow::Borrow;
use std::fmt;

/// Why a string is not a valid [`Principal`] or [`ToolName`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidName {
    /// The name is empty.
    Empty,
    /// The name contains an ASCII control character (`0x00..=0x1F` or
    /// `0x7F`).
    ControlCharacter,
}

impl fmt::Display for InvalidName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InvalidName::Empty => f.write_str("name is empty"),
            InvalidName::ControlCharacter => f.write_str("name contains a control character"),
        }
    }
}

impl std::error::Error for InvalidName {}

/// The one validation rule shared by both name types.
fn validate(name: &str) -> Result<(), InvalidName> {
    if name.is_empty() {
        return Err(InvalidName::Empty);
    }
    if name.bytes().any(|b| b.is_ascii_control()) {
        return Err(InvalidName::ControlCharacter);
    }
    Ok(())
}

/// The identity a call is attributed to for authorization and tracing.
///
/// In T1 it is static per proxy process (from configuration); MCP
/// `clientInfo` is untrusted data and never identity. Identity is not
/// authority: a [`crate::GrantEnvelope`] binds a principal to the grants it
/// holds, and attenuation compares grants, not principals.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Principal(String);

impl Principal {
    /// Validates and wraps a principal name.
    pub fn new(name: &str) -> Result<Self, InvalidName> {
        validate(name)?;
        Ok(Principal(name.to_string()))
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Principal {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for Principal {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for Principal {
    type Error = InvalidName;
    fn try_from(name: &str) -> Result<Self, InvalidName> {
        Principal::new(name)
    }
}

impl TryFrom<String> for Principal {
    type Error = InvalidName;
    fn try_from(name: String) -> Result<Self, InvalidName> {
        validate(&name)?;
        Ok(Principal(name))
    }
}

/// The name of a tool as a grant refers to it — the MCP `tools/call`
/// `name`, byte for byte.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolName(String);

impl ToolName {
    /// Validates and wraps a tool name.
    pub fn new(name: &str) -> Result<Self, InvalidName> {
        validate(name)?;
        Ok(ToolName(name.to_string()))
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ToolName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for ToolName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for ToolName {
    type Error = InvalidName;
    fn try_from(name: &str) -> Result<Self, InvalidName> {
        ToolName::new(name)
    }
}

impl TryFrom<String> for ToolName {
    type Error = InvalidName;
    fn try_from(name: String) -> Result<Self, InvalidName> {
        validate(&name)?;
        Ok(ToolName(name))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn accepts_ordinary_names() {
        for name in [
            "invoice-bot",
            "read_file",
            "fs.read",
            "a b",
            "é",
            "server/tool",
            "~",
            "\u{20}x\u{7e}",
        ] {
            assert!(Principal::new(name).is_ok(), "{name:?}");
            assert!(ToolName::new(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(Principal::new(""), Err(InvalidName::Empty));
        assert_eq!(ToolName::new(""), Err(InvalidName::Empty));
        assert_eq!(ToolName::try_from(String::new()), Err(InvalidName::Empty));
    }

    #[test]
    fn rejects_control_characters() {
        for name in ["a\nb", "\t", "x\u{7f}", "\u{0}", "line\r", "a\u{1f}b"] {
            assert_eq!(
                Principal::new(name),
                Err(InvalidName::ControlCharacter),
                "{name:?}"
            );
            assert_eq!(
                ToolName::new(name),
                Err(InvalidName::ControlCharacter),
                "{name:?}"
            );
        }
    }

    #[test]
    fn display_and_borrow() {
        let tool = ToolName::new("read_file").unwrap();
        assert_eq!(tool.to_string(), "read_file");
        assert_eq!(tool.as_str(), "read_file");
        let set: BTreeSet<ToolName> = [tool].into_iter().collect();
        assert!(set.contains("read_file"));
        assert!(!set.contains("write_file"));
        assert_eq!(InvalidName::Empty.to_string(), "name is empty");

        let bot = Principal::try_from("bot").unwrap();
        assert_eq!(bot, Principal::try_from(String::from("bot")).unwrap());
        assert_eq!(bot.to_string(), "bot");
        assert_eq!(bot.as_ref(), "bot");
        let principals: BTreeSet<Principal> = [bot].into_iter().collect();
        assert!(principals.contains("bot"));
        assert!(!principals.contains("other"));
        assert_eq!(ToolName::try_from("t").unwrap().as_str(), "t");
        assert_eq!(
            Principal::try_from(String::from("a\u{1}")),
            Err(InvalidName::ControlCharacter)
        );
    }
}
