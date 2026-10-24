//! Closing membership and readiness rebuild from the same journal prefix.

use std::collections::BTreeMap;

use dudes_proto::{ClosingPhase, Entry, ExitStatus, Record, TurnOutcome};
use dudes_supervisor::{Input, closing_input_digest, testing::Harness};
use dudes_types::{Lsn, Provenance, Timestamp};

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(17),
        record,
    }
}

#[test]
fn acknowledgement_needs_separate_observed_finish_and_never_expands_snapshot() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness.set_goal(agent, &["/usr/bin/true"]);
    let turn = harness.supervisor.world().agents[&agent]
        .open_turn
        .expect("running turn");
    let mut world = harness.supervisor.world().clone();
    world.apply(
        Lsn(1000),
        &entry(Record::ClosingTime {
            phase: ClosingPhase::Announced,
        }),
    );
    world.apply(
        Lsn(1001),
        &entry(Record::ClosingTime {
            phase: ClosingPhase::Ack { agent },
        }),
    );
    let session = world.closing.as_ref().expect("announcement folded");
    assert_eq!(session.announcement, Lsn(1000));
    assert_eq!(session.participants.len(), 1);
    assert_eq!(session.participants[&agent].acknowledged, Some(Lsn(1001)));
    assert!(
        !session.ready(&world),
        "an ACK does not end the actual turn"
    );

    world.apply(
        Lsn(1002),
        &entry(Record::ClosingTime {
            phase: ClosingPhase::Announced,
        }),
    );
    world.apply(
        Lsn(1003),
        &entry(Record::ClosingTime {
            phase: ClosingPhase::Ack { agent },
        }),
    );
    assert_eq!(world.closing.as_ref().unwrap().announcement, Lsn(1000));
    assert_eq!(
        world.closing.as_ref().unwrap().participants[&agent].acknowledged,
        Some(Lsn(1001))
    );

    harness.drive(Input::ChildExited {
        report: dudes_proto::TurnReport::default(),
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: None,
    });
    let observed = harness
        .journal
        .iter()
        .find(
            |entry| matches!(entry.record, Record::TurnEnded { turn: ended, .. } if ended == turn),
        )
        .unwrap()
        .clone();
    let mut inferred = observed.clone();
    if let Record::TurnEnded { provenance, .. } = &mut inferred.record {
        *provenance = Provenance::Inferred;
    }
    world.apply(Lsn(1004), &inferred);
    assert!(!world.closing.as_ref().unwrap().ready(&world));
    world.apply(Lsn(1005), &observed);
    assert!(world.closing.as_ref().unwrap().ready(&world));

    let late = harness.hire();
    let hired = harness
        .journal
        .iter()
        .find(|entry| matches!(entry.record, Record::AgentHired { id, .. } if id == late))
        .unwrap();
    world.apply(Lsn(1006), hired);
    assert_eq!(
        world
            .closing
            .as_ref()
            .unwrap()
            .participants
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [agent]
    );
}

#[test]
fn closing_hash_names_actual_task_answers_and_closure_but_leaves_ordinary_hash_unchanged() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness.set_goal(agent, &["/usr/bin/true"]);
    let goal = harness.supervisor.world().agents[&agent].goal.unwrap();
    let ordinary = harness
        .supervisor
        .world()
        .loop_input(
            agent,
            dudes_supervisor::SpendView::default(),
            dudes_policy::WorkInput::default(),
            None,
            dudes_proto::Spend::default(),
        )
        .unwrap();
    let normal = dudes_supervisor::input_digest(&ordinary).unwrap();
    let mut input = dudes_policy::closing_input(Lsn(91), agent, goal, ordinary.clone());
    let digest = closing_input_digest(&input).unwrap();
    assert_ne!(normal, digest);
    assert_eq!(dudes_supervisor::input_digest(&ordinary).unwrap(), normal);
    input.announcement = Lsn(92);
    assert_ne!(closing_input_digest(&input).unwrap(), digest);
    input.announcement = Lsn(91);
    if let dudes_policy::ClosingWorkChoice::Ready { task, answers } = &mut input.selected_work {
        task.push_str(" changed");
        assert!(answers.is_empty());
    }
    assert_ne!(closing_input_digest(&input).unwrap(), digest);
    input = dudes_policy::closing_input(Lsn(91), agent, goal, ordinary);
    if let dudes_policy::ClosingWorkChoice::Ready { answers, .. } = &mut input.selected_work {
        answers.push(Lsn(82));
    }
    assert_ne!(closing_input_digest(&input).unwrap(), digest);
}

#[test]
fn an_incomplete_child_projection_remains_in_the_replayed_participant_snapshot() {
    let mut source = Harness::new();
    let agent = source.hire();
    source.set_goal(agent, &["/usr/bin/true"]);
    let mut world = source.supervisor.world().clone();
    let turn = world.agents[&agent].open_turn.unwrap();
    world.turns.get_mut(&turn).unwrap().start_time = None;
    world.apply(
        Lsn(1000),
        &entry(Record::ClosingTime {
            phase: ClosingPhase::Announced,
        }),
    );
    let session = world.closing.as_ref().unwrap();
    assert_eq!(
        session.participants.keys().copied().collect::<Vec<_>>(),
        [agent]
    );
    assert_eq!(session.missing().collect::<Vec<_>>(), [agent]);
    assert!(!session.ready(&world));
    assert!(matches!(
        source.supervisor.closing_announce().unwrap(),
        [dudes_supervisor::Effect::Persist {
            entry: Entry {
                record: Record::ClosingTime {
                    phase: ClosingPhase::Announced
                },
                ..
            },
            ..
        }]
    ));
}

#[test]
fn completed_cleanup_does_not_capture_a_later_normal_turn() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness.set_goal(agent, &["/usr/bin/true"]);
    let original = harness.supervisor.world().agents[&agent].open_turn.unwrap();
    let mut world = harness.supervisor.world().clone();
    world.apply(
        Lsn(1000),
        &entry(Record::ClosingTime {
            phase: ClosingPhase::Announced,
        }),
    );
    world.apply(
        Lsn(1001),
        &entry(Record::ClosingTime {
            phase: ClosingPhase::Complete,
        }),
    );
    assert!(world.closing.as_ref().unwrap().needs_recovery());
    harness.drive(Input::ChildExited {
        report: dudes_proto::TurnReport::default(),
        final_reply: None,
        elapsed_ms: None,
        turn: original,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: None,
    });
    let ended = harness
        .journal
        .iter()
        .find(|entry| matches!(entry.record, Record::TurnEnded { turn, .. } if turn == original))
        .unwrap();
    world.apply(Lsn(1002), ended);
    assert!(!world.closing.as_ref().unwrap().needs_recovery());
    let later = dudes_types::TurnId::from_parts(40, 50);
    let mut issued = harness
        .journal
        .iter()
        .find(|entry| matches!(entry.record, Record::TurnIssued { .. }))
        .unwrap()
        .clone();
    if let Record::TurnIssued { turn, .. } = &mut issued.record {
        *turn = later;
    }
    world.apply(Lsn(1003), &issued);
    assert!(world.turns[&later].is_issued_not_spawned());
    assert!(!world.closing.as_ref().unwrap().needs_recovery());
    assert_eq!(world.closing.as_ref().unwrap().complete, Some(Lsn(1001)));
}
