//! Invariant 2 for the case the current machine cannot reach: a `TurnIssued` that announced **no**
//! session is still re-issued by replay, exactly once.
//!
//! # The mutant this file exists to catch
//!
//! `TurnRow::is_issued_not_spawned` is `matches!(self.status, TurnStatus::Issued)`, and it is the
//! sole selector for **both** re-issue paths — crash rule (2) on `Input::Started`, and
//! `loop.resume`. Adding `&& self.session.is_some()` to it drops exactly the rows where the vendor
//! assigns the id, turning *"replay re-issues an un-spawned turn exactly once"* into *"never
//! re-issues it"* for that case.
//!
//! **The whole suite accepts that mutant today**, because the conjunct is a tautology *under the
//! current machine*: the only writer of `TurnIssued` hard-codes `session: Some(..)`, so no row with
//! `None` is ever produced live. It is not a tautology in the **type**: `Record::TurnIssued.session`
//! is `Option<SessionId>` and its own doc says `None` is what the vendor-assigns-the-id case
//! writes — the majority of providers. A journal written by a later version, or by a provider
//! whose adapter does not pin the id, replays into exactly that row.
//!
//! So this file does not drive the machine into the state; it **replays** the state, which is the
//! only way to reach it and is also the way a real daemon would.
//!
//! # The control
//!
//! The same fixture with `session: Some(..)` re-issues exactly once too. Without it, a machine that
//! re-issued *nothing* would satisfy the assertion above.

use std::collections::BTreeMap;

use dudes_proto::{
    AutomaticHold, Entry, ExitStatus, HoldReason, InstructionBaselineRef, LoopDecision, Record,
    TurnOutcome,
};
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_supervisor::testing::{FakeClock, FakeHost, Harness, Seen, journal_with_issued_turn};
use dudes_supervisor::{Command, Input};
use dudes_types::{
    AgentId, GoalId, IsolationMode, Lsn, Mode, Provenance, SessionId, Timestamp, TurnId,
};

/// Distinct, ordered stamps, so the replayed entries look like a real log rather than one moment.
fn at(step: i64) -> Timestamp {
    Timestamp::from_unix_millis(1_787_394_600_000 + step)
}

/// A valid prefix with a vendor-confirmed session A and a later durable `Resume(A)` turn that
/// never reached `TurnSpawned`.
fn journal_with_confirmed_session_and_issued_resume(
    task: &str,
) -> (Vec<Entry>, AgentId, GoalId, TurnId, SessionId) {
    let goal = GoalId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAW").expect("a ulid");
    let prior = TurnId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAY").expect("a ulid");
    let session = SessionId::new("11111111-1111-4111-8111-111111111111");
    let (mut entries, agent, turn) = journal_with_issued_turn(Some(session.clone()), task);
    let Record::GoalSet { goal: set, .. } = &mut entries[1].record else {
        unreachable!()
    };
    // Keep the goal clean so only the durable turn task exercises adapter refusal.
    "make the tests pass".clone_into(&mut set.text);
    entries[2].at = at(6);
    entries.splice(
        2..2,
        [
            Entry {
                at: at(2),
                record: Record::TurnIssued {
                    agent,
                    turn: prior,
                    goal,
                    task: "previous work".to_owned(),
                    session: Some(session.clone()),
                    mode: Mode::Managed,
                    model_selection: None,
                    lsn_of_decision: Lsn(1),
                    answers: Vec::new(),
                },
            },
            Entry {
                at: at(3),
                record: Record::TurnSpawned {
                    agent,
                    turn: prior,
                    pid: 7,
                    start_time: 11,
                    boot: None,
                    session: Some(session.clone()),
                    reissued: false,

                    mcp_grant: None,
                },
            },
            Entry {
                at: at(4),
                record: Record::SessionLearned {
                    agent,
                    turn: prior,
                    session: session.clone(),
                },
            },
            Entry {
                at: at(5),
                record: Record::TurnEnded {
                    final_reply: None,
                    elapsed_ms: None,
                    session_usage: None,
                    agent,
                    turn: prior,
                    mode: Mode::Managed,
                    provenance: Provenance::Observed,
                    exit: ExitStatus {
                        code: Some(0),
                        signal: None,
                    },
                    usage: None,
                    model_usage: BTreeMap::new(),
                    last_message: Some("continue".to_owned()),
                    session: Some(session.clone()),
                    outcome: Some(TurnOutcome::Completed),
                },
            },
        ],
    );
    (entries, agent, goal, turn, session)
}

