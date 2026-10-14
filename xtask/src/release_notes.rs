//! `cargo xtask release-notes` — the body of a GitHub Release, built from the repository's own
//! sources.
//!
//! Two modes print markdown on stdout:
//!
//! - `--draft --from <tag>` lists the squash-merged pull requests since `<tag>` as a draft
//!   `CHANGELOG.md` section. `feat` and `perf` land under Changes, `fix` under Fixes and any `!`
//!   breaking change under Breaking. Other types stay out of the draft and appear only in the
//!   contribution record. A maintainer edits the draft into the released section before tagging.
//! - `--version <v> [--from <tag>] --repo <owner/name> --run-url <url> --sums <file>
//!   --maintainer <login> [--authors <file>] [--preview]` prints the full release body: the
//!   `CHANGELOG.md` section for `<v>` (the editorial part), the install and upgrade text, the
//!   contribution record and the verification block. A release with no section for `<v>`, or an
//!   empty one, fails, so no release goes out without notes. `--preview` falls back to the
//!   Unreleased section so a body can be rendered before the section is cut.
//!
//! Everything that needs the network (author logins, the run URL) arrives as an argument or a file,
//! so [`render_body`] and [`render_draft`] are pure functions from parsed commits and inputs to
//! markdown. Only [`run`] touches git and the file system.
//!
//! The `CHANGELOG.md` section may use the headings Highlights, Changes (or Added, Changed), Fixes
//! (or Fixed) and Breaking. Any other heading is kept as it is after Fixes.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::workspace::Workspace;

const USAGE: &str = "usage: release-notes --draft --from <tag>\n       \
    release-notes --version <v> [--from <tag>] --repo <owner/name> --run-url <url> \
    --sums <file> --maintainer <login> [--authors <file>] [--preview]";

/// What `release-notes` was asked for.
#[derive(Debug, PartialEq)]
pub(crate) enum Mode {
    Draft {
        from: String,
    },
    Body {
        version: String,
        from: Option<String>,
        repo: String,
        run_url: String,
        sums: PathBuf,
        maintainer: String,
        authors: Option<PathBuf>,
        preview: bool,
    },
}

/// Read the command's arguments. Every flag but `--draft` and `--preview` takes one value.
pub(crate) fn parse(args: &[String]) -> Result<Mode> {
    let mut draft = false;
    let mut preview = false;
    let mut values: BTreeMap<&str, String> = BTreeMap::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--draft" => draft = true,
            "--preview" => preview = true,
            flag @ ("--from" | "--version" | "--repo" | "--run-url" | "--sums" | "--maintainer"
            | "--authors") => {
                let value = rest
                    .next()
                    .with_context(|| format!("`{flag}` needs a value"))?;
                ensure!(!value.is_empty(), "`{flag}` needs a value");
                ensure!(
                    values.insert(flag, value.clone()).is_none(),
                    "`{flag}` was given twice"
                );
            }
            other => bail!("unexpected argument `{other}`\n{USAGE}"),
        }
    }
    let mut take = |flag: &str| values.remove(flag);
    let from = take("--from");
    if draft {
        ensure!(
            values.is_empty() && !preview,
            "--draft takes only --from\n{USAGE}"
        );
        let from = from.with_context(|| format!("--draft needs --from <tag>\n{USAGE}"))?;
        return Ok(Mode::Draft { from });
    }
    let mut require = |flag: &str| {
        take(flag).with_context(|| format!("`{flag}` is required for a release body\n{USAGE}"))
    };
    Ok(Mode::Body {
        version: require("--version")?,
        from,
        repo: require("--repo")?,
        run_url: require("--run-url")?,
        sums: require("--sums")?.into(),
        maintainer: require("--maintainer")?,
        authors: take("--authors").map(PathBuf::from),
        preview,
    })
}

