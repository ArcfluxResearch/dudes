//! A goal whose command predicate becomes true reaches `Done`, with the predicate result
//! carrying `Observed` provenance — and one that stays false does not.
//!
//! # Why the verdict arms are scripted
//!
//! These tests are about the *loop*: that a verdict reaches the machine, that the machine
//! journals it as `Observed`, and that `Done` follows a true one and not a false one. A real child
//! would add process timing to a test about none of that. The tests that *are* about a real
//! child's lifetime — the exit sweeps — use the real runner, in `predicate_exit.rs`, and
//! `crates/dudes-host/tests/oneshot.rs` covers the runner itself.
//!
//! The containment discriminator is different: its external arm uses the real runner and asserts
//! the child's output, because its job is to prove the same-shaped out-of-tree program actually
//! spawns. Its in-tree arm remains scripted so a zero call count proves refusal happened before
//! the runner seam.
//!
//! # What is real here, and it is the part invariant 3 turns on
//!
//! The daemon's own resolution runs: `argv[0]` is looked up on the daemon-owned `PATH` and
//! canonicalized before anything is handed to the runner, and the evidence carries
//! the resolved program *beside* the operator's argv rather than in place of it.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use duded::predicate::{PredicateOutcome, PredicateRunner};
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_host::{OneShot, OneShotOutcome};
use dudes_proto::LoopDecision;
use dudes_proto::goal::Evidence;
use dudes_proto::{ExitStatus, Record, TurnOutcome};
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_supervisor::Input;
use dudes_types::Provenance;

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

/// The shipped one-shot child owner behind the daemon's predicate seam.
///
/// Repeated from `main.rs` because the out-of-tree control must prove an executable child ran;
/// [`ScriptedRunner`] remains load-bearing in the in-tree arm because its zero call count proves
/// canonical containment refuses before the runner seam.
struct RealRunner(OneShot);

impl PredicateRunner for RealRunner {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<PredicateOutcome> {
        self.run_cancellable(spec, timeout, &std::sync::atomic::AtomicBool::new(false))
    }

