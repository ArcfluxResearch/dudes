//! Real Git observations of the bounded checkout reader under a scratch HOME.

#![cfg(unix)]

use std::ffi::OsString;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use duded::git_read::{
    Tree,
    diff::{BaselineCapture, DiffReader},
};
use duded::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner};
use dudes_host::{CapturedOutcome, OneShot, OneShotOutcome};
use dudes_proto::{
    DiffAvailability, DiffBinding, DiffFreshness, DiffKind, DiffOmissionReason, DiffRefusal,
    DiffResponse,
};
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_types::{AgentId, CardId, GoalId, Seq, Timestamp};

struct Real {
    host: OneShot,
    commands: AtomicUsize,
    argv: Mutex<Vec<Vec<OsString>>>,
    cut_listing: AtomicBool,
    new_files_time_out: AtomicBool,
    exhaust: AtomicBool,
}
impl PredicateRunner for Real {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<PredicateOutcome> {
        self.host.run(spec, timeout).map(|result| match result {
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
        cap: usize,
    ) -> io::Result<PredicateCaptured> {
        self.commands.fetch_add(1, Ordering::Relaxed);
        self.argv.lock().unwrap().push(spec.argv.clone());
        if self.exhaust.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(400));
            return Err(io::Error::other("the program was lost"));
        }
        if self.new_files_time_out.load(Ordering::Relaxed)
            && spec
                .argv
                .get(3)
                .is_some_and(|command| command == "ls-files")
        {
            return Ok(PredicateCaptured::Finished {
                exit: None,
                stdout: Vec::new(),
                truncated: false,
                timed_out: true,
            });
        }
        self.host
            .run_captured(spec, timeout, cap)
            .map(|result| match result {
                CapturedOutcome::Finished {
                    exit,
                    mut stdout,
                    mut truncated,
                    timed_out,
                } => {
                    if self.cut_listing.load(Ordering::Relaxed)
                        && spec
                            .argv
                            .get(3)
                            .is_some_and(|command| command == "diff-index")
                    {
                        let end = stdout
                            .iter()
                            .enumerate()
                            .filter(|(_, byte)| **byte == 0)
                            .nth(1)
                            .map(|(position, _)| position + 1);
                        if let Some(end) = end {
                            stdout.truncate(end);
                        }
                        truncated = true;
                    }
                    PredicateCaptured::Finished {
                        exit,
                        stdout,
                        truncated,
                        timed_out,
                    }
                }
                CapturedOutcome::Swept => PredicateCaptured::Swept,
            })
    }
    fn sweep(&self) {
        self.host.sweep();
    }
}

