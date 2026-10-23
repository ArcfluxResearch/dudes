//! Invariant 10, first half: every child leads its own process group, so the sweep reaches what
//! the direct signal cannot.
//!
//! It is not covered by any other test here and
//! its absence is a specific, observed failure mode: a supervisor can carry a comment asserting that
//! its children are group leaders while nothing in its code establishes it, and the code that
//! signals `-pid` then names a group that does not exist, falls back to the bare identifier, and
//! orphans every descendant. A comment cannot go red. This can.
//!
//! # Two assertions, and neither replaces the other
//!
//! **Structural** — the child's process-group id equals its own identifier, and the grandchild's
//! process-group id equals the child's identifier while the grandchild is a *different* process.
//! Together those say exactly what "the direct signal never reached it" means: nothing aimed at the
//! child's identifier alone can touch the grandchild, and only the group names both.
//!
//! **Behavioural** — after one `kill` of the child's key, the grandchild stops running.
//!
//! The structural half is `#[cfg(unix)]`. On Windows the equivalent is job membership, and asking
//! for it (`IsProcessInJob`) means `unsafe` in a test file, outside the one module the grant covers
//! — so on that platform the membership is asserted only by its consequence, which is the
//! behavioural half below and runs everywhere.

mod common;

use std::time::Duration;

use dudes_host::RealHost;
use dudes_supervisor::host::{KillOutcome, ProcessHost};

/// Not a test. See [`common::run_if_helper`].
#[test]
fn the_helper_entry_point() {
    common::run_if_helper();
}

/// The process-group id of `pid`, asked of the kernel directly rather than of the code under test.
#[cfg(unix)]
fn process_group_of(pid: u32) -> u32 {
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).expect("a pid")).expect("nonzero");
    let group = rustix::process::getpgid(Some(pid)).expect("getpgid");
    group.as_raw_nonzero().get().try_into().expect("a pgid")
}

/// A spawned child leads its own group, its grandchild is inside that group, and one kill of the
/// child's key ends both.
#[test]
fn a_spawned_child_leads_its_own_process_group_so_the_sweep_reaches_a_grandchild() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut host = RealHost::with_grace(Duration::from_millis(300));

    let mark = dir.path().join("child.pid");
    let key = common::spawned(
        host.spawn(
            &common::helper_spec(common::GRANDCHILD, &mark, dir.path()),
            &common::no_install(),
        ),
        "spawn",
    );

    let child_pid = u32::try_from(common::await_number(&mark)).expect("a pid");
    assert_eq!(child_pid, key.pid);
    let grandchild_mark = dir.path().join("grandchild.pid");
    let grandchild_pid = u32::try_from(common::await_number(&grandchild_mark)).expect("a pid");
    assert_ne!(
        grandchild_pid, child_pid,
        "the grandchild must be a separate process, or there is nothing the direct signal misses"
    );

    #[cfg(unix)]
    {
        assert_eq!(
            process_group_of(child_pid),
            child_pid,
            "the child is not its own group leader, so a sweep of its identifier names a group \
             that is not its own"
        );
        assert_eq!(
            process_group_of(grandchild_pid),
            child_pid,
            "the grandchild is not in the child's group, so the sweep would not reach it"
        );
    }

    let grandchild_beat = common::beat_path(&grandchild_mark);
    assert!(
        common::beating_after_start(&grandchild_beat),
        "anti-vacuity: the grandchild must be running before the kill, or it cannot be shown to \
         have been stopped by it"
    );

    let outcome = host.kill(&key);
    // Single-valued, and it can be: this helper runs as the host's own uid, so the only permission
    // denial that could reach here was the reap window's, and that is now presence rather than an
    // error. An `EPERM` arriving here again is evidence, not something to tolerate.
    match outcome {
        KillOutcome::Signalled => assert_eq!(host.owned(), 0),
        other => panic!("expected the killed group to be signalled, got {other:?}"),
    }

    assert!(
        !common::beating(&grandchild_beat),
        "the grandchild outlived the kill: the sweep did not reach past the child"
    );
}
