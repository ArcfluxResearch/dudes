//! The predicate runner seam, the daemon-owned environment, and the resolution that happens here
//! rather than in a shell.
//!
//! # The seam
//!
//! [`PredicateRunner`] is this package's view of the one-shot runner: `&self`,
//! `Send + Sync`, `run` returning only after the child has been reaped, and a `sweep` the exit path
//! calls. The implementation is `dudes_host::OneShot` and the `impl` is in `main.rs` — the one file
//! in this package that names that crate, which is what `tests/host_seam.rs` pins. Declaring the
//! trait here rather than depending on the concrete type is what keeps the rest of the daemon
//! generic over *both* child owners, the process host and this one.
//!
//! # What this module does that the runner must not
//!
//! Invariant 7 says PATH lookup happens in the daemon, and the check is on the
//! **resolved, canonicalized program** — because a rule about `PATH` entries is a property of
//! strings while what executes is a property of the filesystem, and they come apart at a symlink.
//! So [`resolve`] does the lookup and canonicalizes it; [`spec_for`] canonicalizes the agent tree,
//! refuses a program contained by it, and only then hands the runner the same absolute program.
//! Nothing is re-resolved between the check and the exec.
//!
//! # The environment is an allowlist, and it is handed in
//!
//! A predicate child is not an agent child, so its environment carries neither
//! `DUDES_SOCK` nor `DUDES_TOKEN` — the process that judges an agent is not that agent, and code
//! the agent authored must not be able to speak as it. The environment is a daemon-owned
//! allowlist rather than a filtered inheritance, because an allowlist that is too small produces a
//! predicate that *fails*, and a failing predicate is not `Done`, while an inheritance that is too
//! wide produces one that passes while carrying a credential into attacker-authored code.
//!
//! [`allowlisted`] takes the environment as an argument and reads none: the daemon is handed what
//! it may use. `main.rs` is where the ambient environment is read, once.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::time::Duration;

use dudes_proto::goal::Evidence;
use dudes_providers::launch::{EnvVar, LaunchSpec};

/// What one predicate run produced, as this package sees it.
///
/// Mirrors `dudes_host::OneShotOutcome`, including the property that matters: a run the exit path
/// cut short is a variant carrying **no evidence**, so [`judge`] cannot be handed one and a
/// fabricated `Provenance::Observed` verdict is unrepresentable rather than merely discouraged
/// (invariant 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PredicateOutcome {
    /// The child ran to an end of its own, or to its timeout, and was reaped.
    Finished {
        /// The exit code; `None` when the child was killed by a signal or by the timeout.
        exit: Option<i32>,
        /// The last few KiB of stdout and stderr, interleaved as captured.
        output_tail: String,
        /// Whether the run hit its timeout rather than ending on its own.
        timed_out: bool,
    },
    /// The exit path swept this run. Judges nothing.
    Swept,
}

/// The bounded raw standard-output head of one owned captured child.
///
/// Mirrors `dudes_host::CapturedOutcome`. Display and maintenance consumers validate these bytes;
/// [`judge`] cannot take this result as predicate evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PredicateCaptured {
    /// The child ended, timed out, or was killed at the output cap, and was reaped.
    Finished {
        /// The exit code; `None` when a signal, the timeout or the cap ended it.
        exit: Option<i32>,
        /// The first bytes of standard output, at most the cap.
        stdout: Vec<u8>,
        /// Whether the child wrote more than the cap.
        truncated: bool,
        /// Whether the run hit its timeout rather than ending on its own.
        timed_out: bool,
    },
    /// The exit path swept this run.
    Swept,
}

