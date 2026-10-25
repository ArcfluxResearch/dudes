//! Admission of new agent display names and roles. Historical journal strings remain replayable.

use alloc::string::{String, ToString};

/// Most UTF-8 bytes in an admitted display name.
pub const MAX_DISPLAY_NAME_BYTES: usize = 128;

/// Most UTF-8 bytes in an admitted display role.
pub const MAX_DISPLAY_ROLE_BYTES: usize = 512;

/// `value` without surrounding ASCII space, tab, carriage return and line feed, when that holds 1 to
/// `max` UTF-8 bytes and no control character, which a terminal printing it would obey.
fn admitted(value: &str, max: usize) -> Option<&str> {
    let text = value.trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'));
    ((1..=max).contains(&text.len()) && !text.chars().any(char::is_control)).then_some(text)
}

/// A canonical display name admitted for a new hire or rename.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayName(String);

/// A name outside the admitted byte bounds, or one holding a control character.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "display name must contain 1 to 128 UTF-8 bytes after trimming ASCII space, and no control character"
)]
pub struct InvalidDisplayName;

impl DisplayName {
    /// Trim ASCII space, tab, carriage return and line feed, and admit 1 to 128 UTF-8 bytes with no
    /// control character.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidDisplayName`] for an empty or overlong name, or one that still holds a
    /// control character after trimming.
    pub fn parse(value: &str) -> Result<Self, InvalidDisplayName> {
        admitted(value, MAX_DISPLAY_NAME_BYTES)
            .map(|name| Self(name.to_string()))
            .ok_or(InvalidDisplayName)
    }

    /// Consume the admitted name.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

/// A canonical role admitted for a browser hire or a look edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayRole(String);

/// A role outside the admitted byte bounds, or one holding a control character.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "role must contain 1 to 512 UTF-8 bytes after trimming ASCII space, and no control character"
)]
pub struct InvalidDisplayRole;

impl DisplayRole {
    /// Trim ASCII space, tab, carriage return and line feed, and admit 1 to 512 UTF-8 bytes with no
    /// control character.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidDisplayRole`] for an empty or overlong role, or one that still holds a
    /// control character after trimming.
    pub fn parse(value: &str) -> Result<Self, InvalidDisplayRole> {
        admitted(value, MAX_DISPLAY_ROLE_BYTES)
            .map(|role| Self(role.to_string()))
            .ok_or(InvalidDisplayRole)
    }

    /// Consume the admitted role.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Most UTF-8 bytes of a role as an identity line shows it, ellipsis included.
pub const MAX_IDENTITY_ROLE_BYTES: usize = 160;

/// What an identity line shows for a name that cleans to nothing.
pub const UNNAMED: &str = "(unnamed)";

/// What an identity line shows for a role that cleans to nothing.
pub const NO_ROLE: &str = "(none)";

/// Whether `c` is an invisible formatting character that changes how neighbouring text reads or
/// hides text from a person: direction controls, zero-width characters, word joiners, tag
/// characters and the like (Unicode category Cf).
const fn is_format(c: char) -> bool {
    matches!(c,
        '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}' | '\u{6dd}' | '\u{70f}'
        | '\u{890}'..='\u{891}' | '\u{8e2}' | '\u{180e}' | '\u{200b}'..='\u{200f}'
        | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}'
        | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}'
        | '\u{13430}'..='\u{1343f}' | '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}'
        | '\u{e0001}' | '\u{e0020}'..='\u{e007f}')
}

/// A label made safe to sit inside one bracketed sentence of a task.
///
/// Control and format characters are dropped. Tabs, line breaks and the Unicode line and paragraph
/// separators become a space, as do square brackets, which would end or open the sentence. Runs of
/// spaces collapse and the ends are trimmed. A result longer than `max` bytes is cut on a character
/// boundary and ends in an ellipsis that fits inside `max`. Nothing is rewritten in the journal:
/// the same function runs again wherever a label is shown.
fn clean_label(value: &str, max: usize, empty: &str) -> String {
    let mut cleaned = String::with_capacity(value.len().min(max + 4));
    let mut gap = false;
    for c in value.chars() {
        let c = match c {
            '\t' | '\n' | '\r' | '\u{2028}' | '\u{2029}' | '[' | ']' | ' ' => ' ',
            c if c.is_control() || is_format(c) => continue,
            c => c,
        };
        if c == ' ' {
            gap = !cleaned.is_empty();
        } else {
            if gap {
                cleaned.push(' ');
                gap = false;
            }
            cleaned.push(c);
        }
    }
    if cleaned.len() > max {
        let mut end = max - '…'.len_utf8();
        while !cleaned.is_char_boundary(end) {
            end -= 1;
        }
        cleaned.truncate(end);
        let kept = cleaned.trim_end_matches(' ').len();
        cleaned.truncate(kept);
        cleaned.push('…');
    }
    if cleaned.is_empty() {
        cleaned.push_str(empty);
    }
    cleaned
}

