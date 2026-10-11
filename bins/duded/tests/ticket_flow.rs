//! A ticket given to an agent starts work, waits its turn in a queue, holds its project one
//! ticket at a time, and reaches Ready for you only when the daemon's own check passes.
//!
//! Every test boots the real daemon over a scratch state and runtime directory and drives it
//! through its production socket. The engine is a stand-in: the process host is the fake one, a
//! turn ends when the test says so, and the check runner answers from a script. No model is called.
//!
//! The crash cases fail the nth journal append and restart on the same directories. Appends are
//! fsynced before they are acknowledged, so the journal a refused append leaves behind is the
//! journal a `kill -9` at that instant leaves behind.
#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use common::{Fixture, ScriptedColumn, ScriptedRunner, SharedHost, TestClock};
use duded::floor_actions::{EnrolledCheck, Enrollment, Profile};
use duded::startup::Witness;
use dudes_proto::record::{RecordedWorkspace, samples};
use dudes_proto::rpc::agent::HireParams;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::ticket::{TicketAssignOutcome, TicketAssignResult, TicketReadResult};
use dudes_proto::{
    Actor, AgentBudget, AgentView, CardView, CheckBaseline, Entry, ExitStatus, Goal, GoalBudget,
    LoopDecision, Predicate, Record, RecordKind, TicketCheck, TicketReason, TicketRefusal,
    TicketWait, TurnOutcome,
};
use dudes_providers::adapter::{HookInstall, ManagedInstall};
use dudes_providers::launch::LaunchSpec;
use dudes_providers::stream::ScopedUsage;
use dudes_rpc::Client;
use dudes_rpc::auth::PeerProvenance;
use dudes_supervisor::Input;
use dudes_supervisor::host::{
    AdoptionKey, HostBoot, KillOutcome, Liveness, ProcessHost, SpawnResult,
};
use dudes_types::{
    AgentId, AgentStatus, CardId, CardStatus, GoalId, IsolationMode, Seq, Timestamp, TurnId, Usage,
    Usd,
};
use serde_json::{Value, json};

type Socket =
    Client<std::io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream>;

/// A process host that notes, at every launch, how many turns were already durable on disk.
struct WatchHost {
    inner: SharedHost,
    journal: PathBuf,
    durable_at_spawn: Arc<Mutex<Vec<usize>>>,
    gate: Option<Arc<Gate>>,
}

/// A launch that parks right after it was recorded, so a test can look at the journal while a
/// turn is durable and not yet spawned.
#[derive(Default)]
struct Gate {
    state: Mutex<(bool, bool)>,
    changed: std::sync::Condvar,
}

