//! Tickets over the production socket: what the operator may do to one, what an agent may not,
//! the plain sentence behind every refusal, and what a restart rebuilds.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Fixture, ScriptedColumn, ScriptedRunner, SharedHost, TestClock};
use duded::floor_actions::{EnrolledCheck, Enrollment, Profile};
use duded::startup::Witness;
use dudes_proto::record::{Hex32, RecordedWorkspace, samples};
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::ticket::{TicketAssignOutcome, TicketAssignResult, TicketReadResult};
use dudes_proto::rpc::{ConflictData, CurrentMethod, ErrorCode, Method, Response};
use dudes_proto::{
    Actor, AgentBudget, CardView, Entry, Goal, GoalBudget, Predicate, Record, RecordKind,
    TicketCheck, TicketRefusal,
};
use dudes_rpc::Client;
use dudes_rpc::auth::PeerProvenance;
use dudes_types::{AgentId, CardId, CardStatus, GoalId, IsolationMode, Seq, Timestamp, Usd};
use serde_json::{Value, json};

type Socket =
    Client<std::io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream>;

struct Daemon {
    running: common::Running,
    column: Arc<ScriptedColumn>,
    settings: duded::Settings,
    operator: Socket,
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

/// A repository directory, and settings that enroll it as the one project `service`, whose own
/// check is `/usr/bin/true` and which also offers a second named check.
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
                check("strict", common::false_program()),
            ],
        )],
    };
    (settings, repo)
}

fn start(fixture: &Fixture, settings: duded::Settings) -> Daemon {
    let column = ScriptedColumn::new(PeerProvenance::NoRecordedAgent);
    let mut seams = common::seams_with(
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
        Arc::clone(&column) as Arc<dyn dudes_rpc::auth::PeerResolver>,
    );
    for _ in 0..fixture.records().len() * 8 + 64 {
        dudes_supervisor::Rng::ulid(&mut seams.rng);
    }
    let daemon = duded::boot(&settings, seams).unwrap();
    let running = common::start(daemon);
    let operator = Client::connect(settings.socket()).unwrap();
    Daemon {
        running,
        column,
        settings,
        operator,
    }
}

impl Daemon {
    fn call(&mut self, method: Method, params: Value) -> Response {
        self.operator
            .call_as_operator(method, Some(params), credential(&self.settings))
            .unwrap()
    }

    fn ok<T: serde::de::DeserializeOwned>(&mut self, method: Method, params: Value) -> T {
        let response = self.call(method, params);
        assert!(response.error.is_none(), "{method:?}: {:?}", response.error);
        serde_json::from_value(response.result.unwrap()).unwrap()
    }

    fn checkout(&mut self, params: Value) -> Response {
        self.operator
            .call_current_as_operator(
                CurrentMethod::TicketCheckout,
                Some(params),
                credential(&self.settings),
            )
            .unwrap()
    }

    fn create(&mut self, title: &str) -> CardId {
        let created: dudes_proto::rpc::ticket::TicketCreateResult =
            self.ok(Method::TicketCreate, json!({"title": title}));
        created.card
    }

    fn board(&mut self) -> Vec<CardView> {
        self.ok(Method::BoardList, json!({}))
    }

    fn card(&mut self, card: CardId) -> CardView {
        self.board()
            .into_iter()
            .find(|view| view.id == card)
            .expect("the card is on the board")
    }

    fn read(&mut self, card: CardId) -> TicketReadResult {
        self.ok(Method::TicketRead, json!({"card": card}))
    }

    fn assign(&mut self, card: CardId, agent: AgentId, message: Option<&str>) -> Response {
        let seq = self.card(card).seq;
        self.call(
            Method::TicketAssign,
            json!({"card": card, "agent": agent, "message": message, "expected_seq": seq}),
        )
    }

    /// An agent hired into the project and paused, so a ticket given to it waits in its queue
    /// instead of starting. These tests are about the data the methods keep, not about the start.
    fn paused_agent(&mut self, repo: &Path) -> AgentId {
        let agent = common::hire(&self.running.handle, repo);
        self.ok::<Value>(Method::LoopPause, json!({"agent": agent}));
        agent
    }

    fn stop(mut self) {
        drop(self.operator);
        self.running.stop(duded::ExitCause::ShutdownRpc);
    }
}

/// The refusal a response carries, as its code, its `data.reason` and its sentence.
fn refusal(response: &Response) -> (i32, Option<String>, String) {
    let error = response.error.as_ref().expect("a refusal");
    (
        error.code,
        error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        error.message.clone(),
    )
}