/// A display name as an identity line shows it: cleaned, at most 128 bytes, `(unnamed)` when empty.
#[must_use]
pub fn identity_name(name: &str) -> String {
    clean_label(name, MAX_DISPLAY_NAME_BYTES, UNNAMED)
}

/// A display role as an identity line shows it: cleaned, at most 160 bytes, `(none)` when empty.
#[must_use]
pub fn identity_role(role: &str) -> String {
    clean_label(role, MAX_IDENTITY_ROLE_BYTES, NO_ROLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_trims_only_the_four_ascii_scalars_and_counts_utf8_bytes() {
        assert_eq!(
            DisplayName::parse(" \tKim\r\n").unwrap().into_string(),
            "Kim"
        );
        assert_eq!(
            DisplayName::parse("\u{a0}Kim\u{a0}").unwrap().into_string(),
            "\u{a0}Kim\u{a0}"
        );
        assert!(DisplayName::parse(" \t\r\n").is_err());
        assert!(DisplayName::parse(&"é".repeat(64)).is_ok());
        assert!(DisplayName::parse(&"é".repeat(65)).is_err());
        assert!(DisplayName::parse(&"x".repeat(129)).is_err());
    }

    #[test]
    fn admission_refuses_each_control_character_left_after_trimming() {
        for name in [
            "Kim\u{1b}]52;c;aGk=\u{7}",
            "Kim\u{1b}[2J",
            "Kim\nLeo",
            "Kim\u{7f}",
            "Kim\u{9b}1m",
            "\u{0}",
        ] {
            assert_eq!(
                DisplayName::parse(name),
                Err(InvalidDisplayName),
                "{name:?}"
            );
        }
        assert_eq!(
            DisplayName::parse("\u{202e}Kim\u{2066}")
                .unwrap()
                .into_string(),
            "\u{202e}Kim\u{2066}"
        );
    }

    #[test]
    fn role_admission_trims_counts_512_bytes_and_refuses_control_characters() {
        assert_eq!(
            DisplayRole::parse(" \tengi\u{202e}neer\r\n")
                .unwrap()
                .into_string(),
            "engi\u{202e}neer"
        );
        assert!(DisplayRole::parse(&"é".repeat(256)).is_ok());
        assert_eq!(
            DisplayRole::parse(&"é".repeat(257)),
            Err(InvalidDisplayRole)
        );
        for role in [
            " \t\r\n",
            "engi\u{1b}[2Jneer",
            "engi\nneer",
            "engi\u{9b}1mneer",
        ] {
            assert_eq!(
                DisplayRole::parse(role),
                Err(InvalidDisplayRole),
                "{role:?}"
            );
        }
    }

    #[test]
    fn identity_labels_drop_hostile_characters_and_keep_one_plain_line() {
        for (raw, shown) in [
            ("Alex", "Alex"),
            ("  Alex \t Kim\r\nLee  ", "Alex Kim Lee"),
            ("Al[ex]", "Al ex"),
            ("a\u{2028}b\u{2029}c", "a b c"),
            ("\u{202e}Alex\u{2066}", "Alex"),
            ("Al\u{200b}ex\u{feff}", "Alex"),
            ("Al\u{e0041}ex", "Alex"),
            ("Al\u{1b}[2Jex", "Al 2Jex"),
            ("\u{7f}\u{9b}", "(unnamed)"),
            ("", "(unnamed)"),
            (" [ ] ", "(unnamed)"),
        ] {
            assert_eq!(identity_name(raw), shown, "{raw:?}");
            assert_eq!(identity_name(shown), shown, "fixed point {raw:?}");
        }
        assert_eq!(identity_role(""), "(none)");
        assert_eq!(identity_role("\u{200b}"), "(none)");
    }

    #[test]
    fn identity_roles_are_cut_inside_160_bytes_on_a_character_boundary() {
        let long = "r".repeat(512);
        let cut = identity_role(&long);
        assert_eq!(cut.len(), 160);
        assert_eq!(cut, format!("{}…", "r".repeat(157)));
        let wide = identity_role(&"é".repeat(256));
        assert_eq!(wide, format!("{}…", "é".repeat(78)));
        assert!(wide.len() <= 160);
        let spaced = identity_role(&format!("{} {}", "r".repeat(156), "s".repeat(200)));
        assert_eq!(spaced, format!("{}…", "r".repeat(156)));
        for shown in [cut, wide, spaced] {
            assert_eq!(identity_role(&shown), shown);
        }
        assert_eq!(identity_role(&"r".repeat(160)), "r".repeat(160));
        assert_eq!(identity_name(&"n".repeat(128)), "n".repeat(128));
    }
}
