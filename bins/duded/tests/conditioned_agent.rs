//! Conditioned `agent.answer` and `agent.steer` through the production dispatcher and journal.
#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use duded::ports::Ask;
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::rpc::agent::HireResult;
use dudes_proto::rpc::cred::OperatorCredential;
use dudes_proto::rpc::{ErrorCode, JSONRPC, Method, RefusalGate, RefusedData, Request, RequestId};
use dudes_proto::{ConditionedMethod, Event, EventKind, PeerProvenance, Record};
use dudes_rpc::testing::{AuthorityMutationConnection, FakeMintedTokens};
use dudes_rpc::{Client, Dispatcher, Outbound, PeerClassifier, Subscribers};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{HookId, Input};
use dudes_types::{Provenance, Timestamp};
use serde_json::json;

mod common;

use common::{Fixture, ScriptedColumn, ScriptedRunner, SharedHost, TestClock};

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

fn token_after_mint_pump(
    fixture: &Fixture,
    tokens: &dudes_supervisor::testing::FakeTokens,
    handle: &duded::Handle,
    agent: dudes_types::AgentId,
) -> dudes_proto::rpc::hook::HookToken {
    let hash = fixture.await_record("the exact agent's mint", |record| match record {
        Record::HookTokenMinted {
            agent: owner,
            token_hash,
        } if *owner == agent => Some(token_hash.clone()),
        _ => None,
    });
    let minted = fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| match entry.record {
            Record::HookTokenMinted {
                agent: owner,
                token_hash,
            } if owner == agent && token_hash == hash => Some(lsn),
            _ => None,
        })
        .expect("the exact durable mint position");
    let token = tokens.token_of(&hash);
    let completed = handle
        .doctor()
        .expect("read after the mint's existing pump");
    assert!(completed.lsn >= minted);
    token
}

/// Start in the pre-agent column, retain the accepted connection's authority across the
/// first production hire, and return the hired agent.
fn hire_on_authorized_connection(
    fixture: &Fixture,
) -> (
    duded::Settings,
    common::Running,
    Arc<ScriptedColumn>,
    SocketClient,
    dudes_types::AgentId,
    dudes_supervisor::testing::FakeTokens,
) {
    let settings = fixture.settings();
    let column = ScriptedColumn::new(PeerProvenance::NoRecordedAgent);
    let parts = common::seams_with(
        &settings,
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
        Arc::clone(&column) as Arc<dyn dudes_rpc::PeerResolver>,
    );
    let tokens = parts.rng.tokens();
    let daemon = boot(&settings, parts).expect("boot");
    let running = common::start(daemon);
    let mut operator = Client::connect(settings.socket()).expect("connect before hire");
    let response = operator
        .call_as_operator(
            Method::AgentHire,
            Some(
                serde_json::to_value(common::hire_params(fixture.state.path()))
                    .expect("hire params"),
            ),
            credential_of(&settings),
        )
        .expect("production hire response");
    assert!(response.error.is_none(), "agent.hire: {response:?}");
    let hired: HireResult =
        serde_json::from_value(response.result.expect("hire result")).expect("typed hire result");
    (settings, running, column, operator, hired.agent, tokens)
}

struct MutationReady {
    tokens: dudes_supervisor::testing::FakeTokens,
    settings: duded::Settings,
    running: common::Running,
    column: Arc<ScriptedColumn>,
    agent: dudes_types::AgentId,
    dispatcher: Dispatcher,
    connection: AuthorityMutationConnection,
}

