//! Sites that once assumed a single working copy answer for an unconfined multi-repository hire.

use super::*;
use crate::floor_chat::{ChatOutcome, ChatRequest, ChatVerb};
use dudes_proto::AutomaticHold;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::{Method, Response};
use dudes_rpc::Client;
use dudes_types::AgentId;
use serde_json::{Value, json};

type Socket =
    Client<std::io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream>;

/// Answers every open as a failure, after remembering the launch it was handed.
pub(super) struct RecordingPtyHost(
    pub(super) Arc<std::sync::Mutex<Vec<dudes_providers::launch::LaunchSpec>>>,
);

impl crate::ports::PtyHostPort for RecordingPtyHost {
    fn open(
        &mut self,
        spec: dudes_providers::launch::LaunchSpec,
        _cols: u16,
        _rows: u16,
    ) -> Result<crate::ports::OpenedPty, crate::ports::PtyOpenFailure> {
        self.0.lock().unwrap().push(spec);
        Err(crate::ports::PtyOpenFailure::Failed(std::io::Error::other(
            "the test PTY host records launches only",
        )))
    }

    fn write(&mut self, _: dudes_proto::rpc::pty::PtyId, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("the test PTY host is closed"))
    }

    fn resize(&mut self, _: dudes_proto::rpc::pty::PtyId, _: u16, _: u16) -> std::io::Result<()> {
        Err(std::io::Error::other("the test PTY host is closed"))
    }

    fn subscribe(
        &mut self,
        _: dudes_proto::rpc::pty::PtyId,
    ) -> Option<crate::pty_output::OutputSubscription> {
        None
    }

    fn close(&mut self, _: dudes_proto::rpc::pty::PtyId) -> crate::ports::PtyCloseOutcome {
        crate::ports::PtyCloseOutcome::AlreadyClosed
    }

    fn shutdown(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn call(socket: &mut Socket, setup: &Setup, method: Method, params: Value) -> Response {
    let credential = OperatorCredential::new(
        std::fs::read_to_string(
            setup
                .settings
                .runtime_dir
                .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
        )
        .unwrap(),
    )
    .unwrap();
    socket
        .call_as_operator(method, Some(params), credential)
        .unwrap()
}

fn session() -> crate::floor_session::SessionIdentity {
    crate::floor_session::FloorSessions::new(
        "127.0.0.1:31337".parse().unwrap(),
        Arc::new(common::TestClock::new()),
    )
    .admitted_for_test("127.0.0.1:31337", "http://127.0.0.1:31337")
    .0
}

pub(in crate::runtime) fn send_chat(running: &common::Running, agent: AgentId) {
    let body = json!({
        "request_id": "A".repeat(43),
        "agent": agent.to_string(),
        "text": "hello",
    });
    let request = ChatRequest::parse(ChatVerb::Send, &serde_json::to_vec(&body).unwrap()).unwrap();
    assert!(matches!(
        running.handle.chat(session(), request).unwrap(),
        ChatOutcome::Queued { .. }
    ));
}

pub(super) fn world_of(setup: &Setup) -> dudes_supervisor::World {
    let mut world = dudes_supervisor::World::default();
    for (lsn, entry) in setup.fixture.entries() {
        world.apply(lsn, &entry);
    }
    world
}

fn held_for_missing_copy(setup: &Setup, agent: AgentId) -> dudes_types::WorktreeId {
    setup
        .fixture
        .await_record("the missing copy hold", |record| match record {
            Record::AgentHeld {
                agent: held,
                cause: AutomaticHold::WorktreeMissing { worktree },
                ..
            } if *held == agent => Some(*worktree),
            _ => None,
        })
}

fn issued_turns(setup: &Setup) -> usize {
    setup
        .fixture
        .records()
        .iter()
        .filter(|record| format!("{:?}", record.kind()).contains("Issued"))
        .count()
}

fn assert_no_turn_started(setup: &Setup, issued_before: usize) {
    assert!(setup.host.spawned().is_empty());
    assert!(!setup.fixture.records().iter().any(|record| matches!(
        record,
        Record::TurnSpawned { .. } | Record::TurnEnded { .. }
    )));
    assert_eq!(
        issued_turns(setup),
        issued_before,
        "no turn was issued after the copy went missing"
    );
}

#[test]
fn a_chat_message_to_an_unconfined_agent_whose_second_copy_is_gone_holds_it_without_a_process() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    let copies = setup.copies_of(agent);
    std::fs::rename(&copies[1], copies[1].with_extension("moved")).unwrap();
    let issued_before = issued_turns(&setup);
    send_chat(&running, agent);
    let worktree = held_for_missing_copy(&setup, agent);
    running.stop(ExitCause::Ended);
    let records = setup.fixture.records();
    let leased = records
        .iter()
        .filter_map(Record::worktree_lease)
        .find(|lease| lease.path == copies[1])
        .map(|lease| lease.worktree);
    assert_eq!(Some(worktree), leased);
    assert_no_turn_started(&setup, issued_before);
}

