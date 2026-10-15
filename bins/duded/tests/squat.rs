//! The daemon captures the socket's device and inode at bind and detects a replacement
//! at that path — and invariant 3 bounds what that detection may cause.
//!
//! # What is asserted, and about whose identity
//!
//! **The daemon's own.** `SocketIdentity` as a type
//! is covered by `src/socket.rs`'s unit tests; this file covers the daemon's *use* of
//! it — the value captured at bind and the reaper's re-comparison. Deleting `check_socket` from the
//! reaper, or making the capture answer `None`, would otherwise leave the whole suite green, because
//! the detection's only consequence is a `tracing::error!` and nothing installs a subscriber.
//!
//! The reaper records each beat's [`Standing`] on the `Witness`, and every test below reads
//! that recorder rather than building an identity of its own. Both of those mutations are red
//! against it.
//!
//! # Invariant 3, with a live antecedent
//!
//! An `Inferred` event may raise a breaker rung and may never be the sole cause of `Stop` or `Done`.
//! The detection raises **no** rung — the permissive reading of invariant 3's *"may"*, taken
//! because a rung is per *agent* and a squatted control socket is a daemon-wide fact with no agent
//! to attach it to, and because no `RuntimeSignal` or `BreakerSignal` variant names it.
//! `BreakerSignal` is a wire type, so inventing one would change the published schemas.
//!
//! What is asserted is the *bound*, and it has a present antecedent: the squat was **detected**
//! — `Replaced` is on the recorder — and the journalled decisions are the same as an unmolested
//! run's, and neither reaches `Done`.
//!
//! **One absence assertion was deleted rather than kept.** It filtered for a `BreakerBeat` whose
//! `rung_after` was `Stop`. `machine.rs`'s beat short-circuits when the rung has not moved and no
//! signals were collected, and this fixture pushes no `Input::Signal` at all — so no `BreakerBeat`
//! of any rung is ever written and the filter could not match. It asserted nothing, and an absence
//! with no possible antecedent is worse than no assertion: it reads as coverage.
#![cfg(unix)]

use std::os::unix::net::UnixListener;
use std::sync::Arc;

use duded::socket::{SocketIdentity, Standing};
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::{LoopDecision, Record};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

/// A beat short enough that the re-comparison runs many times inside a test.
const FAST_BEAT: std::time::Duration = std::time::Duration::from_millis(20);

/// Long enough for many beats at [`FAST_BEAT`].
const SEVERAL_BEATS: std::time::Duration = std::time::Duration::from_millis(300);

/// What a running daemon recorded about its own socket, and the decisions it journalled.
struct Run {
    standings: Vec<Standing>,
    decisions: Vec<&'static str>,
}

/// Boot a daemon with a fast beat, optionally squat its socket, and read back what it saw.
fn observe(squat: bool) -> Run {
    let fixture = Fixture::new();
    let settings = duded::Settings {
        beat: FAST_BEAT,
        ..fixture.settings()
    };
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
    .expect("boot");
    let mut running = common::start(daemon);

    let agent = common::hire(&running.handle, fixture.state.path());
    // Paused, so the loop is decidable and quiescent: what the beats are asked to change is
    // nothing, and a decision that moved would be visible.
    running
        .handle
        .ask(duded::ports::Ask::Pause(agent))
        .expect("the pause");
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );

    let squatter = if squat {
        let socket = settings.socket();
        // The own shape: a same-uid peer unlinks the live socket and binds its own at
        // the same path. **No symlink is involved anywhere**, and the daemon's established
        // connections survive — which is why it observes nothing without this check.
        std::fs::remove_file(&socket).expect("unlink");
        Some(UnixListener::bind(&socket).expect("bind another socket at the same path"))
    } else {
        None
    };

    std::thread::sleep(SEVERAL_BEATS);
    let standings = witness.sockets.steps();
    drop(squatter);
    running.stop(ExitCause::Ended);

    let decisions = fixture
        .records()
        .iter()
        .filter_map(|record| match record {
            Record::LoopDecided { decision, .. } => Some(match decision {
                LoopDecision::Continue { .. } => "Continue",
                LoopDecision::Wait { .. } => "Wait",
                LoopDecision::Done => "Done",
                LoopDecision::Exhausted { .. } => "Exhausted",
                LoopDecision::Hold { .. } => "Hold",
            }),
            _ => None,
        })
        .collect();
    Run {
        standings,
        decisions,
    }
}

/// The daemon captured an identity at bind and re-compares it on every beat.
///
/// This is the arm that goes red when the capture stops happening: with `read_identity` answering
/// `None` the reaper has nothing to compare and records nothing, and with the reaper's call deleted
/// it records nothing either.
#[test]
fn a_running_daemon_re_compares_the_identity_it_captured_at_bind() {
    let run = observe(false);
    assert!(
        !run.standings.is_empty(),
        "the daemon captured an identity at bind and re-compared it: it recorded nothing"
    );
    assert!(
        run.standings.iter().all(|seen| *seen == Standing::Same),
        "an unmolested socket reads Same on every beat: {:?}",
        run.standings
    );
}

/// A replacement at the same path is detected — by the daemon, using the value it captured.
///
/// The control is the arm above: an unmolested run records only `Same`, so a `Replaced` here is the
/// squat and not a check that answers `Replaced` unconditionally.
#[test]
fn the_daemon_detects_a_replacement_of_the_socket_it_bound() {
    let run = observe(true);
    assert!(
        run.standings.contains(&Standing::Replaced),
        "the squat was not detected; the daemon saw {:?}",
        run.standings
    );
}

/// Invariant 3: the detection alone causes neither `Stop` nor `Done`.
///
/// The antecedent is live — the squatted run is asserted to have *detected* the squat — so this is
/// a statement about a detection that happened, not about one that may not have.
#[test]
fn a_detected_squat_causes_neither_stop_nor_done() {
    let squatted = observe(true);
    let unmolested = observe(false);

    assert!(
        squatted.standings.contains(&Standing::Replaced),
        "control: the squat was detected, so what follows is about a detection that happened"
    );
    assert!(
        !unmolested.decisions.is_empty(),
        "control: the unmolested daemon decided at least once"
    );
    assert_eq!(
        squatted.decisions, unmolested.decisions,
        "the squat changed no decision: {:?} vs {:?}",
        squatted.decisions, unmolested.decisions
    );
    assert!(
        !squatted.decisions.contains(&"Done"),
        "detection is not a route to Done (invariant 3): {:?}",
        squatted.decisions
    );
}

/// The type behind it, against a socket a real daemon bound.
///
/// `src/socket.rs`'s unit tests drive `SocketIdentity` over a socket a test bound for itself; this
/// one drives it over the daemon's. It is deliberately *not* the evidence for the bound — the recorder
/// arms above are — and is kept because the two together say the type and the daemon agree.
#[test]
fn the_identity_of_a_running_daemons_socket_reads_same_until_it_is_replaced() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    let socket = settings.socket();
    let identity = SocketIdentity::of(&socket).expect("the daemon's socket has an identity");
    assert_eq!(identity.standing(&socket), Standing::Same, "control");

    std::fs::remove_file(&socket).expect("unlink the daemon's socket");
    assert_eq!(
        identity.standing(&socket),
        Standing::Gone,
        "between the unlink and the rebind there is nothing there"
    );
    let squatter = UnixListener::bind(&socket).expect("bind another socket at the same path");
    assert_eq!(identity.standing(&socket), Standing::Replaced);

    drop(squatter);
    running.stop(ExitCause::Ended);
}
