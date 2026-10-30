//! Instruction review across every copy of an unconfined multi-repository hire.

use super::unconfined_sites::send_chat;
use super::*;
use dudes_journal::{Projections, Reader, replay};
use dudes_types::{AgentId, TurnId};

fn chat_turn_to_its_end(setup: &Setup) -> (AgentId, TurnId) {
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    send_chat(&running, agent);
    let turn = setup
        .fixture
        .await_record("the spawn", |record| match record {
            Record::TurnSpawned { turn, .. } => Some(*turn),
            _ => None,
        });
    common::end_turn(&running.handle, turn);
    setup
        .fixture
        .await_record("the finished observation", |record| {
            matches!(
                record,
                Record::InstructionObservationBatchFinished(_)
                    | Record::InstructionObservationFinished { .. }
            )
            .then_some(())
        });
    running.stop(ExitCause::Ended);
    (agent, turn)
}

#[test]
fn a_chat_turn_of_an_unconfined_hire_records_what_the_journal_accepts_and_replays() {
    let setup = Setup::new();
    chat_turn_to_its_end(&setup);
    let records = setup.fixture.records();
    let batches = |wanted: fn(&Record) -> bool| records.iter().filter(|r| wanted(r)).count();
    assert_eq!(
        batches(|r| matches!(r, Record::InstructionObservationBatchStarted(_))),
        1
    );
    assert_eq!(
        batches(|r| matches!(r, Record::InstructionObservationBatchFinished(_))),
        1
    );
    assert_eq!(
        batches(|r| matches!(
            r,
            Record::InstructionObservationStarted { .. }
                | Record::InstructionObservationFinished { .. }
        )),
        0,
        "a scoped hire never records the single-folder shape"
    );
    let reader = Reader::new(&setup.fixture.journal_dir());
    let mut cold = Projections::in_memory().unwrap();
    assert!(replay(&reader, &mut cold).unwrap().applied > 0);
    let mut warm =
        Projections::open(&setup.settings.state_dir.join(crate::boot::PROJECTION_FILE)).unwrap();
    assert_eq!(replay(&reader, &mut warm).unwrap().applied, 0);
}
