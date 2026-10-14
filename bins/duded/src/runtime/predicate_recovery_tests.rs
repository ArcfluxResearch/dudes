use super::work_test_support as common;
use super::*;
use crate::predicate::PredicateOutcome;
use dudes_proto::{ExitStatus, TurnOutcome};
use dudes_providers::launch::LaunchSpec;
use dudes_supervisor::testing::FakeHost;
use dudes_types::Usage;
use std::io;
use std::sync::atomic::AtomicUsize;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Instant;

type TestDaemon = Daemon<
    common::SharedHost,
    ControlledRunner,
    common::TestClock,
    dudes_supervisor::testing::FakeRng,
>;

#[derive(Clone, Copy)]
enum RunnerReply {
    Blocked,
    Error,
    Swept,
}

struct ControlledRunner {
    reply: Mutex<RunnerReply>,
    ready: Condvar,
    calls: AtomicUsize,
}

impl ControlledRunner {
    fn new(reply: RunnerReply) -> Arc<Self> {
        Arc::new(Self {
            reply: Mutex::new(reply),
            ready: Condvar::new(),
            calls: AtomicUsize::new(0),
        })
    }

    fn release_error(&self) {
        *self.reply.lock().unwrap_or_else(PoisonError::into_inner) = RunnerReply::Error;
        self.ready.notify_all();
    }

