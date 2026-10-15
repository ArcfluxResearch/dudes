//! Parsers for the fixed commands and the validators for the files the shadow
//! gitdir copies. Pure functions over bytes: no I/O.
//!
//! Output that was cut at the stdout cap is parsed as far as it is whole. The caller says so with
//! `cut`, and the last record, which may be partial, is left out.

use dudes_proto::{GIT_ID_DIGITS, GitChangeKind, MAX_GIT_PARENTS};
use dudes_types::Timestamp;

/// A 40-digit lowercase hexadecimal object id.
pub(super) fn is_object_id(bytes: &[u8]) -> bool {
    bytes.len() == 40 && bytes.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The id's first [`GIT_ID_DIGITS`] digits, for display.
pub(super) fn short(id: &[u8]) -> Option<String> {
    is_object_id(id).then(|| String::from_utf8_lossy(&id[..GIT_ID_DIGITS]).into_owned())
}

/// Whether `name` is a ref name `git check-ref-format` accepts and this read is willing to copy.
///
/// Stricter than git in one way: valid UTF-8 only, because every name that reaches the Floor is
/// shown as text.
pub(super) fn is_legal_ref_name(name: &str) -> bool {
    if name.is_empty()
        || name == "@"
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
    {
        return false;
    }
    if name.chars().any(|c| {
        c.is_control() || c == ' ' || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    }) {
        return false;
    }
    name.split('/').all(|part| {
        !part.starts_with('.') && !part.starts_with('-') && !part.as_bytes().ends_with(b".lock")
    })
}

/// What `HEAD` says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Head {
    /// `ref: refs/heads/<name>`.
    Branch(String),
    /// A 40-digit id.
    Detached(Vec<u8>),
}

/// `HEAD` as the shadow accepts it, or `None` for anything else.
pub(super) fn head(bytes: &[u8]) -> Option<Head> {
    let line = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if is_object_id(line) {
        return Some(Head::Detached(line.to_vec()));
    }
    let name = std::str::from_utf8(line.strip_prefix(b"ref: refs/heads/")?).ok()?;
    is_legal_ref_name(name).then(|| Head::Branch(name.to_owned()))
}

/// Whether `bytes` is a loose ref the shadow may copy: an id, or a symbolic ref to a branch or to
/// a branch of `origin`.
pub(super) fn is_loose_ref(bytes: &[u8]) -> bool {
    let line = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if is_object_id(line) {
        return true;
    }
    let Some(target) = line.strip_prefix(b"ref: ") else {
        return false;
    };
    let Ok(target) = std::str::from_utf8(target) else {
        return false;
    };
    target
        .strip_prefix("refs/heads/")
        .or_else(|| target.strip_prefix("refs/remotes/origin/"))
        .is_some_and(is_legal_ref_name)
}

/// Why an index was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IndexRefusal {
    /// Version 4 compresses path prefixes, which this read does not decode.
    Version4,
    /// Not an index this read can walk, or an entry name that leaves the checkout.
    Unsafe,
}

const INDEX_ENTRY_FIXED: usize = 62;
const INDEX_EXTENDED_FLAG: u16 = 0x4000;

/// Walk the entry names of a version 2 or 3 index and refuse one that names a path outside the
/// checkout: a `..` or empty component, a leading `/`, or a `.git` component.
///
/// Git compares each entry with the file at that path in the work tree, so an index an agent
/// wrote can make `status` open any file the daemon can reach (measured with `../outside.txt`).
pub(super) fn check_index(bytes: &[u8]) -> Result<(), IndexRefusal> {
    index_summary(bytes).map(|_| ())
}

#[derive(Default)]
pub(super) struct IndexSummary {
    pub(super) has_gitlinks: bool,
    pub(super) has_hidden_entries: bool,
    pub(super) names: std::collections::BTreeSet<Vec<u8>>,
}

