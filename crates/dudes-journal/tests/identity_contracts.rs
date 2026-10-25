#![allow(missing_docs)]
//! A journal may hold Chat turns of both contracts. A turn keeps the contract it was decided
//! under, a task must be exactly what its contract builds for the issuing agent, and the two
//! contracts never pair across a decision and its issue.

use dudes_journal::Projections;
use dudes_proto::{Entry, IdentityLine, identified_conversation_task};
use dudes_types::{AgentId, Lsn, TurnId};
use serde_json::{Value, json};

fn ada() -> AgentId {
    AgentId::from_parts(1, 1)
}
fn bo() -> AgentId {
    AgentId::from_parts(1, 2)
}
fn line(agent: AgentId, name: &str, role: &str) -> IdentityLine {
    IdentityLine::new(agent, name, role)
}
fn entry(value: Value) -> Entry {
    serde_json::from_value(value).unwrap()
}
fn hire(id: AgentId, name: &str, role: &str) -> Entry {
    entry(
        json!({"at":0,"t":"agent_hired","id":id,"name":name,"role":role,"department":"eng",
        "provider":"claude","cwd":"/work","budget":{}}),
    )
}
fn queued(agent: AgentId, context: u64, text: &str) -> Entry {
    entry(
        json!({"at":0,"t":"conversation_message_queued","agent":agent,"text":text,
        "browser":{"request_id_digest":"11".repeat(32),"request_digest":"22".repeat(32),
                   "session_digest":"33".repeat(32)},
        "context_lsn":context,"charge_scope":"agent_and_floor"}),
    )
}
fn decided(kind: &str, agent: AgentId, note: u64, context: u64, task: &str) -> Entry {
    entry(
        json!({"at":0,"t":kind,"agent":agent,"note_lsn":note,"context_lsn":context,
        "charge_scope":"agent_and_floor","input_digest":"digest","gate_trace":[],
        "decision":{"kind":"issue","task":task}}),
    )
}
fn issued(
    kind: &str,
    agent: AgentId,
    turn: u128,
    note: u64,
    context: u64,
    task: &str,
    decision: u64,
) -> Entry {
    entry(
        json!({"at":0,"t":kind,"agent":agent,"turn":TurnId::from_parts(1, turn),
        "note_lsn":note,"context_lsn":context,"charge_scope":"agent_and_floor","task":task,
        "session":null,"model_selection":{},"lsn_of_decision":decision}),
    )
}

fn base() -> Projections {
    let mut p = Projections::in_memory().unwrap();
    for (lsn, e) in [
        hire(ada(), "Ada", "dev"),
        hire(bo(), "Bo", "qa"),
        queued(ada(), 1, "legacy one"),
        queued(bo(), 2, "identified two"),
    ]
    .into_iter()
    .enumerate()
    {
        p.apply(Lsn(u64::try_from(lsn).unwrap() + 1), 0, &e)
            .unwrap();
    }
    p
}

#[test]
fn a_journal_mixes_legacy_and_identified_chat_turns_and_replays_both() {
    let mut p = base();
    let bo_task = identified_conversation_task(&line(bo(), "Bo", "qa"), "identified two", false);
    for (lsn, e) in [
        decided("conversation_turn_decided", ada(), 3, 1, "legacy one"),
        issued("conversation_turn_issued", ada(), 10, 3, 1, "legacy one", 5),
        decided("identified_conversation_turn_decided", bo(), 4, 2, &bo_task),
        issued(
            "identified_conversation_turn_issued",
            bo(),
            11,
            4,
            2,
            &bo_task,
            7,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        p.apply(Lsn(u64::try_from(lsn).unwrap() + 5), 0, &e)
            .unwrap();
    }
    assert_eq!(
        p.agent(ada()).unwrap().unwrap().open_turn,
        Some(TurnId::from_parts(1, 10))
    );
    assert_eq!(
        p.agent(bo()).unwrap().unwrap().open_turn,
        Some(TurnId::from_parts(1, 11))
    );
}

#[test]
fn a_decision_and_its_issue_must_share_one_contract() {
    let bo_task = identified_conversation_task(&line(bo(), "Bo", "qa"), "identified two", false);
    let legacy_pair = [
        ("identified_conversation_turn_decided", &bo_task),
        ("conversation_turn_decided", &"identified two".to_owned()),
    ];
    for (decision_kind, decision_task) in legacy_pair {
        let mut p = base();
        p.apply(
            Lsn(5),
            0,
            &decided(decision_kind, bo(), 4, 2, decision_task),
        )
        .unwrap();
        let other_issue = if decision_kind == "conversation_turn_decided" {
            issued(
                "identified_conversation_turn_issued",
                bo(),
                11,
                4,
                2,
                &bo_task,
                5,
            )
        } else {
            issued(
                "conversation_turn_issued",
                bo(),
                11,
                4,
                2,
                "identified two",
                5,
            )
        };
        assert!(p.apply(Lsn(6), 0, &other_issue).is_err(), "{decision_kind}");
        assert_eq!(p.agent(bo()).unwrap().unwrap().open_turn, None);
    }
}

#[test]
fn an_identified_task_names_the_issuing_agent_and_the_input_exactly() {
    let wrong_agent =
        identified_conversation_task(&line(ada(), "Bo", "qa"), "identified two", false);
    let wrong_text = identified_conversation_task(&line(bo(), "Bo", "qa"), "another note", false);
    let plain = "identified two".to_owned();
    for task in [&wrong_agent, &wrong_text, &plain] {
        let mut p = base();
        assert!(
            p.apply(
                Lsn(5),
                0,
                &decided("identified_conversation_turn_decided", bo(), 4, 2, task)
            )
            .is_err(),
            "{task}"
        );
    }
    let mut p = base();
    let right = identified_conversation_task(&line(bo(), "Bo", "qa"), "identified two", false);
    assert!(
        p.apply(
            Lsn(5),
            0,
            &decided("conversation_turn_decided", bo(), 4, 2, &right)
        )
        .is_err(),
        "a legacy decision may not carry the line"
    );
}
