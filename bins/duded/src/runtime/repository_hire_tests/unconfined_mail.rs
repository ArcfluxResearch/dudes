//! A teammate's request starts the recipient's turn in an unconfined multi-repository project.

use super::*;
use dudes_proto::issue::ManagedIssueRef;
use dudes_proto::{MailAct, MailTo};
use dudes_types::{AgentId, MailId, TurnId};

fn mail_turn_of(record: &Record, agent: AgentId) -> Option<TurnId> {
    match record.issue_payload()? {
        ManagedIssueRef::MailTurn(issue) if issue.agent == agent => Some(issue.turn),
        _ => None,
    }
}

fn send_request(setup: &Setup, from: AgentId, to: AgentId) {
    let outbox = setup
        .settings
        .state_dir
        .join("agents")
        .join(from.to_string())
        .join("outbox");
    std::fs::create_dir_all(&outbox).unwrap();
    let id = MailId::from_parts(9, 1);
    let body = serde_json::json!({
        "to": {"kind": "agent", "agent": to},
        "act": "request",
        "subject": "question",
        "body": "what is 2+2?",
    });
    std::fs::write(
        outbox.join(format!("{id}.json")),
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
}

#[test]
fn a_mail_turn_of_an_unconfined_hire_launches_in_the_primary_copy_with_the_others_added() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let ada = hires.hire(params.clone()).unwrap().agent;
    let bruno = hires.hire(params).unwrap().agent;
    send_request(&setup, ada, bruno);
    setup
        .fixture
        .await_record("the request", |record| match record {
            Record::Mail {
                to: MailTo::Agent { agent },
                act: MailAct::Request,
                ..
            } if *agent == bruno => Some(()),
            _ => None,
        });
    setup.fixture.await_record("the mail turn spawn", |record| {
        matches!(record, Record::TurnSpawned { .. }).then_some(())
    });
    running.stop(ExitCause::Ended);
    let spawned = setup.host.spawned();
    assert_eq!(spawned.len(), 1);
    let issues = setup
        .fixture
        .records()
        .iter()
        .filter(|record| mail_turn_of(record, bruno).is_some())
        .count();
    assert_eq!(issues, 1, "one issue for the one request");
    setup.assert_primary_with_others_added(&spawned[0], bruno);
}

#[test]
fn an_issued_mail_turn_of_an_unconfined_hire_covers_its_mail_on_replay() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let ada = hires.hire(params.clone()).unwrap().agent;
    let bruno = hires.hire(params).unwrap().agent;
    send_request(&setup, ada, bruno);
    let turn = setup
        .fixture
        .await_record("the mail turn issue", |record| mail_turn_of(record, bruno));
    running.stop(ExitCause::Ended);
    let mut world = dudes_supervisor::World::default();
    for (lsn, entry) in setup.fixture.entries() {
        world.apply(lsn, &entry);
    }
    assert!(world.turns.contains_key(&turn), "the issue is a turn row");
    assert!(
        world.mail_turn_candidates(bruno).is_empty(),
        "the covered mail starts no second turn"
    );
    let decisions = setup
        .fixture
        .records()
        .iter()
        .filter(
            |record| matches!(record, Record::MailTurnDecided(decision) if decision.agent == bruno),
        )
        .count();
    assert_eq!(decisions, 1);
}
