//! Closing time stops waiting for a Managed participant that can never acknowledge.

use std::collections::BTreeMap;

use dudes_proto::goal::Evidence;
use dudes_proto::rpc::closing::ClosingSettledReason;
use dudes_proto::{ClosingPhase, Entry, ExitStatus, Record, TurnOutcome};
use dudes_supervisor::closing::Standing;
use dudes_supervisor::testing::Harness;
use dudes_supervisor::{Command, Effect, Input, World};
use dudes_types::{AgentId, GoalId, Lsn, Timestamp, TurnId};

const ANNOUNCED: Lsn = Lsn(10_000);

fn announce(harness: &mut Harness) {
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
        other => panic!("one announcement expected, saw {other:?}"),
    };
    harness.drive(harness.persisted(token, ANNOUNCED));
}

fn end_turn(harness: &mut Harness, turn: TurnId) {
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
}

fn judge(harness: &mut Harness, goal: GoalId, pass: bool) {
    harness.drive(Input::PredicateDone {
        goal,
        pass,
        evidence: Evidence::Command {
            argv: vec!["/usr/bin/true".into()],
            program: Some("/usr/bin/true".into()),
            exit: Some(i32::from(!pass)),
            output_tail: String::new(),
        },
    });
}

fn settled(harness: &Harness) -> Vec<(AgentId, ClosingSettledReason)> {
    let world = harness.supervisor.world();
    world.closing.as_ref().unwrap().settled(world).collect()
}

fn ready(harness: &Harness) -> bool {
    let world = harness.supervisor.world();
    world.closing.as_ref().unwrap().ready(world)
}

fn issued_after(harness: &Harness) -> usize {
    harness
        .records()
        .skip_while(|record| !matches!(record, Record::ClosingTime { .. }))
        .filter(|record| matches!(record, Record::TurnIssued { .. }))
        .count()
}

#[test]
fn a_stop_during_a_turn_that_ends_done_completes_without_waiting_and_names_the_agent() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["/usr/bin/true"]);
    let turn = harness.open_turn(agent).unwrap();
    announce(&mut harness);
    assert!(!ready(&harness), "the running turn is still owed");
    assert!(settled(&harness).is_empty());

    end_turn(&mut harness, turn);
    assert!(
        !ready(&harness),
        "the turn ended but its completion check has not been judged"
    );
    judge(&mut harness, goal, true);

    assert_eq!(settled(&harness), [(agent, ClosingSettledReason::GoalDone)]);
    assert!(ready(&harness));
    assert_eq!(issued_after(&harness), 0, "a Done agent is never tasked");
    harness.supervisor.closing_finish(false).unwrap();
    assert!(matches!(
        harness.supervisor.effects(),
        [Effect::Persist {
            entry: Entry {
                record: Record::ClosingTime {
                    phase: ClosingPhase::Complete
                },
                ..
            },
            ..
        }]
    ));
}

#[test]
fn an_open_human_question_stays_owed_to_the_bound() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["/usr/bin/true"]);
    let turn = harness.open_turn(agent).unwrap();
    harness.drive(Input::Command(Command::Ask {
        agent,
        text: "which branch?".into(),
    }));
    announce(&mut harness);
    end_turn(&mut harness, turn);
    judge(&mut harness, goal, false);

    assert!(
        harness.supervisor.world().agents[&agent]
            .open_question
            .is_some()
    );
    assert!(settled(&harness).is_empty());
    assert!(!ready(&harness));
    assert!(matches!(
        harness.supervisor.closing_finish(false),
        Err(dudes_supervisor::closing::ClosingError::NotReady)
    ));
}

#[test]
fn a_paused_agent_stays_owed_to_the_bound() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["/usr/bin/true"]);
    let turn = harness.open_turn(agent).unwrap();
    harness.drive(Input::Command(Command::Pause {
        agent,
        browser: None,
    }));
    announce(&mut harness);
    end_turn(&mut harness, turn);
    judge(&mut harness, goal, false);
    assert!(settled(&harness).is_empty());
    assert!(!ready(&harness));
}

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(17),
        record,
    }
}

fn find(harness: &Harness, f: impl Fn(&Record) -> bool) -> Entry {
    harness
        .journal
        .iter()
        .find(|entry| f(&entry.record))
        .unwrap()
        .clone()
}

fn announced_world(harness: &Harness) -> World {
    let mut world = harness.supervisor.world().clone();
    world.apply(
        Lsn(1000),
        &entry(Record::ClosingTime {
            phase: ClosingPhase::Announced,
        }),
    );
    world
}

fn standing(world: &World, agent: AgentId) -> Standing {
    let session = world.closing.as_ref().unwrap();
    world.closing_standing(agent, &session.participants[&agent])
}

fn idle_agent_world() -> (Harness, AgentId, GoalId, World) {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["/usr/bin/true"]);
    let turn = harness.open_turn(agent).unwrap();
    let mut world = announced_world(&harness);
    end_turn(&mut harness, turn);
    let ended = find(
        &harness,
        |record| matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn),
    );
    world.apply(Lsn(1001), &ended);
    (harness, agent, goal, world)
}

