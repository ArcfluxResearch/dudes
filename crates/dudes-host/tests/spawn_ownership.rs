//! Invariant 10, inside `spawn`: **a spawn that fails after the process exists still owns it.**
//!
//! `spawn` does four things in order — create the process, take the platform hold, deliver
//! standard input, read the adoption key — and only the first of them is allowed to be the last
//! `?`. Dropping a `std::process::Child` neither signals nor reaps, so an early return past the
//! exec leaves a running process in a process group or job of its own that the host holds no
//! record of: `shutdown` cannot reach it, `kill` cannot name it, and nothing ever reaps it. The
//! error still reaches the caller — the policy for a half-succeeded spawn is separate — but it
//! reaches the caller with the child owned.
//!
//! # How the failure is induced
//!
//! By the one lever a test has: `LaunchSpec.stdin`. A helper that closes or abandons the read end
//! makes the host's write fail with `EPIPE`, and the payload is larger than any pipe buffer so
//! that the outcome does not depend on which of the two happens first — either the read end is
//! already gone and the first write fails, or the buffer fills and the write blocks until it goes.
//!
//! # Two arms, and the second is the one that witnesses the invariant
//!
//! The portable arm uses a helper that exits, and asserts the count: a failed spawn that created a
//! process owns one child. The Unix arm keeps the child **alive** — it redirects its own standard
//! input to the null device by an exec in place — so the assertion can be the thing invariant 10
//! is actually about: the host can still reach a live child it never returned a key for, and
//! `shutdown` stops it.

mod common;

use dudes_providers::launch::LaunchInput;

use std::time::Duration;

use dudes_host::RealHost;
use dudes_supervisor::host::{ProcessHost, SpawnResult};

/// Not a test. See [`common::run_if_helper`].
#[test]
fn the_helper_entry_point() {
    common::run_if_helper();
}

/// A spawn whose standard input could not be delivered reports the failure **and** owns the child.
///
/// The control is the count. A `spawn` that failed before creating anything owns nothing, so
/// `owned() == 1` is not something an unrelated failure could produce — and `shutdown` returning
/// `Ok` with the count back to zero is what says the child was reaped rather than merely
/// forgotten.
#[test]
fn a_spawn_that_fails_after_the_process_exists_still_owns_the_child() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut host = RealHost::with_grace(Duration::from_millis(300));
    let mark = dir.path().join("brief.pid");

    let mut spec = common::helper_spec(common::BRIEF, &mark, dir.path());
    spec.stdin = LaunchInput::WriteThenClose(vec![b'x'; common::MORE_THAN_A_PIPE_HOLDS]);

    let error = match host.spawn(&spec, &common::no_install()) {
        SpawnResult::CleanupUnconfirmed { reason } => reason,
        SpawnResult::Spawned(_) => panic!("the helper closes stdin, so delivery must fail"),
        SpawnResult::NotLaunched { reason } => {
            panic!("the child existed, so absence cannot be a pre-exec fact: {reason}")
        }
    };
    assert_eq!(
        host.owned(),
        1,
        "spawn -> {error:?} and the host owns nothing: it created a process and dropped the \
         handle, so nothing can signal or reap it (invariant 10)"
    );
    // The child really was created, which is what makes the count above about ownership rather
    // than about a spawn that never got that far.
    let pid = u32::try_from(common::await_number(&mark)).expect("a pid");

    assert_ne!(pid, 0, "the helper reported a real process identifier");

    common::shutdown_retaining_incomplete(&mut host, "shutdown");
}

/// The same failure begins with a live child and retains its capabilities after cleanup remains
/// unconfirmed.
///
/// Unix only: the helper closes the pipe by redirecting its own standard input to the null device
/// through an exec in place, and Windows has no exec in place. Doing it with a direct `close(0)`
/// would need `unsafe` in a test file, which is the one thing the unsafe grant is trying not to
/// normalise.
#[cfg(unix)]
#[test]
fn a_child_kept_by_a_failed_spawn_is_still_reachable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut host = RealHost::with_grace(Duration::from_millis(300));
    let mark = dir.path().join("deaf.pid");

    let mut spec = common::helper_spec(common::DEAF, &mark, dir.path());
    spec.stdin = LaunchInput::WriteThenClose(vec![b'x'; common::MORE_THAN_A_PIPE_HOLDS]);

    let error = match host.spawn(&spec, &common::no_install()) {
        SpawnResult::CleanupUnconfirmed { reason } => reason,
        SpawnResult::Spawned(_) => panic!("the helper redirects stdin, so delivery must fail"),
        SpawnResult::NotLaunched { reason } => {
            panic!("the child existed, so absence cannot be a pre-exec fact: {reason}")
        }
    };
    assert_eq!(
        host.owned(),
        1,
        "spawn -> {error:?} and the host owns nothing"
    );

    // Anti-vacuity: the post-exec error came from a real helper that reached its heartbeat path.
    common::await_number(&mark);
    let beat = common::beat_path(&mark);
    assert!(
        !common::beating(&beat),
        "the polite/hard cleanup did not stop the directly owned child"
    );

    common::shutdown_retaining_incomplete(&mut host, "shutdown");
    assert!(
        !common::beating(&beat),
        "the child kept by a failed spawn outlived shutdown: the host owned it in name only"
    );
}

/// Failed-spawn cleanup selects its own `Owned` entry and leaves an existing
/// healthy child's heartbeat, key and ownership intact.
#[cfg(unix)]
#[test]
fn failed_spawn_cleanup_does_not_touch_a_healthy_bystander() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut host = RealHost::with_grace(Duration::from_millis(300));

    let healthy_mark = dir.path().join("healthy.pid");
    let healthy = common::spawned(
        host.spawn(
            &common::helper_spec(common::PARK, &healthy_mark, dir.path()),
            &common::no_install(),
        ),
        "healthy spawn",
    );
    common::await_number(&healthy_mark);
    let healthy_beat = common::beat_path(&healthy_mark);
    assert!(
        common::beating_after_start(&healthy_beat),
        "healthy control is not live"
    );

    let failed_mark = dir.path().join("failed.pid");
    let mut failed = common::helper_spec(common::DEAF, &failed_mark, dir.path());
    failed.stdin = LaunchInput::WriteThenClose(vec![b'x'; common::MORE_THAN_A_PIPE_HOLDS]);
    assert!(matches!(
        host.spawn(&failed, &common::no_install()),
        SpawnResult::CleanupUnconfirmed { .. }
    ));

    assert_eq!(host.owned(), 2, "both ownership rows remain represented");
    assert_eq!(host.alive(&healthy), dudes_supervisor::Liveness::Alive);
    // A signalled helper can never beat again, so a wider window cannot hide a signal; it only
    // stops a descheduled live helper from reading as dead.
    assert!(
        common::beating_after_start(&healthy_beat),
        "single-child cleanup signalled the healthy bystander"
    );

    common::shutdown_retaining_incomplete(&mut host, "shutdown");
}
