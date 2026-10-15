//! The Floor Git metadata view and the separate bounded review reader in [`diff`].
//!
//! The metadata-view contract below describes [`GitReader::read`]. The review reader owns its
//! baseline metadata, scope samples and bounded source-text responses separately.
//!
//! # The rule
//!
//! The daemon never runs `git` against the agent's own `.git`. Ordinary porcelain `git` in a
//! directory an agent wrote runs agent-planted programs: a filter driver, a file-system monitor, a
//! hook, a textconv or external diff, and a gitfile redirect to another repository, each measured
//! against real git. Hardening flags are a blocklist of a vendor-owned
//! surface, and the clean filter survived the ones tried. So [`GitReader::read`] builds a
//! per-read shadow gitdir the daemon owns (`shadow`), with a literal configuration and copies of
//! the repository's small files, and runs a **fixed** set of read-only commands against it. The
//! agent's `config`, `hooks` and `info` are never opened, so nothing about them needs enumerating.
//!
//! # Two layouts, one per [`Tree`]
//!
//! An agent hired into an ordinary checkout is read from that checkout's own `.git`, which must be
//! a real directory, and compared with the remote default branch. An agent hired into a worktree
//! the daemon created is read through the paths its lease records, never through the link files
//! in the worktree, and compared with the commit the worktree started from. The loop hands the
//! read one [`Tree`], so a read cannot take one layout's files and the other's comparison.
//!
//! # The commands
//!
//! The metadata view uses seven invocations in four steps, and no other. `status` always runs with
//! `--ignore-submodules=all`, **which is a security control**: without it a gitlink entry starts a
//! child git with the nested repository's own configuration. `status` is the only command that
//! compares the working tree, and none may be added without review. Output is parsed as bytes
//! and every string is checked before it can reach the browser (`text`).
//!
//! # What this is not
//!
//! A [`GitReader::read`] result is a live read of a directory the agent writes: never journaled,
//! never in a projection, never evidence. That metadata view never reaches the loop.
//!
//! The read is Unix only. Elsewhere [`GitReader::read`] answers `unsupported_platform` and runs
//! nothing, so the Unix half is gated rather than stubbed.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use dudes_proto::{GitState, GitView};
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_types::{GitCommit, Timestamp, WorktreeId};

use crate::predicate::{PredicateCaptured, PredicateRunner};

pub mod diff;
mod parse;
#[cfg(unix)]
mod shadow;
mod text;
mod view;

use view::{Committed, Facts, assemble};

#[cfg(unix)]
#[doc(hidden)]
pub use shadow::Shadow;

/// The whole read, shadow building included.
pub const READ_DEADLINE: Duration = Duration::from_secs(10);
const COMMAND_DEADLINE: Duration = Duration::from_secs(3);
/// One command's standard output; the child's group is killed past it.
const STDOUT_CAP: usize = 128 * 1024;
/// The oldest git the planted-program matrix was run against: Apple's 2.39.3, the git a Mac with
/// only the command line tools has. Raising the floor to 2.44 for `GIT_NO_LAZY_FETCH` is
/// unnecessary: the shadow's literal configuration names no promisor remote, so no version can fetch lazily from
/// it (`a_partial_clone_does_not_fetch_for_a_missing_object`), and a floor
/// of 2.44 would leave the tab dead on those Macs.
const MIN_VERSION: (u32, u32) = (2, 39);
/// The commit format: id, parents, committer time and subject, tab separated.
const LOG_FORMAT: &str = "--format=%H%x09%P%x09%ct%x09%s";

/// The tree one read opens, as the journal records it. The loop builds it from recorded values and
/// opens nothing; every file system step is the read's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tree {
    /// An agent hired without a worktree: its working directory, whose `.git` must be a directory.
    Checkout(PathBuf),
    /// An agent hired into a worktree the daemon created.
    Leased(Lease),
}

