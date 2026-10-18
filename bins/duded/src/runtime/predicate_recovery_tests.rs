use super::work_test_support as common;
use super::*;
use crate::predicate::PredicateOutcome;
use dudes_proto::rpc::recovery::{
    ExpectedRecovery, RecoveryAllowedChoices, RecoveryChoice, RecoveryInspectResult,
    RecoveryKnownPrefix, RecoverySubmitRefusal, RecoverySubmitResult, RecoveryWaitStatus,
    RecoveryWaitSummary,
};
use dudes_proto::{ExitStatus, TurnOutcome};
use dudes_providers::launch::LaunchSpec;
use dudes_supervisor::testing::FakeHost;
use dudes_types::{Provenance, Usage};
use std::io;
use std::sync::atomic::AtomicUsize;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Instant;

type TestDaemon =
    Daemon<common::SharedHost, ControlledRunner, common::TestClock, common::FixtureRng>;

#[derive(Clone, Copy)]
enum RunnerReply {
    Blocked,
    Error,
    False,
    Swept,
}

struct ControlledReplies {
    all: RunnerReply,
    folders: BTreeMap<PathBuf, RunnerReply>,
}

struct ControlledRunner {
    reply: Mutex<ControlledReplies>,
    ready: Condvar,
    calls: AtomicUsize,
}

impl ControlledRunner {
    fn new(reply: RunnerReply) -> Arc<Self> {
        Arc::new(Self {
            reply: Mutex::new(ControlledReplies {
                all: reply,
                folders: BTreeMap::new(),
            }),
            ready: Condvar::new(),
            calls: AtomicUsize::new(0),
        })
    }

    fn release_error(&self) {
        self.reply
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .all = RunnerReply::Error;
        self.ready.notify_all();
    }

    fn release_folder_error(&self, folder: PathBuf) {
        self.reply
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .folders
            .insert(folder, RunnerReply::Error);
        self.ready.notify_all();
    }

    fn wait_for_run(&self) {
        self.wait_for_calls(1);
    }

