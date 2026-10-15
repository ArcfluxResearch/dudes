//! What a string from a repository must be before the Floor may show it.
//!
//! Every string the daemon reads out of an agent-written repository is untrusted display text. A
//! string holding a control character, a format character or a line separator is dropped whole and
//! counted: the Floor never sees it, so a bidirectional override or a newline cannot reorder or
//! forge what an operator reads. A string over its byte cap is cut on a character boundary.

use dudes_proto::{GitSection, MAX_GIT_NAME_BYTES, MAX_GIT_PATH_BYTES, MAX_GIT_SUBJECT_BYTES};
use serde::Serialize;

/// The mark a cut display string ends with.
const ELLIPSIS: char = '\u{2026}';

/// Unicode general category Cf (format), as of Unicode 16: the bidirectional controls and joiners
/// among them. Kept as ranges because the standard library has no category table and a dependency
/// for one predicate would enter a supply chain `cargo deny` gates.
const FORMAT_CHARS: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
];

pub(super) fn is_format(c: char) -> bool {
    let code = u32::from(c);
    FORMAT_CHARS
        .iter()
        .any(|&(low, high)| (low..=high).contains(&code))
}

/// Whether `c` may not appear in a string the Floor shows.
fn forbidden(c: char) -> bool {
    c.is_control() || c == '\u{2028}' || c == '\u{2029}' || is_format(c)
}

/// `text` if it is showable, else `None`.
fn showable(text: &str) -> Option<&str> {
    (!text.chars().any(forbidden)).then_some(text)
}

/// The longest prefix of `text` of at most `max` bytes, ending on a character boundary.
fn prefix(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A name or subject: showable and within `max` bytes, with an ellipsis where it was cut.
fn display(bytes: &[u8], max: usize) -> Option<String> {
    let text = showable(std::str::from_utf8(bytes).ok()?)?;
    if text.len() <= max {
        return Some(text.to_owned());
    }
    let mut cut = prefix(text, max - ELLIPSIS.len_utf8()).to_owned();
    cut.push(ELLIPSIS);
    Some(cut)
}

/// A branch name, or `None` when it must be dropped.
pub(super) fn name(bytes: &[u8]) -> Option<String> {
    display(bytes, MAX_GIT_NAME_BYTES)
}

/// A commit subject, or `None` when it must be dropped.
pub(super) fn subject(bytes: &[u8]) -> Option<String> {
    display(bytes, MAX_GIT_SUBJECT_BYTES)
}

/// A repository-relative path and whether it was cut, or `None` when it must be dropped.
///
/// A path that is absolute, or that holds a `..` or empty component, is not repository-relative and
/// is dropped, so the reply never says where the checkout is.
pub(super) fn path(bytes: &[u8]) -> Option<(String, bool)> {
    let text = showable(std::str::from_utf8(bytes).ok()?)?;
    if text.starts_with('/') || text.split('/').any(|part| part.is_empty() || part == "..") {
        return None;
    }
    let shown = prefix(text, MAX_GIT_PATH_BYTES);
    Some((shown.to_owned(), shown.len() < text.len()))
}

/// What a section's envelope costs when it is empty: `{"items":[],"truncated":false,"dropped":0}`
/// with room for the longest counter.
const ENVELOPE_BYTES: usize = 64;

/// Fill a section from `entries`, where `None` is an entry dropped for its text.
///
/// Stops at `max_items` entries or when the next entry would take the serialized section past
/// `budget` bytes, and says so in `truncated`. A cost is the entry's own JSON plus its comma, so
/// the budget bounds the real reply and not an estimate of it.
pub(super) fn fill<T: Serialize>(
    entries: impl IntoIterator<Item = Option<T>>,
    max_items: usize,
    budget: usize,
) -> GitSection<T> {
    let mut section = GitSection::default();
    let mut spent = ENVELOPE_BYTES;
    for entry in entries {
        let Some(item) = entry else {
            section.dropped = section.dropped.saturating_add(1);
            continue;
        };
        let cost = serde_json::to_vec(&item).map_or(usize::MAX, |json| json.len() + 1);
        if section.items.len() == max_items || spent.saturating_add(cost) > budget {
            section.truncated = true;
            break;
        }
        spent += cost;
        section.items.push(item);
    }
    section
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_formats_and_separators_drop_a_string() {
        for text in [
            "a\nb",
            "tab\there",
            "nul\0x",
            "esc\u{1b}[0m",
            "c1\u{85}",
            "bidi\u{202e}txt.exe",
            "isolate\u{2066}x",
            "joiner\u{200d}x",
            "bom\u{feff}",
            "tag\u{e0041}",
            "ls\u{2028}x",
            "ps\u{2029}x",
        ] {
            assert_eq!(name(text.as_bytes()), None, "{text:?}");
        }
        assert_eq!(
            name("feature/日本語-ok".as_bytes()).unwrap(),
            "feature/日本語-ok"
        );
    }

    #[test]
    fn invalid_utf8_drops_a_string() {
        assert_eq!(subject(&[b'a', 0xff, b'b']), None);
    }

    #[test]
    fn a_long_name_is_cut_on_a_character_boundary_and_marked() {
        let long = "é".repeat(80);
        let shown = name(long.as_bytes()).unwrap();
        assert!(shown.len() <= MAX_GIT_NAME_BYTES, "{}", shown.len());
        assert!(shown.ends_with(ELLIPSIS));
        assert!(shown.starts_with("éé"));
        let exact = "x".repeat(MAX_GIT_NAME_BYTES);
        assert_eq!(
            name(exact.as_bytes()).unwrap(),
            exact,
            "the cap itself is not a cut"
        );
    }

    #[test]
    fn a_path_is_relative_and_reports_its_cut() {
        assert_eq!(
            path(b"src/main.rs"),
            Some(("src/main.rs".to_owned(), false))
        );
        for hostile in [
            b"/etc/passwd".as_slice(),
            b"../x",
            b"a/../b",
            b"a//b",
            b"",
            b"a/",
        ] {
            assert_eq!(path(hostile), None, "{hostile:?}");
        }
        let long = format!("d/{}", "é".repeat(150));
        let (shown, cut) = path(long.as_bytes()).unwrap();
        assert!(cut && shown.len() <= MAX_GIT_PATH_BYTES);
        assert!(long.starts_with(&shown));
    }

    #[test]
    fn a_section_stops_at_its_byte_budget_and_counts_what_it_dropped() {
        let entries = (0..100).map(|n| Some(format!("entry-{n:03}")));
        let section = fill(entries, 1000, 64 + 5 * 13);
        assert_eq!(section.items.len(), 5);
        assert!(section.truncated);
        let wire = serde_json::to_vec(&section).unwrap();
        assert!(wire.len() <= 64 + 5 * 13, "{}", wire.len());

        let section = fill(vec![Some("a"), None, Some("b"), None], 10, 1000);
        assert_eq!(
            (section.items.len(), section.dropped, section.truncated),
            (2, 2, false)
        );

        let section = fill((0..9).map(Some), 3, 1000);
        assert_eq!((section.items, section.truncated), (vec![0, 1, 2], true));
    }
}
