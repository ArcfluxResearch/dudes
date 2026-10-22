//! `dudes uninstall`: work out what Dudes owns, refuse while anything irreplaceable remains, and
//! delete the rest in an order that keeps `dudes` itself until last.
//!
//! The daemon is reached through [`Owned`], so everything here is drivable without one.
//!
//! # What is never deleted
//!
//! A directory is touched only when it is the default path, or an override that holds content only
//! Dudes writes. The filesystem root, the home directory, any parent of it and any symbolic link are
//! refused by name. Agent worktrees are never deleted: they can hold the only copy of an agent's
//! work, so their presence refuses the whole command before anything is touched, and again after
//! the daemon stops.
//!
//! A folder that a setting chose is never removed wholesale. Dudes deletes the entries it knows
//! it writes there, then removes the folder only if that left it empty.

use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use dudes_proto::rpc::cred::OperatorCredential;

use crate::args::UninstallOptions;
use crate::env::LOCK_NAME;
use crate::exit;
use crate::layout::{self, NOTICES, PROGRAMS};
use crate::lifecycle::{self, Daemon};
use crate::render::{self, Escaped};
use crate::setup::Terminal;

/// The first bytes of a `redb` database, which only the daemon's projection writes here.
const REDB_MAGIC: [u8; 9] = [b'r', b'e', b'd', b'b', 0x1A, 0x0A, 0xA9, 0x0D, 0x0A];

/// The top-level tables the daemon reads from the operator file.
const OPERATOR_TABLES: [&str; 5] = ["budget", "floor", "providers", "memory", "mcp"];

/// What the state directory holds, markers last so an interrupted run can be repeated.
const STATE_KNOWN: Known = Known {
    names: &[
        "journal",
        "agents",
        "condense-model-cwd",
        "providers",
        "worktrees",
        "floor-port",
        "floor",
        "LICENSE-APACHE",
        "LICENSE-MIT",
        "THIRD-PARTY.md",
    ],
    prefixes: &[".install-rollback."],
    last: &["state.redb", LOCK_NAME],
};

/// What the runtime directory holds, markers last.
const RUNTIME_KNOWN: Known = Known {
    names: &["agent-bin", "git-read"],
    prefixes: &["shutdown-"],
    last: &["duded.log", "duded.sock", "operator.cred"],
};

const CONFIG_KNOWN: Known = Known {
    names: &["config.toml"],
    prefixes: &[".config.toml."],
    last: &[],
};

const SHARE_KNOWN: Known = Known {
    names: &["floor", "LICENSE-APACHE", "LICENSE-MIT", "THIRD-PARTY.md"],
    prefixes: &[".install-rollback."],
    last: &[],
};

/// Where everything lives, resolved once from the environment.
pub(crate) struct Layout {
    pub(crate) home: PathBuf,
    pub(crate) state_dir: PathBuf,
    pub(crate) runtime_dir: PathBuf,
    pub(crate) config: PathBuf,
    pub(crate) config_overridden: bool,
    pub(crate) executable: PathBuf,
    pub(crate) lock_wait: Duration,
    /// Where Codex keeps its files when `CODEX_HOME` names a folder other than `~/.codex`.
    pub(crate) codex_home: Option<PathBuf>,
}

/// What `uninstall` needs beyond the shared daemon: the trees the daemon knows of.
pub(crate) trait Owned: Daemon {
    /// The path of every worktree the daemon knows of. An error is a refusal, not an empty list.
    fn worktrees(&self) -> Result<Vec<PathBuf>, String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Runtime,
    State,
    ConfigFolder,
    ConfigFile,
    Share,
    Launcher,
    Program,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Runtime => "runtime directory",
            Self::State => "state directory",
            Self::ConfigFolder => "operator file folder",
            Self::ConfigFile => "operator file",
            Self::Share => "installed Floor and notices",
            Self::Launcher => "launcher",
            Self::Program => "program",
        }
    }
}

/// The entries Dudes writes in a folder a setting may have chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Known {
    names: &'static [&'static str],
    prefixes: &'static [&'static str],
    last: &'static [&'static str],
}

