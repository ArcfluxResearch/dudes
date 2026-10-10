//! The Git tab's read of a checkout and of a worktree the daemon created, against real git.
//!
//! Every vector is planted in a scratch repository and run twice: once through plain `git` in the
//! checkout, which must run the planted program or show the other repository (the positive
//! control, without which an absent log proves nothing), and once through the daemon's read,
//! which must not.
#![cfg(unix)]

use std::ffi::OsString;
use std::io;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use duded::git_read::{GitReader, Lease, READ_DEADLINE, Tree};
use duded::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner};
use duded::worktree::git::Git as DaemonGit;
use dudes_host::{CapturedOutcome, OneShot, OneShotOutcome};
use dudes_proto::{GitAgainst, GitChangeKind, GitState, GitView};
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_types::{Timestamp, WorktreeId};

const NOW: Timestamp = Timestamp::from_unix_millis(1_700_000_000_000);

struct Real(OneShot);

impl PredicateRunner for Real {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<PredicateOutcome> {
        self.0.run(spec, timeout).map(|outcome| match outcome {
            OneShotOutcome::Finished {
                exit,
                output_tail,
                timed_out,
            } => PredicateOutcome::Finished {
                exit,
                output_tail,
                timed_out,
            },
            OneShotOutcome::Swept => PredicateOutcome::Swept,
        })
    }

    fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        stdout_cap: usize,
    ) -> io::Result<PredicateCaptured> {
        self.0
            .run_captured(spec, timeout, stdout_cap)
            .map(|outcome| match outcome {
                CapturedOutcome::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                } => PredicateCaptured::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                },
                CapturedOutcome::Swept => PredicateCaptured::Swept,
            })
    }

    fn sweep(&self) {
        self.0.sweep();
    }
}

/// A scratch world: repositories, planted programs, and the log each of them appends to.
struct Lab {
    root: tempfile::TempDir,
    runner: Arc<Real>,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("lab directory");
        for dir in ["home", "tools", "scratch"] {
            std::fs::create_dir(root.path().join(dir)).expect("lab subdirectory");
        }
        // Canonical, so that a path the test compares is the one the daemon canonicalizes to.
        let root_path = std::fs::canonicalize(root.path()).expect("canonical lab");
        std::fs::write(root_path.join("tools/ran.log"), "").expect("empty log");
        Self {
            root,
            runner: Arc::new(Real(OneShot::new())),
        }
    }

    fn path(&self) -> PathBuf {
        std::fs::canonicalize(self.root.path()).expect("canonical lab")
    }

    fn env(&self) -> Vec<EnvVar> {
        vec![
            EnvVar::new("PATH", std::env::var_os("PATH").unwrap_or_default()),
            EnvVar::new("HOME", self.path().join("home")),
            EnvVar::new("GIT_CONFIG_NOSYSTEM", "1"),
            EnvVar::new("GIT_CONFIG_GLOBAL", "/dev/null"),
            EnvVar::new("GIT_TERMINAL_PROMPT", "0"),
            EnvVar::new("GIT_AUTHOR_NAME", "Lab"),
            EnvVar::new("GIT_AUTHOR_EMAIL", "lab@example.com"),
            EnvVar::new("GIT_COMMITTER_NAME", "Lab"),
            EnvVar::new("GIT_COMMITTER_EMAIL", "lab@example.com"),
        ]
    }

    fn spec(&self, cwd: &Path, argv: &[&str]) -> LaunchSpec {
        LaunchSpec {
            argv: argv.iter().map(OsString::from).collect(),
            env: self.env(),
            cwd: cwd.to_path_buf(),
            stdin: LaunchInput::Null,
        }
    }

    /// Plain `git` in `cwd`, the way a person or an unhardened daemon would run it.
    fn plain(&self, cwd: &Path, args: &[&str]) -> (Option<i32>, String) {
        match self.plain_run(cwd, args, Duration::from_secs(120)) {
            PredicateOutcome::Finished {
                exit, output_tail, ..
            } => (exit, output_tail),
            PredicateOutcome::Swept => panic!("swept"),
        }
    }

    fn plain_run(&self, cwd: &Path, args: &[&str], deadline: Duration) -> PredicateOutcome {
        let mut spec = self.spec(cwd, &[&["git"][..], args].concat());
        spec.argv[0] = resolve_git().into_os_string();
        self.runner.run(&spec, deadline).expect("plain git runs")
    }

    /// Plain `git` that must succeed, for setup.
    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let (exit, tail) = self.plain(cwd, args);
        assert_eq!(exit, Some(0), "git {args:?}: {tail}");
        tail
    }

    fn sh(&self, cwd: &Path, script: &str) {
        let spec = self.spec(cwd, &["/bin/sh", "-c", script]);
        match self
            .runner
            .run(&spec, Duration::from_secs(120))
            .expect("sh runs")
        {
            PredicateOutcome::Finished { exit: Some(0), .. } => {}
            other => panic!("sh {script}: {other:?}"),
        }
    }

    fn log_path(&self) -> PathBuf {
        self.path().join("tools/ran.log")
    }

    /// A program that appends `what` to the log and then runs `tail`.
    fn tool(&self, name: &str, tail: &str) -> PathBuf {
        let path = self.path().join("tools").join(name);
        let body = format!(
            "#!/bin/sh\necho {name} >> '{}'\n{tail}\n",
            self.log_path().display()
        );
        std::fs::write(&path, body).expect("write tool");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn ran(&self) -> String {
        std::fs::read_to_string(self.log_path()).expect("read log")
    }

    fn forget(&self) {
        std::fs::write(self.log_path(), "").expect("clear log");
    }

    /// A repository on `main` with two commits and a tracked `a.txt` whose worktree copy has been
    /// rewritten to the same size, so any command that compares the tree must hash it.
    fn repo(&self, name: &str) -> PathBuf {
        let repo = self.path().join(name);
        std::fs::create_dir_all(&repo).expect("repo directory");
        self.git(&repo, &["init", "--quiet", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "one\n").expect("write a");
        std::fs::write(repo.join("b.txt"), "two\n").expect("write b");
        self.git(&repo, &["add", "-A"]);
        self.git(&repo, &["commit", "--quiet", "-m", "first"]);
        std::fs::write(repo.join("a.txt"), "ONE\n").expect("rewrite a");
        self.git(&repo, &["commit", "--quiet", "-a", "-m", "second"]);
        std::fs::write(repo.join("a.txt"), "uno\n").expect("same-size edit");
        // Same content, new modification time: the index is stale for `b.txt`, so a command that
        // refreshes it writes it back, which is what runs an index hook.
        std::fs::write(repo.join("b.txt"), "two\n").expect("touch b");
        repo
    }

    fn reader(&self) -> GitReader {
        GitReader::new(
            Arc::clone(&self.runner) as Arc<dyn PredicateRunner>,
            std::env::var_os("PATH"),
            self.path().join("scratch"),
        )
    }

    fn read(&self, cwd: &Path) -> GitView {
        self.reader().read(&checkout(cwd), NOW)
    }

    /// Everything under the scratch directory: empty once a read has ended.
    fn scratch_left(&self) -> usize {
        std::fs::read_dir(self.path().join("scratch")).map_or(0, Iterator::count)
    }
}

/// A directory read as an ordinary checkout, the layout of every agent hired without a worktree.
fn checkout(cwd: &Path) -> Tree {
    Tree::Checkout(cwd.to_path_buf())
}

fn resolve_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH")
}

fn json(view: &GitView) -> String {
    serde_json::to_string(view).expect("serialize")
}

/// One planted vector: plain git runs the program, the daemon's read does not.
fn vector(name: &str, plant: impl Fn(&Lab, &Path), plain: &[&str]) -> (Lab, PathBuf, GitView) {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    plant(&lab, &repo);
    lab.plain(&repo, plain);
    assert_ne!(
        lab.ran(),
        "",
        "{name}: control: plain git `{plain:?}` must run the planted program"
    );
    lab.forget();
    let view = lab.read(&repo);
    assert_eq!(view.state, GitState::Ok, "{name}");
    assert_eq!(lab.ran(), "", "{name}: the read ran a planted program");
    assert_eq!(lab.scratch_left(), 0, "{name}: the shadow was removed");
    let changed: Vec<_> = view.changes.items.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(
        changed,
        ["a.txt"],
        "{name}: the read still reports the edit"
    );
    (lab, repo, view)
}

/// Hooks live in a directory the shadow does not have, and no command in the set writes the index
/// or a ref, so this covers the command set as much as the shadow.
#[test]
fn a_hook_does_not_run() {
    vector(
        "post-index-change hook",
        |lab, repo| {
            let hook = lab.tool("hook", "");
            std::fs::rename(hook, repo.join(".git/hooks/post-index-change")).expect("install hook");
        },
        &["status"],
    );
}

#[test]
fn a_filesystem_monitor_does_not_run() {
    vector(
        "core.fsmonitor",
        |lab, repo| {
            let tool = lab.tool("fsmonitor", "");
            lab.git(
                repo,
                &["config", "core.fsmonitor", &tool.display().to_string()],
            );
        },
        &["status"],
    );
}

#[test]
fn a_clean_filter_does_not_run_whether_bound_in_tree_or_in_info() {
    vector(
        "filter.evil.clean",
        |lab, repo| {
            let tool = lab.tool("filter", "cat");
            lab.git(
                repo,
                &["config", "filter.evil.clean", &tool.display().to_string()],
            );
            std::fs::write(repo.join(".git/info/attributes"), "*.txt filter=evil\n").expect("info");
        },
        &["status"],
    );
    vector(
        "filter.evil.clean through an in-tree attribute",
        |lab, repo| {
            let tool = lab.tool("filter", "cat");
            lab.git(
                repo,
                &["config", "filter.evil.clean", &tool.display().to_string()],
            );
            std::fs::write(repo.join(".gitattributes"), "*.txt filter=evil\n").expect("attributes");
        },
        &["status"],
    );
}

/// The plain control runs `git diff`, which is where textconv and the external diff run. The
/// read's own commands never ask for a diff of content, so this covers the command set; the
/// shadow's missing configuration is what the filter and fsmonitor tests cover.
#[test]
fn a_textconv_driver_does_not_run() {
    vector(
        "diff.evil.textconv",
        |lab, repo| {
            let tool = lab.tool("textconv", "cat \"$1\"");
            lab.git(
                repo,
                &["config", "diff.evil.textconv", &tool.display().to_string()],
            );
            std::fs::write(repo.join(".git/info/attributes"), "*.txt diff=evil\n").expect("info");
        },
        &["diff", "HEAD~1", "HEAD"],
    );
}

