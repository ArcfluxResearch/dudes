//! `ProcessHost::shutdown` is called on every exit path, exactly once, and after the
//! journal flush.
//!
//! # Why the arms are in-process, and what that costs
//!
//! Two of the three causes — the `system.shutdown` RPC and `SIGTERM` — reach the loop by exactly
//! one line each: the registered handler calls `Handle::stop(ExitCause::ShutdownRpc)` and the signal
//! thread calls `Handle::stop(ExitCause::Signal)`. These arms call the same line. What they do **not**
//! exercise is the wire and the signal delivery themselves.
//!
//! **The over-the-wire arm is `wire.rs`'s, and it exists** — it drives the real `Dispatcher` over a
//! real socket, against the classifier `boot` builds and the credential the daemon minted.
//!
//! **What is genuinely not available is the *separate-process* arm**, and one blocker remains:
//! `main.rs` hard-codes the Claude adapter, whose command is `claude`, so a real `duded` on a box
//! without that program has no child to be asserted gone. Supplying a permissive authorizer to work
//! around that is not an option, and this package could not: `AdmitAll` and `RefuseAll` are behind
//! `dudes-rpc`'s `testing` feature.
//!
//! # What the fake buys, which is the whole
//!
//! `FakeHost::shutdown` **models** the sweep: it counts the call and marks every pid it handed out
//! `NoSuchPid`, so every key it returned reads `Gone` afterwards. A fake that returned `Ok(())` and
//! changed nothing would let a daemon which never called it pass a test that said it did. Each arm
//! that creates children therefore asserts the count, the sweep, and — as the anti-vacuity control —
//! that every one of those keys read `Alive` first.
//!
//! # The fourth path, which is not an `ExitCause`
//!
//! `Daemon::run` is not the only way a daemon leaves the world. Dropping before `run` still owes
//! teardown, but a fresh host cannot turn an earlier `CleanupUnconfirmed` into evidence that
//! reissue is safe. [`drop_before_run_preserves_unconfirmed_cleanup_and_tears_down_once`] records
//! that actual uncertainty first, then checks that reboot preserves it and `Drop` tears down once.
//!
//! # The mutation control is manual, recorded rather than kept
//!
//! *"reads alive against a build with the call removed"*: deleting `self.ctx.host.shutdown()` from
//! `Daemon::exit` and running this file leaves every spawned key reading `Alive` and
//! `shutdowns == 0`. There is no `impl Drop for RealHost`, so nothing but the call kills them. That
//! mutation is run and reverted; what stands here is the assertion.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use duded::startup::{TeardownStep, Witness};
use duded::{ExitCause, boot};
use dudes_proto::{
    ExitStatus, InstructionBaselineRef, InstructionGapKind, InstructionObservationOwner,
    InstructionSettlement, InstructionUnknownReason, Record, TurnOutcome,
};
use dudes_supervisor::host::Liveness;
use dudes_supervisor::{Input, World};
use dudes_types::TurnId;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record as SpanRecord};
use tracing::{Event, Metadata, Subscriber};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

const FLOOR_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/floor-release-v3"
);

fn floor_fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("Floor fixture");
    fs::create_dir(root.path().join("assets")).expect("assets");
    for path in [
        "index.html",
        "release-manifest.json",
        "assets/app-01234567.js",
    ] {
        fs::copy(Path::new(FLOOR_FIXTURE).join(path), root.path().join(path)).expect("copy");
    }
    root
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<String>>>);