struct Lab {
    root: tempfile::TempDir,
    real: Arc<Real>,
    reader: DiffReader,
    git: PathBuf,
}
impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let real = Arc::new(Real {
            host: OneShot::new(),
            commands: AtomicUsize::new(0),
            argv: Mutex::new(Vec::new()),
            cut_listing: AtomicBool::new(false),
            new_files_time_out: AtomicBool::new(false),
            exhaust: AtomicBool::new(false),
        });
        let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join("git"))
            .find(|path| path.is_file())
            .unwrap();
        let reader = DiffReader::new(
            real.clone(),
            std::env::var_os("PATH"),
            &root.path().join("scratch"),
        );
        let lab = Self {
            root,
            real,
            reader,
            git,
        };
        std::fs::create_dir(lab.repo()).unwrap();
        std::fs::create_dir(lab.root.path().join("home")).unwrap();
        lab.git(&["init", "--quiet", "-b", "main"]);
        lab.write("a.txt", "old a\n");
        lab.write("b.txt", "old b\n");
        lab.git(&["add", "-A"]);
        lab.git(&["commit", "--quiet", "-m", "initial"]);
        lab
    }
    fn redacting(self, redactor: dudes_supervisor::Redactor) -> Self {
        Self {
            reader: self.reader.with_redactor(redactor),
            ..self
        }
    }
    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"]).trim().to_owned()
    }
    fn repo(&self) -> PathBuf {
        self.root.path().join("repo")
    }
    fn write(&self, path: &str, text: &str) {
        std::fs::write(self.repo().join(path), text).unwrap();
    }
    fn env(&self) -> Vec<EnvVar> {
        vec![
            EnvVar::new("PATH", std::env::var_os("PATH").unwrap_or_default()),
            EnvVar::new("HOME", self.root.path().join("home")),
            EnvVar::new("GIT_CONFIG_NOSYSTEM", "1"),
            EnvVar::new("GIT_CONFIG_GLOBAL", "/dev/null"),
            // A commit otherwise detaches a background maintenance run that briefly adds a lock file to the object directory.
            EnvVar::new("GIT_CONFIG_COUNT", "1"),
            EnvVar::new("GIT_CONFIG_KEY_0", "maintenance.auto"),
            EnvVar::new("GIT_CONFIG_VALUE_0", "false"),
            EnvVar::new("GIT_AUTHOR_NAME", "Fixture"),
            EnvVar::new("GIT_AUTHOR_EMAIL", "fixture@example.invalid"),
            EnvVar::new("GIT_COMMITTER_NAME", "Fixture"),
            EnvVar::new("GIT_COMMITTER_EMAIL", "fixture@example.invalid"),
        ]
    }
    fn plain(&self, args: &[&str]) -> (Option<i32>, String) {
        let mut command = vec![self.git.as_os_str().to_owned()];
        command.extend(args.iter().map(OsString::from));
        let spec = LaunchSpec {
            argv: command,
            env: self.env(),
            cwd: self.repo(),
            stdin: LaunchInput::Null,
        };
        match self.real.run(&spec, Duration::from_secs(120)).unwrap() {
            PredicateOutcome::Finished {
                exit, output_tail, ..
            } => (exit, output_tail),
            PredicateOutcome::Swept => panic!("swept"),
        }
    }
    fn git(&self, args: &[&str]) -> String {
        let (exit, text) = self.plain(args);
        assert_eq!(exit, Some(0), "git {args:?}: {text}");
        text
    }
    fn tool(&self, name: &str, tail: &str) -> PathBuf {
        let program = self.root.path().join(name);
        let log = self
            .root
            .path()
            .join("programs.log")
            .to_string_lossy()
            .replace('\'', "'\\''");
        std::fs::write(
            &program,
            format!("#!/bin/sh\nprintf '{name}\\n' >> '{log}'\n{tail}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }
    fn log(&self) -> String {
        std::fs::read_to_string(self.root.path().join("programs.log")).unwrap_or_default()
    }
    fn clear_log(&self) {
        std::fs::write(self.root.path().join("programs.log"), "").unwrap();
    }
    fn tree(&self) -> Tree {
        Tree::Checkout(self.repo())
    }
    fn baseline(&self) -> BaselineCapture {
        self.reader
            .capture_baseline(&self.tree(), cancel())
            .unwrap()
    }
    fn files(&self, baseline: &BaselineCapture) -> dudes_proto::DiffContent {
        let capture = self
            .reader
            .capture_review(&self.tree(), baseline, cancel())
            .unwrap();
        let manifest = capture
            .into_manifest(binding(), Timestamp::UNIX_EPOCH, DiffFreshness::Unknown)
            .unwrap();
        let DiffResponse::Manifest(page) =
            serde_json::from_slice(manifest.page(None).unwrap().bytes()).unwrap()
        else {
            panic!("page")
        };
        let ids = page
            .entries
            .iter()
            .filter_map(|entry| entry.entry_id.clone())
            .collect::<Vec<_>>();
        let DiffResponse::Content(content) = serde_json::from_slice(
            self.reader
                .read_content(&self.tree(), baseline, &manifest, &ids, cancel())
                .unwrap()
                .bytes(),
        )
        .unwrap() else {
            panic!("content")
        };
        content
    }
}
fn cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}
fn binding() -> DiffBinding {
    DiffBinding {
        card: CardId::NIL,
        seq: Seq(1),
        round: 1,
        goal: GoalId::NIL,
        claimant: AgentId::NIL,
        check_lsn: None,
        baseline_id: "a".repeat(64),
        target_id: "b".repeat(64),
        sampler_version: 2,
        policy_version: 2,
        preexisting_tracked: false,
        preexisting_untracked: false,
    }
}

#[test]
fn baseline_and_current_text_include_committed_staged_and_uncommitted_edits() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    assert!(!baseline.preexisting_tracked);
    lab.write("a.txt", "committed a\n");
    lab.git(&["commit", "--quiet", "-a", "-m", "edit"]);
    lab.write("a.txt", "current a\n");
    lab.write("new.txt", "new\n");
    lab.git(&["add", "new.txt"]);
    std::fs::remove_file(lab.repo().join("b.txt")).unwrap();
    lab.write("untracked-private-name", "not admitted\n");
    let capture = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap();
    assert_eq!(capture.untracked, 0);
    assert!(capture.complete);
    let files = lab.files(&baseline).files;
    assert_eq!(
        files
            .iter()
            .map(|file| (
                file.path.as_str(),
                file.kind,
                file.old.as_str(),
                file.current.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("a.txt", DiffKind::Modified, "old a\n", "current a\n"),
            ("b.txt", DiffKind::Deleted, "old b\n", ""),
            ("new.txt", DiffKind::Added, "", "new\n"),
            (
                "untracked-private-name",
                DiffKind::Added,
                "",
                "not admitted\n"
            )
        ]
    );
}

#[test]
fn exact_blob_rename_pairs_but_modified_add_delete_does_not_guess() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    lab.git(&["mv", "a.txt", "renamed.txt"]);
    lab.git(&["rm", "--quiet", "b.txt"]);
    lab.write("other.txt", "different\n");
    lab.git(&["add", "other.txt"]);
    let files = lab.files(&baseline).files;
    assert_eq!(
        files
            .iter()
            .map(|file| (file.path.as_str(), file.kind, file.old_path.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            ("b.txt", DiffKind::Deleted, None),
            ("other.txt", DiffKind::Added, None),
            ("renamed.txt", DiffKind::Renamed, Some("a.txt"))
        ]
    );
}

#[test]
fn dirty_round_start_is_warned_and_unavailable_objects_never_choose_a_new_base() {
    let lab = Lab::new();
    lab.write("a.txt", "dirty before\n");
    let mut baseline = lab.baseline();
    assert!(baseline.preexisting_tracked);
    baseline.head = "1111111111111111111111111111111111111111".parse().unwrap();
    assert!(matches!(
        lab.reader.capture_review(&lab.tree(), &baseline, cancel()),
        Err(DiffRefusal::BaselineUnavailable)
    ));
}

#[test]
fn replaced_root_and_changed_index_invalidate_without_returning_old_selections() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    lab.write("a.txt", "changed\n");
    let manifest = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap()
        .into_manifest(binding(), Timestamp::UNIX_EPOCH, DiffFreshness::Unknown)
        .unwrap();
    let DiffResponse::Manifest(page) =
        serde_json::from_slice(manifest.page(None).unwrap().bytes()).unwrap()
    else {
        panic!("manifest")
    };
    let ids = page
        .entries
        .iter()
        .filter_map(|entry| entry.entry_id.clone())
        .collect::<Vec<_>>();
    lab.write("new.txt", "new\n");
    lab.git(&["add", "new.txt"]);
    assert!(matches!(
        lab.reader
            .read_content(&lab.tree(), &baseline, &manifest, &ids, cancel()),
        Err(DiffRefusal::StaleManifest)
    ));
    std::fs::rename(lab.repo(), lab.root.path().join("old-repo")).unwrap();
    std::fs::create_dir(lab.repo()).unwrap();
    assert!(matches!(
        lab.reader.capture_review(&lab.tree(), &baseline, cancel()),
        Err(DiffRefusal::RootMismatch)
    ));
}

