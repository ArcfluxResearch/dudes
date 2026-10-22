//! `cargo xtask naming` — public text carries no ticket ids, decision-record numbers or private
//! paths.
//!
//! The command reads every file under [`ROOTS`] line by line and fails on four shapes:
//!
//! - a ticket-shaped id: the three letters `DUD` followed by digits, with an optional `-` or `_`
//!   between the letters and the digits;
//! - a decision-record cite: `ADR` followed by a number, with an optional `-`, `_` or space;
//! - a private doc path: `docs/` followed by `adr/`, `research/`, `decisions/` or `plan/`;
//! - a home-directory path: `/Users/<name>/` or `/home/<name>/`, unless `<name>` is one of
//!   [`SYNTHETIC_HOMES`].
//!
//! Matching is case-insensitive for ids and cites, and a match must start at a word boundary, so
//! `adrift` and a longer word that merely ends in those letters are not hits.
//!
//! # Why the home-path exemption is a short list of names
//!
//! Tests need believable absolute paths, and a path that never names a user cannot exercise
//! home-relative logic. The exemption is an exact list of obviously made-up names rather than a
//! pattern such as "short names", because a pattern lets a real account name through as soon as it
//! happens to fit. Placeholders (`<name>`, `$HOME`, `{home}`) and dot-directories
//! (`/home/.claude/`) are not user names and never match. Adding a name to the list is a visible,
//! reviewed change.
//!
//! # This file scans itself
//!
//! `xtask/` is under [`ROOTS`], so this file is checked like every other. Its needles are built at
//! run time with `format!`, and its documentation describes the shapes in words, so nothing in it
//! matches. [`tests::this_file_does_not_trip_its_own_scan`] pins that.
//!
//! # Controls
//!
//! A passing run over a clean tree proves nothing unless the same code can be shown to fail on a
//! dirty one. [`tests::the_scan_catches_every_planted_form`] plants each shape in a temporary tree
//! and asserts [`scan`], the function the command runs, reports it. [`verdict`] refuses a corpus
//! smaller than [`MIN_FILES`], and [`scan`] refuses a root that is missing or empty, so a walk
//! that stops seeing files fails instead of reading as clean.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::workspace::Workspace;

/// What is read, relative to the workspace root. A directory is walked to any depth; a plain file
/// is read as it stands. Every entry must exist and must yield at least one file — see [`scan`].
const ROOTS: &[&str] = &[
    "crates",
    "bins",
    "xtask",
    "schemas",
    "examples",
    "floor",
    ".github",
    "docs",
    "README.md",
];

/// The floor under the number of files the walk must see.
///
/// It sits below the current corpus (the command prints the exact count on every run) so ordinary
/// churn does not touch it, and close enough that a whole root vanishing does. The sharper
/// protection is per-root and lives in [`scan`]: a root that is missing or empty fails there by
/// name, whatever the total. What neither reaches is a top-level directory that is not in
/// [`ROOTS`] at all.
const MIN_FILES: usize = 300;

/// User names that appear in made-up absolute paths in tests and fixtures.
const SYNTHETIC_HOMES: &[&str] = &["op", "operator", "someone", "x"];

/// The doc directories that are not part of the public tree.
const PRIVATE_DOC_DIRS: &[&str] = &["adr", "research", "decisions", "plan"];

/// How much of a hit line is quoted in a failure.
const EXCERPT: usize = 100;

/// Which shape a [`Finding`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    TicketId,
    DecisionCite,
    PrivateDocPath,
    HomePath,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::TicketId => "ticket-shaped id",
            Kind::DecisionCite => "decision-record cite",
            Kind::PrivateDocPath => "private doc path",
            Kind::HomePath => "home-directory path",
        }
    }
}

/// One line carrying one barred shape.
#[derive(Debug)]
struct Finding {
    path: PathBuf,
    line: usize,
    kind: Kind,
    excerpt: String,
}

/// What one run read, and what it found.
#[derive(Debug)]
struct Scan {
    /// Files read per corpus root, in [`ROOTS`] order. Kept per root so a root that has stopped
    /// yielding files is visible in the transcript, not just in a sum.
    per_root: Vec<(&'static str, usize)>,
    findings: Vec<Finding>,
}

impl Scan {
    fn files(&self) -> usize {
        self.per_root.iter().map(|(_, count)| count).sum()
    }

