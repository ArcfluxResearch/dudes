//! The baseline and stamp capture service against real temporary Git repositories.

#![cfg(unix)]

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use duded::diff_capture::DiffCapture;
use duded::git_read::Tree;
use duded::git_read::diff::DiffReader;
use duded::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner};
use dudes_host::{CapturedOutcome, OneShot, OneShotOutcome};
use dudes_proto::DiffRefusal;
use dudes_proto::diff_stamp::{
    BaselineOutcome, BaselinePayload, StampOutcome, StampPayload, TicketBinding,
};
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_types::{AgentId, CardId, GoalId, Lsn, Timestamp};

struct Real(OneShot);
impl PredicateRunner for Real {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<PredicateOutcome> {
        self.0.run(spec, timeout).map(|result| match result {
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
        self.0
            .run_captured(spec, timeout, cap)
            .map(|result| match result {
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

struct Lab {
    root: tempfile::TempDir,
    runner: Arc<Real>,
    capture: DiffCapture,
    git: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let runner = Arc::new(Real(OneShot::new()));
        let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join("git"))
            .find(|path| path.is_file())
            .unwrap();
        let reader = Arc::new(DiffReader::new(
            runner.clone(),
            std::env::var_os("PATH"),
            &root.path().join("scratch"),
        ));
        let ticks = AtomicI64::new(1_000);
        let capture = DiffCapture::new(
            reader,
            Box::new(move || Timestamp::from_unix_millis(ticks.fetch_add(1, Ordering::Relaxed))),
        );
        let lab = Self {
            root,
            runner,
            capture,
            git,
        };
        std::fs::create_dir(lab.repo()).unwrap();
        std::fs::create_dir(lab.root.path().join("home")).unwrap();
        lab.git(&["init", "--quiet", "-b", "main"]);
        lab.write("a.txt", "old a\n");
        lab.git(&["add", "-A"]);
        lab.git(&["commit", "--quiet", "-m", "initial"]);
        lab
    }
    fn repo(&self) -> PathBuf {
        self.root.path().join("repo")
    }
    fn tree(&self) -> Tree {
        Tree::Checkout(self.repo())
    }
    fn write(&self, path: &str, text: &str) {
        std::fs::write(self.repo().join(path), text).unwrap();
    }
    fn git(&self, args: &[&str]) -> String {
        let mut command = vec![self.git.as_os_str().to_owned()];
        command.extend(args.iter().map(OsString::from));
        let spec = LaunchSpec {
            argv: command,
            env: vec![
                EnvVar::new("PATH", std::env::var_os("PATH").unwrap_or_default()),
                EnvVar::new("HOME", self.root.path().join("home")),
                EnvVar::new("GIT_CONFIG_NOSYSTEM", "1"),
                EnvVar::new("GIT_CONFIG_GLOBAL", "/dev/null"),
                EnvVar::new("GIT_CONFIG_COUNT", "1"),
                EnvVar::new("GIT_CONFIG_KEY_0", "maintenance.auto"),
                EnvVar::new("GIT_CONFIG_VALUE_0", "false"),
                EnvVar::new("GIT_AUTHOR_NAME", "Fixture"),
                EnvVar::new("GIT_AUTHOR_EMAIL", "fixture@example.invalid"),
                EnvVar::new("GIT_COMMITTER_NAME", "Fixture"),
                EnvVar::new("GIT_COMMITTER_EMAIL", "fixture@example.invalid"),
            ],
            cwd: self.repo(),
            stdin: LaunchInput::Null,
        };
        match self.runner.run(&spec, Duration::from_secs(120)).unwrap() {
            PredicateOutcome::Finished {
                exit: Some(0),
                output_tail,
                ..
            } => output_tail,
            PredicateOutcome::Finished {
                exit, output_tail, ..
            } => panic!("git {args:?} exited {exit:?}: {output_tail}"),
            PredicateOutcome::Swept => panic!("git {args:?} swept"),
        }
    }
    fn baseline(&self) -> BaselinePayload {
        self.capture
            .baseline(&self.tree(), binding(), cancel())
            .unwrap()
    }
    fn stamp(&self, baseline: &BaselinePayload) -> StampPayload {
        self.capture
            .stamp(
                &self.tree(),
                baseline,
                Lsn(12),
                binding(),
                Lsn(41),
                cancel(),
            )
            .unwrap()
    }
}

fn cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn binding() -> TicketBinding {
    TicketBinding {
        card: CardId::NIL,
        round: 2,
        goal: GoalId::NIL,
        claimant: AgentId::NIL,
    }
}

fn sampled(stamp: &StampPayload) -> (bool, String, u32, u32) {
    match stamp.outcome {
        StampOutcome::Sampled {
            complete,
            digest,
            omitted,
            untracked,
            ..
        } => (complete, digest.as_str().to_owned(), omitted, untracked),
        other => panic!("not sampled: {other:?}"),
    }
}

#[test]
fn a_clean_start_records_the_head_the_root_and_the_interval() {
    let lab = Lab::new();
    let head = lab.git(&["rev-parse", "HEAD"]).trim().to_owned();
    let baseline = lab.baseline();
    assert_eq!(baseline.binding, binding());
    assert_eq!(baseline.interval.started_at.unix_millis(), 1_000);
    assert_eq!(baseline.interval.ended_at.unix_millis(), 1_001);
    assert_eq!(baseline.versions.sampler, 1);
    let BaselineOutcome::Captured {
        head: recorded,
        preexisting_tracked,
        ..
    } = baseline.outcome
    else {
        panic!("{:?}", baseline.outcome)
    };
    assert_eq!(recorded.to_string(), head);
    assert!(!preexisting_tracked);
    assert_eq!(
        BaselinePayload::decode(baseline.encode().unwrap().as_slice()).unwrap(),
        baseline
    );
}

#[test]
fn tracked_edits_before_the_round_set_the_dirty_flag() {
    let lab = Lab::new();
    lab.write("a.txt", "dirty before\n");
    let BaselineOutcome::Captured {
        preexisting_tracked,
        ..
    } = lab.baseline().outcome
    else {
        panic!("not captured")
    };
    assert!(preexisting_tracked);
}

#[test]
fn a_directory_that_is_not_a_repository_is_an_unavailable_baseline() {
    let lab = Lab::new();
    let empty = lab.root.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let baseline = lab
        .capture
        .baseline(&Tree::Checkout(empty), binding(), cancel())
        .unwrap();
    assert_eq!(
        baseline.outcome,
        BaselineOutcome::Unavailable {
            reason: DiffRefusal::UnsupportedStorage
        }
    );
}

#[test]
fn a_cancelled_capture_is_an_unavailable_baseline_and_stamp() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    let stop = Arc::new(AtomicBool::new(true));
    let cut = lab
        .capture
        .baseline(&lab.tree(), binding(), stop.clone())
        .unwrap();
    assert_eq!(
        cut.outcome,
        BaselineOutcome::Unavailable {
            reason: DiffRefusal::TimedOut
        }
    );
    let stamp = lab
        .capture
        .stamp(&lab.tree(), &baseline, Lsn(12), binding(), Lsn(41), stop)
        .unwrap();
    assert_eq!(
        stamp.outcome,
        StampOutcome::Unavailable {
            reason: DiffRefusal::TimedOut
        }
    );
}

#[test]
fn a_stamp_samples_the_scope_and_names_its_baseline_and_check() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    lab.write("a.txt", "agent edit\n");
    lab.write("scratch-notes", "untracked\n");
    let first = lab.stamp(&baseline);
    assert_eq!(first.check_lsn, Lsn(41));
    assert_eq!(first.baseline_lsn, Lsn(12));
    let (complete, digest, omitted, untracked) = sampled(&first);
    assert_eq!((complete, omitted, untracked), (true, 0, 1));
    assert_eq!(digest.len(), 64);
    assert_eq!(sampled(&lab.stamp(&baseline)).1, digest);
    lab.write("a.txt", "edited again\n");
    assert_ne!(sampled(&lab.stamp(&baseline)).1, digest);
}

#[test]
fn an_unavailable_baseline_gives_an_unavailable_stamp_without_a_sample() {
    let lab = Lab::new();
    let stop = Arc::new(AtomicBool::new(true));
    let cut = lab.capture.baseline(&lab.tree(), binding(), stop).unwrap();
    assert_eq!(
        lab.stamp(&cut).outcome,
        StampOutcome::Unavailable {
            reason: DiffRefusal::BaselineUnavailable
        }
    );
}

#[test]
fn a_stamp_for_another_round_is_stale() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    let mut later = binding();
    later.round = 3;
    let stamp = lab
        .capture
        .stamp(&lab.tree(), &baseline, Lsn(12), later, Lsn(41), cancel())
        .unwrap();
    assert_eq!(stamp.binding.round, 3);
    assert_eq!(
        stamp.outcome,
        StampOutcome::Unavailable {
            reason: DiffRefusal::StaleTicket
        }
    );
}

#[test]
fn a_replaced_root_is_an_unavailable_stamp_not_a_sample_of_the_new_repository() {
    let lab = Lab::new();
    let baseline = lab.baseline();
    std::fs::rename(lab.repo(), lab.root.path().join("moved")).unwrap();
    std::fs::create_dir(lab.repo()).unwrap();
    lab.git(&["init", "--quiet", "-b", "main"]);
    lab.write("a.txt", "old a\n");
    lab.git(&["add", "-A"]);
    lab.git(&["commit", "--quiet", "-m", "other"]);
    assert_eq!(
        lab.stamp(&baseline).outcome,
        StampOutcome::Unavailable {
            reason: DiffRefusal::RootMismatch
        }
    );
}

#[test]
fn a_missing_start_commit_is_an_unavailable_stamp_never_a_new_base() {
    let lab = Lab::new();
    let mut baseline = lab.baseline();
    let BaselineOutcome::Captured {
        device,
        inode,
        preexisting_tracked,
        ..
    } = baseline.outcome
    else {
        panic!("not captured")
    };
    baseline.outcome = BaselineOutcome::Captured {
        device,
        inode,
        head: "1111111111111111111111111111111111111111".parse().unwrap(),
        preexisting_tracked,
    };
    assert_eq!(
        lab.stamp(&baseline).outcome,
        StampOutcome::Unavailable {
            reason: DiffRefusal::BaselineUnavailable
        }
    );
}