fn vector(name: &str, plant: impl FnOnce(&Lab), plain: &[&str]) {
    let lab = Lab::new();
    let baseline = lab.baseline();
    lab.write("a.txt", "changed current\n");
    plant(&lab);
    lab.plain(plain);
    assert!(
        lab.log().contains(name),
        "ordinary Git must execute the live {name} control"
    );
    lab.clear_log();
    lab.real.argv.lock().unwrap().clear();
    let capture = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap();
    assert_eq!(
        capture
            .candidates
            .iter()
            .map(|candidate| candidate.entry.path.as_str())
            .filter(|path| *path != ".gitattributes")
            .collect::<Vec<_>>(),
        ["a.txt"]
    );
    assert_eq!(lab.log(), "", "bounded reader executed {name}");
    let mut commands = lab
        .real
        .argv
        .lock()
        .unwrap()
        .iter()
        .filter_map(|argv| {
            argv.get(3)
                .and_then(|value| value.to_str())
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    commands.sort();
    commands.dedup();
    assert_eq!(
        commands,
        ["cat-file", "diff-files", "diff-index", "ls-files"],
        "each new protected argv ran under the live control"
    );
    for argv in lab.real.argv.lock().unwrap().iter() {
        let words = argv
            .iter()
            .filter_map(|word| word.to_str())
            .collect::<Vec<_>>();
        match words.get(3).copied() {
            Some("diff-index") => assert_eq!(
                words[1..],
                [
                    "--no-pager",
                    "--no-optional-locks",
                    "diff-index",
                    "--cached",
                    "--raw",
                    "-z",
                    "--no-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    lab.head().as_str()
                ]
            ),
            Some("diff-files") => assert_eq!(
                words[1..],
                [
                    "--no-pager",
                    "--no-optional-locks",
                    "diff-files",
                    "--raw",
                    "-z",
                    "--no-renames",
                    "--ignore-submodules=all",
                    "--no-ext-diff",
                    "--no-textconv"
                ]
            ),
            Some("ls-files") => assert_eq!(
                words[1..],
                [
                    "--no-pager",
                    "--no-optional-locks",
                    "ls-files",
                    "--others",
                    "--exclude-standard",
                    "-z"
                ]
            ),
            _ => {}
        }
    }
    assert!(lab.baseline().preexisting_tracked);
    assert_eq!(lab.log(), "", "baseline metadata executed {name}");
}

#[test]
fn live_filter_programs_do_not_run_for_any_content_argv() {
    vector(
        "filter",
        |lab| {
            let tool = lab.tool("filter", "cat");
            lab.git(&["config", "filter.evil.clean", tool.to_str().unwrap()]);
            lab.write(".gitattributes", "*.txt filter=evil\n");
        },
        &["add", "a.txt"],
    );
}

#[test]
fn live_monitor_programs_do_not_run_for_any_content_argv() {
    vector(
        "monitor",
        |lab| {
            let tool = lab.tool("monitor", ":");
            lab.git(&["config", "core.fsmonitor", tool.to_str().unwrap()]);
        },
        &["status", "--porcelain"],
    );
}

#[test]
fn live_textconv_programs_do_not_run_for_any_content_argv() {
    vector(
        "textconv",
        |lab| {
            let tool = lab.tool("textconv", "cat \"$1\"");
            lab.git(&["config", "diff.evil.textconv", tool.to_str().unwrap()]);
            lab.write(".gitattributes", "*.txt diff=evil\n");
        },
        &["diff", "--textconv"],
    );
}

#[test]
fn live_external_diff_programs_do_not_run_for_any_content_argv() {
    vector(
        "external",
        |lab| {
            let tool = lab.tool("external", ":");
            lab.git(&["config", "diff.external", tool.to_str().unwrap()]);
        },
        &["diff"],
    );
}

#[test]
fn live_hook_programs_do_not_run_for_any_content_argv() {
    vector(
        "hook",
        |lab| {
            let tool = lab.tool("hook", ":");
            std::fs::rename(tool, lab.repo().join(".git/hooks/post-index-change")).unwrap();
        },
        &["add", "a.txt"],
    );
}

#[test]
fn a_gitlink_never_enters_a_live_nested_repository() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    std::fs::create_dir(lab.repo().join("sub")).unwrap();
    lab.git(&["-C", "sub", "init", "--quiet", "-b", "main"]);
    lab.write("sub/file", "nested\n");
    lab.git(&["-C", "sub", "add", "file"]);
    lab.git(&["-C", "sub", "commit", "--quiet", "-m", "nested"]);
    let head = lab.git(&["-C", "sub", "rev-parse", "HEAD"]);
    lab.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{},sub", head.trim()),
    ]);
    lab.git(&["commit", "--quiet", "-m", "gitlink"]);
    let tool = lab.tool("nested-monitor", ":");
    lab.git(&[
        "-C",
        "sub",
        "config",
        "core.fsmonitor",
        tool.to_str().unwrap(),
    ]);
    lab.write("sub/file", "dirty nested\n");
    lab.plain(&["status", "--porcelain"]);
    assert!(lab.log().contains("nested-monitor"));
    lab.clear_log();
    let capture = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap();
    assert_eq!(lab.log(), "");
    assert_eq!(
        capture
            .candidates
            .iter()
            .map(|candidate| candidate.entry.path.as_str())
            .collect::<Vec<_>>(),
        ["sub"]
    );
    assert_eq!(
        capture.candidates[0].entry.availability,
        dudes_proto::DiffAvailability::Unavailable {
            reason: dudes_proto::DiffOmissionReason::UnsupportedKind
        }
    );
}

