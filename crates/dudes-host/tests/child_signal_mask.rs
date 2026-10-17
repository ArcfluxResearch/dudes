//! A child must not inherit the daemon's blocked-signal mask.
//!
//! The daemon blocks `SIGTERM` and `SIGINT` on every thread so that a stop signal cannot kill it
//! on an arbitrary thread. A spawned process inherits the mask of the thread that spawned it, so
//! without a reset every agent and every predicate child starts deaf to `SIGTERM`: the polite stop
//! never lands and each stop costs the whole grace before `SIGKILL`.
//!
//! Each test blocks `SIGTERM` in the spawning thread, as the daemon does, launches a shell that
//! traps `SIGTERM` and writes a marker, then stops it through the real stop path. The marker is the
//! assertion; the elapsed bound only shows the grace was not spent.
#![cfg(unix)]

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use dudes_host::{OneShot, OneShotOutcome, RealHost};
use dudes_providers::launch::{LaunchInput, LaunchSpec};
use dudes_supervisor::host::{KillOutcome, ProcessHost};
use nix::sys::signal::{SigSet, Signal};

mod common;

/// Long enough that a run which waits it out is plainly the grace and not scheduling noise.
const GRACE: Duration = Duration::from_secs(10);

/// Well under [`GRACE`], well over a polite stop on a loaded machine.
const PROMPT: Duration = Duration::from_secs(2);

/// The one-shot run's timeout. It starts at spawn, so it must outlast the shell starting and
/// installing its trap, or a slow machine looks like the bug.
const ONE_SHOT_TIMEOUT: Duration = Duration::from_millis(1500);

fn block_sigterm() {
    let mut set = SigSet::empty();
    set.add(Signal::SIGTERM);
    set.thread_block().expect("block SIGTERM on this thread");
}

/// A shell that reports readiness, then idles until `SIGTERM`, which it records and exits on.
fn trapping_spec(dir: &Path) -> LaunchSpec {
    let script = format!(
        "trap 'echo term > \"{term}\"; exit 0' TERM; echo ready > \"{ready}\"; \
         while :; do /bin/sleep 0.05; done",
        term = dir.join("term").display(),
        ready = dir.join("ready").display(),
    );
    LaunchSpec {
        argv: vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(script),
        ],
        env: Vec::new(),
        cwd: dir.to_path_buf(),
        stdin: LaunchInput::Null,
    }
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_managed_child_receives_the_stop_signal_the_daemon_thread_blocks() {
    let dir = tempfile::tempdir().expect("tempdir");
    block_sigterm();
    let mut host = RealHost::with_grace(GRACE);
    let key = common::spawned(
        host.spawn(&trapping_spec(dir.path()), &common::no_install()),
        "spawn",
    );
    wait_for(&dir.path().join("ready"));

    let started = Instant::now();
    assert!(matches!(host.kill(&key), KillOutcome::Signalled));
    let elapsed = started.elapsed();

    assert!(
        dir.path().join("term").exists(),
        "the child never saw SIGTERM; it was killed after {elapsed:?}"
    );
    assert!(
        elapsed < PROMPT,
        "the stop took {elapsed:?}, so it waited out the grace"
    );
    host.shutdown().ok();
}

#[test]
fn a_one_shot_child_receives_the_stop_signal_the_daemon_thread_blocks() {
    let dir = tempfile::tempdir().expect("tempdir");
    block_sigterm();
    let runner = OneShot::with_grace(GRACE);

    let started = Instant::now();
    let outcome = runner
        .run(&trapping_spec(dir.path()), ONE_SHOT_TIMEOUT)
        .expect("run");
    let elapsed = started.elapsed();

    assert!(
        matches!(
            outcome,
            OneShotOutcome::Finished {
                timed_out: true,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert!(
        dir.path().join("ready").exists(),
        "the shell had not started within the {ONE_SHOT_TIMEOUT:?} timeout; the machine is too slow \
         for this test, which says nothing about the signal mask"
    );
    assert!(
        dir.path().join("term").exists(),
        "the child never saw SIGTERM; the run took {elapsed:?}"
    );
    assert!(
        elapsed < ONE_SHOT_TIMEOUT + PROMPT,
        "the run took {elapsed:?}, so it waited out the grace"
    );
}