/// One first-parent commit: a squash-merged pull request, or a direct commit with no number.
#[derive(Debug, PartialEq)]
struct Entry {
    /// The Conventional Commit type, empty when the subject does not follow the convention.
    kind: String,
    scope: Option<String>,
    breaking: bool,
    /// The description after `type(scope): `, or the whole title when there is no convention.
    text: String,
    /// The subject without its ` (#n)` suffix.
    title: String,
    pr: Option<u32>,
}

fn parse_subject(subject: &str) -> Entry {
    let subject = subject.trim();
    let (title, pr) = match subject.strip_suffix(')').and_then(|s| s.rsplit_once(" (#")) {
        Some((title, number)) => match number.parse() {
            Ok(pr) => (title.trim_end(), Some(pr)),
            Err(_) => (subject, None),
        },
        None => (subject, None),
    };
    let entry = Entry {
        kind: String::new(),
        scope: None,
        breaking: false,
        text: title.to_owned(),
        title: title.to_owned(),
        pr,
    };
    let Some((head, text)) = title.split_once(": ") else {
        return entry;
    };
    let (head, breaking) = head.strip_suffix('!').map_or((head, false), |h| (h, true));
    let (kind, scope) = match head.split_once('(') {
        Some((kind, scope)) => match scope.strip_suffix(')') {
            Some(scope) if !scope.is_empty() => (kind, Some(scope)),
            _ => return entry,
        },
        None => (head, None),
    };
    if kind.is_empty() || !kind.bytes().all(|b| b.is_ascii_lowercase()) {
        return entry;
    }
    Entry {
        kind: kind.to_owned(),
        scope: scope.map(str::to_owned),
        breaking,
        text: text.to_owned(),
        ..entry
    }
}

/// The draft `CHANGELOG.md` section for these subjects, oldest last as `git log` prints them.
fn render_draft(subjects: &[String]) -> String {
    let mut changes = Vec::new();
    let mut fixes = Vec::new();
    let mut breaking = Vec::new();
    for entry in subjects.iter().map(|s| parse_subject(s)) {
        let bullet = draft_bullet(&entry);
        if entry.breaking {
            breaking.push(bullet);
        } else if matches!(entry.kind.as_str(), "feat" | "perf") {
            changes.push(bullet);
        } else if entry.kind == "fix" {
            fixes.push(bullet);
        }
    }
    let mut out = String::from("## [x.y.z] - YYYY-MM-DD\n");
    for (heading, bullets) in [
        ("Changes", changes),
        ("Fixes", fixes),
        ("Breaking", breaking),
    ] {
        if !bullets.is_empty() {
            let _ = write!(out, "\n### {heading}\n\n{}\n", bullets.join("\n"));
        }
    }
    out
}

fn draft_bullet(entry: &Entry) -> String {
    let scope = entry
        .scope
        .as_ref()
        .map_or_else(String::new, |scope| format!("{scope}: "));
    let pr = entry.pr.map_or_else(String::new, |pr| format!(" (#{pr})"));
    format!("- {scope}{}{pr}", entry.text)
}

/// A released section of `CHANGELOG.md`.
#[derive(Debug, Default, PartialEq)]
struct Section {
    /// The text before the first `###` heading.
    intro: String,
    /// Each `###` heading with its body, in file order.
    blocks: Vec<(String, String)>,
}

impl Section {
    fn is_empty(&self) -> bool {
        self.intro.is_empty() && self.blocks.iter().all(|(_, body)| body.is_empty())
    }

    /// The bodies of the blocks whose heading is one of `names`, joined by a blank line.
    fn take(&mut self, names: &[&str]) -> String {
        let (taken, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.blocks)
            .into_iter()
            .partition(|(heading, _)| names.contains(&heading.as_str()));
        self.blocks = kept;
        let bodies: Vec<_> = taken
            .into_iter()
            .map(|(_, body)| body)
            .filter(|body| !body.is_empty())
            .collect();
        bodies.join("\n\n")
    }
}