impl Gate {
    fn arrive(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.0 = true;
        self.changed.notify_all();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !state.1 && std::time::Instant::now() < deadline {
            state = self
                .changed
                .wait_timeout(state, std::time::Duration::from_millis(100))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn wait_reached(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !self.state.lock().unwrap_or_else(PoisonError::into_inner).0 {
            assert!(
                std::time::Instant::now() < deadline,
                "no launch reached the gate"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn release(&self) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
        self.changed.notify_all();
    }
}

/// What a boot changes about the ordinary stand-in.
#[derive(Default)]
struct Options {
    /// Refuse the nth append after boot, once, as a clean refusal.
    refuse_nth_append: Option<std::num::NonZeroUsize>,
    /// Park the first launch at this gate.
    gate: Option<Arc<Gate>>,
    /// The engine refuses its first launch outright.
    fail_spawn: bool,
}

impl ProcessHost for WatchHost {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        let durable = dudes_journal::Reader::new(&self.journal)
            .iter_from(dudes_types::Lsn::ZERO)
            .expect("the journal reads")
            .filter_map(Result::ok)
            .filter(|(_, entry)| matches!(entry.record, Record::TurnIssued { .. }))
            .count();
        self.durable_at_spawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(durable);
        if let Some(gate) = &self.gate {
            gate.arrive();
        }
        self.inner.spawn(spec, install)
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.inner.kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.inner.alive(key)
    }

    fn boot_identity(&self) -> HostBoot {
        self.inner.boot_identity()
    }

    fn shutdown(&mut self) -> std::io::Result<()> {
        self.inner.shutdown()
    }

    fn write_hooks(&mut self, install: &HookInstall) -> std::io::Result<()> {
        self.inner.write_hooks(install)
    }
}

fn check(id: &str, program: &str) -> EnrolledCheck {
    EnrolledCheck {
        id: id.to_owned(),
        label: format!("{id} passes"),
        predicate: Predicate::Command {
            argv: vec![program.to_owned()],
            timeout_s: 10,
        },
        may_already_pass: true,
    }
}

fn profile(id: &str, cwd: &Path, checks: Vec<EnrolledCheck>) -> Profile {
    Profile {
        id: id.to_owned(),
        label: format!("{id} workspace"),
        checks,
        cwd: cwd.to_path_buf(),
        written_cwd: cwd.to_path_buf(),
        department: "engineering".to_owned(),
        isolation: IsolationMode::Default,
        agent_budget: AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        goal_budget: GoalBudget {
            usd: Some(Usd::from_dollars(5)),
            tokens: None,
        },
        hop_cap: 4,
        mark: None,
    }
}

/// A repository directory under the fixture and settings that enroll it as the project `service`.
/// Its own check passes, `ghost` names a program that does not exist.
fn enrolled(fixture: &Fixture) -> (duded::Settings, PathBuf) {
    let repo = fixture.state.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let repo = repo.canonicalize().unwrap();
    let mut settings = fixture.settings();
    settings.enrollment = Enrollment {
        profiles: vec![profile(
            "service",
            &repo,
            vec![
                check("default", common::true_program()),
                check("ghost", "/no/such/program"),
            ],
        )],
    };
    (settings, repo)
}

struct Flow {
    running: common::Running,
    settings: duded::Settings,
    operator: Socket,
    runner: Arc<ScriptedRunner>,
    host: SharedHost,
    durable_at_spawn: Arc<Mutex<Vec<usize>>>,
    append_failure: duded::runtime::AppendFailure,
    repo: PathBuf,
    column: Arc<ScriptedColumn>,
}

fn credential(settings: &duded::Settings) -> OperatorCredential {
    OperatorCredential::new(
        std::fs::read_to_string(
            settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .unwrap(),
    )
    .unwrap()
}

impl Flow {
    /// Boot over `fixture` with the check runner answering `answers` in order, repeating the last.
    fn boot(
        fixture: &Fixture,
        settings: duded::Settings,
        repo: PathBuf,
        answers: impl IntoIterator<Item = duded::predicate::PredicateOutcome>,
    ) -> Self {
        Self::boot_with(fixture, settings, repo, answers, Options::default())
    }

    fn boot_with(
        fixture: &Fixture,
        settings: duded::Settings,
        repo: PathBuf,
        answers: impl IntoIterator<Item = duded::predicate::PredicateOutcome>,
        options: Options,
    ) -> Self {
        let column = ScriptedColumn::new(PeerProvenance::NoRecordedAgent);
        let host = if options.fail_spawn {
            SharedHost::with(dudes_supervisor::testing::FakeHost {
                fail_spawn: Some(std::io::ErrorKind::PermissionDenied),
                ..dudes_supervisor::testing::FakeHost::wall_clock_anchored()
            })
        } else {
            SharedHost::new()
        };
        let runner = Arc::new(ScriptedRunner::scripted(answers));
        let durable_at_spawn = Arc::new(Mutex::new(Vec::new()));
        let mut seams = common::seams_with(
            WatchHost {
                inner: host.clone(),
                journal: fixture.journal_dir(),
                durable_at_spawn: Arc::clone(&durable_at_spawn),
                gate: options.gate,
            },
            Arc::clone(&runner),
            Arc::new(TestClock::new()),
            Witness::default(),
            Arc::clone(&column) as Arc<dyn dudes_rpc::auth::PeerResolver>,
        );
        for _ in 0..fixture.records().len() * 8 + 64 {
            dudes_supervisor::Rng::ulid(&mut seams.rng);
        }
        let daemon = duded::boot(&settings, seams).unwrap();
        let append_failure = daemon.append_failure();
        if let Some(nth) = options.refuse_nth_append {
            append_failure.fail_nth(nth);
        }
        let running = common::start(daemon);
        let operator = Client::connect(settings.socket()).unwrap();
        Self {
            running,
            settings,
            operator,
            runner,
            host,
            durable_at_spawn,
            append_failure,
            repo,
            column,
        }
    }

    fn call(
        &mut self,
        method: dudes_proto::rpc::Method,
        params: Value,
    ) -> dudes_proto::rpc::Response {
        self.operator
            .call_as_operator(method, Some(params), credential(&self.settings))
            .unwrap()
    }

    fn ok<T: serde::de::DeserializeOwned>(
        &mut self,
        method: dudes_proto::rpc::Method,
        params: Value,
    ) -> T {
        let response = self.call(method, params);
        assert!(response.error.is_none(), "{method:?}: {:?}", response.error);
        serde_json::from_value(response.result.unwrap()).unwrap()
    }

    fn hire(&mut self, name: &str) -> AgentId {
        self.hire_with(name, common::hire_params(&self.repo).budget)
    }

    fn hire_with(&mut self, name: &str, budget: AgentBudget) -> AgentId {
        let params = HireParams {
            name: name.to_owned(),
            department: "engineering".to_owned(),
            budget,
            ..common::hire_params(&self.repo)
        };
        match self
            .running
            .handle
            .ask(duded::ports::Ask::Hire(Box::new(params)))
            .expect("the hire")
        {
            duded::ports::Answered::Hired(result) => result.agent,
            other => panic!("agent.hire answered {other:?}"),
        }
    }

    fn create(&mut self, title: &str) -> CardId {
        self.create_with(json!({"title": title}))
    }

    fn create_with(&mut self, params: Value) -> CardId {
        let created: dudes_proto::rpc::ticket::TicketCreateResult =
            self.ok(dudes_proto::rpc::Method::TicketCreate, params);
        created.card
    }

    fn board(&mut self) -> Vec<CardView> {
        self.ok(dudes_proto::rpc::Method::BoardList, json!({}))
    }

    fn card(&mut self, card: CardId) -> CardView {
        self.board()
            .into_iter()
            .find(|view| view.id == card)
            .expect("the card is on the board")
    }

    fn read(&mut self, card: CardId) -> TicketReadResult {
        self.ok(dudes_proto::rpc::Method::TicketRead, json!({"card": card}))
    }

    fn give(
        &mut self,
        card: CardId,
        agent: AgentId,
        message: Option<&str>,
    ) -> dudes_proto::rpc::Response {
        let seq = self.card(card).seq;
        self.call(
            dudes_proto::rpc::Method::TicketAssign,
            json!({"card": card, "agent": agent, "message": message, "expected_seq": seq}),
        )
    }

    fn give_ok(
        &mut self,
        card: CardId,
        agent: AgentId,
        message: Option<&str>,
    ) -> TicketAssignOutcome {
        let response = self.give(card, agent, message);
        assert!(response.error.is_none(), "{:?}", response.error);
        serde_json::from_value::<TicketAssignResult>(response.result.unwrap())
            .unwrap()
            .outcome
    }

    fn accept(&mut self, card: CardId) -> dudes_proto::rpc::Response {
        let seq = self.card(card).seq;
        self.call(
            dudes_proto::rpc::Method::TicketAccept,
            json!({"card": card, "expected_seq": seq}),
        )
    }

    fn agent(&mut self, agent: AgentId) -> AgentView {
        let agents: Vec<AgentView> = self.ok(dudes_proto::rpc::Method::AgentList, json!({}));
        agents
            .into_iter()
            .find(|view| view.id == agent)
            .expect("the agent is listed")
    }

    fn control(&mut self, method: dudes_proto::rpc::Method, agent: AgentId) {
        self.ok::<Value>(method, json!({"agent": agent}));
    }

    /// Every turn the journal has issued so far, oldest first.
    fn turns(fixture: &Fixture) -> Vec<(TurnId, AgentId, GoalId, String)> {
        fixture
            .records()
            .into_iter()
            .filter_map(|record| match record {
                Record::TurnIssued {
                    turn,
                    agent,
                    goal,
                    task,
                    ..
                } => Some((turn, agent, goal, task)),
                _ => None,
            })
            .collect()
    }

    /// Wait until the journal holds `count` issued turns, and answer the newest.
    fn await_turn_number(fixture: &Fixture, count: usize) -> (TurnId, AgentId, GoalId, String) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let turns = Self::turns(fixture);
            if turns.len() >= count {
                return turns[count - 1].clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "turn {count} was never issued; records: {:?}",
                fixture
                    .records()
                    .iter()
                    .map(Record::kind)
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// The agent's engine finishes `turn` and says `message`.
    fn end_turn(&self, turn: TurnId, message: Option<&str>) {
        self.end_turn_charging(turn, message, None);
    }

    /// As [`Flow::end_turn`], with what the turn cost.
    fn end_turn_charging(&self, turn: TurnId, message: Option<&str>, cost: Option<Usd>) {
        self.running.handle.input(Input::ChildExited {
            final_reply: None,
            elapsed_ms: Some(5),
            turn,
            exit: ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: cost.map(|usd| {
                ScopedUsage::Turn(Usage {
                    usd: Some(usd),
                    ..Usage::ZERO
                })
            }),
            model_usage: BTreeMap::new(),
            outcome: TurnOutcome::Completed,
            last_message: message.map(str::to_owned),
        });
    }

    /// Wait until `card` has `status`, and answer it.
    fn await_status(&mut self, card: CardId, status: CardStatus) -> CardView {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let view = self.card(card);
            if view.status == status {
                return view;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{card} is {:?}, not {status:?}",
                view.status
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn stop(mut self) {
        drop(self.operator);
        self.running.stop(duded::ExitCause::ShutdownRpc);
    }
}

fn kinds(fixture: &Fixture) -> Vec<RecordKind> {
    fixture.records().iter().map(Record::kind).collect()
}

fn position_of(records: &[Record], wanted: impl Fn(&Record) -> bool) -> usize {
    records.iter().position(wanted).unwrap_or_else(|| {
        panic!(
            "no such record in {:?}",
            records.iter().map(Record::kind).collect::<Vec<_>>()
        )
    })
}

fn started_goals(records: &[Record]) -> Vec<(CardId, AgentId, Goal)> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::TicketStarted {
                card, agent, goal, ..
            } => Some((*card, *agent, goal.clone())),
            _ => None,
        })
        .collect()
}

fn transitions(records: &[Record], card: CardId) -> Vec<(CardStatus, CardStatus, Actor)> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::CardTransition {
                card: moved,
                from,
                to,
                by,
                ..
            } if *moved == card => Some((*from, *to, *by)),
            _ => None,
        })
        .collect()
}

fn queued(outcome: &TicketAssignOutcome) -> (u32, TicketWait, &str) {
    match outcome {
        TicketAssignOutcome::Queued { position, waiting } => {
            (*position, waiting.wait, waiting.note.as_str())
        }
        other => panic!("expected a queued ticket, got {other:?}"),
    }
}

fn refusal_reason(response: &dudes_proto::rpc::Response) -> String {
    response
        .error
        .as_ref()
        .and_then(|error| error.data.as_ref())
        .and_then(|data| data.get("reason"))
        .and_then(Value::as_str)
        .expect("a ticket refusal names its reason")
        .to_owned()
}

/// An idle agent in a free project starts the ticket on the same fold that records the
/// assignment. The journal shows the assignment, the start with the daemon-chosen baseline, the
/// loop's decision and the fsynced turn, in that order, and no check ran before the turn.
#[test]
fn giving_a_ticket_to_an_idle_agent_starts_it_at_once() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    let agent = flow.hire("Cody");
    let card = flow
        .create_with(json!({"title": "Fix the parser", "description": "Cover the empty input."}));
    assert_eq!(flow.agent(agent).status, AgentStatus::Idle);

    let outcome = flow.give_ok(card, agent, None);
    assert_eq!(outcome, TicketAssignOutcome::Started);

    let (turn, who, goal_id, task) = Flow::await_turn_number(&fixture, 1);
    assert_eq!(who, agent);
    assert!(
        task.starts_with("Fix the parser\n\nCover the empty input."),
        "{task}"
    );
    assert_eq!(flow.agent(agent).status, AgentStatus::Running);
    let view = flow.card(card);
    assert_eq!(view.status, CardStatus::Doing);
    assert_eq!(view.status_by, Actor::System);
    assert_eq!(view.claimed_by, Some(agent));
    assert_eq!(flow.read(card).ticket.goal, Some(goal_id));
    assert_eq!(flow.read(card).ticket.round, 1);

    let records = fixture.records();
    let assigned = position_of(&records, |r| matches!(r, Record::TicketAssigned { .. }));
    let started = position_of(&records, |r| matches!(r, Record::TicketStarted { .. }));
    let decided = position_of(&records, |r| {
        matches!(
            r,
            Record::LoopDecided {
                decision: LoopDecision::Continue { .. },
                ..
            }
        )
    });
    let issued = position_of(&records, |r| matches!(r, Record::TurnIssued { .. }));
    assert!(
        assigned < started && started < decided && decided < issued,
        "{:?}",
        kinds(&fixture)
    );

    let goals = started_goals(&records);
    assert_eq!(goals.len(), 1);
    let (started_card, started_agent, goal) = &goals[0];
    assert_eq!(
        (*started_card, *started_agent, goal.id),
        (card, agent, goal_id)
    );
    assert_eq!(goal.baseline, Some(CheckBaseline::MayAlreadyPass));
    assert_eq!(goal.text, "Fix the parser\n\nCover the empty input.");
    let ticket_check = flow.read(card).ticket.done_when;
    assert_eq!(goal.predicate, ticket_check.predicate);
    assert_eq!(
        goal.completion_check,
        duded::ticket_goal_check(&ticket_check)
    );
    assert_eq!(
        (goal.hop_cap, goal.budget.usd),
        (4, Some(Usd::from_dollars(5)))
    );