/// The daemon's side of the one-shot runner.
///
/// `Send + Sync` and `&self` throughout, so the predicate thread and the exit path hold one
/// through an `Arc` at the same time. That is how the exit path sweeps without waiting on the predicate thread; a `&mut self` API would put
/// that stall on the exit path instead.
pub trait PredicateRunner: Send + Sync + 'static {
    /// Run `spec` to an end, bounded by `timeout`, killing its process group or job and reaping it
    /// before returning — pass, fail or timeout alike.
    ///
    /// # Errors
    ///
    /// Whatever the platform reports for building the command, taking the hold, or the exec.
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<PredicateOutcome>;

    /// [`PredicateRunner::run`] with standard output and error on the null device, for a
    /// diagnostic child whose exit status is the whole answer. Its `output_tail` is
    /// empty, and [`PredicateRunner::sweep`] reaches it like any other run.
    ///
    /// # Errors
    ///
    /// As `run`. A runner that starts no diagnostic child answers
    /// [`io::ErrorKind::Unsupported`], which is the default.
    fn run_discarding_output(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
    ) -> io::Result<PredicateOutcome> {
        let _ = (spec, timeout);
        Err(io::ErrorKind::Unsupported.into())
    }

    /// Run `spec` as [`PredicateRunner::run`] does, but keep the first `stdout_cap` bytes of
    /// standard output and send standard error to the null device. The child's
    /// group is killed once it writes past the cap.
    ///
    /// # Errors
    ///
    /// As `run`. A runner that starts no captured child answers [`io::ErrorKind::Unsupported`],
    /// which is the default.
    fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        stdout_cap: usize,
    ) -> io::Result<PredicateCaptured> {
        let _ = (spec, timeout, stdout_cap);
        Err(io::ErrorKind::Unsupported.into())
    }

    /// [`PredicateRunner::run_captured`] for a child that exits when its input ends: the input is
    /// written and left open, and the child's group is ended once `answered` holds for the output
    /// so far, rather than at its end.
    ///
    /// # Errors
    ///
    /// As [`PredicateRunner::run_captured`].
    fn run_captured_until(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        stdout_cap: usize,
        answered: fn(&[u8]) -> bool,
    ) -> io::Result<PredicateCaptured> {
        let _ = (spec, timeout, stdout_cap, answered);
        Err(io::ErrorKind::Unsupported.into())
    }

    /// [`PredicateRunner::run`] that also ends early once `cancel` is set, killing and reaping the
    /// child and answering [`PredicateOutcome::Swept`]. A runner that cannot end one run early
    /// runs it to its end, which is the default.
    ///
    /// # Errors
    ///
    /// As `run`.
    fn run_cancellable(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> io::Result<PredicateOutcome> {
        let _ = cancel;
        self.run(spec, timeout)
    }

    /// Kill and reap every in-flight predicate. Idempotent, and safe to call while a
    /// [`PredicateRunner::run`] is in flight. A run that starts afterwards starts no child and
    /// answers [`PredicateOutcome::Swept`].
    fn sweep(&self);
}

pub use dudes_providers::launch::PREDICATE_ENV_ALLOWLIST;

/// The allowlisted subset of `vars`, in [`PREDICATE_ENV_ALLOWLIST`] order.
///
/// Takes the environment rather than reading one, which is what makes the rule testable without a
/// process-wide mutation and what keeps every ambient read in `main.rs`.
#[must_use]
pub fn allowlisted(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<EnvVar> {
    let present: Vec<(OsString, OsString)> = vars.into_iter().collect();
    PREDICATE_ENV_ALLOWLIST
        .iter()
        .filter_map(|name| {
            present
                .iter()
                .find(|(have, _)| have == OsStr::new(name))
                .map(|(name, value)| EnvVar::new(name.clone(), value.clone()))
        })
        .collect()
}

/// Resolve `argv0` to an absolute, canonical program, exactly as the table says.
///
/// Three shapes, not two: a **bare** name resolves against the daemon-owned `PATH` and `cwd` does
/// not decide it; a **relative** name with a separator resolves against `cwd`, as the operator
/// wrote it; an **absolute** name resolves as written. All three are canonicalized here, then
/// [`spec_for`] applies the same canonical agent-tree refusal to the result.
///
/// `path` is the daemon's `PATH`, handed in — never read from this process's environment, because
/// the value a predicate is resolved against must be the one the daemon chose rather than whatever
/// the operator's shell happened to export into the daemon.
///
/// # Errors
///
/// [`io::ErrorKind::NotFound`] when nothing resolves, and [`io::ErrorKind::InvalidData`] when the
/// canonical path is not valid UTF-8 — refused rather than lossily encoded,
/// because `Evidence::Command.program` is a `String` on the wire and a lossy one would name a
/// program that was never run.
pub fn resolve(argv0: &str, cwd: &Path, path: Option<&OsStr>) -> io::Result<String> {
    let candidate = Path::new(argv0);
    let found = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else if candidate.components().count() > 1 {
        cwd.join(candidate)
    } else {
        crate::program::on_path(OsStr::new(argv0), path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("`{argv0}` is not on the daemon's PATH"),
            )
        })?
    };
    let canonical = found
        .canonicalize()
        .map_err(|cause| io::Error::new(cause.kind(), format!("`{}`: {cause}", found.display())))?;
    canonical.to_str().map(ToOwned::to_owned).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the canonical path of `{argv0}` is not valid UTF-8 and is refused rather than \
                 lossily encoded"
            ),
        )
    })
}

