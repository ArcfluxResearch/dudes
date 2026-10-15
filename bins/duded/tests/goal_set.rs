//! `goal.set` over the production socket: goal text the method refuses before any append.
//!
//! The work gate puts the goal text into the agent's next Managed task, and no launch can carry U+0000.
//! A goal holding one was journaled and decided `Continue`, and no turn followed until a clean
//! `goal.set` replaced it. The method now refuses such text as invalid params, so nothing reaches
//! the journal: that is asserted on the disk, because a refusal behind the append would answer
//! the caller the same way.
#![cfg(unix)]

use std::sync::Arc;

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::rpc::agent::HireResult;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::goal::GoalSetResult;
use dudes_proto::rpc::{ErrorCode, Method, Response};
use dudes_proto::{CompletionCheckRef, LoopDecision, Record};
use dudes_rpc::Client;
use dudes_types::AgentId;
use serde_json::{Value, json};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock};

type SocketClient =
    Client<std::io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream>;

fn credential_of(settings: &duded::Settings) -> OperatorCredential {
    let text = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("the daemon minted a credential");
    OperatorCredential::new(text).expect("the credential is well formed")
}

/// A booted daemon, an operator connection presenting its credential, and one agent hired over
/// that connection with no goal yet.
fn hired(fixture: &Fixture) -> (duded::Settings, common::Running, SocketClient, AgentId) {
    let settings = fixture.settings();
    let daemon = boot(
        &settings,
        common::seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("boot");
    let running = common::start(daemon);
    let mut operator = Client::connect(settings.socket()).expect("connect");
    let hire = operator
        .call_as_operator(
            Method::AgentHire,
            Some(serde_json::to_value(common::hire_params(fixture.state.path())).expect("params")),
            credential_of(&settings),
        )
        .expect("agent.hire response");
    let agent = serde_json::from_value::<HireResult>(hire.result.expect("hired"))
        .expect("a hire result")
        .agent;
    (settings, running, operator, agent)
}

fn set_goal(
    operator: &mut SocketClient,
    settings: &duded::Settings,
    agent: AgentId,
    text: &str,
) -> Response {
    let mut params = common::goal_params(agent, &["/usr/bin/false"]);
    text.clone_into(&mut params.text);
    operator
        .call_as_operator(
            Method::GoalSet,
            Some(serde_json::to_value(params).expect("params")),
            credential_of(settings),
        )
        .expect("goal.set response")
}

fn goal_texts(records: &[Record], agent: AgentId) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::GoalSet {
                agent: who, goal, ..
            } if *who == agent => Some(goal.text.clone()),
            _ => None,
        })
        .collect()
}

/// On a real operator connection, a `goal.set`
/// whose text holds U+0000 is refused `-32602` with no `data`, and the journal does not move — no
/// `GoalSet`, and the agent keeps the goal it had.
///
/// Control, first: the same connection sets U+0001 in the same place. It applies once, with the
/// bytes sent, and its turn is issued and launched.
///
/// Mutants: deleting the check appends the U+0000 goal; refusing every `Cc` refuses the control;
/// refusing `-32004` fails the code assertion.
#[test]
fn goal_set_with_nul_is_refused_before_append() {
    let fixture = Fixture::new();
    let (settings, mut running, mut operator, agent) = hired(&fixture);

    let applied = set_goal(&mut operator, &settings, agent, "ship\u{1}it");
    assert!(applied.error.is_none(), "control goal: {applied:?}");
    let control = serde_json::from_value::<GoalSetResult>(applied.result.expect("a result"))
        .expect("a goal.set result")
        .goal;
    assert_eq!(
        goal_texts(&fixture.records(), agent),
        ["ship\u{1}it"],
        "control: one goal, with the bytes sent"
    );
    fixture.await_record("the control goal's launch", |record| {
        matches!(record, Record::TurnSpawned { agent: who, .. } if *who == agent).then_some(())
    });

    let before = fixture.records();
    let refused = set_goal(&mut operator, &settings, agent, "ship\u{0}it");
    let error = refused.error.expect("a goal holding U+0000 is refused");
    assert_eq!(error.code, ErrorCode::InvalidParams.code(), "{error:?}");
    assert_eq!(error.data, None, "invalid params carry no data");
    assert_eq!(
        fixture.records(),
        before,
        "the refusal is in front of any append"
    );
    assert_eq!(
        common::goal_of(&before),
        Some(control),
        "the agent keeps the control goal"
    );
    running.stop(ExitCause::Ended);
}