#[test]
fn every_settled_reason_is_read_from_the_journal_and_nothing_else_settles() {
    let (harness, agent, goal, open) = idle_agent_world();
    assert_eq!(
        standing(&open, agent),
        Standing::Open,
        "a live goal with no open turn is still owed a saving turn"
    );

    let mut cleared = open.clone();
    cleared.apply(Lsn(1002), &entry(Record::GoalCleared { agent, goal }));
    assert_eq!(
        standing(&cleared, agent),
        Standing::Settled(ClosingSettledReason::GoalCleared)
    );

    let mut retired = open.clone();
    retired.apply(
        Lsn(1002),
        &entry(Record::AgentRetired {
            id: agent,
            reason: "done".into(),
            browser_retire: None,
        }),
    );
    assert_eq!(
        standing(&retired, agent),
        Standing::Settled(ClosingSettledReason::Retired)
    );

    let mut replaced = open.clone();
    let mut set = find(&harness, |record| matches!(record, Record::GoalSet { .. }));
    if let Record::GoalSet { goal: new, .. } = &mut set.record {
        new.id = GoalId::from_parts(7, 8);
    }
    replaced.apply(Lsn(1002), &set);
    assert_eq!(
        standing(&replaced, agent),
        Standing::Settled(ClosingSettledReason::GoalReplaced)
    );

    let mut halted = open;
    halted.apply(
        Lsn(1002),
        &entry(Record::Halt {
            agent,
            text: Some("stop".into()),
        }),
    );
    assert_eq!(
        standing(&halted, agent),
        Standing::Open,
        "a standing control can be lifted inside the window"
    );
}

#[test]
fn an_agent_whose_goal_was_cleared_before_announcement_is_settled_as_having_no_goal() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["/usr/bin/true"]);
    let turn = harness.open_turn(agent).unwrap();
    harness.drive(Input::Command(Command::GoalClear { agent, goal }));
    announce(&mut harness);
    assert_eq!(
        harness
            .supervisor
            .world()
            .closing
            .as_ref()
            .unwrap()
            .participants[&agent]
            .goal,
        None
    );
    assert!(!ready(&harness), "the running turn is still owed");
    end_turn(&mut harness, turn);
    assert_eq!(settled(&harness), [(agent, ClosingSettledReason::NoGoal)]);
    assert!(ready(&harness));
}

#[test]
fn an_attached_participant_is_never_settled() {
    let (_, agent, goal, mut world) = idle_agent_world();
    world.apply(Lsn(1002), &entry(Record::GoalCleared { agent, goal }));
    assert_eq!(
        standing(&world, agent),
        Standing::Settled(ClosingSettledReason::GoalCleared)
    );
    world
        .closing
        .as_mut()
        .unwrap()
        .participants
        .get_mut(&agent)
        .unwrap()
        .mode = dudes_types::Mode::Attached;
    assert_eq!(standing(&world, agent), Standing::Open);
    let session = world.closing.as_ref().unwrap();
    assert!(!session.ready(&world));
    assert!(session.settled(&world).next().is_none());
}

#[test]
fn an_open_turn_keeps_a_terminal_goal_open_and_an_acknowledged_participant_is_never_settled() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["/usr/bin/true"]);
    let mut world = announced_world(&harness);
    world.apply(Lsn(1001), &entry(Record::GoalCleared { agent, goal }));
    let session = world.closing.as_ref().unwrap();
    assert_eq!(
        standing(&world, agent),
        Standing::Open,
        "a terminal goal does not settle a participant whose turn is still open"
    );
    assert!(!session.ready(&world));
    assert!(
        session.settled(&world).next().is_none(),
        "at expiry the report keeps it missing and unsettled, so the CLI still warns"
    );
    assert_eq!(session.missing().collect::<Vec<_>>(), [agent]);
    world.apply(
        Lsn(1002),
        &entry(Record::ClosingTime {
            phase: ClosingPhase::Ack { agent },
        }),
    );
    assert_eq!(standing(&world, agent), Standing::Open);
    assert!(
        world
            .closing
            .as_ref()
            .unwrap()
            .settled(&world)
            .next()
            .is_none()
    );
}

#[test]
fn standing_is_recomputed_from_the_journal_and_replay_reaches_the_same_report() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["/usr/bin/true"]);
    let turn = harness.open_turn(agent).unwrap();
    announce(&mut harness);
    let announced_at = harness.journal.len();
    assert!(!ready(&harness));
    end_turn(&mut harness, turn);
    judge(&mut harness, goal, true);
    harness.supervisor.closing_finish(false).unwrap();
    let live = harness.supervisor.world();
    let complete = entry(Record::ClosingTime {
        phase: ClosingPhase::Complete,
    });

    let mut journal: Vec<Entry> = harness.journal.clone();
    journal.insert(
        announced_at,
        entry(Record::ClosingTime {
            phase: ClosingPhase::Announced,
        }),
    );
    let replay = |entries: &[Entry]| {
        let mut world = World::default();
        for (index, entry) in entries.iter().enumerate() {
            world.apply(Lsn(index as u64 + 1), entry);
        }
        world.apply(Lsn(entries.len() as u64 + 1), &complete);
        world
    };
    let report = |world: &World| {
        let session = world.closing.as_ref().unwrap();
        (
            session.missing().collect::<Vec<_>>(),
            session.settled(world).collect::<Vec<_>>(),
            session.ready(world),
        )
    };
    let (first, second) = (replay(&journal), replay(&journal));
    assert_eq!(report(&first), report(&second));
    assert_eq!(
        report(&first),
        (
            vec![agent],
            vec![(agent, ClosingSettledReason::GoalDone)],
            true
        )
    );
    assert_eq!(
        live.closing
            .as_ref()
            .unwrap()
            .settled(live)
            .collect::<Vec<_>>(),
        [(agent, ClosingSettledReason::GoalDone)]
    );
}