#[test]
fn object_redirection_controls_are_live_and_refused_without_other_repository_content() {
    let lab = Lab::new();
    let mut baseline = lab.baseline();
    let other = Lab::new();
    other.write("private-other", "outside fixture text\n");
    other.git(&["add", "private-other"]);
    other.git(&["commit", "--quiet", "-m", "other"]);
    let head = other.git(&["rev-parse", "HEAD"]);
    let blob = other.git(&["rev-parse", "HEAD:private-other"]);
    std::fs::write(
        lab.repo().join(".git/objects/info/alternates"),
        format!("{}\n", other.repo().join(".git/objects").display()),
    )
    .unwrap();
    assert_eq!(
        lab.git(&["cat-file", "blob", blob.trim()]),
        "outside fixture text\n"
    );
    assert!(
        lab.git(&["ls-tree", "-r", head.trim()])
            .contains("private-other")
    );
    baseline.head = head.trim().parse().unwrap();
    assert!(matches!(
        lab.reader.capture_review(&lab.tree(), &baseline, cancel()),
        Err(DiffRefusal::UnsupportedStorage | DiffRefusal::BaselineUnavailable)
    ));
}

fn candidates(capture: &duded::git_read::diff::ReviewCapture) -> Vec<&str> {
    capture
        .candidates
        .iter()
        .map(|candidate| candidate.entry.path.as_str())
        .collect()
}

#[test]
fn a_change_after_the_256th_tracked_path_is_still_a_candidate() {
    let lab = Lab::new();
    for n in 0..257 {
        lab.write(&format!("{n:04}.txt"), &format!("small {n}\n"));
    }
    lab.git(&["add", "-A"]);
    lab.git(&["commit", "--quiet", "-m", "many files"]);
    let baseline = lab.baseline();
    assert!(!baseline.preexisting_tracked);
    lab.write("0256.txt", "changed after the prefix\n");
    lab.real.commands.store(0, Ordering::Relaxed);
    let capture = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap();
    assert_eq!(candidates(&capture), ["0256.txt"]);
    assert_eq!(
        (
            capture.omitted,
            capture.omitted_lower_bound,
            capture.complete
        ),
        (0, false, true)
    );
    assert_eq!(lab.real.commands.load(Ordering::Relaxed), 4);
}