fn assert_refused(response: &Response, reason: TicketRefusal, state: CardStatus) {
    let (code, found, message) = refusal(response);
    assert_eq!(code, ErrorCode::Refused.code(), "{message}");
    assert_eq!(found.as_deref(), Some(reason.code()));
    assert_eq!(message, reason.sentence(state));
}

fn position(response: Response) -> u32 {
    assert!(response.error.is_none(), "{:?}", response.error);
    let result: TicketAssignResult = serde_json::from_value(response.result.unwrap()).unwrap();
    let TicketAssignOutcome::Queued { position, .. } = result.outcome else {
        panic!("a paused agent only queues: {:?}", result.outcome);
    };
    position
}

fn operator() -> Actor {
    Actor::Peer { pid: 1, uid: None }
}

fn ticket_check() -> TicketCheck {
    TicketCheck {
        id: "default".into(),
        label: "default passes".into(),
        predicate: Predicate::Command {
            argv: vec![common::true_program().into()],
            timeout_s: 10,
        },
    }
}

fn sample_agent() -> AgentId {
    let Record::AgentHired { id, .. } = samples::record(RecordKind::AgentHired) else {
        unreachable!("the sample is a hire");
    };
    id
}

fn hire_into(repo: &Path) -> Record {
    let mut hired = samples::record(RecordKind::AgentHired);
    let Record::AgentHired {
        cwd,
        workspace,
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
    (*sandbox, *isolation, *max_turns, *max_budget, *trusted) = (None, None, None, None, false);
    mcp_servers.clear();
    *cwd = repo.to_path_buf();
    *workspace = Some(RecordedWorkspace {
        id: "service".into(),
        label: "service workspace".into(),
    });
    hired
}

fn ticket_created(card: CardId, title: &str) -> Record {
    Record::TicketCreated {
        card,
        title: title.into(),
        description: String::new(),
        department: "engineering".into(),
        project: "service".into(),
        done_when: ticket_check(),
        by: operator(),
    }
}

fn ticket_assigned(card: CardId, agent: AgentId, expected_seq: u64) -> Record {
    Record::TicketAssigned {
        card,
        agent,
        by: operator(),
        message: None,
        expected_seq: Seq(expected_seq),
    }
}

fn ticket_started(card: CardId, agent: AgentId, goal: u128, expected_seq: u64) -> Record {
    let check = ticket_check();
    Record::TicketStarted {
        card,
        agent,
        goal: Goal {
            id: GoalId::from_parts(8, goal),
            text: "work".into(),
            completion_check: duded::ticket_goal_check(&check),
            predicate: check.predicate,
            baseline: Some(dudes_proto::CheckBaseline::MayAlreadyPass),
            budget: GoalBudget {
                usd: None,
                tokens: None,
            },
            hop_cap: 4,
        },
        expected_seq: Seq(expected_seq),
    }
}

fn to_status(card: CardId, from: CardStatus, to: CardStatus, expected_seq: u64) -> Record {
    Record::CardTransition {
        card,
        from,
        to,
        by: Actor::System,
        expected_seq: Seq(expected_seq),
        reason: None,
    }
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

fn kinds(fixture: &Fixture) -> Vec<RecordKind> {
    fixture.records().iter().map(Record::kind).collect()
}

#[test]
fn checkout_returns_the_recorded_subdirectory_and_appends_nothing() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let directory = repo.join("source files");
    std::fs::create_dir(&directory).unwrap();
    let card = CardId::from_parts(4, 1);
    seed(
        &fixture,
        vec![
            hire_into(&directory),
            ticket_created(card, "Review the source"),
            ticket_assigned(card, sample_agent(), 0),
        ],
    );
    let mut daemon = start(&fixture, settings);
    fixture.await_record("checkout owner's settled starting sample", |record| {
        matches!(record, Record::InstructionObservationStarted {
            owner: dudes_proto::InstructionObservationOwner::Managed { agent, .. }, ..
        } if *agent == sample_agent())
        .then_some(())
    });
    fixture.await_record("checkout owner's actual spawn", |record| {
        matches!(record, Record::TurnSpawned { agent, .. } if *agent == sample_agent())
            .then_some(())
    });
    let detail = daemon.read(card);
    let review = detail
        .ticket
        .instruction_review
        .as_ref()
        .expect("the actual starting evidence is visible");
    assert!(
        review.pending_comparison,
        "the real started turn is still open"
    );
    let seq = detail.card.seq;
    let before = fixture.records();
    let result = daemon.checkout(json!({"card": card, "expected_seq": seq}));
    assert!(result.error.is_none(), "{:?}", result.error);
    assert_eq!(result.result.unwrap(), json!({"directory": directory}));
    assert_eq!(fixture.records(), before);
    daemon.stop();
}

#[test]
fn checkout_refuses_stale_unassigned_and_removed_tickets_without_appending() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut daemon = start(&fixture, settings);
    let agent = daemon.paused_agent(&repo);
    let card = daemon.create("Assigned");
    assert_eq!(position(daemon.assign(card, agent, None)), 1);
    let free = daemon.create("Unassigned");
    let removed = daemon.create("Removed");
    daemon.ok::<Value>(
        Method::TicketRemove,
        json!({"card": removed, "expected_seq": 0}),
    );
    let before = fixture.records();
    let stale = daemon.checkout(json!({"card": card, "expected_seq": 0}));
    let error = stale.error.unwrap();
    assert_eq!(error.code, ErrorCode::Conflict.code());
    assert_eq!(
        serde_json::from_value::<ConflictData>(error.data.unwrap())
            .unwrap()
            .current_seq,
        daemon.card(card).seq
    );
    let unassigned = daemon.checkout(json!({"card": free, "expected_seq": 0}));
    assert_eq!(
        refusal(&unassigned),
        (
            ErrorCode::Refused.code(),
            Some("unassigned".into()),
            "That ticket has no current agent.".into()
        )
    );
    let removed = daemon.checkout(json!({"card": removed, "expected_seq": 0}));
    assert_eq!(removed.error.unwrap().code, ErrorCode::NotFound.code());
    let forged = daemon.checkout(json!({"card": card, "expected_seq": 1, "directory": repo}));
    assert_eq!(forged.error.unwrap().code, ErrorCode::InvalidParams.code());
    assert_eq!(fixture.records(), before);
    daemon.stop();
}

