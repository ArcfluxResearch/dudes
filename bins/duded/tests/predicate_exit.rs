//! An in-flight predicate child reads **gone** after each of the three
//! exit paths.
//!
//! # Why this file exists at all
//!
//! Having the one-shot runner makes the exit sweep *expressible* but does not by itself
//! prove the daemon **calls** it. What has to be tested is that an in-flight predicate child reads
//! gone after each of the three exit paths, not merely that the method exists. A method that is
//! tested but never called proves nothing about the daemon. So the runner here is the real
//! `dudes_host::OneShot` and the child is a real process.
//!
//! # What is real and what is driven
//!
//! The **child** is real, the **runner** is real, and the **exit sequence** is the daemon's own. The
//! *trigger* is `Handle::stop`, which is exactly the one line the registered `system.shutdown`
//! handler runs and exactly the one line `main.rs`'s signal thread runs.
//!
//! **So the three arms below are one exit sequence with three labels**, and saying so is fairer than
//! implying three independent paths: `ExitCause` reaches nothing but a log line. What shows the sweep works
//! for a *real* `SIGTERM`ed process is a **composition**, and it holds across two files:
//! `restart.rs` proves a real binary sent `SIGTERM` exits successfully, which means `run()` returned,
//! which means `exit()` ran in full — it has no early return; and this file proves the sweep *inside*
//! `exit()` kills a real predicate child. The wire trigger has its own arm in `wire.rs`.
//!
//! # The control
//!
//! Each arm asserts the child is **running** before the exit — otherwise "gone afterwards" is
//! satisfied by a child that never started. And the mutation control: with
//! `self.ctx.predicates.sweep()` deleted from `Daemon::exit`, every arm here reads the child
//! **alive** after the daemon has stopped. Run and reverted; the assertion is what stands.
//!
//! Unix-scoped: on Windows the job object's `KILL_ON_JOB_CLOSE` makes
//! the exit sweep defence in depth behind the job object rather than the load-bearing thing.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use duded::predicate::{PredicateOutcome, PredicateRunner};
use duded::settings::Seams;
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_host::{OneShot, OneShotOutcome};
use dudes_proto::rpc::goal::GoalSetParams;
use dudes_proto::{ExitStatus, GoalBudget, Record, TurnOutcome};
use dudes_providers::launch::LaunchSpec;
use dudes_supervisor::testing::FakeRng;
use dudes_supervisor::{Input, Rng as _};
use dudes_types::{GoalId, TurnId, Usd};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

/// The real one-shot runner, as the daemon's [`PredicateRunner`].
///
/// The same wrapper `main.rs` writes, repeated here because a test may name `dudes_host` and
/// `src/` may not — `host_seam.rs` is the scan that keeps that true.
struct RealRunner(OneShot);

impl PredicateRunner for RealRunner {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<PredicateOutcome> {
        Ok(match self.0.run(spec, timeout)? {
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

    fn sweep(&self) {
        self.0.sweep();
    }
}

/// At the spawn boundary: even when the daemon itself is handed agent socket
/// credentials, the complete environment passed to a real predicate child contains neither one.
/// `PATH` is the positive control proving the child received the allowlisted environment rather
/// than no environment at all.
#[test]
fn predicate_child_has_no_dudes_token() {
    let cwd = tempfile::tempdir().expect("predicate cwd");
    let handed = vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), "/home/operator".into()),
        ("DUDES_TOKEN".into(), "agent-secret".into()),
        ("DUDES_SOCK".into(), "/tmp/duded.sock".into()),
        ("ANTHROPIC_API_KEY".into(), "vendor-secret".into()),
    ];
    let env = duded::predicate::allowlisted(handed);
    let argv = vec!["/usr/bin/env".to_owned()];
    let (_program, spec) = duded::predicate::spec_for(&argv, cwd.path(), &env, None)
        .expect("the out-of-tree env program resolves");
    let outcome = RealRunner(OneShot::with_grace(Duration::from_millis(200)))
        .run(&spec, Duration::from_secs(10))
        .expect("the real predicate child ran");
    let PredicateOutcome::Finished {
        exit,
        output_tail,
        timed_out,
    } = outcome
    else {
        panic!("nothing swept this child")
    };
    assert_eq!(exit, Some(0));
    assert!(!timed_out);
    assert!(
        output_tail.lines().any(|line| line == "PATH=/usr/bin:/bin"),
        "control: the child received an allowlisted value: {output_tail:?}"
    );
    for barred in ["DUDES_TOKEN=", "DUDES_SOCK=", "ANTHROPIC_API_KEY="] {
        assert!(
            !output_tail.lines().any(|line| line.starts_with(barred)),
            "{barred} reached the predicate child: {output_tail:?}"
        );
    }
}