struct Fields(Vec<String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push(format!("{}={value:?}", field.name()));
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &SpanRecord<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields(Vec::new());
        event.record(&mut fields);
        self.0
            .lock()
            .expect("capture lock")
            .push(fields.0.join(" "));
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// Boot a daemon with one agent whose turn has a child, drive it to `cause`, and return what the
/// fakes recorded.
fn exit_through(cause: ExitCause) {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let runner = Arc::new(ScriptedRunner::default());
    let witness = Witness::default();
    let injected = seams(
        &settings,
        host.clone(),
        Arc::clone(&runner),
        Arc::new(TestClock::new()),
        witness.clone(),
    );
    host.bind_child_ends(Arc::clone(&injected.children));
    let daemon = boot(&settings, injected).expect("boot");
    let mut running = common::start(daemon);

    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let turn = fixture.await_turn();
    fixture.await_record(
        "the child the shutdown control will sweep",
        |record| match record {
            dudes_proto::Record::TurnSpawned { turn: spawned, .. } if *spawned == turn => Some(()),
            _ => None,
        },
    );

    // Control: the child the daemon launched is alive before the exit, so a later `Gone` is this
    // call's doing and not a fake that never knew it.
    let pids = host.pids();
    assert!(!pids.is_empty(), "the daemon launched a child to be swept");
    for pid in &pids {
        assert_eq!(
            host.alive_of(*pid),
            Liveness::Alive,
            "control: pid {pid} is a live child before the exit"
        );
    }
    assert_eq!(host.shutdowns(), 0, "nothing has called shutdown yet");

    let reported = running.stop(cause);
    assert_eq!(reported, cause, "the daemon reports the cause it was given");

    assert_eq!(
        host.shutdowns(),
        1,
        "exactly once, and once is the number an exit path owes"
    );
    for pid in &pids {
        assert_eq!(
            host.alive_of(*pid),
            Liveness::Gone,
            "pid {pid} was swept by the shutdown"
        );
    }
    assert_eq!(
        runner.sweeps(),
        1,
        "the predicates are swept in the same sequence"
    );

    // The order: the flush is before the sweep.
    let teardown = witness.teardown.steps();
    assert_eq!(
        teardown,
        TeardownStep::ORDER.to_vec(),
        "the exit sequence ran in its documented order"
    );
    let index = |step: TeardownStep| {
        teardown
            .iter()
            .position(|seen| *seen == step)
            .expect("every step ran")
    };
    assert!(
        index(TeardownStep::JournalFlushed) < index(TeardownStep::HostShutdown),
        "shutdown is ordered after the journal flush: a sweep that ran first would kill the \
         children whose turn ends were still being written"
    );
}

/// The `system.shutdown` RPC path. `ExitCause::ShutdownRpc` is what the registered handler sends,
/// and this drives the same line.
#[test]
fn the_shutdown_rpc_path_calls_shutdown_exactly_once_after_the_flush() {
    exit_through(ExitCause::ShutdownRpc);
}

/// The `SIGTERM` path. `ExitCause::Signal` is what `main.rs`'s signal thread sends when `sigwait`
/// returns, and this drives the same line.
#[test]
fn the_signal_path_calls_shutdown_exactly_once_after_the_flush() {
    exit_through(ExitCause::Signal);
}

/// The `main`-returning path: the loop stopped without being asked over the wire or by a signal.
#[test]
fn the_main_returning_path_calls_shutdown_exactly_once_after_the_flush() {
    exit_through(ExitCause::Ended);
}

/// An idle daemon — one that never launched a child — still runs the whole exit sequence.
///
/// The control for the three arms above: without it, a `shutdown` that only ran when children
/// existed would pass every one of them.
#[test]
fn an_idle_daemon_still_shuts_its_host_down() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::new();
    let runner = Arc::new(ScriptedRunner::default());
    let witness = Witness::default();
    let daemon = boot(
        &fixture.settings(),
        seams(
            &settings,
            host.clone(),
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            witness.clone(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    running.stop(ExitCause::Ended);

    assert!(host.pids().is_empty(), "control: it launched nothing");
    assert_eq!(host.shutdowns(), 1, "and still called shutdown once");
    assert_eq!(runner.sweeps(), 1);
    assert_eq!(witness.teardown.steps(), TeardownStep::ORDER.to_vec());
}

#[test]
fn host_shutdown_error_is_visible_and_later_teardown_runs() {
    let fixture = Fixture::new();
    let floor = floor_fixture();
    let host = common::ShutdownErrorHost::new();
    let runner = Arc::new(ScriptedRunner::default());
    let witness = Witness::default();
    let mut settings = fixture.settings();
    settings.floor = Some(duded::settings::FloorSettings {
        assets_dir: floor.path().to_path_buf(),
        port: Some(0),
    });
    let daemon = boot(
        &settings,
        seams(
            &settings,
            host.clone(),
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            witness.clone(),
        ),
    )
    .expect("boot");
    let floor_port = daemon.floor_addr().expect("Floor address").port();
    let handle = daemon.handle();
    handle.stop(ExitCause::Ended);

    let capture = Capture::default();
    let events = Arc::clone(&capture.0);
    let cause = tracing::subscriber::with_default(capture, || daemon.run()).expect("daemon run");

    assert_eq!(cause, ExitCause::Ended);
    assert_eq!(
        host.shutdowns(),
        1,
        "shutdown was not attempted exactly once"
    );
    assert_eq!(host.errors(), 1, "the shutdown error arm did not run");
    let events = events.lock().expect("captured events").join("\n");
    assert!(
        events.contains("host shutdown left unresolved child ownership; continuing later teardown"),
        "the unresolved-ownership diagnostic was not emitted: {events}"
    );
    assert!(
        events.contains("injected unresolved ownership"),
        "the concrete shutdown error was not visible: {events}"
    );
    assert_eq!(
        witness.teardown.steps(),
        TeardownStep::FLOOR_ORDER.to_vec(),
        "the Floor and control teardown after HostShutdown did not run"
    );
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", floor_port)).is_err(),
        "the host error did not skip Floor teardown"
    );
    assert_eq!(runner.sweeps(), 1, "the predicate sweep was skipped");
}

fn replay_shutdown_fixture(fixture: &Fixture) -> World {
    let mut world = World::default();
    for (lsn, entry) in fixture.entries() {
        world.apply(lsn, &entry);
    }
    world
}

fn recorded_unconfirmed_cleanup(fixture: &Fixture) -> (TurnId, World) {
    let settings = fixture.settings();
    let crashed = common::UnconfirmedHost::new(true);
    let mut first_seams = seams(
        &settings,
        crashed.clone(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    first_seams.adapters = dudes_providers::registry::Adapters::new([Box::new(
        dudes_providers::claude::ClaudeAdapter::new(
            dudes_providers::claude::config::ClaudeConfig {
                command: std::env::current_exe()
                    .expect("owned test program")
                    .into_os_string(),
                ..dudes_providers::claude::config::ClaudeConfig::default()
            },
        ),
    )
        as Box<dyn dudes_providers::adapter::Adapter>]);
    let first = boot(&fixture.settings(), first_seams).expect("boot");
    let mut first = common::start(first);
    let agent = common::hire(&first.handle, fixture.state.path());
    common::set_goal(
        &first.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let turn = fixture.await_turn();
    let baseline = fixture.await_record("actual unconfirmed cleanup", |record| match record {
        Record::InstructionObservationFinished {
            owner:
                InstructionObservationOwner::Managed {
                    agent: seen_agent,
                    turn: seen,
                    ..
                },
            baseline: InstructionBaselineRef::Started { lsn },
            settlement:
                InstructionSettlement::Unknown {
                    reason: InstructionUnknownReason::CleanupUnconfirmed,
                    end: None,
                    sampled_at: None,
                    recovery: false,
                    comparison,
                },
            ..
        } if *seen_agent == agent
            && *seen == turn
            && comparison
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::CleanupUnconfirmed) =>
        {
            Some(*lsn)
        }
        _ => None,
    });
    first.stop(ExitCause::Ended);
    assert_eq!(crashed.children(), 0, "control: run one created no child");
    assert_eq!(crashed.shared().shutdowns(), 1);
    let before = replay_shutdown_fixture(fixture);
    let attempt = before.instructions.attempt(baseline).unwrap();
    assert!(attempt.finished.is_some());
    assert!(attempt.boundary_settled.is_none());
    assert!(
        before
            .instructions
            .unresolved_launch_attempts(agent)
            .any(|(seen, _)| *seen == baseline)
    );
    (turn, before)
}

fn assert_unconfirmed_cleanup_preserved(
    fixture: &Fixture,
    before: &World,
    after: &World,
    turn: TurnId,
) {
    let agent = before.turns[&turn].agent;
    assert_eq!(after.agents[&agent].open_turn, Some(turn));
    assert_eq!(after.turns[&turn].task, before.turns[&turn].task);
    assert_eq!(
        after
            .instructions
            .unresolved_launch_attempts(agent)
            .collect::<Vec<_>>(),
        before
            .instructions
            .unresolved_launch_attempts(agent)
            .collect::<Vec<_>>()
    );
    assert!(
        !fixture.records().iter().any(|record| match record {
            Record::TurnSpawned { turn: seen, .. }
            | Record::TurnEnded { turn: seen, .. }
            | Record::InstructionAttemptBoundarySettled {
                owner: InstructionObservationOwner::Managed { turn: seen, .. },
                ..
            } => *seen == turn,
            _ => false,
        }),
        "neither a fresh empty host nor teardown proves native absence"
    );
}

#[test]
fn drop_before_run_preserves_unconfirmed_cleanup_and_tears_down_once() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let (turn, before) = recorded_unconfirmed_cleanup(&fixture);

    // A fresh empty host has no native ownership evidence for the earlier attempt.
    let restarted = common::UnconfirmedHost::new(false);
    let host = restarted.shared();
    let runner = Arc::new(ScriptedRunner::default());
    let witness = Witness::default();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            restarted.clone(),
            Arc::clone(&runner),
            Arc::new(TestClock::ticking(1_787_394_700_000, 1)),
            witness.clone(),
        ),
    )
    .expect("the same state directory boots again");

    assert!(host.pids().is_empty());
    assert!(host.spawned().is_empty());
    assert_eq!(host.shutdowns(), 0, "and nothing has swept yet");
    assert_unconfirmed_cleanup_preserved(&fixture, &before, daemon.world(), turn);

    // The path: the daemon is dropped without `run` ever being called, exactly as `boot`'s own `?`
    // would drop it.
    drop(daemon);

    assert_eq!(
        host.shutdowns(),
        1,
        "invariant 10 holds: dropping the daemon swept its host once"
    );
    assert_eq!(
        runner.sweeps(),
        1,
        "the predicates go with it, in the same sequence"
    );
    assert_eq!(
        witness.teardown.steps(),
        TeardownStep::ORDER.to_vec(),
        "the whole exit sequence ran, in order"
    );
    assert!(host.pids().is_empty());
    assert!(host.spawned().is_empty());
    assert_unconfirmed_cleanup_preserved(
        &fixture,
        &before,
        &replay_shutdown_fixture(&fixture),
        turn,
    );
}

/// And the ordinary path is not made to sweep twice by the destructor.
///
/// The tests above assert `shutdown` is called **exactly once**, so `Drop` running the sequence a second
/// time after `run` had already run it would break every arm above. This is the flag that stops it,
/// asserted where a reader looks for it.
#[test]
fn a_daemon_that_ran_does_not_sweep_again_when_it_is_dropped() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::new();
    let runner = Arc::new(ScriptedRunner::default());
    let daemon = boot(
        &fixture.settings(),
        seams(
            &settings,
            host.clone(),
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    running.stop(ExitCause::Ended);
    // `Running::stop` joins the thread that owns the daemon, so the value has been dropped by the
    // time this returns.
    assert_eq!(host.shutdowns(), 1, "once, not twice");
    assert_eq!(runner.sweeps(), 1);
}

/// A turn that has already ended is not a child the exit path has to sweep, and the daemon still
/// calls `shutdown` once — the fake's own sweep is idempotent over pids it already marked.
#[test]
fn a_daemon_whose_turn_has_ended_shuts_down_once() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::new();
    let daemon = boot(
        &fixture.settings(),
        seams(
            &settings,
            host.clone(),
            Arc::new(ScriptedRunner::scripted([common::fails()])),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let turn = fixture.await_turn();
    running.handle.input(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: None,
    });
    fixture.await_record("TurnEnded", |record| {
        matches!(record, dudes_proto::Record::TurnEnded { .. }).then_some(())
    });

    running.stop(ExitCause::ShutdownRpc);
    assert_eq!(host.shutdowns(), 1);
}

#[test]
fn enabled_floor_stops_before_control_transport_on_every_exit_path() {
    for cause in [ExitCause::ShutdownRpc, ExitCause::Signal, ExitCause::Ended] {
        let fixture = Fixture::new();
        let floor = floor_fixture();
        let host = SharedHost::new();
        let witness = Witness::default();
        let mut settings = fixture.settings();
        settings.floor = Some(duded::settings::FloorSettings {
            assets_dir: floor.path().to_path_buf(),
            port: Some(0),
        });
        let daemon = boot(
            &settings,
            seams(
                &settings,
                host.clone(),
                Arc::new(ScriptedRunner::default()),
                Arc::new(TestClock::new()),
                witness.clone(),
            ),
        )
        .expect("enabled Floor boot");
        let port = daemon.floor_addr().expect("Floor address").port();
        let mut running = common::start(daemon);
        assert_eq!(running.stop(cause), cause);
        assert_eq!(witness.teardown.steps(), TeardownStep::FLOOR_ORDER.to_vec());
        assert_eq!(host.shutdowns(), 1);
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
    }
}

#[test]
fn enabled_floor_drop_before_run_stops_once_within_bound() {
    let fixture = Fixture::new();
    let floor = floor_fixture();
    let host = SharedHost::new();
    let witness = Witness::default();
    let mut settings = fixture.settings();
    settings.floor = Some(duded::settings::FloorSettings {
        assets_dir: floor.path().to_path_buf(),
        port: Some(0),
    });
    let daemon = boot(
        &settings,
        seams(
            &settings,
            host.clone(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            witness.clone(),
        ),
    )
    .expect("enabled Floor boot");
    let port = daemon.floor_addr().expect("Floor address").port();
    let started = std::time::Instant::now();
    drop(daemon);
    assert!(started.elapsed() < duded::floor_gateway::JOIN_BOUND);
    assert_eq!(host.shutdowns(), 1);
    assert_eq!(witness.teardown.steps(), TeardownStep::FLOOR_ORDER.to_vec());
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
}