#[test]
fn an_external_diff_does_not_run() {
    vector(
        "diff.external",
        |lab, repo| {
            let tool = lab.tool("external", "");
            lab.git(
                repo,
                &["config", "diff.external", &tool.display().to_string()],
            );
        },
        &["diff", "HEAD~1", "HEAD"],
    );
}

/// `status.showUntrackedFiles=no` is configuration; the read applies none.
#[test]
fn configured_untracked_hiding_does_not_hide_the_count() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    std::fs::write(repo.join("untracked.txt"), "x\n").expect("untracked");
    lab.git(&repo, &["config", "status.showUntrackedFiles", "no"]);
    let (_, plain) = lab.plain(&repo, &["status", "--porcelain"]);
    assert!(
        !plain.contains("untracked.txt"),
        "control: git hides it: {plain}"
    );
    let view = lab.read(&repo);
    assert_eq!(view.head.clone().expect("head").untracked, 1);
}

/// A gitlink whose directory is a nested repository carrying a hostile configuration. The control
/// is the same shadow with `status` run without `--ignore-submodules=all`.
#[test]
fn a_gitlink_submodule_does_not_run_its_own_configuration() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    let nested = repo.join("sub");
    std::fs::create_dir(&nested).expect("nested");
    lab.git(&nested, &["init", "--quiet", "-b", "main"]);
    std::fs::write(nested.join("f.txt"), "nested\n").expect("nested file");
    lab.git(&nested, &["add", "-A"]);
    lab.git(&nested, &["commit", "--quiet", "-m", "nested"]);
    let tip = lab.git(&nested, &["rev-parse", "HEAD"]).trim().to_owned();
    let tool = lab.tool("nested-fsmonitor", "");
    lab.git(
        &nested,
        &["config", "core.fsmonitor", &tool.display().to_string()],
    );
    lab.git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{tip},sub"),
        ],
    );
    lab.git(&repo, &["commit", "--quiet", "-m", "add the gitlink"]);
    std::fs::write(nested.join("f.txt"), "edited\n").expect("dirty the nested repository");

    let reader = lab.reader();
    let shadow = reader.shadow(&checkout(&repo)).expect("a shadow");
    let spec = reader
        .launch(&shadow, &["status", "--porcelain=v1", "-z"])
        .expect("launch");
    lab.runner
        .run_captured(&spec, Duration::from_secs(20), 64 * 1024)
        .expect("status without the flag runs");
    assert_ne!(
        lab.ran(),
        "",
        "control: without --ignore-submodules=all the nested repository's fsmonitor runs"
    );
    drop(shadow);
    lab.forget();

    let view = reader.read(&checkout(&repo), NOW);
    assert_eq!(view.state, GitState::Ok);
    assert_eq!(
        lab.ran(),
        "",
        "the read runs the fixed status, which ignores submodules"
    );
    assert!(
        view.changes.items.iter().all(|change| change.path != "sub"),
        "the nested repository is not entered"
    );
}

/// A `.git` that is a file redirects plain git into a second repository, running its programs and
/// showing its history. The read refuses without opening it.
#[test]
fn a_gitfile_redirect_is_refused_and_the_other_repository_stays_unread() {
    let lab = Lab::new();
    let other = lab.repo("other");
    lab.git(&other, &["branch", "other-secret-branch"]);
    lab.git(
        &other,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "other-secret-subject",
        ],
    );
    let tool = lab.tool("other-fsmonitor", "");
    lab.git(
        &other,
        &["config", "core.fsmonitor", &tool.display().to_string()],
    );

    let victim = lab.path().join("victim");
    std::fs::create_dir(&victim).expect("victim");
    std::fs::write(
        victim.join(".git"),
        format!("gitdir: {}\n", other.join(".git").display()),
    )
    .expect("gitfile");
    let (_, plain) = lab.plain(&victim, &["log", "-1", "--format=%s"]);
    assert!(
        plain.contains("other-secret-subject"),
        "control: plain git shows it: {plain}"
    );
    lab.plain(&victim, &["status"]);
    assert_ne!(
        lab.ran(),
        "",
        "control: plain git ran the second repository's program"
    );
    lab.forget();

    let view = lab.read(&victim);
    assert_eq!(view.state, GitState::GitfileUnsupported);
    assert_eq!(lab.ran(), "");
    let wire = json(&view);
    for leaked in ["other-secret", "other.git", "first"] {
        assert!(!wire.contains(leaked), "{leaked} in {wire}");
    }
}

/// Alternates plus a `packed-refs` line name another repository's commit, with no symlink at all.
#[test]
fn alternates_and_a_packed_ref_do_not_show_another_repositorys_history() {
    let lab = Lab::new();
    let other = lab.repo("other");
    lab.git(
        &other,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "other-secret-subject",
        ],
    );
    std::fs::write(other.join("other-secret-file.txt"), "x\n").expect("other file");
    lab.git(&other, &["add", "-A"]);
    lab.git(
        &other,
        &["commit", "--quiet", "-m", "other-secret-subject two"],
    );
    let tip = lab.git(&other, &["rev-parse", "HEAD"]).trim().to_owned();

    let victim = lab.repo("victim");
    std::fs::create_dir_all(victim.join(".git/objects/info")).expect("info");
    std::fs::write(
        victim.join(".git/objects/info/alternates"),
        format!("{}\n", other.join(".git/objects").display()),
    )
    .expect("alternates");
    std::fs::remove_file(victim.join(".git/refs/heads/main")).expect("drop the loose ref");
    std::fs::write(
        victim.join(".git/packed-refs"),
        format!("{tip} refs/heads/main\n"),
    )
    .expect("packed-refs");
    let (_, plain) = lab.plain(&victim, &["log", "-1", "--format=%s", "main"]);
    assert!(
        plain.contains("other-secret-subject two"),
        "control: {plain}"
    );

    let view = lab.read(&victim);
    let wire = json(&view);
    for leaked in ["other-secret", "other.git"] {
        assert!(!wire.contains(leaked), "{leaked} in {wire}");
    }
    assert_eq!(lab.scratch_left(), 0);
}

fn symlinked(entry: &str) {
    let lab = Lab::new();
    let other = lab.repo("other");
    lab.git(&other, &["branch", "other-secret-branch"]);
    if entry == "packed-refs" {
        lab.git(&other, &["pack-refs", "--all"]);
    }
    let other_tip = lab.git(&other, &["rev-parse", "HEAD"]).trim().to_owned();
    let victim = lab.repo("victim");
    let target = other.join(".git").join(entry);
    let link = victim.join(".git").join(entry);
    match std::fs::symlink_metadata(&link) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&link).expect("remove dir"),
        Ok(_) => std::fs::remove_file(&link).expect("remove file"),
        Err(_) => {}
    }
    symlink(&target, &link).expect("plant the link");
    let probe: Vec<&str> = match entry {
        "refs" | "packed-refs" => vec!["for-each-ref", "--format=%(refname)"],
        "index" => vec!["ls-files"],
        "objects" => vec!["cat-file", "-t", &other_tip],
        _ => vec!["symbolic-ref", "HEAD"],
    };
    let (exit, plain) = lab.plain(&victim, &probe);
    if entry == "HEAD" {
        // Git refuses a linked `HEAD` itself, so there is no redirect to control for.
        assert_eq!(exit, Some(128), "{plain}");
    } else if entry == "refs" || entry == "packed-refs" {
        assert!(
            plain.contains("other-secret-branch"),
            "control: plain git follows the {entry} link: {exit:?} {plain}"
        );
    } else {
        assert_eq!(
            exit,
            Some(0),
            "control: plain git follows the {entry} link: {plain}"
        );
    }

    let view = lab.read(&victim);
    assert_eq!(view.state, GitState::Unreadable, "{entry}");
    assert!(!json(&view).contains("other-secret"), "{entry}");
    assert_eq!(lab.scratch_left(), 0, "{entry}");
}

#[test]
fn a_symlinked_refs_directory_is_refused() {
    symlinked("refs");
}

#[test]
fn a_symlinked_objects_directory_is_refused() {
    symlinked("objects");
}

#[test]
fn a_symlinked_index_is_refused() {
    symlinked("index");
}

#[test]
fn a_symlinked_packed_refs_is_refused() {
    symlinked("packed-refs");
}

#[test]
fn a_symlinked_head_is_refused() {
    symlinked("HEAD");
}

#[test]
fn a_hostile_head_string_is_refused() {
    for hostile in [
        "ref: refs/heads/../../../../etc/passwd\n",
        "ref: ../../other/.git/HEAD\n",
        "ref: refs/heads/a b\n",
        "garbage\n",
        "",
    ] {
        let lab = Lab::new();
        let repo = lab.repo("repo");
        std::fs::write(repo.join(".git/HEAD"), hostile).expect("hostile HEAD");
        let view = lab.read(&repo);
        assert_eq!(view.state, GitState::Unreadable, "{hostile:?}");
        assert_eq!(lab.scratch_left(), 0);
    }
}

/// A FIFO named `HEAD` blocks a blocking `open()` for ever. The control opens it the blocking way
/// and sees it hang; the read opens without blocking and refuses a file that is not a regular one.
#[test]
fn a_fifo_head_is_refused_at_once_where_a_blocking_open_hangs() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    let head = repo.join(".git/HEAD");
    std::fs::remove_file(&head).expect("remove HEAD");
    nix::unistd::mkfifo(&head, nix::sys::stat::Mode::from_bits_truncate(0o600)).expect("mkfifo");

    let blocked = head.clone();
    let opener = std::thread::spawn(move || std::fs::File::open(blocked).map(drop));
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !opener.is_finished(),
        "control: a blocking open of the FIFO waits for a writer"
    );

    let started = Instant::now();
    let view = lab.read(&repo);
    assert_eq!(view.state, GitState::Unreadable);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(lab.scratch_left(), 0);

    let _writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&head)
        .expect("release the opener");
    opener
        .join()
        .expect("opener thread")
        .expect("the opener returned");
}