/// Drive a real predicate child into flight, exit through `cause`, and assert the child is gone.
fn in_flight_predicate_dies_with_the_daemon(cause: ExitCause) {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let pidfile = fixture.runtime.path().join("predicate.pid");

    let host = SharedHost::new();
    // A short grace: the child exits on `SIGTERM`, so nothing here should need the escalation, and
    // a test must not pay `DEFAULT_GRACE` to find that out.
    let runner = Arc::new(RealRunner(OneShot::with_grace(Duration::from_millis(200))));
    let daemon = boot(
        &settings,
        seams(
            host,
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    let agent = common::hire(&running.handle, fixture.state.path());
    // A predicate that records its own identifier and then sits still for far longer than this
    // test. `exec` replaces the shell, so the identifier the file names is the one still running —
    // and it is the process-group leader, which is how the runner spawns it.
    //
    // Three typed argv elements and nothing interpolated: the shell here is the *operator's* choice
    // of predicate, which is allowed, not the daemon building a command from strings.
    let script = format!("echo $$ > {}; exec sleep 300", pidfile.display());
    common::set_goal(
        &running.handle,
        GoalSetParams {
            predicate: dudes_proto::goal::Predicate::Command {
                argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
                // Far longer than this test: nothing here may end because the bound expired.
                timeout_s: 600,
            },
            budget: GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
            ..common::goal_params(agent, &["/bin/sh"])
        },
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
        matches!(record, Record::TurnEnded { .. }).then_some(())
    });

    // The predicate is now in flight, and the control is that it is *running*: without this,
    // "gone afterwards" would be satisfied by a child that never started.
    let pid = common::await_number(&pidfile);
    assert!(
        !common::finished(pid),
        "control: the predicate child at pid {pid} is running before the exit"
    );

    running.stop(cause);

    // The sweep signals and the runner's own waiter reaps, so "gone" may briefly mean "a zombie
    // this process has not reaped yet" — which `finished` reads the same way `dudes-host`'s own
    // `quiet` does.
    common::await_finished(pid, "the in-flight predicate child outlived the daemon");
}

/// The `system.shutdown` RPC path.
#[test]
fn an_in_flight_predicate_dies_on_the_shutdown_rpc_path() {
    in_flight_predicate_dies_with_the_daemon(ExitCause::ShutdownRpc);
}

/// The `SIGTERM` path.
#[test]
fn an_in_flight_predicate_dies_on_the_signal_path() {
    in_flight_predicate_dies_with_the_daemon(ExitCause::Signal);
}

/// The `main`-returning path.
#[test]
fn an_in_flight_predicate_dies_when_main_returns() {
    in_flight_predicate_dies_with_the_daemon(ExitCause::Ended);
}

/// How long this file's late-run test waits for any one step.
const BOUND: Duration = Duration::from_secs(10);

/// The real runner, holding each run at its door until the exit path's sweep has returned.
///
/// This forces an order the daemon leaves to the scheduler: a predicate thread started before the
/// sweep reaches the runner after it.
struct LateRunner {
    real: RealRunner,
    entered: mpsc::Sender<()>,
    swept: mpsc::Sender<()>,
    released: Mutex<mpsc::Receiver<()>>,
    ran: mpsc::Sender<Result<PredicateOutcome, String>>,
}

impl PredicateRunner for LateRunner {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<PredicateOutcome> {
        let _ = self.entered.send(());
        self.released
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv_timeout(BOUND)
            .map_err(|_| io::Error::other("the exit path never swept"))?;
        let outcome = self.real.run(spec, timeout);
        let _ = self
            .ran
            .send(outcome.as_ref().cloned().map_err(ToString::to_string));
        outcome
    }

    fn sweep(&self) {
        self.real.sweep();
        let _ = self.swept.send(());
    }
}

/// Kills the process group led by the pid in a file, so a child this test failed to prevent does
/// not outlive the test.
struct KillGroupAt(PathBuf);

impl Drop for KillGroupAt {
    fn drop(&mut self) {
        if let Some(pid) = read_pid(&self.0).and_then(as_pid) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn as_pid(pid: u32) -> Option<rustix::process::Pid> {
    rustix::process::Pid::from_raw(i32::try_from(pid).ok()?)
}

/// A predicate thread that reaches the runner only after the exit path's sweep starts no child,
/// so nothing outlives the daemon, and its goal is judged again after a restart.
///
/// A turn that ends while the daemon drains its queue during exit starts a predicate thread, and
/// that thread can reach the runner after the sweep took its list. A child started then leads its
/// own group, the sweep has already passed, and nothing else on the exit path reaches it.
#[test]
fn a_predicate_that_reaches_the_runner_after_the_sweep_starts_no_child() {
    let fixture = Fixture::new();
    let pidfile = fixture.runtime.path().join("late.pid");
    let _cleanup = KillGroupAt(pidfile.clone());
    let (swept, released) = mpsc::channel();
    let (entered_sender, entered) = mpsc::channel();
    let (ran_sender, ran) = mpsc::channel();
    let runner = Arc::new(LateRunner {
        real: RealRunner(OneShot::with_grace(Duration::from_millis(200))),
        entered: entered_sender,
        swept,
        released: Mutex::new(released),
        ran: ran_sender,
    });
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let runtime_seams = seams(
        host.clone(),
        Arc::clone(&runner),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&runtime_seams.children));
    let daemon = boot(&fixture.settings(), runtime_seams).expect("boot");
    let mut running = common::start(daemon);

    let agent = common::hire(&running.handle, fixture.state.path());
    let script = format!("echo $$ > {}; exec sleep 30", pidfile.display());
    let goal = common::set_goal(
        &running.handle,
        GoalSetParams {
            predicate: dudes_proto::goal::Predicate::Command {
                argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
                timeout_s: 60,
            },
            ..common::goal_params(agent, &["/bin/sh"])
        },
    );
    let first_turn = fixture.await_turn();
    common::end_turn_with_native(&fixture, &running.handle, &host, first_turn);
    entered
        .recv_timeout(BOUND)
        .expect("the dispatched predicate reached its pre-sweep gate");
    running.stop(ExitCause::ShutdownRpc);

    let deadline = Instant::now() + BOUND;
    let outcome = loop {
        if let Ok(outcome) = ran.try_recv() {
            break outcome;
        }
        if let Some(pid) = read_pid(&pidfile) {
            assert!(
                common::finished(pid),
                "the predicate child at pid {pid} (pgid {:?}) started after the sweep and is \
                 alive after the daemon's exit sequence finished",
                rustix::process::getpgid(as_pid(pid))
            );
        }
        assert!(
            Instant::now() < deadline,
            "the late run neither answered nor started a child"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        outcome,
        Ok(PredicateOutcome::Swept),
        "a run that reaches a swept runner yields no evidence"
    );
    assert!(
        !pidfile.exists(),
        "no predicate child started after the sweep"
    );
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::PredicateResult { .. })),
        "a swept check journals nothing"
    );
    assert_eq!(
        checks_after_a_restart(&fixture, goal, first_turn),
        1,
        "the swept goal is checked once after restart against its original turn end"
    );
}

