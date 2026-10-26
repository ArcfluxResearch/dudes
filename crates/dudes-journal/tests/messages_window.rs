#![allow(missing_docs)]

use std::time::Instant;

use dudes_journal::Projections;
use dudes_proto::{Entry, MailAct, MailFrom, MailTo, Record};
use dudes_types::{AgentId, Lsn, MailId, MailState, Timestamp};

fn mail(number: u64) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(900),
        record: Record::Mail {
            thread: None,
            thread_seq: 0,
            members: Vec::new(),
            id: MailId::from_parts(10_000 - number, 1),
            from: MailFrom::Agent {
                agent: AgentId::from_parts(1, 1),
            },
            to: MailTo::Agent {
                agent: AgentId::from_parts(1, 2),
            },
            act: MailAct::Message,
            subject: "private subject".into(),
            subject_admission: None,
            body: Some("private body".into()),
            body_len: 12,
            body_path: None,
            body_blake3: None,
            state: MailState::Durable,
            actor: None,
            provenance: None,
        },
    }
}

#[test]
#[ignore = "manual message append measurement"]
#[allow(clippy::print_stdout)]
fn measure_live_message_append_cost() {
    for history in [128u64, 2_048, 8_192] {
        let mut store = Projections::in_memory().unwrap();
        let entries: Vec<_> = (1..=history).map(mail).collect();
        store
            .apply_batch(
                entries
                    .iter()
                    .enumerate()
                    .map(|(i, entry)| (Lsn(u64::try_from(i).unwrap() + 1), 0, entry)),
            )
            .unwrap();
        let mut lsn = history;
        let mut mail_count = history;
        for case in ["pause", "new_mail", "retained_state", "evicted_state"] {
            let mut samples = Vec::new();
            let starting_mail_count = mail_count;
            for sample in 1..=101 {
                let entry = match case {
                    "pause" => Entry {
                        at: Timestamp::from_unix_millis(900),
                        record: Record::Pause {
                            agent: AgentId::NIL,
                            browser: None,
                        },
                    },
                    "new_mail" => {
                        mail_count += 1;
                        mail(mail_count)
                    }
                    _ => Entry {
                        at: Timestamp::from_unix_millis(-900),
                        record: Record::MailState {
                            id: MailId::from_parts(
                                10_000
                                    - if case == "retained_state" {
                                        mail_count
                                    } else {
                                        1
                                    },
                                1,
                            ),
                            state: if sample % 2 == 0 {
                                MailState::Acked
                            } else {
                                MailState::Notified { n: 5 }
                            },
                        },
                    },
                };
                lsn += 1;
                let start = Instant::now();
                store.apply(Lsn(lsn), 0, &entry).unwrap();
                let rows = store.messages().unwrap();
                assert_eq!(rows.len(), 128);
                std::hint::black_box(rows);
                samples.push(start.elapsed().as_nanos());
            }
            samples.sort_unstable();
            println!(
                "case={case} mail_history_start={starting_mail_count} append_plus_messages_median_ns={} p95_ns={}",
                samples[50], samples[95]
            );
        }
    }
}

#[test]
fn message_bytes_preserve_the_floor_contract_through_state_changes() {
    let id = MailId::parse("01ARZ3NDEKTSV4RRFFQ69G5FB0").unwrap();
    let from = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FB1").unwrap();
    let to = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FB2").unwrap();
    let mut entry = mail(1);
    let Record::Mail {
        id: mail_id,
        from: sender,
        to: recipient,
        ..
    } = &mut entry.record
    else {
        unreachable!()
    };
    *mail_id = id;
    *sender = MailFrom::Agent { agent: from };
    *recipient = MailTo::Agent { agent: to };
    let mut store = Projections::in_memory().unwrap();
    store.apply(Lsn(1), 0, &entry).unwrap();
    assert_eq!(serde_json::to_vec(&store.messages().unwrap()).unwrap(), br#"[{"id":"01ARZ3NDEKTSV4RRFFQ69G5FB0","from":"01ARZ3NDEKTSV4RRFFQ69G5FB1","to":"01ARZ3NDEKTSV4RRFFQ69G5FB2","at":900,"lsn":"1","state":"durable","subject":null,"act":"message","reply_status":"not_expected"}]"#);
    let mut human = mail(2);
    if let Record::Mail { from, .. } = &mut human.record {
        *from = MailFrom::Human;
    }
    store.apply(Lsn(2), 0, &human).unwrap();
    let mut duplicate = entry.clone();
    if let Record::Mail { to, .. } = &mut duplicate.record {
        *to = MailTo::Human;
    }
    store.apply(Lsn(3), 0, &duplicate).unwrap();
    let state = Entry {
        at: Timestamp::from_unix_millis(-500),
        record: Record::MailState {
            id,
            state: MailState::Bounced {
                why: "private reason".into(),
            },
        },
    };
    store.apply(Lsn(4), 0, &state).unwrap();
    assert_eq!(serde_json::to_vec(&store.messages().unwrap()).unwrap(), br#"[{"id":"01ARZ3NDEKTSV4RRFFQ69G5FB0","from":"01ARZ3NDEKTSV4RRFFQ69G5FB1","to":"01ARZ3NDEKTSV4RRFFQ69G5FB2","at":900,"lsn":"1","state":"bounced","subject":null,"act":"message","reply_status":"not_expected"}]"#);
}

#[test]
fn the_row_count_bound_keeps_even_the_largest_messages_under_the_byte_bound() {
    let mut store = Projections::in_memory().unwrap();
    let entry = Entry {
        at: Timestamp::from_unix_millis(i64::MIN),
        record: Record::Mail {
            thread: None,
            thread_seq: 0,
            members: Vec::new(),
            id: MailId::from_parts(1, 1),
            from: MailFrom::Agent {
                agent: AgentId::NIL,
            },
            to: MailTo::Agent {
                agent: AgentId::NIL,
            },
            act: MailAct::Message,
            subject: String::new(),
            subject_admission: None,
            body: None,
            body_len: 0,
            body_path: None,
            body_blake3: None,
            state: MailState::Notified { n: u8::MAX },
            actor: None,
            provenance: None,
        },
    };
    for number in 0..128 {
        let mut entry = entry.clone();
        if let Record::Mail { id, .. } = &mut entry.record {
            *id = MailId::from_parts(1, number);
        }
        store
            .apply(
                Lsn(u64::MAX - 127 + u64::try_from(number).unwrap()),
                0,
                &entry,
            )
            .unwrap();
    }
    let rows = store.messages().unwrap();
    assert_eq!(rows.len(), 128);
    assert_eq!(serde_json::to_vec(&rows).unwrap().len(), 30_849);
    assert_eq!(rows.first().unwrap().lsn, "18446744073709551488");
    assert_eq!(rows.last().unwrap().lsn, "18446744073709551615");
}