/// A 64 MiB commit message, compressed to a loose object of a few hundred KiB, makes plain git
/// stream far more than the cap. The read ends inside its deadline with the group gone.
#[test]
fn a_huge_loose_object_ends_inside_the_deadline() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    lab.sh(
        &repo,
        "tree=$(git write-tree) && { printf 'tree %s\\nparent %s\\nauthor A <a@x> 1 +0000\\ncommitter A <a@x> 1 +0000\\n\\n' \"$tree\" \"$(git rev-parse HEAD)\"; head -c 67108864 /dev/zero | tr '\\0' 'a'; } > ../huge-body && id=$(git hash-object -t commit -w --stdin < ../huge-body) && git update-ref refs/heads/main \"$id\"",
    );
    let git = resolve_git();
    let spec = lab.spec(
        &repo,
        &[git.to_str().expect("utf-8"), "log", "-1", "--format=%s"],
    );
    let CapturedOutcome::Finished { truncated, .. } = lab
        .runner
        .0
        .run_captured(&spec, Duration::from_secs(60), 1024 * 1024)
        .expect("plain log runs")
    else {
        panic!("swept");
    };
    assert!(
        truncated,
        "control: plain git streams the whole message, far past the cap"
    );

    let started = Instant::now();
    let view = lab.read(&repo);
    assert!(
        started.elapsed() < READ_DEADLINE + Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(json(&view).len() < dudes_proto::MAX_GIT_VIEW_BYTES);
    assert_eq!(lab.runner.0.in_flight(), 0, "every child was reaped");
    assert_eq!(lab.scratch_left(), 0);
}

fn origin_and_clone(lab: &Lab, depth: bool) -> (PathBuf, PathBuf) {
    let upstream = lab.repo("upstream-work");
    lab.git(&upstream, &["checkout", "--quiet", "--", "."]);
    let bare = lab.path().join("origin.git");
    lab.git(
        &lab.path(),
        &[
            "clone",
            "--quiet",
            "--bare",
            upstream.to_str().expect("utf-8"),
            bare.to_str().expect("utf-8"),
        ],
    );
    let work = lab.path().join("work");
    let source = if depth {
        format!("file://{}", bare.display())
    } else {
        bare.display().to_string()
    };
    let mut args = vec!["clone", "--quiet"];
    if depth {
        args.push("--depth=1");
    }
    args.extend([source.as_str(), work.to_str().expect("utf-8")]);
    lab.git(&lab.path(), &args);
    (bare, work)
}

/// An agent that pushed its branch is still ahead of the remote default branch. Comparing with
/// `origin/<branch>` reported nothing ahead after the push.
#[test]
fn a_pushed_branch_is_still_ahead_of_the_default_branch() {
    let lab = Lab::new();
    let (_bare, work) = origin_and_clone(&lab, false);
    lab.git(&work, &["checkout", "--quiet", "-b", "agent/feature"]);
    std::fs::write(work.join("new.txt"), "n\n").expect("new");
    lab.git(&work, &["add", "-A"]);
    lab.git(&work, &["commit", "--quiet", "-m", "agent one"]);
    std::fs::write(work.join("b.txt"), "changed\n").expect("b");
    lab.git(&work, &["commit", "--quiet", "-a", "-m", "agent two"]);
    lab.git(&work, &["push", "--quiet", "-u", "origin", "agent/feature"]);
    let (_, upstream_view) = lab.plain(
        &work,
        &["rev-list", "--count", "origin/agent/feature..HEAD"],
    );
    assert_eq!(
        upstream_view.trim(),
        "0",
        "control: against its own upstream it is 0 ahead"
    );

    let view = lab.read(&work);
    assert_eq!(view.state, GitState::Ok);
    let head = view.head.clone().expect("head");
    assert_eq!((head.ahead, head.behind), (Some(2), Some(0)));
    assert_eq!(head.branch.as_deref(), Some("agent/feature"));
    let subjects: Vec<_> = view
        .commits
        .items
        .iter()
        .map(|c| c.subject.as_str())
        .collect();
    assert_eq!(subjects, ["agent two", "agent one"]);
    let committed: Vec<_> = view
        .committed
        .items
        .iter()
        .map(|file| (file.path.as_str(), file.kind))
        .collect();
    assert_eq!(
        committed,
        [
            ("b.txt", GitChangeKind::Modified),
            ("new.txt", GitChangeKind::Added)
        ]
    );
    assert_eq!(
        view.branches.items[0].name, "agent/feature",
        "the current branch comes first"
    );
    assert!(view.branches.items[0].current);
    assert!(
        !json(&view).contains("origin"),
        "no remote name reaches the reply"
    );
}

#[test]
fn a_shallow_clone_is_read() {
    let lab = Lab::new();
    let (_bare, work) = origin_and_clone(&lab, true);
    assert!(
        work.join(".git/shallow").exists(),
        "control: the clone is shallow"
    );
    let view = lab.read(&work);
    assert_eq!(view.state, GitState::Ok);
    assert!(view.head.clone().expect("head").id.is_some());
    assert_eq!(lab.scratch_left(), 0);
}

/// A partial clone fetches a missing object from its remote, through the program its own
/// configuration names (`remote.origin.uploadpack`). The base ref is moved to a commit whose tree
/// was never fetched, so a diff against it needs the remote: the read, which never opens that
/// configuration, does not reach for it, and plain git runs the planted program to get it.
#[test]
fn a_partial_clone_does_not_fetch_for_a_missing_object() {
    let lab = Lab::new();
    let upstream = lab.repo("upstream-work");
    lab.git(&upstream, &["checkout", "--quiet", "--", "."]);
    std::fs::write(upstream.join("c.txt"), "three\n").expect("third file");
    lab.git(&upstream, &["add", "-A"]);
    lab.git(&upstream, &["commit", "--quiet", "-m", "third"]);
    let bare = lab.path().join("origin.git");
    lab.git(
        &lab.path(),
        &[
            "clone",
            "--quiet",
            "--bare",
            upstream.to_str().expect("utf-8"),
            bare.to_str().expect("utf-8"),
        ],
    );
    lab.git(&bare, &["config", "uploadpack.allowFilter", "true"]);
    lab.git(&bare, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    let work = lab.path().join("work");
    lab.git(
        &lab.path(),
        &[
            "clone",
            "--quiet",
            "--filter=tree:0",
            &format!("file://{}", bare.display()),
            work.to_str().expect("utf-8"),
        ],
    );
    let missing = lab.git(&work, &["rev-parse", "HEAD~2"]).trim().to_owned();
    lab.git(&work, &["update-ref", "refs/remotes/origin/main", &missing]);
    let tool = lab.tool("uploadpack", "exec git-upload-pack \"$@\"");
    lab.git(
        &work,
        &[
            "config",
            "remote.origin.uploadpack",
            &tool.display().to_string(),
        ],
    );

    let view = lab.read(&work);
    assert_eq!(view.state, GitState::Ok);
    assert_eq!(lab.ran(), "", "the read never reached for the remote");
    assert!(
        view.committed.items.is_empty(),
        "the diff it could not make is empty, not wrong"
    );

    // The same command, from the same shadow, without `GIT_NO_LAZY_FETCH`: the literal
    // configuration alone is what keeps the remote out, on any git version.
    let reader = lab.reader();
    let shadow = reader.shadow(&checkout(&work)).expect("a shadow");
    let mut spec = reader
        .launch(
            &shadow,
            &[
                "diff-tree",
                "-r",
                "--raw",
                "-z",
                "--no-renames",
                &missing,
                "HEAD",
            ],
        )
        .expect("launch");
    spec.env.retain(|var| var.name() != "GIT_NO_LAZY_FETCH");
    assert!(spec.env.iter().all(|var| var.name() != "GIT_NO_LAZY_FETCH"));
    lab.runner
        .run_captured(&spec, Duration::from_secs(20), 64 * 1024)
        .expect("diff-tree without the variable runs");
    drop(shadow);
    assert_eq!(lab.ran(), "", "no lazy-fetch variable, and still no remote");

    // The control runs last, because it fetches the tree the read must find missing.
    lab.plain(
        &work,
        &["diff-tree", "-r", "--raw", "--no-renames", &missing, "HEAD"],
    );
    assert_ne!(
        lab.ran(),
        "",
        "control: plain git fetches the missing tree through the planted program"
    );
}

/// Hidden files stay hidden: the shadow reads no `info/exclude`, so an operator-excluded secret is
/// counted and never named.
#[test]
fn operator_excluded_files_appear_only_in_the_untracked_count() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    std::fs::write(repo.join(".git/info/exclude"), ".env.production\nkey.pem\n").expect("exclude");
    for name in [".env.production", "key.pem", "plain.txt"] {
        std::fs::write(repo.join(name), "secret\n").expect("write");
    }
    let (_, plain) = lab.plain(&repo, &["status", "--porcelain"]);
    assert!(
        !plain.contains("key.pem") && plain.contains("plain.txt"),
        "control: {plain}"
    );
    let view = lab.read(&repo);
    assert_eq!(view.head.clone().expect("head").untracked, 3);
    let wire = json(&view);
    for name in [".env.production", "key.pem", "plain.txt"] {
        assert!(!wire.contains(name), "{name} in {wire}");
    }
}

#[test]
fn a_hundred_thousand_untracked_files_are_counted_to_a_cap_and_not_a_hang() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    for n in 0..100_000 {
        std::fs::File::create(repo.join(format!("u{n}"))).expect("create untracked fixture");
    }
    let started = Instant::now();
    let view = lab.read(&repo);
    assert!(
        started.elapsed() < READ_DEADLINE + Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(view.state, GitState::Ok);
    let head = view.head.clone().expect("head");
    assert!(head.untracked_cut && head.untracked > 1000, "{head:?}");
    let changed: Vec<_> = view.changes.items.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(
        changed,
        ["a.txt"],
        "the tracked edit is listed before the untracked files"
    );
    assert!(
        !view.changes.truncated,
        "a cut inside the untracked files loses no tracked change"
    );
    assert!(json(&view).len() < dudes_proto::MAX_GIT_VIEW_BYTES);
}

#[test]
fn thirty_thousand_staged_files_fill_the_changes_budget_and_say_so() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    let staged = repo.join("t");
    std::fs::create_dir(&staged).expect("create staged fixture directory");
    for n in 0..30_000 {
        std::fs::File::create(staged.join(format!("staged-file-number-{n}")))
            .expect("create staged fixture");
    }
    lab.git(&repo, &["add", "t"]);
    let started = Instant::now();
    let view = lab.read(&repo);
    assert!(
        started.elapsed() < READ_DEADLINE + Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(view.state, GitState::Ok);
    assert!(
        view.changes.truncated && view.changes.items.len() > 100,
        "{}",
        view.changes.items.len()
    );
    let section = serde_json::to_vec(&view.changes).expect("serialize");
    assert!(
        section.len() <= dudes_proto::MAX_GIT_CHANGE_BYTES,
        "{}",
        section.len()
    );
    assert!(json(&view).len() < dudes_proto::MAX_GIT_VIEW_BYTES);
}

