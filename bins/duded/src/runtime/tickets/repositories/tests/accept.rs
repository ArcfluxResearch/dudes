//! Accepting a ticket round of a multi-repository hire, per policy.

use super::*;
use dudes_proto::rpc::CurrentMethod;
use dudes_proto::rpc::recovery::{
    RecoveryChoice, RecoveryInspectParams, RecoveryInspectResult, RecoverySubmitParams,
    RecoverySubmitResult, RecoveryWaitStatus,
};

fn changed_skill(setup: &Setup, agent: AgentId, copy: usize) -> impl FnOnce() + use<> {
    let skill = setup.copies_of(agent)[copy].join(".claude/skills/review/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, "review v1").unwrap();
    move || std::fs::write(&skill, "review v2, edited during the round").unwrap()
}

#[test]
fn an_instruction_change_in_a_second_copy_blocks_accept_until_the_operator_reviews_it() {
    let setup = native_setup();
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let card = assigned_unconfined_ticket(&setup, agent, &mut socket);
    let detail = finish_the_round(&setup, &mut socket, card, changed_skill(&setup, agent, 1));
    assert_eq!(detail.card.status, dudes_types::CardStatus::Blocked);
    let review = detail.ticket.instruction_review.as_ref().unwrap();
    assert!(!review.coverage_unknown);
    assert_eq!(review.changed_paths.len(), 1, "{:?}", review.changed_paths);
    let changed = &review.changed_paths[0];
    assert!(
        format!("{:?}", changed.path).contains(".claude/skills/review/SKILL.md"),
        "{changed:?}"
    );
    assert!(changed.repo_id.is_some(), "{changed:?}");
    let unreviewed = call(
        &mut socket,
        &setup,
        Method::TicketAccept,
        json!({"card": card, "expected_seq": detail.card.seq}),
    );
    assert!(unreviewed.error.is_some(), "accepted without the review");
    let accepted = accept(&mut socket, &setup, &detail);
    assert!(accepted.error.is_none(), "{:?}", accepted.error);
    assert_eq!(
        setup
            .fixture
            .records()
            .iter()
            .filter(|record| matches!(
                record,
                Record::TicketInstructionReviewed {
                    action: dudes_proto::InstructionReviewAction::Accept { .. },
                    ..
                }
            ))
            .count(),
        1
    );
    drop(socket);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn a_copy_that_is_gone_at_the_end_of_a_round_blocks_accept() {
    let setup = native_setup();
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let card = assigned_unconfined_ticket(&setup, agent, &mut socket);
    let copy = setup.copies_of(agent)[1].clone();
    let detail = finish_the_round(&setup, &mut socket, card, || {
        std::fs::rename(&copy, copy.with_extension("moved")).unwrap();
    });
    let refused = accept(&mut socket, &setup, &detail);
    assert_eq!(
        refused.error.map(|error| error.message),
        Some(
            "Instruction coverage is incomplete in a hired repository. Ask for changes before accepting this work."
                .to_owned()
        ),
        "{:?}",
        detail.card.status
    );
    drop(socket);
    running.stop(crate::ExitCause::Ended);
}

fn recovery(socket: &mut Socket, setup: &Setup, method: CurrentMethod, params: Value) -> Value {
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
    let response = socket
        .call_current_as_operator(method, Some(params), credential)
        .unwrap();
    assert!(response.error.is_none(), "{:?}", response.error);
    response.result.unwrap()
}

#[test]
fn an_interrupted_unconfined_ticket_turn_is_recovered_and_launches_again() {
    let mut setup = native_setup();
    for check in &mut setup.settings.enrollment.profiles[0].checks {
        check.predicate = Predicate::Command {
            argv: vec![common::false_program().into()],
            timeout_s: 1,
        };
    }
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let card = assigned_unconfined_ticket(&setup, agent, &mut socket);
    let turn = setup
        .fixture
        .await_record("the ticket turn", |record| match record {
            Record::TurnSpawned { turn, .. } => Some(*turn),
            _ => None,
        });
    let key = common::reap_native_child(
        &setup.fixture,
        &setup.host,
        dudes_supervisor::Input::ChildExited {
            report: dudes_proto::TurnReport::default(),
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: dudes_proto::ExitStatus::UNKNOWN,
            usage: None,
            model_usage: std::collections::BTreeMap::new(),
            outcome: dudes_proto::TurnOutcome::Vanished,
            last_message: None,
        },
    );
    setup.host.await_native_acknowledged(&key);
    setup
        .fixture
        .await_record("recovery is required", |record| {
            matches!(record, Record::RecoveryRequired(required) if required.binding.work.turn == turn)
                .then_some(())
        });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let inspected = loop {
        let inspected: RecoveryInspectResult = serde_json::from_value(recovery(
            &mut socket,
            &setup,
            CurrentMethod::RecoveryInspect,
            serde_json::to_value(RecoveryInspectParams { agent }).unwrap(),
        ))
        .unwrap();
        if matches!(inspected, RecoveryInspectResult::Available { .. })
            || std::time::Instant::now() > deadline
        {
            break inspected;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let RecoveryInspectResult::Available {
        expected, waiting, ..
    } = inspected
    else {
        panic!("the interrupted turn offers recovery: {inspected:?}");
    };
    assert_eq!(
        (waiting.instruction_review, waiting.instruction_comparison),
        (RecoveryWaitStatus::Ready, RecoveryWaitStatus::Ready)
    );
    let submitted: RecoverySubmitResult = serde_json::from_value(recovery(
        &mut socket,
        &setup,
        CurrentMethod::RecoverySubmit,
        serde_json::to_value(RecoverySubmitParams {
            request_id: Hex32::new([61; 32]),
            expected: *expected,
            choice: RecoveryChoice::FreshWithRecap,
        })
        .unwrap(),
    ))
    .unwrap();
    assert!(matches!(submitted, RecoverySubmitResult::Accepted { .. }));
    let relaunched = setup
        .fixture
        .await_record("the recovered turn", |record| match record {
            Record::TurnSpawned { turn: next, .. } if *next != turn => Some(*next),
            _ => None,
        });
    assert_ne!(relaunched, turn);
    assert_eq!(setup.host.spawned().len(), 2);
    assert_eq!(
        setup
            .fixture
            .records()
            .iter()
            .filter(|record| matches!(record, Record::TurnIssuedWithRepositories(_)))
            .count(),
        2,
        "one original issue and one recovery issue for card {card}"
    );
    drop(socket);
    running.stop(crate::ExitCause::Ended);
}

fn chat(running: &common::Running, agent: AgentId, request_id: &str) {
    use crate::floor_chat::{ChatOutcome, ChatRequest, ChatVerb};
    let sessions = crate::floor_session::FloorSessions::new(
        "127.0.0.1:31337".parse().unwrap(),
        std::sync::Arc::new(common::TestClock::new()),
    );
    let session = sessions
        .admitted_for_test("127.0.0.1:31337", "http://127.0.0.1:31337")
        .0;
    let body = json!({"request_id": request_id, "agent": agent.to_string(), "text": "hello"});
    let request = ChatRequest::parse(ChatVerb::Send, &serde_json::to_vec(&body).unwrap()).unwrap();
    assert!(matches!(
        running.handle.chat(session, request).unwrap(),
        ChatOutcome::Queued { .. }
    ));
}

fn reap_completed(setup: &Setup, turn: dudes_types::TurnId) {
    let key = common::reap_native_child(
        &setup.fixture,
        &setup.host,
        dudes_supervisor::Input::ChildExited {
            report: dudes_proto::TurnReport::default(),
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: dudes_proto::ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: None,
            model_usage: std::collections::BTreeMap::new(),
            outcome: dudes_proto::TurnOutcome::Completed,
            last_message: None,
        },
    );
    setup.host.await_native_acknowledged(&key);
}

fn spawned_turns(setup: &Setup) -> Vec<dudes_types::TurnId> {
    setup
        .fixture
        .records()
        .iter()
        .filter_map(|record| match record {
            Record::TurnSpawned { turn, .. } => Some(*turn),
            _ => None,
        })
        .collect()
}

fn next_turn(setup: &Setup, known: &[dudes_types::TurnId]) -> dudes_types::TurnId {
    setup
        .fixture
        .await_record("the chat turn", |record| match record {
            Record::TurnSpawned { turn, .. } if !known.contains(turn) => Some(*turn),
            _ => None,
        })
}

fn chat_after_a_ticket_round(restart: bool) {
    let mut setup = native_setup();
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    let mut socket = Client::connect(setup.settings.socket()).unwrap();
    let card = assigned_unconfined_ticket(&setup, agent, &mut socket);
    let detail = finish_the_round(&setup, &mut socket, card, || {});
    let accepted = accept(&mut socket, &setup, &detail);
    assert!(accepted.error.is_none(), "{:?}", accepted.error);
    drop(socket);
    let launches = if restart {
        running.stop(crate::ExitCause::Ended);
        setup.host = common::SharedHost::with_native_evidence(setup.fixture.journal_dir());
        running = common::start(setup.boot_unconfined());
        0
    } else {
        1
    };
    let known = spawned_turns(&setup);
    chat(&running, agent, &"A".repeat(43));
    let turn = next_turn(&setup, &known);
    let spawned = setup.host.spawned();
    assert_eq!(spawned.len(), launches + 1);
    setup.assert_primary_with_others_added(&spawned[launches], agent);
    reap_completed(&setup, turn);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn chat_is_delivered_after_a_ticket_round() {
    chat_after_a_ticket_round(false);
}

#[test]
fn chat_is_delivered_after_a_ticket_round_and_a_restart() {
    chat_after_a_ticket_round(true);
}