impl Known {
    fn matches(self, name: &str) -> bool {
        self.names.contains(&name)
            || self.last.contains(&name)
            || self.prefixes.iter().any(|prefix| name.starts_with(prefix))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Removal {
    /// The default path: everything in it is Dudes' own.
    Whole,
    /// A path a setting chose: only the known entries, then the folder if it is empty.
    Known(Known),
    File,
    /// A launcher `install.sh` wrote: a file or an app folder, deleted whole once its marker is found.
    Launcher,
}

/// The line `install.sh` writes into a desktop entry, and the line it writes into an app's
/// `Info.plist`, so a launcher is told from a file or an app of the same name that is someone
/// else's. Each must be a whole line, not a word somewhere in the file.
const DESKTOP_MARKER: &str = "X-Dudes-Launcher=1";
const PLIST_MARKER: &str = "<key>DudesLauncher</key><true/>";

/// Where the launchers go, below the home directory: the macOS app and the Linux desktop entry.
const LAUNCHERS: [&str; 2] = [
    "Applications/Dudes.app",
    ".local/share/applications/dudes.desktop",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Content {
    Any,
    State,
    Runtime,
    Share,
}

#[derive(Debug)]
struct Entry {
    kind: Kind,
    path: PathBuf,
    removal: Removal,
    content: Content,
    operator_file: bool,
    bytes: u64,
}

/// A tree under the state directory's `worktrees/`.
#[derive(Debug, PartialEq, Eq)]
struct Tree {
    path: PathBuf,
    repository: Option<PathBuf>,
    branch: Option<String>,
}

struct Plan {
    entries: Vec<Entry>,
    skipped: Vec<String>,
    by_hand: Option<Vec<PathBuf>>,
    why_by_hand: String,
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn size(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| size(&entry.path()))
        .sum()
}

fn known_size(path: &Path, known: Known) -> u64 {
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| known.matches(&entry.file_name().to_string_lossy()))
        .map(|entry| size(&entry.path()))
        .sum()
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut unit = 0;
    let mut scaled = u128::from(bytes) * 10 / 1024;
    while scaled >= 10_240 && unit + 1 < UNITS.len() {
        scaled /= 1024;
        unit += 1;
    }
    format!("{}.{} {}", scaled / 10, scaled % 10, UNITS[unit])
}

fn regular(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn starts_with_magic(path: &Path) -> bool {
    use std::io::Read as _;
    let mut head = [0_u8; 9];
    regular(path)
        && std::fs::File::open(path)
            .and_then(|mut file| file.read_exact(&mut head))
            .is_ok()
        && head == REDB_MAGIC
}

#[cfg(unix)]
fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
}

#[cfg(not(unix))]
fn is_socket(_: &Path) -> bool {
    false
}

/// Whether `dir` holds content only the named writer produces, not just a name.
fn has_content(dir: &Path, content: Content) -> bool {
    match content {
        Content::Any => true,
        Content::State => {
            regular(&dir.join(LOCK_NAME)) && starts_with_magic(&dir.join("state.redb"))
        }
        Content::Runtime => {
            std::fs::read_to_string(dir.join("operator.cred"))
                .is_ok_and(|text| OperatorCredential::new(text).is_ok())
                || is_socket(&dir.join("duded.sock"))
                || std::fs::read_to_string(dir.join("duded.log"))
                    .is_ok_and(|log| log.starts_with("quickstart started duded pid "))
        }
        Content::Share => NOTICES.iter().all(|name| regular(&dir.join(name))),
    }
}

/// Why `path` may not be deleted as a directory, or `Ok` when it may.
fn vet_directory(
    path: &Path,
    home: &Path,
    content: Content,
    removal: Removal,
) -> Result<(), String> {
    let shown = path.display();
    if !path.is_absolute() {
        return Err(format!("{shown} is not an absolute path"));
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| format!("{shown}: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "{shown} is a symbolic link, and Dudes does not delete through one; remove the link yourself if it is yours"
        ));
    }
    if !metadata.is_dir() {
        return Err(format!("{shown} is not a directory"));
    }
    let real = path
        .canonicalize()
        .map_err(|error| format!("{shown}: {error}"))?;
    let home_real = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    if real.parent().is_none() {
        return Err(format!("{shown} is the filesystem root"));
    }
    if home_real.starts_with(&real) {
        return Err(format!(
            "{shown} is the home directory or a parent of it, which Dudes never deletes"
        ));
    }
    if removal != Removal::Whole && !has_content(path, content) {
        return Err(format!(
            "{shown} is not the default location and does not hold what Dudes writes there, so Dudes will not delete in it"
        ));
    }
    Ok(())
}

/// Why `path` is not a launcher `install.sh` wrote, or `None` when it is.
fn launcher_problem(path: &Path) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    let (text, marker) = if metadata.file_type().is_symlink() {
        (None, "")
    } else if metadata.is_dir() {
        (
            std::fs::read_to_string(path.join("Contents/Info.plist")).ok(),
            PLIST_MARKER,
        )
    } else {
        (std::fs::read_to_string(path).ok(), DESKTOP_MARKER)
    };
    text.is_none_or(|text| !text.lines().any(|line| line.trim() == marker))
        .then(|| "it is not the launcher install.sh writes".to_owned())
}