pub(super) fn index_summary(bytes: &[u8]) -> Result<IndexSummary, IndexRefusal> {
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    if bytes.get(..4) != Some(b"DIRC") {
        return Err(IndexRefusal::Unsafe);
    }
    match word(4) {
        Some(2 | 3) => {}
        Some(4) => return Err(IndexRefusal::Version4),
        _ => return Err(IndexRefusal::Unsafe),
    }
    let count = word(8).ok_or(IndexRefusal::Unsafe)?;
    let mut at = 12;
    let mut summary = IndexSummary::default();
    for _ in 0..count {
        summary.has_gitlinks |= word(at + 24).ok_or(IndexRefusal::Unsafe)? & 0o170_000 == 0o160_000;
        let flags = bytes
            .get(at + 60..at + 62)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .ok_or(IndexRefusal::Unsafe)?;
        let extended = flags & INDEX_EXTENDED_FLAG != 0;
        let extended_flags = if extended {
            u16::from_be_bytes(
                bytes
                    .get(at + 62..at + 64)
                    .and_then(|bytes| bytes.try_into().ok())
                    .ok_or(IndexRefusal::Unsafe)?,
            )
        } else {
            0
        };
        summary.has_hidden_entries |= flags & 0x8000 != 0 || extended_flags & 0x4000 != 0;
        let start = at + INDEX_ENTRY_FIXED + if extended { 2 } else { 0 };
        let rest = bytes.get(start..).ok_or(IndexRefusal::Unsafe)?;
        let length = rest
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(IndexRefusal::Unsafe)?;
        let name = &rest[..length];
        if name.starts_with(b"/")
            || name
                .split(|byte| *byte == b'/')
                .any(|part| part.is_empty() || part == b".." || part.eq_ignore_ascii_case(b".git"))
        {
            return Err(IndexRefusal::Unsafe);
        }
        summary.names.insert(name.to_vec());
        // The entry, name and padding are a multiple of eight bytes, with at least one NUL.
        at += (start - at + length + 8) & !7;
    }
    Ok(summary)
}

/// Whether `bytes` is a `shallow` file: lines of 40-digit ids.
pub(super) fn is_shallow(bytes: &[u8]) -> bool {
    bytes
        .split(|b| *b == b'\n')
        .enumerate()
        .all(|(n, line)| is_object_id(line) || (line.is_empty() && n > 0))
}

/// The fields of `lines` split at the end of every line the output holds whole.
fn whole_lines(output: &[u8], cut: bool) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = output.split(|b| *b == b'\n').collect();
    // The text after the last newline is empty for complete output and partial for cut output.
    let last = lines.pop().unwrap_or_default();
    if !cut && !last.is_empty() {
        lines.push(last);
    }
    lines
}

/// Local branches from `for-each-ref --format=%(objectname)%00%(refname) refs/heads`: the name and
/// the 40-digit tip.
pub(super) fn branches(output: &[u8], cut: bool) -> Vec<(Vec<u8>, Vec<u8>)> {
    whole_lines(output, cut)
        .into_iter()
        .filter_map(|line| {
            let (id, refname) = {
                let at = line.iter().position(|b| *b == 0)?;
                (&line[..at], &line[at + 1..])
            };
            let name = refname.strip_prefix(b"refs/heads/")?;
            is_object_id(id).then(|| (name.to_vec(), id.to_vec()))
        })
        .collect()
}

/// `(behind, ahead)` from `rev-list --left-right --count <base>...HEAD`.
pub(super) fn counts(output: &[u8]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(output).ok()?;
    let mut fields = text.split_whitespace();
    let behind = fields.next()?.parse().ok()?;
    let ahead = fields.next()?.parse().ok()?;
    fields.next().is_none().then_some((behind, ahead))
}

/// One commit as `log -z --format=%H%x09%P%x09%ct%x09%s` writes it, before its text is checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RawCommit {
    pub(super) id: String,
    pub(super) parents: Vec<String>,
    pub(super) at: Timestamp,
    pub(super) subject: Vec<u8>,
}

