//! Closing progress counts.

use std::collections::BTreeMap;

use dudes_proto::{ClosingPhase, Entry, ExitStatus, Record, TurnOutcome};
use dudes_supervisor::{Input, World, testing::Harness};
use dudes_types::{Lsn, Timestamp};

fn apply(world: &mut World, position: u64, record: Record) {
    world.apply(
        Lsn(position),
        &Entry {
            at: Timestamp::from_unix_millis(17),
            record,
        },
    );
}

fn counts(world: &World) -> (u32, u32, u32, u32) {
    let session = world.closing.as_ref().unwrap();
    let progress = session.counts(world);
    assert_eq!(
        progress.owed == 0 && progress.running == 0,
        session.ready(world)
    );
    assert_eq!(progress.acknowledged + progress.settled + progress.owed, 3);
    (
        progress.acknowledged,
        progress.settled,
        progress.owed,
        progress.running,
    )
}

#[test]
fn durable_counts_partition_participants_and_keep_acknowledged_turns_running() {
    let mut harness = Harness::new();
    let (ack, _) = harness.hire_with_goal(&["/usr/bin/true"]);
    let (settled, goal) = harness.hire_with_goal(&["/usr/bin/true"]);
    let (owed, _) = harness.hire_with_goal(&["/usr/bin/true"]);
    let mut world = harness.supervisor.world().clone();
    apply(
        &mut world,
        1000,
        Record::ClosingTime {
            phase: ClosingPhase::Announced,
        },
    );
    assert_eq!(counts(&world), (0, 0, 3, 3));
    apply(
        &mut world,
        1001,
        Record::ClosingTime {
            phase: ClosingPhase::Ack { agent: ack },
        },
    );
    assert_eq!(counts(&world), (1, 0, 2, 3));
    apply(
        &mut world,
        1002,
        Record::GoalCleared {
            agent: settled,
            goal,
        },
    );
    assert_eq!(counts(&world), (1, 0, 2, 3));
    for (position, agent, expected) in [
        (1003, ack, (1, 0, 2, 2)),
        (1004, settled, (1, 1, 1, 1)),
        (1005, owed, (1, 1, 1, 0)),
    ] {
        let turn = harness.open_turn(agent).unwrap();
        harness.drive(Input::ChildExited {
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
        let ended = harness.journal.iter().find(|entry| matches!(entry.record, Record::TurnEnded { turn: ended, .. } if ended == turn)).unwrap();
        world.apply(Lsn(position), ended);
        assert_eq!(counts(&world), expected);
    }
    apply(
        &mut world,
        1006,
        Record::ClosingTime {
            phase: ClosingPhase::Ack { agent: ack },
        },
    );
    assert_eq!(counts(&world), (1, 1, 1, 0));
    apply(
        &mut world,
        1007,
        Record::ClosingTime {
            phase: ClosingPhase::Ack { agent: settled },
        },
    );
    assert_eq!(
        counts(&world),
        (2, 0, 1, 0),
        "acknowledgement takes precedence over settled standing"
    );
    apply(
        &mut world,
        1008,
        Record::AgentRetired {
            id: owed,
            reason: "finished".into(),
            browser_retire: None,
        },
    );
    assert_eq!(counts(&world), (2, 1, 0, 0));
}

#[test]
fn a_tick_can_queue_one_durable_change_per_roster_row_before_any_fold() {
    let mut harness = Harness::new();
    let agents: Vec<_> = (0..128).map(|_| harness.hire()).collect();
    let announcement = harness
        .supervisor
        .closing_announce()
        .unwrap()
        .iter()
        .map(dudes_supervisor::testing::Seen::of)
        .collect::<Vec<_>>();
    harness.trace.extend(announcement.iter().cloned());
    let token = match announcement.as_slice() {
        [dudes_supervisor::testing::Seen::Persist { token, .. }] => *token,
        _ => panic!("one announcement"),
    };
    harness.drive(harness.persisted(token, Lsn(10_000)));
    assert!(harness.supervisor.is_closing());
    for agent in &agents {
        harness.step(Input::Signal {
            agent: *agent,
            signal: dudes_supervisor::RuntimeSignal::HookTimeout,
        });
    }
    let before = harness.supervisor.world().last_applied_lsn;
    harness.step(Input::Tick);
    let records: Vec<_> = harness
        .supervisor
        .effects()
        .iter()
        .filter_map(|effect| match effect {
            dudes_supervisor::Effect::Persist { entry, .. } => Some(&entry.record),
            _ => None,
        })
        .collect();
    assert_eq!(records.len(), 128);
    assert!(
        records
            .iter()
            .all(|record| matches!(record, Record::BreakerBeat { .. }))
    );
    assert_eq!(harness.supervisor.world().last_applied_lsn, before);
}

#[test]
fn a_done_goal_with_an_open_child_stays_owed_until_observed_end() {
    let mut source = Harness::new();
    let (agent, goal) = source.hire_with_goal(&["/usr/bin/true"]);
    source.hire_with_goal(&["/usr/bin/true"]);
    source.hire_with_goal(&["/usr/bin/true"]);
    let turn = source.open_turn(agent).unwrap();
    let mut world = source.supervisor.world().clone();
    apply(
        &mut world,
        1000,
        Record::ClosingTime {
            phase: ClosingPhase::Announced,
        },
    );
    source.drive(Input::ChildExited {
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
    source.drive(Input::PredicateDone {
        goal,
        pass: true,
        evidence: dudes_proto::goal::Evidence::Command {
            argv: vec!["/usr/bin/true".into()],
            program: Some("/usr/bin/true".into()),
            exit: Some(0),
            output_tail: String::new(),
        },
    });
    let done = source.journal.iter().find(|entry| matches!(entry.record, Record::LoopDecided { agent: owner, decision: dudes_proto::LoopDecision::Done, .. } if owner == agent)).unwrap();
    world.apply(Lsn(1001), done);
    assert!(world.completed_goal_lsn(goal).is_some());
    assert_eq!(counts(&world), (0, 0, 3, 3));
    let end = source
        .journal
        .iter()
        .find(
            |entry| matches!(entry.record, Record::TurnEnded { turn: ended, .. } if ended == turn),
        )
        .unwrap();
    world.apply(Lsn(1002), end);
    assert_eq!(counts(&world), (0, 1, 2, 2));
}