fn operator_file_problem(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let Ok(table) = text.parse::<toml::Table>() else {
        return Some("it is not valid TOML".to_owned());
    };
    let foreign: Vec<_> = table
        .keys()
        .filter(|key| !OPERATOR_TABLES.contains(&key.as_str()))
        .cloned()
        .collect();
    (!foreign.is_empty()).then(|| {
        format!(
            "it has tables an operator file does not: {}",
            foreign.join(", ")
        )
    })
}

fn vet(entry: &Entry, home: &Path) -> Result<(), String> {
    match entry.removal {
        Removal::Whole | Removal::Known(_) => {
            vet_directory(&entry.path, home, entry.content, entry.removal)
        }
        Removal::Launcher => launcher_problem(&entry.path).map_or(Ok(()), Err),
        Removal::File => {
            let metadata = std::fs::symlink_metadata(&entry.path)
                .map_err(|error| format!("{}: {error}", entry.path.display()))?;
            if metadata.is_dir() {
                return Err(format!(
                    "{} is a directory, not a file",
                    entry.path.display()
                ));
            }
            Ok(())
        }
    }
}

fn candidates(layout: &Layout, prefix: Option<&Path>) -> Vec<Entry> {
    let home = &layout.home;
    let default = |path: &Path, tail: &str| path == home.join(tail);
    let directory = |kind, path: &Path, is_default: bool, known, content| Entry {
        kind,
        path: path.to_path_buf(),
        removal: if is_default {
            Removal::Whole
        } else {
            Removal::Known(known)
        },
        content,
        operator_file: false,
        bytes: 0,
    };
    let mut list = vec![
        directory(
            Kind::Runtime,
            &layout.runtime_dir,
            default(&layout.runtime_dir, ".local/state/dudes"),
            RUNTIME_KNOWN,
            Content::Runtime,
        ),
        directory(
            Kind::State,
            &layout.state_dir,
            default(&layout.state_dir, ".local/share/dudes"),
            STATE_KNOWN,
            Content::State,
        ),
    ];
    let folder = layout.config.parent().filter(|folder| {
        !layout.config_overridden
            && layout.config.file_name() == Some("config.toml".as_ref())
            && folder.file_name() == Some("dudes".as_ref())
    });
    match folder {
        Some(folder) => list.push(Entry {
            removal: Removal::Known(CONFIG_KNOWN),
            ..directory(
                Kind::ConfigFolder,
                folder,
                false,
                CONFIG_KNOWN,
                Content::Any,
            )
        }),
        None => list.push(Entry {
            kind: Kind::ConfigFile,
            path: layout.config.clone(),
            removal: Removal::File,
            content: Content::Any,
            operator_file: layout.config_overridden,
            bytes: 0,
        }),
    }
    if let Some(prefix) = prefix {
        let share = prefix.join("share/dudes");
        if share != layout.state_dir {
            list.push(directory(
                Kind::Share,
                &share,
                false,
                SHARE_KNOWN,
                Content::Share,
            ));
        }
        for launcher in LAUNCHERS {
            list.push(Entry {
                kind: Kind::Launcher,
                path: home.join(launcher),
                removal: Removal::Launcher,
                content: Content::Any,
                operator_file: false,
                bytes: 0,
            });
        }
        for name in PROGRAMS {
            list.push(Entry {
                kind: Kind::Program,
                path: prefix
                    .join("bin")
                    .join(format!("{name}{}", std::env::consts::EXE_SUFFIX)),
                removal: Removal::File,
                content: Content::Any,
                operator_file: false,
                bytes: 0,
            });
        }
    }
    list
}