#[test]
fn replacing_a_goal_of_an_unconfined_agent_is_refused_with_a_reason_and_appends_nothing() {
    let mut setup = Setup::new();
    setup.settings.floor_control = crate::settings::FloorControl::On {
        goal: true,
        retire: false,
        renew: true,
    };
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let before = setup.fixture.records();
    let response = call(
        &mut socket,
        &setup,
        Method::GoalReplace,
        json!({
            "request_id": "ab".repeat(32),
            "agent": agent,
            "expected_goal": dudes_types::GoalId::from_parts(8, 1),
            "text": "make it pass",
            "predicate": dudes_proto::Predicate::Command {
                argv: vec!["/usr/bin/true".into()],
                timeout_s: 1,
            },
            "check": {"id": "unit", "label": "Unit check"},
            "may_already_pass": true,
        }),
    );
    let error = response.error.expect("a replacement is refused");
    assert_eq!(error.message, crate::runtime::GOAL_OUTSIDE_TICKET_REFUSAL);
    assert_eq!(setup.fixture.records(), before, "a refusal appends nothing");
    drop(socket);
    running.stop(ExitCause::Ended);
}

#[test]
fn attaching_an_unconfined_agent_is_refused_with_a_reason_and_appends_nothing() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    running.stop(ExitCause::Ended);
    learn_a_session(&setup, agent);
    let mut daemon = setup.boot_unconfined();
    let before = setup.fixture.records();
    let error = daemon
        .attach(agent, dudes_proto::Actor::System, None, 80, 24)
        .expect_err("a multi-repository attach waits for its instruction record");
    assert_eq!(
        error.message,
        crate::runtime::ATTACH_MULTI_REPOSITORY_REFUSAL
    );
    assert_eq!(setup.fixture.records(), before);
    assert!(setup.pty_opened.lock().unwrap().is_empty());
}

const SESSION: &str = "550e8400-e29b-41d4-a716-446655440000";

fn learn_a_session(setup: &Setup, agent: AgentId) {
    let (mut writer, _) =
        dudes_journal::open(&setup.fixture.journal_dir(), setup.settings.journal).unwrap();
    writer
        .append(&dudes_proto::Entry {
            at: dudes_types::Timestamp::from_unix_millis(1),
            record: Record::SessionLearned {
                agent,
                turn: dudes_types::TurnId::NIL,
                session: dudes_types::SessionId::from(SESSION),
            },
        })
        .unwrap();
    drop(writer);
    std::fs::remove_file(
        setup
            .fixture
            .state
            .path()
            .join(crate::boot::PROJECTION_FILE),
    )
    .ok();
}

#[test]
fn a_confined_hire_is_still_not_attachable() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start(true);
    let agent = hires.hire(params).unwrap().agent;
    running.stop(ExitCause::Ended);
    learn_a_session(&setup, agent);
    let mut daemon = setup.boot(true);
    let error = daemon
        .attach(agent, dudes_proto::Actor::System, None, 80, 24)
        .expect_err("a confined hire is not attached");
    assert!(error.message.contains("cannot be attached"), "{error:?}");
    assert!(setup.pty_opened.lock().unwrap().is_empty());
}

