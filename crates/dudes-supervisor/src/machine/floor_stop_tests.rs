//! The Floor's stop of one turn: what it closes, what it leaves for the kill, and what a crash
//! or a resume does afterwards.

use dudes_proto::record::{BrowserControl, Hex32};
use dudes_proto::recovery::RecoveryAttribution;
use dudes_proto::{Entry, Record, TurnOutcome};
use dudes_types::{AgentId, Lsn, TurnId};

use crate::host::{ProcessHost as _, SpawnResult};
use crate::input::{RecoveryChoice, RecoveryCommand};
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
    let mut browser = attribution(control_lsn);
    let distinct = u8::try_from(harness.journal.len() + 100).expect("a short journal");
    browser.request_id = Hex32::new([distinct; 32]);
    harness.drive(Input::Command(Command::BrowserHalt { agent, browser }));
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

fn note_attribution() -> dudes_proto::BrowserNoteAttribution {
    dudes_proto::BrowserNoteAttribution {
        request_id_digest: Hex32::new([1; 32]),
        request_digest: Hex32::new([2; 32]),
        session_digest: Hex32::new([3; 32]),
    }
}

/// A no-goal agent whose queued message became a conversation turn with its launch preparation
/// still out. Nothing has answered the preparation.
fn conversation_turn_preparing() -> (Harness, AgentId, TurnId) {
    let mut harness = Harness::new();
    harness.launch = false;
    let agent = harness.hire();
    harness.drive(Input::Spend {
        agent,
        view: crate::input::SpendView::default(),
    });
    let context_lsn = harness.supervisor.world().agents[&agent].context_lsn;
    harness.drive(Input::Command(Command::ConversationMessageQueued {
        agent,
        text: "look at the failing test".into(),
        browser: note_attribution(),
        context_lsn,
        context_reset_turn: None,
    }));
    let turn = harness.open_turn(agent).expect("the message issued a turn");
    assert!(harness.is_spawning(turn), "a control: its launch is out");
    (harness, agent, turn)
}

fn answer_preparation(harness: &mut Harness, turn: TurnId) {
    let (generation, mut spec) = harness
        .trace
        .iter()
        .rev()
        .find_map(|seen| match seen {
            Seen::PrepareBrowserLaunch {
                turn: t,
                generation,
                spec,
            } if *t == turn => Some((*generation, spec.clone())),
            _ => None,
        })
        .expect("a preparation was requested");
    spec.argv[0] = std::env::temp_dir()
        .join("prepared-managed-engine")
        .into_os_string();
    harness.drive(Input::BrowserLaunchReady {
        turn,
        generation,
        prepared: Ok(Box::new(spec)),
    });
}

fn assert_nothing_stranded(harness: &mut Harness, agent: AgentId, turn: TurnId) {
    assert_eq!(halts(harness), [Some(turn)], "the stop closes the turn");
    assert_eq!(harness.open_turn(agent), None);
    assert!(harness.supervisor.world().turns[&turn].has_ended());
    assert!(!harness.is_spawning(turn));
    assert!(!harness.supervisor.browser_launches.contains_key(&turn));
    assert!(
        harness
            .journal
            .iter()
            .all(|entry| !matches!(entry.record, Record::BrowserNoteDispatchIntent { .. })),
        "no dispatch intent is recorded for the stopped task"
    );

    let lsn = harness.supervisor.world().agents[&agent].control_lsn;
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: Some(attribution(lsn)),
    }));
    assert!(
        harness.spawns_of(turn).is_empty(),
        "a resume relaunches nothing"
    );
    assert_eq!(harness.open_turn(agent), None);
}

/// A message turn stopped while its launch preparation is out is closed by the stop, the late
/// preparation answer launches nothing, and a resume does not bring the task back.
#[test]
fn a_stop_while_a_message_turn_is_preparing_closes_it() {
    let (mut harness, agent, turn) = conversation_turn_preparing();
    stop(&mut harness, agent);
    answer_preparation(&mut harness, turn);
    assert_nothing_stranded(&mut harness, agent, turn);
}

/// The same stop one step later, with the dispatch intent still waiting to be written.
#[test]
fn a_stop_while_a_message_turn_awaits_its_dispatch_intent_closes_it() {
    let (mut harness, agent, turn) = conversation_turn_preparing();
    harness
        .unfsynced
        .insert(dudes_proto::record::RecordKind::BrowserNoteDispatchIntent);
    answer_preparation(&mut harness, turn);
    stop(&mut harness, agent);
    assert_nothing_stranded(&mut harness, agent, turn);
}