#[test]
fn checkout_returns_an_active_recorded_lease_but_refuses_removed_and_retired_owners() {
    for ending in ["active", "removed", "retired"] {
        let fixture = Fixture::new();
        let (settings, origin) = enrolled(&fixture);
        let directory = origin.parent().unwrap().join("leased");
        std::fs::create_dir(&directory).unwrap();
        let mut leased = samples::record(RecordKind::WorktreeLeased);
        let Record::WorktreeLeased {
            worktree,
            origin: recorded_origin,
            path,
            ..
        } = &mut leased
        else {
            unreachable!()
        };
        let id = *worktree;
        *recorded_origin = origin.clone();
        *path = directory.clone();
        let mut hired = hire_into(&directory);
        let Record::AgentHired { worktree, .. } = &mut hired else {
            unreachable!()
        };
        *worktree = Some(id);
        let card = CardId::from_parts(4, 1);
        let agent = sample_agent();
        let mut records = vec![
            leased,
            hired,
            ticket_created(card, "Leased checkout"),
            ticket_assigned(card, agent, 0),
        ];
        match ending {
            "active" => {}
            "removed" => records.push(Record::WorktreeRemoved {
                worktree: id,
                how: dudes_proto::WorktreeRemoval::Operator,
            }),
            "retired" => records.push(Record::AgentRetired {
                id: agent,
                reason: "Finished".into(),
                browser_retire: None,
            }),
            _ => unreachable!(),
        }
        seed(&fixture, records);
        let mut daemon = start(&fixture, settings);
        let seq = daemon.card(card).seq;
        let before = fixture.records();
        let response = daemon.checkout(json!({"card": card, "expected_seq": seq}));
        if ending == "active" {
            assert!(response.error.is_none(), "{:?}", response.error);
            assert_eq!(
                response.result.unwrap(),
                json!({"directory": directory, "worktree": {"worktree": id, "origin": origin}})
            );
        } else {
            let expected = if ending == "removed" {
                "worktree_unavailable"
            } else {
                "unassigned"
            };
            let (code, reason, _) = refusal(&response);
            assert_eq!(code, ErrorCode::Refused.code());
            assert_eq!(reason.as_deref(), Some(expected));
        }
        assert_eq!(fixture.records(), before);
        daemon.stop();
    }
}

