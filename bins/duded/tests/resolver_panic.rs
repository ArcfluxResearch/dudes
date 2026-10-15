//! A peer resolver that panics stops the daemon for a named cause.
//!
//! # Why a silent stall is not acceptable
//!
//! The daemon's only reachable stop paths are its own door — `system.shutdown`, and every hook
//! event an agent's shim sends — and a signal. A resolver that panics takes the first of those
//! with it while the loop thread carries on: the daemon answers nothing, reports nothing, keeps
//! every child, and the only thing left that can reach it is `SIGTERM`. Nothing in the exit
//! sequence runs, because the exit sequence is downstream of an exit *cause* and a panic produces
//! none.
//!
//! Naming the cause is what makes it observable. `ExitCause::Ended` would not do: it is what an
//! ordinary in-process stop produces, so a test asserting it could not tell a panic stop from the
//! fixture's own stop.
//!
//! # Why the arms are shaped this way
//!
//! The bounded poll on the teardown recorder, rather than a bare `join()`, is deliberate. Against
//! a daemon that does *not* stop, a `join()` blocks for
//! ever and the arm hangs instead of failing. Polling with a deadline makes the negative arm
//! report the property it falsified and then let `Running`'s own `Drop` take the daemon down.
//!
//! # And no connection is served with a column nobody resolved
//!
//! Stopping is half of it. The other half is what the broken resolver's own connection, and every
//! connection after it, is answered — and the answer is *nothing*: the guard re-raises rather than
//! returning a resolution at all. **The reason is not that there is no value to return.**
//! `PeerResolution::Refuse` exists for that, and only the daemon barrier's non-answers and the
//! host loop's exhausted ceiling may produce it. A caught
//! panic is neither, and it means something different besides: a refusal is one bounded
//! connection lost and reconnected, a panic is a daemon that can place no peer at all and stops
//! for it. The third arm pins the latch through the admission halt and final transport
//! release, keeping a broken resolver from being asked twice.
//!
//! The divergence itself is pinned where `GuardedResolver` is reachable — `classify::tests`, the
//! type being `pub(crate)` — by `a_caught_panic_diverges_rather_than_answering_a_refusal`, whose
//! control is that an inner resolver's own `Refuse` passes straight back out of the guard.
#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use duded::predicate::{PredicateOutcome, PredicateRunner};
use duded::startup::{TeardownStep, Witness};
use duded::{ExitCause, boot};
use dudes_providers::launch::LaunchSpec;
use dudes_rpc::Client;
use dudes_rpc::auth::{PeerIdentity, PeerResolution, PeerResolver};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams, seams_with};

/// How long a poll waits before calling the property false.
const DEADLINE: Duration = Duration::from_secs(10);

/// How long [`GatedSweep`] holds the exit sequence open before it releases itself.
///
/// Strictly longer than [`DEADLINE`] so a failed polling assertion fires before the gate's
/// fail-safe release lets teardown finish and accidentally turns a stuck daemon into a pass.
const SWEEP_BOUND: Duration = Duration::from_secs(DEADLINE.as_secs() * 4);

/// A resolver that cannot answer, in the way a process-table read fails: by panicking.
///
/// `asked` is the anti-vacuity counter. Without it every assertion below is satisfied by a run in
/// which no connection ever reached the resolver at all.
#[derive(Debug, Default)]
struct PanickingResolver {
    asked: AtomicUsize,
}

impl PanickingResolver {
    /// How many peers it has been asked about.
    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

impl PeerResolver for PanickingResolver {
    fn resolve(&self, _peer: &PeerIdentity) -> PeerResolution {
        self.asked.fetch_add(1, Ordering::SeqCst);
        panic!("this resolver cannot read what it was asked to read");
    }
}

/// A predicate runner whose exit-path sweep blocks until the test lets it through.
///
/// `Daemon::exit` sweeps at step 2 of four and closes the door at step 4, so a held sweep is a
/// daemon that is *stopping* with its accept loop still running and its socket still connectable.
/// That window is the only place the latch is observable from outside the guard, and holding it
/// open is what turns the third arm's second connection into a fact rather than a race against the
/// exit sequence.
///
/// It counts nothing the other arms read; `run` is delegated because this fixture drives no
/// predicate and should not quietly answer as though it had.
#[derive(Debug, Default)]
struct GatedSweep {
    inner: ScriptedRunner,
    entered: AtomicBool,
    released: AtomicBool,
}

impl GatedSweep {
    /// Whether the exit sequence has reached the sweep.
    fn sweeping(&self) -> bool {
        self.entered.load(Ordering::SeqCst)
    }

    /// Let the exit sequence run on.
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }
}

impl PredicateRunner for GatedSweep {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> std::io::Result<PredicateOutcome> {
        self.inner.run(spec, timeout)
    }