#[test]
fn more_changed_files_than_the_cap_are_counted_as_omitted() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    for n in 0..260 {
        lab.write(&format!("{n:04}.txt"), &format!("new {n}\n"));
    }
    lab.git(&["add", "-A"]);
    let capture = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap();
    assert_eq!(capture.candidates.len(), 256);
    assert_eq!(
        (
            capture.omitted,
            capture.omitted_lower_bound,
            capture.complete
        ),
        (4, false, false)
    );
}

#[test]
fn ordinary_packed_objects_preserve_the_same_complete_old_text() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    lab.write("a.txt", "new current\n");
    lab.git(&["gc", "--quiet"]);
    let content = lab.files(&baseline);
    assert_eq!(
        (
            content.files[0].old.as_str(),
            content.files[0].current.as_str()
        ),
        ("old a\n", "new current\n")
    );
}

#[test]
fn a_recorded_lease_reads_its_own_index_and_refuses_link_disagreement() {
    let lab = Lab::new();
    let id = dudes_types::WorktreeId::from_bytes([7; 16]);
    let path = lab.root.path().join(id.to_string());
    let base = lab.git(&["rev-parse", "HEAD"]).trim().parse().unwrap();
    lab.git(&[
        "worktree",
        "add",
        "--quiet",
        "-b",
        "lease",
        path.to_str().unwrap(),
        "HEAD",
    ]);
    let tree = Tree::Leased(duded::git_read::Lease {
        worktree: id,
        origin: lab.repo(),
        path: path.clone(),
        base,
    });
    let baseline = lab.reader.capture_baseline(&tree, cancel()).unwrap();
    std::fs::write(path.join("a.txt"), "lease edit\n").unwrap();
    let capture = lab
        .reader
        .capture_review(&tree, &baseline, cancel())
        .unwrap();
    assert_eq!(capture.candidates[0].entry.path, "a.txt");
    std::fs::write(
        path.join(".git"),
        format!("gitdir: {}\n", lab.repo().join(".git").display()),
    )
    .unwrap();
    assert!(matches!(
        lab.reader.capture_review(&tree, &baseline, cancel()),
        Err(DiffRefusal::WorktreeMismatch)
    ));
}

#[test]
fn content_time_edits_and_an_aba_digest_never_inherit_a_manifest_freshness_claim() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    lab.write("a.txt", "first sample\n");
    let first = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap();
    let digest = first.digest.clone();
    let mut binding = binding();
    binding.check_lsn = Some(dudes_types::Lsn(9));
    let manifest = first
        .into_manifest(
            binding,
            Timestamp::UNIX_EPOCH,
            DiffFreshness::MatchesReviewSample,
        )
        .unwrap();
    let DiffResponse::Manifest(page) =
        serde_json::from_slice(manifest.page(None).unwrap().bytes()).unwrap()
    else {
        panic!("manifest")
    };
    assert_eq!(page.freshness, DiffFreshness::MatchesReviewSample);
    let ids = page
        .entries
        .iter()
        .filter_map(|entry| entry.entry_id.clone())
        .collect::<Vec<_>>();
    lab.write("a.txt", "later edit\n");
    let DiffResponse::Content(content) = serde_json::from_slice(
        lab.reader
            .read_content(&lab.tree(), &baseline, &manifest, &ids, cancel())
            .unwrap()
            .bytes(),
    )
    .unwrap() else {
        panic!("content")
    };
    assert_eq!(content.files[0].current, "later edit\n");
    assert_eq!(content.freshness, DiffFreshness::Unknown);
    let later = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap();
    assert_ne!(later.digest, digest);
    lab.write("a.txt", "first sample\n");
    assert_eq!(
        lab.reader
            .capture_review(&lab.tree(), &baseline, cancel())
            .unwrap()
            .digest,
        digest
    );
}

#[test]
fn baseline_metadata_remains_available_with_clean_oversize_binary_and_sensitive_files() {
    for (name, bytes) in [
        ("Cargo.lock", vec![b'x'; 20 * 1024]),
        ("asset.bin", b"asset\0bytes".to_vec()),
        (".env.production", b"ordinary fixture\n".to_vec()),
    ] {
        let lab = Lab::new();
        std::fs::write(lab.repo().join(name), bytes).unwrap();
        lab.git(&["add", "-A"]);
        lab.git(&["commit", "--quiet", "-m", "excluded source"]);
        let baseline = lab.baseline();
        assert!(!baseline.preexisting_tracked, "clean metadata {name}");
        lab.write("a.txt", "ordinary small edit\n");
        let capture = lab
            .reader
            .capture_review(&lab.tree(), &baseline, cancel())
            .unwrap();
        assert!(
            capture
                .candidates
                .iter()
                .any(|candidate| candidate.entry.path == "a.txt")
        );
        assert!(lab.baseline().preexisting_tracked, "dirty metadata {name}");
    }
}