    assert_eq!(flow.runner.runs(), 0, "no check runs before the first turn");
    assert_eq!(
        *flow.durable_at_spawn.lock().unwrap(),
        [1],
        "the turn was durable when its process launched"
    );
    assert_eq!(flow.host.spawned().len(), 1);
    let _ = turn;
    flow.stop();
}

/// The agent's own words move nothing. A failing check after its turn leaves the ticket Working
/// and the loop issues another turn. The daemon's own passing run, after a later turn, moves the
/// ticket to Ready for you, authored by the daemon. The summary and check result are the next
/// step's work.
#[test]
fn the_agents_word_moves_nothing_and_the_daemons_pass_does() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(
        &fixture,
        settings,
        repo,
        [common::fails(), common::passes()],
    );
    let agent = flow.hire("Cody");
    let card = flow.create("Fix the parser");
    flow.give_ok(card, agent, None);

    let (first, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(first, Some("All done. The check passes now."));
    let (second, ..) = Flow::await_turn_number(&fixture, 2);
    assert_eq!(
        flow.card(card).status,
        CardStatus::Doing,
        "a claim of success with a failing check moves nothing"
    );
    assert!(transitions(&fixture.records(), card).is_empty());

    flow.end_turn(second, Some("Fixed."));
    let view = flow.await_status(card, CardStatus::Review);
    assert_eq!(view.status_by, Actor::System);
    let records = fixture.records();
    let moves = transitions(&records, card);
    assert_eq!(
        moves,
        [(CardStatus::Doing, CardStatus::Review, Actor::System)]
    );
    let pass = position_of(&records, |r| {
        matches!(r, Record::PredicateResult { pass: true, .. })
    });
    let done = position_of(&records, |r| {
        matches!(
            r,
            Record::LoopDecided {
                decision: LoopDecision::Done,
                ..
            }
        )
    });
    let ready = position_of(&records, |r| matches!(r, Record::CardTransition { .. }));
    assert!(
        pass < done && done < ready,
        "Ready for you follows the loop's Done: {:?}",
        kinds(&fixture)
    );
    flow.stop();
}

/// Ready for you needs the loop's verdict, not just a passing run. A paused agent's passing check
/// is journaled, the control gate holds the loop before Done, and the ticket stays Working until
/// the agent is resumed.
#[test]
fn a_passing_check_on_a_paused_agent_moves_nothing_until_the_loop_says_done() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    let agent = flow.hire("Cody");
    let card = flow.create("Fix the parser");
    flow.give_ok(card, agent, None);
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);

    flow.control(dudes_proto::rpc::Method::LoopPause, agent);
    flow.end_turn(turn, None);
    fixture.await_record("PredicateResult", |r| {
        matches!(r, Record::PredicateResult { pass: true, .. }).then_some(())
    });
    let decided = fixture.records().len();
    let after = settled(&fixture, decided);
    assert!(
        after.iter().any(|r| matches!(
            r,
            Record::LoopDecided {
                decision: LoopDecision::Hold { .. },
                ..
            }
        )),
        "the control gate held the loop before Done"
    );
    assert_eq!(flow.card(card).status, CardStatus::Doing);
    assert!(
        !kinds(&fixture).contains(&RecordKind::CardTransition),
        "no move before the loop decides Done: {:?}",
        kinds(&fixture)
    );

    flow.control(dudes_proto::rpc::Method::LoopResume, agent);
    flow.await_status(card, CardStatus::Review);
    flow.stop();
}

/// A check that already passes still takes one turn and an observed pass after it, in the first
/// round and in a round that sends the work back. Neither refuses the ticket, and neither runs
/// the check before the turn.
#[test]
fn a_check_that_already_passes_still_needs_a_turn_in_every_round() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    let agent = flow.hire("Cody");
    let card = flow.create("Fix the parser");

    assert_eq!(
        flow.give_ok(card, agent, None),
        TicketAssignOutcome::Started
    );
    assert_eq!(flow.runner.runs(), 0);
    assert_eq!(flow.card(card).status, CardStatus::Doing);
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(turn, Some("Nothing needed changing."));
    flow.await_status(card, CardStatus::Review);
    assert_eq!(flow.runner.runs(), 1);

    let outcome = flow.give_ok(card, agent, Some("Please add a test."));
    assert_eq!(outcome, TicketAssignOutcome::Started);
    assert_eq!(flow.card(card).status, CardStatus::Doing);
    assert_eq!(flow.read(card).ticket.round, 2);
    let (turn, _, goal, task) = Flow::await_turn_number(&fixture, 2);
    assert!(
        task.starts_with(
            "Fix the parser\n\nThe operator reviewed the previous work and asked for changes:\nPlease add a test."
        ),
        "{task}"
    );
    let goals = started_goals(&fixture.records());
    assert_eq!(goals.len(), 2);
    assert_eq!(goals[1].2.baseline, Some(CheckBaseline::MayAlreadyPass));
    assert_eq!(goals[1].2.id, goal);
    assert_ne!(goals[0].2.id, goals[1].2.id, "a fresh goal for every round");
    assert_eq!(
        flow.runner.runs(),
        1,
        "the second round has not run its check before its turn"
    );
    assert_eq!(
        flow.read(card).ticket.message,
        None,
        "the message is spent by the start"
    );

    flow.end_turn(turn, None);
    flow.await_status(card, CardStatus::Review);
    assert_eq!(flow.runner.runs(), 2);
    let moves = transitions(&fixture.records(), card);
    assert_eq!(moves.len(), 2);
    flow.stop();
}

/// Two agents hired into one project take one ticket at a time. The second ticket waits with a
/// note that names the first, keeps waiting while the first is Ready for you, and starts on the
/// fold that records Accept. Accept is authored by the operator and refuses a ticket in any
/// other state.
#[test]
fn a_project_runs_one_ticket_and_accept_starts_the_next() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    let cody = flow.hire("Cody");
    let dana = flow.hire("Dana");
    let first = flow.create("Fix the parser");
    let second = flow.create("Add the lexer");

    assert_eq!(
        flow.give_ok(first, cody, None),
        TicketAssignOutcome::Started
    );
    let outcome = flow.give_ok(second, dana, None);
    let (position, wait, note) = queued(&outcome);
    assert_eq!(position, 1);
    assert_eq!(wait, TicketWait::Ticket { card: first });
    assert_eq!(
        note,
        "Waiting for Fix the parser to be finished, accepted or sent back."
    );
    assert_eq!(flow.read(second).waiting.unwrap().wait, wait);

    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(turn, Some("Done."));
    flow.await_status(first, CardStatus::Review);
    assert_eq!(
        flow.card(second).status,
        CardStatus::Claimed,
        "a ticket that is Ready for you holds its project"
    );
    assert_eq!(Flow::turns(&fixture).len(), 1);

    let wrong = flow.accept(second);
    assert_eq!(refusal_reason(&wrong), TicketRefusal::WrongState.code());
    let seq = flow.card(first).seq;
    let stale = flow.call(
        dudes_proto::rpc::Method::TicketAccept,
        json!({"card": first, "expected_seq": seq.0 + 7}),
    );
    assert_eq!(
        stale.error.unwrap().code,
        dudes_proto::rpc::ErrorCode::Conflict.code()
    );

    let accepted = flow.accept(first);
    assert!(accepted.error.is_none(), "{:?}", accepted.error);
    let done = flow.card(first);
    assert_eq!(done.status, CardStatus::Done);
    assert_eq!(
        done.status_by,
        Actor::Peer {
            pid: std::process::id(),
            uid: None,
        },
        "accepted by the operator's own process"
    );
    assert_eq!(
        flow.card(second).status,
        CardStatus::Doing,
        "Accept started the next ticket"
    );
    let (_, who, ..) = Flow::await_turn_number(&fixture, 2);
    assert_eq!(who, dana);
    let records = fixture.records();
    let accepted_at = position_of(&records, |r| {
        matches!(
            r,
            Record::CardTransition {
                to: CardStatus::Done,
                ..
            }
        )
    });
    let started_at = records
        .iter()
        .rposition(|r| matches!(r, Record::TicketStarted { .. }))
        .unwrap();
    assert!(accepted_at < started_at);
    flow.stop();
}

/// One agent with three tickets works them in the order they were given. While the first is
/// Working the others wait with positions 1 and 2 and a note naming the agent. Sending the first
/// back after review puts it ahead of the ticket that was only waiting.
#[test]
fn a_busy_agent_queues_in_order_and_sent_back_work_goes_first() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    let cody = flow.hire("Cody");
    let (one, two, three) = (flow.create("One"), flow.create("Two"), flow.create("Three"));

    assert_eq!(flow.give_ok(one, cody, None), TicketAssignOutcome::Started);
    let waiting_two = flow.give_ok(two, cody, None);
    let (position, wait, note) = queued(&waiting_two);
    assert_eq!((position, wait), (1, TicketWait::AgentBusy));
    assert_eq!(note, "Waiting for Cody to finish its current work.");
    let waiting_three = flow.give_ok(three, cody, None);
    assert_eq!(queued(&waiting_three).0, 2);

    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(turn, None);
    flow.await_status(one, CardStatus::Review);
    assert_eq!(flow.card(two).status, CardStatus::Claimed);
    assert_eq!(
        flow.read(two).waiting.unwrap().wait,
        TicketWait::Ticket { card: one }
    );

    assert!(flow.accept(one).error.is_none());
    assert_eq!(
        flow.card(two).status,
        CardStatus::Doing,
        "the earlier assignment starts first"
    );
    assert_eq!(flow.card(three).status, CardStatus::Claimed);

    let (turn, ..) = Flow::await_turn_number(&fixture, 2);
    flow.end_turn(turn, None);
    flow.await_status(two, CardStatus::Review);
    let outcome = flow.give_ok(two, cody, Some("Rename it."));
    assert_eq!(outcome, TicketAssignOutcome::Started);
    assert_eq!(
        flow.card(three).status,
        CardStatus::Claimed,
        "sent-back work goes ahead of new work"
    );
    let (_, _, _, task) = Flow::await_turn_number(&fixture, 3);
    assert!(task.starts_with("Two\n\n"), "{task}");
    assert!(task.contains("Rename it."));
    flow.stop();
}

