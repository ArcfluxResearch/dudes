//! The Floor's stop of one turn: what it closes, what it leaves for the kill, and what a crash
//! or a resume does afterwards.

use dudes_proto::record::{BrowserControl, Hex32};
use dudes_proto::{Entry, Record, TurnOutcome};
use dudes_types::{AgentId, Lsn, TurnId};

use crate::host::{ProcessHost as _, SpawnResult};
use crate::testing::{Harness, Seen};
use crate::{Command, Input};

fn attribution(control_lsn: Option<Lsn>) -> BrowserControl {
    BrowserControl {
        request_id: Hex32::new([7; 32]),
        request_digest: Hex32::new([8; 32]),
        control_lsn,
        session_digest: Hex32::new([9; 32]),
    }
}

fn stop(harness: &mut Harness, agent: AgentId) {
    let control_lsn = harness.supervisor.world().agents[&agent].control_lsn;
    harness.drive(Input::Command(Command::BrowserHalt {
        agent,
        browser: attribution(control_lsn),
    }));
}

fn halts(harness: &Harness) -> Vec<Option<TurnId>> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::BrowserHalt { abandons, .. } => Some(*abandons),
            _ => None,
        })
        .collect()
}

fn spawn_recorded(harness: &Harness, turn: TurnId) -> bool {
    harness.journal.iter().any(
        |Entry { record, .. }| matches!(record, Record::TurnSpawned { turn: t, .. } if *t == turn),
    )
}

fn killed(harness: &Harness, turn: TurnId) -> bool {
    harness
        .trace
        .iter()
        .any(|seen| matches!(seen, Seen::Kill { turn: t, .. } if *t == turn))
}

/// Deliver the launch the machine emitted for `turn`, as the host would have answered it later.
fn deliver_spawn(harness: &mut Harness, turn: TurnId) {
    let Some(Seen::Spawn {
        spec,
        install,
        mcp_grant,
        ..
    }) = harness.spawns_of(turn).last().map(|seen| (*seen).clone())
    else {
        panic!("no launch was emitted for the turn");
    };
    let SpawnResult::Spawned(key) = harness.host.spawn(&spec, &install) else {
        panic!("the fake host launches");
    };
    harness.drive(Input::Spawned {
        turn,
        key,
        mcp_grant,
    });
}

/// A turn whose launch is out but whose spawn has not been answered is never closed by the stop:
/// closing it would make the late answer return early, and the started process would run
/// unrecorded. The halt leaves it open, and the spawn that arrives is recorded and then killed.
#[test]
fn a_stop_during_dispatch_leaves_the_turn_open_and_kills_it_when_the_spawn_arrives() {
    let mut harness = Harness::new();
    harness.launch = false;
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    assert!(harness.is_spawning(turn), "a control: its launch is out");

    stop(&mut harness, agent);
    assert_eq!(
        halts(&harness),
        [None],
        "no launch is abandoned mid-dispatch"
    );
    assert_eq!(harness.open_turn(agent), Some(turn));
    assert!(!killed(&harness, turn), "nothing exists to kill yet");

    deliver_spawn(&mut harness, turn);
    assert!(
        spawn_recorded(&harness, turn),
        "the started child is recorded"
    );
    assert!(killed(&harness, turn), "and the halt ends it");

    harness.drive(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: dudes_proto::ExitStatus {
            code: None,
            signal: Some(9),
        },
        usage: None,
        model_usage: std::collections::BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: None,
    });
    assert!(
        harness.records().any(|record| matches!(
            record,
            Record::TurnEnded {
                turn: t,
                outcome: Some(TurnOutcome::Killed {
                    by: dudes_proto::KillReason::Halt
                }),
                ..
            } if *t == turn
        )),
        "the turn ends as killed by the halt"
    );
}

/// A turn no launch has begun for is closed by the same append that halts, so neither a resume nor
/// a restart brings its task back.
#[test]
fn a_stop_before_dispatch_closes_the_turn_and_nothing_relaunches_it() {
    let mut harness = Harness::new();
    harness.launch = false;
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    harness.supervisor.spawning.clear();
    assert!(!harness.is_spawning(turn));

    stop(&mut harness, agent);
    assert_eq!(halts(&harness), [Some(turn)]);
    assert_eq!(harness.open_turn(agent), None, "the turn is closed");
    assert!(harness.supervisor.world().turns[&turn].has_ended());

    let lsn = harness.supervisor.world().agents[&agent].control_lsn;
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: Some(attribution(lsn)),
    }));
    assert!(
        harness.spawns_of(turn).len() <= 1,
        "a resume reissues nothing for the closed turn"
    );
    harness.restart();
    assert!(
        !harness.supervisor.world().turns[&turn].is_issued_not_spawned(),
        "a restart finds no unspawned turn"
    );
    deliver_spawn_if_any(&mut harness, turn);
    assert!(!spawn_recorded(&harness, turn));
}

fn deliver_spawn_if_any(harness: &mut Harness, turn: TurnId) {
    if !harness.spawns_of(turn).is_empty() {
        deliver_spawn(harness, turn);
    }
}

/// A crash inside the dispatch window leaves the turn issued under the Floor's halt. Replay
/// launches nothing while the halt stands.
#[test]
fn a_crash_after_a_stop_during_dispatch_launches_nothing_until_resume() {
    let mut harness = Harness::new();
    harness.launch = false;
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    stop(&mut harness, agent);
    let before = harness.spawns_of(turn).len();

    harness.restart();
    let row = &harness.supervisor.world().agents[&agent];
    assert!(row.halted && row.halted_by_floor);
    assert!(harness.supervisor.world().turns[&turn].is_issued_not_spawned());
    assert_eq!(
        harness.spawns_of(turn).len(),
        before,
        "the halt holds the relaunch"
    );
}

/// A later halt from the operator takes the Floor's mark off the standing halt, and a resume
/// clears it.
#[test]
fn the_floor_mark_follows_the_newest_halt() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    stop(&mut harness, agent);
    assert!(harness.supervisor.world().agents[&agent].halted_by_floor);

    harness.drive(Input::Command(Command::Halt { agent, text: None }));
    let row = &harness.supervisor.world().agents[&agent];
    assert!(row.halted && !row.halted_by_floor);

    stop(&mut harness, agent);
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    let row = &harness.supervisor.world().agents[&agent];
    assert!(!row.halted && !row.halted_by_floor);
}

/// A launch that re-issued a turn takes a different path on its spawn than a first launch does,
/// and the stop must end it just the same.
#[test]
fn a_stop_during_a_reissued_dispatch_also_kills_the_child() {
    let mut harness = Harness::new();
    harness.launch = false;
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    harness.supervisor.reissued.insert(turn, None);

    stop(&mut harness, agent);
    deliver_spawn(&mut harness, turn);
    assert!(spawn_recorded(&harness, turn));
    assert!(killed(&harness, turn));
}