    fn run_cancellable(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> io::Result<PredicateOutcome> {
        Ok(match self.0.run_cancellable(spec, timeout, cancel)? {
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

/// Drive one goal to a predicate run and return the journal's records.
fn run_one_predicate(outcome: duded::predicate::PredicateOutcome) -> (Fixture, Vec<Record>) {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let host = SharedHost::new();
    let runner = Arc::new(ScriptedRunner::scripted([outcome]));
    let daemon = boot(
        &fixture.settings(),
        seams(
            &settings,
            host,
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        // Absolute and real, so the daemon's own resolution has something to resolve.
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let turn = fixture.await_turn();

    // The turn ends, which is what makes the machine judge the goal.
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
    // Waiting on the *runner* and not on the journal, because one of the three cases this helper
    // serves deliberately journals nothing: a swept run has no evidence, so a wait
    // for a `PredicateResult` would be a wait for something that must never arrive.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while runner.runs() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon never ran the predicate"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(runner.runs(), 1, "the daemon ran the predicate once");

    // The stop drains what is queued before it flushes, so a verdict already posted is journaled.
    running.stop(ExitCause::ShutdownRpc);
    let records = fixture.records();
    (fixture, records)
}

/// Drive a bare program found through the daemon-owned `PATH`, where that entry is a symlink to
/// either an in-tree or out-of-tree target. Both arms otherwise take the identical daemon path and
/// exact argv; the caller chooses the runner so the external arm can execute while the refusing
/// arm can count the calls it never receives.
#[cfg(unix)]
fn run_symlinked_predicate<P: PredicateRunner>(
    target_inside_tree: bool,
    runner: &Arc<P>,
) -> Vec<Record> {
    let fixture = Fixture::new();
    let tree = tempfile::tempdir().expect("agent tree");
    let bin = tempfile::tempdir().expect("daemon PATH entry");
    let link = bin.path().join("checker");
    std::os::unix::fs::symlink("/bin/echo", &link).expect("PATH symlink");

    let path = std::ffi::OsString::from(bin.path());
    let mut settings = fixture.settings();
    settings.predicate_env = vec![EnvVar::new("PATH", path.clone())];
    settings.predicate_path = Some(path);

    let host = SharedHost::new();
    let daemon = boot(
        &settings,
        seams(
            &settings,
            host,
            Arc::clone(runner),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);

    let agent = common::hire(&running.handle, tree.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["checker", "real-external-predicate"]),
    );
    // `goal.set` refuses a check that resolves into the tree, so the link moves there after the
    // goal is set, and the run's own refusal is what decides.
    if target_inside_tree {
        use std::os::unix::fs::PermissionsExt as _;
        let target = tree.path().join("checker-target");
        std::fs::write(&target, "#!/bin/sh\n").expect("target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .expect("a runnable target");
        std::fs::remove_file(&link).expect("unlink");
        std::os::unix::fs::symlink(&target, &link).expect("PATH symlink into the tree");
    }
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
    fixture.await_record("PredicateResult", |record| {
        matches!(record, Record::PredicateResult { .. }).then_some(())
    });
    running.stop(ExitCause::ShutdownRpc);
    fixture.records()
}

/// A predicate that holds reaches `Done`, and the result that got it there is `Observed`.
#[test]
fn a_command_predicate_that_holds_reaches_done_with_observed_provenance() {
    let (_fixture, records) = run_one_predicate(common::passes());

    let result = records
        .iter()
        .find_map(|record| match record {
            Record::PredicateResult {
                pass,
                provenance,
                evidence,
                ..
            } => Some((*pass, *provenance, evidence.clone())),
            _ => None,
        })
        .expect("a predicate result was journaled");
    assert!(result.0, "the predicate held");
    assert_eq!(
        result.1,
        Provenance::Observed,
        "invariant 3: Done rests on a result the daemon observed"
    );
    match result.2 {
        Evidence::Command { argv, program, .. } => {
            assert_eq!(
                argv,
                vec!["/usr/bin/true".to_owned()],
                "the operator's argv, never rewritten"
            );
            let program = program.expect("the daemon resolved the program");
            assert!(
                std::path::Path::new(&program).is_absolute(),
                "and the resolved program is the canonical one it spawned: {program}"
            );
        }
        other => panic!("a command predicate produces command evidence: {other:?}"),
    }

    assert!(
        reached_done(&records),
        "the loop reached Done: {:?}",
        kinds(&records)
    );
}

/// The control: a predicate that stays false does not reach `Done`.
///
/// It still journals a result — the daemon ran the command and observed that it failed, which is a
/// verdict — and the goal stays open.
#[test]
fn a_command_predicate_that_stays_false_does_not_reach_done() {
    let (_fixture, records) = run_one_predicate(common::fails());

    let (pass, provenance) = records
        .iter()
        .find_map(|record| match record {
            Record::PredicateResult {
                pass, provenance, ..
            } => Some((*pass, *provenance)),
            _ => None,
        })
        .expect("a predicate result was journaled");
    assert!(!pass, "the predicate did not hold");
    assert_eq!(
        provenance,
        Provenance::Observed,
        "a failing run is still something the daemon observed"
    );

    assert!(
        !reached_done(&records),
        "Done is unreachable: {:?}",
        kinds(&records)
    );
}

/// Invariant 3 and daemon-side `PATH` resolution, end to end: resolving a bare program through an external
/// `PATH` entry must still refuse it when the symlink's canonical target is inside the canonical
/// agent tree. Refusal happens before the runner, records an Observed failure with no spawned
/// program, and cannot reach `Done`; the identical out-of-tree shape is the spawn/pass control.
#[cfg(unix)]
#[test]
fn in_tree_resolved_program_cannot_yield_observed_pass() {
    let outside_records = run_symlinked_predicate(
        false,
        &Arc::new(RealRunner(OneShot::with_grace(Duration::from_millis(200)))),
    );
    assert!(
        reached_done(&outside_records),
        "control: the real external child's observed pass may reach Done"
    );
    let output_tail = outside_records
        .iter()
        .find_map(|record| match record {
            Record::PredicateResult {
                evidence: Evidence::Command { output_tail, .. },
                ..
            } => Some(output_tail.as_str()),
            _ => None,
        })
        .expect("the external predicate produced command evidence");
    assert_eq!(
        output_tail.trim(),
        "real-external-predicate",
        "control: the external arm executed a real child rather than accepting a script"
    );

    let inside_runner = Arc::new(ScriptedRunner::scripted([common::passes()]));
    let inside_records = run_symlinked_predicate(true, &inside_runner);
    assert_eq!(
        inside_runner.runs(),
        0,
        "the refusal happens before any runner call"
    );
    let (pass, provenance, evidence) = inside_records
        .iter()
        .find_map(|record| match record {
            Record::PredicateResult {
                pass,
                provenance,
                evidence,
                ..
            } => Some((*pass, *provenance, evidence)),
            _ => None,
        })
        .expect("the refusal is journaled as a failing observation");
    assert!(!pass, "a refused program never produces an observed pass");
    assert_eq!(provenance, Provenance::Observed);
    match evidence {
        Evidence::Command {
            program,
            output_tail,
            ..
        } => {
            assert_eq!(
                program, &None,
                "no program is recorded as spawned when nothing ran"
            );
            assert!(
                output_tail.contains("is inside the agent tree"),
                "the run refused the in-tree program: {output_tail}"
            );
        }
        other => panic!("a command refusal produces command evidence: {other:?}"),
    }
    assert!(
        !reached_done(&inside_records),
        "invariant 3: Done is unreachable after the refusal"
    );
}

/// A predicate the exit path swept journals **nothing**.
///
/// Answering `pass: false` for a run the daemon interrupted would journal a fabricated
/// `Provenance::Observed` verdict — `on_predicate_done` stamps it unconditionally — which invariant
/// 3 forbids. The goal stays un-judged and `Done` unreachable until the agent's next turn ends and
/// the predicate runs again. Replay does not re-run a swept predicate.
///
/// The control is the two tests above: a run that *finished* does journal a result, so this is not
/// green because nothing is ever journaled.
#[test]
fn a_swept_predicate_journals_no_result() {
    let (_fixture, records) = run_one_predicate(duded::predicate::PredicateOutcome::Swept);

    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::PredicateResult { .. })),
        "a swept run has no evidence, so nothing can be journaled: {:?}",
        kinds(&records)
    );
    assert!(!reached_done(&records), "and the goal stays un-judged");
}

/// Whether any decision in the journal was `Done`.
///
/// `Done` is a *decision*, not a record of its own: `LoopDecision::Done` is what the done gate produces
/// once the goal's predicate has passed with `Observed` provenance, and it is journaled inside the
/// `LoopDecided` that carries the gate trace.
fn reached_done(records: &[Record]) -> bool {
    records.iter().any(|record| {
        matches!(
            record,
            Record::LoopDecided {
                decision: LoopDecision::Done,
                ..
            }
        )
    })
}

/// Record kinds, for a failure message.
fn kinds(records: &[Record]) -> Vec<dudes_proto::RecordKind> {
    records.iter().map(Record::kind).collect()
}

/// The goal's latest result as the projection holds it after the daemon stopped.
fn last_result(fixture: &Fixture) -> dudes_proto::PredicateOutcome {
    let projections = dudes_journal::Projections::open(
        &fixture
            .settings()
            .state_dir
            .join(duded::boot::PROJECTION_FILE),
    )
    .expect("the projection store");
    let goals = projections.goals().expect("the goals table");
    let [goal] = goals.as_slice() else {
        panic!("one goal: {goals:?}");
    };
    goal.last_result.expect("a result")
}

/// A check whose program left the daemon's `PATH` after the goal was set never starts. Its result
/// says so in the projection and in the done gate's note, while a check that timed out after a turn ran
/// and stays an ordinary failure. A fixture's pass with no program is covered by
/// `Evidence::not_run`'s own table in `dudes-proto`, and the upgrade by
/// `a_version_39_store_rebuilds_a_result_whose_check_did_not_run` in `dudes-journal`.
///
/// Mutants: deriving `not_run` from `exit: None` alone turns the timeout arm true; dropping the
/// fold's `not_run` leaves the first arm false; dropping the done gate's arm prints "predicate observed
/// and failed".
#[test]
fn a_check_that_cannot_run_reads_not_run() {
    let fixture = Fixture::new();
    let bin = tempfile::tempdir().expect("daemon PATH entry");
    std::os::unix::fs::symlink("/usr/bin/false", bin.path().join("checker")).expect("checker");
    let path = std::ffi::OsString::from(bin.path());
    let mut settings = fixture.settings();
    settings.predicate_env = vec![EnvVar::new("PATH", path.clone())];
    settings.predicate_path = Some(path);
    let runner = Arc::new(ScriptedRunner::default());
    let daemon = boot(
        &settings,
        seams(
            &settings,
            SharedHost::new(),
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let mut running = common::start(daemon);
    let agent = common::hire(&running.handle, fixture.state.path());
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["checker", "--run"]),
    );
    let turn = fixture.await_turn();
    std::fs::remove_file(bin.path().join("checker")).expect("the program leaves the PATH");
    common::end_turn(&running.handle, turn);
    let evidence = fixture.await_record("PredicateResult", |record| match record {
        Record::PredicateResult { evidence, pass, .. } => Some((evidence.clone(), *pass)),
        _ => None,
    });
    let note = fixture.await_record("the decision after the result", |record| match record {
        Record::LoopDecided { gate_trace, .. } => gate_trace
            .iter()
            .find(|step| step.gate == dudes_proto::Gate::Done)
            .and_then(|step| step.note.clone()),
        _ => None,
    });
    running.stop(ExitCause::ShutdownRpc);
    assert_eq!(runner.runs(), 0, "nothing was spawned");
    let (
        Evidence::Command {
            program: None,
            exit: None,
            ..
        },
        false,
    ) = evidence
    else {
        panic!("a refused run: {evidence:?}");
    };
    assert_eq!(note, "the check could not run, so nothing was judged");
    let result = last_result(&fixture);
    assert!(result.not_run && !result.pass, "{result:?}");

    let (timed_out, _) = run_one_predicate(PredicateOutcome::Finished {
        exit: None,
        output_tail: String::new(),
        timed_out: true,
    });
    let result = last_result(&timed_out);
    assert!(!result.not_run && !result.pass, "{result:?}");
}

/// The real runner, counting every run it is handed, so a test can say no check child started.
struct CountingRunner {
    inner: RealRunner,
    runs: std::sync::atomic::AtomicUsize,
    ended: std::sync::atomic::AtomicUsize,
}

impl CountingRunner {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RealRunner(OneShot::with_grace(Duration::from_millis(200))),
            runs: std::sync::atomic::AtomicUsize::new(0),
            ended: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn ended(&self) -> usize {
        self.ended.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn runs(&self) -> usize {
        self.runs.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl PredicateRunner for CountingRunner {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<PredicateOutcome> {
        self.run_cancellable(spec, timeout, &std::sync::atomic::AtomicBool::new(false))
    }

    fn run_cancellable(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> io::Result<PredicateOutcome> {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let outcome = self.inner.run_cancellable(spec, timeout, cancel);
        self.ended.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        outcome
    }

    fn sweep(&self) {
        self.inner.sweep();
    }
}

/// A daemon on the counting real runner, with one agent hired into its own new tree.
struct Baselined {
    fixture: Fixture,
    tree: tempfile::TempDir,
    runner: Arc<CountingRunner>,
    host: SharedHost,
    running: common::Running,
    agent: dudes_types::AgentId,
    append_failure: duded::runtime::AppendFailure,
    pause: duded::runtime::OnboardingPause,
}

fn baselined() -> Baselined {
    baselined_with(|_| {}, Vec::new())
}

/// [`baselined`] over `settings` as `edit` leaves them, with `seeded` journaled before boot.
fn baselined_with(edit: impl FnOnce(&mut duded::Settings), seeded: Vec<Record>) -> Baselined {
    let fixture = Fixture::new();
    if !seeded.is_empty() {
        let (mut writer, _) = dudes_journal::open(
            &fixture.journal_dir(),
            dudes_journal::JournalOptions {
                checkpoint_every: 0,
                ..Default::default()
            },
        )
        .unwrap();
        for record in seeded {
            writer
                .append(&dudes_proto::Entry {
                    at: dudes_types::Timestamp::from_unix_millis(1_787_394_800_000),
                    record,
                })
                .unwrap();
        }
    }
    let mut settings = fixture.settings();
    edit(&mut settings);
    let tree = tempfile::tempdir().expect("agent tree");
    let runner = CountingRunner::new();
    let host = SharedHost::with_native_evidence(fixture.journal_dir());
    let seams = seams(
        &settings,
        host.clone(),
        Arc::clone(&runner),
        Arc::new(TestClock::new()),
        Witness::default(),
    );
    host.bind_child_ends(Arc::clone(&seams.children));
    let daemon = boot(&settings, seams).expect("boot");
    let append_failure = daemon.append_failure();
    let pause = daemon.onboarding_pause();
    let running = common::start(daemon);
    let agent = common::hire(&running.handle, tree.path());
    Baselined {
        fixture,
        tree,
        runner,
        host,
        running,
        agent,
        append_failure,
        pause,
    }
}

/// `goal.set` params whose check runs before the first turn: `argv`, a two-turn hop cap.
fn checked_goal(
    agent: dudes_types::AgentId,
    argv: &[&str],
) -> dudes_proto::rpc::goal::GoalSetParams {
    dudes_proto::rpc::goal::GoalSetParams {
        hop_cap: 2,
        may_already_pass: false,
        ..common::goal_params(agent, argv)
    }
}

impl Baselined {
    fn set(
        &self,
        params: dudes_proto::rpc::goal::GoalSetParams,
    ) -> Result<dudes_proto::rpc::goal::GoalSetResult, dudes_proto::rpc::RpcError> {
        self.running
            .handle
            .set_goal(params, &dudes_rpc::HangUp::never())
    }

    fn goal_sets(&self) -> Vec<dudes_proto::Goal> {
        self.fixture
            .records()
            .into_iter()
            .filter_map(|record| match record {
                Record::GoalSet { goal, .. } => Some(goal),
                _ => None,
            })
            .collect()
    }

    /// `GoalSet`, `LoopDecided` and `TurnIssued` records, which a refused goal set appends none of.
    fn effects(&self) -> usize {
        self.fixture
            .records()
            .iter()
            .filter(|record| {
                matches!(
                    record,
                    Record::GoalSet { .. } | Record::LoopDecided { .. } | Record::TurnIssued { .. }
                )
            })
            .count()
    }
}

/// The gate a refusal names, when it names one.
fn gate(error: &dudes_proto::rpc::RpcError) -> Option<dudes_proto::rpc::RefusalGate> {
    serde_json::from_value::<dudes_proto::rpc::RefusedData>(error.data.clone()?)
        .ok()
        .map(|data| data.gate)
}

/// The canonical `test` program the daemon's `PATH` resolves to.
fn test_program() -> String {
    ["/usr/bin/test", "/bin/test"]
        .into_iter()
        .find(|path| std::path::Path::new(path).exists())
        .and_then(|path| std::fs::canonicalize(path).ok())
        .and_then(|path| path.to_str().map(ToOwned::to_owned))
        .expect("a test program")
}

/// A check that passes before any work is refused, and nothing is appended. Were it accepted, the
/// goal would be recorded and go `Done` after one turn.
///
/// Mutation: mapping a pass to `CommandFailed` lets a `GoalSet` land.
#[test]
fn goal_set_refuses_a_check_that_already_passes() {
    let daemon = baselined();
    std::fs::write(daemon.tree.path().join("hello.txt"), "hello\n").unwrap();
    let error = daemon
        .set(checked_goal(daemon.agent, &["test", "-f", "hello.txt"]))
        .expect_err("a check that already passes is refused");
    assert_eq!(error.code, dudes_proto::rpc::ErrorCode::Refused.code());
    assert_eq!(
        gate(&error),
        Some(dudes_proto::rpc::RefusalGate::CheckAlreadyPasses)
    );
    assert_eq!(
        error.message,
        format!(
            "the completion check already passes (`test -f hello.txt` exited 0 in {}), so it \
             cannot show when this goal is done",
            daemon.tree.path().display()
        )
    );
    assert_eq!(daemon.runner.runs(), 1, "the check ran once");
    assert_eq!(daemon.effects(), 0, "no GoalSet, LoopDecided or TurnIssued");
}

/// A check that fails before work is admitted with what the run saw, and a later pass reads as a
/// change. A stand-in that changes nothing never reaches `Done`.
///
/// Mutation: recording `MayAlreadyPass` for a run baseline changes the kind and the note.
#[test]
fn goal_set_admits_a_failing_check_and_records_its_baseline() {
    let mut daemon = baselined();
    let set = daemon
        .set(checked_goal(
            daemon.agent,
            &["test", "-f", "CONTRIBUTING.md"],
        ))
        .expect("a failing check is admitted");
    let goals = daemon.goal_sets();
    let [goal] = goals.as_slice() else {
        panic!("one goal: {goals:?}");
    };
    assert_eq!(goal.id, set.goal);
    assert_eq!(
        goal.baseline,
        Some(dudes_proto::CheckBaseline::CommandFailed {
            program: test_program(),
            exit: 1
        })
    );
    let turn = daemon.fixture.await_turn();
    std::fs::write(
        daemon.tree.path().join("CONTRIBUTING.md"),
        "# Contributing\n",
    )
    .unwrap();
    common::end_turn_with_native(&daemon.fixture, &daemon.running.handle, &daemon.host, turn);
    let note = daemon
        .fixture
        .await_record("the Done decision", |record| match record {
            Record::LoopDecided {
                decision: LoopDecision::Done,
                gate_trace,
                ..
            } => gate_trace.last().and_then(|step| step.note.clone()),
            _ => None,
        });
    assert_eq!(
        note,
        "predicate observed and passed; it failed before the first turn"
    );
    daemon.running.stop(ExitCause::ShutdownRpc);

    let mut idle = baselined();
    idle.set(checked_goal(idle.agent, &["test", "-f", "CONTRIBUTING.md"]))
        .expect("admitted");
    let mut ended = Vec::new();
    for _ in 0..2 {
        let turn = idle.fixture.await_record("a turn", |record| match record {
            Record::TurnIssued { turn, .. } if !ended.contains(turn) => Some(*turn),
            _ => None,
        });
        ended.push(turn);
        common::end_turn_with_native(&idle.fixture, &idle.running.handle, &idle.host, turn);
        idle.fixture.await_record("its result", |record| {
            matches!(record, Record::PredicateResult { turn: Some(t), .. } if *t == turn)
                .then_some(())
        });
        if idle.fixture.records().iter().any(|record| {
            matches!(
                record,
                Record::LoopDecided {
                    decision: LoopDecision::Exhausted { .. },
                    ..
                }
            )
        }) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    idle.fixture
        .await_record("Exhausted", |record| match record {
            Record::LoopDecided {
                decision: LoopDecision::Exhausted { reason },
                ..
            } => Some(*reason),
            _ => None,
        });
    assert!(
        !reached_done(&idle.fixture.records()),
        "a stand-in that changes nothing never reaches Done"
    );
    idle.running.stop(ExitCause::ShutdownRpc);
}

/// A check that timed out, and one a signal ended, shows nothing about the state before work.
///
/// Mutation: treating `timed_out` as a non-zero exit lets a `GoalSet` land.
#[test]
fn goal_set_refuses_a_check_that_did_not_finish() {
    let daemon = baselined();
    let timeout = dudes_proto::rpc::goal::GoalSetParams {
        predicate: dudes_proto::goal::Predicate::Command {
            argv: vec!["sleep".into(), "5".into()],
            timeout_s: 1,
        },
        ..checked_goal(daemon.agent, &["sleep"])
    };
    let error = daemon
        .set(timeout)
        .expect_err("a timed-out check is refused");
    assert_eq!(
        gate(&error),
        Some(dudes_proto::rpc::RefusalGate::CheckDidNotFinish)
    );
    assert_eq!(
        error.message,
        "the completion check did not finish (it timed out after 1 s, or a signal ended it), so it \
         cannot show whether the check already passes"
    );
    let error = daemon
        .set(checked_goal(daemon.agent, &["sh", "-c", "kill -9 $$"]))
        .expect_err("a check a signal ended is refused");
    assert_eq!(
        gate(&error),
        Some(dudes_proto::rpc::RefusalGate::CheckDidNotFinish)
    );
    assert_eq!(daemon.effects(), 0);
}

/// A check the operator says may already pass is not run before the first turn, its goal records
/// that, its name takes the second revision, and a pass after one turn is `Done`.
///
/// Mutation: running the baseline anyway makes the count 1.
#[test]
fn goal_set_may_already_pass_skips_the_run() {
    let daemon = baselined();
    std::fs::write(daemon.tree.path().join("hello.txt"), "hello\n").unwrap();
    let mut params = checked_goal(daemon.agent, &["test", "-f", "hello.txt"]);
    params.may_already_pass = true;
    params.check = Some(
        dudes_proto::rpc::goal::CompletionCheckDeclaration::new("hello", "hello.txt exists")
            .unwrap(),
    );
    daemon.set(params).expect("admitted without a run");
    assert_eq!(
        daemon.runner.runs(),
        0,
        "no check ran before the first turn"
    );
    let goals = daemon.goal_sets();
    let [goal] = goals.as_slice() else {
        panic!("one goal: {goals:?}");
    };
    assert_eq!(
        goal.baseline,
        Some(dudes_proto::CheckBaseline::MayAlreadyPass)
    );
    assert_eq!(
        serde_json::to_value(goal.completion_check.as_ref().unwrap().revision).unwrap(),
        common::check_revision_as(
            "hello",
            "hello.txt exists",
            &["test", "-f", "hello.txt"],
            600,
            true
        )
    );
    let turn = daemon.fixture.await_turn();
    common::end_turn_with_native(&daemon.fixture, &daemon.running.handle, &daemon.host, turn);
    let note = daemon.fixture.await_record("Done", |record| match record {
        Record::LoopDecided {
            decision: LoopDecision::Done,
            gate_trace,
            ..
        } => gate_trace.last().and_then(|step| step.note.clone()),
        _ => None,
    });
    assert_eq!(
        note,
        "predicate observed and passed; the operator said it may already have passed before the \
         first turn"
    );
}

/// A check that cannot run is refused with nothing appended: an unresolved program and one inside
/// the tree by the goal set's own resolution refusal, and a board card that does not exist by the
/// baseline.
///
/// Mutation: mapping the missing card to no verdict answers unavailable.
#[test]
fn goal_set_refuses_a_check_that_cannot_run() {
    let daemon = baselined();
    let error = daemon
        .set(checked_goal(daemon.agent, &["no-such-check-program"]))
        .expect_err("an unresolved program is refused");
    assert_eq!(
        error.code,
        dudes_proto::rpc::ErrorCode::InvalidParams.code(),
        "{error:?}"
    );
    std::fs::write(daemon.tree.path().join("checker"), "#!/bin/sh\nexit 1\n").unwrap();
    let error = daemon
        .set(checked_goal(daemon.agent, &["./checker"]))
        .expect_err("a program inside the tree is refused");
    assert_eq!(
        error.code,
        dudes_proto::rpc::ErrorCode::InvalidParams.code(),
        "{error:?}"
    );
    let card = dudes_types::CardId::from_parts(9, 9);
    let error = daemon
        .set(dudes_proto::rpc::goal::GoalSetParams {
            predicate: dudes_proto::goal::Predicate::BoardState {
                card,
                status: dudes_types::CardStatus::Done,
            },
            ..checked_goal(daemon.agent, &["unused"])
        })
        .expect_err("a missing card is refused");
    assert_eq!(
        gate(&error),
        Some(dudes_proto::rpc::RefusalGate::CheckCannotRun)
    );
    assert_eq!(
        error.message,
        format!("the completion check could not run, because the board card {card} does not exist")
    );
    assert_eq!(daemon.runner.runs(), 0);
    assert_eq!(daemon.effects(), 0);
}

/// The check runs in the agent's recorded directory, not the daemon's.
///
/// Mutation: running the baseline in the daemon's working directory admits the first arm.
#[test]
fn baseline_runs_in_the_goal_directory() {
    let daemon = baselined();
    std::fs::write(daemon.tree.path().join("marker"), "here\n").unwrap();
    let error = daemon
        .set(checked_goal(daemon.agent, &["test", "-f", "marker"]))
        .expect_err("the marker is in the agent's directory");
    assert_eq!(
        gate(&error),
        Some(dudes_proto::rpc::RefusalGate::CheckAlreadyPasses)
    );
    let elsewhere = baselined();
    std::fs::write(daemon.tree.path().join("only-there"), "here\n").unwrap();
    elsewhere
        .set(checked_goal(elsewhere.agent, &["test", "-f", "only-there"]))
        .expect("the marker is in another agent's directory, not this one's");
}

/// A board check whose card the operator moved to its status is refused, and one an agent moved
/// there is admitted, because an agent's status cannot complete a goal.
///
/// Mutation: dropping the board's author rule from the baseline refuses the agent-moved arm.
#[test]
fn goal_set_refuses_a_board_check_already_at_its_status() {
    let (by_operator, by_agent) = (
        dudes_types::CardId::from_parts(5, 1),
        dudes_types::CardId::from_parts(5, 2),
    );
    let operator = dudes_proto::Actor::Peer {
        pid: 77,
        uid: Some(501),
    };
    let mut seeded = Vec::new();
    for (card, mover) in [
        (by_operator, operator),
        (
            by_agent,
            dudes_proto::Actor::Agent {
                agent: dudes_types::AgentId::from_parts(5, 3),
            },
        ),
    ] {
        seeded.push(Record::CardCreated {
            card,
            title: "Ship".into(),
            department: "engineering".into(),
            by: operator,
            deps: Vec::new(),
        });
        seeded.push(Record::CardTransition {
            card,
            from: dudes_types::CardStatus::Todo,
            to: dudes_types::CardStatus::Done,
            by: mover,
            expected_seq: dudes_types::Seq(0),
            reason: None,
        });
    }
    let daemon = baselined_with(|_| {}, seeded);
    let board = |card| dudes_proto::rpc::goal::GoalSetParams {
        predicate: dudes_proto::goal::Predicate::BoardState {
            card,
            status: dudes_types::CardStatus::Done,
        },
        ..checked_goal(daemon.agent, &["unused"])
    };
    let error = daemon
        .set(board(by_operator))
        .expect_err("the operator already moved the card to Done");
    assert_eq!(
        gate(&error),
        Some(dudes_proto::rpc::RefusalGate::CheckAlreadyPasses)
    );
    assert_eq!(daemon.effects(), 0);
    daemon
        .set(board(by_agent))
        .expect("an agent's move does not pass the check");
    let goals = daemon.goal_sets();
    assert_eq!(
        goals.last().and_then(|goal| goal.baseline.clone()),
        Some(dudes_proto::CheckBaseline::BoardNotMet)
    );
    assert_eq!(daemon.runner.runs(), 0, "a board check is read, not run");
}

/// While the agent has a turn open, or its previous goal's check is running, a goal set whose
/// check would run there is refused and starts no child. One whose check may already pass replaces
/// the goal as before.
///
/// Mutation: dropping the guard starts a check child during the turn.
#[test]
fn goal_set_refuses_a_baseline_beside_live_work() {
    let daemon = baselined();
    let held = daemon.tree.path().join("release");
    let first = dudes_proto::rpc::goal::GoalSetParams {
        hop_cap: 3,
        ..common::goal_params(
            daemon.agent,
            &[
                "sh",
                "-c",
                "while [ ! -f release ]; do sleep 0.05; done; exit 1",
            ],
        )
    };
    daemon
        .set(first)
        .expect("a goal whose check may already pass");
    let turn = daemon.fixture.await_turn();
    let error = daemon
        .set(checked_goal(daemon.agent, &["test", "-f", "absent"]))
        .expect_err("a turn is open");
    assert_eq!(gate(&error), Some(dudes_proto::rpc::RefusalGate::OpenTurn));
    assert_eq!(
        error.message,
        "the agent has a turn or a check running in its directory, so the completion check cannot \
         run there now"
    );
    assert_eq!(daemon.runner.runs(), 0, "no check child started");

    common::end_turn_with_native(&daemon.fixture, &daemon.running.handle, &daemon.host, turn);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while daemon.runner.runs() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the post-turn check never started"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let error = daemon
        .set(checked_goal(daemon.agent, &["test", "-f", "absent"]))
        .expect_err("the previous goal's check is running");
    assert_eq!(gate(&error), Some(dudes_proto::rpc::RefusalGate::OpenTurn));
    assert_eq!(daemon.runner.runs(), 1, "only the post-turn check ran");
    let mut kept = checked_goal(daemon.agent, &["test", "-f", "absent"]);
    kept.may_already_pass = true;
    daemon
        .set(kept)
        .expect("a check that may already pass starts no run, so it replaces the goal");
    std::fs::write(held, "go\n").unwrap();
    assert_eq!(daemon.runner.runs(), 1);
}

/// An unknown or retired agent is refused before any check runs, and so is one whose directory
/// was deleted, by the goal set's own resolution of the check.
///
/// Mutation: dropping the agent check journals a `GoalSet` for an unknown agent.
#[test]
fn goal_set_refuses_an_absent_agent_and_a_missing_directory() {
    let daemon = baselined();
    let unknown = dudes_types::AgentId::from_parts(4, 4);
    let error = daemon
        .set(checked_goal(unknown, &["test", "-f", "absent"]))
        .expect_err("an unknown agent");
    assert_eq!(error.code, dudes_proto::rpc::ErrorCode::NotFound.code());
    assert_eq!(error.message, format!("there is no live agent {unknown}"));
    match daemon
        .running
        .handle
        .ask(duded::ports::Ask::Retire(Box::new(
            dudes_proto::rpc::agent::RetireParams {
                agent: daemon.agent,
                reason: "done".into(),
            },
        )))
        .expect("retired")
    {
        duded::ports::Answered::Done => {}
        other => panic!("retire answered {other:?}"),
    }
    let error = daemon
        .set(checked_goal(daemon.agent, &["test", "-f", "absent"]))
        .expect_err("a retired agent");
    assert_eq!(error.code, dudes_proto::rpc::ErrorCode::NotFound.code());
    assert_eq!(daemon.runner.runs(), 0);
    assert_eq!(daemon.effects(), 0);

    let gone = baselined();
    let directory = gone.tree.path().to_path_buf();
    std::fs::remove_dir_all(&directory).unwrap();
    let error = gone
        .set(checked_goal(gone.agent, &["test", "-f", "absent"]))
        .expect_err("the directory is gone");
    assert!(
        error.message.contains(&directory.display().to_string()),
        "the refusal names the directory: {error:?}"
    );
    assert_eq!(gone.runner.runs(), 0);
    assert_eq!(gone.effects(), 0);
}

/// Today's guards run before the check: a Claimed board check, and a goal that needs a ceiling it
/// lacks, are refused with no check child started.
///
/// Mutation: starting the baseline before the guards makes the count 1.
#[test]
fn goal_set_guards_run_before_the_baseline() {
    let fixture = Fixture::new();
    let tree = tempfile::tempdir().expect("agent tree");
    let boot_with = |env: Vec<EnvVar>, runner: Arc<CountingRunner>| {
        let mut settings = fixture.settings();
        settings.daemon_env = env;
        settings.floor_usd_per_day = None;
        common::start(
            boot(
                &settings,
                seams(
                    &settings,
                    SharedHost::new(),
                    runner,
                    Arc::new(TestClock::new()),
                    Witness::default(),
                ),
            )
            .expect("boot"),
        )
    };
    let mut unbilled = boot_with(Vec::new(), CountingRunner::new());
    let hire = dudes_proto::rpc::agent::HireParams {
        budget: dudes_proto::AgentBudget {
            usd_per_day: None,
            tokens_per_day: None,
        },
        ..common::hire_params(tree.path())
    };
    let agent = match unbilled
        .handle
        .ask(duded::ports::Ask::Hire(Box::new(hire)))
        .expect("hired without a ceiling")
    {
        duded::ports::Answered::Hired(result) => result.agent,
        other => panic!("agent.hire answered {other:?}"),
    };
    unbilled.stop(ExitCause::ShutdownRpc);
    let runner = CountingRunner::new();
    let running = boot_with(
        vec![EnvVar::new("ANTHROPIC_API_KEY", "sk-fixture")],
        Arc::clone(&runner),
    );
    let set = |params| running.handle.set_goal(params, &dudes_rpc::HangUp::never());
    let before = fixture.records().len();
    let claimed = dudes_proto::rpc::goal::GoalSetParams {
        predicate: dudes_proto::goal::Predicate::BoardState {
            card: dudes_types::CardId::from_parts(6, 1),
            status: dudes_types::CardStatus::Claimed,
        },
        ..checked_goal(agent, &["unused"])
    };
    let error = set(claimed).expect_err("Claimed is no completion");
    assert_eq!(
        error.code,
        dudes_proto::rpc::ErrorCode::InvalidParams.code()
    );
    let mut uncapped = checked_goal(agent, &["test", "-f", "absent"]);
    uncapped.budget = dudes_proto::GoalBudget {
        usd: None,
        tokens: None,
    };
    let error = set(uncapped).expect_err("a billable goal needs a ceiling");
    assert_eq!(
        gate(&error),
        Some(dudes_proto::rpc::RefusalGate::CeilingRequired)
    );
    assert_eq!(runner.runs(), 0, "no check child started");
    assert!(
        !fixture.records()[before..]
            .iter()
            .any(|record| matches!(record, Record::GoalSet { .. })),
        "nothing was appended"
    );
}

/// The operator credential the daemon minted in its runtime directory.
fn operator_credential(settings: &duded::Settings) -> dudes_proto::rpc::cred::OperatorCredential {
    dudes_proto::rpc::cred::OperatorCredential::new(
        std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .expect("the daemon minted a credential"),
    )
    .expect("a well-formed credential")
}

/// A `goal.set` frame sent on a socket the test keeps a handle to, so it can hang up.
fn send_goal_set(
    settings: &duded::Settings,
    params: &dudes_proto::rpc::goal::GoalSetParams,
) -> std::os::unix::net::UnixStream {
    let stream = std::os::unix::net::UnixStream::connect(settings.socket()).expect("connect");
    let mut client = dudes_rpc::Client::new(
        std::io::BufReader::new(stream.try_clone().unwrap()),
        stream.try_clone().unwrap(),
    );
    client
        .send_presenting(
            dudes_proto::rpc::Method::GoalSet,
            Some(serde_json::to_value(params).unwrap()),
            Some(operator_credential(settings)),
        )
        .expect("the frame is written");
    stream
}

/// A check that waits for `release` in the agent's tree, then fails.
fn held_check(agent: dudes_types::AgentId) -> dudes_proto::rpc::goal::GoalSetParams {
    checked_goal(
        agent,
        &[
            "sh",
            "-c",
            "while [ ! -f release ]; do sleep 0.02; done; exit 1",
        ],
    )
}

fn wait_for(mut ready: impl FnMut() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(std::time::Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A daemon that stops while a local goal set's check runs answers that caller unavailable, writes
/// a clean receipt, and journals no goal, also when the check ends while the daemon stops.
///
/// Mutation: waiting on the check without regard to the stop holds the connection join past its
/// bound, and the receipt reports the control listener.
#[test]
fn baseline_at_daemon_stop_appends_nothing_and_tears_down_cleanly() {
    for ends_during_exit in [false, true] {
        let mut daemon = baselined();
        let settings = daemon.fixture.settings();
        let stream = send_goal_set(&settings, &held_check(daemon.agent));
        wait_for(|| daemon.runner.runs() == 1, "the check started");
        let release = daemon.tree.path().join("release");
        let releaser = std::thread::spawn(move || {
            if ends_during_exit {
                std::thread::sleep(Duration::from_millis(30));
                std::fs::write(release, "go\n").unwrap();
            }
        });
        daemon.running.stop(ExitCause::ShutdownRpc);
        releaser.join().unwrap();
        let mut answer = String::new();
        let _ = std::io::Read::read_to_string(&mut &stream, &mut answer);
        assert!(
            answer.is_empty() || answer.contains("shutting down"),
            "unavailable or a closed connection: {answer}"
        );
        let receipts = receipts(&settings);
        assert!(
            !receipts.is_empty() && receipts.iter().all(|text| text.starts_with("complete")),
            "a clean teardown: {receipts:?}"
        );
        assert!(
            !daemon
                .fixture
                .records()
                .iter()
                .any(|record| matches!(record, Record::GoalSet { .. })),
            "no goal was journaled"
        );
    }
    recheck_behind_the_stop();
}

/// Every receipt the daemon wrote under `settings`, which a clean teardown writes as `complete`.
fn receipts(settings: &duded::Settings) -> Vec<String> {
    std::fs::read_dir(&settings.runtime_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".receipt"))
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
        .collect()
}

/// The drain arm: a goal set whose check ends after the stop was queued, while another goal
/// set's append job holds the loop, has its own append job answered by the exit path's drain. It
/// is refused there, the teardown stays clean, and no goal is journaled for it.
///
/// Mutation: answering that job with an internal error makes the receipt report the drain.
fn recheck_behind_the_stop() {
    let mut daemon = baselined();
    let settings = daemon.fixture.settings();
    let other = tempfile::tempdir().expect("a second tree");
    let holder = common::hire(&daemon.running.handle, other.path());
    let _held = send_goal_set(&settings, &held_check(daemon.agent));
    wait_for(|| daemon.runner.runs() == 1, "the held check started");
    daemon
        .pause
        .arm(duded::runtime::OnboardingPausePoint::BeforeBaselineRecheck);
    let _holding = send_goal_set(&settings, &checked_goal(holder, &["test", "-f", "absent"]));
    assert!(
        daemon.pause.wait_reached(Duration::from_secs(10)),
        "the second goal set's append job holds the loop"
    );
    daemon.running.handle.stop(ExitCause::ShutdownRpc);
    std::fs::write(daemon.tree.path().join("release"), "go\n").unwrap();
    wait_for(|| daemon.runner.ended() == 2, "the held check ended");
    std::thread::sleep(Duration::from_millis(300));
    daemon.pause.release();
    daemon.running.join();
    let receipts = receipts(&settings);
    assert!(
        !receipts.is_empty() && receipts.iter().all(|text| text.starts_with("complete")),
        "a clean teardown: {receipts:?}"
    );
    assert!(
        !daemon.fixture.records().iter().any(|record| matches!(
            record,
            Record::GoalSet { agent, .. } if *agent == daemon.agent
        )),
        "no goal was journaled for the check that ended after the stop"
    );
}

/// A caller that closes its socket, or shuts down its write side, while its goal set's check runs
/// or after the run and before the append, gets no goal. A hang-up during the run ends the check's
/// child at once, so the agent's next goal set is not refused beside it. A replacement keeps
/// running after a hang-up, and its same-request retry returns its result.
///
/// Mutations: skipping the probe in the append job journals a goal in the third arm; waiting on
/// the run without reading the probe leaves the held check running.
#[test]
fn goal_set_is_abandoned_when_the_client_disconnects() {
    for arm in ["closed", "half-closed"] {
        let daemon = baselined();
        let settings = daemon.fixture.settings();
        let stream = send_goal_set(&settings, &held_check(daemon.agent));
        wait_for(|| daemon.runner.runs() == 1, "the check started");
        if arm == "closed" {
            stream.shutdown(std::net::Shutdown::Both).unwrap();
            drop(stream);
        } else {
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        }
        // Never released: the hang-up itself ends the check.
        wait_for(|| daemon.runner.ended() == 1, "the hang-up ended the check");
        std::thread::sleep(Duration::from_millis(300));
        let control = daemon
            .set(common::goal_params(daemon.agent, &["test", "-f", "absent"]))
            .expect("a later goal set is admitted");
        let goals = daemon.goal_sets();
        assert_eq!(
            goals.iter().map(|goal| goal.id).collect::<Vec<_>>(),
            [control.goal],
            "{arm}: only the later goal was journaled"
        );
    }

    let fixture = Fixture::new();
    let settings = fixture.settings();
    let tree = tempfile::tempdir().expect("agent tree");
    let daemon = boot(
        &fixture.settings(),
        seams(
            &settings,
            SharedHost::new(),
            CountingRunner::new(),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let pause = daemon.onboarding_pause();
    let running = common::start(daemon);
    let agent = common::hire(&running.handle, tree.path());
    pause.arm(duded::runtime::OnboardingPausePoint::BeforeBaselineRecheck);
    let stream = send_goal_set(
        &fixture.settings(),
        &checked_goal(agent, &["test", "-f", "absent"]),
    );
    assert!(
        pause.wait_reached(Duration::from_secs(10)),
        "the append job was reached"
    );
    stream.shutdown(std::net::Shutdown::Both).unwrap();
    drop(stream);
    std::thread::sleep(Duration::from_millis(50));
    pause.release();
    let control = running
        .handle
        .set_goal(
            common::goal_params(agent, &["test", "-f", "absent"]),
            &dudes_rpc::HangUp::never(),
        )
        .expect("a later goal set is admitted");
    let goals: Vec<_> = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::GoalSet { goal, .. } => Some(goal.id),
            _ => None,
        })
        .collect();
    assert_eq!(goals, [control.goal], "after the run: only the later goal");
}

/// A paused agent with a goal whose check may already pass, so a replacement can be admitted.
fn paused_with_goal(daemon: &Baselined) -> dudes_types::GoalId {
    match daemon
        .running
        .handle
        .ask(duded::ports::Ask::Pause(daemon.agent))
        .expect("paused")
    {
        duded::ports::Answered::Done => {}
        other => panic!("loop.pause answered {other:?}"),
    }
    daemon
        .set(common::goal_params(daemon.agent, &["test", "-f", "absent"]))
        .expect("a goal")
        .goal
}

/// A replacement of `goal` whose check waits for `release` in the agent's tree, then fails.
fn held_replacement(
    agent: dudes_types::AgentId,
    goal: dudes_types::GoalId,
    id: u8,
) -> dudes_proto::rpc::goal::GoalReplaceParams {
    dudes_proto::rpc::goal::GoalReplaceParams {
        request_id: dudes_proto::record::Hex32::new([id; 32]),
        agent,
        expected_goal: goal,
        text: "replacement".into(),
        predicate: dudes_proto::goal::Predicate::Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "while [ ! -f release ]; do sleep 0.02; done; exit 1".into(),
            ],
            timeout_s: 60,
        },
        check: dudes_proto::rpc::goal::CompletionCheckDeclaration::new("held", "Held check")
            .unwrap(),
        may_already_pass: false,
    }
}

fn replace_on(
    settings: &duded::Settings,
    request: &dudes_proto::rpc::goal::GoalReplaceParams,
) -> dudes_proto::rpc::goal::GoalReplaceResult {
    let response = dudes_rpc::Client::connect(settings.socket())
        .unwrap()
        .call_as_operator(
            dudes_proto::rpc::Method::GoalReplace,
            Some(serde_json::to_value(request).unwrap()),
            operator_credential(settings),
        )
        .unwrap();
    serde_json::from_value(response.result.expect("a replacement result")).unwrap()
}

/// While a replacement's check runs, a newer goal, a retirement or a halt lands first, and the
/// replacement's admission, run again before its append, refuses it.
///
/// Mutation: skipping the recheck appends the replacement over the newer state.
#[test]
fn conditional_routes_recheck_after_the_baseline() {
    use dudes_proto::rpc::goal::{GoalReplaceRefusal, GoalReplaceResult};
    for (arm, expected) in [
        ("goal.set", GoalReplaceRefusal::StaleGoal),
        ("agent.retire", GoalReplaceRefusal::AgentAbsent),
        ("loop.halt", GoalReplaceRefusal::AgentHalted),
        ("fenced writer", GoalReplaceRefusal::Unavailable),
    ] {
        let daemon = baselined();
        let settings = daemon.fixture.settings();
        let goal = paused_with_goal(&daemon);
        let request = held_replacement(daemon.agent, goal, 3);
        let replacing = std::thread::spawn(move || replace_on(&settings, &request));
        wait_for(
            || daemon.runner.runs() == 1,
            "the replacement's check started",
        );
        let landed = match arm {
            "goal.set" => daemon
                .set(common::goal_params(daemon.agent, &["test", "-f", "absent"]))
                .map(|_| ()),
            "fenced writer" => {
                daemon.append_failure.short_write_next(5);
                let bystander = tempfile::tempdir().expect("a second tree");
                let failed = daemon.running.handle.ask(duded::ports::Ask::Hire(Box::new(
                    common::hire_params(bystander.path()),
                )));
                assert!(failed.is_err(), "the failed append fences the writer");
                Ok(())
            }
            "agent.retire" => daemon
                .running
                .handle
                .ask(duded::ports::Ask::Retire(Box::new(
                    dudes_proto::rpc::agent::RetireParams {
                        agent: daemon.agent,
                        reason: "done".into(),
                    },
                )))
                .map(|_| ()),
            _ => daemon
                .running
                .handle
                .ask(duded::ports::Ask::Halt(daemon.agent))
                .map(|_| ()),
        };
        landed.unwrap_or_else(|error| panic!("{arm} lands during the check: {error:?}"));
        std::fs::write(daemon.tree.path().join("release"), "go\n").unwrap();
        let result = replacing.join().unwrap();
        assert!(
            matches!(result, GoalReplaceResult::Refused { reason, .. } if reason == expected),
            "{arm}: {result:?}"
        );
    }
}

/// A replacement's caller that hangs up during its check does not abandon it, and a retry of the
/// same request returns its result.
#[test]
fn goal_replace_keeps_running_after_a_hang_up() {
    use dudes_proto::rpc::goal::GoalReplaceResult;
    let daemon = baselined();
    let settings = daemon.fixture.settings();
    let goal = paused_with_goal(&daemon);
    let request = held_replacement(daemon.agent, goal, 4);
    let stream = std::os::unix::net::UnixStream::connect(settings.socket()).expect("connect");
    let mut client = dudes_rpc::Client::new(
        std::io::BufReader::new(stream.try_clone().unwrap()),
        stream.try_clone().unwrap(),
    );
    client
        .send_presenting(
            dudes_proto::rpc::Method::GoalReplace,
            Some(serde_json::to_value(&request).unwrap()),
            Some(operator_credential(&settings)),
        )
        .unwrap();
    wait_for(|| daemon.runner.runs() == 1, "the check started");
    stream.shutdown(std::net::Shutdown::Both).unwrap();
    drop((client, stream));
    std::fs::write(daemon.tree.path().join("release"), "go\n").unwrap();
    wait_for(
        || {
            daemon
                .goal_sets()
                .iter()
                .any(|set| set.text == "replacement")
        },
        "the replacement was appended",
    );
    // The first call's thread answers its gone caller before it lets the request id go, so a
    // retry in that moment is still pending.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let retried = loop {
        let retried = replace_on(&settings, &request);
        if !matches!(retried, GoalReplaceResult::Pending { .. })
            || std::time::Instant::now() > deadline
        {
            break retried;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        matches!(retried, GoalReplaceResult::Changed { .. }),
        "{retried:?}"
    );
}