/// An edit made while a ticket waits is the text it starts with.
#[test]
fn an_edit_to_a_queued_ticket_is_used_when_it_starts() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    let cody = flow.hire("Cody");
    let (one, two) = (flow.create("One"), flow.create("Two"));
    flow.give_ok(one, cody, None);
    flow.give_ok(two, cody, None);

    let seq = flow.card(two).seq;
    flow.ok::<Value>(
        dudes_proto::rpc::Method::TicketEdit,
        json!({"card": two, "title": "Two, renamed", "description": "With the new note.", "expected_seq": seq}),
    );
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(turn, None);
    flow.await_status(one, CardStatus::Review);
    flow.accept(one);
    assert_eq!(flow.card(two).status, CardStatus::Doing);
    let (.., task) = Flow::await_turn_number(&fixture, 2);
    assert!(
        task.starts_with("Two, renamed\n\nWith the new note."),
        "{task}"
    );
    flow.stop();
}

/// A paused agent keeps the ticket in its queue with a note that names the agent. Resume starts it.
/// An agent that has a goal of its own is not given a second one until that goal is finished.
#[test]
fn a_held_agent_starts_nothing_and_a_goal_of_its_own_is_kept() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    let cody = flow.hire("Cody");
    let card = flow.create("Fix the parser");
    flow.control(dudes_proto::rpc::Method::LoopPause, cody);
    let outcome = flow.give_ok(card, cody, None);
    let (position, wait, note) = queued(&outcome);
    assert_eq!((position, wait), (1, TicketWait::AgentPaused));
    assert_eq!(note, "Waiting for Cody to be resumed.");
    assert!(started_goals(&fixture.records()).is_empty());
    flow.control(dudes_proto::rpc::Method::LoopResume, cody);
    flow.await_status(card, CardStatus::Doing);
    flow.stop();

    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    let dana = flow.hire("Dana");
    let own = common::set_goal(
        &flow.running.handle,
        common::goal_params(dana, &["/usr/bin/false"]),
    );
    let card = flow.create("Fix the parser");
    let outcome = flow.give_ok(card, dana, None);
    assert_eq!(queued(&outcome).1, TicketWait::AgentBusy);
    assert!(started_goals(&fixture.records()).is_empty());
    let held = flow.agent(dana);
    assert_eq!(
        held.goal.map(|goal| goal.id),
        Some(own),
        "the operator's goal is not replaced"
    );
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(turn, None);
    flow.await_status(card, CardStatus::Doing);
    assert_eq!(
        started_goals(&fixture.records()).len(),
        1,
        "it started once the goal was done"
    );
    flow.stop();
}

/// A goal that ran out of turns is not finished: the agent still holds it, and a ticket given to
/// that agent waits instead of replacing it.
#[test]
fn an_agent_whose_own_goal_ran_out_is_not_given_a_second_goal() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    let dana = flow.hire("Dana");
    let own = common::set_goal(
        &flow.running.handle,
        dudes_proto::rpc::goal::GoalSetParams {
            hop_cap: 1,
            ..common::goal_params(dana, &["/usr/bin/false"])
        },
    );
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(turn, None);
    fixture.await_record("an Exhausted decision", |record| {
        matches!(
            record,
            Record::LoopDecided {
                decision: LoopDecision::Exhausted { .. },
                ..
            }
        )
        .then_some(())
    });
    assert_eq!(flow.agent(dana).status, AgentStatus::Exhausted);

    let card = flow.create("Fix the parser");
    let outcome = flow.give_ok(card, dana, None);
    assert_eq!(queued(&outcome).1, TicketWait::AgentBusy);
    assert!(started_goals(&fixture.records()).is_empty());
    assert_eq!(
        flow.agent(dana).goal.map(|goal| goal.id),
        Some(own),
        "the goal that ran out is not replaced"
    );
    flow.stop();
}

/// A check whose program cannot run moves the ticket to Needs you with the reason and the next
/// step, appends no goal, and leaves the agent free for the next ticket.
#[test]
fn a_check_that_cannot_run_sends_the_ticket_to_needs_you() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    let cody = flow.hire("Cody");
    let broken = flow.create_with(json!({"title": "Broken check", "done_when": "ghost"}));
    let fine = flow.create("Fine");

    let outcome = flow.give_ok(broken, cody, None);
    assert_eq!(
        outcome,
        TicketAssignOutcome::NeedsYou {
            reason: TicketReason::CheckCannotRun,
            note: CHECK_CANNOT_RUN.to_owned(),
        }
    );
    let view = flow.card(broken);
    assert_eq!(view.status, CardStatus::Blocked);
    assert_eq!(view.status_by, Actor::System);
    let detail = flow.read(broken);
    assert_eq!(detail.ticket.reason, Some(TicketReason::CheckCannotRun));
    assert_eq!(detail.ticket.reason.unwrap().sentence(), CHECK_CANNOT_RUN);
    assert_eq!(detail.ticket.queue, None, "it left the queue");
    assert!(started_goals(&fixture.records()).is_empty());

    assert_eq!(flow.give_ok(fine, cody, None), TicketAssignOutcome::Started);
    flow.stop();
}

const CHECK_CANNOT_RUN: &str = "The check's program was not found. Install it, then Give to agent. If the check itself is wrong, Remove this ticket and create it again.";
const PROJECT_UNAVAILABLE: &str = "This project is not set up any more. Enroll it again with `dudes workspace add`, then Give to agent, or Remove the ticket.";
const CEILING_REQUIRED: &str = "Set a spending limit first, because your account bills per use. Add one to the project's enrollment (`dudes workspace add`) or the agent's hire, then Give to agent.";
const RESERVE_MISSING: &str = "A per-turn spending reserve is missing. Run `dudes config set budget.per_turn_floor.usd <dollars>` (or `budget.per_turn_floor.tokens <count>`), then Give to agent.";

/// A ticket the daemon moved to Needs you at boot: its reason, the literal sentence the operator
/// reads, and that it left the queue.
fn assert_needs_you(flow: &mut Flow, card: CardId, reason: TicketReason, sentence: &str) {
    let view = flow.card(card);
    assert_eq!(
        (view.status, view.status_by),
        (CardStatus::Blocked, Actor::System)
    );
    let ticket = flow.read(card).ticket;
    assert_eq!(ticket.reason, Some(reason));
    assert_eq!(reason.sentence(), sentence);
    assert_eq!(ticket.queue, None, "it left the queue");
}

fn seeded_hire(project: &str, repo: &Path, budget: Option<AgentBudget>) -> Record {
    let mut hired = samples::record(RecordKind::AgentHired);
    let Record::AgentHired {
        cwd,
        workspace,
        budget: hired_budget,
        sandbox,
        isolation,
        max_turns,
        max_budget,
        trusted,
        mcp_servers,
        ..
    } = &mut hired
    else {
        unreachable!("the sample is a hire");
    };
    *sandbox = None;
    *isolation = None;
    *max_turns = None;
    *max_budget = None;
    *trusted = false;
    mcp_servers.clear();
    *cwd = repo.to_path_buf();
    *workspace = Some(RecordedWorkspace {
        id: project.into(),
        label: "Service".into(),
    });
    if let Some(budget) = budget {
        *hired_budget = budget;
    }
    hired
}

fn sample_agent() -> AgentId {
    let Record::AgentHired { id, .. } = samples::record(RecordKind::AgentHired) else {
        unreachable!("the sample is a hire");
    };
    id
}

fn operator() -> Actor {
    Actor::Peer { pid: 1, uid: None }
}

fn default_check() -> TicketCheck {
    TicketCheck {
        id: "default".into(),
        label: "default passes".into(),
        predicate: Predicate::Command {
            argv: vec![common::true_program().into()],
            timeout_s: 10,
        },
    }
}

/// The records a daemon leaves when a ticket was given to an agent and nothing else happened.
fn given(project: &str, repo: &Path, budget: Option<AgentBudget>, card: CardId) -> Vec<Record> {
    vec![
        seeded_hire(project, repo, budget),
        Record::TicketCreated {
            card,
            title: "Fix the parser".into(),
            description: String::new(),
            department: "engineering".into(),
            project: project.into(),
            done_when: default_check(),
            by: operator(),
        },
        Record::TicketAssigned {
            card,
            agent: sample_agent(),
            by: operator(),
            message: None,
            expected_seq: Seq(0),
        },
    ]
}