/// The section of `changelog` headed `## [<version>]`. A link reference definition such as
/// `[0.79.0]: https://...` ends the section.
fn changelog_section(changelog: &str, version: &str, preview: bool) -> Result<Section> {
    let find = |name: &str| {
        let heading = format!("## [{name}]");
        let mut lines = changelog
            .lines()
            .skip_while(|line| !line.starts_with(&heading));
        lines.next().map(|_| {
            lines
                .take_while(|line| !line.starts_with("## ") && !is_link_definition(line))
                .collect::<Vec<_>>()
        })
    };
    let (lines, name) = match find(version) {
        Some(lines) => (lines, version),
        None if preview => (find("Unreleased").unwrap_or_default(), "Unreleased"),
        None => bail!(
            "CHANGELOG.md has no section `## [{version}]`; a release needs its notes. \
             Run `cargo xtask release-notes --draft --from <previous tag>` for a start"
        ),
    };
    let mut section = Section::default();
    let mut current: Option<(String, Vec<&str>)> = None;
    let mut intro = Vec::new();
    for line in lines {
        if let Some(heading) = line.strip_prefix("### ") {
            if let Some((heading, body)) = current.take() {
                section
                    .blocks
                    .push((heading, body.join("\n").trim().to_owned()));
            }
            current = Some((heading.trim().to_owned(), Vec::new()));
        } else if let Some((_, body)) = current.as_mut() {
            body.push(line);
        } else {
            intro.push(line);
        }
    }
    if let Some((heading, body)) = current {
        section
            .blocks
            .push((heading, body.join("\n").trim().to_owned()));
    }
    intro.join("\n").trim().clone_into(&mut section.intro);
    ensure!(
        preview || !section.is_empty(),
        "the CHANGELOG.md section for {name} is empty; a release needs its notes"
    );
    Ok(section)
}

fn is_link_definition(line: &str) -> bool {
    line.strip_prefix('[')
        .is_some_and(|rest| rest.split_once("]:").is_some())
}

/// What the network and the workflow supply.
#[derive(Debug)]
struct Inputs<'a> {
    version: &'a str,
    from: Option<&'a str>,
    repo: &'a str,
    run_url: &'a str,
    sha: &'a str,
    sums: &'a str,
    maintainer: &'a str,
    /// Pull request number to author login.
    authors: &'a BTreeMap<u32, String>,
    preview: bool,
}

