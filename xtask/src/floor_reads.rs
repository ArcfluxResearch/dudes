//! `cargo xtask floor-reads` — the lists that decide which CI jobs a change needs stay complete.
//!
//! A pull request that touches only `floor/` skips the Rust jobs, and one that touches only Rust
//! skips the Floor job. That is safe only while two lists in `.github/ci/` name every file that
//! crosses the line:
//!
//! - [`TO_FLOOR`]: the Floor files that Rust source points at. A change to one runs the Rust jobs.
//! - [`FROM_FLOOR`]: the Rust-side files that Floor source points at. A change to one runs the
//!   Floor job.
//!
//! The command reads the quoted strings in each side's source and reports a path into the other
//! side that its list does not cover, with the file and line. A reference is a quoted string that
//! begins with the other side's directory name, or reaches it through `../`, and whose next
//! component is a real entry of that directory (so a URL such as the daemon's `/floor/session`
//! and an install layout such as `floor/assets` are not repository paths). `floor/dist` is a
//! build output and counts as an entry although a clean checkout has none.
//!
//! It is a heuristic over string literals: a path assembled from separate segments, such as
//! `.join("floor").join("src")`, is not seen. A scan that finds no reference at all fails, because
//! both sides hold some and a scan that sees nothing proves nothing.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::naming::files_under;
use crate::workspace::Workspace;

/// One direction of the crossing and the list that must cover it.
struct Direction {
    label: &'static str,
    /// Where the references are read, relative to the workspace root.
    scanned: &'static [&'static str],
    /// The directories a reference may point into.
    targets: &'static [&'static str],
    /// The covering list, relative to the workspace root.
    list: &'static str,
}

const TO_FLOOR: Direction = Direction {
    label: "Rust to Floor",
    scanned: &["crates", "bins", "xtask"],
    targets: &["floor"],
    list: ".github/ci/floor-files-rust-reads.txt",
};

const FROM_FLOOR: Direction = Direction {
    label: "Floor to Rust",
    scanned: &["floor"],
    targets: &["crates", "bins", "xtask", "schemas", "docs", "examples"],
    list: ".github/ci/rust-files-floor-reads.txt",
};

/// Source files that hold string literals worth reading.
const EXTENSIONS: &[&str] = &["rs", "mjs", "js", "ts", "tsx", "html"];

/// Build outputs that are real paths although a clean checkout lacks them.
const BUILD_OUTPUTS: &[&str] = &["dist"];

/// This file names its own examples in tests.
const SELF_FILE: &str = "xtask/src/floor_reads.rs";

struct Reference {
    file: PathBuf,
    line: usize,
    path: String,
}

/// `cargo xtask floor-reads`.
pub(crate) fn run(workspace: &Workspace) -> Result<()> {
    let mut problems = Vec::new();
    for direction in [&TO_FLOOR, &FROM_FLOOR] {
        problems.extend(check(workspace.root(), direction)?);
    }
    if problems.is_empty() {
        println!("floor-reads: ok");
        return Ok(());
    }
    bail!(
        "the CI path lists are out of date; add each path to its list:\n  {}",
        problems.join("\n  ")
    )
}

/// Print one direction's tally and return what it found wrong.
fn check(root: &Path, direction: &Direction) -> Result<Vec<String>> {
    let patterns = patterns_in(&root.join(direction.list))?;
    let references = references_under(root, direction)?;
    if references.is_empty() {
        bail!(
            "{}: found no references; the scan has stopped seeing the code",
            direction.label
        );
    }
    println!(
        "floor-reads: {}: {} references checked against {}",
        direction.label,
        references.len(),
        direction.list
    );
    let mut problems = Vec::new();
    for reference in &references {
        if !covered(&patterns, &reference.path) {
            problems.push(format!(
                "{}:{}: `{}` is not covered by {}",
                reference.file.display(),
                reference.line,
                reference.path,
                direction.list
            ));
        }
    }
    for pattern in &patterns {
        if let Some(missing) = stale(root, pattern) {
            problems.push(format!("{}: `{missing}` no longer exists", direction.list));
        }
    }
    Ok(problems)
}

/// The glob lines of a list: blank lines and `#` lines are skipped.
fn patterns_in(list: &Path) -> Result<Vec<String>> {
    let text = fs::read_to_string(list).with_context(|| format!("reading {}", list.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect())
}

/// The fixed part of a pattern that names something now gone, or `None`.
fn stale(root: &Path, pattern: &str) -> Option<String> {
    let fixed = match pattern.find('*') {
        None => pattern,
        Some(at) if pattern[..at].ends_with('/') => pattern[..at].trim_end_matches('/'),
        Some(_) => return None,
    };
    let built = fixed
        .rsplit('/')
        .next()
        .is_some_and(|name| BUILD_OUTPUTS.contains(&name));
    (!built && !root.join(fixed).exists()).then(|| fixed.to_owned())
}

/// Whether the list covers `path`. A directory is covered when its contents are.
fn covered(patterns: &[String], path: &str) -> bool {
    let inside = format!("{path}/x");
    patterns
        .iter()
        .any(|pattern| glob(pattern, path) || glob(pattern, &inside))
}

/// Shell-style matching with `*` standing for any run of characters, `/` included, which is how
/// `case` in the workflow's script reads the same lines.
fn glob(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, tail)) => text.strip_prefix(head).is_some_and(|rest| {
            (0..=rest.len())
                .filter(|&at| rest.is_char_boundary(at))
                .any(|at| glob(tail, &rest[at..]))
        }),
    }
}