#[test]
fn a_created_ticket_replays_to_the_same_board_after_the_cache_is_deleted() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut daemon = start(&fixture, settings.clone());
    let agent = daemon.paused_agent(&repo);
    let first = daemon.create("  Fix the parser \n");
    let created: dudes_proto::rpc::ticket::TicketCreateResult = daemon.ok(
        Method::TicketCreate,
        json!({"title": "Add the lexer", "description": "Cover the empty input.\n", "done_when": "strict"}),
    );
    let second = created.card;
    assert_eq!(
        position(daemon.assign(first, agent, Some("Mind the edge."))),
        1
    );
    daemon.ok::<Value>(
        Method::TicketEdit,
        json!({"card": second, "title": "Add the tokenizer", "expected_seq": 0}),
    );

    let (board, details) = (
        daemon.board(),
        [first, second].map(|card| daemon.read(card)),
    );
    let first_view = &details[0];
    assert_eq!(first_view.card.title, "Fix the parser");
    assert_eq!(first_view.card.status, CardStatus::Claimed);
    assert_eq!(first_view.card.claimed_by, Some(agent));
    assert_eq!(first_view.card.status_by, operator_of(&first_view.card));
    assert_eq!(first_view.ticket.project, "service");
    assert_eq!(first_view.ticket.description, "");
    assert_eq!(first_view.ticket.done_when.id, "default");
    assert_eq!(first_view.ticket.message.as_deref(), Some("Mind the edge."));
    assert_eq!(first_view.position, Some(1));
    let second_view = &details[1];
    assert_eq!(second_view.card.title, "Add the tokenizer");
    assert_eq!(second_view.card.status, CardStatus::Todo);
    assert_eq!(second_view.card.department, "engineering");
    assert_eq!(
        second_view.ticket.description, "Cover the empty input.\n",
        "the note is kept exactly as written"
    );
    assert_eq!(second_view.ticket.done_when.id, "strict");
    assert_eq!(second_view.position, None);

    daemon.stop();
    std::fs::remove_file(settings.state_dir.join("state.redb")).unwrap();
    let mut daemon = start(&fixture, settings);
    assert_eq!(daemon.board(), board, "the rebuilt board is the live one");
    assert_eq!(&daemon.read(first), first_view);
    assert_eq!(&daemon.read(second), second_view);
    daemon.stop();
}

fn operator_of(view: &CardView) -> Actor {
    assert!(matches!(view.status_by, Actor::Peer { .. }));
    view.status_by
}

#[test]
fn the_ticket_records_are_journaled_in_required_frames() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut daemon = start(&fixture, settings);
    let agent = daemon.paused_agent(&repo);
    let card = daemon.create("Frame check");
    daemon.assign(card, agent, None);
    let seq = daemon.card(card).seq;
    daemon.ok::<Value>(
        Method::TicketRemove,
        json!({"card": card, "expected_seq": seq}),
    );
    daemon.stop();
    let ticket_kinds: Vec<_> = kinds(&fixture)
        .into_iter()
        .filter(|kind| kind.requires_required_frame())
        .collect();
    assert_eq!(
        ticket_kinds,
        [
            RecordKind::TicketCreated,
            RecordKind::TicketAssigned,
            RecordKind::TicketRemoved
        ]
    );
}

/// Edit replaces the title, note and check, leaves the ticket where it is in its agent's queue,
/// and is refused once work has started.
#[test]
fn edit_changes_the_three_fields_and_keeps_the_queue_place() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut daemon = start(&fixture, settings);
    let agent = daemon.paused_agent(&repo);
    let first = daemon.create("First");
    let second = daemon.create("Second");
    assert_eq!(position(daemon.assign(first, agent, None)), 1);
    assert_eq!(position(daemon.assign(second, agent, None)), 2);
    let before = daemon.read(first);

    let seq = daemon.card(first).seq;
    daemon.ok::<Value>(
        Method::TicketEdit,
        json!({
            "card": first, "title": " Renamed ", "description": "Now with a note.",
            "done_when": "strict", "expected_seq": seq
        }),
    );
    let after = daemon.read(first);
    assert_eq!(after.card.title, "Renamed");
    assert_eq!(after.ticket.description, "Now with a note.");
    assert_eq!(after.ticket.done_when.id, "strict");
    assert_eq!(after.card.status, CardStatus::Claimed);
    assert_eq!(after.card.claimed_by, Some(agent));
    assert_eq!(after.card.seq, before.card.seq.next());
    assert_eq!(after.position, Some(1), "the edit keeps its place");
    assert_eq!(after.ticket.queue, before.ticket.queue);
    assert_eq!(daemon.read(second).position, Some(2));

    let seq = daemon.card(first).seq;
    daemon.ok::<Value>(
        Method::TicketEdit,
        json!({"card": first, "description": "", "expected_seq": seq}),
    );
    assert_eq!(daemon.read(first).ticket.description, "");
    daemon.stop();
}