/// Threads are what a Linux child's address-space limit takes away at random: each one reserves a
/// stack, and `status` over a large index dies with "unable to create threaded lstat" when the
/// limit leaves no room for the next, which the read reported as an unreadable tree. The shadow's
/// configuration turns them all off.
#[test]
fn the_shadow_configuration_keeps_status_single_threaded() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    lab.git(
        &repo,
        &["commit", "--quiet", "--allow-empty", "-m", "first"],
    );
    let reader = lab.reader();
    let shadow = reader.shadow(&checkout(&repo)).expect("a shadow");
    for key in ["core.preloadindex", "index.threads"] {
        let spec = reader
            .launch(&shadow, &["config", "--type=bool", "--get", key])
            .expect("launch");
        let Ok(PredicateCaptured::Finished { exit, stdout, .. }) =
            lab.runner.run_captured(&spec, Duration::from_secs(20), 64)
        else {
            panic!("config {key} did not finish");
        };
        assert_eq!(
            String::from_utf8_lossy(&stdout).trim(),
            "false",
            "{key} (exit {exit:?})"
        );
    }
}

/// Strings an agent controls: a right-to-left override, a newline, a shell metacharacter run.
#[test]
fn hostile_strings_are_dropped_or_left_for_the_floor_to_quote() {
    let lab = Lab::new();
    let (_bare, work) = origin_and_clone(&lab, false);
    lab.git(&work, &["checkout", "--quiet", "-b", "feature"]);
    let shell = "'; rm -rf ~; '.txt";
    for name in ["evil\u{202e}gpj.exe", "new\nline.txt", shell, "ok.txt"] {
        std::fs::write(work.join(name), "x\n").expect("write");
    }
    lab.git(&work, &["add", "-A"]);
    lab.git(
        &work,
        &["commit", "--quiet", "-m", "subject \u{202e}tpircs"],
    );
    lab.git(&work, &["branch", "bad\u{202e}name"]);
    std::fs::write(work.join("a.txt"), "edited\n").expect("edit");
    lab.git(&work, &["add", "a.txt"]);
    lab.git(&work, &["commit", "--quiet", "-m", &"long ".repeat(60)]);

    let view = lab.read(&work);
    let wire = json(&view);
    for forbidden in ["\u{202e}", "\\u202e", "\\n"] {
        assert!(!wire.contains(forbidden), "{forbidden:?} in {wire}");
    }
    let paths: Vec<_> = view
        .committed
        .items
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert!(
        paths.contains(&shell) && paths.contains(&"ok.txt"),
        "{paths:?}"
    );
    assert_eq!(
        view.committed.dropped, 2,
        "the override and the newline were dropped and counted"
    );
    assert_eq!(
        view.branches.dropped, 1,
        "the branch with the override was dropped"
    );
    assert_eq!(
        view.commits.dropped, 1,
        "the subject with the override was dropped"
    );
    assert!(view.commits.items[0].subject.ends_with('\u{2026}'));
    assert!(view.commits.items[0].subject.len() <= dudes_proto::MAX_GIT_SUBJECT_BYTES);
}

#[test]
fn a_checkout_has_no_working_directory_in_its_reply() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    std::fs::write(repo.join("untracked.txt"), "x\n").expect("untracked");
    let wire = json(&lab.read(&repo));
    for path in [
        lab.path().display().to_string(),
        "scratch".to_owned(),
        "/private".to_owned(),
    ] {
        assert!(!wire.contains(&path), "{path} in {wire}");
    }
}

#[test]
fn the_reply_names_the_branch_the_commits_the_changes_and_the_count() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    std::fs::write(repo.join("untracked.txt"), "x\n").expect("untracked");
    let view = lab.read(&repo);
    assert_eq!(view.state, GitState::Ok);
    let head = view.head.clone().expect("head");
    assert_eq!(head.branch.as_deref(), Some("main"));
    assert_eq!((head.ahead, head.behind, head.untracked), (None, None, 1));
    assert_eq!(head.id.as_deref().map(str::len), Some(12));
    let subjects: Vec<_> = view
        .commits
        .items
        .iter()
        .map(|c| c.subject.as_str())
        .collect();
    assert_eq!(
        subjects,
        ["second", "first"],
        "with no base the last commits are shown"
    );
    assert!(view.committed.items.is_empty());
    let changes: Vec<_> = view
        .changes
        .items
        .iter()
        .map(|c| (c.path.as_str(), c.kind, c.staged))
        .collect();
    assert_eq!(changes, [("a.txt", GitChangeKind::Modified, false)]);
    assert!(!json(&view).contains("lab@example.com") && !json(&view).contains("Lab"));
}

#[test]
fn a_detached_head_and_an_unborn_branch_are_read() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    lab.git(
        &repo,
        &["checkout", "--quiet", "--force", "--detach", "HEAD~1"],
    );
    let head = lab.read(&repo).head.expect("head");
    assert_eq!(head.branch, None);
    assert_eq!(head.id.as_deref().map(str::len), Some(12));

    let fresh = lab.path().join("fresh");
    std::fs::create_dir(&fresh).expect("fresh");
    lab.git(&fresh, &["init", "--quiet", "-b", "main"]);
    std::fs::write(fresh.join("f.txt"), "x\n").expect("f");
    let view = lab.read(&fresh);
    assert_eq!(view.state, GitState::Ok);
    let head = view.head.clone().expect("head");
    assert_eq!((head.branch.as_deref(), head.id), (Some("main"), None));
    assert_eq!(head.untracked, 1);
}

#[test]
fn what_is_not_a_checkout_root_is_refused_with_its_own_state() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    let sub = repo.join("nested-dir");
    std::fs::create_dir(&sub).expect("subdirectory");
    assert_eq!(
        lab.read(&sub).state,
        GitState::NotACheckoutRoot,
        "no search upwards"
    );
    let empty = lab.path().join("empty");
    std::fs::create_dir(&empty).expect("empty");
    assert_eq!(lab.read(&empty).state, GitState::NotACheckoutRoot);
    assert_eq!(
        lab.read(&lab.path().join("missing")).state,
        GitState::NotACheckoutRoot
    );
    let sha256 = lab.path().join("reftable");
    std::fs::create_dir(&sha256).expect("reftable repo");
    lab.git(&sha256, &["init", "--quiet", "-b", "main"]);
    std::fs::create_dir(sha256.join(".git/reftable")).expect("reftable directory");
    assert_eq!(lab.read(&sha256).state, GitState::UnsupportedStorage);
}

#[test]
fn a_missing_git_is_unavailable() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    let reader = GitReader::new(
        Arc::clone(&lab.runner) as Arc<dyn PredicateRunner>,
        Some(OsString::from(lab.path().join("no-bin"))),
        lab.path().join("scratch"),
    );
    assert_eq!(
        reader.read(&checkout(&repo), NOW).state,
        GitState::GitUnavailable
    );
}

/// A `git` inside the agent's own tree is refused, as a predicate program is.
#[test]
fn a_git_inside_the_agent_tree_is_refused() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    let bin = repo.join("bin");
    std::fs::create_dir(&bin).expect("bin");
    std::fs::copy(resolve_git(), bin.join("git")).expect("copy git");
    let reader = GitReader::new(
        Arc::clone(&lab.runner) as Arc<dyn PredicateRunner>,
        Some(bin.into_os_string()),
        lab.path().join("scratch"),
    );
    assert_eq!(
        reader.read(&checkout(&repo), NOW).state,
        GitState::GitUnavailable
    );
}

#[test]
fn a_crashed_runs_scratch_is_swept() {
    let lab = Lab::new();
    let stale = lab.path().join("scratch/1234-0/objects");
    std::fs::create_dir_all(&stale).expect("stale shadow");
    symlink("/etc", stale.join("pack")).expect("stale link");
    lab.reader().sweep_scratch();
    assert_eq!(lab.scratch_left(), 0);
    assert!(Path::new("/etc").exists(), "a link is removed as a link");
}

/// A `git` that answers its version and then never answers: the read ends at the command deadline
/// as `timed_out`, with its child and its shadow gone.
#[test]
fn a_git_that_never_answers_ends_as_timed_out() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    let bin = lab.path().join("slow-bin");
    std::fs::create_dir(&bin).expect("bin");
    let script = bin.join("git");
    std::fs::write(
        &script,
        "#!/bin/sh\ncase \"$1\" in --version) echo git version 2.54.0;; *) /bin/sleep 60;; esac\n",
    )
    .expect("slow git");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    // The first run of a new program can be slow to start on macOS, which would spend the version
    // check's deadline before the read began.
    lab.sh(&bin, &format!("'{}' --version", script.display()));
    let reader = GitReader::new(
        Arc::clone(&lab.runner) as Arc<dyn PredicateRunner>,
        Some(bin.into_os_string()),
        lab.path().join("scratch"),
    );
    let started = Instant::now();
    let view = reader.read(&checkout(&repo), NOW);
    assert_eq!(view.state, GitState::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        lab.runner.0.in_flight(),
        0,
        "the child was killed and reaped"
    );
    assert_eq!(lab.scratch_left(), 0);
}

/// The version floor is the oldest git the matrix was run against: one release older is refused
/// with a finite state, and the floor itself gets as far as reading.
#[test]
fn a_git_older_than_the_measured_floor_is_unavailable() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    let state_with = |version: &str| {
        let bin = lab.path().join(format!("bin-{version}"));
        std::fs::create_dir(&bin).expect("bin");
        let script = bin.join("git");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncase \"$1\" in --version) echo git version {version};; *) exit 1;; esac\n"),
        )
        .expect("fake git");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        lab.sh(&bin, &format!("'{}' --version", script.display()));
        GitReader::new(
            Arc::clone(&lab.runner) as Arc<dyn PredicateRunner>,
            Some(bin.into_os_string()),
            lab.path().join("scratch"),
        )
        .read(&checkout(&repo), NOW)
        .state
    };
    assert_eq!(state_with("2.38.9"), GitState::GitUnavailable);
    assert_eq!(state_with("1.9.1"), GitState::GitUnavailable);
    assert_eq!(
        state_with("2.39.0"),
        GitState::Unreadable,
        "control: the floor passes the version check, and this fake then answers nothing else"
    );
}