    fn sweep(&self) {
        self.entered.store(true, Ordering::SeqCst);
        // Bounded, for the reason the module doc gives about `join()`: an assertion that fails
        // before the release must end the test rather than wedge the daemon it is holding. The
        // bound is SWEEP_BOUND and not DEADLINE, for the reason that constant carries.
        let deadline = Instant::now() + SWEEP_BOUND;
        while !self.released.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.inner.sweep();
    }
}

/// Poll `read` until it is true, or fail saying it never became true.
fn poll_until(what: &str, read: impl Fn() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !read() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The control: the shipped resolver answers, and the daemon stops only for the cause it was told.
///
/// Run as its own arm because the subject's cause means nothing unless some *other* cause is what
/// this fixture produces by default. A daemon that returned `ResolverPanicked` for every exit would
/// pass the subject perfectly.
#[test]
fn a_shipped_resolver_stops_for_the_cause_it_was_asked_to() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let witness = Witness::default();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            witness.clone(),
        ),
    )
    .expect("the fixture boots");
    let mut running = common::start(daemon);

    // The shipped resolver is asked about this connection and answers, so the door works.
    let _peer = Client::connect(settings.socket()).expect("the shipped daemon answers its door");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        witness.teardown.steps().is_empty(),
        "a connection through the shipped resolver does not stop the daemon: {:?}",
        witness.teardown.steps()
    );

    assert_eq!(
        running.stop(ExitCause::Ended),
        ExitCause::Ended,
        "this fixture's daemon stops for the cause it is given, so the subject's cause is a \
         discriminator and not the only value this exit path can produce"
    );
}

/// The subject: one connection, one panicking resolve, and the daemon exits saying so.
#[test]
fn a_panicking_resolver_stops_the_daemon_with_a_named_cause() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let witness = Witness::default();
    let resolver = Arc::new(PanickingResolver::default());
    let daemon = boot(
        &settings,
        seams_with(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            witness.clone(),
            Arc::clone(&resolver) as Arc<dyn PeerResolver>,
        ),
    )
    .expect("the fixture boots");
    let mut running = common::start(daemon);

    // Before: the daemon is up and has torn nothing down, so the teardown read below is caused by
    // the connection and not by boot.
    assert!(
        witness.teardown.steps().is_empty(),
        "the daemon has not begun its exit sequence before anything connected"
    );

    let peer = Client::connect(settings.socket()).expect("the door is open");
    poll_until("the connection never reached the resolver", || {
        resolver.asked() >= 1
    });
    poll_until(
        "the daemon kept running after its resolver panicked: no exit sequence within the bound",
        || {
            witness
                .teardown
                .steps()
                .contains(&TeardownStep::HostShutdown)
        },
    );
    // Dropped before the join so the exit path is not waiting on this peer's halt.
    drop(peer);

    assert_eq!(
        running.join(),
        ExitCause::ResolverPanicked,
        "the daemon stopped itself and said why"
    );
    assert_eq!(
        resolver.asked(),
        1,
        "exactly one connection, resolved exactly once: the assertions above are not vacuous"
    );
}

/// The latch: after the panic, a later connection is refused without the inner resolver being
/// asked at all.
///
/// Admission is halted before the predicate sweep, so the gate holds the daemon after the door has
/// stopped accepting. The resolver count must remain one throughout that hold, and after release
/// the final transport drop makes a later connection fail at connect rather than entering a
/// resolver the daemon already knows is broken.
#[test]
fn no_connection_reaches_a_broken_resolver_after_exit_begins() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let witness = Witness::default();
    let resolver = Arc::new(PanickingResolver::default());
    let predicates = Arc::new(GatedSweep::default());
    let daemon = boot(
        &settings,
        seams_with(
            &settings,
            SharedHost::new(),
            Arc::clone(&predicates),
            Arc::new(TestClock::new()),
            witness.clone(),
            Arc::clone(&resolver) as Arc<dyn PeerResolver>,
        ),
    )
    .expect("the fixture boots");
    let mut running = common::start(daemon);

    let first = Client::connect(settings.socket()).expect("the door is open");
    poll_until("the first connection never reached the resolver", || {
        resolver.asked() >= 1
    });
    poll_until(
        "the resolver's panic never started an exit sequence: the sweep was not reached",
        || predicates.sweeping(),
    );
    drop(first);

    assert_eq!(
        resolver.asked(),
        1,
        "no post-stop connection can ask the broken resolver again"
    );

    predicates.release();
    assert_eq!(
        running.join(),
        ExitCause::ResolverPanicked,
        "the exit this test held open is still the one the panic named"
    );
    assert!(
        Client::connect(settings.socket()).is_err(),
        "the final transport release refuses later connections"
    );
}