/// Replay `entries` into a fresh daemon run and return the harness it produced.
fn replay(entries: Vec<Entry>) -> Harness {
    let mut harness = Harness::new();
    harness.journal = entries;
    // A fresh machine, the journal replayed through `Input::Replayed`, then `Input::Started` —
    // which is what runs the crash rules.
    harness.restart();
    harness
}

/// How many launches the harness asked for, for `turn`.
fn spawns(harness: &Harness, turn: TurnId) -> usize {
    harness
        .trace
        .iter()
        .filter(|seen| matches!(seen, Seen::Spawn { turn: t, .. } if *t == turn))
        .count()
}

fn assert_fresh_first_launch(harness: &Harness, turn_id: TurnId, old_session: &SessionId) {
    let announced = harness
        .records()
        .find_map(|record| match record {
            Record::TurnIssued { turn, session, .. } if *turn == turn_id => session.clone(),
            _ => None,
        })
        .expect("the later turn announces a session");
    assert_ne!(&announced, old_session, "the later turn does not resume A");
    let argv: Vec<String> = harness
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::Spawn { turn, spec, .. } if *turn == turn_id => Some(
                spec.argv
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect(),
            ),
            _ => None,
        })
        .expect("the later turn reaches one spawn");
    assert!(argv.iter().any(|arg| arg == "--session-id"), "{argv:?}");
    assert!(!argv.iter().any(|arg| arg == "--resume"), "{argv:?}");
    assert!(
        !argv.iter().any(|arg| arg == old_session.as_str()),
        "{argv:?}"
    );
}

/// **The mutant's target.** A `TurnIssued` that announced no session is re-issued exactly once.
///
/// Adding `&& self.session.is_some()` to `TurnRow::is_issued_not_spawned` makes this zero.
#[test]
fn replay_reissues_a_turn_that_announced_no_session_exactly_once() {
    let (entries, _agent, turn) = journal_with_issued_turn(None, "make the tests pass");
    let harness = replay(entries);

    assert_eq!(
        spawns(&harness, turn),
        1,
        "invariant 2: an un-spawned turn is re-issued exactly once, whether or not it announced a \
         session — the selector is the turn's *status*, and a session is not part of it"
    );
    assert_eq!(
        harness
            .records()
            .filter(|record| matches!(record, Record::TurnIssued { .. }))
            .count(),
        0,
        "and no second TurnIssued: the first one is already on the disk"
    );
}

#[test]
fn a_manually_loaded_prefix_records_its_missing_issue_boundary_once() {
    let (entries, _agent, turn) = journal_with_issued_turn(None, "make the tests pass");
    let harness = replay(entries);
    assert_eq!(
        harness
            .records()
            .filter(|record| matches!(record,
                Record::InstructionObservationFinished {
                    baseline: InstructionBaselineRef::MissingIssueBoundary { turn: found, .. },
                    ..
                } if *found == turn))
            .count(),
        1,
        "the replayed prefix leaves one exact missing-boundary obligation"
    );
}

