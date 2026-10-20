//! This crate's central requirement: a deferred kill against a reaped
//! identifier does **not** signal, and the same code path against a live matching child does.
//!
//! # How the hazard is reproduced rather than waited for
//!
//! The case is this: a child exits, is reaped, its identifier is handed out again, and a
//! kill decided earlier is still holding that identifier by value. Waiting for a real recycle takes
//! however long the kernel's identifier space takes to wrap, so the test builds the same situation
//! deterministically — a key whose identifier belongs to a **live** process and whose start value
//! belongs to a different, already-dead one.
//!
//! That construction is the point rather than a convenience. The target of the stale key *exists*,
//! so an implementation that probed for existence instead of comparing the key would sail through
//! the guard and kill it. This file is the sentence *the stranger exists* made
//! mechanical.
//!
//! # Two hosts, because a reservation would hide the bug
//!
//! The killer is a different `RealHost` from the one that owns the target. That is not scaffolding:
//! a host that owns a child holds it unreaped, and an unreaped child's identifier cannot be reused,
//! so the dangerous case *cannot* be built against a host's own children. The hazard only exists
//! where the reservation has lapsed — after a restart, or after this run already reaped — and a
//! host that never spawned the target is exactly that situation.
//!
//! # Why "did not signal" is witnessed by a heartbeat
//!
//! Not by the absence of an error, which would also be returned by a kill path that never fires,
//! and not by a probe, which cannot tell a running process from one that has exited and not yet
//! been reaped — both answer `Alive`. The helper advances a counter while it runs, so the witness
//! for "still alive" is the counter still moving, and the witness for "died" is that it stopped.
//!
//! # And the two tests at the bottom, which are about the path that signals on the reservation
//!
//! Everything above is the adoption path, where the host holds only a key and the comparison
//! before every signal is the entire guarantee. On the path where the host holds the child itself,
//! what authorises the signals is the reservation — `src/kill.rs`'s module docs say why — but what
//! chooses *which* child is still the key, compared whole. The last two tests pin that selection
//! in both directions: a key recorded before a reboot does not select a child of this run, and
//! neither does a key naming another process at the same identifier.

mod common;

#[cfg(unix)]
use std::ffi::OsString;
use std::time::Duration;

use dudes_host::RealHost;
use dudes_supervisor::host::{AdoptionKey, KillOutcome, Liveness, ProcessHost};

/// Not a test. See [`common::run_if_helper`].
#[test]
fn the_helper_entry_point() {
    common::run_if_helper();
}

/// Short, because both arms wait it out and neither is about how long a child is given.
const GRACE: Duration = Duration::from_millis(300);

/// How long the owner's watcher thread has to reap after the hard signal. Reaping is not part of
/// the kill's promise, and a starved CI runner can delay that thread past the kill grace; a child
/// that never receives the hard signal still never goes `Gone`, so this only delays that failure.
#[cfg(unix)]
const REAP_DEADLINE: Duration = Duration::from_secs(5);

/// What the host answers for a child it killed and reaped. Windows never claims positive absence:
/// `src/windows.rs` documents why a pid that no longer opens answers `Unknown`, not `Gone`.
const REAPED: Liveness = if cfg!(windows) {
    Liveness::Unknown
} else {
    Liveness::Gone
};

/// Establish the dead setup child without assuming which side of the watcher race reports it.
/// `Refused { Gone }` is accepted only after a second production liveness observation says Gone;
/// the stale-key arm below still proves its live wrong-key target receives no signal.
///
/// **One attempt, no retry and no sleep.** Retrying with a pause and accepting a permission failure in between
/// would wait out the reap window and then pass, so the defect that window produces would leave
/// no trace here. The window is now presence rather
/// than an error, these helpers all run as the host's own uid, and a kill that does not land on the
/// first attempt is a result this test should report rather than retry away.
fn kill_owned_and_observe_gone(host: &mut RealHost, key: &AdoptionKey, context: &str) {
    let outcome = host.kill(key);
    if matches!(&outcome, KillOutcome::Signalled) {
        assert_eq!(host.owned(), 0, "{context}: reaped row was retained");
        assert_eq!(
            host.alive(key),
            REAPED,
            "{context}: successful kill did not establish the setup child's absence"
        );
        return;
    }
    if matches!(
        &outcome,
        KillOutcome::Refused {
            liveness: Liveness::Gone
        }
    ) {
        assert_eq!(
            host.alive(key),
            Liveness::Gone,
            "{context}: Gone refusal was not reproducible at the host seam"
        );
        return;
    }
    panic!("{context}: expected an accepted signal or a Gone refusal, got {outcome:?}");
}

