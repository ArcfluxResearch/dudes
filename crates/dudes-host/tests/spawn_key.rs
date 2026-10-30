//! A key read at spawn describes the child just spawned, and a key assembled from
//! two different processes does not match either of them.
//!
//! The second half is what makes the key a key. A pid on its own is a number the kernel hands out
//! again; the pair (pid, start value) is what distinguishes the child from whatever holds that
//! number next, and a test that only checked the happy case would pass against an implementation
//! that compared pids alone.

mod common;

use std::thread::sleep;
use std::time::Duration;

use dudes_host::RealHost;
use dudes_supervisor::host::{AdoptionKey, Liveness, ProcessHost};

/// Not a test. See [`common::run_if_helper`] — every process test in this crate re-executes this
/// binary at this entry point.
#[test]
fn the_helper_entry_point() {
    common::run_if_helper();
}

/// Long enough that two spawns cannot share a start value on the coarsest platform: Linux records
/// `starttime` in clock ticks, usually 10 ms apart, so two children started in the same tick would
/// legitimately read the same value and the distinctness assertions would be about the clock rather
/// than about the key.
const APART: Duration = Duration::from_millis(60);

/// The key read at spawn names the child that was spawned, and the child agrees.
///
/// The identifier is checked against what the *child* reports about itself rather than against
/// anything the host returned twice, so a host that invented a plausible key could not pass.
#[test]
fn a_key_read_at_spawn_describes_the_child_just_spawned() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mark = dir.path().join("child.pid");
    let mut host = RealHost::with_grace(Duration::from_millis(200));

    let key = common::spawned(
        host.spawn(
            &common::helper_spec(common::PARK, &mark, dir.path()),
            &common::no_install(),
        ),
        "spawn",
    );

    let reported = common::await_number(&mark);
    assert_eq!(
        u64::from(key.pid),
        reported,
        "the key's identifier is not the one the child reports for itself"
    );
    assert_eq!(host.alive(&key), Liveness::Alive);

    common::shutdown_retaining_incomplete(&mut host, "shutdown");
}

/// A key whose halves come from two different children matches neither, in both directions — and
/// the unswapped keys match, so the probe demonstrably can say `Alive`.
///
/// # The platform's resolution is what this test has to survive
///
/// Linux records `starttime` in clock ticks at 100 Hz, so **two processes created inside the same
/// 10 ms tick share a start value legitimately** and a key built from one of them is a truthful
/// description of the other. Every "must not match" assertion below therefore needs its two
/// processes to be more than a tick apart, and needs that separation to be *guaranteed* rather
/// than typical.
///
/// Two separations, and neither is incidental. [`APART`] between the children covers the pair of
/// swaps. The same wait at the top covers the third assertion, whose foreign process is **this
/// test process** — created by the runner immediately before this function and, without the wait,
/// inside the first child's tick. That is not a hypothetical: it is what turned this test red on
/// Linux under nextest, where every test gets its own freshly spawned process, while macOS
/// (microseconds) and Windows (100 ns) never saw it.
///
/// `std::thread::sleep` sleeps *at least* its argument, so both separations are guaranteed at
/// 100 Hz rather than merely likely.
#[test]
fn a_key_assembled_from_two_children_matches_neither() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut host = RealHost::with_grace(Duration::from_millis(200));

    // Before the first spawn, so this process and the child below cannot share a tick. See the
    // doc comment: the third assertion is about this very process.
    sleep(APART);

    let first_mark = dir.path().join("first.pid");
    let first = common::spawned(
        host.spawn(
            &common::helper_spec(common::PARK, &first_mark, dir.path()),
            &common::no_install(),
        ),
        "spawn first",
    );
    sleep(APART);
    let second_mark = dir.path().join("second.pid");
    let second = common::spawned(
        host.spawn(
            &common::helper_spec(common::PARK, &second_mark, dir.path()),
            &common::no_install(),
        ),
        "spawn second",
    );

    // The control comes first: without it, every assertion below could be satisfied by a probe
    // that never answers `Alive` at all.
    assert_eq!(
        host.alive(&first),
        Liveness::Alive,
        "control: first is ours"
    );
    assert_eq!(
        host.alive(&second),
        Liveness::Alive,
        "control: second is ours"
    );
    assert_ne!(
        first.start_time, second.start_time,
        "two children spawned {APART:?} apart must not share a start value, or the swaps below \
         assert nothing"
    );

    let swapped = |pid: u32, start_time: u64| AdoptionKey {
        pid,
        start_time,
        boot: first.boot.clone(),
    };
    // `Gone` rather than "not `Alive`", which is the stronger statement and the one that cannot be
    // satisfied by accident: `Gone` is returned only on *positive* evidence, so it says the probe
    // resolved the identifier and the comparison rejected what it found. "Not `Alive`" would also
    // be satisfied by a probe that failed and answered `Unknown`.
    assert_eq!(
        host.alive(&swapped(second.pid, first.start_time)),
        Liveness::Gone,
        "the second child's identifier with the first child's start value must not match"
    );
    assert_eq!(
        host.alive(&swapped(first.pid, second.start_time)),
        Liveness::Gone,
        "the first child's identifier with the second child's start value must not match"
    );

    // A process that is certainly alive and certainly not ours: this very test. Separated from
    // `first` by the wait at the top of this function, without which the two share a tick on Linux
    // and this key describes this process truthfully.
    assert_eq!(
        host.alive(&swapped(std::process::id(), first.start_time)),
        Liveness::Gone,
        "a foreign process must not match a key that was never about it"
    );

    common::shutdown_retaining_incomplete(&mut host, "shutdown");
}

/// A host that has just spawned two children owns two rows; killing one keeps its reaped row
/// for native settlement, while shutdown stops the other child and retains its incomplete row. The counts catch
/// a `kill` that forgets confirmed removal or a `spawn` that forgets ownership.
#[test]
fn the_host_owns_what_it_spawned_until_it_is_reaped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut host = RealHost::with_grace(Duration::from_millis(200));
    assert_eq!(host.owned(), 0);

    let first = dir.path().join("a.pid");
    let key = common::spawned(
        host.spawn(
            &common::helper_spec(common::PARK, &first, dir.path()),
            &common::no_install(),
        ),
        "spawn",
    );
    let second = dir.path().join("b.pid");
    common::spawned(
        host.spawn(
            &common::helper_spec(common::PARK, &second, dir.path()),
            &common::no_install(),
        ),
        "spawn",
    );
    assert_eq!(host.owned(), 2);

    common::await_number(&first);
    common::await_number(&second);

    host.kill(&key);
    assert_eq!(
        host.owned(),
        2,
        "a killed child keeps its reaped row for native settlement"
    );

    common::shutdown_retaining_incomplete(&mut host, "shutdown");
}