/// A minimal SHA-1, for the checksum at the end of an index the test rewrites.
#[expect(
    clippy::many_single_char_names,
    reason = "the names the algorithm is written with"
)]
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in message.chunks(64) {
        let mut w = [0_u32; 80];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, next);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut out = [0_u8; 20];
    for (chunk, value) in out.chunks_mut(4).zip(h) {
        chunk.copy_from_slice(&value.to_be_bytes());
    }
    out
}

#[test]
fn the_test_sha1_is_sha1() {
    let hex = |digest: [u8; 20]| {
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .concat()
    };
    assert_eq!(
        hex(sha1(b"abc")),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    assert_eq!(hex(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
}

/// An index entry named `../outside.txt` makes plain git compare, and so open, a file above the
/// checkout. The read walks the index it copies and refuses such a name before git runs.
#[test]
fn an_index_entry_that_leaves_the_checkout_is_refused() {
    let lab = Lab::new();
    let repo = lab.path().join("repo");
    std::fs::create_dir_all(repo.join("aa")).expect("repo");
    lab.git(&repo, &["init", "--quiet", "-b", "main"]);
    std::fs::write(repo.join("aa/outside.txt"), "inside\n").expect("tracked");
    lab.git(&repo, &["add", "-A"]);
    lab.git(&repo, &["commit", "--quiet", "-m", "first"]);
    let index = repo.join(".git/index");
    let mut bytes = std::fs::read(&index).expect("index");
    let at = bytes
        .windows(14)
        .position(|window| window == b"aa/outside.txt")
        .expect("the entry name");
    bytes[at..at + 3].copy_from_slice(b"../");
    let end = bytes.len() - 20;
    let sum = sha1(&bytes[..end]);
    bytes[end..].copy_from_slice(&sum);
    std::fs::write(&index, &bytes).expect("rewrite the index");
    std::fs::write(lab.path().join("outside.txt"), "OUTSIDE SECRET\n").expect("outside file");

    let (exit, plain) = lab.plain(&repo, &["status", "--porcelain"]);
    assert!(
        plain.contains("outside.txt"),
        "control: plain git compares the file above the checkout: {exit:?} {plain}"
    );

    let view = lab.read(&repo);
    assert_eq!(view.state, GitState::Unreadable);
    assert!(!json(&view).contains("outside"));
    assert_eq!(lab.scratch_left(), 0);
}

#[test]
fn an_index_in_version_four_is_unsupported_and_version_two_reads() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    lab.git(&repo, &["update-index", "--index-version", "4"]);
    assert_eq!(lab.read(&repo).state, GitState::UnsupportedStorage);
    lab.git(&repo, &["update-index", "--index-version", "2"]);
    assert_eq!(
        lab.read(&repo).state,
        GitState::Ok,
        "control: a version 2 index reads"
    );
}

/// An untracked subdirectory whose `.git` is a file makes `status` open the repository it names.
/// A FIFO named `HEAD` there blocks the open, and the command deadline is what ends it. This is a
/// residual the shadow does not close: the reply leaks nothing and the read ends.
#[test]
fn a_nested_gitfile_to_a_blocking_fifo_ends_at_the_command_deadline() {
    let lab = Lab::new();
    let repo = lab.repo("repo");
    let elsewhere = lab.path().join("elsewhere");
    std::fs::create_dir_all(elsewhere.join("objects")).expect("elsewhere");
    std::fs::create_dir_all(elsewhere.join("refs")).expect("refs");
    nix::unistd::mkfifo(
        &elsewhere.join("HEAD"),
        nix::sys::stat::Mode::from_bits_truncate(0o600),
    )
    .expect("mkfifo");
    let nested = repo.join("sub");
    std::fs::create_dir(&nested).expect("nested");
    std::fs::write(
        nested.join(".git"),
        format!("gitdir: {}\n", elsewhere.display()),
    )
    .expect("gitfile");

    let started = Instant::now();
    let view = lab.read(&repo);
    assert_eq!(view.state, GitState::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    assert!(!json(&view).contains("elsewhere"));
    assert_eq!(
        lab.runner.0.in_flight(),
        0,
        "the child was killed and reaped"
    );
    assert_eq!(lab.scratch_left(), 0);
}

/// The names only the secret repository holds. No reply may carry one.
const SECRET_NAMES: [&str; 3] = ["secret-branch-zq", "SECRET-SUBJECT-zq", "secret-file-zq"];
/// How long a control may run. Plain git blocks on some planted files, and the planted program
/// has run by then.
const CONTROL: Duration = Duration::from_secs(15);

static LEASES: AtomicU64 = AtomicU64::new(1);

/// A lease id whose first twelve digits, and so its branch, differ from every other in the run.
fn lease_id() -> WorktreeId {
    let drawn = LEASES
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&drawn.to_be_bytes());
    bytes[8..12].copy_from_slice(&std::process::id().to_be_bytes());
    WorktreeId::from_bytes(bytes)
}

/// A worktree the daemon created, and what its lease records.
struct Leased {
    lease: Lease,
    branch: String,
}

impl Leased {
    fn tree(&self) -> Tree {
        Tree::Leased(self.lease.clone())
    }

    fn path(&self) -> &Path {
        &self.lease.path
    }

    fn origin(&self) -> &Path {
        &self.lease.origin
    }

    /// `<origin>/.git/worktrees/<lease id>`, where git keeps the tree's `HEAD` and index.
    fn admin(&self) -> PathBuf {
        self.origin()
            .join(".git/worktrees")
            .join(self.lease.worktree.to_string())
    }

    fn base(&self) -> String {
        self.lease.base.to_string()
    }
}

/// A second repository with names nothing else holds, a linked worktree of its own and a planted
/// file-system monitor: where a rewritten link file takes plain git.
struct Secret {
    git_dir: PathBuf,
    admin: PathBuf,
    tree: PathBuf,
}

impl Lab {
    fn commit_file(&self, cwd: &Path, name: &str, text: &str, message: &str) {
        std::fs::write(cwd.join(name), text).expect("a file to commit");
        self.git(cwd, &["add", "--", name]);
        self.git(cwd, &["commit", "--quiet", "-m", message]);
    }

    /// The repository a lease is added from: `main` with two commits and a clean tree.
    fn origin(&self) -> PathBuf {
        let origin = self.path().join("origin");
        std::fs::create_dir(&origin).expect("origin");
        self.git(&origin, &["init", "--quiet", "-b", "main"]);
        std::fs::write(origin.join("a.txt"), "alpha\n").expect("a");
        std::fs::write(origin.join("b.txt"), "bravo\n").expect("b");
        self.git(&origin, &["add", "-A"]);
        self.git(&origin, &["commit", "--quiet", "-m", "origin one"]);
        self.commit_file(&origin, "a.txt", "alpha two\n", "origin two");
        origin
    }

    /// A lease of `origin`, made as the daemon makes one: its own `git worktree add` of a branch
    /// named for the lease, at `HEAD` or at another lease's branch tip.
    fn lease(&self, origin: &Path, from: Option<&Leased>, worktree: WorktreeId) -> Leased {
        let git = DaemonGit::new(
            Arc::clone(&self.runner) as Arc<dyn PredicateRunner>,
            std::env::var_os("PATH"),
            Duration::from_secs(60),
        );
        let rev = from.map_or_else(
            || "HEAD".to_owned(),
            |from| format!("refs/heads/{}", from.branch),
        );
        let base = git.commit(origin, &rev).expect("the base resolves");
        let path = self
            .path()
            .join("data/worktrees")
            .join(worktree.to_string());
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the data directory");
        let branch = format!("dudes/{}", worktree.short());
        git.add(origin, &path, &branch, base)
            .expect("the daemon's worktree add");
        Leased {
            lease: Lease {
                worktree,
                origin: origin.to_path_buf(),
                path,
                base,
            },
            branch,
        }
    }

    /// Two commits in the tree, then an unstaged edit, a staged edit and an untracked file.
    fn agent_work(&self, leased: &Leased) {
        let tree = leased.path();
        self.commit_file(tree, "d.txt", "delta\n", "agent one");
        self.commit_file(tree, "e.txt", "echo\n", "agent two");
        std::fs::write(tree.join("a.txt"), "alpha agent\n").expect("an unstaged edit");
        std::fs::write(tree.join("b.txt"), "bravo staged\n").expect("a staged edit");
        self.git(tree, &["add", "--", "b.txt"]);
        std::fs::write(tree.join("c-untracked.txt"), "charlie\n").expect("an untracked file");
    }

    fn secret(&self) -> Secret {
        let repo = self.path().join("secret");
        std::fs::create_dir(&repo).expect("secret");
        self.git(&repo, &["init", "--quiet", "-b", "secret-branch-zq"]);
        self.commit_file(
            &repo,
            "secret-file-zq.txt",
            "classified\n",
            "SECRET-SUBJECT-zq",
        );
        let tree = self.path().join("secret-wt");
        self.git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "secret-wt-zq",
                "--",
                tree.to_str().expect("utf-8"),
            ],
        );
        let monitor = self.tool("secret-fsmonitor", "");
        self.git(
            &repo,
            &["config", "core.fsmonitor", monitor.to_str().expect("utf-8")],
        );
        Secret {
            git_dir: repo.join(".git"),
            admin: repo.join(".git/worktrees/secret-wt"),
            tree,
        }
    }

    /// A lease with the agent's work in it, and the secret repository beside it.
    fn leased_world(&self) -> (Leased, Secret) {
        let origin = self.origin();
        let leased = self.lease(&origin, None, lease_id());
        self.agent_work(&leased);
        (leased, self.secret())
    }

    /// Plain git in `cwd` that may block: its output, or `None` when `deadline` killed it.
    fn plain_until(&self, cwd: &Path, args: &[&str], deadline: Duration) -> Option<String> {
        match self.plain_run(cwd, args, deadline) {
            PredicateOutcome::Finished {
                timed_out: true, ..
            } => None,
            PredicateOutcome::Finished { output_tail, .. } => Some(output_tail),
            PredicateOutcome::Swept => panic!("swept"),
        }
    }

    /// Plain git in the tree as an operator would type it: the three commands a redirect changes.
    fn plain_in_tree(&self, leased: &Leased) -> String {
        [
            &["status", "--porcelain"][..],
            &["branch", "--list"],
            &["log", "-3", "--format=%s"],
        ]
        .iter()
        .map(|args| {
            self.plain_until(leased.path(), args, CONTROL)
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("\n")
    }

    /// The production read of `leased`. Nothing planted runs, and no name of the secret
    /// repository, no path under the lab and no full lease id reaches the reply.
    fn read_lease(&self, leased: &Leased) -> GitView {
        self.forget();
        let view = self.reader().read(&leased.tree(), NOW);
        assert_eq!(self.ran(), "", "the read ran a planted program");
        let wire = json(&view);
        let root = self.path().display().to_string();
        let id = leased.lease.worktree.to_string();
        for leaked in SECRET_NAMES
            .iter()
            .copied()
            .chain([root.as_str(), id.as_str()])
        {
            assert!(!wire.contains(leaked), "{leaked} in {wire}");
        }
        assert_eq!(self.scratch_left(), 0, "the shadow was removed");
        view
    }
}

/// A redirect row: plain git in the tree runs the secret repository's program and shows its
/// names, and the read answers `worktree_mismatch` without reaching it.
fn assert_redirect(lab: &Lab, leased: &Leased, row: &str) {
    lab.forget();
    let shown = lab.plain_in_tree(leased);
    assert!(
        lab.ran().contains("secret-fsmonitor"),
        "{row}: control: plain git runs the other repository's program"
    );
    assert!(
        SECRET_NAMES.iter().any(|name| shown.contains(name)),
        "{row}: control: plain git shows the other repository: {shown}"
    );
    assert_eq!(
        lab.read_lease(leased).state,
        GitState::WorktreeMismatch,
        "{row}"
    );
}

/// A row where `real` is moved aside and a link to the moved directory takes its name. Every link
/// file still resolves to the directory the lease names, so plain git reads the tree as before,
/// and only opening `real` without following a link refuses it. Returns the moved directory.
fn assert_own_link_refused(lab: &Lab, leased: &Leased, real: &Path, row: &str) -> PathBuf {
    let moved = real.with_extension("moved");
    std::fs::rename(real, &moved).expect("move the directory");
    symlink(&moved, real).expect("link to its own directory");
    let shown = lab.plain_in_tree(leased);
    assert!(
        shown.contains(&leased.branch) && shown.contains("agent two"),
        "{row}: control: plain git reads through the link: {shown}"
    );
    assert_eq!(
        lab.read_lease(leased).state,
        GitState::WorktreeMismatch,
        "{row}"
    );
    moved
}

fn subjects(view: &GitView) -> Vec<&str> {
    view.commits
        .items
        .iter()
        .map(|commit| commit.subject.as_str())
        .collect()
}

fn committed(view: &GitView) -> Vec<&str> {
    view.committed
        .items
        .iter()
        .map(|file| file.path.as_str())
        .collect()
}

fn distance(view: &GitView) -> (Option<u32>, Option<u32>, GitAgainst) {
    let head = view.head.as_ref().expect("a head");
    (head.ahead, head.behind, head.against)
}

/// A0, and the benign equivalence: the read of an untouched lease names what plain git in the tree
/// names, measured from the commit the worktree started at.
#[test]
fn a_lease_is_read_through_its_recorded_paths_and_agrees_with_plain_git_in_the_tree() {
    let lab = Lab::new();
    let origin = lab.origin();
    let leased = lab.lease(&origin, None, lease_id());
    lab.agent_work(&leased);
    assert_eq!(
        lab.read(leased.path()).state,
        GitState::GitfileUnsupported,
        "control: read as an ordinary checkout, the tree's .git is a file"
    );

    let view = lab.read_lease(&leased);
    assert_eq!(view.state, GitState::Ok);
    let head = view.head.clone().expect("a head");
    assert_eq!(head.branch.as_deref(), Some(leased.branch.as_str()));
    assert_eq!(
        distance(&view),
        (Some(2), Some(0), GitAgainst::WorktreeStart)
    );
    assert_eq!(head.untracked, 1);
    assert_eq!(subjects(&view), ["agent two", "agent one"]);
    assert_eq!(committed(&view), ["d.txt", "e.txt"]);
    let changes: Vec<_> = view
        .changes
        .items
        .iter()
        .map(|change| (change.path.as_str(), change.kind, change.staged))
        .collect();
    assert_eq!(
        changes,
        [
            ("a.txt", GitChangeKind::Modified, false),
            ("b.txt", GitChangeKind::Modified, true)
        ]
    );
    let branches: Vec<_> = view
        .branches
        .items
        .iter()
        .map(|branch| (branch.name.as_str(), branch.current))
        .collect();
    assert_eq!(branches, [(leased.branch.as_str(), true), ("main", false)]);

    let tree = leased.path();
    let base = leased.base();
    let counts = lab.git(
        tree,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{base}...HEAD"),
        ],
    );
    assert_eq!(
        counts.split_whitespace().collect::<Vec<_>>(),
        [
            head.behind.expect("behind").to_string(),
            head.ahead.expect("ahead").to_string()
        ]
    );
    let log = lab.git(tree, &["log", "--format=%s", &format!("{base}..HEAD")]);
    assert_eq!(log.lines().collect::<Vec<_>>(), subjects(&view));
    let status = lab.git(tree, &["status", "--porcelain=v1"]);
    let plain_changes: Vec<_> = status
        .lines()
        .filter(|line| !line.starts_with("??"))
        .map(|line| (&line[3..], !line.starts_with(' ')))
        .collect();
    let read_changes: Vec<_> = view
        .changes
        .items
        .iter()
        .map(|change| (change.path.as_str(), change.staged))
        .collect();
    assert_eq!(read_changes, plain_changes);
    assert_eq!(
        status.lines().filter(|line| line.starts_with("??")).count(),
        usize::try_from(head.untracked).expect("a count")
    );
}

/// A1: the tree's `.git` file names another repository's `.git`.
#[test]
fn a_gitfile_naming_another_repository_is_a_mismatch() {
    let lab = Lab::new();
    let (leased, secret) = lab.leased_world();
    std::fs::write(
        leased.path().join(".git"),
        format!("gitdir: {}\n", secret.git_dir.display()),
    )
    .expect("rewrite the gitfile");
    assert_redirect(&lab, &leased, "A1");
}

/// A2: the tree's `.git` file names another repository's linked-worktree directory.
#[test]
fn a_gitfile_naming_another_repositorys_worktree_directory_is_a_mismatch() {
    let lab = Lab::new();
    let (leased, secret) = lab.leased_world();
    std::fs::write(
        leased.path().join(".git"),
        format!("gitdir: {}\n", secret.admin.display()),
    )
    .expect("rewrite the gitfile");
    assert_redirect(&lab, &leased, "A2");
}

/// A3: the administrative directory's `commondir` names another repository's `.git`.
#[test]
fn a_commondir_naming_another_repository_is_a_mismatch() {
    let lab = Lab::new();
    let (leased, secret) = lab.leased_world();
    std::fs::write(
        leased.admin().join("commondir"),
        format!("{}\n", secret.git_dir.display()),
    )
    .expect("rewrite commondir");
    assert_redirect(&lab, &leased, "A3");
}

/// A4: `<admin>/gitdir` steers only `git worktree` housekeeping, so rewriting it changes nothing
/// plain git shows and nothing the read shows, until a prune deletes the directory.
#[test]
fn a_rewritten_admin_gitdir_changes_nothing_until_a_prune_removes_the_directory() {
    let lab = Lab::new();
    let (leased, _) = lab.leased_world();
    let elsewhere = lab.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("elsewhere");
    std::fs::write(
        leased.admin().join("gitdir"),
        format!("{}\n", elsewhere.join(".git").display()),
    )
    .expect("rewrite gitdir");
    let shown = lab.plain_in_tree(&leased);
    assert!(
        shown.contains(&leased.branch) && shown.contains("agent two"),
        "control: plain git in the tree is unchanged: {shown}"
    );
    let view = lab.read_lease(&leased);
    assert_eq!(view.state, GitState::Ok);
    assert_eq!(
        distance(&view),
        (Some(2), Some(0), GitAgainst::WorktreeStart)
    );

    lab.git(leased.origin(), &["worktree", "prune"]);
    assert!(
        !leased.admin().exists(),
        "control: the prune deleted the administrative directory"
    );
    assert_eq!(
        lab.plain(leased.path(), &["status", "--porcelain"]).0,
        Some(128),
        "control: plain git in the tree fails"
    );
    assert_eq!(lab.read_lease(&leased).state, GitState::WorktreeMismatch);
}

/// A5 and A5b: a per-worktree configuration, with the extension on, at format version 1 and 0.
#[test]
fn a_per_worktree_configuration_runs_nothing() {
    for format in ["1", "0"] {
        let lab = Lab::new();
        let (leased, _) = lab.leased_world();
        let origin = leased.origin();
        let monitor = lab.tool("worktree-fsmonitor", "");
        let config = if format == "1" {
            let filter = lab.tool("worktree-filter", "cat");
            let pager = lab.tool("worktree-pager", "cat");
            std::fs::write(origin.join(".git/info/attributes"), "* filter=evil\n")
                .expect("attributes");
            std::fs::write(leased.path().join("d.txt"), "DELTA\n").expect("an edit to hash");
            format!(
                "[core]\n\tfsmonitor = {}\n\tpager = {}\n[filter \"evil\"]\n\tclean = {}\n",
                monitor.display(),
                pager.display(),
                filter.display()
            )
        } else {
            format!("[core]\n\tfsmonitor = {}\n", monitor.display())
        };
        lab.git(origin, &["config", "core.repositoryformatversion", format]);
        lab.git(origin, &["config", "extensions.worktreeConfig", "true"]);
        std::fs::write(leased.admin().join("config.worktree"), config).expect("config.worktree");
        lab.forget();
        lab.plain_until(leased.path(), &["status", "--porcelain"], CONTROL);
        assert!(
            lab.ran().contains("worktree-fsmonitor"),
            "format {format}: control: plain git runs the per-worktree monitor"
        );
        let view = lab.read_lease(&leased);
        assert_eq!(view.state, GitState::Ok, "format {format}");
        assert_eq!(
            subjects(&view),
            ["agent two", "agent one"],
            "format {format}"
        );
    }
}

/// A6: `<common>/worktrees/<id>` is a link to another repository's worktree directory.
#[test]
fn a_linked_administrative_directory_is_a_mismatch() {
    let lab = Lab::new();
    let (leased, secret) = lab.leased_world();
    let admin = leased.admin();
    std::fs::rename(&admin, admin.with_extension("moved")).expect("move the directory");
    symlink(&secret.admin, &admin).expect("plant the link");
    assert_redirect(&lab, &leased, "A6");
}

/// A6, with `<common>/worktrees/<id>` a link to its own moved directory.
#[test]
fn an_administrative_directory_link_to_its_own_moved_directory_is_a_mismatch() {
    let lab = Lab::new();
    let (leased, _) = lab.leased_world();
    assert_own_link_refused(&lab, &leased, &leased.admin(), "A6");
}

/// A7: `<common>/worktrees` is a link to a planted copy whose `commondir` names another repository.
#[test]
fn a_linked_worktrees_directory_is_a_mismatch() {
    let lab = Lab::new();
    let (leased, secret) = lab.leased_world();
    let worktrees = leased.origin().join(".git/worktrees");
    let planted = lab.path().join("planted-worktrees");
    lab.sh(
        &lab.path(),
        &format!("cp -R '{}' '{}'", worktrees.display(), planted.display()),
    );
    std::fs::write(
        planted
            .join(leased.lease.worktree.to_string())
            .join("commondir"),
        format!("{}\n", secret.git_dir.display()),
    )
    .expect("plant commondir");
    std::fs::rename(&worktrees, worktrees.with_extension("moved")).expect("move worktrees");
    symlink(&planted, &worktrees).expect("plant the link");
    assert_redirect(&lab, &leased, "A7");
}

/// A7, with `<common>/worktrees` a link to its own moved directory.
#[test]
fn a_worktrees_directory_link_to_its_own_moved_directory_is_a_mismatch() {
    let lab = Lab::new();
    let (leased, _) = lab.leased_world();
    assert_own_link_refused(&lab, &leased, &leased.origin().join(".git/worktrees"), "A7");
}

/// A8: `<origin>/.git` is a link: to its own moved directory, which plain git reads normally, and
/// then to another repository holding copies of the tree's files.
#[test]
fn a_linked_origin_git_directory_is_a_mismatch_even_when_it_is_the_repositorys_own() {
    let lab = Lab::new();
    let (leased, secret) = lab.leased_world();
    let real = leased.origin().join(".git");
    let moved = assert_own_link_refused(&lab, &leased, &real, "A8");

    std::fs::remove_file(&real).expect("drop the link");
    symlink(&secret.git_dir, &real).expect("link to the other repository");
    let id = leased.lease.worktree.to_string();
    let copied = secret.git_dir.join("worktrees").join(&id);
    std::fs::create_dir(&copied).expect("a copied directory");
    for name in ["HEAD", "commondir", "gitdir", "index"] {
        std::fs::copy(
            moved.join("worktrees").join(&id).join(name),
            copied.join(name),
        )
        .expect("copy the tree's file");
    }
    assert_redirect(&lab, &leased, "A8");
}

/// A9: `<admin>/HEAD` is a FIFO. Plain git blocks on it; the read refuses it at once.
#[test]
fn a_fifo_admin_head_is_refused_at_once() {
    let lab = Lab::new();
    let (leased, _) = lab.leased_world();
    let head = leased.admin().join("HEAD");
    std::fs::remove_file(&head).expect("remove HEAD");
    nix::unistd::mkfifo(&head, nix::sys::stat::Mode::from_bits_truncate(0o600)).expect("mkfifo");
    assert!(
        lab.plain_until(
            leased.path(),
            &["status", "--porcelain"],
            Duration::from_secs(3)
        )
        .is_none(),
        "control: plain git blocks on the FIFO"
    );
    let started = Instant::now();
    assert_eq!(lab.read_lease(&leased).state, GitState::Unreadable);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

/// A10: `<admin>/HEAD` names a per-worktree ref, which the read does not accept.
#[test]
fn an_admin_head_naming_a_per_worktree_ref_is_refused() {
    let lab = Lab::new();
    let (leased, _) = lab.leased_world();
    std::fs::write(
        leased.admin().join("HEAD"),
        "ref: refs/worktree/elsewhere\n",
    )
    .expect("rewrite HEAD");
    let (exit, shown) = lab.plain(leased.path(), &["status", "--porcelain"]);
    assert!(
        exit == Some(0) && shown.contains("A  d.txt"),
        "control: plain git shows every file as added: {exit:?} {shown}"
    );
    assert_eq!(lab.read_lease(&leased).state, GitState::Unreadable);
}

/// A11: a monitor, a clean filter, a pager and an external diff in `<common>/config`, the filter
/// bound in `<common>/info/attributes`, and two hooks.
#[test]
fn hostile_configuration_attributes_and_hooks_in_the_common_directory_run_nothing() {
    let lab = Lab::new();
    let (leased, _) = lab.leased_world();
    let common = leased.origin().join(".git");
    for (key, name, tail) in [
        ("core.fsmonitor", "common-fsmonitor", ""),
        ("filter.evil.clean", "common-filter", "cat"),
        ("core.pager", "common-pager", "cat"),
        ("diff.external", "common-diff-external", ""),
    ] {
        let tool = lab.tool(name, tail);
        lab.git(
            leased.origin(),
            &["config", key, tool.to_str().expect("utf-8")],
        );
    }
    std::fs::write(common.join("info/attributes"), "* filter=evil\n").expect("attributes");
    for hook in ["post-index-change", "reference-transaction"] {
        std::fs::rename(lab.tool(hook, ""), common.join("hooks").join(hook)).expect("hook");
    }
    std::fs::write(leased.path().join("d.txt"), "DELTA\n").expect("an edit to hash");
    lab.forget();
    lab.plain_until(leased.path(), &["status", "--porcelain"], CONTROL);
    lab.plain_until(leased.path(), &["diff", "HEAD~1"], CONTROL);
    let ran = lab.ran();
    assert!(
        ran.contains("common-fsmonitor") && ran.contains("common-diff-external"),
        "control: plain git runs the planted programs: {ran}"
    );
    let view = lab.read_lease(&leased);
    assert_eq!(view.state, GitState::Ok);
    assert!(
        view.changes
            .items
            .iter()
            .any(|change| change.path == "d.txt"),
        "the read still reports the edit"
    );
}

/// A12: a gitlink in the tree whose nested repository plants a monitor. The shadow's own status
/// without `--ignore-submodules=all` runs it, which is why the flag is a security control.
#[test]
fn a_gitlink_in_a_lease_does_not_run_its_nested_configuration() {
    let lab = Lab::new();
    let (leased, _) = lab.leased_world();
    let nested = leased.path().join("nested");
    std::fs::create_dir(&nested).expect("nested");
    lab.git(&nested, &["init", "--quiet", "-b", "main"]);
    lab.commit_file(&nested, "n.txt", "nested\n", "nested one");
    lab.git(leased.path(), &["add", "nested"]);
    lab.git(
        leased.path(),
        &["commit", "--quiet", "-m", "agent adds a gitlink"],
    );
    let monitor = lab.tool("nested-fsmonitor", "");
    lab.git(
        &nested,
        &["config", "core.fsmonitor", monitor.to_str().expect("utf-8")],
    );
    std::fs::write(nested.join("n.txt"), "nested edit\n").expect("dirty the nested repository");
    lab.forget();
    lab.plain_until(leased.path(), &["status", "--porcelain"], CONTROL);
    assert!(
        lab.ran().contains("nested-fsmonitor"),
        "control: plain git runs the nested monitor"
    );

    let reader = lab.reader();
    let shadow = reader.shadow(&leased.tree()).expect("a shadow");
    let spec = reader
        .launch(&shadow, &["status", "--porcelain=v1", "-z"])
        .expect("launch");
    lab.forget();
    lab.runner
        .run_captured(&spec, Duration::from_secs(20), 64 * 1024)
        .expect("status without the flag runs");
    assert!(
        lab.ran().contains("nested-fsmonitor"),
        "positive control: the shadow's status without --ignore-submodules=all runs it"
    );
    drop(shadow);

    let view = lab.read_lease(&leased);
    assert_eq!(view.state, GitState::Ok);
    assert!(
        view.changes
            .items
            .iter()
            .all(|change| change.path != "nested")
    );
}

/// A13: the tree itself is a link to another repository's linked worktree.
#[test]
fn a_tree_that_is_a_link_is_a_mismatch() {
    let lab = Lab::new();
    let (leased, secret) = lab.leased_world();
    let tree = leased.path();
    std::fs::rename(tree, tree.with_extension("moved")).expect("move the tree");
    symlink(&secret.tree, tree).expect("plant the link");
    assert_redirect(&lab, &leased, "A13");
}

/// A13, with the tree a link to its own moved directory.
#[test]
fn a_tree_link_to_its_own_moved_directory_is_a_mismatch() {
    let lab = Lab::new();
    let (leased, _) = lab.leased_world();
    assert_own_link_refused(&lab, &leased, leased.path(), "A13");
}

/// What else answers `worktree_mismatch` or a storage refusal without a read: a missing
/// `commondir`, a gitfile with a second line, a gitfile that is a FIFO, a missing tree, and
/// `reftable` in either directory.
#[test]
fn a_lease_whose_files_are_missing_or_malformed_is_refused() {
    type Plant = fn(&Leased);
    let rows: [(&str, Plant, GitState); 6] = [
        (
            "no commondir",
            |leased| std::fs::remove_file(leased.admin().join("commondir")).expect("remove"),
            GitState::WorktreeMismatch,
        ),
        (
            "a gitfile with a second line",
            |leased| {
                std::fs::write(
                    leased.path().join(".git"),
                    format!("gitdir: {}\nmore\n", leased.admin().display()),
                )
                .expect("rewrite");
            },
            GitState::WorktreeMismatch,
        ),
        (
            "a gitfile that is a FIFO",
            |leased| {
                let gitfile = leased.path().join(".git");
                std::fs::remove_file(&gitfile).expect("remove");
                nix::unistd::mkfifo(&gitfile, nix::sys::stat::Mode::from_bits_truncate(0o600))
                    .expect("mkfifo");
            },
            GitState::WorktreeMismatch,
        ),
        (
            "no tree",
            |leased| std::fs::remove_dir_all(leased.path()).expect("remove the tree"),
            GitState::WorktreeMismatch,
        ),
        (
            "reftable in the administrative directory",
            |leased| std::fs::create_dir(leased.admin().join("reftable")).expect("reftable"),
            GitState::UnsupportedStorage,
        ),
        (
            "reftable in the common directory",
            |leased| {
                std::fs::create_dir(leased.origin().join(".git/reftable")).expect("reftable");
            },
            GitState::UnsupportedStorage,
        ),
    ];
    for (row, plant, state) in rows {
        let lab = Lab::new();
        let origin = lab.origin();
        let leased = lab.lease(&origin, None, lease_id());
        assert_eq!(
            lab.read_lease(&leased).state,
            GitState::Ok,
            "{row}: control: the lease reads before the plant"
        );
        plant(&leased);
        let started = Instant::now();
        assert_eq!(lab.read_lease(&leased).state, state, "{row}");
        assert!(started.elapsed() < Duration::from_secs(2), "{row}");
    }
}

/// B1: an origin with no remote has no remote default branch, and the start commit still measures
/// the agent's own work.
#[test]
fn an_origin_with_no_remote_is_measured_from_the_start_commit() {
    let lab = Lab::new();
    let (leased, _) = lab.leased_world();
    assert_ne!(
        lab.plain(
            leased.path(),
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/remotes/origin/HEAD"
            ]
        )
        .0,
        Some(0),
        "control: there is no remote default branch to compare with"
    );
    let view = lab.read_lease(&leased);
    assert_eq!(
        distance(&view),
        (Some(2), Some(0), GitAgainst::WorktreeStart)
    );
    assert_eq!(subjects(&view), ["agent two", "agent one"]);
}

/// B2: the remote default branch moved after the hire. Against it the agent is one behind; against
/// its start it is not.
#[test]
fn a_remote_branch_that_moved_after_the_hire_does_not_change_the_distance() {
    let lab = Lab::new();
    let upstream = lab.path().join("upstream");
    std::fs::create_dir(&upstream).expect("upstream");
    lab.git(&upstream, &["init", "--quiet", "-b", "main"]);
    lab.commit_file(&upstream, "a.txt", "alpha\n", "upstream one");
    let origin = lab.path().join("origin");
    lab.git(
        &lab.path(),
        &[
            "clone",
            "--quiet",
            "--",
            upstream.to_str().expect("utf-8"),
            origin.to_str().expect("utf-8"),
        ],
    );
    let leased = lab.lease(&origin, None, lease_id());
    lab.agent_work(&leased);
    lab.commit_file(&upstream, "u.txt", "upstream\n", "upstream two");
    lab.git(&origin, &["fetch", "--quiet", "origin"]);
    let counts = lab.git(
        leased.path(),
        &[
            "rev-list",
            "--left-right",
            "--count",
            "refs/remotes/origin/HEAD...HEAD",
        ],
    );
    assert_eq!(
        counts.split_whitespace().collect::<Vec<_>>(),
        ["1", "2"],
        "control: against the remote default branch it is two ahead and one behind"
    );
    let view = lab.read_lease(&leased);
    assert_eq!(
        distance(&view),
        (Some(2), Some(0), GitAgainst::WorktreeStart)
    );
    assert_eq!(subjects(&view), ["agent two", "agent one"]);
}

/// B3: a handoff. B starts at A's tip, so each lists only its own commits, even after A commits
/// again.
#[test]
fn a_handoff_lists_only_each_agents_own_commits() {
    let lab = Lab::new();
    let origin = lab.origin();
    let first = lab.lease(&origin, None, lease_id());
    lab.commit_file(first.path(), "a1.txt", "a1\n", "A one");
    lab.commit_file(first.path(), "a2.txt", "a2\n", "A two");
    let second = lab.lease(&origin, Some(&first), lease_id());
    assert_eq!(
        second.base(),
        lab.git(
            &origin,
            &["rev-parse", &format!("refs/heads/{}", first.branch)]
        )
        .trim(),
        "control: B's recorded start is A's tip"
    );
    lab.commit_file(second.path(), "b1.txt", "b1\n", "B one");
    lab.commit_file(first.path(), "a3.txt", "a3\n", "A three");

    let a = lab.read_lease(&first);
    assert_eq!(distance(&a), (Some(3), Some(0), GitAgainst::WorktreeStart));
    assert_eq!(subjects(&a), ["A three", "A two", "A one"]);
    assert_eq!(committed(&a), ["a1.txt", "a2.txt", "a3.txt"]);
    let b = lab.read_lease(&second);
    assert_eq!(distance(&b), (Some(1), Some(0), GitAgainst::WorktreeStart));
    assert_eq!(subjects(&b), ["B one"]);
    assert_eq!(committed(&b), ["b1.txt"]);
}

/// The agent moves its branch onto an amended `main`: what B4 and B5 share.
fn move_onto_amended_main(lab: &Lab, leased: &Leased, prune: bool) {
    lab.git(
        leased.origin(),
        &["commit", "--quiet", "--amend", "-m", "origin two amended"],
    );
    if prune {
        lab.git(leased.path(), &["stash", "--quiet", "--include-untracked"]);
    }
    lab.git(
        leased.path(),
        &["checkout", "--quiet", "-B", &leased.branch, "main"],
    );
    lab.commit_file(leased.path(), "after.txt", "after\n", "agent after moving");
    if prune {
        lab.git(leased.path(), &["stash", "drop", "--quiet"]);
        lab.git(
            leased.origin(),
            &["reflog", "expire", "--expire=now", "--all"],
        );
        lab.git(
            leased.path(),
            &["reflog", "expire", "--expire=now", "--all"],
        );
        lab.git(leased.origin(), &["gc", "--quiet", "--prune=now"]);
    }
}

/// B4: behind counts what the start commit has and `HEAD` lacks, which an agent that moved its
/// branch off its start produces, and the committed files include the operator's amended change.
#[test]
fn an_agent_that_moved_its_branch_off_its_start_is_behind() {
    let lab = Lab::new();
    let (leased, _) = lab.leased_world();
    move_onto_amended_main(&lab, &leased, false);
    let view = lab.read_lease(&leased);
    assert_eq!(
        distance(&view),
        (Some(2), Some(1), GitAgainst::WorktreeStart)
    );
    assert_eq!(
        subjects(&view),
        ["agent after moving", "origin two amended"]
    );
    assert_eq!(committed(&view), ["a.txt", "after.txt", "b.txt"]);
}

/// B5: once the start commit is pruned there is no distance and no committed section, and the read
/// does not fall back to another line.
#[test]
fn a_start_commit_that_is_gone_shows_no_distance() {
    let lab = Lab::new();
    let (leased, _) = lab.leased_world();
    move_onto_amended_main(&lab, &leased, true);
    assert_eq!(
        lab.plain(
            leased.origin(),
            &["cat-file", "-e", &format!("{}^{{commit}}", leased.base())]
        )
        .0,
        Some(128),
        "control: the start commit is gone"
    );
    let view = lab.read_lease(&leased);
    assert_eq!(view.state, GitState::Ok);
    assert_eq!(distance(&view), (None, None, GitAgainst::WorktreeStart));
    assert_eq!(
        subjects(&view),
        ["agent after moving", "origin two amended", "origin one"]
    );
    assert!(committed(&view).is_empty());
}

/// A directory with the lease id's name before the add makes git name the tree's directory with a
/// suffix. A planted, non-empty one does on every measured git, and the read then never reads the
/// lease. An empty one is suffixed by older git and may be reused by newer git, and the read
/// follows what git did.
#[test]
fn a_lease_whose_administrative_directory_took_a_suffix_is_never_read() {
    let lab = Lab::new();
    let origin = lab.origin();
    let secret = lab.secret();
    let id = lease_id();
    let planted = origin.join(".git/worktrees").join(id.to_string());
    std::fs::create_dir_all(&planted).expect("a planted directory");
    std::fs::write(
        planted.join("commondir"),
        format!("{}\n", secret.git_dir.display()),
    )
    .expect("a planted commondir");
    std::fs::write(planted.join("planted.txt"), "planted\n").expect("a planted file");
    let leased = lab.lease(&origin, None, id);
    let gitfile = std::fs::read_to_string(leased.path().join(".git")).expect("the gitfile");
    assert!(
        gitfile.trim_end().ends_with(&format!("{id}1")),
        "control: git suffixed the directory: {gitfile}"
    );
    let (exit, shown) = lab.plain(leased.path(), &["branch", "--list"]);
    assert!(
        exit == Some(0) && shown.contains(&leased.branch),
        "control: plain git in the tree works: {shown}"
    );
    assert_eq!(lab.read_lease(&leased).state, GitState::WorktreeMismatch);

    let lab = Lab::new();
    let origin = lab.origin();
    let id = lease_id();
    std::fs::create_dir_all(origin.join(".git/worktrees").join(id.to_string()))
        .expect("an empty directory");
    let leased = lab.lease(&origin, None, id);
    let gitfile = std::fs::read_to_string(leased.path().join(".git")).expect("the gitfile");
    let reused = gitfile.trim_end().ends_with(&id.to_string());
    let expected = if reused {
        GitState::Ok
    } else {
        GitState::WorktreeMismatch
    };
    assert_eq!(lab.read_lease(&leased).state, expected, "{gitfile}");
}

/// Relative links, which newer git writes when the origin sets `worktree.useRelativePaths`. An
/// older git ignores that setting, so the test writes the relative gitfile itself.
#[test]
fn relative_links_are_read() {
    let lab = Lab::new();
    let origin = lab.origin();
    lab.git(&origin, &["config", "worktree.useRelativePaths", "true"]);
    let leased = lab.lease(&origin, None, lease_id());
    lab.agent_work(&leased);
    let gitfile = leased.path().join(".git");
    let written = std::fs::read_to_string(&gitfile).expect("the gitfile");
    let named = written
        .trim_end()
        .strip_prefix("gitdir: ")
        .expect("a gitfile");
    if Path::new(named).is_absolute() {
        std::fs::write(
            &gitfile,
            format!(
                "gitdir: ../../../origin/.git/worktrees/{}\n",
                leased.lease.worktree
            ),
        )
        .expect("a relative gitfile");
    }
    assert_eq!(
        std::fs::read_to_string(leased.admin().join("commondir")).expect("commondir"),
        "../..\n"
    );
    assert_eq!(
        lab.plain(leased.path(), &["status", "--porcelain"]).0,
        Some(0),
        "control: plain git follows the relative links"
    );
    let view = lab.read_lease(&leased);
    assert_eq!(view.state, GitState::Ok);
    assert_eq!(
        distance(&view),
        (Some(2), Some(0), GitAgainst::WorktreeStart)
    );
}