    fn wait_for_calls(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.calls.load(Ordering::SeqCst) < count {
            assert!(
                Instant::now() < deadline,
                "the actual predicate worker must enter the runner"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl PredicateRunner for ControlledRunner {
    fn run(&self, spec: &LaunchSpec, _timeout: Duration) -> io::Result<PredicateOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut reply = self.reply.lock().unwrap_or_else(PoisonError::into_inner);
        let selected =
            |reply: &ControlledReplies| reply.folders.get(&spec.cwd).copied().unwrap_or(reply.all);
        while matches!(selected(&reply), RunnerReply::Blocked) {
            reply = self
                .ready
                .wait(reply)
                .unwrap_or_else(PoisonError::into_inner);
        }
        match selected(&reply) {
            RunnerReply::Error => Err(io::Error::other("controlled predicate runner failure")),
            RunnerReply::False => Ok(common::fails()),
            RunnerReply::Swept => Ok(PredicateOutcome::Swept),
            RunnerReply::Blocked => unreachable!("the runner waits until released"),
        }
    }

    fn sweep(&self) {
        let mut reply = self.reply.lock().unwrap_or_else(PoisonError::into_inner);
        reply.all = RunnerReply::Swept;
        reply.folders.clear();
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
    boot_with_host(fixture, runner, host, false)
}

fn boot_with_host(
    fixture: &common::Fixture,
    runner: &Arc<ControlledRunner>,
    host: common::SharedHost,
    native_children: bool,
) -> (TestDaemon, common::SharedHost) {
    let settings = fixture.settings();
    let mut seams = common::seams(
        &settings,
        host.clone(),
        Arc::clone(runner),
        Arc::new(common::TestClock::new()),
        Witness::default(),
    );
    seams.adapters = [
        Box::new(dudes_providers::claude::ClaudeAdapter::new(
            dudes_providers::claude::config::ClaudeConfig {
                command: "/usr/bin/true".into(),
                ..dudes_providers::claude::config::ClaudeConfig::default()
            },
        )) as Box<dyn dudes_providers::adapter::Adapter>,
        Box::new(dudes_providers::codex::CodexAdapter::new(
            dudes_providers::codex::config::CodexConfig::default(),
        )),
        Box::new(dudes_providers::pi::managed::PiAdapter::new(
            dudes_providers::pi::config::PiConfig::default(),
        )),
    ]
    .into_iter()
    .collect();
    if native_children {
        host.bind_child_ends(Arc::clone(&seams.children));
    }
    (crate::boot(&settings, seams).unwrap(), host)
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
    source_fixture: common::Fixture,
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
            source_fixture: source,
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

fn take_predicate_end(daemon: &TestDaemon, goal: GoalId) -> PathBuf {
    let deadline = Instant::now() + Duration::from_secs(5);
    for _ in 0..64 {
        match daemon
            .jobs
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the actual RunEnd must post its no-verdict completion")
        {
            Job::PredicateEnded {
                goal: ended,
                folder,
                verdict,
            } => {
                assert_eq!(ended, goal);
                assert!(verdict.is_none());
                return folder;
            }
            Job::Input(_) | Job::Stop(_) => {}
            _ => panic!("the fixture received an unrelated completion"),
        }
    }
    panic!("the actual predicate completion must arrive within the fixture bound");
}

fn operator_admission() -> dudes_rpc::dispatch::OperatorAdmission {
    let (sent, received) = std::sync::mpsc::channel();
    let credential = "C".repeat(43);
    let mut dispatcher = dudes_rpc::Dispatcher::new(dudes_rpc::PeerClassifier::new(
        dudes_proto::rpc::cred::OperatorCredential::new(credential.clone()).unwrap(),
        Arc::new(crate::classify::WorldTokens::new()),
    ));
    dispatcher.register_current_operator(
        dudes_proto::rpc::Method::SystemPing.into(),
        move |_, admission| {
            sent.send(admission).unwrap();
            Ok(serde_json::Value::Null)
        },
    );
    dudes_rpc::server::serve_stream(
        std::io::Cursor::new(
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"system.ping\",\"cred\":\"{credential}\"}}\n"
            )
            .into_bytes(),
        ),
        &Arc::new(dudes_rpc::Outbound::sink()),
        dudes_rpc::PeerIdentity {
            uid: Some(501),
            gid: None,
            pid: dudes_rpc::PeerPid::Reported {
                pid: 4242,
                pidversion: None,
            },
        },
        dudes_rpc::PeerProvenance::NoRecordedAgent,
        &dispatcher,
    )
    .unwrap();
    received.recv().unwrap()
}

fn end_native_turn(daemon: &mut TestDaemon, host: &common::SharedHost, turn: TurnId) {
    let row = &daemon.sup.world().turns[&turn];
    let key = dudes_supervisor::AdoptionKey {
        pid: row.pid.unwrap(),
        start_time: row.start_time.unwrap(),
        boot: row.boot.clone(),
    };
    host.mark_native_reaped(&key);
    host.finish_owned_child(
        &key,
        Input::ChildExited {
            final_reply: None,
            elapsed_ms: Some(6_000),
            turn,
            exit: ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: None,
            model_usage: BTreeMap::new(),
            outcome: TurnOutcome::Completed,
            last_message: None,
        },
    );
    let Job::Input(input) = daemon.jobs.recv_timeout(Duration::from_secs(5)).unwrap() else {
        panic!("the actual owned child completion must arrive");
    };
    assert!(matches!(input.as_ref(), Input::ChildExited { turn: ended, .. } if *ended == turn));
    daemon.pump(*input).unwrap();
    assert!(host.native_acknowledged(&key));
}

fn take_ending_sample(daemon: &mut TestDaemon) -> Input {
    let deadline = Instant::now() + Duration::from_secs(5);
    for _ in 0..64 {
        let Job::Input(input) = daemon
            .jobs
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the actual ending sample must arrive")
        else {
            panic!("the ending sample precedes its predicate run");
        };
        if matches!(
            input.as_ref(),
            Input::InstructionSampled {
                comparison: Some(_),
                ..
            }
        ) {
            return *input;
        }
        daemon.pump(*input).unwrap();
    }
    panic!("the actual ending sample must arrive within the fixture bound");
}

fn take_false_completion(daemon: &TestDaemon, goal: GoalId) -> (PathBuf, Evidence) {
    let Job::PredicateEnded {
        goal: ended,
        folder,
        verdict: Some((pass, evidence)),
    } = daemon.jobs.recv_timeout(Duration::from_secs(5)).unwrap()
    else {
        panic!("the actual predicate must complete with its observed verdict");
    };
    assert_eq!(ended, goal);
    assert!(!pass);
    (folder, evidence)
}

fn inspect_unconfirmed(daemon: &mut TestDaemon, agent: AgentId) -> ExpectedRecovery {
    let Answered::RecoveryInspected(RecoveryInspectResult::Available {
        expected,
        reason,
        choices,
        waiting,
        ..
    }) = daemon
        .recovery_inspect(agent, operator_admission())
        .unwrap()
    else {
        panic!("the settled unconfirmed turn offers recovery");
    };
    assert_eq!(
        reason,
        dudes_proto::recovery::RecoveryReason::UnconfirmedContext
    );
    assert_eq!(choices, RecoveryAllowedChoices::FreshOnly);
    assert_eq!(
        waiting,
        RecoveryWaitSummary {
            open_question: RecoveryWaitStatus::Ready,
            budget: RecoveryWaitStatus::Ready,
            hops: RecoveryWaitStatus::Ready,
            instruction_review: RecoveryWaitStatus::Ready,
            instruction_comparison: RecoveryWaitStatus::Ready,
        }
    );
    *expected
}

fn fresh_consent(expected: &ExpectedRecovery) -> dudes_supervisor::input::RecoveryCommand {
    crate::ports::recovery_command(
        &dudes_proto::rpc::recovery::RecoverySubmitParams {
            request_id: dudes_proto::record::Hex32::new([83; 32]),
            expected: *expected,
            choice: RecoveryChoice::FreshWithRecap,
        },
        dudes_proto::recovery::RecoveryAttribution::LocalOperator {
            pid: 4242,
            uid: Some(501),
        },
    )
    .unwrap()
}

fn assert_one_fresh_launch(
    daemon: &mut TestDaemon,
    host: &common::SharedHost,
    fixture: &common::Fixture,
    expected: &ExpectedRecovery,
    launches: usize,
) {
    let fresh = inspect_unconfirmed(daemon, expected.work.agent);
    assert_eq!(fresh, *expected);
    assert!(matches!(
        daemon.recovery_submit(fresh_consent(&fresh), operator_admission()),
        Answered::RecoverySubmitted(RecoverySubmitResult::Accepted { .. })
    ));
    let recovered = await_spawn(daemon, expected.work.agent);
    assert_ne!(recovered, expected.work.turn);
    assert_eq!(host.spawned().len(), launches + 1);
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::RecoveryAuthorized(_)))
            .count(),
        1
    );
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::TurnIssued { .. }))
            .count(),
        3
    );
}