#[test]
fn selected_file_disappearance_or_kind_change_invalidates_the_manifest() {
    for special in [false, true] {
        let lab = Lab::new();
        let baseline = lab.baseline();
        lab.write("a.txt", "manifest sample\n");
        let manifest = lab
            .reader
            .capture_review(&lab.tree(), &baseline, cancel())
            .unwrap()
            .into_manifest(binding(), Timestamp::UNIX_EPOCH, DiffFreshness::Unknown)
            .unwrap();
        let DiffResponse::Manifest(page) =
            serde_json::from_slice(manifest.page(None).unwrap().bytes()).unwrap()
        else {
            panic!("manifest")
        };
        let ids = page
            .entries
            .iter()
            .filter_map(|entry| entry.entry_id.clone())
            .collect::<Vec<_>>();
        std::fs::remove_file(lab.repo().join("a.txt")).unwrap();
        if special {
            nix::unistd::mkfifo(&lab.repo().join("a.txt"), nix::sys::stat::Mode::S_IRUSR).unwrap();
        }
        assert!(matches!(
            lab.reader
                .read_content(&lab.tree(), &baseline, &manifest, &ids, cancel()),
            Err(DiffRefusal::StaleManifest)
        ));
    }
}

#[test]
fn hidden_index_flags_cannot_establish_a_false_clean_baseline() {
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        let lab = Lab::new();
        lab.git(&["update-index", flag, "a.txt"]);
        lab.write("a.txt", "hidden edit\n");
        assert_eq!(
            lab.git(&["status", "--porcelain=v1", "-z", "--ignore-submodules=all"]),
            "",
            "ordinary metadata control hides {flag} edit"
        );
        assert!(matches!(
            lab.reader.capture_baseline(&lab.tree(), cancel()),
            Err(DiffRefusal::BaselineUnavailable)
        ));
        lab.write("b.txt", "observed other edit\n");
        assert!(lab.baseline().preexisting_tracked);
    }
}

#[test]
fn content_reclamation_does_not_adopt_metadata_scratch_entries() {
    let lab = Lab::new();
    let _ = lab.baseline();
    let metadata = lab.root.path().join("scratch/metadata");
    std::fs::create_dir_all(&metadata).unwrap();
    std::fs::write(metadata.join("unmarked"), "metadata only").unwrap();
    assert_eq!(lab.reader.reclaim_scratch(cancel()).unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(metadata.join("unmarked")).unwrap(),
        "metadata only"
    );
}

#[test]
fn a_cut_diff_listing_never_turns_unknown_membership_into_an_added_file() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    lab.write("y.txt", "first staged\n");
    lab.write("z.txt", "second staged\n");
    lab.git(&["add", "y.txt", "z.txt"]);
    lab.real.cut_listing.store(true, Ordering::Relaxed);
    let capture = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap();
    assert_eq!(candidates(&capture), ["y.txt"]);
    assert_eq!(
        (
            capture.untracked,
            capture.omitted_lower_bound,
            capture.complete
        ),
        (0, true, false)
    );
}

#[test]
fn staging_unchanged_current_text_does_not_claim_a_scope_content_change() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    lab.write("a.txt", "visible edit\n");
    let before = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap();
    lab.git(&["add", "a.txt"]);
    let after = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap();
    assert!(before.complete && after.complete);
    assert_eq!(before.digest, after.digest);
}

impl Lab {
    /// The first page of a fresh manifest, and the content of every available entry on it.
    fn review(
        &self,
        baseline: &BaselineCapture,
    ) -> (dudes_proto::DiffPage, dudes_proto::DiffContent) {
        let capture = self
            .reader
            .capture_review(&self.tree(), baseline, cancel())
            .unwrap();
        let manifest = capture
            .into_manifest(binding(), Timestamp::UNIX_EPOCH, DiffFreshness::Unknown)
            .unwrap();
        let DiffResponse::Manifest(page) =
            serde_json::from_slice(manifest.page(None).unwrap().bytes()).unwrap()
        else {
            panic!("page")
        };
        let ids = page
            .entries
            .iter()
            .filter_map(|entry| entry.entry_id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            let none = dudes_proto::DiffContent {
                manifest_id: page.manifest_id.clone(),
                binding: page.binding.clone(),
                expires_at: page.expires_at,
                freshness: DiffFreshness::Unknown,
                files: Vec::new(),
                omitted: Vec::new(),
                truncated: false,
            };
            return (page, none);
        }
        let DiffResponse::Content(content) = serde_json::from_slice(
            self.reader
                .read_content(&self.tree(), baseline, &manifest, &ids, cancel())
                .unwrap()
                .bytes(),
        )
        .unwrap() else {
            panic!("content")
        };
        (page, content)
    }
}

fn rows(page: &dudes_proto::DiffPage) -> Vec<(&str, DiffKind, bool)> {
    page.entries
        .iter()
        .map(|entry| {
            (
                entry.path.as_str(),
                entry.kind,
                entry.availability == DiffAvailability::Available,
            )
        })
        .collect()
}