#[test]
fn a_working_ticket_can_be_neither_edited_nor_removed() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let card = CardId::from_parts(4, 1);
    let agent = sample_agent();
    seed(
        &fixture,
        vec![
            hire_into(&repo),
            ticket_created(card, "In progress"),
            ticket_assigned(card, agent, 0),
            ticket_started(card, agent, 1, 1),
        ],
    );
    let mut daemon = start(&fixture, settings);
    fixture.await_record("working ticket's exact starting spawn", |record| {
        matches!(record, Record::TurnSpawned { agent: owner, .. } if *owner == agent).then_some(())
    });
    let detail = daemon.read(card);
    assert_eq!(detail.card.status, CardStatus::Doing);
    assert!(
        detail
            .ticket
            .instruction_review
            .as_ref()
            .is_some_and(|review| review.pending_comparison)
    );
    let before = fixture.records().len();
    let seq = detail.card.seq;
    for (method, params) in [
        (
            Method::TicketEdit,
            json!({"card": card, "title": "Too late", "expected_seq": seq}),
        ),
        (
            Method::TicketRemove,
            json!({"card": card, "expected_seq": seq}),
        ),
    ] {
        let response = daemon.call(method, params);
        let reason = if method == Method::TicketRemove {
            TicketRefusal::InstructionComparisonPending
        } else {
            TicketRefusal::WrongState
        };
        assert_refused(&response, reason, CardStatus::Doing);
    }
    assert_eq!(fixture.records().len(), before, "a refusal appends nothing");
    assert_eq!(daemon.read(card).card.title, "In progress");
    daemon.stop();
}

