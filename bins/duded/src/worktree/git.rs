//! The daemon's `git` child for worktree creation and cleanup.
//!
//! Every call is one typed argv for one owned child: the program is the absolute file the daemon's
//! own `PATH` lookup found, and the environment is a short fixed list. The child runs through the
//! daemon's one-shot runner, so it has a deadline, lives in a process group and is swept when the
//! daemon exits. Nothing here runs on the loop thread.
//!
//! What this prevents, measured against real git:
//!
//! - user and system configuration is not read (`GIT_CONFIG_GLOBAL`, `GIT_CONFIG_NOSYSTEM`);
//! - hook scripts do not run (`-c core.hooksPath=/dev/null`), and neither does the file-system
//!   monitor (`-c core.fsmonitor=false`);
//! - a partial clone does not fetch from its remote behind the daemon's back (`GIT_NO_LAZY_FETCH`);
//! - git is never prompted for a credential (`GIT_TERMINAL_PROMPT`).
//!
//! What it does not prevent:
//!
//! - **Hooks configured as `hook.<name>.command` still run, whatever `core.hooksPath` says.** The
//!   worktree hire therefore refuses a repository whose configuration has any `hook.*` key, which
//!   [`Git::has_hook_config`] reads without running anything.
//! - Filters and an LFS filter configured in the repository itself still run when a tree is
//!   checked out, which is why a worktree hire requires the operator's trust declaration. A filter
//!   installed only in the operator's global configuration does not run, so an LFS repository gets
//!   pointer files in its tree.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_types::GitCommit;

use crate::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner};

/// The oldest git with `git worktree remove`.
const MIN_VERSION: (u32, u32) = (2, 17);
/// A read-only question: where a repository is, what a name resolves to.
const QUICK: Duration = Duration::from_secs(20);
/// How long `git worktree add` may take, which checks a whole tree out.
pub const CHECKOUT_DEADLINE: Duration = Duration::from_secs(300);
/// Removing a tree and its branch.
const CLEANUP: Duration = Duration::from_secs(60);

/// Why a git call did not produce an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitError {
    /// No usable git: not on the daemon's `PATH`, or older than `git worktree remove`.
    Unavailable(String),
    /// Git ran and refused, or its answer was not the shape the call needs.
    Failed(String),
    /// The deadline killed it.
    TimedOut,
    /// The daemon is exiting.
    Swept,
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(text) | Self::Failed(text) => f.write_str(text),
            Self::TimedOut => f.write_str("git did not finish within its deadline"),
            Self::Swept => f.write_str("the daemon is exiting"),
        }
    }
}

impl std::error::Error for GitError {}

/// The daemon's git, ready to run.
#[derive(Clone)]
pub struct Git {
    runner: Arc<dyn PredicateRunner>,
    path: Option<OsString>,
    checkout: Duration,
}

impl std::fmt::Debug for Git {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Git").finish_non_exhaustive()
    }
}

impl Git {
    /// A git that finds its program on `path` and lets `git worktree add` run for `checkout`.
    #[must_use]
    pub fn new(
        runner: Arc<dyn PredicateRunner>,
        path: Option<OsString>,
        checkout: Duration,
    ) -> Self {
        Self {
            runner,
            path,
            checkout,
        }
    }

    fn program(&self) -> Result<PathBuf, GitError> {
        crate::program::resolve(OsStr::new("git"), self.path.as_deref())
            .map_err(|_| GitError::Unavailable("git is not on the daemon's PATH".to_owned()))
    }

    /// Run `git <args>` in `cwd` and return its trimmed output.
    ///
    /// # Errors
    ///
    /// [`GitError`] for a missing git, a nonzero exit, a deadline or a daemon exit.
    pub fn exec(
        &self,
        cwd: &Path,
        args: &[&OsStr],
        deadline: Duration,
    ) -> Result<String, GitError> {
        match self.status(cwd, args, deadline)? {
            (Some(0), text) => Ok(text),
            (exit, text) => Err(GitError::Failed(format!(
                "git exited with {}: {}",
                exit.map_or_else(|| "a signal".to_owned(), |code| code.to_string()),
                last_lines(&text)
            ))),
        }
    }