fn seed(fixture: &Fixture, records: Vec<Record>) {
    let settings = fixture.settings();
    let (mut writer, _) = dudes_journal::open(&fixture.journal_dir(), settings.journal).unwrap();
    for record in records {
        writer
            .append(&Entry {
                at: Timestamp::from_unix_millis(1_000),
                record,
            })
            .unwrap();
    }
}

/// A restart finds a ticket that is already working and starts nothing: one start, whatever
/// the number of restarts, and the ticket is still Working.
#[test]
fn a_restart_does_not_start_a_working_ticket_again() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings.clone(), repo.clone(), [common::fails()]);
    let cody = flow.hire("Cody");
    let card = flow.create("Fix the parser");
    assert_eq!(flow.give_ok(card, cody, None), TicketAssignOutcome::Started);
    flow.stop();

    for _ in 0..2 {
        let mut flow = Flow::boot(&fixture, settings.clone(), repo.clone(), [common::fails()]);
        assert_eq!(flow.card(card).status, CardStatus::Doing);
        flow.create("A later ticket");
        flow.stop();
        assert_eq!(
            started_goals(&fixture.records()).len(),
            1,
            "one start in all"
        );
    }
}

/// An agent whose hire did not record a project is never given a ticket to start, even when the
/// journal says the ticket was queued for it. The daemon neither starts it nor stops.
#[test]
fn an_agent_hired_without_a_project_starts_nothing() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let card = CardId::from_parts(4, 1);
    let mut records = given("service", &repo, None, card);
    let Record::AgentHired { workspace, .. } = &mut records[0] else {
        unreachable!("the first record is the hire");
    };
    *workspace = None;
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    assert_eq!(flow.card(card).status, CardStatus::Claimed);
    let waiting = flow.read(card).waiting.expect("it waits");
    assert_eq!(waiting.wait, TicketWait::AgentBusy);
    assert!(started_goals(&fixture.records()).is_empty());
    let other = flow.create("Still works");
    assert!(
        flow.card(other).seq == Seq::ZERO,
        "the daemon still answers"
    );
    flow.stop();
}

/// The reasons that only show at boot, because the hire that makes them possible cannot be
/// made on a running daemon: a project that is no longer enrolled, a spending limit that is
/// required and absent, and a per-turn reserve that is not set. Each moves a queued ticket to
/// Needs you when the daemon starts, with the sentence for its reason, and starts nothing.
#[test]
fn a_ticket_that_cannot_start_at_boot_goes_to_needs_you_with_its_reason() {
    let card = CardId::from_parts(4, 1);
    let no_ceiling = AgentBudget {
        usd_per_day: None,
        tokens_per_day: None,
    };

    let fixture = Fixture::new();
    let (mut settings, repo) = enrolled(&fixture);
    settings.enrollment = Enrollment {
        profiles: Vec::new(),
    };
    seed(&fixture, given("service", &repo, None, card));
    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    assert_needs_you(
        &mut flow,
        card,
        TicketReason::ProjectUnavailable,
        PROJECT_UNAVAILABLE,
    );
    flow.stop();

    let fixture = Fixture::new();
    let (mut settings, repo) = enrolled(&fixture);
    settings.floor_usd_per_day = None;
    settings
        .daemon_env
        .push(dudes_providers::launch::EnvVar::new(
            "ANTHROPIC_API_KEY",
            "fixture-value-not-a-credential",
        ));
    settings.enrollment.profiles[0].goal_budget = GoalBudget {
        usd: None,
        tokens: None,
    };
    seed(&fixture, given("service", &repo, Some(no_ceiling), card));
    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    assert_needs_you(
        &mut flow,
        card,
        TicketReason::CeilingRequired,
        CEILING_REQUIRED,
    );
    flow.stop();

    let fixture = Fixture::new();
    let (mut settings, repo) = enrolled(&fixture);
    settings.floor_usd_per_day = None;
    settings.per_turn_floor = dudes_proto::Spend {
        usd: Usd::ZERO,
        tokens: dudes_types::Tokens::ZERO,
        work_tokens: dudes_types::Tokens::ZERO,
    };
    seed(&fixture, given("service", &repo, Some(no_ceiling), card));
    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    assert_needs_you(
        &mut flow,
        card,
        TicketReason::ReserveMissing,
        RESERVE_MISSING,
    );
    assert!(started_goals(&fixture.records()).is_empty());
    flow.stop();
}

/// The journal a daemon wrote when it started a ticket, and the journal of a goal set on the
/// same agent with the same goal, replay to the same agent, the same goal and the same first task.
#[test]
fn a_ticket_start_and_a_goal_set_replay_to_the_same_world() {
    let source = Fixture::new();
    let (settings, repo) = enrolled(&source);
    let mut flow = Flow::boot(&source, settings, repo, [common::fails()]);
    let cody = flow.hire("Cody");
    let card = flow.create("Fix the parser");
    assert_eq!(flow.give_ok(card, cody, None), TicketAssignOutcome::Started);
    flow.stop();
    let live = source.records();
    assert!(
        matches!(live[0], Record::AgentHired { .. }),
        "the journal starts with the hire"
    );
    let start = live
        .iter()
        .position(|r| matches!(r, Record::TicketStarted { .. }))
        .expect("the daemon started the ticket");
    let Record::TicketStarted { agent, goal, .. } = live[start].clone() else {
        unreachable!("found by its kind");
    };

    let mut worlds = Vec::new();
    for ticket in [true, false] {
        let fixture = Fixture::new();
        let (settings, repo) = enrolled(&fixture);
        let records = if ticket {
            live[..=start].to_vec()
        } else {
            vec![
                live[0].clone(),
                Record::CardCreated {
                    card,
                    title: "Fix the parser".into(),
                    department: "engineering".into(),
                    by: operator(),
                    deps: Vec::new(),
                },
                Record::Claim {
                    card,
                    agent,
                    expected_seq: Seq(0),
                },
                Record::GoalSet {
                    agent,
                    goal: goal.clone(),
                    browser_onboarding: None,
                    browser_goal: None,
                    goal_replacement: None,
                },
                Record::CardTransition {
                    card,
                    from: CardStatus::Claimed,
                    to: CardStatus::Doing,
                    by: Actor::System,
                    expected_seq: Seq(1),
                    reason: None,
                },
            ]
        };
        seed(&fixture, records);
        let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
        let (_, who, goal_id, task) = Flow::await_turn_number(&fixture, 1);
        let board = flow.board();
        worlds.push((flow.agent(agent), who, goal_id, task, board));
        flow.stop();
    }
    assert_eq!(worlds[0], worlds[1]);
    assert_eq!(worlds[0].2, goal.id);
    assert_eq!(worlds[0].4[0].status, CardStatus::Doing);
}

/// How many appends after position `from` the journal needs to reach the first record `target`
/// names, in a run that was not interrupted.
fn appends_between(records: &[Record], from: usize, target: impl Fn(&Record) -> bool) -> usize {
    records[from..]
        .iter()
        .position(target)
        .map(|index| index + 1)
        .expect("the reference run reached the target record")
}

/// Wait until the journal holds at least `expected` records, then until it stops growing, which
/// is when a daemon whose append was refused has stopped writing.
fn settled(fixture: &Fixture, expected: usize) -> Vec<Record> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while fixture.records().len() < expected {
        assert!(
            std::time::Instant::now() < deadline,
            "the journal never reached {expected} records"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut seen = fixture.records().len();
    let mut quiet = 0;
    while quiet < 5 {
        std::thread::sleep(std::time::Duration::from_millis(60));
        let now = fixture.records().len();
        quiet = if now == seen { quiet + 1 } else { 0 };
        seen = now;
        assert!(
            std::time::Instant::now() < deadline,
            "the journal never settled"
        );
    }
    fixture.records()
}

/// Run the story once to learn which append is `target`, again failing that append, and then
/// restart on the same directories and check what the restart made of it.
fn crash_and_restart(target: impl Fn(&Record) -> bool, after_restart: impl Fn(&[Record])) {
    let reference = Fixture::new();
    let (settings, repo) = enrolled(&reference);
    let mut flow = Flow::boot(&reference, settings, repo, [common::passes()]);
    let cody = flow.hire("Cody");
    let card = flow.create("Fix the parser");
    let before = reference.records().len();
    flow.give_ok(card, cody, None);
    Flow::await_turn_number(&reference, 1);
    let nth = appends_between(&reference.records(), before, &target);
    flow.stop();

    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings.clone(), repo.clone(), [common::passes()]);
    let cody = flow.hire("Cody");
    let card = flow.create("Fix the parser");
    let before = fixture.records().len();
    flow.append_failure.short_write_after(nth - 1, 5);
    let _ = flow.give(card, cody, None);
    let crashed = settled(&fixture, before + nth - 1);
    assert_eq!(
        crashed.len(),
        before + nth - 1,
        "the failed append and everything after it are not in the journal"
    );
    assert!(!crashed[before..].iter().any(&target));
    flow.stop();

    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    Flow::await_turn_number(&fixture, 1);
    let _ = flow.card(card);
    let records = settled(&fixture, crashed.len() + 1);
    flow.stop();
    after_restart(&records);
}