/// A refused U+0000 goal is never decided on, so the
/// clean `goal.set` that follows is the agent's first `Continue`, and its turn is issued with the
/// clean text. Were the U+0000 goal journaled, it would be decided `Continue` with no
/// `TurnIssued`, because the adapter refuses a task holding U+0000, until a clean goal replaced it.
#[test]
fn clean_goal_after_a_refused_nul_goal_issues_its_turn() {
    let fixture = Fixture::new();
    let (settings, mut running, mut operator, agent) = hired(&fixture);
    let decided = |records: &[Record]| {
        records
            .iter()
            .filter(|record| {
                matches!(
                    record,
                    Record::LoopDecided {
                        agent: who,
                        decision: LoopDecision::Continue { .. },
                        ..
                    } if *who == agent
                )
            })
            .count()
    };

    let refused = set_goal(&mut operator, &settings, agent, "ship\u{0}it");
    assert_eq!(
        refused.error.expect("refused").code,
        ErrorCode::InvalidParams.code()
    );
    assert_eq!(decided(&fixture.records()), 0, "nothing was decided on it");

    let applied = set_goal(&mut operator, &settings, agent, "ship it");
    assert!(applied.error.is_none(), "clean goal: {applied:?}");
    let task = fixture.await_record("the clean goal's turn", |record| match record {
        Record::TurnIssued {
            agent: who, task, ..
        } if *who == agent => Some(task.clone()),
        _ => None,
    });
    let records = fixture.records();
    assert_eq!(goal_texts(&records, agent), ["ship it"]);
    assert_eq!(decided(&records), 1, "one Continue, and it issued the turn");
    assert!(
        task.contains("ship it"),
        "the task carries the goal: {task:?}"
    );
    running.stop(ExitCause::Ended);
}

/// A `goal.set` that names its check records that name, its label trimmed, and a revision of the
/// check that a reader can recompute, and the record replays at the next boot. One without a name
/// records none, and a named board check is invalid params before any append.
///
/// Mutants: recording `None` for a named check fails the first arm; keeping the replay rule that
/// refused a name on a plain `GoalSet` fails the boot; accepting a named board check appends a
/// goal.
#[test]
fn goal_set_records_the_check_name_it_is_given() {
    let fixture = Fixture::new();
    let (settings, mut running, mut operator, agent) = hired(&fixture);
    let call = |operator: &mut SocketClient, params: Value| {
        operator
            .call_as_operator(Method::GoalSet, Some(params), credential_of(&settings))
            .expect("goal.set response")
    };
    let params = |check: Option<Value>| {
        let mut params =
            serde_json::to_value(common::goal_params(agent, &["/usr/bin/false"])).unwrap();
        if let Some(check) = check {
            params["check"] = check;
        }
        params
    };

    let unnamed = call(&mut operator, params(None));
    assert!(unnamed.error.is_none(), "control: {unnamed:?}");
    let named = call(
        &mut operator,
        params(Some(
            json!({"id": "contributing", "label": " CONTRIBUTING.md exists "}),
        )),
    );
    assert!(named.error.is_none(), "named: {named:?}");
    let checks: Vec<Option<CompletionCheckRef>> = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::GoalSet { goal, .. } => Some(goal.completion_check),
            _ => None,
        })
        .collect();
    let [None, Some(check)] = checks.as_slice() else {
        panic!("one unnamed goal, then one named goal: {checks:?}");
    };
    assert_eq!(
        (check.id.as_str(), check.label.as_str()),
        ("contributing", "CONTRIBUTING.md exists")
    );
    assert_eq!(
        serde_json::to_value(check.revision).unwrap(),
        common::check_revision_as(
            "contributing",
            "CONTRIBUTING.md exists",
            &["/usr/bin/false"],
            600,
            true
        )
    );

    let before = fixture.records();
    let mut board = params(Some(json!({"id": "card", "label": "Card done"})));
    board["predicate"] =
        json!({"board_state": {"card": "01ARZ3NDEKTSV4RRFFQ69G5FAZ", "status": "done"}});
    let refused = call(&mut operator, board);
    assert_eq!(
        refused.error.expect("a named board check").code,
        ErrorCode::InvalidParams.code()
    );
    assert_eq!(fixture.records(), before, "nothing was appended");
    running.stop(ExitCause::Ended);

    let again = boot(
        &settings,
        common::seams(
            &settings,
            SharedHost::new(),
            Arc::new(ScriptedRunner::default()),
            Arc::new(TestClock::new()),
            Witness::default(),
        ),
    )
    .expect("the named goal replays at boot");
    common::start(again).stop(ExitCause::Ended);
}