/// The owning host remains responsible for reaping its child even while another host adopts and
/// kills it. Keeping this guard test-local makes a failing hard-signal assertion safe to run under
/// nextest: `Child` drop alone neither signals nor waits.
#[cfg(unix)]
struct ReapOnDrop(RealHost);

#[cfg(unix)]
impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        let _ = self.0.shutdown();
    }
}

/// The ordinary helper wrapped so its final `exec` inherits an ignored `SIGTERM` disposition.
/// Each item remains one argv part: the shell is the fixture's program, never a command string
/// assembled from dynamic input.
#[cfg(unix)]
fn term_ignoring_spec(
    mark: &std::path::Path,
    cwd: &std::path::Path,
) -> dudes_providers::launch::LaunchSpec {
    let mut spec = common::helper_spec(common::PARK, mark, cwd);
    let helper = std::mem::take(&mut spec.argv);
    spec.argv = vec![
        OsString::from("/bin/sh"),
        OsString::from("-c"),
        OsString::from(r#"trap "" TERM; exec "$0" "$@""#),
        helper[0].clone(),
    ];
    spec.argv.extend(helper.into_iter().skip(1));
    spec
}

/// The adopter has no child handle and therefore cannot wait. Its signal result only says the
/// syscall accepted the group; the owner is the independent witness that the recorded child has
/// actually become absent. A final heartbeat can race the signal delivery, while `Gone` cannot.
#[cfg(unix)]
fn await_observed_gone(owner: &RealHost, key: &AdoptionKey) {
    let started = std::time::Instant::now();
    let deadline = started + REAP_DEADLINE;
    loop {
        let liveness = owner.alive(key);
        if liveness == Liveness::Gone {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the TERM-ignoring adopted child was not observed gone after the hard signal: \
             last observed {liveness:?} after {:?}",
            started.elapsed()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The adopted path has no `Child` to wait on, so this TERM-ignoring helper proves that escalation
/// reaches the real hard signal rather than merely returning after the polite one.
#[cfg(unix)]
#[test]
fn a_real_child_that_ignores_the_polite_signal_is_struck_by_the_hard_one() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let mark = dir.path().join("term-ignoring.pid");
    let mut owner = ReapOnDrop(RealHost::with_grace(GRACE));
    let key = common::spawned(
        owner.0.spawn(
            &term_ignoring_spec(&mark, dir.path()),
            &common::no_install(),
        ),
        "spawn the TERM-ignoring child",
    );
    common::await_number(&mark);
    let beat = common::beat_path(&mark);
    assert!(common::beating(&beat), "anti-vacuity: the child is running");

    let mut adopter = RealHost::with_grace(GRACE);
    let outcome = adopter.kill(&key);
    assert!(
        matches!(outcome, KillOutcome::Signalled),
        "the adopted hard-kill path did not signal: {outcome:?}"
    );
    await_observed_gone(&owner.0, &key);
}

/// Which start value the key handed to the killer carries. The **only** difference between the two
/// arms: one field of one struct, so "the same code path" is true by construction rather than by
/// assertion.
#[derive(Clone, Copy, Debug)]
enum Arm {
    /// The start value of a child that has already been reaped — the recycled case.
    Stale,
    /// The target's own start value — the control.
    Matching,
}

/// The shared body. Returns what the kill answered and whether the target was still running
/// afterwards.
fn run(arm: Arm) -> (KillOutcome, bool) {
    let dir = tempfile::tempdir().expect("tempdir");

    // A host that spawns a child and kills it, so that a start value exists which belongs to a
    // process that is gone. This is the "deferred kill's key" half of the hazard.
    let mut owner = RealHost::with_grace(GRACE);
    let dead_mark = dir.path().join("dead.pid");
    let dead = common::spawned(
        owner.spawn(
            &common::helper_spec(common::PARK, &dead_mark, dir.path()),
            &common::no_install(),
        ),
        "spawn the child that will be reaped",
    );
    common::await_number(&dead_mark);
    kill_owned_and_observe_gone(
        &mut owner,
        &dead,
        "the setup kill must stop and reap its child",
    );

    // A second host owning a live child. To the killer below this is a stranger: it never spawned
    // it, holds no handle to it and has no reservation on its identifier.
    let mut bystander = RealHost::with_grace(GRACE);
    let live_mark = dir.path().join("live.pid");
    let live = common::spawned(
        bystander.spawn(
            &common::helper_spec(common::PARK, &live_mark, dir.path()),
            &common::no_install(),
        ),
        "spawn the live child",
    );
    common::await_number(&live_mark);
    assert_ne!(
        dead.start_time, live.start_time,
        "the two children share a start value, so the stale arm's key would match by accident; \
         Linux records this in clock ticks and two spawns inside one tick read the same value"
    );
    let beat = common::beat_path(&live_mark);
    assert!(
        common::beating(&beat),
        "anti-vacuity: the target must be running before the kill, or 'it survived' means nothing"
    );

    let mut killer = RealHost::with_grace(GRACE);
    assert_eq!(
        killer.alive(&live),
        Liveness::Alive,
        "control: the killer can see the target, so a refusal below is about the key and not \
         about a probe that never works"
    );

    let key = AdoptionKey {
        pid: live.pid,
        start_time: match arm {
            Arm::Stale => dead.start_time,
            Arm::Matching => live.start_time,
        },
        boot: live.boot.clone(),
    };
    let outcome = killer.kill(&key);
    let survived = common::beating(&beat);

    common::shutdown_retaining_incomplete(&mut bystander, "shutdown the bystander");
    (outcome, survived)
}

/// The refusal arm: an identifier that exists, a start value that does not match it, and a target
/// that is still running when the dust settles.
#[test]
fn a_kill_whose_key_no_longer_matches_signals_nothing() {
    let (outcome, survived) = run(Arm::Stale);
    assert!(
        matches!(
            outcome,
            KillOutcome::Refused {
                liveness: Liveness::Gone | Liveness::Unknown
            }
        ),
        "expected a refusal, got {outcome:?}"
    );
    assert!(
        survived,
        "the process at that identifier was signalled; the key comparison did not hold"
    );
}

/// The control arm, which is not optional: without it a `kill` that never
/// fires at all passes the refusal arm perfectly.
#[test]
fn a_kill_whose_key_still_matches_signals_and_the_child_dies() {
    let (outcome, survived) = run(Arm::Matching);
    assert!(
        matches!(&outcome, KillOutcome::Signalled),
        "expected the matching key to signal, got {outcome:?}"
    );
    assert!(
        !survived,
        "the kill reported Signalled and the target kept running"
    );
}

/// The same refusal, reached the way the daemon will reach it: kill a child, then kill it again.
///
/// This is the requirement in its literal form — spawn, reap, and a deferred kill against the reaped
/// identifier does not signal — and it is kept beside the constructed version above because the two
/// fail differently. Here the identifier is usually simply absent, which the constructed test
/// cannot exercise; there the identifier is occupied, which this one cannot.
#[test]
fn killing_a_reaped_identifier_a_second_time_signals_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut host = RealHost::with_grace(GRACE);
    let mark = dir.path().join("child.pid");
    let key = common::spawned(
        host.spawn(
            &common::helper_spec(common::PARK, &mark, dir.path()),
            &common::no_install(),
        ),
        "spawn",
    );
    common::await_number(&mark);

    kill_owned_and_observe_gone(&mut host, &key, "first kill");

    let again = host.kill(&key);
    assert!(
        matches!(again, KillOutcome::Refused { .. }),
        "a second kill of a reaped identifier must refuse, got {again:?}"
    );
}

/// What the refusal below must be, which is not the same on every platform — and asserting the
/// flavour rather than just "a refusal" is what pins both cells of the liveness diagonal.
///
/// On Linux this host's own keys carry a boot identity, so the synthetic key is *two present
/// identities that differ*: positive evidence that the machine rebooted, hence `Gone`. On macOS
/// and Windows they carry none, so it is a *presence disagreement* — a key this host could not
/// have emitted, which is evidence about nothing and must never be `Gone` (invariant 3,
/// the liveness table).
///
/// With the assertion loosened to `Refused { .. }`, answering `Gone` for the presence
/// disagreement would leave every test green.
#[cfg(target_os = "linux")]
const EXPECTED_REFUSAL: Liveness = Liveness::Gone;
/// See the Linux arm.
#[cfg(not(target_os = "linux"))]
const EXPECTED_REFUSAL: Liveness = Liveness::Unknown;

/// The owned path signals on the reservation, but it selects on the **whole key** — so a key that
/// names another boot does not select a child of this one.
///
/// The reservation says a signal cannot reach a *stranger*: the child is held unreaped, so its
/// identifier cannot have been handed to anything else. It says nothing about which of this host's
/// own children the caller meant, and on Linux that question needs the boot identity. `start_time`
/// there is clock ticks since boot, so a daemon that starts at boot and spawns its first agent at
/// a similar offset every time produces the same (pid, start value) pair across two boots. After a
/// reboot the daemon replays the journal, holds keys from the previous boot and emits a kill for
/// one; without this comparison that kill lands on a live child of this run — this crate's own
/// hazard, reached along the one axis the boot identity exists to close.
///
/// **Refusing costs a kill and never a leak**, which is what makes erring this way cheap: the
/// refused child is re-inserted (asserted below), and `shutdown` selects nothing and kills all of
/// them.
///
/// The key built here is synthetic on a platform whose `read_key` never produces `Some` — it is a
/// key this host could not emit — so which cell of the liveness diagonal it lands in depends on
/// the platform, and [`EXPECTED_REFUSAL`] asserts whichever one this runner reaches.
#[test]
fn a_key_from_another_boot_does_not_select_a_child_of_this_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut host = RealHost::with_grace(GRACE);
    let mark = dir.path().join("own.pid");
    let key = common::spawned(
        host.spawn(
            &common::helper_spec(common::PARK, &mark, dir.path()),
            &common::no_install(),
        ),
        "spawn",
    );
    common::await_number(&mark);
    let beat = common::beat_path(&mark);
    assert!(
        common::beating(&beat),
        "anti-vacuity: the child must be running before the kill"
    );

    let from_another_boot = AdoptionKey {
        pid: key.pid,
        start_time: key.start_time,
        boot: Some("a-boot-id-from-before-the-reboot".to_owned()),
    };
    let outcome = host.kill(&from_another_boot);
    assert!(
        matches!(outcome, KillOutcome::Refused { liveness } if liveness == EXPECTED_REFUSAL),
        "a key from another boot selected a child of this one, got {outcome:?}"
    );
    assert_eq!(host.owned(), 1, "a refused kill must leave the child owned");
    assert!(
        common::beating(&beat),
        "the host killed a live child on a key recorded before a reboot"
    );

    // The control, and the reason the refusal above is about the boot half: the *same* key with
    // this host's own boot value signals, and the child dies. Without it a `kill` that had simply
    // stopped working would pass everything above.
    kill_owned_and_observe_gone(&mut host, &key, "same-boot control");
    assert!(!common::beating(&beat));
}

/// The other side of that, so that "the owned path signals on the reservation" cannot be read
/// as "the owned path signals at anything".
///
/// The key still **selects**. A start value that does not match the one read at spawn names an
/// earlier process at that identifier — one this host has already reaped, which is why the number
/// was free to be handed out again — and the child standing there now is a different agent's. A
/// host that reaped agent A at pid 100 and then spawned agent B at pid 100 must not let a stale
/// kill for A take B down.
#[test]
fn a_key_with_the_wrong_start_value_does_not_kill_the_child_the_host_owns_now() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut host = RealHost::with_grace(GRACE);
    let mark = dir.path().join("current.pid");
    let key = common::spawned(
        host.spawn(
            &common::helper_spec(common::PARK, &mark, dir.path()),
            &common::no_install(),
        ),
        "spawn",
    );
    common::await_number(&mark);
    let beat = common::beat_path(&mark);
    assert!(
        common::beating(&beat),
        "anti-vacuity: the child must be running"
    );

    let for_an_earlier_process = AdoptionKey {
        pid: key.pid,
        start_time: key.start_time.wrapping_add(1),
        boot: key.boot.clone(),
    };
    let outcome = host.kill(&for_an_earlier_process);
    assert!(
        matches!(
            outcome,
            KillOutcome::Refused {
                liveness: Liveness::Gone
            }
        ),
        "expected a refusal for a key that names another process at that identifier, got \
         {outcome:?}"
    );
    assert_eq!(host.owned(), 1, "a refused kill must leave the child owned");
    assert!(
        common::beating(&beat),
        "the host killed the child it owns now on a key that named an earlier one"
    );

    // The control: the *right* key does kill it, so the refusal above is about the start value
    // and not about a kill path that stopped working on this host.
    kill_owned_and_observe_gone(&mut host, &key, "matching-start control");
    assert!(!common::beating(&beat));
}
