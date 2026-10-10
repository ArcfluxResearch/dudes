//! Card replay retains the status author and conditional sequence.

use dudes_journal::Projections;
use dudes_proto::{Actor, Entry, Record};
use dudes_types::{AgentId, CardId, CardStatus, Lsn, Seq, Timestamp};

#[test]
fn card_records_replay_to_the_same_status_author_and_sequence() {
    let card = CardId::from_parts(1, 1);
    let agent = AgentId::from_parts(1, 2);
    let entries = [
        Record::CardCreated {
            card,
            title: "Ship board".to_owned(),
            department: "engineering".to_owned(),
            by: Actor::Peer { pid: 31, uid: None },
            deps: vec![],
        },
        Record::Claim {
            card,
            agent,
            expected_seq: Seq::ZERO,
        },
        Record::CardTransition {
            card,
            from: CardStatus::Claimed,
            to: CardStatus::Doing,
            by: Actor::Agent { agent },
            expected_seq: Seq(1),
            reason: None,
        },
    ]
    .map(|record| Entry {
        at: Timestamp::from_unix_millis(1_000),
        record,
    });

    let mut live = Projections::in_memory().unwrap();
    let mut replayed = Projections::in_memory().unwrap();
    for (index, entry) in entries.iter().enumerate() {
        live.apply(
            Lsn(u64::try_from(index).unwrap() + 1),
            u32::try_from(index).unwrap(),
            entry,
        )
        .unwrap();
    }
    replayed
        .apply_batch(entries.iter().enumerate().map(|(index, entry)| {
            (
                Lsn(u64::try_from(index).unwrap() + 1),
                u32::try_from(index).unwrap(),
                entry,
            )
        }))
        .unwrap();

    let view = live.card(card).unwrap().unwrap();
    assert_eq!(view.status, CardStatus::Doing);
    assert_eq!(view.status_by, Actor::Agent { agent });
    assert_eq!(view.claimed_by, Some(agent));
    assert_eq!(view.seq, Seq(2));
    assert_eq!(live.cards().unwrap(), vec![view]);
    assert_eq!(live.export().unwrap(), replayed.export().unwrap());
}