#[test]
fn a_new_file_is_listed_as_added_and_opens_with_its_text() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    std::fs::create_dir(lab.repo().join("src")).unwrap();
    lab.write("src/fresh.rs", "pub fn fresh() {}\n");
    let (page, content) = lab.review(&baseline);
    assert_eq!(rows(&page), [("src/fresh.rs", DiffKind::Added, true)]);
    assert_eq!((page.untracked, page.untracked_lower_bound), (0, false));
    assert_eq!(
        content
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.old.as_str(), file.current.as_str()))
            .collect::<Vec<_>>(),
        [("src/fresh.rs", "", "pub fn fresh() {}\n")]
    );
}

#[test]
fn ignored_files_are_neither_listed_nor_counted_and_the_private_exclude_list_is_honoured() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    lab.write(".gitignore", "build/\n*.log\n");
    std::fs::create_dir(lab.repo().join("build")).unwrap();
    lab.write("build/out.rs", "ignored\n");
    lab.write("run.log", "ignored\n");
    std::fs::write(lab.repo().join(".git/info/exclude"), "private-notes.md\n").unwrap();
    lab.write("private-notes.md", "operator excluded\n");
    lab.write("kept.rs", "listed\n");
    let (page, _) = lab.review(&baseline);
    assert_eq!(
        rows(&page),
        [
            (".gitignore", DiffKind::Added, true),
            ("kept.rs", DiffKind::Added, true)
        ]
    );
    assert_eq!(page.untracked, 0);
}

#[test]
fn a_staged_deletion_still_on_disk_is_one_changed_file_never_also_a_new_one() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    lab.git(&["rm", "--quiet", "--cached", "b.txt"]);
    lab.write("b.txt", "rewritten b\n");
    let (page, content) = lab.review(&baseline);
    assert_eq!(rows(&page), [("b.txt", DiffKind::Modified, true)]);
    assert_eq!(
        content
            .files
            .iter()
            .map(|file| (file.old.as_str(), file.current.as_str()))
            .collect::<Vec<_>>(),
        [("old b\n", "rewritten b\n")]
    );
}

#[test]
fn a_nested_repository_is_listed_once_and_never_opened() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    lab.git(&["init", "--quiet", "nested"]);
    lab.write("nested/inner.rs", "never read\n");
    let capture = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap();
    assert!(
        !capture.complete,
        "an unopened folder keeps the label Unknown"
    );
    let (page, content) = lab.review(&baseline);
    assert_eq!(rows(&page), [("nested", DiffKind::NestedRepository, false)]);
    assert_eq!(
        page.entries[0].availability,
        DiffAvailability::Unavailable {
            reason: DiffOmissionReason::UnsupportedKind
        }
    );
    assert!(content.files.is_empty());
}

#[test]
fn a_hard_linked_new_file_is_listed_with_its_text_withheld_for_its_own_reason() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    let outside = lab.root.path().join("outside.txt");
    std::fs::write(&outside, "belongs elsewhere\n").unwrap();
    std::fs::hard_link(&outside, lab.repo().join("linked.txt")).unwrap();
    let (page, content) = lab.review(&baseline);
    assert_eq!(rows(&page), [("linked.txt", DiffKind::Added, false)]);
    assert_eq!(
        page.entries[0].availability,
        DiffAvailability::Unavailable {
            reason: DiffOmissionReason::HardLinked
        }
    );
    assert!(content.files.is_empty());
}

#[test]
fn a_new_file_with_a_credential_name_is_listed_as_added_and_withheld() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    lab.write("prod.tfvars", "region = \"eu\"\n");
    let (page, content) = lab.review(&baseline);
    assert_eq!(rows(&page), [("prod.tfvars", DiffKind::Added, false)]);
    assert_eq!(
        page.entries[0].availability,
        DiffAvailability::Unavailable {
            reason: DiffOmissionReason::SensitivePath
        }
    );
    assert!(content.files.is_empty());
}

#[test]
fn a_source_file_that_declares_a_password_field_is_still_shown() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    lab.write("login.rs", "struct Login { password: String }\n");
    let (_, content) = lab.review(&baseline);
    assert_eq!(
        content
            .files
            .iter()
            .map(|file| file.current.as_str())
            .collect::<Vec<_>>(),
        ["struct Login { password: String }\n"]
    );
}

#[test]
fn a_symbolic_link_and_a_binary_new_file_stay_added() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    std::os::unix::fs::symlink("a.txt", lab.repo().join("alias")).unwrap();
    std::fs::write(lab.repo().join("blob.bin"), b"\0\x01binary").unwrap();
    let (page, _) = lab.review(&baseline);
    assert_eq!(
        rows(&page),
        [
            ("alias", DiffKind::Added, false),
            ("blob.bin", DiffKind::Added, false)
        ]
    );
}