    fn wait_for_run(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.calls.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "the actual predicate worker must enter the runner"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl PredicateRunner for ControlledRunner {
    fn run(&self, _spec: &LaunchSpec, _timeout: Duration) -> io::Result<PredicateOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut reply = self.reply.lock().unwrap_or_else(PoisonError::into_inner);
        while matches!(*reply, RunnerReply::Blocked) {
            reply = self
                .ready
                .wait(reply)
                .unwrap_or_else(PoisonError::into_inner);
        }
        match *reply {
            RunnerReply::Error => Err(io::Error::other("controlled predicate runner failure")),
            RunnerReply::Swept => Ok(PredicateOutcome::Swept),
            RunnerReply::Blocked => unreachable!("the runner waits until released"),
        }
    }

    fn sweep(&self) {
        *self.reply.lock().unwrap_or_else(PoisonError::into_inner) = RunnerReply::Swept;
        self.ready.notify_all();
    }
}

fn boot(
    fixture: &common::Fixture,
    runner: &Arc<ControlledRunner>,
    reserved: Option<(AgentId, GoalId, TurnId)>,
) -> (TestDaemon, common::SharedHost) {
    let mut fake = FakeHost::wall_clock_anchored();
    fake.next_pid = if reserved.is_some() { 9_000 } else { 1_000 };
    let host = common::SharedHost::with(fake);
    let mut seams = common::seams(
        host.clone(),
        Arc::clone(runner),
        Arc::new(common::TestClock::new()),
        Witness::default(),
    );
    if let Some((agent, goal, turn)) = reserved {
        let last = agent.as_ulid().max(goal.as_ulid()).max(turn.as_ulid());
        while dudes_supervisor::Rng::ulid(&mut seams.rng) <= last {}
    }
    seams.adapters = [Box::new(dudes_providers::claude::ClaudeAdapter::new(
        dudes_providers::claude::config::ClaudeConfig {
            command: "/usr/bin/true".into(),
            ..dudes_providers::claude::config::ClaudeConfig::default()
        },
    )) as Box<dyn dudes_providers::adapter::Adapter>]
    .into_iter()
    .collect();
    (crate::boot(&fixture.settings(), seams).unwrap(), host)
}

fn hire(daemon: &mut TestDaemon, cwd: &std::path::Path) -> AgentId {
    let Answered::Hired(result) = daemon
        .ask(Ask::Hire(Box::new(common::hire_params(cwd))))
        .expect("the fixture hire must be durable")
    else {
        panic!("the fixture hire must return its actual agent");
    };
    result.agent
}

fn goal(daemon: &mut TestDaemon, agent: AgentId, hop_cap: u32) -> GoalId {
    let mut params = common::goal_params(agent, &["/usr/bin/false"]);
    params.may_already_pass = true;
    params.hop_cap = hop_cap;
    let Answered::GoalSet(result) = daemon
        .ask(Ask::SetGoal(Box::new(params)))
        .expect("the fixture goal must be durable")
    else {
        panic!("the fixture goal must return its actual identity");
    };
    result.goal
}

fn pump_worker_input(daemon: &mut TestDaemon, deadline: Instant) {
    let Job::Input(input) = daemon
        .jobs
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .expect("the bounded sample must return its actual worker input")
    else {
        panic!("the fixture must own predicate completion separately");
    };
    daemon
        .pump(*input)
        .expect("the actual runtime pump must fold its input");
}

fn await_spawn(daemon: &mut TestDaemon, agent: AgentId) -> TurnId {
    let deadline = Instant::now() + Duration::from_secs(5);
    for _ in 0..64 {
        if let Some(turn) = daemon.sup.world().agents[&agent].open_turn
            && daemon.sup.world().turns[&turn].is_open_child()
        {
            return turn;
        }
        pump_worker_input(daemon, deadline);
    }
    panic!("the admitted independent goal must record its physical spawn");
}

struct Prefix {
    fixture: common::Fixture,
    _source: common::Fixture,
    agent: AgentId,
    goal: GoalId,
    turn: TurnId,
    started: Lsn,
    end: Lsn,
}

impl Prefix {
    fn unfinished_end() -> Self {
        let source = common::Fixture::new();
        let (mut daemon, _) = boot(&source, &ControlledRunner::new(RunnerReply::Error), None);
        let agent = hire(&mut daemon, source.state.path());
        let goal = goal(&mut daemon, agent, 1);
        let turn = await_spawn(&mut daemon, agent);
        let started = daemon
            .sup
            .world()
            .instructions
            .current_for_turn(turn)
            .unwrap()
            .0;
        daemon
            .pump(Input::ChildExited {
                final_reply: None,
                elapsed_ms: Some(6_000),
                turn,
                exit: ExitStatus {
                    code: Some(0),
                    signal: None,
                },
                usage: Some(dudes_providers::stream::ScopedUsage::Turn(Usage::ZERO)),
                model_usage: BTreeMap::new(),
                outcome: TurnOutcome::Completed,
                last_message: None,
            })
            .unwrap();
        let end = daemon
            .sup
            .world()
            .instructions
            .managed_end_lsn(agent, turn)
            .unwrap();
        assert!(
            daemon
                .sup
                .world()
                .instructions
                .attempt(started)
                .unwrap()
                .finished
                .is_none()
        );
        let fixture = common::Fixture::new();
        std::fs::create_dir(fixture.journal_dir()).unwrap();
        for file in std::fs::read_dir(source.journal_dir()).unwrap() {
            let file = file.unwrap();
            std::fs::copy(file.path(), fixture.journal_dir().join(file.file_name())).unwrap();
        }
        assert_eq!(fixture.entries(), source.entries());
        assert!(fixture.entries().iter().any(|(lsn, entry)| *lsn == end
            && matches!(entry.record, Record::TurnEnded { turn: ended, .. } if ended == turn)));
        assert!(!fixture.records().iter().any(|record| matches!(
            record,
            Record::InstructionObservationFinished { .. } | Record::PredicateResult { .. }
        )));
        drop(daemon);
        Self {
            fixture,
            _source: source,
            agent,
            goal,
            turn,
            started,
            end,
        }
    }

    fn restart(&self, runner: &Arc<ControlledRunner>) -> (TestDaemon, common::SharedHost) {
        let (daemon, host) = boot(
            &self.fixture,
            runner,
            Some((self.agent, self.goal, self.turn)),
        );
        assert_eq!(
            daemon
                .sup
                .world()
                .instructions
                .managed_end_lsn(self.agent, self.turn),
            Some(self.end)
        );
        assert!(
            daemon
                .sup
                .world()
                .instructions
                .attempt(self.started)
                .unwrap()
                .finished
                .is_none()
        );
        (daemon, host)
    }