/// The launch for one command predicate: the resolved program, the operator's arguments after it,
/// the daemon-owned environment, and the agent's working directory.
///
/// `argv[0]` is replaced by the resolved program and every later element is kept exactly as the
/// operator wrote it — typed parts, nothing interpolated, no shell (invariant 7).
///
/// # Errors
///
/// As [`resolve`], plus [`io::ErrorKind::InvalidInput`] for an empty `argv` and
/// [`io::ErrorKind::PermissionDenied`] when the canonical program is inside the canonical agent
/// tree. Both sides are canonicalized before the component-wise comparison.
pub fn spec_for(
    argv: &[String],
    cwd: &Path,
    env: &[EnvVar],
    path: Option<&OsStr>,
) -> io::Result<(String, LaunchSpec)> {
    spec_for_repositories(argv, cwd, env, path, &[])
}

/// Build a command check whose program must be outside every recorded hired repository.
/// The working directory selects the check target independently of this denial set.
///
/// # Errors
/// As [`spec_for`], including a missing or unresolvable repository root.
pub fn spec_for_repositories(
    argv: &[String],
    cwd: &Path,
    env: &[EnvVar],
    path: Option<&OsStr>,
    repositories: &[std::path::PathBuf],
) -> io::Result<(String, LaunchSpec)> {
    let (first, rest) = argv.split_first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a command predicate with an empty argv has nothing to run",
        )
    })?;
    let program = resolve(first, cwd, path)?;
    for root in std::iter::once(cwd).chain(repositories.iter().map(std::path::PathBuf::as_path)) {
        let canonical_tree = root.canonicalize().map_err(|cause| {
            io::Error::new(
                cause.kind(),
                format!("the agent directory `{}`: {cause}", root.display()),
            )
        })?;
        if Path::new(&program).starts_with(&canonical_tree) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "the canonical predicate program `{program}` is inside the agent tree `{}` and \
                 is refused before spawn",
                    canonical_tree.display()
                ),
            ));
        }
    }
    let mut launch_argv = Vec::with_capacity(argv.len());
    launch_argv.push(OsString::from(&program));
    launch_argv.extend(rest.iter().map(OsString::from));
    Ok((
        program,
        LaunchSpec {
            argv: launch_argv,
            env: env.to_vec(),
            cwd: cwd.to_path_buf(),
            stdin: dudes_providers::launch::LaunchInput::Null,
        },
    ))
}

/// Turn one finished run into the verdict and the evidence the loop journals.
///
/// `None` for a swept run: a swept run journals nothing, and the type is what enforces it — there is no
/// `PredicateOutcome::Swept` arm carrying an exit code to be mistaken for one.
///
/// A predicate holds when its command exits `0`. A timeout and a signal both leave `exit` at `None`
/// and therefore both fail, which is the direction invariant 3 wants: `Done` is reachable only from
/// a verdict the daemon positively observed.
#[must_use]
pub fn judge(
    outcome: &PredicateOutcome,
    argv: &[String],
    program: Option<String>,
) -> Option<(bool, Evidence)> {
    match outcome {
        PredicateOutcome::Swept => None,
        PredicateOutcome::Finished {
            exit,
            output_tail,
            timed_out,
        } => Some((
            !*timed_out && *exit == Some(0),
            Evidence::Command {
                argv: argv.to_vec(),
                program,
                exit: *exit,
                output_tail: output_tail.clone(),
            },
        )),
    }
}

/// The evidence for a predicate whose launch spec was refused, so nothing ran.
///
/// A refusal is a genuine `Observed` `pass: false`: the daemon resolved as far as its trust
/// boundary permits and observed that it must not spawn. `program` stays absent because
/// it is defined as the program that was spawned, and none was.
#[must_use]
pub fn refused(argv: &[String], error: &io::Error) -> (bool, Evidence) {
    (
        false,
        Evidence::Command {
            argv: argv.to_vec(),
            program: None,
            exit: None,
            output_tail: format!("the predicate did not run: {error}"),
        },
    )
}