/// The control: the same fixture *with* a session re-issues exactly once too.
///
/// Without this arm, a machine that re-issued nothing at all would pass the test above — and that
/// is precisely the failure the mutant produces, so the pair is what makes the mutant visible.
#[test]
fn replay_reissues_a_turn_that_announced_a_session_exactly_once() {
    let session = SessionId::new("11111111-1111-4111-8111-111111111111");
    let (entries, _agent, turn) = journal_with_issued_turn(Some(session), "make the tests pass");
    let harness = replay(entries);

    assert_eq!(
        spawns(&harness, turn),
        1,
        "control: the case the live machine does produce is re-issued once"
    );
}

/// And the fold itself: a replayed `TurnIssued` with no session leaves a row that
/// `issued_not_spawned` selects.
///
/// The unit-level statement of the same fact, one layer below the harness, so a failure says
/// whether the selector or the runtime around it moved. `world.rs` had no test of its own before
/// this.
#[test]
fn a_replayed_turn_without_a_session_is_selected_as_issued_not_spawned() {
    let (entries, _agent, turn) = journal_with_issued_turn(None, "make the tests pass");
    let mut harness = Harness::new();
    for (entry, position) in entries.into_iter().zip(1u64..) {
        let _ = harness
            .supervisor
            .handle(dudes_supervisor::Input::Replayed {
                lsn: dudes_types::Lsn(position),
                entry,
            });
    }
    let selected: Vec<TurnId> = harness
        .supervisor
        .world()
        .issued_not_spawned()
        .map(|(turn, _)| turn)
        .collect();
    assert_eq!(
        selected,
        vec![turn],
        "the row is selected on its status alone; its session is not part of the question"
    );
    assert!(
        harness.supervisor.world().turns[&turn].session.is_none(),
        "control: the row really does carry no session, so the assertion above is about that case"
    );
}

/// Replay refusal ends once without a child and invalidates the confirmed session.
/// The hold survives restart. Resume announces a fresh First session.
#[test]
fn replay_refusal_ends_without_child_clears_session_and_next_turn_is_first() {
    let (entries, agent, _goal, refused_turn, session_a) =
        journal_with_confirmed_session_and_issued_resume("ship\0it");
    let mut harness = replay(entries);

    assert_eq!(
        spawns(&harness, refused_turn),
        0,
        "typed adapter refusal happens before any host spawn"
    );

    assert_refused_end(&harness, agent, refused_turn);
    assert_eq!(harness.supervisor.world().agents[&agent].session, None);
    assert_eq!(harness.supervisor.world().agents[&agent].open_turn, None);

    let issued_before_restart = harness
        .journal
        .iter()
        .filter(|entry| matches!(entry.record, Record::TurnIssued { .. }))
        .count();
    harness.restart();
    assert_eq!(
        harness
            .journal
            .iter()
            .filter(|entry| {
                matches!(entry.record, Record::TurnEnded { turn, .. } if turn == refused_turn)
            })
            .count(),
        1,
        "the durable end suppresses both a second end and a later reissue"
    );
    assert_eq!(spawns(&harness, refused_turn), 0);
    assert_eq!(
        harness.open_turn(agent),
        None,
        "restart preserves the refusal hold"
    );
    assert_eq!(
        harness
            .journal
            .iter()
            .filter(|entry| matches!(entry.record, Record::TurnIssued { .. }))
            .count(),
        issued_before_restart
    );
    assert_eq!(
        harness.supervisor.world().agents[&agent].automatic_hold,
        Some(AutomaticHold::LaunchRefused {
            reason: "task contains a NUL byte at byte offset 4".into()
        })
    );
    assert!(harness.records().any(|record| matches!(
        record,
        Record::LoopDecided {
            decision: LoopDecision::Hold {
                reason: HoldReason::LaunchRefused
            },
            ..
        }
    )));
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    let next = harness.open_turn(agent).unwrap_or_else(|| {
        panic!(
            "explicit Resume admits later clean work; hold: {:?}; records: {:?}",
            harness.supervisor.world().agents[&agent].automatic_hold,
            harness.records().map(Record::kind).collect::<Vec<_>>()
        )
    });
    assert_eq!(spawns(&harness, next), 1);
    assert_eq!(spawns(&harness, refused_turn), 0);
    assert_ne!(next, refused_turn);
    assert_fresh_first_launch(&harness, next, &session_a);
}