/// Retain a real pre-agent lease but dispatch with a failing provenance. Production cannot create
/// this pair; it exists only so deleting the provenance predicate changes observable behavior.
fn ready_with_mutation(fixture: &Fixture) -> MutationReady {
    let settings = fixture.settings();
    let column = ScriptedColumn::new(PeerProvenance::NoRecordedAgent);
    let parts = common::seams_with(
        &settings,
        SharedHost::new(),
        Arc::new(ScriptedRunner::default()),
        Arc::new(TestClock::new()),
        Witness::default(),
        Arc::clone(&column) as Arc<dyn dudes_rpc::PeerResolver>,
    );
    let tokens = parts.rng.tokens();
    let daemon = boot(&settings, parts).expect("boot");
    let authority = Arc::clone(daemon.operator_authority());
    let connection = AuthorityMutationConnection::acquire(&authority, PeerProvenance::Unresolvable)
        .expect("the mutation connection acquires in the pre-agent epoch");
    let dispatcher = duded::boot::dispatch(
        &settings,
        PeerClassifier::new(credential_of(&settings), Arc::new(FakeMintedTokens::none())),
        duded::hooks::Ingress::new(daemon.handle(), Arc::new(TestClock::new())),
        &fixture.journal_dir(),
        &Arc::new(Subscribers::new()),
        Arc::new(AtomicU64::new(0)),
        authority,
    );
    let running = common::start(daemon);
    let mut operator = Client::connect(settings.socket()).expect("connect before hire");
    let response = operator
        .call_as_operator(
            Method::AgentHire,
            Some(
                serde_json::to_value(common::hire_params(fixture.state.path()))
                    .expect("hire params"),
            ),
            credential_of(&settings),
        )
        .expect("production hire response");
    assert!(response.error.is_none(), "agent.hire: {response:?}");
    let hired: HireResult =
        serde_json::from_value(response.result.expect("hire result")).expect("typed hire result");
    MutationReady {
        tokens,
        settings,
        running,
        column,
        agent: hired.agent,
        dispatcher,
        connection,
    }
}

fn mutation_call(ready: &MutationReady, method: Method, text: &str) -> dudes_proto::rpc::Response {
    ready.connection.dispatch(
        &ready.dispatcher,
        &Request {
            jsonrpc: JSONRPC.to_owned(),
            id: RequestId::Number(313),
            method: method.name().to_owned(),
            params: Some(json!({ "agent": ready.agent, "text": text })),
            v: None,
            cred: Some(credential_of(&ready.settings)),
        },
        &Arc::new(Outbound::sink()),
    )
}

/// Protects durable answer and wait release: the RPC response follows the fsynced answer, and only
/// that answer clears the agent's open question.
#[test]
fn operator_answer_applies_after_fsync_and_clears_waiting() {
    let fixture = Fixture::new();
    let (settings, mut running, column, mut operator, agent, tokens) =
        hire_on_authorized_connection(&fixture);
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let token = token_after_mint_pump(&fixture, &tokens, &running.handle, agent);
    column.set(PeerProvenance::Agent(agent));
    let mut asker = Client::connect(settings.socket()).expect("agent connection");
    let ask = |client: &mut Client<_, _>, text: &str| {
        client
            .call(
                Method::AgentAsk,
                Some(json!({ "token": token.expose(), "text": text })),
            )
            .expect("agent.ask response")
    };
    let first = ask(&mut asker, "which branch?");
    assert!(first.error.is_none(), "first question: {first:?}");

    let answered = operator
        .call_as_operator(
            Method::AgentAnswer,
            Some(json!({ "agent": agent, "text": "main" })),
            credential_of(&settings),
        )
        .expect("agent.answer response");
    assert!(answered.error.is_none(), "agent.answer: {answered:?}");
    assert!(fixture.records().iter().any(|record| matches!(
        record,
        Record::HumanAnswer {
            agent: who,
            text,
            provenance: Some(PeerProvenance::NoRecordedAgent),
            browser_answer: None,
        } if *who == agent && text == "main"
    )));

    let next = ask(&mut asker, "and the tag?");
    assert!(
        next.error.is_none(),
        "the durable answer reopened the question slot: {next:?}"
    );
    running.stop(ExitCause::Ended);
}