/// The commits of a `log -z` run. A record that is not four fields with a full id is skipped.
pub(super) fn commits(output: &[u8], cut: bool) -> Vec<RawCommit> {
    let mut records: Vec<&[u8]> = output.split(|b| *b == 0).collect();
    let last = records.pop().unwrap_or_default();
    if !cut && !last.is_empty() {
        records.push(last);
    }
    records
        .into_iter()
        .filter_map(|record| {
            let mut fields = record.splitn(4, |b| *b == b'\t');
            let id = short(fields.next()?)?;
            let parents = fields
                .next()?
                .split(|b| *b == b' ')
                .filter(|part| !part.is_empty())
                .take(MAX_GIT_PARENTS)
                .map(short)
                .collect::<Option<Vec<_>>>()?;
            let seconds: i64 = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
            Some(RawCommit {
                id,
                parents,
                at: Timestamp::from_unix_millis(seconds.saturating_mul(1000)),
                subject: fields.next()?.to_vec(),
            })
        })
        .collect()
}

fn kind_of(letter: u8) -> Option<GitChangeKind> {
    Some(match letter {
        b'A' => GitChangeKind::Added,
        b'M' => GitChangeKind::Modified,
        b'D' => GitChangeKind::Deleted,
        b'R' | b'C' => GitChangeKind::Renamed,
        b'T' => GitChangeKind::TypeChanged,
        b'U' => GitChangeKind::Conflicted,
        _ => return None,
    })
}

/// The NUL-separated fields of `output`, without a partial last field when it was cut.
fn fields(output: &[u8], cut: bool) -> Vec<&[u8]> {
    let mut fields: Vec<&[u8]> = output.split(|b| *b == 0).collect();
    let last = fields.pop().unwrap_or_default();
    if !cut && !last.is_empty() {
        fields.push(last);
    }
    fields
}

/// `(kind, path)` pairs from `diff-tree -r --raw -z --no-renames`: a `:`-led metadata field whose
/// last letter is the kind, then the path.
pub(super) fn committed(output: &[u8], cut: bool) -> Vec<(GitChangeKind, Vec<u8>)> {
    let fields = fields(output, cut);
    let mut found = Vec::new();
    let mut at = 0;
    while at + 1 < fields.len() {
        let kind = Some(fields[at])
            .filter(|meta| meta.starts_with(b":"))
            .and_then(|meta| meta.last().copied())
            .and_then(kind_of);
        if let Some(kind) = kind {
            found.push((kind, fields[at + 1].to_vec()));
            at += 2;
        } else {
            at += 1;
        }
    }
    found
}

/// One entry of `status --porcelain=v1 -z`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum StatusEntry {
    /// `??`.
    Untracked,
    /// A tracked path that differs, and whether the difference is staged.
    Changed {
        kind: GitChangeKind,
        staged: bool,
        path: Vec<u8>,
    },
}