#[test]
fn a_removed_ticket_leaves_the_board_and_stays_in_the_journal() {
    let fixture = Fixture::new();
    let (settings, _repo) = enrolled(&fixture);
    let mut daemon = start(&fixture, settings);
    let kept = daemon.create("Kept");
    let removed = daemon.create("Removed");
    let seq = daemon.card(removed).seq;
    daemon.ok::<Value>(
        Method::TicketRemove,
        json!({"card": removed, "expected_seq": seq}),
    );
    assert_eq!(
        daemon
            .board()
            .iter()
            .map(|view| view.id)
            .collect::<Vec<_>>(),
        vec![kept]
    );
    let response = daemon.call(Method::TicketRead, json!({"card": removed}));
    assert_eq!(response.error.unwrap().code, ErrorCode::NotFound.code());
    assert!(
        fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::TicketCreated { card, .. } if *card == removed))
    );
    assert!(
        fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::TicketRemoved { card, .. } if *card == removed))
    );
    daemon.stop();
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one daemon, one story: the refusals are only meaningful against the same board"
)]
fn an_agent_cannot_claim_a_ticket_or_move_its_own_to_done_and_a_plain_card_is_unchanged() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut daemon = start(&fixture, settings);
    let agent = common::hire(&daemon.running.handle, &repo);
    common::set_goal(
        &daemon.running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let hash = fixture.await_record("agent token", |record| match record {
        Record::HookTokenMinted {
            agent: owner,
            token_hash,
        } if *owner == agent => Some(token_hash.clone()),
        _ => None,
    });
    let token = common::token_of(&hash);
    let held = daemon.create("Queued for the agent");
    let free = daemon.create("Still unassigned");
    assert_eq!(position(daemon.assign(held, agent, None)), 1);
    let plain: dudes_proto::rpc::board::CardCreateResult = daemon.ok(
        Method::BoardCreate,
        json!({"title": "Plain", "department": "infra"}),
    );
    let held_seq = daemon.card(held).seq;
    let before = fixture.records().len();

    daemon.column.set(PeerProvenance::Agent(agent));
    let mut as_agent = Client::connect(daemon.settings.socket()).unwrap();
    for (method, params) in [
        (
            Method::BoardClaim,
            json!({"token": token.expose(), "card": free, "agent": agent, "expected_seq": 0}),
        ),
        (
            Method::BoardTransition,
            json!({"token": token.expose(), "card": held, "to": "done", "expected_seq": held_seq}),
        ),
        (
            Method::BoardTransition,
            json!({"token": token.expose(), "card": held, "to": "review", "expected_seq": held_seq}),
        ),
    ] {
        let response = as_agent.call(method, Some(params)).unwrap();
        assert_refused(&response, TicketRefusal::TicketCard, CardStatus::Todo);
    }
    for (method, params) in [
        (Method::TicketCreate, json!({"title": "forged"})),
        (
            Method::TicketEdit,
            json!({"card": held, "title": "x", "expected_seq": held_seq}),
        ),
        (
            Method::TicketAssign,
            json!({"card": free, "agent": agent, "expected_seq": 0}),
        ),
        (
            Method::TicketRemove,
            json!({"card": held, "expected_seq": held_seq}),
        ),
        (Method::TicketRead, json!({"card": held})),
    ] {
        let refused = as_agent.call(method, Some(params)).unwrap().error.unwrap();
        assert_eq!(refused.code, ErrorCode::Unauthorized.code(), "{method:?}");
    }
    for cred in [None, Some(credential(&daemon.settings))] {
        let refused = as_agent
            .call_current_presenting(
                CurrentMethod::TicketCheckout,
                Some(json!({"card": held, "expected_seq": held_seq, "token": token.expose()})),
                cred,
            )
            .unwrap()
            .error
            .unwrap();
        assert_eq!(refused.code, ErrorCode::Unauthorized.code());
    }
    assert_eq!(
        fixture.records().len(),
        before,
        "no refusal appended a record"
    );
    assert_eq!(daemon.card(held).status, CardStatus::Claimed);
    assert_eq!(daemon.card(free).claimed_by, None);

    // The operator is refused too: a ticket moves only through the ticket methods.
    for (method, params) in [
        (
            Method::BoardClaim,
            json!({"card": free, "agent": agent, "expected_seq": 0}),
        ),
        (
            Method::BoardTransition,
            json!({"card": held, "to": "done", "expected_seq": held_seq}),
        ),
    ] {
        let response = daemon.call(method, params);
        assert_eq!(refusal(&response).1.as_deref(), Some("ticket_card"));
    }

    // A plain card behaves as it always did.
    assert!(
        as_agent
            .call(
                Method::BoardClaim,
                Some(json!({
                    "token": token.expose(), "card": plain.card, "agent": agent, "expected_seq": 0
                }))
            )
            .unwrap()
            .error
            .is_none()
    );
    assert!(
        as_agent
            .call(
                Method::BoardTransition,
                Some(json!({
                    "token": token.expose(), "card": plain.card, "to": "doing", "expected_seq": 1
                }))
            )
            .unwrap()
            .error
            .is_none()
    );
    assert_eq!(daemon.card(plain.card).status, CardStatus::Doing);
    daemon.stop();
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one daemon, one story: the refusals are only meaningful against the same board"
)]
fn every_refusal_carries_its_fixed_code_and_plain_sentence_and_appends_nothing() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut daemon = start(&fixture, settings);
    let agent = daemon.paused_agent(&repo);
    let elsewhere = common::hire(&daemon.running.handle, fixture.state.path());
    let card = daemon.create("Subject");
    let plain: dudes_proto::rpc::board::CardCreateResult = daemon.ok(
        Method::BoardCreate,
        json!({"title": "Plain", "department": "engineering"}),
    );
    let before = fixture.records().len();

    let stale = daemon.call(
        Method::TicketEdit,
        json!({"card": card, "title": "x", "expected_seq": 9}),
    );
    let (code, _, message) = refusal(&stale);
    assert_eq!(code, ErrorCode::Conflict.code());
    assert_eq!(
        message,
        "This ticket changed while you were looking. Reload and try again."
    );
    assert_eq!(
        serde_json::from_value::<ConflictData>(stale.error.unwrap().data.unwrap())
            .unwrap()
            .current_seq,
        Seq::ZERO
    );

    for (method, params) in [
        (Method::TicketRead, json!({"card": plain.card})),
        (
            Method::TicketEdit,
            json!({"card": plain.card, "title": "x", "expected_seq": 0}),
        ),
        (
            Method::TicketRemove,
            json!({"card": plain.card, "expected_seq": 0}),
        ),
        (
            Method::TicketAssign,
            json!({"card": plain.card, "agent": agent, "expected_seq": 0}),
        ),
    ] {
        assert_refused(
            &daemon.call(method, params),
            TicketRefusal::NotATicket,
            CardStatus::Todo,
        );
    }
    assert_refused(
        &daemon.call(
            Method::TicketCreate,
            json!({"title": "x", "project": "gone"}),
        ),
        TicketRefusal::ProjectUnavailable,
        CardStatus::Todo,
    );
    assert_refused(
        &daemon.call(
            Method::TicketCreate,
            json!({"title": "x", "done_when": "strict", "done_when_revision": Hex32::new([9; 32])}),
        ),
        TicketRefusal::DoneWhenChanged,
        CardStatus::Todo,
    );
    let unknown = AgentId::from_parts(9, 9);
    assert_refused(
        &daemon.call(
            Method::TicketAssign,
            json!({"card": card, "agent": unknown, "expected_seq": 0}),
        ),
        TicketRefusal::AgentUnavailable,
        CardStatus::Todo,
    );
    assert_refused(
        &daemon.call(
            Method::TicketAssign,
            json!({"card": card, "agent": elsewhere, "expected_seq": 0}),
        ),
        TicketRefusal::AgentNoProject,
        CardStatus::Todo,
    );
    assert_eq!(fixture.records().len(), before, "a refusal appends nothing");

    let strict: dudes_proto::rpc::ticket::TicketCreateResult = daemon.ok(
        Method::TicketCreate,
        json!({
            "title": "Pinned", "done_when": "strict",
            "done_when_revision": duded::ticket_goal_check(&TicketCheck {
                id: "strict".into(),
                label: "strict passes".into(),
                predicate: Predicate::Command {
                    argv: vec![common::false_program().into()],
                    timeout_s: 10,
                },
            })
            .unwrap()
            .revision,
        }),
    );
    assert_eq!(daemon.read(strict.card).ticket.done_when.id, "strict");

    // The queue holds eight; moving a ticket that is already in it does not count against it.
    let mut queued = vec![card];
    for index in 1..8 {
        queued.push(daemon.create(&format!("Queued {index}")));
    }
    for (index, queued_card) in queued.iter().enumerate() {
        assert_eq!(
            position(daemon.assign(*queued_card, agent, None)),
            u32::try_from(index + 1).unwrap()
        );
    }
    let ninth = daemon.create("Ninth");
    assert_refused(
        &daemon.assign(ninth, agent, None),
        TicketRefusal::QueueFull,
        CardStatus::Todo,
    );
    assert_eq!(
        refusal(&daemon.assign(ninth, agent, None)).2,
        "That agent already has 8 tickets waiting."
    );
    assert_eq!(
        position(daemon.assign(queued[0], agent, None)),
        8,
        "a ticket moved again goes to the back of the queue it is already in"
    );
    daemon.stop();
}

