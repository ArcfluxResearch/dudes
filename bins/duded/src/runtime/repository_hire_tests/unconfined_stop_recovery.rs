//! A Stop hook in an attached session of an unconfined hire is settled once and stays settled
//! across a daemon restart.

use super::unconfined_sites::{attachable_setup, learn_a_session, world_of};
use super::*;
use dudes_proto::InstructionObservationOwner;
use serde_json::json;

fn stop_settlements(setup: &Setup) -> usize {
    setup
        .fixture
        .records()
        .iter()
        .filter(|record| {
            matches!(record, Record::InstructionObservationBatchFinished(batch)
                if matches!(batch.owner, InstructionObservationOwner::AttachedTurn { .. }))
        })
        .count()
}

fn restart_after_stops(stops: u64) {
    let mut setup = attachable_setup();
    setup.native_ptys = Some(common::SharedPtyHost::with_native_evidence(
        setup.fixture.journal_dir(),
    ));
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    let agent = hires.hire(params).unwrap().agent;
    running.stop(ExitCause::Ended);
    learn_a_session(&setup, agent);
    let mut daemon = setup.boot_unconfined();
    let answered = daemon
        .attach(agent, dudes_proto::Actor::System, None, 80, 24)
        .expect("the session opens");
    let crate::ports::Answered::Attached(pty) = answered else {
        panic!("attach answered {answered:?}");
    };
    for hook in 0..stops {
        daemon
            .pump(dudes_supervisor::Input::Hook {
                hook: dudes_supervisor::HookId(91 + hook),
                event: dudes_providers::claude::translate(
                    agent,
                    dudes_types::Timestamp::UNIX_EPOCH,
                    &json!({"hook_event_name": "Stop", "last_assistant_message": "done"}),
                )
                .unwrap(),
            })
            .expect("a stop in the session is recorded");
    }
    daemon
        .pty_close(agent, pty, dudes_proto::Actor::System, None, "test close")
        .expect("the session closes");
    drop(daemon);
    assert_eq!(stop_settlements(&setup), usize::try_from(stops).unwrap());
    let before = setup.fixture.entries().len();

    let mut again = setup.boot_unconfined();
    again.started().expect("startup completes");
    drop(again);

    let after = setup.fixture.entries().len();
    assert_eq!(
        stop_settlements(&setup),
        usize::try_from(stops).unwrap(),
        "a restart settles no Stop again"
    );
    assert_eq!(
        after, before,
        "a restart appends nothing for a settled Stop"
    );
    assert_eq!(world_of(&setup).unmatched_attached_turn_end_count(), 0);
}

#[test]
fn a_stop_in_an_unconfined_attached_session_is_not_settled_again_after_a_restart() {
    restart_after_stops(1);
}

#[test]
fn two_stops_before_a_restart_are_each_settled_once() {
    restart_after_stops(2);
}