fn references_under(root: &Path, direction: &Direction) -> Result<Vec<Reference>> {
    let mut found = Vec::new();
    for dir in direction.scanned {
        let mut files = Vec::new();
        files_under(&root.join(dir), &mut files)?;
        for file in files {
            let relative = file.strip_prefix(root).unwrap_or(&file);
            let supported = file
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| EXTENSIONS.contains(&ext));
            if !supported || relative.ends_with(SELF_FILE) {
                continue;
            }
            let text =
                fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let rust = file.extension().is_some_and(|ext| ext == "rs");
            for (index, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") || trimmed.starts_with('*') {
                    continue;
                }
                for path in paths_in(line, root, direction.targets, rust) {
                    found.push(Reference {
                        file: relative.to_owned(),
                        line: index + 1,
                        path,
                    });
                }
            }
        }
    }
    Ok(found)
}

/// The repository paths named by the quoted strings of one line.
fn paths_in(line: &str, root: &Path, targets: &[&str], rust: bool) -> Vec<String> {
    let mut paths = Vec::new();
    for literal in literals(line, rust) {
        for target in targets {
            let needle = format!("{target}/");
            let mut from = 0;
            while let Some(found) = literal[from..].find(&needle) {
                let at = from + found;
                from = at + needle.len();
                if at != 0 && !literal[..at].ends_with("../") {
                    continue;
                }
                let path = literal[at..]
                    .split(|c: char| c.is_whitespace() || "\"'`)?#{}$".contains(c))
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches(['/', '.', ',']);
                let next = path
                    .get(needle.len()..)
                    .and_then(|rest| rest.split('/').next())
                    .unwrap_or_default();
                let real = !next.is_empty()
                    && (BUILD_OUTPUTS.contains(&next) || root.join(target).join(next).exists());
                if real {
                    paths.push(path.to_owned());
                }
            }
        }
    }
    paths
}

/// The contents of the quoted strings on a line. Rust has only double quotes; the other
/// languages here also quote with single quotes and backticks.
fn literals(line: &str, rust: bool) -> Vec<&str> {
    let quotes: &[char] = if rust { &['"'] } else { &['"', '\'', '`'] };
    let mut found = Vec::new();
    let mut open: Option<(char, usize)> = None;
    let mut escaped = false;
    for (index, c) in line.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = open.is_some();
        } else if let Some((quote, start)) = open {
            if c == quote {
                found.push(&line[start..index]);
                open = None;
            }
        } else if quotes.contains(&c) {
            open = Some((c, index + c.len_utf8()));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temporary directory");
        for (path, text) in files {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
            fs::write(path, text).expect("write");
        }
        dir
    }

    #[test]
    fn a_quoted_path_into_the_floor_is_a_reference_and_a_url_is_not() {
        let dir = tree(&[("floor/src/a.ts", ""), ("floor/package.json", "")]);
        let seen = |line| paths_in(line, dir.path(), &["floor"], true);
        assert_eq!(
            seen(r#"include_str!("../../../floor/src/a.ts")"#),
            ["floor/src/a.ts"]
        );
        assert_eq!(
            seen(r#"root.join("floor/package.json")"#),
            ["floor/package.json"]
        );
        assert_eq!(seen(r#"concat!(dir, "/../../floor/src")"#), ["floor/src"]);
        assert!(seen(r#"let path = "/floor/src/a.ts";"#).is_empty());
        assert!(seen(r#"let url = "POST /floor/session";"#).is_empty());
        assert!(seen(r#"let installed = "floor/assets/app.js";"#).is_empty());
        assert!(seen(r#"let sentence = "built with floor/src in mind";"#).is_empty());
    }

    #[test]
    fn a_list_covers_exact_paths_and_directories() {
        let list = [
            "floor/src/a.ts".to_owned(),
            "floor/tests/fixtures/*".to_owned(),
        ];
        assert!(covered(&list, "floor/src/a.ts"));
        assert!(covered(&list, "floor/tests/fixtures/x.json"));
        assert!(covered(&list, "floor/tests/fixtures"));
        assert!(!covered(&list, "floor/src/b.ts"));
        assert!(!covered(&list, "floor/tests"));
    }

    #[test]
    fn the_scan_names_the_uncovered_reference_and_passes_once_it_is_listed() {
        let dir = tree(&[
            ("floor/src/a.ts", ""),
            ("floor/src/b.ts", ""),
            (
                "crates/c/src/lib.rs",
                "const A: &str = include_str!(\"../../../floor/src/a.ts\");\n\
                 const B: &str = include_str!(\"../../../floor/src/b.ts\");\n",
            ),
            (".github/ci/list.txt", "floor/src/a.ts\n"),
        ]);
        let direction = Direction {
            label: "test",
            scanned: &["crates"],
            targets: &["floor"],
            list: ".github/ci/list.txt",
        };
        let list = dir.path().join(direction.list);
        let problems = check(dir.path(), &direction).expect("the scan runs");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("lib.rs:2") && problems[0].contains("floor/src/b.ts"));

        fs::write(&list, "floor/src/a.ts\nfloor/src/b.ts\n").expect("write");
        assert!(
            check(dir.path(), &direction)
                .expect("the scan runs")
                .is_empty()
        );

        fs::write(&list, "floor/src/a.ts\nfloor/src/b.ts\nfloor/gone.ts\n").expect("write");
        let problems = check(dir.path(), &direction).expect("the scan runs");
        assert!(problems[0].contains("floor/gone.ts"), "{problems:?}");
    }
}