/// Crash after the assignment is durable and before the start: the restart finds the ticket in
/// the queue and starts it once.
#[test]
fn a_crash_between_the_assignment_and_the_start_starts_the_ticket_once() {
    crash_and_restart(
        |r| matches!(r, Record::TicketStarted { .. }),
        |records| {
            assert_eq!(started_goals(records).len(), 1, "one start");
            let assigned = records
                .iter()
                .filter(|r| matches!(r, Record::TicketAssigned { .. }))
                .count();
            assert_eq!(assigned, 1);
        },
    );
}

/// Crash after the start and before the first turn is durable: the restart decides again and
/// issues exactly one turn, with no second start.
#[test]
fn a_crash_between_the_start_and_the_turn_issues_one_turn() {
    crash_and_restart(
        |r| matches!(r, Record::TurnIssued { .. }),
        |records| {
            assert_eq!(started_goals(records).len(), 1, "one start");
            let issued = records
                .iter()
                .filter(|r| matches!(r, Record::TurnIssued { .. }))
                .count();
            assert_eq!(issued, 1, "one turn, no duplicate");
        },
    );
}

/// Crash after the loop latched Done and before the move to Ready for you is appended: the
/// restart owes the move, makes it once, and issues no turn.
#[test]
fn a_crash_after_a_passing_check_and_before_ready_for_you_moves_the_ticket_once() {
    let moved = |r: &Record| matches!(r, Record::CardTransition { .. });
    let drive = |fixture: &Fixture, flow: &mut Flow| {
        let cody = flow.hire("Cody");
        let card = flow.create("Fix the parser");
        flow.give_ok(card, cody, None);
        let (turn, ..) = Flow::await_turn_number(fixture, 1);
        (card, turn)
    };

    let reference = Fixture::new();
    let (settings, repo) = enrolled(&reference);
    let mut flow = Flow::boot(&reference, settings, repo, [common::passes()]);
    let (card, turn) = drive(&reference, &mut flow);
    let before = reference.records().len();
    flow.end_turn(turn, Some("Done."));
    flow.await_status(card, CardStatus::Review);
    let nth = appends_between(&reference.records(), before, moved);
    flow.stop();

    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings.clone(), repo.clone(), [common::passes()]);
    let (card, turn) = drive(&fixture, &mut flow);
    let before = fixture.records().len();
    flow.append_failure.short_write_after(nth - 1, 5);
    flow.end_turn(turn, Some("Done."));
    let crashed = settled(&fixture, before + nth - 1);
    assert_eq!(crashed.len(), before + nth - 1);
    assert!(
        crashed[before..].iter().any(|r| matches!(
            r,
            Record::LoopDecided {
                decision: LoopDecision::Done,
                ..
            }
        )),
        "Done is latched and the move is not: {:?}",
        crashed[before..]
            .iter()
            .map(Record::kind)
            .collect::<Vec<_>>()
    );
    assert!(!crashed[before..].iter().any(&moved));
    flow.stop();

    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    let view = flow.await_status(card, CardStatus::Review);
    assert_eq!(view.status_by, Actor::System);
    let records = settled(&fixture, crashed.len() + 1);
    flow.stop();
    assert_eq!(
        transitions(&records, card),
        [(CardStatus::Doing, CardStatus::Review, Actor::System)],
        "one move to Ready for you"
    );
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r, Record::TurnIssued { .. }))
            .count(),
        1,
        "no turn follows a latched Done"
    );
}

/// A ticket given back from Ready for you is not sent to Ready for you again on the previous
/// round's pass. When its start is refused it ends in Needs you with the reason, and nothing
/// moves it to Ready for you until a new start has a turn and an observed pass of its own.
#[test]
fn a_reworked_ticket_whose_start_is_refused_does_not_return_on_the_old_pass() {
    let card = CardId::from_parts(4, 1);
    let agent = sample_agent();
    let fixture = Fixture::new();
    let (mut settings, repo) = enrolled(&fixture);
    settings.enrollment = Enrollment {
        profiles: Vec::new(),
    };
    let check = default_check();
    let first = Goal {
        id: GoalId::from_parts(8, 1),
        text: "Fix the parser".into(),
        completion_check: duded::ticket_goal_check(&check),
        predicate: check.predicate.clone(),
        baseline: Some(CheckBaseline::MayAlreadyPass),
        budget: GoalBudget {
            usd: None,
            tokens: None,
        },
        hop_cap: 4,
    };
    let mut records = given("service", &repo, None, card);
    records.extend([
        Record::TicketStarted {
            card,
            agent,
            goal: first.clone(),
            expected_seq: Seq(1),
        },
        Record::PredicateResult {
            goal: first.id,
            turn: None,
            provenance: dudes_types::Provenance::Observed,
            pass: true,
            evidence: dudes_proto::goal::Evidence::Command {
                argv: vec![common::true_program().into()],
                program: None,
                exit: Some(0),
                output_tail: String::new(),
            },
        },
        Record::LoopDecided {
            agent,
            input_digest: String::new(),
            decision: LoopDecision::Done,
            gate_trace: Vec::new(),
        },
        Record::CardTransition {
            card,
            from: CardStatus::Doing,
            to: CardStatus::Review,
            by: Actor::System,
            expected_seq: Seq(2),
            reason: None,
        },
        Record::TicketAssigned {
            card,
            agent,
            by: operator(),
            message: Some("again".into()),
            expected_seq: Seq(3),
        },
    ]);
    seed(&fixture, records);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::passes()]);
    assert_needs_you(
        &mut flow,
        card,
        TicketReason::ProjectUnavailable,
        PROJECT_UNAVAILABLE,
    );
    let after = settled(&fixture, fixture.records().len());
    flow.stop();
    assert_eq!(
        transitions(&after, card),
        [
            (CardStatus::Doing, CardStatus::Review, Actor::System),
            (CardStatus::Claimed, CardStatus::Blocked, Actor::System),
        ],
        "the old pass moved the ticket once, and the refused start moved it to Needs you"
    );
}

/// A start that fails does not keep another project from starting its own ticket, and the failed
/// one is offered again on the next job.
#[test]
fn a_failed_start_does_not_stall_the_other_projects() {
    let fixture = Fixture::new();
    let (mut settings, repo) = enrolled(&fixture);
    let docs = fixture.state.path().join("docs");
    std::fs::create_dir_all(docs.join(".git")).unwrap();
    let docs = docs.canonicalize().unwrap();
    settings.enrollment.profiles.push(profile(
        "docs",
        &docs,
        vec![check("default", common::true_program())],
    ));
    let (service_card, docs_card) = (CardId::from_parts(4, 1), CardId::from_parts(4, 2));
    let mut records = given("service", &repo, None, service_card);
    let dana = AgentId::from_parts(9, 1);
    let mut second = seeded_hire("docs", &docs, None);
    let Record::AgentHired { id, name, .. } = &mut second else {
        unreachable!("the sample is a hire");
    };
    (*id, *name) = (dana, "Dana".to_owned());
    records.push(second);
    records.push(Record::TicketCreated {
        card: docs_card,
        title: "Write the guide".into(),
        description: String::new(),
        department: "engineering".into(),
        project: "docs".into(),
        done_when: default_check(),
        by: operator(),
    });
    records.push(Record::TicketAssigned {
        card: docs_card,
        agent: dana,
        by: operator(),
        message: None,
        expected_seq: Seq(0),
    });
    seed(&fixture, records);
    let mut flow = Flow::boot_with(
        &fixture,
        settings,
        repo,
        [common::fails()],
        Options {
            refuse_nth_append: std::num::NonZeroUsize::new(1),
            ..Options::default()
        },
    );
    flow.await_status(service_card, CardStatus::Doing);
    flow.await_status(docs_card, CardStatus::Doing);
    assert_eq!(started_goals(&fixture.records()).len(), 2);
    flow.stop();
}

