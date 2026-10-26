#![allow(missing_docs)]

use dudes_journal::Projections;
use dudes_proto::{Entry, MailAct, MailFrom, MailTo, Record};
use dudes_types::{AgentId, Lsn, MailId, MailState, Timestamp};

#[test]
fn replay_never_replaces_the_first_mail_recipient_or_terminal_state() {
    let original = AgentId::from_parts(1, 1);
    let replacement = AgentId::from_parts(1, 2);
    let id = MailId::from_parts(2, 1);
    let mail = Record::Mail {
        thread: None,
        thread_seq: 0,
        members: Vec::new(),
        id,
        from: MailFrom::Agent { agent: original },
        to: MailTo::Agent { agent: original },
        act: MailAct::Message,
        subject: "original".to_owned(),
        subject_admission: None,
        body: Some("body".to_owned()),
        body_len: 4,
        body_path: None,
        body_blake3: None,
        state: MailState::Durable,
        actor: None,
        provenance: None,
    };
    let mut duplicate = mail.clone();
    if let Record::Mail { to, subject, .. } = &mut duplicate {
        *to = MailTo::Agent { agent: replacement };
        *subject = "replacement".to_owned();
    }
    let mut projection = Projections::in_memory().unwrap();
    for (n, record) in [mail, duplicate.clone()].into_iter().enumerate() {
        projection
            .apply(
                Lsn(u64::try_from(n).unwrap() + 1),
                u32::try_from(n).unwrap(),
                &Entry {
                    at: Timestamp::from_unix_millis(1),
                    record,
                },
            )
            .unwrap();
    }
    assert_eq!(projection.unread_by_agent(original).unwrap(), 1);
    assert_eq!(projection.unread_by_agent(replacement).unwrap(), 0);
    projection
        .apply(
            Lsn(3),
            3,
            &Entry {
                at: Timestamp::from_unix_millis(1),
                record: Record::MailState {
                    id,
                    state: MailState::Acked,
                },
            },
        )
        .unwrap();
    projection
        .apply(
            Lsn(4),
            4,
            &Entry {
                at: Timestamp::from_unix_millis(1),
                record: duplicate,
            },
        )
        .unwrap();
    assert_eq!(projection.unread_by_agent(original).unwrap(), 0);
    assert_eq!(projection.unread_by_agent(replacement).unwrap(), 0);
}