/// Query every declared directory for the default change check under one timeout.
/// An unavailable query leaves the aggregate unobserved.
///
/// # Errors
/// Refuses an empty declaration, an unresolved program or a program inside any hired root.
pub fn files_changed(
    runner: &dyn PredicateRunner,
    argv: &[String],
    directories: &[std::path::PathBuf],
    env: &[EnvVar],
    path: Option<&OsStr>,
    hired_roots: &[std::path::PathBuf],
    timeout: Duration,
) -> io::Result<Option<(bool, Evidence)>> {
    files_changed_or_committed(
        runner,
        argv,
        directories,
        env,
        path,
        hired_roots,
        timeout,
        None,
    )
}

/// What an unconfined hire adds to [`files_changed`]: for the declared directory at an index, whether
/// its checkout holds a commit that changes a path since the round's baseline. `None` is an
/// unanswered query.
pub type CommittedSince<'a> = &'a dyn Fn(usize) -> Option<bool>;

/// [`files_changed`], where `committed` also counts work an agent committed. A directory with no
/// uncommitted change is asked; one with an uncommitted change already passes and is not.
///
/// # Errors
/// As [`files_changed`].
#[expect(
    clippy::too_many_arguments,
    reason = "files_changed's arguments plus the one query only an unconfined hire adds"
)]
pub fn files_changed_or_committed(
    runner: &dyn PredicateRunner,
    argv: &[String],
    directories: &[std::path::PathBuf],
    env: &[EnvVar],
    path: Option<&OsStr>,
    hired_roots: &[std::path::PathBuf],
    timeout: Duration,
    committed: Option<CommittedSince<'_>>,
) -> io::Result<Option<(bool, Evidence)>> {
    if directories.is_empty() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let started = std::time::Instant::now();
    let mut changed = 0;
    let mut complete = true;
    let mut program = None;
    for (index, directory) in directories.iter().enumerate() {
        let (resolved, spec) = spec_for_repositories(argv, directory, env, path, hired_roots)?;
        if program.as_ref().is_some_and(|program| program != &resolved) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        program = Some(resolved);
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(None);
        }
        match runner.run(&spec, remaining) {
            Ok(PredicateOutcome::Finished {
                exit: Some(exit),
                timed_out: false,
                ..
            }) if exit == 0 || exit == 1 => {
                if exit == 0 {
                    changed += 1;
                } else if let Some(committed) = committed {
                    match committed(index) {
                        Some(true) => changed += 1,
                        Some(false) => {}
                        None => complete = false,
                    }
                }
            }
            Ok(PredicateOutcome::Swept) => return Ok(None),
            Ok(_) | Err(_) => complete = false,
        }
    }
    Ok(complete.then(|| {
        let pass = changed > 0;
        (
            pass,
            Evidence::Command {
                argv: argv.to_vec(),
                program,
                exit: Some(i32::from(!pass)),
                output_tail: format!(
                    "Queried all {} declared repositories; {changed} contain {}.",
                    directories.len(),
                    if committed.is_some() {
                        "uncommitted changes or commits since the round baseline"
                    } else {
                        "uncommitted changes"
                    }
                ),
            },
        )
    }))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::path::PathBuf;

    use super::*;

    /// The allowlist admits what it names and nothing else, and it reads no ambient
    /// environment. Both arms — a named variable present, an unnamed one absent — because "keeps
    /// everything" and "keeps nothing" are both wrong.
    #[test]
    fn predicate_env_drops_a_vendor_credential() {
        let handed = vec![
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (OsString::from("ANTHROPIC_API_KEY"), OsString::from("sk-x")),
            (OsString::from("DUDES_TOKEN"), OsString::from("secret")),
            (OsString::from("NODE_OPTIONS"), OsString::from("--require")),
            (OsString::from("HOME"), OsString::from("/home/hector")),
        ];
        let env = allowlisted(handed);
        let names: Vec<String> = env
            .iter()
            .map(|var| var.name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["PATH", "HOME"],
            "control: the two allowlisted names are kept, in the list's order"
        );
        for barred in ["ANTHROPIC_API_KEY", "DUDES_TOKEN", "NODE_OPTIONS"] {
            assert!(
                !names.iter().any(|name| name == barred),
                "{barred} reached a predicate child"
            );
        }
    }

    /// The no-agent-credentials guarantee as a check rather than as prose: whatever the daemon's own environment holds,
    /// nothing beginning `DUDES_` can reach a predicate child, because the allowlist names none.
    #[test]
    fn predicate_env_has_no_dudes_key() {
        assert!(
            !PREDICATE_ENV_ALLOWLIST
                .iter()
                .any(|name| name.starts_with("DUDES_")),
            "a predicate child carries neither DUDES_SOCK nor DUDES_TOKEN"
        );
    }

    /// The fixed allowlist is an exact contract, not a subset check: adding or removing even one
    /// key must fail here. That keeps a future credential or interpreter hook from joining the
    /// child environment as an unnoticed "platform convenience".
    #[test]
    fn predicate_env_is_exactly_the_allowlist() {
        assert_eq!(
            PREDICATE_ENV_ALLOWLIST,
            [
                "PATH",
                "HOME",
                "TMPDIR",
                "LANG",
                "USERPROFILE",
                "TEMP",
                "TMP",
            ],
            "the key set is fixed; a subset assertion would miss growth"
        );
    }

    /// The file a bare `checker` resolves to. Windows looks a name without a `.` up as an `.exe`.
    const CHECKER: &str = if cfg!(windows) {
        "checker.exe"
    } else {
        "checker"
    };

    /// A script at `path` that the daemon may run, as a bare name is only found when it is.
    fn write_runnable(path: &Path) {
        std::fs::write(path, "#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
                .expect("make it executable");
        }
    }

    /// The three program shapes. The bare form goes to `PATH` and is *not* decided by `cwd` — the control
    /// is a decoy of the same name sitting in `cwd`, which a resolution that consulted `cwd` would
    /// pick up.
    #[test]
    fn a_bare_name_resolves_against_the_daemons_path_and_not_against_cwd() {
        let bin = tempfile::tempdir().expect("tempdir");
        let cwd = tempfile::tempdir().expect("tempdir");
        let real = bin.path().join(CHECKER);
        write_runnable(&real);
        write_runnable(&cwd.path().join(CHECKER));

        let path = OsString::from(bin.path());
        let resolved = resolve("checker", cwd.path(), Some(&path)).expect("it resolves");
        assert_eq!(
            resolved,
            real.canonicalize().expect("canonical").to_string_lossy(),
            "the PATH entry won, and the answer is canonical"
        );
    }

    /// The other half of program resolution: a **relative** `PATH` entry is skipped, never joined to the directory the
    /// daemon happens to be running in. `Predicate::Command`'s doc comment ships that promise in
    /// `schemas/journal-record.schema.json` — *"never a relative entry"* — and a promise the code
    /// keeps only by habit is not kept.
    ///
    /// The two arms name the same file through the same directory and differ in exactly one thing,
    /// the entry's shape. That directory sits in the one the test process runs in:
    /// [`crate::program::on_path`] asks whether a candidate is a runnable file, which resolves a
    /// relative candidate against *that* directory, so anchoring the fixture there is the property
    /// rather than a convenience. The absolute arm is a true control — it proves the relative join
    /// would have found something.
    #[test]
    fn a_relative_path_entry_is_skipped_and_not_joined_to_the_daemons_directory() {
        let here = std::env::current_dir().expect("the directory the test process runs in");
        let bin = tempfile::tempdir_in(&here).expect("a directory beside the test process");
        let cwd = tempfile::tempdir().expect("tempdir");
        write_runnable(&bin.path().join(CHECKER));

        let absolute = OsString::from(bin.path());
        let resolved = resolve("checker", cwd.path(), Some(&absolute)).expect("it resolves");
        assert_eq!(
            resolved,
            bin.path()
                .join(CHECKER)
                .canonicalize()
                .expect("canonical")
                .to_string_lossy(),
            "control: the same entry made absolute does hold this name, so the arm below fails for \
             the entry's shape and for nothing else"
        );

        let relative = OsString::from(bin.path().file_name().expect("a named directory"));
        let error = resolve("checker", cwd.path(), Some(&relative))
            .expect_err("a relative PATH entry is not a place the daemon looks");
        assert_eq!(
            error.kind(),
            io::ErrorKind::NotFound,
            "the relative entry was skipped rather than joined to the daemon's own directory"
        );
    }

    /// The relative-with-a-separator form is the one `cwd` decides during resolution. This test
    /// pins only that lookup rule; [`spec_for`] separately refuses any resulting canonical program
    /// inside the agent tree.
    #[test]
    fn a_relative_name_with_a_separator_resolves_against_cwd() {
        let cwd = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(cwd.path().join("scripts")).expect("mkdir");
        let script = cwd.path().join("scripts/check.sh");
        std::fs::write(&script, "#!/bin/sh\n").expect("write");

        let resolved = resolve("./scripts/check.sh", cwd.path(), None).expect("it resolves");
        assert_eq!(
            resolved,
            script.canonicalize().expect("canonical").to_string_lossy()
        );
    }

    /// Program resolution at the filesystem seam: a daemon-owned `PATH` entry can look external while the bare
    /// program inside it is a symlink into the canonicalized agent tree. The same-shape external
    /// symlink is the positive control; only the target's containment may distinguish the arms.
    #[cfg(unix)]
    #[test]
    fn predicate_program_symlinked_into_the_tree_is_refused() {
        let tree = tempfile::tempdir().expect("agent tree");
        let inside_bin = tempfile::tempdir().expect("inside PATH entry");
        let outside_bin = tempfile::tempdir().expect("outside PATH entry");
        let outside_tree = tempfile::tempdir().expect("external target tree");

        let inside_target = tree.path().join("checker-target");
        let outside_target = outside_tree.path().join("checker-target");
        write_runnable(&inside_target);
        write_runnable(&outside_target);
        std::os::unix::fs::symlink(&inside_target, inside_bin.path().join("checker"))
            .expect("inside symlink");
        std::os::unix::fs::symlink(&outside_target, outside_bin.path().join("checker"))
            .expect("outside symlink");

        let outside_path = OsString::from(outside_bin.path());
        let (program, _) = spec_for(
            &["checker".to_owned()],
            tree.path(),
            &[],
            Some(&outside_path),
        )
        .expect("control: the same-shape out-of-tree target is allowed");
        assert_eq!(
            PathBuf::from(program),
            outside_target
                .canonicalize()
                .expect("canonical outside target")
        );

        let inside_path = OsString::from(inside_bin.path());
        let error = spec_for(
            &["checker".to_owned()],
            tree.path(),
            &[],
            Some(&inside_path),
        )
        .expect_err("a canonical target inside the agent tree is refused before spawn");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn a_check_runs_in_its_target_but_refuses_programs_in_every_hired_repository() {
        let declared_first = tempfile::tempdir().unwrap();
        let check_target = tempfile::tempdir().unwrap();
        let undeclared = tempfile::tempdir().unwrap();
        let roots = [
            declared_first.path().to_path_buf(),
            check_target.path().to_path_buf(),
            undeclared.path().to_path_buf(),
        ];
        for root in &roots {
            let program = root.join("check");
            write_runnable(&program);
            let error = spec_for_repositories(
                &[program.to_str().unwrap().to_owned()],
                check_target.path(),
                &[],
                None,
                &roots,
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
        let script = undeclared.path().join("check").to_str().unwrap().to_owned();
        let (_, launch) = spec_for_repositories(
            &["/bin/sh".into(), script.clone()],
            check_target.path(),
            &[],
            None,
            &roots,
        )
        .unwrap();
        assert_eq!(launch.cwd, check_target.path());
        assert_eq!(launch.argv[1], OsString::from(script));
    }

    #[test]
    fn a_fake_change_query_requires_known_results_for_every_declared_member() {
        use std::collections::VecDeque;
        use std::sync::Mutex;
        struct Queries {
            answers: Mutex<VecDeque<io::Result<PredicateOutcome>>>,
            calls: Mutex<Vec<std::path::PathBuf>>,
        }
        impl PredicateRunner for Queries {
            fn run(&self, spec: &LaunchSpec, _timeout: Duration) -> io::Result<PredicateOutcome> {
                self.calls.lock().unwrap().push(spec.cwd.clone());
                self.answers.lock().unwrap().pop_front().unwrap()
            }
            fn sweep(&self) {}
        }
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let roots = [first.path().to_path_buf(), second.path().to_path_buf()];
        let bin = tempfile::tempdir().unwrap();
        let program = bin.path().join(CHECKER);
        write_runnable(&program);
        let argv = [program.to_str().unwrap().to_owned()];
        for (codes, expected) in [
            ([1, 1], Some(false)),
            ([0, 1], Some(true)),
            ([1, 0], Some(true)),
            ([0, 128], None),
            ([128, 0], None),
            ([0, -1], None),
        ] {
            let runner = Queries {
                answers: Mutex::new(
                    codes
                        .into_iter()
                        .map(|exit| {
                            if exit < 0 {
                                Err(io::ErrorKind::Other.into())
                            } else {
                                Ok(PredicateOutcome::Finished {
                                    exit: Some(exit),
                                    output_tail: "fake predicate seam".into(),
                                    timed_out: false,
                                })
                            }
                        })
                        .collect(),
                ),
                calls: Mutex::default(),
            };
            let result = files_changed(
                &runner,
                &argv,
                &roots,
                &[],
                None,
                &roots,
                Duration::from_secs(1),
            )
            .unwrap();
            assert_eq!(result.map(|(pass, _)| pass), expected, "{codes:?}");
            assert_eq!(*runner.calls.lock().unwrap(), roots);
        }
    }

    /// A program that does not resolve makes the run fail and never pass, and the evidence says so
    /// with `program` absent and `argv` as attempted.
    #[test]
    fn a_program_that_does_not_resolve_fails_the_run() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let error =
            resolve("no-such-program-anywhere", cwd.path(), None).expect_err("nothing resolves it");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);

        let argv = vec!["no-such-program-anywhere".to_owned()];
        let (pass, evidence) = refused(&argv, &error);
        assert!(!pass, "a predicate that could not run has not held");
        match evidence {
            Evidence::Command {
                argv: recorded,
                program,
                exit,
                ..
            } => {
                assert_eq!(recorded, argv, "argv is what was attempted");
                assert_eq!(program, None, "nothing was resolved, so nothing is claimed");
                assert_eq!(exit, None);
            }
            other => panic!("a command predicate produces command evidence: {other:?}"),
        }
    }

    /// The verdict: `0` holds, anything else does not, a timeout does not, and a swept run judges
    /// nothing at all. The last arm is the swept-run case and is the reason this returns an `Option`.
    #[test]
    fn only_a_zero_exit_holds_and_a_swept_run_judges_nothing() {
        let argv = vec!["npm".to_owned(), "test".to_owned()];
        let finished = |exit, timed_out| PredicateOutcome::Finished {
            exit,
            output_tail: String::new(),
            timed_out,
        };

        let (pass, _) = judge(&finished(Some(0), false), &argv, None).expect("it judged");
        assert!(pass, "control: exit 0 is what a command predicate holds on");
        let (pass, _) = judge(&finished(Some(1), false), &argv, None).expect("it judged");
        assert!(!pass, "a nonzero exit does not hold");
        let (pass, _) = judge(&finished(None, true), &argv, None).expect("it judged");
        assert!(!pass, "a timeout does not hold");

        assert!(
            judge(&PredicateOutcome::Swept, &argv, None).is_none(),
            "a swept run produces no verdict, so no fabricated Observed result can be journaled"
        );
    }

    /// The spec hands the runner the resolved program and keeps the operator's arguments exactly —
    /// so `Evidence::Command` can carry both, which is the separation.
    #[test]
    fn the_spec_carries_the_resolved_program_and_the_operators_arguments() {
        let bin = tempfile::tempdir().expect("tempdir");
        let cwd = tempfile::tempdir().expect("tempdir");
        let real = bin.path().join(CHECKER);
        write_runnable(&real);
        let path = OsString::from(bin.path());

        let argv = vec!["checker".to_owned(), "--all".to_owned()];
        let (program, spec) =
            spec_for(&argv, cwd.path(), &[], Some(&path)).expect("the spec is built");
        assert_eq!(spec.argv.len(), 2);
        assert_eq!(
            spec.argv[0],
            OsString::from(&program),
            "argv[0] is resolved"
        );
        assert_eq!(
            spec.argv[1],
            OsString::from("--all"),
            "the rest is untouched"
        );
        assert_eq!(spec.cwd, cwd.path(), "it runs in the agent's directory");
        assert!(
            matches!(spec.stdin, dudes_providers::launch::LaunchInput::Null),
            "a predicate is given no standard input"
        );
    }
}