/// An assignment that is durable is answered even when the start that follows it fails; the
/// start is offered again and happens.
#[test]
fn an_assignment_is_answered_when_the_start_after_it_fails() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    let cody = flow.hire("Cody");
    let card = flow.create("Fix the parser");
    flow.append_failure
        .fail_nth(std::num::NonZeroUsize::new(2).unwrap());
    let outcome = flow.give_ok(card, cody, None);
    assert_eq!(queued(&outcome).0, 1);
    assert!(
        kinds(&fixture).contains(&RecordKind::TicketAssigned),
        "the assignment is durable"
    );
    flow.await_status(card, CardStatus::Doing);
    assert_eq!(started_goals(&fixture.records()).len(), 1);
    flow.stop();
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The window persist-before-spawn exists for: the turn is durable and its process has not been
/// launched. The stand-in host parks the launch right after noting what was durable, the journal
/// as it stands is copied (which is what a kill at that instant leaves), and a second daemon
/// boots on the copy. It finds one start, launches the one durable turn once, and starts nothing
/// else. The stand-in host has no process group to check.
#[test]
fn a_crash_with_the_turn_durable_and_not_launched_launches_it_once() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let gate = Arc::new(Gate::default());
    let mut flow = Flow::boot_with(
        &fixture,
        settings.clone(),
        repo.clone(),
        [common::fails()],
        Options {
            gate: Some(Arc::clone(&gate)),
            ..Options::default()
        },
    );
    let cody = flow.hire("Cody");
    let card = flow.create("Fix the parser");
    let socket = flow.settings.clone();
    let giver = std::thread::spawn(move || {
        let mut client = Client::connect(socket.socket()).unwrap();
        client
            .call_as_operator(
                dudes_proto::rpc::Method::TicketAssign,
                Some(json!({"card": card, "agent": cody, "expected_seq": 0})),
                credential(&socket),
            )
            .unwrap()
    });
    gate.wait_reached();

    let before = fixture.records();
    assert_eq!(
        before
            .iter()
            .filter(|r| matches!(r, Record::TurnIssued { .. }))
            .count(),
        1,
        "the turn is durable"
    );
    assert!(
        !before
            .iter()
            .any(|r| matches!(r, Record::TurnSpawned { .. })),
        "and not launched"
    );
    let issued = Flow::turns(&fixture)[0].0;
    let second = Fixture::new();
    copy_dir(&fixture.journal_dir(), &second.journal_dir());
    let mut settings = second.settings();
    settings.enrollment = flow.settings.enrollment.clone();
    let mut revived = Flow::boot(&second, settings, repo, [common::fails()]);
    let spawned = second.await_record("TurnSpawned", |record| match record {
        Record::TurnSpawned { turn, .. } => Some(*turn),
        _ => None,
    });
    assert_eq!(spawned, issued, "the durable turn is the one launched");
    let records = settled(&second, second.records().len());
    assert_eq!(started_goals(&records).len(), 1, "one start");
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r, Record::TurnIssued { .. }))
            .count(),
        1,
        "no second turn"
    );
    assert_eq!(
        revived.host.spawned().len(),
        1,
        "one launch after the restart"
    );
    assert_eq!(revived.card(card).status, CardStatus::Doing);
    revived.stop();

    gate.release();
    let _ = giver.join();
    flow.stop();
}

fn stop_params(flow: &mut Flow, card: CardId) -> Value {
    let seq = flow.card(card).seq;
    json!({"card": card, "expected_seq": seq})
}

fn hop_cap_one(settings: &mut duded::Settings) {
    settings.enrollment.profiles[0].hop_cap = 1;
}

/// Stop on a Working ticket clears its goal, then returns the card to To do, unassigned, authored
/// by the operator. It does not end the running turn: the agent keeps running, the host is asked
/// to kill nothing, and the project stays busy until the turn ends by itself. Nothing falls
/// between the two records, so the card is never in Needs you on the way.
#[test]
fn stop_returns_the_ticket_to_to_do_and_the_project_waits_for_the_running_turn() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    let (cody, dana) = (flow.hire("Cody"), flow.hire("Dana"));
    let (one, two) = (flow.create("One"), flow.create("Two"));
    assert_eq!(flow.give_ok(one, cody, None), TicketAssignOutcome::Started);
    let waiting = flow.give_ok(two, dana, None);
    assert_eq!(queued(&waiting).1, TicketWait::Ticket { card: one });
    let (turn, _, goal, _) = Flow::await_turn_number(&fixture, 1);
    let before = fixture.records().len();

    let params = stop_params(&mut flow, one);
    let stopped = flow.call(dudes_proto::rpc::Method::TicketStop, params);
    assert!(stopped.error.is_none(), "{:?}", stopped.error);

    let view = flow.card(one);
    assert_eq!(view.status, CardStatus::Todo);
    assert_eq!(view.claimed_by, None);
    assert!(matches!(view.status_by, Actor::Peer { .. }));
    let appended = fixture.records()[before..].to_vec();
    assert_eq!(
        appended.iter().map(Record::kind).collect::<Vec<_>>(),
        [RecordKind::GoalCleared, RecordKind::CardTransition]
    );
    assert!(matches!(&appended[0], Record::GoalCleared { goal: cleared, .. } if *cleared == goal));
    assert_eq!(
        transitions(&fixture.records(), one).len(),
        1,
        "no move to Needs you on the way"
    );
    assert!(
        flow.host.peek(|host| host.killed.is_empty()),
        "the turn is not killed"
    );
    assert_eq!(flow.agent(cody).status, AgentStatus::Running);
    assert!(flow.agent(cody).goal.is_none());
    assert_eq!(flow.read(one).waiting.unwrap().wait, TicketWait::Stopping);
    assert_eq!(
        flow.read(one).waiting.unwrap().note,
        "Stopping: the agent is finishing its current step."
    );
    assert_eq!(
        flow.read(two).waiting.unwrap().wait,
        TicketWait::ProjectBusy
    );
    assert_eq!(Flow::turns(&fixture).len(), 1, "no turn follows a stop");

    flow.end_turn(turn, Some("late words"));
    flow.await_status(two, CardStatus::Doing);
    assert!(
        !fixture
            .records()
            .iter()
            .any(|r| matches!(r, Record::PredicateResult { goal: judged, .. } if *judged == goal)),
        "a stopped goal is not judged"
    );
    assert_eq!(flow.card(one).status, CardStatus::Todo);
    flow.stop();
}

/// A crash between Stop's two records leaves a Working ticket with a cleared goal. The restart
/// finds it owing Needs you, and a second Stop finishes the job without clearing anything twice.
#[test]
fn a_crash_between_stops_two_appends_leaves_a_ticket_a_second_stop_finishes() {
    let moved = |r: &Record| matches!(r, Record::CardTransition { .. });
    let drive = |fixture: &Fixture, flow: &mut Flow| {
        let cody = flow.hire("Cody");
        let card = flow.create("One");
        flow.give_ok(card, cody, None);
        Flow::await_turn_number(fixture, 1);
        card
    };

    let reference = Fixture::new();
    let (settings, repo) = enrolled(&reference);
    let mut flow = Flow::boot(&reference, settings, repo, [common::fails()]);
    let card = drive(&reference, &mut flow);
    let before = reference.records().len();
    let params = stop_params(&mut flow, card);
    flow.ok::<Value>(dudes_proto::rpc::Method::TicketStop, params);
    let nth = appends_between(&reference.records(), before, moved);
    flow.stop();

    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings.clone(), repo.clone(), [common::fails()]);
    let card = drive(&fixture, &mut flow);
    let before = fixture.records().len();
    flow.append_failure.short_write_after(nth - 1, 5);
    let params = stop_params(&mut flow, card);
    let _ = flow.call(dudes_proto::rpc::Method::TicketStop, params);
    let crashed = settled(&fixture, before + nth - 1);
    assert!(
        crashed[before..]
            .iter()
            .any(|r| matches!(r, Record::GoalCleared { .. }))
    );
    assert!(
        !crashed[before..].iter().any(moved),
        "the card did not move"
    );
    flow.stop();

    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    flow.await_status(card, CardStatus::Blocked);
    let detail = flow.read(card);
    assert_eq!(detail.ticket.reason, Some(TicketReason::GoalChanged));
    let params = stop_params(&mut flow, card);
    flow.ok::<Value>(dudes_proto::rpc::Method::TicketStop, params);
    let view = flow.card(card);
    assert_eq!((view.status, view.claimed_by), (CardStatus::Todo, None));
    let records = settled(&fixture, crashed.len() + 1);
    flow.stop();
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r, Record::GoalCleared { .. }))
            .count(),
        1,
        "the goal was cleared once"
    );
}

/// A goal that ran out of turns moves the ticket to Needs you with the reason. The ticket holds
/// its agent and its project until the operator settles it: another ticket for the same agent
/// starts nothing, and a ticket for another agent in the project waits with a note that names the
/// stalled ticket. Giving the stalled ticket to an agent again clears the old goal first and
/// restarts it ahead of the others.
#[test]
fn an_exhausted_ticket_holds_its_agent_and_project_until_the_operator_settles_it() {
    let fixture = Fixture::new();
    let (mut settings, repo) = enrolled(&fixture);
    hop_cap_one(&mut settings);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    let (cody, dana) = (flow.hire("Cody"), flow.hire("Dana"));
    let (one, two, three) = (flow.create("One"), flow.create("Two"), flow.create("Three"));
    assert_eq!(flow.give_ok(one, cody, None), TicketAssignOutcome::Started);
    let (turn, _, old_goal, _) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(turn, Some("I tried."));
    flow.await_status(one, CardStatus::Blocked);
    let detail = flow.read(one);
    assert_eq!(detail.ticket.reason, Some(TicketReason::NoTurns));
    assert_eq!(
        TicketReason::NoTurns.sentence(),
        "The agent used all its turns on this ticket. Give to agent to try again, or Stop."
    );

    let same_agent = flow.give_ok(two, cody, None);
    assert_eq!(queued(&same_agent).1, TicketWait::AgentBusy);
    let other_agent = flow.give_ok(three, dana, None);
    let (_, wait, note) = queued(&other_agent);
    assert_eq!(wait, TicketWait::Ticket { card: one });
    assert_eq!(
        note,
        "Waiting for One to be finished, accepted or sent back."
    );
    let settled_len = fixture.records().len();
    assert_eq!(
        started_goals(&settled(&fixture, settled_len)).len(),
        1,
        "nothing else started"
    );

    let before = fixture.records().len();
    assert_eq!(
        flow.give_ok(one, cody, Some("Try the other way.")),
        TicketAssignOutcome::Started
    );
    let appended = fixture.records()[before..].to_vec();
    let cleared = position_of(
        &appended,
        |r| matches!(r, Record::GoalCleared { goal, .. } if *goal == old_goal),
    );
    let assigned = position_of(&appended, |r| matches!(r, Record::TicketAssigned { .. }));
    let started = position_of(&appended, |r| matches!(r, Record::TicketStarted { .. }));
    assert!(
        cleared < assigned && assigned < started,
        "{:?}",
        appended.iter().map(Record::kind).collect::<Vec<_>>()
    );
    assert_eq!(flow.read(one).ticket.round, 2);
    let (_, who, new_goal, task) = Flow::await_turn_number(&fixture, 2);
    assert_eq!(who, cody);
    assert_ne!(new_goal, old_goal, "a fresh goal");
    assert_eq!(flow.read(one).ticket.goal, Some(new_goal));
    assert!(
        task.starts_with(
            "One\n\nThe operator reviewed the previous work and asked for changes:\nTry the other way."
        ),
        "{task}"
    );
    assert_eq!(flow.card(two).status, CardStatus::Claimed);
    flow.stop();
}