#[test]
fn text_is_trimmed_bounded_and_free_of_control_characters() {
    let fixture = Fixture::new();
    let (settings, _repo) = enrolled(&fixture);
    let mut daemon = start(&fixture, settings);
    let before = fixture.records().len();
    for (params, why) in [
        (json!({"title": "  \n "}), "an empty title"),
        (json!({"title": "x".repeat(257)}), "a title over 256 bytes"),
        (
            json!({"title": "bell\u{7}"}),
            "a control character in the title",
        ),
        (json!({"title": "nul\0"}), "NUL in the title"),
        (json!({"title": "two\nlines"}), "a newline in the title"),
        (json!({"title": "tab\there"}), "a tab in the title"),
        (json!({"title": "del\u{7f}"}), "DEL in the title"),
        (
            json!({"title": "ok", "description": "x".repeat(8_193)}),
            "a note over 8192 bytes",
        ),
        (
            json!({"title": "ok", "description": "esc\u{1b}[2J"}),
            "a control character in the note",
        ),
    ] {
        let response = daemon.call(Method::TicketCreate, params);
        assert_eq!(
            response.error.expect(why).code,
            ErrorCode::InvalidParams.code(),
            "{why}"
        );
    }
    assert_eq!(fixture.records().len(), before);
    let exact = daemon.create(&"x".repeat(256));
    assert_eq!(daemon.read(exact).card.title.len(), 256);
    let kept: dudes_proto::rpc::ticket::TicketCreateResult = daemon.ok(
        Method::TicketCreate,
        json!({"title": "multi", "description": "line one\nline\ttwo"}),
    );
    assert_eq!(
        daemon.read(kept.card).ticket.description,
        "line one\nline\ttwo"
    );
    let edit = daemon.call(
        Method::TicketEdit,
        json!({"card": exact, "expected_seq": 0}),
    );
    assert_eq!(edit.error.unwrap().code, ErrorCode::InvalidParams.code());
    let before = fixture.records().len();
    let revision = daemon.call(
        Method::TicketEdit,
        json!({"card": exact, "title": "x", "done_when_revision": Hex32::new([1; 32]), "expected_seq": 0}),
    );
    assert_eq!(
        revision.error.unwrap().code,
        ErrorCode::InvalidParams.code()
    );
    let note = daemon.create("Note kept");
    let kept_note = daemon.card(note).seq;
    daemon.ok::<Value>(
        Method::TicketEdit,
        json!({"card": note, "description": "  padded \n", "expected_seq": kept_note}),
    );
    assert_eq!(daemon.read(note).ticket.description, "  padded \n");
    assert_eq!(
        fixture.records().len(),
        before + 2,
        "the refusal appended nothing"
    );
    daemon.stop();
}

/// The message that goes to the agent is trimmed, bounded and free of control characters.
#[test]
fn an_assignment_message_is_trimmed_and_refuses_control_characters() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let mut daemon = start(&fixture, settings);
    let agent = daemon.paused_agent(&repo);
    let card = daemon.create("Messaged");
    let before = fixture.records().len();
    for message in ["esc\u{1b}[2J", "nul\0", "del\u{7f}", &"x".repeat(2_049)] {
        let response = daemon.assign(card, agent, Some(message));
        assert_eq!(
            response.error.unwrap().code,
            ErrorCode::InvalidParams.code()
        );
    }
    assert_eq!(fixture.records().len(), before);
    assert_eq!(
        position(daemon.assign(card, agent, Some("  Mind the edge.\n\tAnd the end. \n"))),
        1
    );
    assert_eq!(
        daemon.read(card).ticket.message.as_deref(),
        Some("Mind the edge.\n\tAnd the end.")
    );
    daemon.stop();
}