    /// Run `git <args>` in `cwd` and return its exit code and trimmed output, whatever the code.
    ///
    /// # Errors
    ///
    /// [`GitError`] for a missing git, a deadline or a daemon exit.
    pub fn status(
        &self,
        cwd: &Path,
        args: &[&OsStr],
        deadline: Duration,
    ) -> Result<(Option<i32>, String), GitError> {
        let spec = self.spec(cwd, args)?;
        match self.runner.run(&spec, deadline) {
            Ok(PredicateOutcome::Finished {
                timed_out: true, ..
            }) => Err(GitError::TimedOut),
            Ok(PredicateOutcome::Finished {
                exit, output_tail, ..
            }) => Ok((exit, output_tail.trim().to_owned())),
            Ok(PredicateOutcome::Swept) => Err(GitError::Swept),
            Err(error) => Err(GitError::Failed(format!("git could not start: {error}"))),
        }
    }

    /// The launch of `git <args>` in `cwd`: no hooks, no filesystem monitor, no system or user
    /// configuration, and no prompt.
    fn spec(&self, cwd: &Path, args: &[&OsStr]) -> Result<LaunchSpec, GitError> {
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let mut command: Vec<OsString> = vec![
            self.program()?.into_os_string(),
            OsString::from("-c"),
            OsString::from(format!("core.hooksPath={null}")),
            OsString::from("-c"),
            OsString::from("core.fsmonitor=false"),
        ];
        command.extend(args.iter().map(|arg| (*arg).to_owned()));
        let mut env = vec![
            EnvVar::new("GIT_CONFIG_NOSYSTEM", "1"),
            EnvVar::new("GIT_CONFIG_GLOBAL", null),
            EnvVar::new("GIT_NO_LAZY_FETCH", "1"),
            EnvVar::new("GIT_TERMINAL_PROMPT", "0"),
            EnvVar::new("GIT_OPTIONAL_LOCKS", "0"),
            EnvVar::new("LC_ALL", "C"),
        ];
        if let Some(path) = &self.path {
            env.push(EnvVar::new("PATH", path.clone()));
        }
        Ok(LaunchSpec {
            argv: command,
            env,
            cwd: cwd.to_path_buf(),
            stdin: LaunchInput::Null,
        })
    }

    /// Run `git <args>` in `cwd` and return its exit code and the first `cap` bytes of its
    /// standard output, untrimmed, and whether it wrote more.
    ///
    /// # Errors
    ///
    /// [`GitError`] for a missing git, a deadline, a daemon exit, or a runner that cannot capture.
    pub fn capture(
        &self,
        cwd: &Path,
        args: &[&OsStr],
        deadline: Duration,
        cap: usize,
    ) -> Result<(Option<i32>, Vec<u8>, bool), GitError> {
        let spec = self.spec(cwd, args)?;
        match self.runner.run_captured(&spec, deadline, cap) {
            Ok(PredicateCaptured::Finished {
                timed_out: true, ..
            }) => Err(GitError::TimedOut),
            Ok(PredicateCaptured::Finished {
                exit,
                stdout,
                truncated,
                ..
            }) => Ok((exit, stdout, truncated)),
            Ok(PredicateCaptured::Swept) => Err(GitError::Swept),
            Err(error) => Err(GitError::Failed(format!("git could not start: {error}"))),
        }
    }

    /// Whether the repository at `origin` configures any `hook.*` key, which git runs as a hook
    /// whatever `core.hooksPath` says. Reads configuration and runs nothing.
    ///
    /// # Errors
    ///
    /// [`GitError`] when git could not answer.
    pub fn has_hook_config(&self, origin: &Path) -> Result<bool, GitError> {
        match self.status(
            origin,
            &[
                OsStr::new("config"),
                OsStr::new("--get-regexp"),
                OsStr::new("^hook\\."),
            ],
            QUICK,
        )? {
            (Some(0), _) => Ok(true),
            (Some(1), _) => Ok(false),
            (exit, text) => Err(GitError::Failed(format!(
                "git config exited with {exit:?}: {}",
                last_lines(&text)
            ))),
        }
    }