/// Remove on a ticket in Needs you clears its unfinished goal first, so it does not hold the agent
/// afterwards. Remove on a Working ticket is refused and appends nothing.
#[test]
fn remove_clears_an_unfinished_goal_first_and_refuses_a_working_ticket() {
    let fixture = Fixture::new();
    let (mut settings, repo) = enrolled(&fixture);
    hop_cap_one(&mut settings);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    let cody = flow.hire("Cody");
    let (one, two) = (flow.create("One"), flow.create("Two"));
    flow.give_ok(one, cody, None);
    flow.give_ok(two, cody, None);
    let (turn, _, goal, _) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(turn, None);
    flow.await_status(one, CardStatus::Blocked);

    let before = fixture.records().len();
    let seq = flow.card(one).seq;
    flow.ok::<Value>(
        dudes_proto::rpc::Method::TicketRemove,
        json!({"card": one, "expected_seq": seq}),
    );
    let appended = fixture.records()[before..].to_vec();
    assert!(matches!(&appended[0], Record::GoalCleared { goal: cleared, .. } if *cleared == goal));
    assert!(matches!(&appended[1], Record::TicketRemoved { .. }));
    assert!(flow.board().iter().all(|view| view.id != one));
    flow.await_status(two, CardStatus::Doing);

    let before = fixture.records().len();
    let seq = flow.card(two).seq;
    let refused = flow.call(
        dudes_proto::rpc::Method::TicketRemove,
        json!({"card": two, "expected_seq": seq}),
    );
    assert_eq!(refusal_reason(&refused), TicketRefusal::WrongState.code());
    assert_eq!(fixture.records().len(), before);
    flow.stop();
}

/// An engine that refuses its launch holds its agent. The Working ticket moves to Needs you with
/// the hold's sentence and keeps the project: the other agent's ticket waits for it. Stop from
/// Needs you clears the goal and frees the project.
#[test]
fn a_hold_moves_the_working_ticket_to_needs_you_and_keeps_the_project() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let card = CardId::from_parts(4, 1);
    let mut records = given("service", &repo, None, card);
    let dana = AgentId::from_parts(9, 1);
    let mut second = seeded_hire("service", &repo, None);
    let Record::AgentHired { id, name, .. } = &mut second else {
        unreachable!("the sample is a hire");
    };
    (*id, *name) = (dana, "Dana".to_owned());
    records.insert(1, second);
    seed(&fixture, records);
    let mut flow = Flow::boot_with(
        &fixture,
        settings,
        repo,
        [common::fails()],
        Options {
            fail_spawn: true,
            ..Options::default()
        },
    );
    flow.await_status(card, CardStatus::Blocked);
    let ticket = flow.read(card).ticket;
    assert_eq!(ticket.reason, Some(TicketReason::HeldLaunch));
    assert_eq!(
        TicketReason::HeldLaunch.sentence(),
        "The agent's engine would not start. Fix the cause, then resume the agent in the Floor or with `dudes resume`."
    );

    let other = flow.create("Other");
    let waiting = flow.give_ok(other, dana, None);
    assert_eq!(queued(&waiting).1, TicketWait::Ticket { card });

    let params = stop_params(&mut flow, card);
    flow.ok::<Value>(dudes_proto::rpc::Method::TicketStop, params);
    assert_eq!(flow.card(card).status, CardStatus::Todo);
    flow.await_status(other, CardStatus::Doing);
    flow.stop();
}

/// A daily limit that resets by itself keeps the ticket Working and shows a note. A question the
/// agent asks moves no card.
#[test]
fn a_daily_limit_and_an_open_question_move_no_card() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    let cody = flow.hire_with(
        "Cody",
        AgentBudget {
            usd_per_day: Some(Usd::from_dollars(1)),
            tokens_per_day: None,
        },
    );
    let card = flow.create("One");
    flow.give_ok(card, cody, None);
    let (turn, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn_charging(turn, None, Some(Usd::from_dollars(2)));
    fixture.await_record("an Exhausted decision", |record| {
        matches!(
            record,
            Record::LoopDecided {
                decision: LoopDecision::Exhausted { .. },
                ..
            }
        )
        .then_some(())
    });
    let after = settled(&fixture, fixture.records().len());
    assert_eq!(flow.card(card).status, CardStatus::Doing);
    let waiting = flow.read(card).waiting.expect("a waiting note");
    assert_eq!(waiting.wait, TicketWait::AgentLimit);
    assert_eq!(
        waiting.note,
        "The agent reached its daily limit. It carries on by itself when the limit resets."
    );
    assert!(
        !after
            .iter()
            .any(|r| matches!(r, Record::CardTransition { .. }))
    );
    flow.stop();

    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(&fixture, settings, repo, [common::fails()]);
    let dana = flow.hire("Dana");
    let card = flow.create("Two");
    flow.give_ok(card, dana, None);
    let hash = fixture.await_record("agent token", |record| match record {
        Record::HookTokenMinted { agent, token_hash } if *agent == dana => Some(token_hash.clone()),
        _ => None,
    });
    let token = common::token_of(&hash);
    flow.column.set(PeerProvenance::Agent(dana));
    let mut as_agent = Client::connect(flow.settings.socket()).unwrap();
    let asked = as_agent
        .call(
            dudes_proto::rpc::Method::AgentAsk,
            Some(json!({"token": token.expose(), "text": "which branch?"})),
        )
        .unwrap();
    assert!(asked.error.is_none(), "{:?}", asked.error);
    fixture.await_record("the question", |record| {
        matches!(record, Record::HumanQuestion { .. }).then_some(())
    });
    let after = settled(&fixture, fixture.records().len());
    assert_eq!(flow.agent(dana).status, AgentStatus::Waiting);
    assert_eq!(flow.card(card).status, CardStatus::Doing);
    assert!(
        !after
            .iter()
            .any(|r| matches!(r, Record::CardTransition { .. }))
    );
    drop(as_agent);
    flow.stop();
}

/// The summary is the agent's last message, cut and stripped of control bytes, and the check
/// result is the daemon's own newest run: pass or fail, exit status and time. A new round starts
/// with neither.
#[test]
fn the_summary_and_the_check_result_follow_the_ticket() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut flow = Flow::boot(
        &fixture,
        settings,
        repo,
        [common::fails(), common::passes()],
    );
    let cody = flow.hire("Cody");
    let card = flow.create("One");
    flow.give_ok(card, cody, None);
    assert_eq!(flow.read(card).ticket.summary, None);

    let (first, ..) = Flow::await_turn_number(&fixture, 1);
    flow.end_turn(first, Some("first words"));
    let (second, ..) = Flow::await_turn_number(&fixture, 2);
    let failed = flow.read(card).ticket;
    assert_eq!(failed.summary.as_deref(), Some("first words"));
    let result = failed.check.expect("the check ran after the first turn");
    assert_eq!((result.pass, result.exit), (false, Some(1)));

    let long = format!("{}\u{7}{}", "x".repeat(3_000), "y".repeat(3_000));
    flow.end_turn(second, Some(&long));
    flow.await_status(card, CardStatus::Review);
    let ready = flow.read(card).ticket;
    let summary = ready.summary.expect("a summary");
    assert_eq!(summary.len(), 4_096);
    assert!(!summary.contains('\u{7}'));
    assert!(summary.starts_with(&"x".repeat(3_000)));
    let result = ready.check.expect("the check ran");
    assert_eq!((result.pass, result.exit), (true, Some(0)));
    assert!(result.at > Timestamp::from_unix_millis(1_000));

    flow.give_ok(card, cody, Some("again"));
    let fresh = flow.read(card).ticket;
    assert_eq!((fresh.summary, fresh.check), (None, None));
    flow.stop();
}
