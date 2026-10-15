//! A panic in the accept loop's own frame stops the daemon for a named cause.
//!
//! # The door this closes, and the one it does not
//!
//! `dudes_rpc::serve` records a door and declines it in terms: `scope.spawn` **panics** when the OS
//! refuses a thread, the panic unwinds out of the accept loop's closure, and the scope joins the
//! live connections with nothing having halted them. That is the motivating instance, and it stays
//! named here because the seam below is not it.
//!
//! `serve`'s behaviour is unchanged by this file. What changed is the frame the unwind lands in:
//! `boot::start_accept`'s closure handled `Err` only, so a panic skipped both the `handle.stop` that
//! names an exit and the `ended_tx.send` a receiver waits on. The loop thread was untouched — the
//! job channel keeps other senders — so the daemon ran on with no socket call able to reach it,
//! answering `system.doctor` over a door that no longer existed and keeping every child. Invariant
//! 10's daemon-exit half with nothing but `SIGTERM` left to reach it.
//!
//! # Why the seam is not an `RLIMIT_NPROC` harness
//!
//! Forcing a real thread-creation failure needs the process limit lowered, and that limit is
//! per-user on both platforms this file runs on: lowering it inside a `cargo nextest` process
//! degrades every other test in the same run and every other process the user owns. Not hermetic,
//! and not attempted. `duded::AcceptPanic` raises the panic in the same frame instead —
//! `boot::AcceptDoor` asks it between `Listener::accept` returning and `serve` using what it
//! returned — so the unwind traverses the real scope closure, the real `thread::scope` re-raise and
//! the real `serve_inner`/`serve_with_authority` frames on its way to the catch.
//!
//! # Why the arms are shaped this way
//!
//! The bounded poll on the teardown recorder, rather than a bare `join()`, is `resolver_panic.rs`'s
//! reason and the same one applies: against a daemon that does *not* stop — which is what the
//! unrepaired code is — a `join()` blocks for ever and the arm hangs instead of failing. Polling
//! with a deadline makes the arm report the property it falsified and then let `Running`'s own
//! `Drop` take the daemon down.
#![cfg(unix)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::startup::{TeardownStep, Witness};
use duded::{ExitCause, boot};
use dudes_rpc::Client;

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

/// How long a poll waits before calling the property false.
const DEADLINE: Duration = Duration::from_secs(10);

/// Poll `read` until it is true, or fail saying it never became true.
fn poll_until(what: &str, read: impl Fn() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !read() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The control: the same door, unarmed, takes the ordinary path and exits for what it was told.
///
/// Run as its own arm because the subject's cause means nothing unless some *other* cause is what
/// this fixture produces by default. A daemon whose accept loop panicked on every connection — or
/// one that answered `AcceptLoopPanicked` for every exit — would pass the subject perfectly and
/// fail only here.
#[test]
fn an_unarmed_accept_loop_stops_for_the_cause_it_was_asked_to() {
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
    let panic = daemon.accept_panic();
    let mut running = common::start(daemon);

    // Accepted, admitted and served through an unarmed door, so the seam is what differs between
    // this arm and the subject and not the connection.
    let peer = Client::connect(settings.socket()).expect("the door is open");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        witness.teardown.steps().is_empty(),
        "an unarmed accept does not stop the daemon: {:?}",
        witness.teardown.steps()
    );
    drop(peer);

    assert_eq!(
        running.stop(ExitCause::Ended),
        ExitCause::Ended,
        "this fixture's daemon stops for the cause it is given, so the subject's cause is a \
         discriminator and not the only value this exit path can produce"
    );
    assert_eq!(
        panic.taken(),
        0,
        "the unarmed seam fired on nothing: this arm's exit is the ordinary path"
    );
}

/// The subject: one armed accept, and the daemon exits saying the accept loop panicked.
#[test]
fn a_panicking_accept_loop_stops_the_daemon_with_a_named_cause() {
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
    let panic = daemon.accept_panic();
    let mut running = common::start(daemon);

    // Before: the daemon is up and has torn nothing down, so the teardown read below is caused by
    // the connection and not by boot.
    assert!(
        witness.teardown.steps().is_empty(),
        "the daemon has not begun its exit sequence before anything connected"
    );

    // Armed after the loop is running and before anything connects: the accept thread is parked in
    // a blocking accept until this connection arrives, so there is no race to lose.
    panic.arm();
    let peer = Client::connect(settings.socket()).expect("the door is open");
    poll_until(
        "the daemon kept running after its accept loop panicked: no exit sequence within the bound",
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
        ExitCause::AcceptLoopPanicked,
        "the daemon stopped itself and said which of its own moving parts broke"
    );
    assert_eq!(
        panic.taken(),
        1,
        "exactly one accept fired the seam: the assertions above are not vacuous"
    );
}
