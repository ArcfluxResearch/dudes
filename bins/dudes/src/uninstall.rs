//! `dudes uninstall`: work out what Dudes owns, refuse while anything irreplaceable remains, and
//! delete the rest in an order that keeps `dudes` itself until last.
//!
//! The daemon is reached through [`Daemon`], so everything here is drivable without one.
//!
//! # What is never deleted
//!
//! A directory is deleted only when it is the default path, or an override that holds Dudes' own
//! files. The filesystem root, the home directory, any parent of it and any symbolic link are
//! refused by name. Agent worktrees are never deleted: they can hold the only copy of an agent's
//! work, so their presence refuses the whole command before anything is touched.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::args::UninstallOptions;
use crate::env::LOCK_NAME;
use crate::exit;
use crate::render::{self, Escaped};
use crate::setup::Terminal;

/// The three programs `install.sh` puts in `<prefix>/bin`, by their exact names.
const PROGRAMS: [&str; 3] = ["duded", "dudes", "dudes-hook"];

/// Files that show a directory was made by the daemon's state.
const STATE_MARKERS: [&str; 3] = ["journal", "state.redb", LOCK_NAME];

/// Files that show a directory was made by the daemon's runtime.
const RUNTIME_MARKERS: [&str; 3] = ["duded.sock", "operator.cred", "duded.log"];

/// Files that show a `share/dudes` directory was made by `install.sh`.
const SHARE_MARKERS: [&str; 4] = ["floor", "LICENSE-APACHE", "LICENSE-MIT", "THIRD-PARTY.md"];

/// Where everything lives, resolved once from the environment.
pub(crate) struct Layout {
    pub(crate) home: PathBuf,
    pub(crate) state_dir: PathBuf,
    pub(crate) runtime_dir: PathBuf,
    pub(crate) config: PathBuf,
    pub(crate) config_overridden: bool,
    pub(crate) executable: PathBuf,
}

/// What `uninstall` needs from a daemon that may be running.
pub(crate) trait Daemon {
    /// Whether a daemon answers.
    fn running(&self) -> bool;
    /// The agents that are mid-turn, each as a printable `name (id)`.
    fn mid_turn(&self) -> Result<Vec<String>, String>;
    /// The path of every worktree the daemon knows of.
    fn worktrees(&self) -> Vec<PathBuf>;
    /// Stop it the way `dudes stop` does, or at once when `now`.
    fn stop(&self, now: bool, out: &mut dyn Write, err: &mut dyn Write) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Guard {
    Directory {
        markers: &'static [&'static str],
        default: bool,
    },
    File,
}

#[derive(Debug)]
struct Entry {
    label: &'static str,
    path: PathBuf,
    guard: Guard,
    bytes: u64,
}

/// A tree under the state directory's `worktrees/`.
#[derive(Debug, PartialEq, Eq)]
struct Tree {
    path: PathBuf,
    repository: Option<PathBuf>,
    branch: Option<String>,
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

/// Why `path` may not be deleted as a directory, or `Ok` when it may.
fn vet_directory(path: &Path, home: &Path, markers: &[&str], default: bool) -> Result<(), String> {
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
    if !default && !markers.iter().any(|name| exists(&path.join(name))) {
        return Err(format!(
            "{shown} is not the default location and holds none of Dudes' own files ({}), so Dudes will not delete it",
            markers.join(", ")
        ));
    }
    Ok(())
}

fn vet_file(path: &Path) -> Result<(), String> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if metadata.is_dir() {
        return Err(format!("{} is a directory, not a file", path.display()));
    }
    Ok(())
}

fn vet(entry: &Entry, home: &Path) -> Result<(), String> {
    match entry.guard {
        Guard::Directory { markers, default } => vet_directory(&entry.path, home, markers, default),
        Guard::File => vet_file(&entry.path),
    }
}

