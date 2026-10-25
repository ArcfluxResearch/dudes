//! The model lists, driven through the real one-shot runner against stand-in engine programs.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::engine_models::{EngineModels, ItemKind, MAX_ITEMS, Refusal, Source};
use duded::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner};
use dudes_host::{CapturedOutcome, OneShot, OneShotOutcome};
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_types::Provider;

struct Real(OneShot);

impl PredicateRunner for Real {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> std::io::Result<PredicateOutcome> {
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
    ) -> std::io::Result<PredicateCaptured> {
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

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("a script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("executable");
    path
}

fn lists(
    dir: &Path,
    provider: Provider,
    program: PathBuf,
    deadline: Duration,
    cap: usize,
) -> (EngineModels, Arc<Real>) {
    let runner = Arc::new(Real(OneShot::new()));
    let models = EngineModels::with_limits(
        runner.clone(),
        dir.to_path_buf(),
        [Source {
            provider,
            program: program.into_os_string(),
            env: vec![
                EnvVar::new("PATH", "/usr/bin:/bin"),
                EnvVar::new("PI_CODING_AGENT_DIR", "profile-a"),
            ],
        }],
        deadline,
        cap,
    );
    (models, runner)
}

fn alive(pid_file: &Path) -> bool {
    let pid: i32 = std::fs::read_to_string(pid_file)
        .expect("the stand-in wrote its pid")
        .trim()
        .parse()
        .expect("a pid");
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

fn gone_within(pid_file: &Path, bound: Duration) -> bool {
    let until = Instant::now() + bound;
    while Instant::now() < until {
        if !alive(pid_file) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn pi_is_listed_under_the_profile_it_runs_with() {
    let dir = tempfile::tempdir().unwrap();
    let pi = script(
        dir.path(),
        "pi",
        r#"[ "$1" = "--list-models" ] || exit 2
echo "provider  model  context  max-out  thinking  images"
echo "openai  gpt-5.1-$PI_CODING_AGENT_DIR  400K  128K  yes  yes"
echo "openai-codex  gpt-5.1-$PI_CODING_AGENT_DIR  400K  128K  yes  yes""#,
    );
    let (models, _) = lists(dir.path(), Provider::Pi, pi, Duration::from_secs(4), 65_536);
    let list = models.refresh(Provider::Pi).expect("a list");
    let ids: Vec<_> = list
        .items
        .iter()
        .map(|item| (item.id.as_str(), item.kind))
        .collect();
    assert_eq!(
        ids,
        [
            ("openai/gpt-5.1-profile-a", ItemKind::Listed),
            ("openai-codex/gpt-5.1-profile-a", ItemKind::Listed)
        ]
    );
    assert!(!list.checked);
}

#[test]
fn a_child_that_prints_without_end_is_killed_at_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("pid");
    let pi = script(
        dir.path(),
        "pi",
        &format!(
            "echo \"provider model context\"\necho $$ > {}\nexec yes 'openai gpt-x 1K 1K yes yes'",
            pid.display()
        ),
    );
    let (models, _) = lists(dir.path(), Provider::Pi, pi, Duration::from_secs(4), 65_536);
    let started = Instant::now();
    let list = models
        .refresh(Provider::Pi)
        .expect("the head of the output is a list");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "killed at the cap, not at the deadline"
    );
    assert_eq!(list.items.len(), 1);
    assert!(
        gone_within(&pid, Duration::from_secs(3)),
        "the listing child is gone"
    );
}

#[test]
fn a_child_that_never_exits_is_killed_at_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("pid");
    let pi = script(
        dir.path(),
        "pi",
        &format!("echo $$ > {}\nexec sleep 60", pid.display()),
    );
    let (models, _) = lists(
        dir.path(),
        Provider::Pi,
        pi,
        Duration::from_millis(1500),
        65_536,
    );
    let started = Instant::now();
    assert_eq!(models.refresh(Provider::Pi), Err(Refusal::TimedOut));
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(
        gone_within(&pid, Duration::from_secs(3)),
        "the listing child is gone"
    );
    assert!(
        models.list(Provider::Pi).is_none(),
        "a failed listing keeps no list"
    );
}

#[test]
fn stopping_the_daemon_mid_listing_leaves_no_child() {
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("pid");
    let pi = script(
        dir.path(),
        "pi",
        &format!("echo $$ > {}\nexec sleep 60", pid.display()),
    );
    let (models, runner) = lists(dir.path(), Provider::Pi, pi, Duration::from_secs(4), 65_536);
    let waiting = {
        let models = models.clone();
        std::thread::spawn(move || models.refresh(Provider::Pi))
    };
    let until = Instant::now() + Duration::from_secs(3);
    while !pid.exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    runner.sweep();
    assert_eq!(waiting.join().unwrap(), Err(Refusal::Stopping));
    assert!(
        gone_within(&pid, Duration::from_secs(3)),
        "the listing child is gone"
    );
}

#[test]
fn a_repeated_refresh_runs_one_child() {
    let dir = tempfile::tempdir().unwrap();
    let count = dir.path().join("count");
    let pi = script(
        dir.path(),
        "pi",
        &format!(
            "echo x >> {}\nsleep 1\necho 'provider model context'\necho 'p m1 1K'",
            count.display()
        ),
    );
    let (models, _) = lists(dir.path(), Provider::Pi, pi, Duration::from_secs(4), 65_536);
    let threads: Vec<_> = (0..3)
        .map(|_| {
            let models = models.clone();
            std::thread::spawn(move || models.refresh(Provider::Pi))
        })
        .collect();
    let answers: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert!(
        answers
            .iter()
            .all(|answer| answer == &answers[0] && answer.is_ok()),
        "{answers:?}"
    );
    assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 1);
}

#[test]
fn codex_answers_model_list_after_one_write_then_close() {
    let dir = tempfile::tempdir().unwrap();
    let capture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../crates/dudes-providers/tests/fixtures/codex/app-server-model-list.jsonl"
    );
    let codex = script(
        dir.path(),
        "codex",
        &format!(
            "[ \"$1\" = app-server ] || exit 2\nread init; read initialized; read list\ncase \"$list\" in *'\"method\":\"model/list\"'*) cat {capture};; *) exit 3;; esac"
        ),
    );
    let (models, _) = lists(
        dir.path(),
        Provider::Codex,
        codex,
        Duration::from_secs(4),
        65_536,
    );
    let list = models.refresh(Provider::Codex).expect("a list");
    assert_eq!(list.items.len(), 7);
    assert_eq!(list.items[1].id, "gpt-6-astra");
}

#[test]
fn a_list_never_exceeds_two_hundred_items() {
    let dir = tempfile::tempdir().unwrap();
    let pi = script(
        dir.path(),
        "pi",
        "echo 'provider model context'\ni=0\nwhile [ $i -lt 400 ]; do echo \"p m$i 1K\"; i=$((i+1)); done",
    );
    let (models, _) = lists(dir.path(), Provider::Pi, pi, Duration::from_secs(4), 65_536);
    assert_eq!(models.refresh(Provider::Pi).unwrap().items.len(), MAX_ITEMS);
}