/// What a lease records about its worktree. The daemon wrote these values, so they are trusted as
/// names of where the files should be; nothing found on disk under them is trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    /// The lease, which also names the worktree's administrative directory.
    pub worktree: WorktreeId,
    /// The repository the worktree was added from.
    pub origin: PathBuf,
    /// The worktree.
    pub path: PathBuf,
    /// The commit the worktree was created at.
    pub base: GitCommit,
}

/// The reader. One per daemon, shared by every read.
pub struct GitReader {
    runner: Arc<dyn PredicateRunner>,
    path: Option<OsString>,
    scratch: PathBuf,
    next: AtomicU64,
    /// The canonical `git` that passed the version check, so it runs once per program.
    verified: Mutex<Option<String>>,
}

impl std::fmt::Debug for GitReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitReader").finish_non_exhaustive()
    }
}

impl GitReader {
    /// A reader that finds `git` on `path` and keeps shadows under `scratch`, a directory the
    /// daemon owns and never an agent tree.
    #[must_use]
    pub fn new(runner: Arc<dyn PredicateRunner>, path: Option<OsString>, scratch: PathBuf) -> Self {
        Self {
            runner,
            path,
            scratch,
            next: AtomicU64::new(0),
            verified: Mutex::new(None),
        }
    }

    /// Remove what a crashed run left under the scratch directory. Called once at start, before
    /// any read, so nothing here can be a live shadow.
    pub fn sweep_scratch(&self) {
        let Ok(entries) = std::fs::read_dir(&self.scratch) else {
            return;
        };
        for entry in entries.flatten() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }

    /// Read `tree`. Every outcome is a [`GitView`]: a refusal carries a finite state and no
    /// repository data.
    #[must_use]
    pub fn read(&self, tree: &Tree, now: Timestamp) -> GitView {
        match self.read_tree(tree, now) {
            Ok(view) => view,
            Err(state) => GitView::refused(state, now),
        }
    }