#[test]
fn several_projects_need_the_caller_to_name_one() {
    let fixture = Fixture::new();
    let (mut settings, repo) = enrolled(&fixture);
    let other = fixture.state.path().join("other");
    std::fs::create_dir_all(other.join(".git")).unwrap();
    let other = other.canonicalize().unwrap();
    settings.enrollment.profiles.push(profile(
        "docs",
        &other,
        vec![check("default", common::true_program())],
    ));
    let _ = repo;
    let mut daemon = start(&fixture, settings);
    let before = fixture.records().len();
    let response = daemon.call(Method::TicketCreate, json!({"title": "Which?"}));
    assert_eq!(
        response.error.unwrap().code,
        ErrorCode::InvalidParams.code()
    );
    assert_eq!(fixture.records().len(), before);
    let created: dudes_proto::rpc::ticket::TicketCreateResult = daemon.ok(
        Method::TicketCreate,
        json!({"title": "Docs ticket", "project": "docs"}),
    );
    assert_eq!(daemon.read(created.card).ticket.project, "docs");
    daemon.stop();
}

#[test]
fn the_board_holds_at_most_512_open_tickets() {
    let fixture = Fixture::new();
    let (settings, _repo) = enrolled(&fixture);
    let mut records = Vec::new();
    for index in 0..512u128 {
        records.push(ticket_created(CardId::from_parts(5, index), "Seeded"));
    }
    seed(&fixture, records);
    let mut daemon = start(&fixture, settings);
    assert_eq!(daemon.board().len(), 512);
    assert_refused(
        &daemon.call(Method::TicketCreate, json!({"title": "One too many"})),
        TicketRefusal::LimitReached,
        CardStatus::Todo,
    );
    let seq = daemon.card(CardId::from_parts(5, 0)).seq;
    daemon.ok::<Value>(
        Method::TicketRemove,
        json!({"card": CardId::from_parts(5, 0), "expected_seq": seq}),
    );
    daemon.create("Room again");
    daemon.stop();
}

/// Retiring an agent returns what it was doing, and leaves a decision waiting for the operator.
#[test]
fn retiring_an_agent_keeps_its_ready_ticket_and_returns_the_rest() {
    let fixture = Fixture::new();
    let (settings, repo) = enrolled(&fixture);
    let agent = sample_agent();
    let (working, ready, queued) = (
        CardId::from_parts(4, 1),
        CardId::from_parts(4, 2),
        CardId::from_parts(4, 3),
    );
    seed(
        &fixture,
        vec![
            hire_into(&repo),
            ticket_created(working, "Working"),
            ticket_assigned(working, agent, 0),
            ticket_started(working, agent, 1, 1),
            ticket_created(ready, "Ready for you"),
            ticket_assigned(ready, agent, 0),
            ticket_started(ready, agent, 2, 1),
            to_status(ready, CardStatus::Doing, CardStatus::Review, 2),
            ticket_created(queued, "Queued"),
            ticket_assigned(queued, agent, 0),
        ],
    );
    let mut daemon = start(&fixture, settings);
    let response = daemon.call(
        Method::AgentRetire,
        json!({"agent": agent, "reason": "moved on"}),
    );
    assert!(response.error.is_none(), "{:?}", response.error);
    let ready_view = daemon.card(ready);
    assert_eq!(
        ready_view.status,
        CardStatus::Review,
        "the decision is not erased"
    );
    assert_eq!(
        ready_view.claimed_by,
        Some(agent),
        "the assignee is history"
    );
    for card in [working, queued] {
        let view = daemon.card(card);
        assert_eq!(view.status, CardStatus::Todo, "{card}");
        assert_eq!(view.status_by, Actor::System);
        assert_eq!(view.claimed_by, None);
        assert_eq!(daemon.read(card).position, None);
    }
    daemon.stop();
}

#[test]
fn the_card_view_of_a_ticket_is_unchanged_for_older_readers() {
    let fixture = Fixture::new();
    let (settings, _repo) = enrolled(&fixture);
    let mut daemon = start(&fixture, settings);
    let card = daemon.create("Shape");
    let board: Value = daemon.ok(Method::BoardList, json!({}));
    assert_eq!(
        board[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "department",
            "deps",
            "id",
            "seq",
            "status",
            "status_by",
            "title"
        ]
        .into_iter()
        .collect(),
        "board.list shows a ticket as the card it is, and never its note or its check"
    );
    let _ = card;
    daemon.stop();
}