fn assert_refused_end(harness: &Harness, agent: dudes_types::AgentId, refused_turn: TurnId) {
    let ends: Vec<&Record> = harness
        .records()
        .filter(|record| matches!(record, Record::TurnEnded { turn, .. } if *turn == refused_turn))
        .collect();
    assert_eq!(ends.len(), 1, "one refusal writes exactly one end");
    let Record::TurnEnded {
        agent: ended_agent,
        mode,
        provenance,
        exit,
        usage,
        model_usage,
        last_message,
        session,
        outcome,
        ..
    } = ends[0]
    else {
        unreachable!()
    };
    assert_eq!(*ended_agent, agent);
    assert_eq!(*mode, Mode::Managed);
    assert_eq!(*provenance, Provenance::Observed);
    assert_eq!(*exit, ExitStatus::UNKNOWN);
    assert_eq!(*usage, None);
    assert!(model_usage.is_empty());
    assert_eq!(*last_message, None);
    assert_eq!(*session, None, "a no-child end copies no turn session");
    let Some(TurnOutcome::LaunchRefused { reason }) = outcome else {
        panic!("the end must carry LaunchRefused: {outcome:?}");
    };
    assert_eq!(reason, "task contains a NUL byte at byte offset 4");
    assert!(!reason.contains("ship"), "the reason copies no task bytes");
}

/// The otherwise-identical clean replay remains a spawn, proving that the
/// new end is selected by typed refusal rather than by replay itself.
#[test]
fn replay_still_spawns_a_clean_task_after_refusal_end_is_added() {
    let (clean_entries, _agent, _goal, clean_turn, _session) =
        journal_with_confirmed_session_and_issued_resume("ship it");
    let clean = replay(clean_entries);
    assert_eq!(
        spawns(&clean, clean_turn),
        1,
        "control: the same replay path still spawns a clean task exactly once"
    );
    assert!(clean.records().all(|record| !matches!(
        record,
        Record::TurnEnded {
            outcome: Some(TurnOutcome::LaunchRefused { .. }),
            ..
        }
    )));
}

/// The future-grammar requirement: a clean task from an old journal can become refused when
/// its recorded untrusted repository meets an adapter configuration that cannot suppress project
/// settings. The refusal ends without a child and carries only the fixed safe display.
#[test]
fn replayed_untrusted_settings_refusal_ends_without_a_child() {
    let (entries, _agent, _goal, turn, session) =
        journal_with_confirmed_session_and_issued_resume("ship it");
    let mut harness = Harness::with_adapter(
        FakeClock::ticking(1_787_394_600_000, 1),
        FakeHost::wall_clock_anchored(),
        || {
            Box::new(ClaudeAdapter::new(ClaudeConfig {
                isolation: IsolationMode::Inherit,
                ..ClaudeConfig::default()
            }))
        },
    );
    harness.journal = entries;
    harness.restart();

    assert_eq!(spawns(&harness, turn), 0, "the refusal creates no child");
    let ends: Vec<&Record> = harness
        .records()
        .filter(|record| matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn))
        .collect();
    assert_eq!(ends.len(), 1, "one old turn gets one durable no-child end");
    let Record::TurnEnded {
        outcome: Some(TurnOutcome::LaunchRefused { reason }),
        session: ended_session,
        ..
    } = ends[0]
    else {
        panic!("expected LaunchRefused, got {:?}", ends[0]);
    };
    assert_eq!(
        reason,
        "an untrusted repository cannot contribute project settings; record repository trust at \
         hire or select settings that suppress the project source"
    );
    assert_eq!(*ended_session, None);
    assert!(!reason.contains("ship it"));
    assert!(!reason.contains(session.as_str()));
}