/// Everything Dudes owns that is present, in deletion order, or the first guard that refuses.
fn plan(layout: &Layout) -> Result<Vec<Entry>, String> {
    let home = &layout.home;
    let mut candidates = vec![
        (
            "runtime directory",
            layout.runtime_dir.clone(),
            Guard::Directory {
                markers: &RUNTIME_MARKERS,
                default: layout.runtime_dir == home.join(".local/state/dudes"),
            },
        ),
        (
            "state directory",
            layout.state_dir.clone(),
            Guard::Directory {
                markers: &STATE_MARKERS,
                default: layout.state_dir == home.join(".local/share/dudes"),
            },
        ),
    ];
    let folder = layout
        .config
        .parent()
        .filter(|folder| !layout.config_overridden && folder.file_name() == Some("dudes".as_ref()));
    match folder {
        Some(folder) => candidates.push((
            "operator file folder",
            folder.to_path_buf(),
            Guard::Directory {
                markers: &[],
                default: true,
            },
        )),
        None => candidates.push(("operator file", layout.config.clone(), Guard::File)),
    }
    let bin = layout.executable.parent();
    if let Some(share) = bin
        .filter(|bin| bin.file_name() == Some("bin".as_ref()))
        .and_then(Path::parent)
        .map(|prefix| prefix.join("share/dudes"))
        .filter(|share| *share != layout.state_dir)
        .filter(|share| SHARE_MARKERS.iter().any(|name| exists(&share.join(name))))
    {
        candidates.push((
            "installed Floor and notices",
            share,
            Guard::Directory {
                markers: &SHARE_MARKERS,
                default: false,
            },
        ));
    }
    if let Some(bin) = bin {
        for name in PROGRAMS {
            let name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
            candidates.push(("program", bin.join(name), Guard::File));
        }
    }
    let mut entries = Vec::new();
    for (label, path, guard) in candidates {
        if !exists(&path) {
            continue;
        }
        let mut entry = Entry {
            bytes: 0,
            label,
            path,
            guard,
        };
        vet(&entry, home)?;
        entry.bytes = size(&entry.path);
        entries.push(entry);
    }
    Ok(entries)
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
            "refusing to uninstall: {count} agent worktree{} exist{}. A tree can hold the only copy of an agent's work, so nothing was deleted.",
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

fn lock_held(state_dir: &Path) -> bool {
    File::open(state_dir.join(LOCK_NAME)).is_ok_and(|lock| {
        matches!(
            lock.try_lock_shared(),
            Err(std::fs::TryLockError::WouldBlock)
        )
    })
}

fn remove(entry: &Entry, home: &Path) -> Result<(), String> {
    vet(entry, home)?;
    let result = match entry.guard {
        Guard::Directory { .. } => std::fs::remove_dir_all(&entry.path),
        Guard::File => std::fs::remove_file(&entry.path),
    };
    result.map_err(|error| format!("{}: {error}", entry.path.display()))
}

fn leftovers(out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out)?;
    writeln!(out, "Dudes cannot clean up these, so they are yours to do:")?;
    writeln!(
        out,
        "  - Claude Code and Codex keep their own session history in their own folders, for example"
    )?;
    writeln!(
        out,
        "    ~/.claude/projects/ and ~/.codex/sessions/. Dudes does not read or delete them."
    )?;
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
    daemon: &dyn Daemon,
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

/// What stops a run before it asks or deletes: an exit code when something does.
fn blockers(
    layout: &Layout,
    options: UninstallOptions,
    daemon: &dyn Daemon,
    running: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<Option<u8>, String> {
    let io = |error: std::io::Error| error.to_string();
    let found = trees(
        &layout.state_dir,
        if running {
            daemon.worktrees()
        } else {
            Vec::new()
        },
    );
    if !found.is_empty() {
        writeln!(out).map_err(io)?;
        refuse_worktrees(&found, err).map_err(io)?;
        return Ok(Some(exit::REFUSED));
    }
    if running {
        if !options.now {
            let busy = daemon.mid_turn().map_err(|reason| {
                format!(
                    "could not list the agents that are mid-turn ({reason}); nothing was deleted. Pass --now to stop the daemon anyway"
                )
            })?;
            if !busy.is_empty() {
                render::local_error(
                    err,
                    &format!(
                        "refusing to uninstall: these agents are mid-turn: {}. Wait for them to finish, or pass --now to stop the daemon and end their turns at once. Nothing was deleted.",
                        busy.join(", ")
                    ),
                )
                .map_err(io)?;
                return Ok(Some(exit::REFUSED));
            }
        }
        writeln!(out, "\nThe daemon is running and will be stopped first.").map_err(io)?;
    } else if lock_held(&layout.state_dir) {
        render::local_error(
            err,
            &format!(
                "refusing to uninstall: a process holds the lock in {}, so a daemon is still running; stop it with dudes stop. Nothing was deleted.",
                layout.state_dir.display()
            ),
        )
        .map_err(io)?;
        return Ok(Some(exit::REFUSED));
    }
    Ok(None)
}

fn confirm(
    options: UninstallOptions,
    terminal: Option<&mut dyn BufRead>,
    out: &mut dyn Write,
) -> Result<(), String> {
    if options.yes {
        return Ok(());
    }
    let Some(input) = terminal else {
        return Err(
            "uninstall needs --yes when it is not run on a terminal; nothing was deleted"
                .to_owned(),
        );
    };
    writeln!(out).map_err(|error| error.to_string())?;
    match Terminal::new(input, out).yes("Delete all of this?", false) {
        Ok(true) => Ok(()),
        _ => Err("uninstall canceled; nothing was deleted".to_owned()),
    }
}

fn uninstall(
    layout: &Layout,
    options: UninstallOptions,
    terminal: Option<&mut dyn BufRead>,
    daemon: &dyn Daemon,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, String> {
    let io = |error: std::io::Error| error.to_string();
    let entries = match plan(layout) {
        Ok(entries) => entries,
        Err(reason) => {
            render::local_error(err, &format!("refusing to uninstall: {reason}")).map_err(io)?;
            writeln!(err, "Nothing was deleted.").map_err(io)?;
            return Ok(exit::REFUSED);
        }
    };
    if entries.is_empty() {
        writeln!(out, "Dudes has nothing installed or stored here.").map_err(io)?;
        return Ok(exit::SUCCESS);
    }
    writeln!(out, "Dudes will remove:").map_err(io)?;
    for entry in &entries {
        writeln!(
            out,
            "  {:<27} {}  ({})",
            entry.label,
            Escaped(&entry.path.display().to_string()),
            human(entry.bytes)
        )
        .map_err(io)?;
    }
    let running = daemon.running();
    if let Some(code) = blockers(layout, options, daemon, running, out, err)? {
        return Ok(code);
    }
    if options.dry_run {
        writeln!(out, "\nThis was a dry run. Nothing was deleted.").map_err(io)?;
        return Ok(exit::SUCCESS);
    }
    confirm(options, terminal, out)?;
    if running {
        daemon
            .stop(options.now, out, err)
            .map_err(|reason| format!("the daemon did not stop ({reason}); nothing was deleted"))?;
        if lock_held(&layout.state_dir) {
            return Err(format!(
                "the daemon stopped but a process still holds the lock in {}; nothing was deleted",
                layout.state_dir.display()
            ));
        }
    }
    delete(layout, &entries, out, err)
}

/// Delete in order, keeping the programs if anything before them failed.
fn delete(
    layout: &Layout,
    entries: &[Entry],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, String> {
    let io = |error: std::io::Error| error.to_string();
    let mut failed = Vec::new();
    let mut removed = 0usize;
    for entry in entries {
        if entry.label == "program" && !failed.is_empty() {
            break;
        }
        if !exists(&entry.path) {
            continue;
        }
        match remove(entry, &layout.home) {
            Ok(()) => {
                removed += 1;
                writeln!(
                    out,
                    "removed {}",
                    Escaped(&entry.path.display().to_string())
                )
                .map_err(io)?;
            }
            Err(reason) => failed.push(reason),
        }
    }
    let remaining: Vec<_> = entries.iter().filter(|entry| exists(&entry.path)).collect();
    if remaining.is_empty() {
        writeln!(out, "\nDudes is uninstalled ({removed} items removed).").map_err(io)?;
        leftovers(out).map_err(io)?;
        return Ok(exit::SUCCESS);
    }
    render::local_error(
        err,
        &format!(
            "uninstall did not finish: {removed} removed, {} remain",
            remaining.len()
        ),
    )
    .map_err(io)?;
    for reason in &failed {
        writeln!(err, "  failed: {}", Escaped(reason)).map_err(io)?;
    }
    writeln!(err, "Still present:").map_err(io)?;
    for entry in &remaining {
        writeln!(err, "  {}", Escaped(&entry.path.display().to_string())).map_err(io)?;
    }
    if remaining.iter().any(|entry| entry.label == "program") {
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