/// Named mutant: `delete_agent_answer_provenance_predicate` makes this apply, writes
/// `HumanAnswer`, and lets the second question through.
#[test]
fn answer_provenance_rejection_is_durable_and_preserves_waiting() {
    let fixture = Fixture::new();
    let mut ready = ready_with_mutation(&fixture);
    common::set_goal(
        &ready.running.handle,
        common::goal_params(ready.agent, &["/usr/bin/true"]),
    );
    let token = token_after_mint_pump(&fixture, &ready.tokens, &ready.running.handle, ready.agent);
    ready.column.set(PeerProvenance::Agent(ready.agent));
    let mut asker = Client::connect(ready.settings.socket()).expect("agent connection");
    let first = asker
        .call(
            Method::AgentAsk,
            Some(json!({ "token": token.expose(), "text": "which branch?" })),
        )
        .expect("first agent.ask response");
    assert!(first.error.is_none(), "first question: {first:?}");

    let refused = mutation_call(&ready, Method::AgentAnswer, "main");
    let error = refused.error.expect("failing provenance is suppressed");
    assert_eq!(error.code, ErrorCode::Refused.code());
    let data: RefusedData =
        serde_json::from_value(error.data.expect("typed refusal")).expect("RefusedData");
    assert_eq!(data.gate, RefusalGate::ConditionedEffect);
    let records = fixture.records();
    assert!(records.iter().any(|record| matches!(
        record,
        Record::ConditionedEffectSuppressed {
            method: ConditionedMethod::AgentAnswer,
            provenance: PeerProvenance::Unresolvable,
            reason,
            ..
        } if reason == "peer provenance does not satisfy the effect"
    )));
    assert!(!records.iter().any(|record| matches!(
        record,
        Record::HumanAnswer { agent, .. } if *agent == ready.agent
    )));

    let second = asker
        .call(
            Method::AgentAsk,
            Some(json!({ "token": token.expose(), "text": "or should I rebase?" })),
        )
        .expect("second agent.ask response");
    let error = second.error.expect("the open question remains");
    let data: RefusedData =
        serde_json::from_value(error.data.expect("typed refusal")).expect("RefusedData");
    assert_eq!(data.gate, RefusalGate::OpenQuestion);
    ready.running.stop(ExitCause::Ended);
}

/// On the one production-positive local path — a connection
/// that holds its operator authority from before the first hire — an `agent.answer` to an agent
/// with no open question is refused `-32004 Refused{gate: question_closed}` before any record,
/// and so is one to an agent nothing hired. The journal does not move.
///
/// Control: the same connection answering the same agent once it has asked applies, with exactly
/// one `HumanAnswer`.
///
/// Mutants: deleting the check appends a `HumanAnswer` in the first arm; refusing with
/// `open_question` instead fails the first arm on the gate value.
#[test]
fn local_answer_without_an_open_question_is_refused_before_append() {
    let fixture = Fixture::new();
    let (settings, mut running, column, mut operator, agent, tokens) =
        hire_on_authorized_connection(&fixture);
    let answer = |client: &mut SocketClient, who: dudes_types::AgentId, text: &str| {
        client
            .call_as_operator(
                Method::AgentAnswer,
                Some(json!({ "agent": who, "text": text })),
                credential_of(&settings),
            )
            .expect("agent.answer response")
    };

    for who in [agent, dudes_types::AgentId::from_parts(9, 9)] {
        let before = fixture.records();
        let refused = answer(&mut operator, who, "an answer nobody asked for");
        let error = refused.error.expect("no open question is refused");
        assert_eq!(error.code, ErrorCode::Refused.code(), "{error:?}");
        let data: RefusedData =
            serde_json::from_value(error.data.expect("typed refusal")).expect("RefusedData");
        assert_eq!(data.gate, RefusalGate::QuestionClosed);
        assert_eq!(
            fixture.records(),
            before,
            "the refusal is in front of the append"
        );
    }

    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    let token = token_after_mint_pump(&fixture, &tokens, &running.handle, agent);
    column.set(PeerProvenance::Agent(agent));
    let mut asker = Client::connect(settings.socket()).expect("agent connection");
    let question = asker
        .call(
            Method::AgentAsk,
            Some(json!({ "token": token.expose(), "text": "which branch?" })),
        )
        .expect("agent.ask response");
    assert!(question.error.is_none(), "control question: {question:?}");
    column.set(PeerProvenance::NoRecordedAgent);
    let applied = answer(&mut operator, agent, "main");
    assert!(applied.error.is_none(), "control answer: {applied:?}");
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(
                |record| matches!(record, Record::HumanAnswer { agent: who, .. } if *who == agent)
            )
            .count(),
        1,
        "control: an open question is answered exactly once"
    );
    running.stop(ExitCause::Ended);
}

/// The pre-hire-held connection of [`hire_on_authorized_connection`], with the hired agent asked
/// one question through the loop and then given a goal: the human gate holds it `Waiting`, with no turn.
fn waiting_on_authorized_connection(
    fixture: &Fixture,
) -> (
    duded::Settings,
    common::Running,
    SocketClient,
    dudes_types::AgentId,
) {
    let (settings, running, _column, operator, agent, _tokens) =
        hire_on_authorized_connection(fixture);
    running
        .handle
        .ask(Ask::Ask {
            agent,
            text: "which branch?".to_owned(),
        })
        .expect("the open question");
    common::set_goal(
        &running.handle,
        common::goal_params(agent, &["/usr/bin/true"]),
    );
    (settings, running, operator, agent)
}