fn checks_after_a_restart(fixture: &Fixture, goal: GoalId, first_turn: TurnId) -> usize {
    // `FakeRng` counts from zero again in a new daemon, so it would reissue the first run's ids.
    let mut rng = FakeRng::new();
    for _ in 0..64 {
        let _ = rng.ulid();
    }
    let scripted = Arc::new(ScriptedRunner::scripted([common::passes()]));
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let runtime_seams = Seams {
        rng,
        ..seams(
            host.clone(),
            Arc::clone(&scripted),
            Arc::new(TestClock::ticking(1_787_394_700_000, 1)),
            Witness::default(),
        )
    };
    host.bind_child_ends(Arc::clone(&runtime_seams.children));
    let daemon =
        boot(&fixture.settings(), runtime_seams).expect("the same state directory boots again");
    let mut running = common::start(daemon);
    fixture.await_record("the restart's PredicateResult", |record| {
        matches!(record, Record::PredicateResult {
            goal: judged, turn: Some(checked), provenance: dudes_types::Provenance::Observed, ..
        } if *judged == goal && *checked == first_turn)
        .then_some(())
    });
    running.stop(ExitCause::Ended);
    let issued: Vec<_> = fixture
        .records()
        .iter()
        .filter_map(|record| match record {
            Record::TurnIssued {
                goal: issued, turn, ..
            } if *issued == goal => Some(*turn),
            _ => None,
        })
        .collect();
    assert_eq!(
        issued,
        [first_turn],
        "restart checks the existing end without issuing more work"
    );
    scripted.runs()
}

/// And a predicate the exit path swept journals nothing at all, with a real child.
///
/// The in-process arm in `predicate.rs` asserts this with a scripted `Swept`; this one asserts that
/// a *real* interrupted run produces one, which is the half a script cannot say.
#[test]
fn a_really_swept_predicate_journals_no_result() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let pidfile = fixture.runtime.path().join("predicate.pid");
    let runner = Arc::new(RealRunner(OneShot::with_grace(Duration::from_millis(200))));
    let daemon = boot(
        &settings,
        seams(
            SharedHost::new(),
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    let agent = common::hire(&running.handle, fixture.state.path());
    let script = format!("echo $$ > {}; exec sleep 300", pidfile.display());
    common::set_goal(
        &running.handle,
        GoalSetParams {
            predicate: dudes_proto::goal::Predicate::Command {
                argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
                timeout_s: 600,
            },
            budget: GoalBudget {
                usd: Some(Usd::from_dollars(5)),
                tokens: None,
            },
            ..common::goal_params(agent, &["/bin/sh"])
        },
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
    let pid = common::await_number(&pidfile);
    assert!(!common::finished(pid), "control: it is running");

    running.stop(ExitCause::ShutdownRpc);
    common::await_finished(pid, "the swept predicate child");

    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::PredicateResult { .. })),
        "a run the exit path cut short has no evidence, so nothing is journaled — feeding the \
         machine a `pass: false` for it would be a fabricated `Observed` verdict (invariant 3)"
    );
}