    /// Whether this git has `git worktree remove`.
    ///
    /// # Errors
    ///
    /// [`GitError::Unavailable`] when git is missing or older than 2.17.
    pub fn check_version(&self) -> Result<(), GitError> {
        let cwd = std::env::temp_dir();
        let text = self.exec(&cwd, &[OsStr::new("--version")], QUICK)?;
        let version = parse_version(&text)
            .ok_or_else(|| GitError::Unavailable(format!("unreadable git version: {text}")))?;
        if version < MIN_VERSION {
            return Err(GitError::Unavailable(format!(
                "git {}.{} is older than 2.17, which added `git worktree remove`",
                version.0, version.1
            )));
        }
        Ok(())
    }

    /// The top directory of the repository `dir` is in.
    ///
    /// # Errors
    ///
    /// [`GitError`], and [`GitError::Failed`] when `dir` is in no repository.
    pub fn toplevel(&self, dir: &Path) -> Result<PathBuf, GitError> {
        let out = self.exec(
            dir,
            &[OsStr::new("rev-parse"), OsStr::new("--show-toplevel")],
            QUICK,
        )?;
        let line = last_line(&out);
        let top = PathBuf::from(line);
        if top.is_absolute() {
            Ok(top)
        } else {
            Err(GitError::Failed(format!(
                "`git rev-parse --show-toplevel` answered `{line}`"
            )))
        }
    }

    /// The commit `rev` names in the repository at `origin`, resolved once.
    ///
    /// # Errors
    ///
    /// [`GitError`], and [`GitError::Failed`] when `rev` names no commit.
    pub fn commit(&self, origin: &Path, rev: &str) -> Result<GitCommit, GitError> {
        let spec = format!("{rev}^{{commit}}");
        let out = self.exec(
            origin,
            &[
                OsStr::new("rev-parse"),
                OsStr::new("--verify"),
                OsStr::new("--quiet"),
                OsStr::new(&spec),
            ],
            QUICK,
        )?;
        GitCommit::parse(last_line(&out))
            .map_err(|_| GitError::Failed(format!("`{rev}` did not resolve to a commit")))
    }

    /// Create `path` as a worktree of `origin` on the new branch `branch` at `base`.
    ///
    /// # Errors
    ///
    /// [`GitError`]; after any error a half-made tree may exist, which [`Git::discard`] removes.
    pub fn add(
        &self,
        origin: &Path,
        path: &Path,
        branch: &str,
        base: GitCommit,
    ) -> Result<(), GitError> {
        let base = base.to_string();
        self.exec(
            origin,
            &[
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--quiet"),
                OsStr::new("-b"),
                OsStr::new(branch),
                OsStr::new("--"),
                path.as_os_str(),
                OsStr::new(&base),
            ],
            self.checkout,
        )
        .map(|_| ())
    }

    /// Remove a tree no agent ever ran in, and its branch. The double force is needed because a
    /// deadline kill leaves the tree locked as initializing, which a single force refuses.
    ///
    /// Each step runs even when an earlier one failed, because the tree may already be gone and the
    /// branch may never have been made. The caller decides from the file system whether it worked.
    pub fn discard(&self, origin: &Path, path: &Path, branch: &str) {
        let _ = self.exec(
            origin,
            &[
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("-f"),
                OsStr::new("-f"),
                OsStr::new("--"),
                path.as_os_str(),
            ],
            CLEANUP,
        );
        let _ = self.exec(
            origin,
            &[
                OsStr::new("branch"),
                OsStr::new("-D"),
                OsStr::new("--"),
                OsStr::new(branch),
            ],
            CLEANUP,
        );
    }
}

/// The `(major, minor)` of the text `git --version` prints, `git version 2.54.0`.
pub(crate) fn parse_version(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.split_whitespace().nth(2)?.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

fn last_line(text: &str) -> &str {
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map_or("", str::trim)
}

fn last_lines(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail = &lines[lines.len().saturating_sub(3)..];
    tail.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_major_and_minor_of_the_third_word() {
        assert_eq!(parse_version("git version 2.54.0"), Some((2, 54)));
        assert_eq!(
            parse_version("git version 2.39.5 (Apple Git-154)"),
            Some((2, 39))
        );
        assert_eq!(parse_version("git version x.1"), None);
        assert_eq!(parse_version("2.54.0"), None);
    }

    #[test]
    fn the_last_non_empty_line_is_the_answer() {
        assert_eq!(last_line("warning: x\nabc\n\n"), "abc");
        assert_eq!(last_line(""), "");
        assert_eq!(last_lines("a\n\nb\nc\nd\n"), "b | c | d");
    }
}
