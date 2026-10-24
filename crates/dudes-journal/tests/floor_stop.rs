#![allow(missing_docs)]
//! The Floor's stop in the projections: the mark on the standing halt, the turn it closes, and
//! the records that clear the mark.

use dudes_journal::Projections;
use dudes_proto::record::{BrowserControl, Hex32, samples};
use dudes_proto::{Entry, Record, RecordKind};
use dudes_types::{AgentId, AgentStatus, Lsn, Timestamp, TurnId, TurnStatus};

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_000),
        record,
    }
}

fn hired(id: AgentId) -> Entry {
    let mut record = samples::record(RecordKind::AgentHired);
    let Record::AgentHired { id: slot, .. } = &mut record else {
        unreachable!()
    };
    *slot = id;
    entry(record)
}

fn issued(id: AgentId, turn: TurnId) -> Entry {
    let mut record = samples::record(RecordKind::TurnIssued);
    let Record::TurnIssued {
        agent, turn: slot, ..
    } = &mut record
    else {
        unreachable!()
    };
    *agent = id;
    *slot = turn;
    entry(record)
}

fn stop(agent: AgentId, abandons: Option<TurnId>) -> Entry {
    entry(Record::BrowserHalt {
        agent,
        browser: BrowserControl {
            request_id: Hex32::new([1; 32]),
            request_digest: Hex32::new([2; 32]),
            control_lsn: None,
            session_digest: Hex32::new([3; 32]),
        },
        abandons,
    })
}

fn apply_all(store: &mut Projections, entries: &[Entry]) {
    for (index, entry) in entries.iter().enumerate() {
        let lsn = Lsn(u64::try_from(index).unwrap() + 1);
        assert!(store.apply(lsn, 7, entry).unwrap());
    }
}

/// A stop that closes the issued turn leaves the agent idle and halted by the Floor, with the
/// turn ended; a stop that closes none leaves the turn running so the page can say "Stopping".
#[test]
fn a_stop_marks_the_halt_and_closes_only_the_turn_it_names() {
    let agent = AgentId::from_parts(5, 1);
    let turn = TurnId::from_parts(5, 2);
    let mut store = Projections::in_memory().unwrap();
    apply_all(
        &mut store,
        &[hired(agent), issued(agent, turn), stop(agent, None)],
    );
    let row = store.agent(agent).unwrap().unwrap();
    assert!(row.halted && row.halted_by_floor);
    assert_eq!(row.open_turn, Some(turn), "the running turn stays open");
    assert_eq!(
        store.turn(turn).unwrap().unwrap().status,
        TurnStatus::Issued
    );

    let mut store = Projections::in_memory().unwrap();
    apply_all(
        &mut store,
        &[hired(agent), issued(agent, turn), stop(agent, Some(turn))],
    );
    let row = store.agent(agent).unwrap().unwrap();
    assert!(row.halted && row.halted_by_floor);
    assert_eq!(row.open_turn, None);
    assert_eq!(row.status, AgentStatus::Idle);
    assert_eq!(
        store.turn(turn).unwrap().unwrap().status,
        TurnStatus::Killed
    );
}

/// The mark belongs to the newest halt: an operator's halt takes it off and a resume clears both.
#[test]
fn an_operators_halt_and_a_resume_clear_the_floor_mark() {
    let agent = AgentId::from_parts(5, 1);
    let mut store = Projections::in_memory().unwrap();
    apply_all(
        &mut store,
        &[
            hired(agent),
            stop(agent, None),
            entry(Record::Halt { agent, text: None }),
        ],
    );
    let row = store.agent(agent).unwrap().unwrap();
    assert!(row.halted && !row.halted_by_floor);

    let mut store = Projections::in_memory().unwrap();
    apply_all(
        &mut store,
        &[
            hired(agent),
            stop(agent, None),
            entry(Record::Resume {
                agent,
                browser: None,
            }),
        ],
    );
    let row = store.agent(agent).unwrap().unwrap();
    assert!(!row.halted && !row.halted_by_floor);
}