#[test]
fn a_goal_outside_a_ticket_is_refused_with_a_reason_for_an_unconfined_agent() {
    let mut setup = Setup::new();
    setup.settings.floor_control = crate::settings::FloorControl::On {
        goal: true,
        retire: false,
        renew: true,
    };
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let before = setup.fixture.records();
    let response = call(
        &mut socket,
        &setup,
        Method::GoalSet,
        json!({
            "agent": agent,
            "text": "make it pass",
            "predicate": dudes_proto::Predicate::Command {
                argv: vec!["true".into()],
                timeout_s: 1,
            },
            "budget": {"usd": null, "tokens": null},
            "hop_cap": 4,
            "may_already_pass": true,
        }),
    );
    let error = response.error.expect("a goal outside a ticket is refused");
    assert_eq!(error.message, crate::runtime::GOAL_OUTSIDE_TICKET_REFUSAL);
    assert_eq!(setup.fixture.records(), before, "a refusal appends nothing");

    let body = json!({
        "request_id": "A".repeat(43),
        "agent": agent.to_string(),
        "goal": dudes_types::GoalId::from_parts(8, 1).to_string(),
        "text": "make it pass",
    });
    let request =
        crate::floor_goal::GoalRequest::parse(&serde_json::to_vec(&body).unwrap()).unwrap();
    let outcome = running.handle.goal(session(), request).unwrap();
    assert!(
        matches!(
            outcome,
            crate::floor_goal::GoalOutcome::Refused {
                reason: "multi_repository",
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(setup.fixture.records(), before);
    drop(socket);
    running.stop(ExitCause::Ended);
}

#[test]
fn only_an_unconfined_multi_repository_hire_is_refused_a_goal_outside_a_ticket() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let unconfined = hires.hire(params).unwrap().agent;
    running.stop(ExitCause::Ended);
    let world = world_of(&setup);
    let repositories = &world.agents[&unconfined].repositories;
    assert_eq!(
        crate::runtime::refuse_goal_outside_a_ticket(repositories)
            .unwrap_err()
            .message,
        crate::runtime::GOAL_OUTSIDE_TICKET_REFUSAL
    );
    let dudes_proto::repository::HireRepositories::Scoped(hire) = repositories else {
        panic!("a repository hire");
    };
    let mut confined = hire.clone();
    confined.policy = dudes_proto::repository::RepositoryPolicy::WorkingTreeV1;
    assert!(
        crate::runtime::refuse_goal_outside_a_ticket(
            &dudes_proto::repository::HireRepositories::Scoped(confined)
        )
        .is_ok()
    );
    assert!(
        crate::runtime::refuse_goal_outside_a_ticket(
            &dudes_proto::repository::HireRepositories::default()
        )
        .is_ok()
    );
}

#[test]
fn the_git_view_of_an_unconfined_hire_lists_every_copy_primary_first() {
    use crate::floor_git::{GitRequest, GitTarget};
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    running.stop(ExitCause::Ended);
    let mut world = world_of(&setup);
    let copies = setup.copies_of(agent);
    let primary_first: Vec<_> = {
        let dudes_proto::repository::HireRepositories::Scoped(hire) =
            &world.agents[&agent].repositories
        else {
            panic!("a repository hire");
        };
        hire.repositories
            .primary_first()
            .map(|entry| entry.repo_id.clone())
            .collect()
    };
    let ids = |target: Option<GitTarget>| match target {
        Some(GitTarget::Repositories { entries, .. }) => entries
            .into_iter()
            .map(|(id, tree)| {
                assert!(tree.is_ok(), "{id:?}");
                id
            })
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    let request = |selection: &str| {
        let body = if selection.is_empty() {
            json!({"agent_id": agent.to_string()})
        } else {
            json!({"agent_id": agent.to_string(), "repositories": {"scope": selection}})
        };
        GitRequest::parse(&serde_json::to_vec(&body).unwrap()).unwrap()
    };
    assert_eq!(copies.len(), primary_first.len());
    for selection in ["", "hired"] {
        assert_eq!(
            ids(GitTarget::selected(&world, &request(selection))),
            primary_first,
            "{selection:?}"
        );
    }
    let dudes_proto::repository::HireRepositories::Scoped(hire) =
        &mut world.agents.get_mut(&agent).unwrap().repositories
    else {
        panic!("a repository hire");
    };
    hire.policy = dudes_proto::repository::RepositoryPolicy::WorkingTreeV1;
    let catalog_order: Vec<_> = hire
        .repositories
        .entries()
        .iter()
        .map(|entry| entry.repo_id.clone())
        .collect();
    assert_eq!(
        ids(GitTarget::selected(&world, &request("hired"))),
        catalog_order,
        "a confined hire keeps its recorded order"
    );
    assert!(matches!(
        GitTarget::selected(&world, &request("")),
        Some(GitTarget::Refused(dudes_proto::GitState::InvalidSelection))
    ));
}