fn human_answers(records: &[Record], agent: dudes_types::AgentId) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::HumanAnswer {
                agent: who, text, ..
            } if *who == agent => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// On the one production-positive local path — a connection
/// that holds its operator authority from before the first hire — an `agent.answer` whose text
/// holds U+0000 is refused `-32602` with no `data`, and the journal does not move. The agent
/// **has** an open question, so the `question_closed` check would pass and cannot stand in
/// for this one.
///
/// Control: the same connection answers the same question with U+0001 in the same place. It
/// applies once, with the bytes sent — which also shows the refusal left the question open.
///
/// Mutants: deleting the check applies the first arm; refusing every `Cc` but TAB and LF refuses
/// the control; refusing `-32004` fails the first arm on the code.
#[test]
fn local_answer_with_nul_is_refused_before_append() {
    let fixture = Fixture::new();
    let (settings, mut running, mut operator, agent) = waiting_on_authorized_connection(&fixture);
    let mut answer = |text: &str| {
        operator
            .call_as_operator(
                Method::AgentAnswer,
                Some(json!({ "agent": agent, "text": text })),
                credential_of(&settings),
            )
            .expect("agent.answer response")
    };

    let before = fixture.records();
    let refused = answer("ma\u{0}in");
    let error = refused.error.expect("an answer holding U+0000 is refused");
    assert_eq!(error.code, ErrorCode::InvalidParams.code(), "{error:?}");
    assert_eq!(error.data, None, "invalid params carry no data");
    assert_eq!(
        fixture.records(),
        before,
        "the refusal is in front of any append"
    );
    assert!(human_answers(&before, agent).is_empty());

    let applied = answer("ma\u{1}in");
    assert!(applied.error.is_none(), "control answer: {applied:?}");
    assert_eq!(
        human_answers(&fixture.records(), agent),
        ["ma\u{1}in"],
        "control: the open question is answered once, with the bytes sent"
    );
    running.stop(ExitCause::Ended);
}