#[test]
fn an_ending_predicate_can_refuse_previously_available_fresh_consent() {
    let fixture = common::Fixture::new();
    let runner = ControlledRunner::new(RunnerReply::False);
    let (mut daemon, host) = boot_with_host(
        &fixture,
        &runner,
        common::SharedHost::with_native_evidence(fixture.journal_dir()),
        true,
    );
    let agent = hire(&mut daemon, fixture.state.path());
    let goal = goal(&mut daemon, agent, 8);
    let first = await_spawn(&mut daemon, agent);
    let session = daemon.sup.world().turns[&first].announced.clone().unwrap();
    daemon
        .pump(Input::SessionLearned {
            turn: first,
            session,
        })
        .unwrap();
    assert!(daemon.sup.world().turns[&first].session_confirmed);
    end_native_turn(&mut daemon, &host, first);
    let sample = take_ending_sample(&mut daemon);
    daemon.pump(sample).unwrap();
    let (folder, evidence) = take_false_completion(&daemon, goal);
    daemon
        .predicate_ended(goal, &folder, Some((false, evidence)))
        .unwrap();
    let interrupted = await_spawn(&mut daemon, agent);
    assert_ne!(interrupted, first);
    assert!(!daemon.sup.world().turns[&interrupted].session_confirmed);
    end_native_turn(&mut daemon, &host, interrupted);
    let ending = take_ending_sample(&mut daemon);
    daemon
        .pump(Input::Command(Command::Resume {
            agent,
            browser: None,
        }))
        .unwrap();
    let expected = inspect_unconfirmed(&mut daemon, agent);
    assert_eq!(expected.work.goal, goal);
    assert_eq!(expected.work.turn, interrupted);
    assert!(daemon.ctx.predicate_runs.is_empty());
    daemon.pump(ending).unwrap();
    let (folder, evidence) = take_false_completion(&daemon, goal);
    assert_eq!(daemon.ctx.predicate_runs.get(&goal), Some(&1));
    assert_eq!(
        daemon.ctx.predicate_folders.get(&(goal, folder.clone())),
        Some(&1)
    );
    assert!(daemon.sup.recovery_opportunity(agent).is_ok());
    assert!(
        daemon
            .ctx
            .admit_recovery_prior_attempt(&daemon.sup, agent, Some(interrupted))
            .is_ok()
    );
    assert!(
        !daemon
            .ctx
            .recovery_consent_checkout_clear(&daemon.sup, agent)
    );
    let before = fixture.entries();
    let launches = host.spawned().len();
    let submitted = daemon.recovery_submit(fresh_consent(&expected), operator_admission());
    let Answered::RecoverySubmitted(submitted) = submitted else {
        panic!("explicit recovery consent returns its published result");
    };
    assert_eq!(
        submitted,
        RecoverySubmitResult::Refused {
            reason: RecoverySubmitRefusal::NotRecoverable,
            prefix: RecoveryKnownPrefix {
                required: None,
                resume: None
            },
        }
    );
    assert_eq!(fixture.entries(), before);
    assert_eq!(host.spawned().len(), launches);
    daemon
        .predicate_ended(goal, &folder, Some((false, evidence)))
        .unwrap();
    assert!(daemon.ctx.predicate_runs.is_empty());
    assert!(daemon.ctx.predicate_folders.is_empty());
    assert!(fixture.records().iter().any(|record| matches!(record,
        Record::PredicateResult { goal: checked, turn: Some(turn), provenance: Provenance::Observed, pass: false, .. }
        if *checked == goal && *turn == interrupted)));
    assert!(
        daemon
            .ctx
            .recovery_consent_checkout_clear(&daemon.sup, agent)
    );
    assert_one_fresh_launch(&mut daemon, &host, &fixture, &expected, launches);
    daemon.exit(ExitCause::Ended);
}