/// A commit subject as plain text: a backslash before each character that markdown, a mention or
/// a cross-reference would act on, and before the slashes of a `://` so no link is made.
fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (at, ch) in text.char_indices() {
        let scheme_slash = ch == '/' && (text[..at].ends_with(':') || text[..at].ends_with(":/"));
        if scheme_slash
            || matches!(
                ch,
                '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '&' | '|' | '~' | '#' | '@'
            )
        {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The full release body.
fn render_body(subjects: &[String], changelog: &str, inputs: &Inputs<'_>) -> Result<String> {
    let Inputs { version, repo, .. } = *inputs;
    let mut section = changelog_section(changelog, version, inputs.preview)?;
    let entries: Vec<Entry> = subjects.iter().map(|s| parse_subject(s)).collect();
    let numbered: Vec<(u32, &Entry)> = entries
        .iter()
        .filter_map(|entry| entry.pr.map(|pr| (pr, entry)))
        .collect();
    let count = plural(numbered.len(), "pull request", "pull requests");

    let mut out = format!("## {version}\n\n");
    match inputs.from {
        Some(from) => {
            let name = from.strip_prefix('v').unwrap_or(from);
            let _ = writeln!(
                out,
                "Changes since [{name}](https://github.com/{repo}/releases/tag/{from}): {count}.\n"
            );
        }
        None => {
            let _ = writeln!(out, "This is the first release: {count}.\n");
        }
    }
    if !section.intro.is_empty() {
        let _ = writeln!(out, "{}\n", section.intro);
    }
    let highlights = section.take(&["Highlights"]);
    let changes = section.take(&["Changes", "Added", "Changed"]);
    let fixes = section.take(&["Fixes", "Fixed"]);
    let breaking = section.take(&["Breaking"]);
    if !highlights.is_empty() {
        let _ = writeln!(out, "### Highlights\n\n{highlights}\n");
    }
    let _ = writeln!(out, "### Install\n\n{}\n", install_text(repo));
    let changes = if changes.is_empty() {
        "No new capabilities in this release."
    } else {
        &changes
    };
    let _ = writeln!(out, "### Changes\n\n{changes}\n");
    let fixes = if fixes.is_empty() {
        "No fixes in this release."
    } else {
        &fixes
    };
    let _ = writeln!(out, "### Fixes\n\n{fixes}\n");
    for (heading, body) in &section.blocks {
        let _ = writeln!(out, "### {heading}\n\n{body}\n");
    }
    let _ = writeln!(
        out,
        "### Upgrading\n\n{}\n",
        upgrade_text(version, repo, &breaking)
    );

    out.push_str("### Complete contribution record\n\n");
    for (pr, entry) in &numbered {
        let thanks = inputs
            .authors
            .get(pr)
            .filter(|login| !login.eq_ignore_ascii_case(inputs.maintainer))
            .map_or_else(String::new, |login| format!(". Thanks @{login}"));
        let _ = writeln!(out, "- #{pr} {}{thanks}", escape_markdown(&entry.title));
    }
    if numbered.is_empty() {
        out.push_str("No pull requests in this release.\n");
    }
    let _ = write!(
        out,
        "\n### Release verification\n\n\
         Release commit: `{sha}`. Built by [this workflow run]({run}).\n\n\
         `SHA256SUMS` for this release:\n\n```\n{sums}\n```\n\n\
         Put the archive for your machine and `SHA256SUMS` in one directory, then run \
         `sha256sum --check --ignore-missing SHA256SUMS`. On macOS, `shasum -a 256 --check SHA256SUMS` \
         works the same way but reports the archives you did not download as missing.\n\n\
         Each archive and `SHA256SUMS` also carry a build provenance attestation. \
         `gh attestation verify <archive> --repo {repo}` checks that this repository's release \
         workflow built the file.\n",
        sha = inputs.sha,
        run = inputs.run_url,
        sums = inputs.sums.trim_end(),
    );
    Ok(out)
}

fn install_text(repo: &str) -> String {
    format!(
        "macOS and Linux, on x86_64 and arm64:\n\n\
         ```sh\n\
         curl -fsSL https://raw.githubusercontent.com/{repo}/main/install.sh | sh\n\
         ~/.local/bin/dudes quickstart\n\
         ```\n\n\
         The script picks the archive for your machine, checks it against `SHA256SUMS`, and installs \
         `duded`, `dudes`, `dudes-hook` and the Floor into `~/.local`. It never uses `sudo` and never \
         edits your shell profile. On Linux it installs the static build, which runs on any \
         distribution. You also need at least one of the `claude`, `codex` or `pi` CLIs, signed in. \
         Windows builds from source only for now; see \
         [Platforms and known limits](https://github.com/{repo}/blob/main/docs/platforms-and-limits.md)."
    )
}

fn upgrade_text(version: &str, repo: &str, breaking: &str) -> String {
    let breaking = if breaking.is_empty() {
        "No breaking changes are listed for this release.".to_owned()
    } else {
        format!("Breaking changes:\n\n{breaking}")
    };
    format!(
        "To install this exact version, run the installer again:\n\n\
         ```sh\n\
         curl -fsSL https://raw.githubusercontent.com/{repo}/main/install.sh | sh -s -- --version {version}\n\
         ```\n\n\
         A running daemon keeps using the old binary until it stops. Run `dudes stop`, then \
         `dudes start`, to start the new one.\n\n{breaking}"
    )
}

/// `<number> <login>` per line, as the workflow writes them.
fn parse_authors(text: &str) -> Result<BTreeMap<u32, String>> {
    let mut authors = BTreeMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let (number, login) = line
            .split_once(' ')
            .with_context(|| format!("author line `{line}` is not `<number> <login>`"))?;
        let number = number
            .parse()
            .with_context(|| format!("author line `{line}` has no pull request number"))?;
        authors.insert(number, login.trim().to_owned());
    }
    Ok(authors)
}

/// The first-parent subjects in `<from>..HEAD`, or all of them when there is no `from`.
fn first_parent_subjects(workspace: &Workspace, from: Option<&str>) -> Result<Vec<String>> {
    let range = match from {
        Some(from) => {
            ensure!(!from.starts_with('-'), "`{from}` is not a tag");
            format!("{from}..HEAD")
        }
        None => "HEAD".to_owned(),
    };
    let log = git(workspace, &["log", "--first-parent", "--format=%s", &range])?;
    Ok(log.lines().map(str::to_owned).collect())
}

fn git(workspace: &Workspace, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace.root())
        .output()
        .context("starting git")?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).context("git printed non-UTF-8 text")
}