/// Everything Dudes owns that is present, in deletion order, or the first guard that refuses.
fn plan(layout: &Layout) -> Result<Plan, String> {
    let installed = layout::installed_prefix(&layout.executable);
    let (prefix, why_by_hand) = match &installed {
        Ok(prefix) => (Some(prefix.as_path()), String::new()),
        Err(reason) => (None, reason.clone()),
    };
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    for mut entry in candidates(layout, prefix) {
        if !exists(&entry.path) {
            continue;
        }
        if entry.removal == Removal::Launcher {
            if let Some(problem) = launcher_problem(&entry.path) {
                skipped.push(format!(
                    "{} is not deleted: {problem}",
                    entry.path.display()
                ));
                continue;
            }
        } else {
            vet(&entry, &layout.home)?;
        }
        if entry.operator_file
            && let Some(problem) = operator_file_problem(&entry.path)
        {
            skipped.push(format!(
                "{} is not deleted: {problem}",
                entry.path.display()
            ));
            continue;
        }
        entry.bytes = match entry.removal {
            Removal::Known(known) => known_size(&entry.path, known),
            Removal::Whole | Removal::File | Removal::Launcher => size(&entry.path),
        };
        entries.push(entry);
    }
    let by_hand = installed
        .is_err()
        .then(|| {
            let real = layout
                .executable
                .canonicalize()
                .unwrap_or_else(|_| layout.executable.clone());
            let mut found: Vec<PathBuf> = real
                .parent()
                .into_iter()
                .flat_map(|bin| PROGRAMS.iter().map(|name| bin.join(name)))
                .filter(|path| exists(path))
                .collect();
            if real != layout.executable && exists(&layout.executable) {
                found.push(layout.executable.clone());
            }
            found
        })
        .filter(|programs| !programs.is_empty());
    Ok(Plan {
        entries,
        skipped,
        by_hand,
        why_by_hand,
    })
}

/// The repository and branch a tree belongs to, read from its `.git` pointer without running git.
fn describe(tree: &Path) -> Tree {
    let read = |path: PathBuf| {
        let metadata = std::fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file() || metadata.len() > 4096 {
            return None;
        }
        std::fs::read_to_string(path).ok()
    };
    let admin = read(tree.join(".git")).and_then(|text| {
        text.trim()
            .strip_prefix("gitdir:")
            .map(|rest| PathBuf::from(rest.trim()))
    });
    let repository = admin.as_deref().and_then(|admin: &Path| {
        let worktrees = admin.parent()?;
        let git = worktrees.parent()?;
        (worktrees.file_name()? == "worktrees" && git.file_name()? == ".git")
            .then(|| git.parent().map(Path::to_path_buf))?
    });
    let branch = admin
        .and_then(|admin| read(admin.join("HEAD")))
        .and_then(|head| {
            head.trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_owned)
        });
    Tree {
        path: tree.to_path_buf(),
        repository,
        branch,
    }
}

fn trees(state_dir: &Path, listed: Vec<PathBuf>) -> Vec<Tree> {
    let mut paths: BTreeSet<PathBuf> = listed.into_iter().filter(|path| exists(path)).collect();
    for entry in std::fs::read_dir(state_dir.join("worktrees"))
        .into_iter()
        .flatten()
        .flatten()
    {
        paths.insert(entry.path());
    }
    paths.iter().map(|path| describe(path)).collect()
}

fn quoted(path: &Path) -> String {
    format!(
        "'{}'",
        Escaped(&path.display().to_string())
            .to_string()
            .replace('\'', "'\\''")
    )
}

fn refuse_worktrees(found: &[Tree], err: &mut dyn Write) -> std::io::Result<()> {
    let count = found.len();
    render::local_error(
        err,
        &format!(
            "refusing to uninstall: {count} agent worktree{} exist{}. A tree can hold the only copy of an agent's work, so it was not deleted.",
            if count == 1 { "" } else { "s" },
            if count == 1 { "s" } else { "" },
        ),
    )?;
    for tree in found {
        writeln!(err)?;
        writeln!(err, "  {}", Escaped(&tree.path.display().to_string()))?;
        let Some(repository) = &tree.repository else {
            writeln!(
                err,
                "    repository: unknown (this tree's .git file is missing or unreadable)"
            )?;
            writeln!(
                err,
                "    git -C {} status --short --ignored",
                quoted(&tree.path)
            )?;
            continue;
        };
        let branch = tree.branch.as_deref().unwrap_or("dudes/<id>");
        writeln!(
            err,
            "    repository: {}",
            Escaped(&repository.display().to_string())
        )?;
        writeln!(
            err,
            "    git -C {} status --short --ignored",
            quoted(&tree.path)
        )?;
        writeln!(
            err,
            "    git -C {} worktree remove {}",
            quoted(repository),
            quoted(&tree.path)
        )?;
        writeln!(
            err,
            "    git -C {} branch -d {}",
            quoted(repository),
            quoted(Path::new(branch))
        )?;
    }
    writeln!(err)?;
    writeln!(
        err,
        "Review each tree for uncommitted work (?? untracked, M modified, !! ignored files that removal deletes), commit or copy out what you want to keep, remove it with the commands above, then run dudes uninstall again."
    )
}