#[test]
fn a_recovered_predicate_runner_error_releases_new_work_without_a_verdict() {
    let prefix = Prefix::unfinished_end();
    let runner = ControlledRunner::new(RunnerReply::Error);
    let (mut daemon, host) = prefix.restart(&runner);
    prefix.recover_finished(&mut daemon);
    let folder = take_predicate_end(&daemon, prefix.goal);
    assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
    daemon.predicate_ended(prefix.goal, &folder, None).unwrap();
    assert!(daemon.ctx.predicate_runs.is_empty());
    assert!(daemon.ctx.predicate_folders.is_empty());
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
    assert!(daemon.ctx.predicate_folders.is_empty());
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
    daemon
        .predicate_ended(wrong, std::path::Path::new("/unmatched-run"), None)
        .unwrap();
    assert_eq!(daemon.ctx.predicate_runs.get(&prefix.goal), Some(&1));
    let probe = prefix.probe(&mut daemon);
    assert_eq!(daemon.sup.world().agents[&probe].open_turn, None);
    assert!(host.spawned().is_empty());
    runner.release_error();
    let folder = take_predicate_end(&daemon, prefix.goal);
    daemon.predicate_ended(prefix.goal, &folder, None).unwrap();
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
    let folder = take_predicate_end(&daemon, prefix.goal);
    let before = prefix.fixture.entries();
    daemon.predicate_ended(prefix.goal, &folder, None).unwrap();
    assert_eq!(prefix.fixture.entries(), before);
    assert_eq!(daemon.sup.world().agents[&probe].open_turn, None);
    assert!(host.spawned().is_empty());
    assert!(daemon.ctx.predicate_runs.is_empty());
    assert!(daemon.ctx.predicate_folders.is_empty());
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
    assert!(daemon.ctx.predicate_folders.is_empty());
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
        .predicate_ended(
            GoalId::from_parts(0, 1_000),
            std::path::Path::new("/unmatched-run"),
            None,
        )
        .unwrap();
    assert_eq!(fixture.entries(), before);
    let agent = hire(&mut daemon, fixture.state.path());
    goal(&mut daemon, agent, 1);
    let turn = await_spawn(&mut daemon, agent);
    assert_eq!(daemon.sup.world().turns[&turn].agent, agent);
    assert_eq!(host.spawned().len(), 1);
    assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn recovery_checkout_admission_uses_actual_check_folders_for_every_engine() {
    let prefix = Prefix::unfinished_end();
    let runner = ControlledRunner::new(RunnerReply::Blocked);
    let (mut daemon, _) = prefix.restart(&runner);
    prefix.recover_finished(&mut daemon);
    runner.wait_for_run();
    let actual = daemon
        .ctx
        .predicate_folders
        .keys()
        .next()
        .unwrap()
        .1
        .clone();
    assert_eq!(
        actual,
        std::fs::canonicalize(prefix.source_fixture.state.path()).unwrap()
    );
    let other = prefix.fixture.state.path().join("unrelated-checkout");
    std::fs::create_dir(&other).unwrap();
    let before = daemon.ctx.predicate_folders.clone();
    assert!(
        !daemon
            .ctx
            .recovery_checkout_clear(&daemon.sup, prefix.agent)
    );
    for provider in [Provider::Claude, Provider::Codex, Provider::Pi] {
        for (cwd, allowed) in [(&actual, false), (&other, true)] {
            let mut params = common::hire_params(cwd);
            params.provider = provider;
            params.budget.tokens_per_day = Some(dudes_types::Tokens::new(100_000));
            let Answered::Hired(hired) = daemon.ask(Ask::Hire(Box::new(params))).unwrap() else {
                panic!("the candidate hire must return its recorded owner");
            };
            assert_eq!(daemon.sup.world().agents[&hired.agent].provider, provider);
            assert_eq!(
                daemon.ctx.recovery_checkout_clear(&daemon.sup, hired.agent),
                allowed,
                "{provider} {cwd:?}"
            );
        }
    }
    assert_eq!(daemon.ctx.predicate_folders, before);
    runner.release_error();
    let folder = take_predicate_end(&daemon, prefix.goal);
    assert_eq!(folder, actual);
    daemon.predicate_ended(prefix.goal, &folder, None).unwrap();
    assert!(daemon.ctx.predicate_folders.is_empty());
}

#[test]
fn same_goal_check_completions_retire_only_their_immutable_execution_folder() {
    let prefix = Prefix::unfinished_end();
    let runner = ControlledRunner::new(RunnerReply::Blocked);
    let (mut daemon, _) = prefix.restart(&runner);
    prefix.recover_finished(&mut daemon);
    runner.wait_for_run();
    let original = daemon
        .ctx
        .predicate_folders
        .keys()
        .next()
        .unwrap()
        .1
        .clone();
    let before = daemon.ctx.predicate_folders.clone();
    let later_goal = goal(&mut daemon, prefix.agent, 1);
    assert_ne!(later_goal, prefix.goal);
    assert_eq!(
        daemon.sup.world().agents[&prefix.agent].goal,
        Some(later_goal)
    );
    assert_eq!(daemon.ctx.predicate_folders, before);
    let changed = prefix.fixture.state.path().join("later-checkout");
    std::fs::create_dir(&changed).unwrap();
    let changed = std::fs::canonicalize(changed).unwrap();
    let mut queue = VecDeque::new();
    daemon.ctx.run_predicate(
        prefix.goal,
        &changed,
        &Predicate::Command {
            argv: vec!["/usr/bin/false".into()],
            timeout_s: 10,
        },
        &mut queue,
    );
    assert!(queue.is_empty());
    runner.wait_for_calls(2);
    assert_eq!(daemon.ctx.predicate_runs.get(&prefix.goal), Some(&2));
    runner.release_folder_error(changed.clone());
    let folder = take_predicate_end(&daemon, prefix.goal);
    assert_eq!(folder, changed);
    daemon.predicate_ended(prefix.goal, &folder, None).unwrap();
    assert_eq!(daemon.ctx.predicate_runs.get(&prefix.goal), Some(&1));
    assert_eq!(
        daemon
            .ctx
            .predicate_folders
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![(prefix.goal, original.clone())]
    );
    daemon.predicate_ended(prefix.goal, &changed, None).unwrap();
    assert_eq!(daemon.ctx.predicate_runs.get(&prefix.goal), Some(&1));
    runner.release_error();
    let folder = take_predicate_end(&daemon, prefix.goal);
    assert_eq!(folder, original);
    daemon.predicate_ended(prefix.goal, &folder, None).unwrap();
    assert!(daemon.ctx.predicate_folders.is_empty());
    assert!(daemon.ctx.predicate_runs.is_empty());
}

#[test]
fn agentless_baseline_folder_blocks_recovery_until_its_actual_end() {
    let fixture = common::Fixture::new();
    let runner = ControlledRunner::new(RunnerReply::Blocked);
    let (mut daemon, _) = boot(&fixture, &runner, None);
    let agent = hire(&mut daemon, fixture.state.path());
    let other = fixture.state.path().join("other-checkout");
    std::fs::create_dir(&other).unwrap();
    let peer = hire(&mut daemon, &other);
    let run = daemon
        .ctx
        .baselines
        .start_browser(None, dudes_proto::record::Hex32::new([78; 32]))
        .unwrap();
    let Answered::CheckRunning(ticket) = daemon
        .start_baseline(
            run,
            vec!["/usr/bin/false".into()],
            10,
            fixture.state.path().to_path_buf(),
        )
        .unwrap()
    else {
        panic!("the actual baseline must own its worker");
    };
    runner.wait_for_run();
    assert_eq!(
        ticket.cwd,
        std::fs::canonicalize(fixture.state.path()).unwrap()
    );
    assert!(!daemon.ctx.recovery_checkout_clear(&daemon.sup, agent));
    assert!(daemon.ctx.recovery_checkout_clear(&daemon.sup, peer));
    runner.release_error();
    let Job::BaselineEnded { id } = daemon.jobs.recv_timeout(Duration::from_secs(5)).unwrap()
    else {
        panic!("the baseline worker must end its own run mark");
    };
    assert_eq!(id, run);
    daemon.ctx.baselines.end(id);
    assert!(daemon.ctx.recovery_checkout_clear(&daemon.sup, agent));
    let missing = daemon.ctx.baselines.start_local(agent);
    let Answered::CheckRunning(ticket) = daemon
        .start_baseline(
            missing,
            vec!["/usr/bin/false".into()],
            10,
            fixture.state.path().join("missing-folder"),
        )
        .unwrap()
    else {
        panic!("a preparation refusal retains the baseline result contract");
    };
    assert!(matches!(
        ticket.wait(
            &std::sync::atomic::AtomicBool::new(false),
            &dudes_rpc::HangUp::never()
        ),
        crate::baseline::Baseline::Refused(crate::baseline::Refusal::CannotRun(_))
    ));
    assert!(!daemon.ctx.baselines.local_running(agent));
    assert_eq!(daemon.ctx.baselines.active_folders(2).count(), 0);
}

#[test]
fn an_unresolved_predicate_folder_keeps_the_observed_preparation_refusal_without_a_run_mark() {
    let fixture = common::Fixture::new();
    let runner = ControlledRunner::new(RunnerReply::Error);
    let (mut daemon, _) = boot(&fixture, &runner, None);
    let goal = GoalId::from_parts(95, 1);
    let cwd = fixture.state.path().join("missing-predicate-folder");
    let argv = vec!["/usr/bin/false".into()];
    let error = predicate::spec_for(
        &argv,
        &cwd,
        &daemon.ctx.predicate_env,
        daemon.ctx.predicate_path.as_deref(),
    )
    .unwrap_err();
    let (expected_pass, expected_evidence) = predicate::refused(&argv, &error);
    let mut queue = VecDeque::new();
    daemon.ctx.run_predicate(
        goal,
        &cwd,
        &Predicate::Command {
            argv,
            timeout_s: 10,
        },
        &mut queue,
    );
    let Some(Input::PredicateDone {
        goal: seen,
        pass,
        evidence,
    }) = queue.pop_front()
    else {
        panic!("the known preparation refusal must remain an observed failed check");
    };
    assert_eq!(seen, goal);
    assert_eq!(pass, expected_pass);
    assert_eq!(evidence, expected_evidence);
    assert!(queue.is_empty());
    assert!(daemon.ctx.predicate_runs.is_empty());
    assert!(daemon.ctx.predicate_folders.is_empty());
    assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
}