/// The entries of `status --porcelain=v1 -z`. Ignored paths (`!!`) are skipped.
pub(super) fn status(output: &[u8], cut: bool) -> Vec<StatusEntry> {
    let fields = fields(output, cut);
    let mut entries = Vec::new();
    let mut at = 0;
    while let Some(field) = fields.get(at) {
        at += 1;
        let [x, y, b' ', path @ ..] = *field else {
            continue;
        };
        match (*x, *y) {
            (b'?', b'?') => entries.push(StatusEntry::Untracked),
            (b'!', b'!') => {}
            (x, y) => {
                if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
                    at += 1;
                }
                let conflicted =
                    x == b'U' || y == b'U' || (x, y) == (b'A', b'A') || (x, y) == (b'D', b'D');
                let letter = if conflicted {
                    b'U'
                } else if x == b' ' {
                    y
                } else {
                    x
                };
                if let Some(kind) = kind_of(letter) {
                    entries.push(StatusEntry::Changed {
                        kind,
                        staged: x != b' ',
                        path: path.to_vec(),
                    });
                }
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn head_is_a_legal_branch_or_a_full_id_and_nothing_else() {
        assert_eq!(
            head(b"ref: refs/heads/main\n"),
            Some(Head::Branch("main".into()))
        );
        assert_eq!(
            head(b"ref: refs/heads/feat/x-1"),
            Some(Head::Branch("feat/x-1".into()))
        );
        assert_eq!(
            head(A.as_bytes()),
            Some(Head::Detached(A.as_bytes().to_vec()))
        );
        for hostile in [
            "ref: refs/heads/../../x",
            "ref: refs/heads/a b",
            "ref: refs/heads/",
            "ref: refs/remotes/origin/main",
            "ref: HEAD",
            "ref: refs/heads/x.lock",
            "ref: refs/heads/.hidden",
            "ref: refs/heads/a..b",
            "ref: refs/heads/-flag",
            "ref: /etc/passwd",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "../../other/.git",
            "",
        ] {
            assert_eq!(head(hostile.as_bytes()), None, "{hostile:?}");
        }
        assert_eq!(head(&[b'r', b'e', b'f', b':', b' ', 0xff]), None);
    }

    #[test]
    fn a_loose_ref_is_an_id_or_a_symbolic_ref_to_a_branch() {
        assert!(is_loose_ref(format!("{A}\n").as_bytes()));
        assert!(is_loose_ref(b"ref: refs/remotes/origin/main\n"));
        assert!(is_loose_ref(b"ref: refs/heads/main\n"));
        assert!(!is_loose_ref(b"ref: refs/remotes/upstream/main\n"));
        assert!(!is_loose_ref(b"ref: ../../x\n"));
        assert!(!is_loose_ref(b"not a ref"));
    }

    fn entry(name: &str, extended: bool) -> Vec<u8> {
        let mut bytes = vec![0_u8; INDEX_ENTRY_FIXED];
        let flags =
            u16::try_from(name.len()).unwrap() | if extended { INDEX_EXTENDED_FLAG } else { 0 };
        bytes[60..62].copy_from_slice(&flags.to_be_bytes());
        if extended {
            bytes.extend_from_slice(&[0, 0]);
        }
        bytes.extend_from_slice(name.as_bytes());
        let padded = (bytes.len() + 8) & !7;
        bytes.resize(padded, 0);
        bytes
    }

    fn index(version: u32, entries: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = b"DIRC".to_vec();
        bytes.extend_from_slice(&version.to_be_bytes());
        bytes.extend_from_slice(&u32::try_from(entries.len()).unwrap().to_be_bytes());
        for each in entries {
            bytes.extend_from_slice(each);
        }
        bytes.extend_from_slice(&[0; 20]);
        bytes
    }

    #[test]
    fn an_index_is_walked_and_a_name_that_leaves_the_checkout_is_refused() {
        let fine = [
            entry("src/main.rs", false),
            entry("dir/a b", true),
            entry("x", false),
        ];
        assert_eq!(check_index(&index(2, &fine)), Ok(()));
        assert_eq!(check_index(&index(3, &fine)), Ok(()));
        assert_eq!(check_index(&index(2, &[])), Ok(()));
        for name in [
            "../outside.txt",
            "a/../b",
            "/etc/passwd",
            "a//b",
            ".git/config",
            "x/.GIT/hooks/p",
            "a/.git",
        ] {
            assert_eq!(
                check_index(&index(2, &[entry("ok", false), entry(name, false)])),
                Err(IndexRefusal::Unsafe),
                "{name}"
            );
        }
        assert_eq!(check_index(&index(4, &fine)), Err(IndexRefusal::Version4));
        assert_eq!(check_index(&index(9, &fine)), Err(IndexRefusal::Unsafe));
        assert_eq!(check_index(b"nope"), Err(IndexRefusal::Unsafe));
        let whole = index(2, &fine);
        assert_eq!(
            check_index(&whole[..whole.len() - 30]),
            Err(IndexRefusal::Unsafe),
            "truncated"
        );
    }

    #[test]
    fn shallow_is_lines_of_ids() {
        assert!(is_shallow(format!("{A}\n{B}\n").as_bytes()));
        assert!(is_shallow(A.as_bytes()));
        assert!(!is_shallow(format!("{A}\n../x\n").as_bytes()));
        assert!(!is_shallow(b"\n"));
    }

    #[test]
    fn branches_parse_and_a_cut_drops_the_partial_line() {
        let out = format!("{A}\0refs/heads/main\n{B}\0refs/heads/feat/x\n{A}\0refs/he");
        let all = branches(out.as_bytes(), true);
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].0, b"feat/x");
        let whole = format!("{A}\0refs/heads/main\n{B}\0refs/heads/feat/x\n");
        assert_eq!(branches(whole.as_bytes(), false).len(), 2);
        let unterminated = format!("{A}\0refs/heads/main");
        assert_eq!(
            branches(unterminated.as_bytes(), false).len(),
            1,
            "an uncut run keeps its last line"
        );
        assert_eq!(
            branches(unterminated.as_bytes(), true).len(),
            0,
            "a cut run does not"
        );
    }

    #[test]
    fn counts_are_behind_then_ahead() {
        assert_eq!(counts(b"3\t7\n"), Some((3, 7)));
        assert_eq!(counts(b"x\t7\n"), None);
        assert_eq!(counts(b"1\t2\t3\n"), None);
    }

    #[test]
    fn commits_split_on_nul_and_tab_and_keep_a_tabbed_subject_whole() {
        let out = format!("{A}\t{B} {B}\t1700000000\tsubject\twith tab\0{B}\t\t5\tsecond\0{A}\t\t");
        let found = commits(out.as_bytes(), true);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].id, "aaaaaaaaaaaa");
        assert_eq!(found[0].parents, ["bbbbbbbbbbbb", "bbbbbbbbbbbb"]);
        assert_eq!(found[0].at, Timestamp::from_unix_millis(1_700_000_000_000));
        assert_eq!(found[0].subject, b"subject\twith tab");
        assert!(found[1].parents.is_empty());
    }

    #[test]
    fn diff_tree_pairs_a_kind_with_a_path() {
        let out = b":100644 100644 aaa bbb M\0src/a.rs\0:000000 100644 000 ccc A\0new file.txt\0:100644 000000 ddd 000 D\0gone\0:100644 120000 eee fff T\0link\0";
        let found = committed(out, false);
        assert_eq!(
            found,
            vec![
                (GitChangeKind::Modified, b"src/a.rs".to_vec()),
                (GitChangeKind::Added, b"new file.txt".to_vec()),
                (GitChangeKind::Deleted, b"gone".to_vec()),
                (GitChangeKind::TypeChanged, b"link".to_vec()),
            ]
        );
        let cut = committed(&out[..out.len() - 3], true);
        assert_eq!(cut.len(), 3, "a path cut mid-name is not reported");
    }

    #[test]
    fn status_counts_untracked_and_lists_only_tracked_changes() {
        let out = b" M a.rs\0M  b.rs\0MM both.rs\0A  new.rs\0 D gone.rs\0R  moved.rs\0old.rs\0UU merge.rs\0?? u1\0?? u2\0!! ignored\0";
        let entries = status(out, false);
        assert_eq!(
            entries
                .iter()
                .filter(|e| matches!(e, StatusEntry::Untracked))
                .count(),
            2
        );
        let changed: Vec<_> = entries
            .iter()
            .filter_map(|e| match e {
                StatusEntry::Changed { kind, staged, path } => {
                    Some((*kind, *staged, String::from_utf8(path.clone()).unwrap()))
                }
                StatusEntry::Untracked => None,
            })
            .collect();
        assert_eq!(
            changed,
            vec![
                (GitChangeKind::Modified, false, "a.rs".to_owned()),
                (GitChangeKind::Modified, true, "b.rs".to_owned()),
                (GitChangeKind::Modified, true, "both.rs".to_owned()),
                (GitChangeKind::Added, true, "new.rs".to_owned()),
                (GitChangeKind::Deleted, false, "gone.rs".to_owned()),
                (GitChangeKind::Renamed, true, "moved.rs".to_owned()),
                (GitChangeKind::Conflicted, true, "merge.rs".to_owned()),
            ],
            "the old name of a rename is not a path of its own"
        );
    }

    #[test]
    fn a_cut_status_leaves_out_its_last_whole_looking_field() {
        let out = b" M a.rs\0?? u1\0?? u";
        assert_eq!(status(out, true).len(), 2);
        assert_eq!(status(out, false).len(), 3);
    }
}
