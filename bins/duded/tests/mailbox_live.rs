#![allow(missing_docs)]
#![cfg(unix)]

use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::{MailFrom, Record};
use dudes_types::{AgentId, MailId, MailState};
use serde_json::json;

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock};

fn wait_for(mut predicate: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(4);
    while !predicate() {
        assert!(Instant::now() < end, "mailbox did not converge");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn directory(fixture: &Fixture, agent: AgentId, name: &str) -> std::path::PathBuf {
    fixture
        .state
        .path()
        .join("agents")
        .join(agent.to_string())
        .join(name)
}

#[test]
fn structural_sender_and_observed_ack_survive_restart() {
    let fixture = Fixture::new();
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
    .unwrap();
    let mut running = common::start(daemon);
    let sender = common::hire(&running.handle, fixture.state.path());
    let recipient = common::hire(&running.handle, fixture.state.path());
    let outbox = directory(&fixture, sender, "outbox");
    fs::create_dir_all(&outbox).unwrap();
    let id = MailId::from_parts(1, 1);
    fs::write(
        outbox.join(format!("{id}.json")),
        serde_json::to_vec(&json!({
            "to": {"kind":"agent", "agent":recipient}, "act":"message",
            "subject":"handoff", "body":"exact bytes",
            "from":{"kind":"human"}, "actor":{"kind":"system"}
        }))
        .unwrap(),
    )
    .unwrap();
    let id = dudes_journal::mail_recipient_id(id, recipient);
    let inbox = directory(&fixture, recipient, "inbox");
    let delivered = inbox.join(format!("{id}.json"));
    wait_for(|| delivered.exists());
    let item: serde_json::Value = serde_json::from_slice(&fs::read(&delivered).unwrap()).unwrap();
    assert_eq!(item["from"], json!({"kind":"agent", "agent":sender}));
    assert_eq!(item["body"], "exact bytes");
    assert!(fixture.records().iter().any(|r| matches!(r,
        Record::Mail { id: found, from: MailFrom::Agent { agent }, actor: None, provenance: None, .. }
        if *found == id && *agent == sender)));
    assert!(!fixture.records().iter().any(|r| matches!(r,
        Record::MailState { id: found, state: MailState::Acked } if *found == id)));
    fs::rename(&delivered, inbox.join(".done").join(format!("{id}.json"))).unwrap();
    wait_for(|| {
        fixture.records().iter().any(|r| {
            matches!(r,
        Record::MailState { id: found, state: MailState::Acked } if *found == id)
        })
    });
    running.stop(ExitCause::ShutdownRpc);
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
    .unwrap();
    let mut running = common::start(daemon);
    std::thread::sleep(Duration::from_millis(250));
    running.stop(ExitCause::ShutdownRpc);
    assert!(!delivered.exists());
    assert_eq!(
        fixture
            .records()
            .iter()
            .filter(|r| matches!(r,
        Record::MailState { id: found, state: MailState::Acked } if *found == id))
            .count(),
        1
    );
}

#[test]
fn checkpoint_poison_retains_sources_and_stops_all_new_mailbox_files() {
    let fixture = Fixture::new();
    let mut settings = fixture.settings();
    settings.journal.checkpoint_every = 1;
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
    .unwrap();
    let failure = daemon.append_failure();
    let mut running = common::start(daemon);
    let sender = common::hire(&running.handle, fixture.state.path());
    let recipient = common::hire(&running.handle, fixture.state.path());
    failure.short_write_after(1, 5);
    let first = MailId::from_parts(4, 1);
    let outbox = directory(&fixture, sender, "outbox");
    fs::create_dir_all(&outbox).unwrap();
    let first_source = outbox.join(format!("{first}.json"));
    fs::write(&first_source, serde_json::to_vec(&json!({"to":{"kind":"agent","agent":recipient}, "act":"message", "subject":"first", "body":"inline"})).unwrap()).unwrap();
    let first = dudes_journal::mail_recipient_id(first, recipient);
    wait_for(|| running.handle.doctor().unwrap().journal_stopped.is_some());
    assert!(
        fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::Mail { id, .. } if *id == first))
    );
    assert!(first_source.exists());
    assert!(
        !directory(&fixture, recipient, "inbox")
            .join(format!("{first}.json"))
            .exists()
    );
    let second = MailId::from_parts(4, 2);
    fs::write(outbox.join(format!("{second}.json")), serde_json::to_vec(&json!({"to":{"kind":"agent","agent":recipient}, "act":"message", "subject":"second", "body":"body".repeat(1500)})).unwrap()).unwrap();
    let second = dudes_journal::mail_recipient_id(second, recipient);
    std::thread::sleep(Duration::from_millis(250));
    running.handle.doctor().unwrap();
    assert!(
        !directory(&fixture, recipient, "inbox/.staging")
            .join(format!("{second}.body.tmp"))
            .exists()
    );
    assert!(
        !fixture
            .records()
            .iter()
            .any(|record| matches!(record, Record::Mail { id, .. } if *id == second))
    );
    running.stop(ExitCause::ShutdownRpc);
}