    fn recover_finished(&self, daemon: &mut TestDaemon) {
        let deadline = Instant::now() + Duration::from_secs(5);
        for _ in 0..64 {
            if let Some(finished) = &daemon
                .sup
                .world()
                .instructions
                .attempt(self.started)
                .unwrap()
                .finished
            {
                assert!(
                    self.fixture
                        .entries()
                        .iter()
                        .any(|(lsn, entry)| *lsn == finished.lsn
                            && matches!(entry.record, Record::InstructionObservationFinished {
                        baseline: dudes_proto::InstructionBaselineRef::Started { lsn }, ..
                    } if lsn == self.started))
                );
                return;
            }
            pump_worker_input(daemon, deadline);
        }
        panic!("recovery must persist the actual ending observation before the check");
    }

    fn probe(&self, daemon: &mut TestDaemon) -> AgentId {
        let cwd = self.fixture.state.path().join("independent-probe");
        std::fs::create_dir(&cwd).unwrap();
        let agent = hire(daemon, &cwd);
        assert_ne!(agent, self.agent);
        let fresh = goal(daemon, agent, 1);
        assert_ne!(fresh, self.goal);
        agent
    }

    fn assert_no_verdict(&self) {
        assert!(
            !self
                .fixture
                .records()
                .iter()
                .any(|record| matches!(record, Record::PredicateResult { .. }))
        );
    }
}

fn take_predicate_end(daemon: &TestDaemon, goal: GoalId) {
    let deadline = Instant::now() + Duration::from_secs(5);
    for _ in 0..64 {
        match daemon
            .jobs
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the actual RunEnd must post its no-verdict completion")
        {
            Job::PredicateEnded {
                goal: ended,
                verdict,
            } => {
                assert_eq!(ended, goal);
                assert!(verdict.is_none());
                return;
            }
            Job::Input(_) | Job::Stop(_) => {}
            _ => panic!("the fixture received an unrelated completion"),
        }
    }
    panic!("the actual predicate completion must arrive within the fixture bound");
}

#[test]
fn a_recovered_predicate_runner_error_releases_new_work_without_a_verdict() {
    let prefix = Prefix::unfinished_end();
    let runner = ControlledRunner::new(RunnerReply::Error);
    let (mut daemon, host) = prefix.restart(&runner);
    prefix.recover_finished(&mut daemon);
    take_predicate_end(&daemon, prefix.goal);
    assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
    daemon.predicate_ended(prefix.goal, None).unwrap();
    assert!(daemon.ctx.predicate_runs.is_empty());
    prefix.assert_no_verdict();
    let probe = prefix.probe(&mut daemon);
    let turn = await_spawn(&mut daemon, probe);
    assert_eq!(daemon.sup.world().turns[&turn].agent, probe);
    assert_eq!(host.spawned().len(), 1);
    prefix.assert_no_verdict();
}

#[test]
fn a_recovered_predicate_thread_spawn_error_releases_new_work_without_a_verdict() {
    let prefix = Prefix::unfinished_end();
    let runner = ControlledRunner::new(RunnerReply::Error);
    let (mut daemon, host) = prefix.restart(&runner);
    daemon.predicate_spawn_failure().arm();
    prefix.recover_finished(&mut daemon);
    assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
    assert!(daemon.ctx.predicate_runs.is_empty());
    prefix.assert_no_verdict();
    let probe = prefix.probe(&mut daemon);
    let turn = await_spawn(&mut daemon, probe);
    assert_eq!(daemon.sup.world().turns[&turn].agent, probe);
    assert_eq!(host.spawned().len(), 1);
    prefix.assert_no_verdict();
}

#[test]
fn a_wrong_goal_no_verdict_keeps_the_actual_recovery_wait() {
    let prefix = Prefix::unfinished_end();
    let runner = ControlledRunner::new(RunnerReply::Blocked);
    let (mut daemon, host) = prefix.restart(&runner);
    prefix.recover_finished(&mut daemon);
    runner.wait_for_run();
    let wrong = GoalId::from_parts(0, 1_000);
    assert_ne!(wrong, prefix.goal);
    daemon.predicate_ended(wrong, None).unwrap();
    assert_eq!(daemon.ctx.predicate_runs.get(&prefix.goal), Some(&1));
    let probe = prefix.probe(&mut daemon);
    assert_eq!(daemon.sup.world().agents[&probe].open_turn, None);
    assert!(host.spawned().is_empty());
    runner.release_error();
    take_predicate_end(&daemon, prefix.goal);
    daemon.predicate_ended(prefix.goal, None).unwrap();
    let turn = await_spawn(&mut daemon, probe);
    assert_eq!(daemon.sup.world().turns[&turn].agent, probe);
    assert_eq!(host.spawned().len(), 1);
    prefix.assert_no_verdict();
}

#[test]
fn a_no_verdict_after_teardown_cannot_restart_waiting_work() {
    let prefix = Prefix::unfinished_end();
    let runner = ControlledRunner::new(RunnerReply::Blocked);
    let (mut daemon, host) = prefix.restart(&runner);
    prefix.recover_finished(&mut daemon);
    runner.wait_for_run();
    let probe = prefix.probe(&mut daemon);
    assert_eq!(daemon.sup.world().agents[&probe].open_turn, None);
    daemon.exit(ExitCause::Ended);
    take_predicate_end(&daemon, prefix.goal);
    let before = prefix.fixture.entries();
    daemon.predicate_ended(prefix.goal, None).unwrap();
    assert_eq!(prefix.fixture.entries(), before);
    assert_eq!(daemon.sup.world().agents[&probe].open_turn, None);
    assert!(host.spawned().is_empty());
    assert!(daemon.ctx.predicate_runs.is_empty());
    prefix.assert_no_verdict();
}

#[test]
fn a_thread_spawn_failure_during_teardown_cannot_resume_waiting_work() {
    let prefix = Prefix::unfinished_end();
    let runner = ControlledRunner::new(RunnerReply::Error);
    let (mut daemon, host) = prefix.restart(&runner);
    let probe = prefix.probe(&mut daemon);
    assert_eq!(daemon.sup.world().agents[&probe].open_turn, None);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ending = None;
    for _ in 0..64 {
        let Job::Input(input) = daemon
            .jobs
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the actual ending sample must return before teardown")
        else {
            panic!("the unfinished interval has not yet run its predicate");
        };
        if matches!(
            input.as_ref(),
            Input::InstructionSampled {
                comparison: Some(_),
                ..
            }
        ) {
            ending = Some(*input);
            break;
        }
        daemon.pump(*input).unwrap();
    }
    let ending = ending.expect("the queued ending sample retains its real request identity");
    daemon.exit(ExitCause::Ended);
    assert!(daemon.exited);
    let before = prefix.fixture.entries().len();
    daemon.predicate_spawn_failure().arm();
    daemon.pump(ending).unwrap();
    assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
    assert!(daemon.ctx.predicate_runs.is_empty());
    assert!(
        daemon
            .sup
            .world()
            .instructions
            .attempt(prefix.started)
            .unwrap()
            .finished
            .is_some()
    );
    let after = prefix.fixture.entries();
    assert!(
        after[before..].iter().any(|(_, entry)| matches!(
            entry.record,
            Record::InstructionObservationFinished { .. }
        ))
    );
    assert!(
        !after[before..]
            .iter()
            .any(|(_, entry)| matches!(entry.record,
        Record::LoopDecided { agent, .. } | Record::TurnIssued { agent, .. }
            | Record::TurnSpawned { agent, .. } if agent == probe))
    );
    assert_eq!(daemon.sup.world().agents[&probe].open_turn, None);
    assert!(host.spawned().is_empty());
    prefix.assert_no_verdict();
}

#[test]
fn an_unmatched_normal_no_verdict_does_not_append_or_block_work() {
    let fixture = common::Fixture::new();
    let runner = ControlledRunner::new(RunnerReply::Error);
    let (mut daemon, host) = boot(&fixture, &runner, None);
    let before = fixture.entries();
    daemon
        .predicate_ended(GoalId::from_parts(0, 1_000), None)
        .unwrap();
    assert_eq!(fixture.entries(), before);
    let agent = hire(&mut daemon, fixture.state.path());
    goal(&mut daemon, agent, 1);
    let turn = await_spawn(&mut daemon, agent);
    assert_eq!(daemon.sup.world().turns[&turn].agent, agent);
    assert_eq!(host.spawned().len(), 1);
    assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
}