pub(crate) fn run(workspace: &Workspace, mode: Mode) -> Result<()> {
    match mode {
        Mode::Draft { from } => {
            let subjects = first_parent_subjects(workspace, Some(&from))?;
            print!("{}", render_draft(&subjects));
        }
        Mode::Body {
            version,
            from,
            repo,
            run_url,
            sums,
            maintainer,
            authors,
            preview,
        } => {
            let read = |path: &PathBuf| {
                fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
            };
            let changelog = read(&workspace.root().join("CHANGELOG.md"))?;
            let sums = read(&sums)?;
            let authors = match authors {
                Some(path) => parse_authors(&read(&path)?)?,
                None => BTreeMap::new(),
            };
            let subjects = first_parent_subjects(workspace, from.as_deref())?;
            let sha = git(workspace, &["rev-parse", "HEAD"])?;
            let inputs = Inputs {
                version: &version,
                from: from.as_deref(),
                repo: &repo,
                run_url: &run_url,
                sha: sha.trim(),
                sums: &sums,
                maintainer: &maintainer,
                authors: &authors,
                preview,
            };
            print!("{}", render_body(&subjects, &changelog, &inputs)?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn subjects(lines: &[&str]) -> Vec<String> {
        words(lines)
    }

    const CHANGELOG: &str = "\
# Changelog

## [Unreleased]

## [0.2.0] - 2026-11-01

Early preview.

### Highlights

- **Faster starts.** The daemon starts in half the time (#7).

### Added

- A new `dudes thing` command (#7).

### Fixed

- A crash on empty input (#9).

### Breaking

- `dudes old` is now `dudes new` (#7).

## [0.1.0] - 2026-10-01

### Added

- The first release.

[Unreleased]: https://example.test/compare/v0.2.0...HEAD
[0.2.0]: https://example.test/compare/v0.1.0...v0.2.0
";

    #[test]
    fn subjects_parse_into_type_scope_breaking_and_pull_request() {
        assert_eq!(
            parse_subject("feat(floor)!: drop the old panel (#12)"),
            Entry {
                kind: "feat".into(),
                scope: Some("floor".into()),
                breaking: true,
                text: "drop the old panel".into(),
                title: "feat(floor)!: drop the old panel".into(),
                pr: Some(12),
            }
        );
        let plain = parse_subject("first public release of Dudes");
        assert_eq!((plain.kind.as_str(), plain.pr), ("", None));
        assert_eq!(plain.text, "first public release of Dudes");
        let odd = parse_subject("Fix: not lower case (#x)");
        assert_eq!((odd.kind.as_str(), odd.pr), ("", None));
    }

    #[test]
    fn the_draft_groups_by_type_and_leaves_the_rest_to_the_record() {
        let draft = render_draft(&subjects(&[
            "fix(git): run git without helper threads (#6)",
            "feat(floor): tidy the header (#3)",
            "perf: faster replay (#5)",
            "chore: bump to 0.3.0 (#4)",
            "docs: add forms (#1)",
            "refactor(rpc)!: rename the ping method (#8)",
            "feat: first release",
        ]));
        assert_eq!(
            draft,
            "\
## [x.y.z] - YYYY-MM-DD

### Changes

- floor: tidy the header (#3)
- faster replay (#5)
- first release

### Fixes

- git: run git without helper threads (#6)

### Breaking

- rpc: rename the ping method (#8)
"
        );
    }

    #[test]
    fn a_missing_or_empty_section_is_refused() {
        let missing = changelog_section(CHANGELOG, "0.3.0", false).expect_err("no section");
        assert!(
            format!("{missing:#}").contains("no section `## [0.3.0]`"),
            "{missing:#}"
        );
        let empty = changelog_section(CHANGELOG, "Unreleased", false).expect_err("empty");
        assert!(format!("{empty:#}").contains("is empty"), "{empty:#}");
        assert_eq!(
            changelog_section(CHANGELOG, "0.3.0", true).expect("preview"),
            Section::default()
        );
    }

    #[test]
    fn a_section_ends_before_the_next_one_and_before_the_link_definitions() {
        let section = changelog_section(CHANGELOG, "0.1.0", false).expect("a section");
        assert_eq!(
            section,
            Section {
                intro: String::new(),
                blocks: vec![("Added".into(), "- The first release.".into())],
            }
        );
    }

    #[test]
    fn the_body_has_every_part_in_order() {
        let authors = BTreeMap::from([
            (7, "maintainer".to_owned()),
            (9, "outsider".to_owned()),
            (10, "Maintainer".to_owned()),
        ]);
        let inputs = Inputs {
            version: "0.2.0",
            from: Some("v0.1.0"),
            repo: "example/dudes",
            run_url: "https://example.test/runs/1",
            sha: "0123456789abcdef0123456789abcdef01234567",
            sums: "aa  dudes-0.2.0-x.tar.gz\nbb  dudes-0.2.0-y.tar.gz\n",
            maintainer: "maintainer",
            authors: &authors,
            preview: false,
        };
        let body = render_body(
            &subjects(&[
                "fix: a crash on empty input (#9)",
                "docs: notes (#10)",
                "feat(cli): a new thing (#7)",
                "feat: direct commit without a number",
            ]),
            CHANGELOG,
            &inputs,
        )
        .expect("a body");
        assert_eq!(
            body,
            "\
## 0.2.0

Changes since [0.1.0](https://github.com/example/dudes/releases/tag/v0.1.0): 3 pull requests.

Early preview.

### Highlights

- **Faster starts.** The daemon starts in half the time (#7).

### Install

macOS and Linux, on x86_64 and arm64:

```sh
curl -fsSL https://raw.githubusercontent.com/example/dudes/main/install.sh | sh
~/.local/bin/dudes quickstart
```

The script picks the archive for your machine, checks it against `SHA256SUMS`, and installs `duded`, `dudes`, `dudes-hook` and the Floor into `~/.local`. It never uses `sudo` and never edits your shell profile. On Linux it installs the static build, which runs on any distribution. You also need at least one of the `claude`, `codex` or `pi` CLIs, signed in. Windows builds from source only for now; see [Platforms and known limits](https://github.com/example/dudes/blob/main/docs/platforms-and-limits.md).

### Changes

- A new `dudes thing` command (#7).

### Fixes

- A crash on empty input (#9).

### Upgrading

To install this exact version, run the installer again:

```sh
curl -fsSL https://raw.githubusercontent.com/example/dudes/main/install.sh | sh -s -- --version 0.2.0
```

A running daemon keeps using the old binary until it stops. Run `dudes stop`, then `dudes start`, to start the new one.

Breaking changes:

- `dudes old` is now `dudes new` (#7).

### Complete contribution record

- #9 fix: a crash on empty input. Thanks @outsider
- #10 docs: notes
- #7 feat(cli): a new thing

### Release verification

Release commit: `0123456789abcdef0123456789abcdef01234567`. Built by [this workflow run](https://example.test/runs/1).

`SHA256SUMS` for this release:

```
aa  dudes-0.2.0-x.tar.gz
bb  dudes-0.2.0-y.tar.gz
```

Put the archive for your machine and `SHA256SUMS` in one directory, then run `sha256sum --check --ignore-missing SHA256SUMS`. On macOS, `shasum -a 256 --check SHA256SUMS` works the same way but reports the archives you did not download as missing.

Each archive and `SHA256SUMS` also carry a build provenance attestation. `gh attestation verify <archive> --repo example/dudes` checks that this repository's release workflow built the file.
"
        );
    }

    #[test]
    fn an_empty_changes_block_and_no_previous_tag_read_plainly() {
        let authors = BTreeMap::new();
        let inputs = Inputs {
            version: "0.1.0",
            from: None,
            repo: "example/dudes",
            run_url: "u",
            sha: "s",
            sums: "x",
            maintainer: "m",
            authors: &authors,
            preview: true,
        };
        let body = render_body(
            &subjects(&["feat: only a direct commit"]),
            "## [Unreleased]\n",
            &inputs,
        )
        .expect("a preview body");
        assert!(
            body.contains("This is the first release: 0 pull requests.\n"),
            "{body}"
        );
        assert!(
            body.contains("### Changes\n\nNo new capabilities in this release.\n"),
            "{body}"
        );
        assert!(
            body.contains("### Fixes\n\nNo fixes in this release.\n"),
            "{body}"
        );
        assert!(
            body.contains("No breaking changes are listed for this release."),
            "{body}"
        );
        assert!(
            body.contains("No pull requests in this release.\n"),
            "{body}"
        );
    }

    #[test]
    fn arguments_select_a_mode_and_refuse_the_rest() {
        assert_eq!(
            parse(&words(&["--draft", "--from", "v0.1.0"])).expect("draft"),
            Mode::Draft {
                from: "v0.1.0".into()
            }
        );
        let body = parse(&words(&[
            "--version",
            "0.2.0",
            "--repo",
            "o/r",
            "--run-url",
            "u",
            "--sums",
            "S",
            "--maintainer",
            "m",
            "--preview",
        ]))
        .expect("body");
        assert_eq!(
            body,
            Mode::Body {
                version: "0.2.0".into(),
                from: None,
                repo: "o/r".into(),
                run_url: "u".into(),
                sums: "S".into(),
                maintainer: "m".into(),
                authors: None,
                preview: true,
            }
        );
        for refused in [
            words(&[]),
            words(&["--draft"]),
            words(&["--draft", "--from", "v1", "--repo", "o/r"]),
            words(&["--version", "0.2.0"]),
            words(&["--from"]),
            words(&["--from", "a", "--from", "b"]),
            words(&["--bogus"]),
        ] {
            assert!(parse(&refused).is_err(), "{refused:?} is refused");
        }
    }

    #[test]
    fn a_hostile_title_renders_as_plain_text() {
        let authors = BTreeMap::new();
        let inputs = Inputs {
            version: "0.1.0",
            from: None,
            repo: "example/dudes",
            run_url: "u",
            sha: "s",
            sums: "x",
            maintainer: "m",
            authors: &authors,
            preview: true,
        };
        let body = render_body(
            &subjects(&[
                "fix: ping @octocat, see [x](https://evil.test) <b>hi</b> `code` #99 (#5)",
            ]),
            "## [Unreleased]\n",
            &inputs,
        )
        .expect("a body");
        assert!(
            body.contains(
                "- #5 fix: ping \\@octocat, see \\[x\\](https:\\/\\/evil.test) \\<b\\>hi\\</b\\> \\`code\\` \\#99\n"
            ),
            "{body}"
        );
        assert_eq!(
            escape_markdown("snake_case & more"),
            "snake\\_case \\& more"
        );
    }

    #[test]
    fn authors_are_read_one_per_line() {
        assert_eq!(
            parse_authors("6 alice\n\n9 bob\n").expect("authors"),
            BTreeMap::from([(6, "alice".to_owned()), (9, "bob".to_owned())])
        );
        assert!(parse_authors("six alice").is_err());
        assert!(parse_authors("6").is_err());
    }
}