    #[cfg(not(unix))]
    fn read_tree(&self, _tree: &Tree, _now: Timestamp) -> Result<GitView, GitState> {
        let _ = (
            &self.runner,
            &self.path,
            &self.scratch,
            &self.next,
            &self.verified,
        );
        Err(GitState::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn read_tree(&self, tree: &Tree, now: Timestamp) -> Result<GitView, GitState> {
        let deadline = Instant::now() + READ_DEADLINE;
        let (cwd, missing) = match tree {
            Tree::Checkout(cwd) => (cwd, GitState::NotACheckoutRoot),
            Tree::Leased(lease) => (&lease.path, GitState::WorktreeMismatch),
        };
        let work_tree = std::fs::canonicalize(cwd).map_err(|_| missing)?;
        let program = self.program(&work_tree)?;
        let shadow = self.shadow(tree)?;
        let git = Git {
            reader: self,
            program: &program,
            shadow: &shadow,
            deadline,
        };
        git.read(now)
    }

    /// Build a shadow for `tree`.
    ///
    /// # Errors
    ///
    /// The finite state that says why the tree cannot be read.
    #[cfg(unix)]
    #[doc(hidden)]
    pub fn shadow(&self, tree: &Tree) -> Result<Shadow, GitState> {
        let name = format!(
            "{}-{}",
            std::process::id(),
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        Shadow::build(&self.scratch, &name, tree).map_err(GitState::from)
    }

    /// The launch of `git <args>` against `shadow`: the daemon-resolved program, global flags, the
    /// fixed environment.5 and `shadow` as the working directory.
    ///
    /// # Errors
    ///
    /// [`GitState::GitUnavailable`] when no usable `git` resolves.
    #[cfg(unix)]
    #[doc(hidden)]
    pub fn launch(&self, shadow: &Shadow, args: &[&str]) -> Result<LaunchSpec, GitState> {
        let program = self.program(shadow.work_tree())?;
        Ok(launch(self, &program, shadow, args))
    }

    /// The daemon's `git`: resolved by the daemon, canonical, outside the agent's tree, and new
    /// enough. The version is asked once per program.
    #[cfg(unix)]
    fn program(&self, cwd: &Path) -> Result<PathBuf, GitState> {
        self.program_with_timeout(cwd, COMMAND_DEADLINE)
    }

    #[cfg(unix)]
    fn program_with_timeout(&self, cwd: &Path, timeout: Duration) -> Result<PathBuf, GitState> {
        if timeout.is_zero() {
            return Err(GitState::TimedOut);
        }
        let unavailable = |_| GitState::GitUnavailable;
        let found =
            crate::predicate::resolve("git", cwd, self.path.as_deref()).map_err(unavailable)?;
        let tree = std::fs::canonicalize(cwd).map_err(unavailable)?;
        if Path::new(&found).starts_with(&tree) {
            return Err(GitState::GitUnavailable);
        }
        let mut verified = self.verified.lock().unwrap_or_else(PoisonError::into_inner);
        if verified.as_deref() != Some(found.as_str()) {
            self.check_version(&found, timeout.min(COMMAND_DEADLINE))?;
            *verified = Some(found.clone());
        }
        Ok(PathBuf::from(found))
    }

    #[cfg(unix)]
    fn check_version(&self, program: &str, timeout: Duration) -> Result<(), GitState> {
        let spec = LaunchSpec {
            argv: vec![OsString::from(program), OsString::from("--version")],
            env: self.base_env(),
            cwd: std::env::temp_dir(),
            stdin: LaunchInput::Null,
        };
        let Ok(PredicateCaptured::Finished {
            exit: Some(0),
            stdout: output,
            timed_out: false,
            ..
        }) = self.runner.run_captured(&spec, timeout, 256)
        else {
            return Err(GitState::GitUnavailable);
        };
        let text = String::from_utf8_lossy(&output);
        match crate::worktree::git::parse_version(&text) {
            Some(version) if version >= MIN_VERSION => Ok(()),
            _ => Err(GitState::GitUnavailable),
        }
    }

    /// The part of the environment every child shares, less the three paths.
    fn base_env(&self) -> Vec<EnvVar> {
        let mut env = vec![
            EnvVar::new("HOME", "/nonexistent"),
            EnvVar::new("GIT_CONFIG_GLOBAL", "/dev/null"),
            EnvVar::new("GIT_CONFIG_NOSYSTEM", "1"),
            EnvVar::new("GIT_TERMINAL_PROMPT", "0"),
            EnvVar::new("GIT_OPTIONAL_LOCKS", "0"),
            EnvVar::new("GIT_NO_REPLACE_OBJECTS", "1"),
            EnvVar::new("GIT_NO_LAZY_FETCH", "1"),
            EnvVar::new("GIT_PAGER", "cat"),
        ];
        if let Some(path) = &self.path {
            env.push(EnvVar::new("PATH", path.clone()));
        }
        env
    }
}

#[cfg(unix)]
fn launch(reader: &GitReader, program: &Path, shadow: &Shadow, args: &[&str]) -> LaunchSpec {
    let mut env = reader.base_env();
    env.push(EnvVar::new("GIT_DIR", shadow.dir().as_os_str()));
    env.push(EnvVar::new("GIT_WORK_TREE", shadow.work_tree().as_os_str()));
    env.push(EnvVar::new("GIT_INDEX_FILE", shadow.dir().join("index")));
    let mut command = vec![
        program.as_os_str().to_owned(),
        OsString::from("--no-pager"),
        OsString::from("--no-optional-locks"),
    ];
    command.extend(args.iter().map(OsString::from));
    LaunchSpec {
        argv: command,
        env,
        cwd: shadow.dir().to_path_buf(),
        stdin: LaunchInput::Null,
    }
}

#[cfg(unix)]
struct Output {
    exit: Option<i32>,
    stdout: Vec<u8>,
    cut: bool,
}

#[cfg(unix)]
impl Output {
    /// The standard output of a command that exited zero, or that was killed at the cap and so
    /// has no exit code of its own.
    fn ok(&self) -> Option<&[u8]> {
        (self.exit == Some(0) || self.cut).then_some(self.stdout.as_slice())
    }

    /// The 40-digit id a command printed, if it printed one and exited zero.
    fn object_id(&self) -> Option<String> {
        let line = self.ok()?.strip_suffix(b"\n")?;
        parse::is_object_id(line).then(|| String::from_utf8_lossy(line).into_owned())
    }
}

#[cfg(unix)]
struct Git<'a> {
    reader: &'a GitReader,
    program: &'a Path,
    shadow: &'a Shadow,
    deadline: Instant,
}

#[cfg(unix)]
impl Git<'_> {
    fn run(&self, args: &[&str]) -> Result<Output, GitState> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(GitState::TimedOut);
        }
        let spec = launch(self.reader, self.program, self.shadow, args);
        match self
            .reader
            .runner
            .run_captured(&spec, left.min(COMMAND_DEADLINE), STDOUT_CAP)
        {
            Ok(PredicateCaptured::Finished {
                timed_out: true, ..
            }) => Err(GitState::TimedOut),
            Ok(PredicateCaptured::Finished {
                exit,
                stdout,
                truncated,
                ..
            }) => Ok(Output {
                exit,
                stdout,
                cut: truncated,
            }),
            Ok(PredicateCaptured::Swept) => Err(GitState::Unreadable),
            Err(_) => Err(GitState::GitUnavailable),
        }
    }

    fn read(&self, now: Timestamp) -> Result<GitView, GitState> {
        // Step 1: the branches, and the comparison commit.
        let refs = self.run(&[
            "for-each-ref",
            "--format=%(objectname)%00%(refname)",
            "refs/heads",
        ])?;
        let tips = parse::branches(refs.ok().ok_or(GitState::Unreadable)?, refs.cut);
        let comparison = self.shadow.comparison();
        let base = self
            .run(&["rev-parse", "--verify", "--quiet", &comparison.revision()])?
            .object_id();

        // Step 2: how far the agent is from the base, and its own commits.
        let counts = match &base {
            Some(base) => self
                .run(&[
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("{base}...HEAD"),
                ])?
                .ok()
                .and_then(parse::counts),
            None => None,
        };
        let range = base.as_ref().map(|base| format!("{base}..HEAD"));
        let (count, target) = match &range {
            Some(range) => ("-20", range.as_str()),
            None => ("-10", "HEAD"),
        };
        let log = self.run(&[
            "log",
            count,
            "-z",
            "--no-show-signature",
            "--parents",
            LOG_FORMAT,
            target,
        ])?;
        let raw_commits = log
            .ok()
            .map(|out| parse::commits(out, log.cut))
            .unwrap_or_default();

        // Step 3: the files those commits changed.
        let committed = match &base {
            Some(base) => match self.run(&["merge-base", base, "HEAD"])?.object_id() {
                Some(merge_base) => {
                    let diff = self.run(&[
                        "diff-tree",
                        "-r",
                        "--raw",
                        "-z",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--no-renames",
                        &merge_base,
                        "HEAD",
                    ])?;
                    diff.ok().map(|out| Committed {
                        files: parse::committed(out, diff.cut),
                        cut: diff.cut,
                    })
                }
                None => None,
            },
            None => None,
        };

        // Step 4: the working tree. `--ignore-submodules=all` is a security control, see the
        // module docs.
        let status = self.run(&["status", "--porcelain=v1", "-z", "--ignore-submodules=all"])?;
        let entries = parse::status(status.ok().ok_or(GitState::Unreadable)?, status.cut);

        Ok(assemble(&Facts {
            head: self.shadow.head(),
            now,
            tips: &tips,
            against: comparison.against(),
            counts,
            commits: &raw_commits,
            log_cut: log.cut,
            committed,
            entries: &entries,
            status_cut: status.cut,
        }))
    }
}