#[test]
fn a_new_file_step_that_times_out_leaves_the_tracked_changes_and_a_lower_bound() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    lab.write("a.txt", "tracked edit\n");
    lab.write("fresh.rs", "unlisted\n");
    lab.real.new_files_time_out.store(true, Ordering::Relaxed);
    let capture = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap();
    assert_eq!(candidates(&capture), ["a.txt"]);
    assert_eq!(
        (
            capture.untracked,
            capture.untracked_lower_bound,
            capture.complete
        ),
        (0, true, false)
    );
}

#[test]
fn tracked_changes_keep_their_slots_when_new_files_exceed_the_cap() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    for n in 0..250 {
        lab.write(&format!("t{n:04}.txt"), "staged\n");
    }
    lab.git(&["add", "-A"]);
    for n in 0..10 {
        lab.write(&format!("a-new{n}.txt"), "untracked\n");
    }
    let capture = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap();
    assert_eq!(capture.candidates.len(), 256);
    assert_eq!(
        capture
            .candidates
            .iter()
            .filter(|candidate| candidate.entry.path.starts_with("t0"))
            .count(),
        250
    );
    assert_eq!(
        (capture.omitted, capture.untracked, capture.complete),
        (0, 4, false)
    );
}

fn open_descriptors() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count()
}

#[test]
fn a_full_read_and_its_cleanup_leave_no_descriptor_open() {
    let lab = Lab::new();
    lab.write("a.txt", "changed\n");
    let start = open_descriptors();
    let baseline = lab.baseline();
    let after_baseline = open_descriptors();
    let content = lab.files(&baseline);
    let after_read = open_descriptors();
    assert!(!content.files.is_empty());
    assert_eq!((after_baseline, after_read), (start, start));
}

#[test]
fn long_unchanged_names_neither_hide_nor_invent_a_changed_file() {
    let lab = Lab::new();
    for n in 0..431 {
        lab.write(&format!("{n:03}{}", "x".repeat(247)), "long name\n");
    }
    lab.git(&["add", "-A"]);
    lab.git(&["commit", "--quiet", "-m", "long names"]);
    let baseline = lab.baseline();
    lab.write("z.txt", "staged after the baseline\n");
    lab.git(&["add", "z.txt"]);
    let capture = lab
        .reader
        .capture_review(&lab.tree(), &baseline, cancel())
        .unwrap();
    assert_eq!(candidates(&capture), ["z.txt"]);
    assert_eq!(
        (
            capture.omitted,
            capture.omitted_lower_bound,
            capture.complete
        ),
        (0, false, true)
    );
}

#[test]
fn a_maintenance_lock_in_the_object_directory_is_a_busy_refusal_until_it_clears() {
    let lab = Lab::new();
    let lock = lab.repo().join(".git/objects/maintenance.lock");
    std::fs::write(&lock, "").unwrap();
    assert_eq!(
        lab.reader
            .capture_baseline(&lab.tree(), cancel())
            .unwrap_err(),
        DiffRefusal::Busy
    );
    std::fs::remove_file(&lock).unwrap();
    assert!(lab.reader.capture_baseline(&lab.tree(), cancel()).is_ok());
}

#[test]
fn a_read_whose_budget_ran_out_before_git_answered_is_a_timeout_not_a_missing_git() {
    let lab = Lab::new();
    let reader = DiffReader::new(
        lab.real.clone(),
        std::env::var_os("PATH"),
        &lab.root.path().join("short-scratch"),
    )
    .with_deadline(Duration::from_millis(150));
    lab.real.exhaust.store(true, Ordering::Relaxed);
    assert_eq!(
        reader.capture_baseline(&lab.tree(), cancel()).unwrap_err(),
        DiffRefusal::TimedOut
    );
}

#[test]
fn git_that_cannot_run_with_budget_to_spare_is_still_a_missing_git() {
    let lab = Lab::new();
    let reader = DiffReader::new(
        lab.real.clone(),
        std::env::var_os("PATH"),
        &lab.root.path().join("long-scratch"),
    );
    lab.real.exhaust.store(true, Ordering::Relaxed);
    let started = std::time::Instant::now();
    assert_eq!(
        reader.capture_baseline(&lab.tree(), cancel()).unwrap_err(),
        DiffRefusal::GitUnavailable
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_known_secret_in_a_changed_file_is_replaced_before_the_text_is_sent() {
    let redactor = dudes_supervisor::Redactor::new();
    redactor.set_source("test", ["plainvaluesecret".to_owned()]);
    let lab = Lab::new().redacting(redactor);
    let baseline = lab.baseline();
    lab.write("a.txt", "token = plainvaluesecret\nkeep this\n");
    let content = lab.files(&baseline);
    let file = content
        .files
        .iter()
        .find(|file| file.path.as_str() == "a.txt")
        .expect("the changed file");
    assert_eq!(file.current, "token = (redacted)\nkeep this\n");
    assert_eq!(file.old, "old a\n");
}