/// What a deletion left behind.
#[derive(Default)]
struct Outcome {
    removed: usize,
    failed: Vec<String>,
    kept: Vec<(PathBuf, Vec<String>)>,
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Delete a whole folder, its license notices last: they mark it as part of an install, so a run that
/// stops early must leave them for the rerun to recognise the install and keep the programs.
fn remove_whole(path: &Path) -> std::io::Result<()> {
    if !std::fs::symlink_metadata(path)?.is_dir() {
        return remove_path(path);
    }
    let (notices, ordinary): (Vec<_>, Vec<_>) = std::fs::read_dir(path)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .partition(|child| NOTICES.iter().any(|name| child.file_name() == *name));
    for child in ordinary.iter().chain(&notices) {
        remove_path(&child.path())?;
    }
    std::fs::remove_dir(path)
}

/// Delete the known entries of a folder a setting chose, markers last, then the folder if empty.
fn remove_known(entry: &Entry, known: Known, outcome: &mut Outcome) {
    let Ok(listing) = std::fs::read_dir(&entry.path) else {
        outcome
            .failed
            .push(format!("{}: cannot be read", entry.path.display()));
        return;
    };
    let mut ordinary = Vec::new();
    let mut last = Vec::new();
    for child in listing.flatten() {
        let name = child.file_name().to_string_lossy().into_owned();
        if known.last.contains(&name.as_str()) {
            last.push(child.path());
        } else if known.matches(&name) {
            ordinary.push(child.path());
        }
    }
    let before = outcome.failed.len();
    for path in ordinary {
        match remove_path(&path) {
            Ok(()) => outcome.removed += 1,
            Err(error) => outcome.failed.push(format!("{}: {error}", path.display())),
        }
    }
    if outcome.failed.len() > before {
        return;
    }
    for path in last {
        match remove_path(&path) {
            Ok(()) => outcome.removed += 1,
            Err(error) => outcome.failed.push(format!("{}: {error}", path.display())),
        }
    }
    if outcome.failed.len() > before {
        return;
    }
    if std::fs::remove_dir(&entry.path).is_ok() {
        outcome.removed += 1;
        return;
    }
    let mut left: Vec<String> = std::fs::read_dir(&entry.path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|child| child.file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    outcome.kept.push((entry.path.clone(), left));
}

fn remove(entry: &Entry, home: &Path, outcome: &mut Outcome) {
    if let Err(reason) = vet(entry, home) {
        outcome.failed.push(reason);
        return;
    }
    match entry.removal {
        Removal::Known(known) => remove_known(entry, known, outcome),
        Removal::Whole | Removal::File | Removal::Launcher => {
            let result = if entry.removal == Removal::Whole {
                remove_whole(&entry.path)
            } else {
                remove_path(&entry.path)
            };
            match result {
                Ok(()) => outcome.removed += 1,
                Err(error) => outcome
                    .failed
                    .push(format!("{}: {error}", entry.path.display())),
            }
        }
    }
}

fn leftovers(out: &mut dyn Write, layout: &Layout) -> std::io::Result<()> {
    let claude_sessions = layout.home.join(".claude/projects");
    let codex_folders: Vec<PathBuf> = std::iter::once(layout.home.join(".codex"))
        .chain(layout.codex_home.clone())
        .collect();
    let codex_sessions = codex_folders
        .iter()
        .any(|folder| exists(&folder.join("sessions")));
    let codex_snapshots = codex_folders
        .iter()
        .any(|folder| exists(&folder.join("shell_snapshots")));
    let session_folders: Vec<&str> = [
        (exists(&claude_sessions), "~/.claude/projects/"),
        (codex_sessions, "~/.codex/sessions/"),
    ]
    .into_iter()
    .filter_map(|(present, name)| present.then_some(name))
    .collect();
    writeln!(out)?;
    writeln!(out, "Dudes cannot clean up these, so they are yours to do:")?;
    if !session_folders.is_empty() {
        writeln!(
            out,
            "  - Claude Code and Codex keep their own session history in their own folders, for example"
        )?;
        writeln!(
            out,
            "    {}. Dudes does not read or delete them.",
            session_folders.join(" and ")
        )?;
    }
    if codex_snapshots {
        writeln!(
            out,
            "  - Codex shell snapshots from earlier versions can hold an agent's DUDES_TOKEN. Delete the"
        )?;
        writeln!(
            out,
            "    ones that do (use $CODEX_HOME instead of ~/.codex if you set it):"
        )?;
        writeln!(
            out,
            "      find ~/.codex/shell_snapshots -type f -exec grep -q 'DUDES_TOKEN=' {{}} \\; -print -delete"
        )?;
    }
    writeln!(
        out,
        "  - Clear your browser's site data for the Floor's 127.0.0.1 address to remove its view preferences."
    )?;
    writeln!(
        out,
        "  - Branches named dudes/<id> in repositories where you hired with --worktree stay until you delete them."
    )
}

/// Run `dudes uninstall`. Returns the process's exit code.
pub(crate) fn run(
    layout: &Layout,
    options: UninstallOptions,
    terminal: Option<&mut dyn BufRead>,
    daemon: &dyn Owned,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    match uninstall(layout, options, terminal, daemon, out, err) {
        Ok(code) => code,
        Err(message) => {
            let _ = render::local_error(err, &message);
            exit::OTHER
        }
    }
}

fn unlisted(reason: &str) -> String {
    format!(
        "refusing to uninstall: could not ask the daemon for its worktrees ({reason}); nothing was deleted"
    )
}

fn refuse(err: &mut dyn Write, message: &str) -> Result<u8, String> {
    render::local_error(err, message).map_err(|error| error.to_string())?;
    Ok(exit::REFUSED)
}

fn print_plan(plan: &Plan, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "Dudes will remove:")?;
    for entry in &plan.entries {
        let only = if matches!(entry.removal, Removal::Known(_)) {
            ", only the files Dudes writes there"
        } else {
            ""
        };
        writeln!(
            out,
            "  {:<27} {}  ({}{only})",
            entry.kind.label(),
            Escaped(&entry.path.display().to_string()),
            human(entry.bytes)
        )?;
    }
    for note in &plan.skipped {
        writeln!(out, "  skipped: {}", Escaped(note))?;
    }
    if let Some(programs) = &plan.by_hand {
        writeln!(out, "\nThe programs are not removed: {}.", plan.why_by_hand)?;
        if !programs.is_empty() {
            writeln!(out, "Remove them by hand when you no longer need them:")?;
            for path in programs {
                writeln!(out, "  rm {}", quoted(path))?;
            }
        }
    }
    Ok(())
}

fn confirm(
    options: UninstallOptions,
    terminal: Option<&mut dyn BufRead>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<Option<u8>, String> {
    if options.yes {
        return Ok(None);
    }
    let Some(input) = terminal else {
        return refuse(
            err,
            "uninstall needs --yes when it is not run on a terminal; nothing was deleted",
        )
        .map(Some);
    };
    writeln!(out).map_err(|error| error.to_string())?;
    if matches!(
        Terminal::new(input, out).yes("Delete all of this?", false),
        Ok(true)
    ) {
        return Ok(None);
    }
    writeln!(out, "Nothing was changed.").map_err(|error| error.to_string())?;
    Ok(Some(exit::SUCCESS))
}

fn uninstall(
    layout: &Layout,
    options: UninstallOptions,
    terminal: Option<&mut dyn BufRead>,
    daemon: &dyn Owned,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, String> {
    let io = |error: std::io::Error| error.to_string();
    let plan = match plan(layout) {
        Ok(plan) => plan,
        Err(reason) => {
            return refuse(
                err,
                &format!("refusing to uninstall: {reason}. Nothing was deleted."),
            );
        }
    };
    if plan.entries.is_empty() && plan.skipped.is_empty() && plan.by_hand.is_none() {
        writeln!(out, "Dudes has nothing installed or stored here.").map_err(io)?;
        return Ok(exit::SUCCESS);
    }
    print_plan(&plan, out).map_err(io)?;
    let running = daemon.running();
    if running {
        match lifecycle::check_busy(daemon, options.now, out) {
            Ok(None) => {}
            Ok(Some(message)) | Err(message) => {
                return refuse(err, &format!("refusing to uninstall: {message}"));
            }
        }
        writeln!(out, "\nThe daemon is running and will be stopped first.").map_err(io)?;
    } else if lifecycle::lock_held(&layout.state_dir) {
        return refuse(
            err,
            &format!(
                "refusing to uninstall: a process holds the lock in {}, so a daemon is still running; stop it with dudes stop. Nothing was deleted.",
                layout.state_dir.display()
            ),
        );
    }
    let listed = if running {
        match daemon.worktrees() {
            Ok(listed) => listed,
            Err(reason) => return refuse(err, &unlisted(&reason)),
        }
    } else {
        Vec::new()
    };
    let found = trees(&layout.state_dir, listed.clone());
    if !found.is_empty() {
        writeln!(out).map_err(io)?;
        refuse_worktrees(&found, err).map_err(io)?;
        return Ok(exit::REFUSED);
    }
    if options.dry_run {
        writeln!(out, "\nThis was a dry run. Nothing was deleted.").map_err(io)?;
        return Ok(exit::SUCCESS);
    }
    let listed_before = listed;
    if let Some(code) = confirm(options, terminal, out, err)? {
        return Ok(code);
    }
    if running {
        daemon
            .stop(out, err)
            .map_err(|reason| format!("the daemon did not stop ({reason}); nothing was deleted"))?;
        if !lifecycle::wait_for_unlock(&layout.state_dir, layout.lock_wait) {
            return refuse(
                err,
                &format!(
                    "the daemon stopped but a process still holds the lock in {}; nothing was deleted",
                    layout.state_dir.display()
                ),
            );
        }
        let found = trees(&layout.state_dir, listed_before);
        if !found.is_empty() {
            refuse_worktrees(&found, err).map_err(io)?;
            return Ok(exit::REFUSED);
        }
    }
    delete(layout, &plan, out, err)
}

/// Delete in order, keeping the programs if anything before them failed.
fn delete(
    layout: &Layout,
    plan: &Plan,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, String> {
    let io = |error: std::io::Error| error.to_string();
    let mut outcome = Outcome::default();
    for entry in &plan.entries {
        if entry.kind == Kind::Program && !outcome.failed.is_empty() {
            break;
        }
        if !exists(&entry.path) {
            continue;
        }
        let before = outcome.removed;
        remove(entry, &layout.home, &mut outcome);
        if outcome.removed > before {
            writeln!(
                out,
                "removed {}",
                Escaped(&entry.path.display().to_string())
            )
            .map_err(io)?;
        }
    }
    for (path, left) in &outcome.kept {
        writeln!(
            out,
            "kept {}: it holds files that are not Dudes': {}",
            Escaped(&path.display().to_string()),
            Escaped(&left.join(", "))
        )
        .map_err(io)?;
    }
    let remaining: Vec<_> = plan
        .entries
        .iter()
        .filter(|entry| {
            exists(&entry.path) && !outcome.kept.iter().any(|(path, _)| *path == entry.path)
        })
        .collect();
    if outcome.failed.is_empty() && remaining.is_empty() {
        writeln!(
            out,
            "\nDudes is uninstalled ({} items removed).",
            outcome.removed
        )
        .map_err(io)?;
        leftovers(out, layout).map_err(io)?;
        return Ok(exit::SUCCESS);
    }
    render::local_error(
        err,
        &format!(
            "uninstall did not finish: {} removed, {} remain",
            outcome.removed,
            remaining.len()
        ),
    )
    .map_err(io)?;
    for reason in &outcome.failed {
        writeln!(err, "  failed: {}", Escaped(reason)).map_err(io)?;
    }
    writeln!(err, "Still present:").map_err(io)?;
    for entry in &remaining {
        writeln!(err, "  {}", Escaped(&entry.path.display().to_string())).map_err(io)?;
    }
    if remaining.iter().any(|entry| entry.kind == Kind::Program) {
        writeln!(
            err,
            "The programs were kept so you can run dudes uninstall again once the cause is fixed."
        )
        .map_err(io)?;
    }
    Ok(exit::OTHER)
}

#[cfg(test)]
mod tests;