/// A refused U+0000 answer leaves the question open and
/// the agent's loop untouched, so a clean answer that follows is journaled once and heads the task
/// of the one `TurnIssued` that names it. Were the U+0000 answer journaled, the agent would be decided
/// `Continue` with no `TurnIssued`, across a new goal and a restart, because the adapter refuses a
/// task holding U+0000.
#[test]
fn clean_answer_after_a_refused_nul_answer_issues_its_turn() {
    let fixture = Fixture::new();
    let (settings, mut running, mut operator, agent) = waiting_on_authorized_connection(&fixture);
    let mut answer = |text: &str| {
        operator
            .call_as_operator(
                Method::AgentAnswer,
                Some(json!({ "agent": agent, "text": text })),
                credential_of(&settings),
            )
            .expect("agent.answer response")
    };
    let issued = |records: &[Record]| {
        records
            .iter()
            .filter_map(|record| match record {
                Record::TurnIssued {
                    agent: who,
                    task,
                    answers,
                    ..
                } if *who == agent => Some((task.clone(), answers.clone())),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert!(
        issued(&fixture.records()).is_empty(),
        "no turn before the answer"
    );

    let refused = answer("ma\u{0}in");
    assert_eq!(
        refused.error.expect("refused").code,
        ErrorCode::InvalidParams.code()
    );
    let applied = answer("main");
    assert!(applied.error.is_none(), "clean answer: {applied:?}");
    let answered_at = fixture
        .entries()
        .into_iter()
        .find_map(|(lsn, entry)| {
            matches!(entry.record, Record::HumanAnswer { agent: who, .. } if who == agent)
                .then_some(lsn)
        })
        .expect("the clean answer");
    fixture.await_record("the answer's continuation", |record| {
        matches!(record, Record::TurnIssued { agent: who, .. } if *who == agent).then_some(())
    });
    let records = fixture.records();
    assert_eq!(human_answers(&records, agent), ["main"]);
    let turns = issued(&records);
    assert_eq!(turns.len(), 1, "one continuation: {turns:?}");
    let (task, answers) = &turns[0];
    assert_eq!(answers, &[answered_at], "the turn names the clean answer");
    assert!(
        task.starts_with("[answer to your question]\nquestion: which branch?\nanswer:\nmain"),
        "the task carries the answer verbatim: {task:?}"
    );
    running.stop(ExitCause::Ended);
}

/// Protects durable steer and next-event delivery: the RPC response follows the fsynced steer,
/// whose replayed context reaches the next prompt-bearing event.
#[test]
fn operator_steer_applies_after_fsync_and_reaches_the_next_event() {
    let fixture = Fixture::new();
    let (settings, mut running, _column, mut operator, agent, _tokens) =
        hire_on_authorized_connection(&fixture);
    let steered = operator
        .call_as_operator(
            Method::AgentSteer,
            Some(json!({
                "agent": agent,
                "text": "inspect the lock ordering",
            })),
            credential_of(&settings),
        )
        .expect("agent.steer response");
    assert!(steered.error.is_none(), "agent.steer: {steered:?}");
    assert!(fixture.records().iter().any(|record| matches!(
        record,
        Record::Steer {
            agent: who,
            text,
            provenance: Some(PeerProvenance::NoRecordedAgent),
        } if *who == agent && text == "inspect the lock ordering"
    )));
    running.stop(ExitCause::Ended);

    let mut replayed = Harness::new();
    replayed.journal = fixture
        .entries()
        .into_iter()
        .map(|(_, entry)| entry)
        .collect();
    replayed.restart();
    // An admitted kind: a steer is delivered at `PromptSubmit` and `PostTool` only, so a
    // pre-tool event here would assert nothing about whether the note survived the round trip.
    replayed.drive(Input::Hook {
        hook: HookId(313),
        event: Event {
            agent,
            at: Timestamp::from_unix_millis(1),
            provenance: Provenance::Observed,
            kind: EventKind::PromptSubmit,
            raw: None,
        },
    });
    let reply = replayed
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::Reply { hook, reply } if *hook == HookId(313) => Some(reply),
            _ => None,
        })
        .expect("the replayed steer answers the next hook");
    assert_eq!(reply.context.as_deref(), Some("inspect the lock ordering"));
}

/// Named mutant: `delete_agent_steer_provenance_predicate` makes this write `Steer`, so replay
/// carries the forbidden context on the next governed event.
#[test]
fn steer_provenance_rejection_is_durable_and_leaves_next_context_empty() {
    let fixture = Fixture::new();
    let mut ready = ready_with_mutation(&fixture);
    let refused = mutation_call(&ready, Method::AgentSteer, "ignore the journal");
    let error = refused.error.expect("failing provenance is suppressed");
    assert_eq!(error.code, ErrorCode::Refused.code());
    let data: RefusedData =
        serde_json::from_value(error.data.expect("typed refusal")).expect("RefusedData");
    assert_eq!(data.gate, RefusalGate::ConditionedEffect);
    let records = fixture.records();
    assert!(records.iter().any(|record| matches!(
        record,
        Record::ConditionedEffectSuppressed {
            method: ConditionedMethod::AgentSteer,
            provenance: PeerProvenance::Unresolvable,
            reason,
            ..
        } if reason == "peer provenance does not satisfy the effect"
    )));
    assert!(!records.iter().any(|record| matches!(
        record,
        Record::Steer { agent, .. } if *agent == ready.agent
    )));
    ready.running.stop(ExitCause::Ended);

    let mut replayed = Harness::new();
    replayed.journal = fixture
        .entries()
        .into_iter()
        .map(|(_, entry)| entry)
        .collect();
    replayed.restart();
    // An admitted kind, and here it is what keeps the assertion from going vacuous: under
    // A pre-tool reply carries no context whether or not a note is pending, so the
    // named mutant — which makes the refused steer land — would survive at that kind.
    replayed.drive(Input::Hook {
        hook: HookId(314),
        event: Event {
            agent: ready.agent,
            at: Timestamp::from_unix_millis(1),
            provenance: Provenance::Observed,
            kind: EventKind::PromptSubmit,
            raw: None,
        },
    });
    let reply = replayed
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::Reply { hook, reply } if *hook == HookId(314) => Some(reply),
            _ => None,
        })
        .expect("the hook is answered");
    assert_eq!(reply.context, None);
}