/// While the stop is still being written, a preparation answer arriving first launches nothing.
#[test]
fn a_preparation_answer_before_the_stop_is_durable_launches_nothing() {
    let (mut harness, agent, turn) = conversation_turn_preparing();
    harness
        .unfsynced
        .insert(dudes_proto::record::RecordKind::BrowserHalt);
    stop(&mut harness, agent);
    let before = harness.trace.len();
    answer_preparation(&mut harness, turn);
    assert!(
        harness.trace[before..].iter().all(|seen| !matches!(
            seen,
            Seen::Spawn { .. }
                | Seen::Persist {
                    entry: Entry {
                        record: Record::BrowserNoteDispatchIntent { .. },
                        ..
                    },
                    ..
                }
        )),
        "the claim stops the launch before the stop is durable"
    );
}

/// A saving turn drains only on its own end, so a stop leaves it to the halt and the kill.
#[test]
fn a_stop_during_closing_time_does_not_close_the_saving_turn() {
    let mut harness = Harness::new();
    harness.launch = false;
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    harness.supervisor.spawning.clear();
    harness.supervisor.closing_active = true;
    stop(&mut harness, agent);
    assert_eq!(halts(&harness), [None]);
    assert_eq!(harness.open_turn(agent), Some(turn));
}

/// A reissue that a breaker at Stop forbids launches nothing, so the kill has no process to end
/// the moment it starts.
#[test]
fn a_reissue_under_a_breaker_stop_launches_nothing() {
    let mut harness = Harness::new();
    harness.launch = false;
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    harness.supervisor.spawning.clear();
    let before = harness.spawns_of(turn).len();
    harness
        .supervisor
        .world
        .agents
        .get_mut(&agent)
        .expect("the agent exists")
        .rung = dudes_types::BreakerRung::Stop;
    let task = harness.supervisor.world().turns[&turn].task.clone();
    harness
        .supervisor
        .reissue_turn(turn, agent, &task, None, &crate::RecoveryOperation::empty());
    assert_eq!(harness.spawns_of(turn).len(), before);
}

/// After a crash inside the dispatch window an ordinary resume relaunches nothing, and the
/// explicitly consented repeat is a launch like any other: a second stop ends it too.
#[test]
fn a_consented_repeat_after_a_stop_is_killed_by_a_second_stop() {
    let mut harness = Harness::new();
    harness.launch = false;
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let source = harness.open_turn(agent).expect("a turn was issued");
    stop(&mut harness, agent);
    harness.restart();

    let launches = |harness: &Harness| {
        harness
            .trace
            .iter()
            .filter(|seen| matches!(seen, Seen::Spawn { .. }))
            .count()
    };
    let before = launches(&harness);
    let lsn = harness.supervisor.world().agents[&agent].control_lsn;
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: Some(attribution(lsn)),
    }));
    assert_eq!(launches(&harness), before, "a resume admits no old work");

    let expected = harness
        .supervisor
        .recovery_opportunity(agent)
        .expect("the interrupted turn offers explicit recovery")
        .expected;
    assert_eq!(expected.work.turn, source);
    harness.recover(RecoveryCommand {
        request_id: Hex32::new([40; 32]),
        digest: Hex32::new([41; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator {
            pid: 7,
            uid: Some(42),
        },
    });
    assert_eq!(
        launches(&harness),
        before + 1,
        "the consented repeat launches"
    );
    let Some(Seen::Spawn { turn, .. }) = harness
        .trace
        .iter()
        .rev()
        .find(|seen| matches!(seen, Seen::Spawn { .. }))
        .cloned()
    else {
        unreachable!()
    };

    stop(&mut harness, agent);
    deliver_spawn(&mut harness, turn);
    assert!(spawn_recorded(&harness, turn));
    assert!(killed(&harness, turn), "the repeated launch is killed");
}

/// The attached agent's keyboard belongs to a person, so the machine refuses the stop itself.
#[test]
fn the_machine_refuses_a_stop_for_an_attached_agent() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness
        .supervisor
        .world
        .agents
        .get_mut(&agent)
        .expect("the agent exists")
        .attached = true;
    stop(&mut harness, agent);
    assert!(halts(&harness).is_empty());
    assert!(!harness.supervisor.world().agents[&agent].halted);
}