    fn per_root_summary(&self) -> String {
        self.per_root
            .iter()
            .map(|(root, count)| format!("{root}={count}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn starts_with_ignore_case(bytes: &[u8], prefix: &str) -> bool {
    bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// Whether `bytes[at..]` begins a three-letter `word`, at a word boundary, followed by an optional
/// separator from `separators` and then a digit.
fn keyword_then_number(bytes: &[u8], at: usize, word: &str, separators: &[u8]) -> bool {
    if at > 0 && bytes[at - 1].is_ascii_alphanumeric() {
        return false;
    }
    if !starts_with_ignore_case(&bytes[at..], word) {
        return false;
    }
    let mut next = at + word.len();
    if bytes.get(next).is_some_and(|b| separators.contains(b)) {
        next += 1;
    }
    bytes.get(next).is_some_and(u8::is_ascii_digit)
}

/// The home-directory path starting at `bytes[at..]`, if it names a user outside
/// [`SYNTHETIC_HOMES`].
fn real_home_at(bytes: &[u8], at: usize) -> bool {
    for marker in ["/Users/", "/home/"] {
        if !bytes[at..].starts_with(marker.as_bytes()) {
            continue;
        }
        let name_start = at + marker.len();
        let name_len = bytes[name_start..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            .count();
        let name = &bytes[name_start..name_start + name_len];
        let closed = bytes.get(name_start + name_len) == Some(&b'/');
        let is_name = name.first().is_some_and(u8::is_ascii_alphanumeric);
        return closed
            && is_name
            && !SYNTHETIC_HOMES
                .iter()
                .any(|synthetic| name.eq_ignore_ascii_case(synthetic.as_bytes()));
    }
    false
}

/// Every barred shape on one line.
fn line_kinds(line: &str) -> Vec<Kind> {
    let bytes = line.as_bytes();
    let mut kinds = Vec::new();
    let mut add = |kind: Kind| {
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    };
    for at in 0..bytes.len() {
        if keyword_then_number(bytes, at, "dud", b"-_") {
            add(Kind::TicketId);
        }
        if keyword_then_number(bytes, at, "adr", b"-_ ") {
            add(Kind::DecisionCite);
        }
        if real_home_at(bytes, at) {
            add(Kind::HomePath);
        }
    }
    for dir in PRIVATE_DOC_DIRS {
        if line.contains(&format!("docs/{dir}/")) {
            add(Kind::PrivateDocPath);
        }
    }
    kinds
}

/// Every file under `path`, at any depth, sorted so a transcript is reproducible.
///
/// A symlink is pushed as a file rather than followed as a directory: this walk has no loop
/// detection, and a symlink that resolves to a directory then fails at the read, which is
/// fail-closed.
fn files_under(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let file_type = fs::symlink_metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .file_type();
    if !file_type.is_dir() {
        out.push(path.to_owned());
        return Ok(());
    }
    if path.ends_with("floor/node_modules") || path.ends_with("floor/dist") {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        children.push(
            entry
                .with_context(|| format!("reading {}", path.display()))?
                .path(),
        );
    }
    children.sort();
    for child in &children {
        files_under(child, out)?;
    }
    Ok(())
}

/// Read every corpus root under `root` and report every barred shape.
///
/// A root that is missing, or that holds no files, fails here rather than reading as clean: an
/// absence measured over nothing is not evidence of absence.
fn scan(root: &Path) -> Result<Scan> {
    let mut per_root = Vec::with_capacity(ROOTS.len());
    let mut findings = Vec::new();
    for entry in ROOTS {
        let path = root.join(entry);
        let mut files = Vec::new();
        files_under(&path, &mut files).with_context(|| {
            format!(
                "corpus root {} could not be walked; it must exist, or this check reads nothing there",
                path.display()
            )
        })?;
        if files.is_empty() {
            bail!(
                "corpus root {} holds no files; a scan of nothing reads as clean, which is the failure this refuses",
                path.display()
            );
        }
        for file in &files {
            let bytes = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
            for (index, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                for kind in line_kinds(line) {
                    findings.push(Finding {
                        path: file.clone(),
                        line: index + 1,
                        kind,
                        excerpt: line.trim().chars().take(EXCERPT).collect(),
                    });
                }
            }
        }
        per_root.push((*entry, files.len()));
    }
    Ok(Scan { per_root, findings })
}

/// Turn a [`Scan`] into a pass or a failure that names every file, line and shape.
///
/// `min_files` is a parameter so the controls can exercise both arms; [`run`] passes
/// [`MIN_FILES`]. The floor is checked first: a scan that read almost nothing has not earned the
/// right to report zero findings.
fn verdict(scan: &Scan, min_files: usize) -> Result<()> {
    let files = scan.files();
    if files < min_files {
        bail!(
            "read only {files} files, below the floor of {min_files} ({}); a corpus this small \
             cannot support a claim that nothing was found — has a directory moved or been \
             renamed?",
            scan.per_root_summary()
        );
    }
    if scan.findings.is_empty() {
        return Ok(());
    }
    let mut report = String::new();
    for finding in &scan.findings {
        let _ = write!(
            report,
            "\n  {}:{}: {}: {}",
            finding.path.display(),
            finding.line,
            finding.kind.label(),
            finding.excerpt
        );
    }
    bail!(
        "{} line(s) carry ids, decision-record numbers or private paths that public text must not:{report}",
        scan.findings.len()
    )
}

/// `cargo xtask naming`.
pub(crate) fn run(workspace: &Workspace) -> Result<()> {
    let scan = scan(workspace.root())?;
    println!(
        "naming: read {} files ({})",
        scan.files(),
        scan.per_root_summary()
    );
    verdict(&scan, MIN_FILES)?;
    println!("naming: ok");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask/ has a parent")
            .to_owned()
    }

    /// A tree with every corpus root present and nothing barred in it, so [`scan`] can walk it.
    /// A root that is in `ROOTS` and missing here makes every test using it fail by name.
    fn clean_corpus(dir: &Path) {
        for (path, text) in [
            ("crates/keep/src/lib.rs", "//! nothing to see\n"),
            ("bins/duded/src/main.rs", "fn main() {}\n"),
            ("xtask/src/main.rs", "fn main() {}\n"),
            ("schemas/thing.schema.json", "{}\n"),
            ("examples/demo/README.md", "# a demo\n"),
            ("floor/package.json", "{}\n"),
            (".github/workflows/ci.yml", "name: ci\n"),
            ("docs/page.md", "# a page\n"),
            ("README.md", "# a repository\n"),
        ] {
            let path = dir.join(path);
            fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
            fs::write(&path, text).expect("write");
        }
    }

    fn plant(dir: &Path, file: &str, text: &str) {
        fs::write(dir.join("crates").join(file), text).expect("write");
    }

    /// Each barred shape, built at run time so this file carries none of them.
    fn planted_forms() -> Vec<(&'static str, Kind, String)> {
        let ticket = "dud".to_ascii_uppercase();
        let adr = "adr".to_ascii_uppercase();
        let user = "pat";
        let mut forms = vec![
            (
                "hyphen.rs",
                Kind::TicketId,
                format!("// see {ticket}-{}", 123),
            ),
            (
                "glued.rs",
                Kind::TicketId,
                format!("fn {}{}_x() {{}}", "dud", 123),
            ),
            (
                "under.rs",
                Kind::TicketId,
                format!("let {}_{} = 1;", "dud", 45),
            ),
            (
                "trail.rs",
                Kind::TicketId,
                format!("const {ticket}{}_X: u8 = 1;", 9),
            ),
            (
                "cite.rs",
                Kind::DecisionCite,
                format!("// per {adr}-{:04}", 7),
            ),
            (
                "cite_space.rs",
                Kind::DecisionCite,
                format!("// per {adr} {}", 12),
            ),
            (
                "users.rs",
                Kind::HomePath,
                format!("let p = \"/Users/{user}/src\";"),
            ),
            (
                "home.rs",
                Kind::HomePath,
                format!("let p = \"/home/{user}/src\";"),
            ),
        ];
        for dir in PRIVATE_DOC_DIRS {
            forms.push((
                "private.rs",
                Kind::PrivateDocPath,
                format!("// see docs/{dir}/file.md"),
            ));
        }
        forms
    }

    /// The command passes by finding nothing, which proves nothing unless the same function can be
    /// shown to find something. Every shape is planted and every shape must come back.
    #[test]
    fn the_scan_catches_every_planted_form() {
        let tmp = tempfile::tempdir().expect("tempdir");
        clean_corpus(tmp.path());
        let before = scan(tmp.path()).expect("scan a clean tree");
        assert!(before.findings.is_empty(), "{:?}", before.findings);

        for (index, (file, _, text)) in planted_forms().iter().enumerate() {
            plant(tmp.path(), &format!("{index}_{file}"), &format!("{text}\n"));
        }
        let after = scan(tmp.path()).expect("scan the planted tree");
        let forms = planted_forms();
        assert_eq!(after.findings.len(), forms.len(), "{:?}", after.findings);
        for (index, (file, kind, _)) in forms.iter().enumerate() {
            let wanted = format!("{index}_{file}");
            let found = after
                .findings
                .iter()
                .find(|finding| finding.path.ends_with(&wanted))
                .unwrap_or_else(|| panic!("`{wanted}` was planted and not reported"));
            assert_eq!(found.kind, *kind, "{wanted}");
            assert_eq!(found.line, 1, "{wanted}");
        }
        let err = verdict(&after, 1).expect_err("a planted tree must fail the check");
        assert!(
            err.to_string().contains("0_hyphen.rs:1"),
            "the failure must name the file and line: {err}"
        );
    }

    #[test]
    fn ordinary_text_and_synthetic_paths_are_not_hits() {
        for innocent in [
            "adrift on a sea of records",
            "a dudes daemon, dud and duds",
            "overdud1 is not an id",
            "version 1.2.3 and abc123",
            "docs/configuration.md and docs/chat.md",
            "/home/operator/.codex and /home/op/venv.sh and /home/x/.claude",
            "/Users/someone/Library",
            "/tmp/home/.claude/plugins and /home/.claude/x",
            "/home/<name>/ and /Users/{user}/ and $HOME/src",
            "/home/pat",
            "the adr tool",
        ] {
            assert!(
                line_kinds(innocent).is_empty(),
                "false positive: {innocent}"
            );
        }
    }

    #[test]
    fn a_real_looking_user_is_a_hit_but_a_synthetic_one_is_not() {
        let real = format!("/{}/{}/x", "home", "jordan");
        let synthetic = format!("/{}/{}/x", "home", "operator");
        assert_eq!(line_kinds(&real), vec![Kind::HomePath]);
        assert!(line_kinds(&synthetic).is_empty());
    }

    #[test]
    fn a_clean_tree_passes_and_an_empty_root_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        clean_corpus(tmp.path());
        for generated in ["floor/node_modules/dependency.js", "floor/dist/index.js"] {
            let path = tmp.path().join(generated);
            fs::create_dir_all(path.parent().expect("generated parent")).expect("create generated");
            fs::write(path, format!("// {}-{}\n", "DUD", 1)).expect("write generated");
        }

        let scan = scan(tmp.path()).expect("scan a clean tree");
        assert!(scan.findings.is_empty(), "{:?}", scan.findings);
        assert_eq!(scan.files(), ROOTS.len(), "{:?}", scan.per_root);
        verdict(&scan, ROOTS.len()).expect("clean, and at the floor");

        let err = verdict(&scan, ROOTS.len() + 1).expect_err("a corpus below the floor must fail");
        assert!(err.to_string().contains("floor"), "{err}");

        fs::remove_dir_all(tmp.path().join("schemas")).expect("remove");
        let err = super::scan(tmp.path()).expect_err("a missing root must be refused");
        assert!(err.to_string().contains("schemas"), "{err}");

        fs::create_dir_all(tmp.path().join("schemas")).expect("create");
        let err = super::scan(tmp.path()).expect_err("an empty root must be refused");
        assert!(err.to_string().contains("schemas"), "{err}");
    }

    #[test]
    fn this_file_does_not_trip_its_own_scan() {
        let path = workspace_root().join("xtask").join("src").join("naming.rs");
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
        assert!(
            text.contains("planted_forms"),
            "the read must find this file"
        );
        for (number, line) in text.lines().enumerate() {
            assert!(
                line_kinds(line).is_empty(),
                "{}:{} trips the scan: {line}",
                path.display(),
                number + 1
            );
        }
    }
}
