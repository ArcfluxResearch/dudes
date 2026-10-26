#![allow(missing_docs)]

use dudes_journal::Projections;
use dudes_proto::{Entry, MailAct, MailFrom, MailTo, Record, ReplyStatus};
use dudes_types::{AgentId, Lsn, MailId, MailState, ThreadId, Timestamp};

fn agent(n: u128) -> AgentId {
    AgentId::from_parts(1, n)
}
fn message(n: u64, from: u128, to: u128, thread: Option<u128>, act: MailAct) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1),
        record: Record::Mail {
            id: MailId::from_parts(1, u128::from(n)),
            from: MailFrom::Agent { agent: agent(from) },
            to: MailTo::Agent { agent: agent(to) },
            act,
            subject: "safe subject".into(),
            subject_admission: Some(1),
            body: Some("never on the Floor".into()),
            body_len: 18,
            body_path: None,
            body_blake3: None,
            thread: thread.map(|t| ThreadId::from_parts(1, t)),
            thread_seq: 1,
            members: if thread.is_some() {
                vec![agent(1), agent(2), agent(3)]
            } else {
                vec![]
            },
            state: MailState::Durable,
            actor: None,
            provenance: None,
        },
    }
}
fn apply(store: &mut Projections, lsn: u64, entry: &Entry) {
    store.apply(Lsn(lsn), 0, entry).unwrap();
}

#[test]
fn answers_use_direction_thread_creation_order_and_delivery_not_acknowledgement() {
    let mut store = Projections::in_memory().unwrap();
    apply(&mut store, 1, &message(1, 2, 1, Some(1), MailAct::Message));
    apply(&mut store, 2, &message(2, 1, 2, Some(1), MailAct::Request));
    apply(&mut store, 3, &message(3, 2, 1, Some(2), MailAct::Message)); // --to starts a new thread
    apply(&mut store, 4, &message(4, 3, 1, Some(1), MailAct::Message));
    apply(
        &mut store,
        5,
        &Entry {
            at: Timestamp::from_unix_millis(1),
            record: Record::MailState {
                id: MailId::from_parts(1, 2),
                state: MailState::Acked,
            },
        },
    );
    assert_eq!(
        store.messages().unwrap()[1].reply_status,
        ReplyStatus::Awaiting
    );
    let mut answer = message(5, 2, 1, Some(1), MailAct::Ack);
    if let Record::Mail { state, .. } = &mut answer.record {
        *state = MailState::Bounced { why: "no".into() };
    }
    apply(&mut store, 6, &answer);
    assert_eq!(
        store.messages().unwrap()[1].reply_status,
        ReplyStatus::Awaiting
    );
    apply(&mut store, 7, &message(6, 2, 1, Some(1), MailAct::Card));
    apply(
        &mut store,
        8,
        &Entry {
            at: Timestamp::from_unix_millis(1),
            record: Record::AgentRetired {
                id: agent(2),
                reason: "retired".into(),
                browser_retire: None,
            },
        },
    );
    let rows = store.messages().unwrap();
    assert_eq!(rows[1].reply_status, ReplyStatus::Answered);
    assert_eq!(rows[5].act, None);
    assert_eq!(rows[5].reply_status, ReplyStatus::NotExpected);
    let wire = serde_json::to_string(&rows).unwrap();
    for forbidden in [
        "body",
        "never on the Floor",
        "subject_admission",
        "thread_seq",
        "members",
    ] {
        assert!(!wire.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn status_precedence_uses_complete_retirement_facts_and_legacy_subjects_stay_hidden() {
    let mut store = Projections::in_memory().unwrap();
    for n in 1..=4 {
        let mut row = message(
            n,
            1,
            if n == 4 { 3 } else { 2 },
            if n == 3 { Some(3) } else { None },
            if n == 1 {
                MailAct::Message
            } else {
                MailAct::Request
            },
        );
        if let Record::Mail {
            thread_seq,
            subject_admission,
            subject,
            state,
            ..
        } = &mut row.record
        {
            *thread_seq = if n == 3 { 12 } else { 1 };
            *subject_admission = if n == 1 {
                None
            } else if n == 2 {
                Some(999)
            } else {
                Some(1)
            };
            *subject = "historical-secret".into();
            if n == 1 {
                *state = MailState::Bounced { why: "no".into() };
            }
        }
        apply(&mut store, n, &row);
    }
    assert_eq!(
        store.messages().unwrap()[2].reply_status,
        ReplyStatus::ThreadFull
    );
    for n in 5..=300 {
        apply(
            &mut store,
            n,
            &Entry {
                at: Timestamp::from_unix_millis(1),
                record: Record::AgentRetired {
                    id: if n == 5 {
                        agent(2)
                    } else {
                        agent(u128::from(n))
                    },
                    reason: "gone".into(),
                    browser_retire: None,
                },
            },
        );
    }
    let rows = store.messages().unwrap();
    assert_eq!(
        rows.iter().map(|m| m.reply_status).collect::<Vec<_>>(),
        vec![
            ReplyStatus::NotExpected,
            ReplyStatus::Undeliverable,
            ReplyStatus::Undeliverable,
            ReplyStatus::Awaiting,
        ]
    );
    assert!(rows[0].subject.is_none() && rows[1].subject.is_none());
}

#[test]
fn combined_budget_keeps_a_suffix_caps_captions_and_rebuilds_exact_threads() {
    let mut store = Projections::in_memory().unwrap();
    for n in 1..=128 {
        let mut row = message(n, 1, 2, Some(u128::from(n)), MailAct::Request);
        if let Record::Mail {
            subject, members, ..
        } = &mut row.record
        {
            *subject = format!("\\\"{}\u{202e}\n", "(redacted)\\\"".repeat(50));
            *members = (1..=8).map(agent).collect();
        }
        apply(&mut store, n, &row);
    }
    let view = store.mail_view().unwrap();
    let bytes = serde_json::to_vec(&view.messages).unwrap().len()
        + serde_json::to_vec(&view.threads).unwrap().len();
    assert!(bytes <= 65_536, "{bytes}");
    assert!(!view.messages.is_empty() && view.messages.len() < 128);
    assert_eq!(view.messages.last().unwrap().lsn, "128");
    assert_eq!(view.threads.len(), view.messages.len());
    for pair in view.messages.windows(2) {
        assert_eq!(
            pair[0].lsn.parse::<u64>().unwrap() + 1,
            pair[1].lsn.parse::<u64>().unwrap()
        );
    }
    assert!(view.messages.iter().all(|row| {
        let subject = row.subject.as_ref().unwrap();
        subject.len() <= 120
            && !subject.contains('\u{202e}')
            && !subject.contains('\n')
            && subject.ends_with('…')
    }));
}
