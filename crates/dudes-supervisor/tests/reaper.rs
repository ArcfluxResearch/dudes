//! The reaper: what an `Effect::Probe` is emitted for, and what each `Input::Liveness` answer
//! decides.
//!
//! Three answers, three rules, and the whole file is arranged so each can fail on its own.
//! `Gone` ends the turn `Observed`; `Unknown` writes **nothing** and defers a standing kill once;
//! `Alive` adopts, which is what repopulates the run-local child map after a restart.
//!
//! **Nothing here injects a liveness answer where the emission is what is under test.** The
//! reaper's decisions are reached by driving `FakeHost` — its process table and its boot identity
//! are the two dials the liveness table turns — so a test that passes cannot be passing on a
//! handler that no effect can reach. The exceptions are deliberate and marked: the `Killed`-versus-`Vanished`
//! distinction, the late-`ChildExited` guard and the ending-turn liveness guard describe answers after their effects are already out; those inject the input the runtime
//! will deliver so the ordering edge itself stays deterministic.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use dudes_proto::goal::Evidence;
use dudes_proto::{ExitStatus, KillDeferredReason, KillReason, Record, RecordKind, TurnOutcome};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{
    Command, HostBoot, Input, Liveness, PersistToken, ProbeAnswer, RuntimeSignal,
};
use dudes_types::{
    AgentId, AgentStatus, BreakerRung, BreakerSignal, GoalId, Lsn, Provenance, TurnId, TurnStatus,
};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;

// ---- helpers ---------------------------------------------------------------------------------

/// A predicate result the machine will accept; its content is never what a test here asserts on.
fn evidence() -> Evidence {
    Evidence::Command {
        argv: vec!["npm".into(), "test".into()],
        program: Some("/opt/toolchain/bin/npm".into()),
        exit: Some(0),
        output_tail: String::new(),
    }
}

/// The pid the journal recorded for a turn.
fn pid_of(harness: &Harness, turn: TurnId) -> u32 {
    harness.supervisor.world().turns[&turn]
        .pid
        .expect("the child's pid was recorded")
}

/// Whether the machine asked the process table about this turn since the trace was last cleared.
fn probed(harness: &Harness, turn: TurnId) -> bool {
    harness
        .trace
        .iter()
        .any(|seen| matches!(seen, Seen::Probe { turn: t } if *t == turn))
}

/// How many times the machine asked the process table about this turn since the trace was last
/// cleared. The boolean above answers "at all"; a scenario that probes more than once needs the
/// count, because the answer to "again" is not a fresh `true`.
fn probes(harness: &Harness, turn: TurnId) -> usize {
    harness
        .trace
        .iter()
        .filter(|seen| matches!(seen, Seen::Probe { turn: t } if *t == turn))
        .count()
}

/// Whether a signal was sent for this turn since the trace was last cleared.
fn killed(harness: &Harness, turn: TurnId) -> bool {
    harness
        .trace
        .iter()
        .any(|seen| matches!(seen, Seen::Kill { turn: t, .. } if *t == turn))
}

/// Every `TurnEnded` the machine asked to have written for one turn, oldest first.
fn ends_of(harness: &Harness, turn: TurnId) -> Vec<(Provenance, ExitStatus, Option<TurnOutcome>)> {
    harness
        .records()
        .filter_map(|record| match record {
            Record::TurnEnded {
                turn: t,
                provenance,
                exit,
                outcome,
                ..
            } if *t == turn => Some((*provenance, *exit, outcome.clone())),
            _ => None,
        })
        .collect()
}

/// How many `TurnSpawned` records name this turn.
fn spawn_records(harness: &Harness, turn: TurnId) -> usize {
    harness
        .records()
        .filter(|record| matches!(record, Record::TurnSpawned { turn: t, .. } if *t == turn))
        .count()
}

/// The one persist a deliberately single-effect step emitted, with its record kind checked.
fn persist_token(seen: &[Seen], kind: RecordKind) -> PersistToken {
    let persists: Vec<_> = seen
        .iter()
        .filter_map(|seen| match seen {
            Seen::Persist { token, entry } => Some((*token, entry.record.kind())),
            _ => None,
        })
        .collect();
    assert_eq!(
        persists.len(),
        1,
        "a control: this step emits exactly one persist"
    );
    assert_eq!(
        persists[0].1, kind,
        "a control: this step emits the expected record kind"
    );
    persists[0].0
}

/// How many `KillDeferred` records name this turn.
fn deferrals(harness: &Harness, turn: TurnId) -> usize {
    harness
        .records()
        .filter(|record| {
            matches!(
                record,
                Record::KillDeferred {
                    turn: t,
                    reason: KillDeferredReason::LivenessUnknown,
                    ..
                } if *t == turn
            )
        })
        .count()
}

/// The rung the fold holds for one agent.
fn rung(harness: &Harness, agent: AgentId) -> BreakerRung {
    harness.supervisor.world().agents[&agent].rung
}

// ---- the three answers -----------------------------------------------------------------------

/// `Gone` ends the turn `Observed` and `Done` stays reachable after a crash.
///
/// The whole sequence is driven, not injected: a daemon run spawns a child, dies, and the second
/// run finds the pid missing from the process table. Crash rule (3) probes every open child, the
/// answer is `Gone`, and the end that follows carries `exit: UNKNOWN` and no `usage` — nothing
/// waited on that process, so there is nothing honest to report about how it died.
///
/// The second half is why the distinction exists: an end that did not run the goal's
/// predicate would leave a crashed goal unjudgeable forever, so the record's acknowledgement runs
/// it exactly as an ordinary end does, and an `Observed` pass is `Done` (invariant 3).
#[test]
fn gone_after_crash_ends_observed_vanished_and_done_is_reachable() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    // The daemon died with the child, and the process table no longer holds that pid.
    harness
        .host
        .answer(pid_of(&harness, turn), ProbeAnswer::NoSuchPid);
    harness.trace.clear();
    harness.restart();

    assert!(
        probed(&harness, turn),
        "crash rule (3) asks about every open child"
    );
    let ends = ends_of(&harness, turn);
    assert_eq!(ends.len(), 1, "one end, from the reaper: {ends:?}");
    assert_eq!(
        ends[0].0,
        Provenance::Observed,
        "the daemon ran the probe itself (invariant 3)"
    );
    assert_eq!(
        ends[0].1,
        ExitStatus::UNKNOWN,
        "nothing waited on the process, so neither code nor signal is known"
    );
    assert_eq!(ends[0].2, Some(TurnOutcome::Vanished));
    assert!(
        harness.records().all(|record| !matches!(
            record,
            Record::TurnEnded {
                elapsed_ms: None,
                usage: Some(_),
                ..
            }
        )),
        "no usage either: no result line was ever read"
    );
    assert_eq!(
        harness.supervisor.world().turns[&turn].status,
        TurnStatus::Ended
    );
    assert!(harness.trace.iter().all(|seen| !matches!(
        seen,
        Seen::Spawn { .. }
            | Seen::Persist {
                entry: dudes_proto::Entry {
                    record: Record::TurnIssued { .. },
                    ..
                },
                ..
            }
    )));

    // And the goal is judged, so `Done` is reachable after a crash rather than only after a
    // clean exit.
    assert!(
        harness
            .trace
            .iter()
            .any(|seen| matches!(seen, Seen::RunPredicate { goal: g } if *g == goal)),
        "the end ran the goal's predicate"
    );
    harness.drive(Input::PredicateDone {
        goal,
        pass: true,
        evidence: evidence(),
    });
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Done);
    assert!(harness.records().any(|record| matches!(record,
        Record::PredicateResult { goal: observed, pass: true, provenance: Provenance::Observed, .. }
            if *observed == goal
    )));
    assert!(harness.records().any(|record| matches!(record,
        Record::LoopDecided { agent: owner, decision: dudes_proto::LoopDecision::Done, .. }
            if *owner == agent
    )));
    assert_eq!(
        harness
            .journal
            .iter()
            .filter(|entry| matches!(entry.record,
                Record::TurnSpawned { turn: spawned, .. } if spawned == turn
            ))
            .count(),
        1
    );
    assert_eq!(
        harness
            .journal
            .iter()
            .filter(|entry| matches!(entry.record,
                Record::TurnIssued { agent: owner, .. } if owner == agent
            ))
            .count(),
        1
    );
}

#[test]
fn gone_after_crash_failed_predicate_keeps_recovery_before_any_new_work() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).unwrap();
    harness
        .host
        .answer(pid_of(&harness, turn), ProbeAnswer::NoSuchPid);
    harness.trace.clear();
    harness.restart();
    assert!(harness.trace.iter().any(|seen| matches!(seen,
        Seen::RunPredicate { goal: observed } if *observed == goal
    )));
    harness.trace.clear();
    let mut failed = evidence();
    let Evidence::Command { exit, .. } = &mut failed else {
        unreachable!()
    };
    *exit = Some(1);
    harness.drive(Input::PredicateDone {
        goal,
        pass: false,
        evidence: failed,
    });
    assert!(harness.records().any(|record| matches!(record,
        Record::PredicateResult { goal: observed, pass: false, provenance: Provenance::Observed, .. }
            if *observed == goal
    )));
    assert!(harness.records().any(|record| matches!(record,
        Record::RecoveryRequired(required)
            if required.binding.work.agent == agent
                && required.binding.work.turn == turn
                && required.reason == dudes_proto::recovery::RecoveryReason::Vanished
    )));
    assert!(harness.trace.iter().all(|seen| !matches!(
        seen,
        Seen::Spawn { .. }
            | Seen::Persist {
                entry: dudes_proto::Entry {
                    record: Record::TurnIssued { .. }
                        | Record::LoopDecided {
                            decision: dudes_proto::LoopDecision::Continue { .. },
                            ..
                        },
                    ..
                },
                ..
            }
    )));
    assert_eq!(
        harness
            .journal
            .iter()
            .filter(|entry| matches!(entry.record,
                Record::TurnSpawned { turn: spawned, .. } if spawned == turn
            ))
            .count(),
        1
    );
    assert!(harness.supervisor.world().completed_goal(agent).is_none());
}

/// `Unknown` writes nothing, defers a standing kill once, and is re-asked on the
/// next beat.
///
/// Three clauses, three independent mutants. The `Unknown` here is the one no action on this host
/// resolves — the recorded key names no boot identity and this host produces one, the liveness table's
/// diagonal — which is exactly the case the record-volume argument is about: it will answer
/// `Unknown` on every beat for the rest of the run.
#[test]
fn unknown_liveness_writes_nothing_defers_kill_and_reprobes_on_tick() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    // The key was recorded with no boot identity; this host now produces one.
    harness.host.host_boot = HostBoot::Read("boot-b".to_owned());
    harness.trace.clear();
    harness.restart();

    // (a) Nothing at all is written for the turn, and nothing moves.
    assert!(probed(&harness, turn), "the open child was asked about");
    assert!(
        ends_of(&harness, turn).is_empty(),
        "an unresolved child is not guessed dead"
    );
    assert_eq!(
        harness.supervisor.world().turns[&turn].status,
        TurnStatus::Spawned,
        "the turn stays open"
    );
    assert_eq!(
        harness.supervisor.status_of(agent),
        AgentStatus::Running,
        "and the agent stays running"
    );

    // (b) A standing cause defers rather than signalling a pid it cannot verify.
    harness.trace.clear();
    harness.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));
    assert_eq!(
        deferrals(&harness, turn),
        1,
        "the halt is journaled as deferred, once"
    );
    assert!(
        !killed(&harness, turn),
        "and no signal was sent to an unverifiable pid"
    );

    // (c) The next beat asks again, and still writes no end.
    harness.trace.clear();
    harness.drive(Input::Tick);
    assert!(
        probed(&harness, turn),
        "the reaper re-asks on every beat while the answer is Unknown"
    );
    assert!(
        ends_of(&harness, turn).is_empty(),
        "and a beat is not evidence of absence"
    );
}

/// `KillDeferred` is appended once per standing cause and not once per beat.
///
/// The retry really is driven — a `StopCap` signal on every beat holds the rung at `Stop`, so
/// `kill_standing_cause` is re-entered on each one — which is what makes the count an assertion
/// about the guard rather than about the retry never happening. Deleting the guard turns 2
/// records into 2 + N.
///
/// The cause change is **breaker `Stop` first, then `Halt`**, deliberately: the precedence is
/// `retired > halted > Stop`, so a halt-then-breaker order would not change the cause at all, and
/// a retire-then-anything order would construct the `Command::Retire` that the RPC layer refuses
/// before it reaches this machine.
#[test]
fn kill_deferred_is_appended_once_per_standing_cause_not_per_tick() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    harness.host.host_boot = HostBoot::Read("boot-b".to_owned());
    harness.restart();
    harness.trace.clear();

    // Climb to `Stop`, the standing cause that is not an operator's command.
    for expected in [
        BreakerRung::Steer,
        BreakerRung::Constrain,
        BreakerRung::Stop,
    ] {
        harness.drive(Input::Signal {
            agent,
            signal: RuntimeSignal::StopCap,
        });
        harness.drive(Input::Tick);
        assert_eq!(rung(&harness, agent), expected);
    }
    assert_eq!(
        deferrals(&harness, turn),
        1,
        "the breaker's cause is deferred once"
    );

    // Ten more beats, each one re-entering the kill with the cause still standing.
    for _ in 0..10 {
        harness.drive(Input::Signal {
            agent,
            signal: RuntimeSignal::StopCap,
        });
        harness.drive(Input::Tick);
    }
    assert_eq!(
        rung(&harness, agent),
        BreakerRung::Stop,
        "a control: the cause stood throughout"
    );
    assert_eq!(
        deferrals(&harness, turn),
        1,
        "the retry loop is in memory: a beat appends nothing"
    );

    // A second, higher-precedence cause is a second fact, and gets a second record.
    harness.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));
    assert_eq!(
        deferrals(&harness, turn),
        2,
        "one per (turn, standing cause)"
    );

    for _ in 0..10 {
        harness.drive(Input::Signal {
            agent,
            signal: RuntimeSignal::StopCap,
        });
        harness.drive(Input::Tick);
    }
    assert_eq!(deferrals(&harness, turn), 2, "and still not one per beat");
    assert!(
        !killed(&harness, turn),
        "nothing was signalled at any point: the liveness never resolved"
    );
}

// ---- which outcome a reaper-ended turn carries -------------------------------------

/// **Which outcome a reaper-ended turn carries.** A turn this run killed, and then found `Gone` before
/// any `ChildExited`, ends `Killed{by}` — not `Vanished`.
///
/// The outcome is taken exactly as the `ChildExited` path takes it, `self.kills.remove(&turn)`
/// first, so the two ends tell the same story about the same act. `Vanished` is the vocabulary
/// for a surprise; spending it on an operator's halt that lost a race with `wait()` would stop it
/// meaning surprise. `exit` is `UNKNOWN` either way: a `Killed` the reaper writes
/// carries no signal number, because nothing waited on the process.
///
/// **The positive control is in the same test**, on a turn with no `kills` entry: without it the
/// first assertion passes on a machine that never writes `Vanished` at all.
///
/// The `Input::Liveness` is injected rather than driven because its real route is
/// `ProcessHost::kill` answering `Refused{liveness: Gone}` (`host.rs`), and no
/// runtime executes `Effect::Kill` yet. The emission half is covered by the driven tests above.
#[test]
fn a_killed_turn_the_reaper_finds_gone_ends_killed_not_vanished() {
    let mut harness = Harness::new();

    // The subject: a standing `Halt`, and the `Kill` it emitted.
    let (halted, _) = harness.hire_with_goal(&["npm", "test"]);
    let killed_turn = harness.open_turn(halted).expect("a turn was issued");
    harness.drive(Input::Command(Command::Halt {
        agent: halted,
        text: Some("stop".into()),
    }));
    assert!(
        killed(&harness, killed_turn),
        "a control: the signal went out, so the ledger holds the cause"
    );

    // The control: a turn nothing was ever killed for.
    let (untouched, _) = harness.hire_with_goal(&["npm", "test"]);
    let vanished_turn = harness.open_turn(untouched).expect("a turn was issued");

    // The reaper answers about both, before either child's exit is reported.
    harness.drive(Input::Liveness {
        turn: killed_turn,
        liveness: Liveness::Gone,
    });
    harness.drive(Input::Liveness {
        turn: vanished_turn,
        liveness: Liveness::Gone,
    });

    assert_eq!(
        ends_of(&harness, killed_turn),
        [(
            Provenance::Observed,
            ExitStatus::UNKNOWN,
            Some(TurnOutcome::Killed {
                by: KillReason::Halt
            })
        )],
        "the daemon knows what it did itself"
    );
    assert_eq!(
        ends_of(&harness, vanished_turn),
        [(
            Provenance::Observed,
            ExitStatus::UNKNOWN,
            Some(TurnOutcome::Vanished)
        )],
        "and a child nobody signalled is the surprise the word is for"
    );
    assert_eq!(
        harness.supervisor.world().turns[&killed_turn].status,
        TurnStatus::Killed,
        "which is the row an operator reads first"
    );
    assert_eq!(
        harness.supervisor.world().turns[&vanished_turn].status,
        TurnStatus::Ended
    );
}

/// **A call made explicitly:** `on_child_exited` refuses a turn
/// that is already ending, and the refusal lives in the one private constructor of
/// `Persist{TurnEnded}`.
///
/// **The world alone cannot close this window, and the control below proves it.** `World::apply`
/// sets a turn's status from the *acknowledged* entry, so between the reaper's emit and its
/// acknowledgement the row still reads `Spawned` — a guard written as "return unless this is an
/// open child" reads a status that has not moved yet, and a `ChildExited` arriving in exactly the
/// crash window the ledger exists to model would sail through it. The guard is therefore
/// run-local, for the same reason the invariant-2 spawn guard is.
///
/// The second end would not merely be redundant: `kills` was consumed by the reaper, so it would
/// carry the adapter's outcome and supersede the correct row last-write-wins by `lsn`. That is why
/// the refusal comes before anything is removed from the run-local maps.
#[test]
fn a_late_child_exited_does_not_append_a_second_turn_ended() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    harness.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));

    // Hold the window open: the end is emitted and never acknowledged.
    harness.unfsynced.insert(RecordKind::TurnEnded);
    harness.trace.clear();
    harness.drive(Input::Liveness {
        turn,
        liveness: Liveness::Gone,
    });
    assert_eq!(ends_of(&harness, turn).len(), 1, "the reaper wrote the end");
    assert_eq!(
        harness.supervisor.world().turns[&turn].status,
        TurnStatus::Spawned,
        "a control: the fold has not moved, so a world-only guard would refuse nothing"
    );

    // The adapter's report arrives after the fact.
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
        last_message: Some("done".into()),
    });

    assert_eq!(
        ends_of(&harness, turn),
        [(
            Provenance::Observed,
            ExitStatus::UNKNOWN,
            Some(TurnOutcome::Killed {
                by: KillReason::Halt
            })
        )],
        "one turn, one end, and the ledger's outcome was not overwritten"
    );
}

/// A `Spawned` that arrives after the turn's end is refused, so no `TurnSpawned`
/// is ever appended after that turn's `TurnEnded`.
///
/// The mirror of the test above, and the same guard read from the other side. Nothing in this
/// crate's contract orders `Input::Spawned` before `Input::ChildExited` — `Spawned` is "an
/// observation after the fact; nothing waits on it" — so the answer to a launch can arrive for a
/// turn that has already ended. Unrefused it writes a `TurnSpawned` *after* the `TurnEnded`, and
/// because the fold is last-write-wins by `lsn` the row goes backwards out of `Ended`, which puts
/// it back among the open children the reaper sweeps. The restart is what makes the cost a
/// measurement rather than a worry: the pid is long gone by then, so the second run ends the turn
/// a **second** time and as `Vanished`, over a turn that exited 0 and completed.
///
/// **Two arms, because the guard is a disjunction and each half owns one.** In the first the end
/// has folded and `TurnRow::has_ended` is what refuses. In the second it is emitted and
/// unacknowledged — the crash window `end_turn`'s own doc names — so the row still reads `Issued`
/// and only the run-local `ending` set knows the turn is over. A guard written as
/// `row.has_ended()` alone passes the first arm and fails the second.
///
/// The launch is held outstanding in both, which is what lets the answer be delivered by hand and
/// late: `Input::Spawned` carrying the key the host would have returned.
#[test]
fn a_late_spawned_does_not_reopen_an_ended_turn() {
    // ---- the end has folded --------------------------------------------------------------------
    let mut harness = Harness::new();
    // The launch is asked for and never answered, so the answer can arrive after the end.
    harness.launch = false;
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    assert_eq!(
        (
            harness.supervisor.world().turns[&turn].status,
            spawn_records(&harness, turn),
            ends_of(&harness, turn).len(),
        ),
        (TurnStatus::Issued, 0, 0),
        "a control: the turn is open, the launch is unanswered, and nothing has ended"
    );

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
        last_message: Some("done".into()),
    });
    assert_eq!(
        (
            harness.supervisor.world().turns[&turn].status,
            ends_of(&harness, turn).len(),
        ),
        (TurnStatus::Ended, 1),
        "a control: the end folded, so the row really has moved to `Ended`"
    );

    // The host answers the launch after the fact, for a pid the process table no longer holds.
    let key = harness.host.key_for(1000);
    harness.host.answer(key.pid, ProbeAnswer::NoSuchPid);
    harness.drive(Input::Spawned {
        turn,
        key,
        mcp_grant: None,
    });

    assert_eq!(
        harness.supervisor.world().turns[&turn].status,
        TurnStatus::Ended,
        "the row did not move backwards"
    );
    assert_eq!(
        spawn_records(&harness, turn),
        0,
        "and no `TurnSpawned` was appended after the `TurnEnded`"
    );

    harness.restart();

    assert_eq!(
        ends_of(&harness, turn).len(),
        1,
        "one turn, one end: the second run found no open child to sweep"
    );

    // ---- the end is emitted and unacknowledged -------------------------------------------------
    let mut harness = Harness::new();
    harness.launch = false;
    // The end never reaches the disk, so the fold leaves the row `Issued`.
    harness.unfsynced.insert(RecordKind::TurnEnded);
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

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
        last_message: Some("done".into()),
    });
    assert_eq!(
        ends_of(&harness, turn).len(),
        1,
        "a control: the end was written"
    );
    assert_eq!(
        harness.supervisor.world().turns[&turn].status,
        TurnStatus::Issued,
        "a control: the fold has not moved, so a `has_ended()`-only guard would refuse nothing here"
    );

    harness.drive(Input::Spawned {
        turn,
        key: harness.host.key_for(1000),
        mcp_grant: None,
    });

    assert_eq!(
        spawn_records(&harness, turn),
        0,
        "the end is decided before it is acknowledged, and the run-local set is what says so"
    );
    assert_eq!(ends_of(&harness, turn).len(), 1, "one turn, one end");
}

#[test]
fn late_unknown_does_not_recreate_an_ending_turn() {
    assert_late_liveness_is_inert(Liveness::Unknown);
}

#[test]
fn late_alive_does_not_recreate_an_ending_turn() {
    assert_late_liveness_is_inert(Liveness::Alive);
}

/// Once `ending` owns a turn, its accepted end has removed the run-local liveness and child state.
/// A probe answer already in flight may still arrive while the journal row says `Spawned`; neither
/// answer may recreate what the end removed. The doctor pair observes liveness state, while a
/// subsequent halt observes the child and deferral paths.
fn assert_late_liveness_is_inert(answer: Liveness) {
    let mut harness = Harness::new();
    harness.unfsynced.insert(RecordKind::TurnEnded);
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

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
    assert_eq!(
        harness.supervisor.world().turns[&turn].status,
        TurnStatus::Spawned,
        "control: the unacknowledged end leaves the journal row open"
    );

    harness.drive(Input::Liveness {
        turn,
        liveness: answer,
    });
    let health = harness.supervisor.key_health(&harness.host);
    let row = health
        .turns
        .iter()
        .find(|row| row.turn == turn)
        .expect("the unacknowledged end still projects its open row");
    assert_eq!(
        (row.liveness, row.unknown_for_ms),
        (Liveness::Unknown, None),
        "a late {answer:?} answer does not recreate liveness state"
    );

    harness.trace.clear();
    harness.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));
    assert!(
        !killed(&harness, turn),
        "a late {answer:?} answer does not recreate a child"
    );
    assert_eq!(
        deferrals(&harness, turn),
        0,
        "a late {answer:?} answer does not recreate an Unknown deferral"
    );
}

/// A refused end leaves the `deferred` map alone, so it does not reset the
/// once-per-standing-cause guard.
///
/// The cause is established before the first end and observed immediately after the second,
/// refused end. That placement keeps the assertion sensitive to cleanup moving above
/// `end_turn`'s refusal guard even though late liveness answers can no longer refill the map.
#[test]
fn a_refused_end_keeps_the_deferral_it_already_journaled() {
    let (mut harness, agent, turn) = a_run_whose_liveness_never_resolves(true);

    // Establish the once-per-cause guard before the end starts. A late liveness answer is inert
    // after that point, so no valid sequence can create the guard inside the ending window.
    for expected in [
        BreakerRung::Steer,
        BreakerRung::Constrain,
        BreakerRung::Stop,
    ] {
        harness.drive(Input::Signal {
            agent,
            signal: RuntimeSignal::StopCap,
        });
        harness.drive(Input::Tick);
        assert_eq!(rung(&harness, agent), expected);
    }
    assert_eq!(
        deferrals(&harness, turn),
        1,
        "a control: the cause was deferred once, so `deferred` holds this turn"
    );

    an_end_held_in_the_fsync_window(&mut harness, turn);

    // The adapter's report arrives for a turn whose end is already in flight: refused.
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
    assert_eq!(
        ends_of(&harness, turn).len(),
        1,
        "a control: the second report was refused"
    );
    assert_eq!(
        deferrals(&harness, turn),
        1,
        "the refused end left the existing once-per-cause guard untouched"
    );

    // Ten more beats keep sweeping the still-open journal row. Their late probe answers are inert,
    // and neither they nor the refused end change the established guard.
    for _ in 0..10 {
        harness.drive(Input::Signal {
            agent,
            signal: RuntimeSignal::StopCap,
        });
        harness.drive(Input::Tick);
    }
    assert_eq!(
        deferrals(&harness, turn),
        1,
        "the refused end left the deferral where it found it, so the retry appended nothing"
    );
}

/// A second run over an open child whose liveness no action on this host ever resolves, holding
/// every `TurnEnded` from here on in the fsync window.
///
/// The liveness table's diagonal: the recorded key names no boot identity and this host produces one, so
/// each probe answers `Unknown` for the rest of the run. `answer_probes` is the one dial the pair
/// of tests below differ on — `false` leaves the restart's own probe unanswered, so a halt raised
/// straight afterwards finds no liveness to defer against and the guard stays empty.
fn a_run_whose_liveness_never_resolves(answer_probes: bool) -> (Harness, AgentId, TurnId) {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    harness.host.host_boot = HostBoot::Read("boot-b".to_owned());
    harness.probe = answer_probes;
    harness.restart();
    harness.unfsynced.insert(RecordKind::TurnEnded);

    (harness, agent, turn)
}

/// The adapter reports the child's exit: the turn's first end, so it is accepted, and it is never
/// acknowledged.
fn an_end_held_in_the_fsync_window(harness: &mut Harness, turn: TurnId) {
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
    assert_eq!(
        ends_of(harness, turn).len(),
        1,
        "a control: the end was written — the first for this turn, so it was accepted"
    );
    assert_eq!(
        harness.supervisor.world().turns[&turn].status,
        TurnStatus::Spawned,
        "a control: the fold has not moved, so this is the window `ending` models"
    );
}

/// The reaper keeps probing a journal row whose end is unacknowledged, but the answers arrive too
/// late to recreate liveness or a deferral. The probe-count control distinguishes that case from a
/// turn which merely fell out of the sweep.
#[test]
fn late_probe_answers_do_not_defer_a_turn_whose_end_is_in_flight() {
    let (mut harness, agent, turn) = a_run_whose_liveness_never_resolves(false);

    harness.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));
    assert_eq!(
        deferrals(&harness, turn),
        0,
        "a control: nothing is deferred yet — the liveness is unresolved and the restart left no \
         child to signal"
    );

    an_end_held_in_the_fsync_window(&mut harness, turn);

    harness.probe = true;
    let before = probes(&harness, turn);
    harness.drive(Input::Tick);
    harness.drive(Input::Tick);

    assert!(
        probes(&harness, turn) >= before + 2,
        "control: both beats probed the still-open journal row"
    );

    assert_eq!(
        deferrals(&harness, turn),
        0,
        "the late Unknown answers did not recreate the liveness needed to defer"
    );
}

/// An end that is *accepted* and not yet acknowledged keeps the once-per-cause
/// guard, so a cause that never changed is journaled once across two beats.
///
/// The mirror image of `a_refused_end_keeps_the_deferral_it_already_journaled`, and the case it
/// cannot reach. That one is about the **second** end — refused, so `end_turn` returns above its
/// teardown and touches nothing. This one is about the **first**: it is accepted, the teardown
/// runs, and the guard remains until the fold confirms the end reached disk. Late liveness answers
/// are inert in that window, so the count can neither disappear nor grow.
#[test]
fn an_accepted_unacknowledged_end_keeps_the_deferral_guard() {
    let (mut harness, agent, turn) = a_run_whose_liveness_never_resolves(true);

    harness.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));
    assert_eq!(
        deferrals(&harness, turn),
        1,
        "a control: the standing halt is journaled as deferred, once, before any end"
    );

    an_end_held_in_the_fsync_window(&mut harness, turn);

    let before = probes(&harness, turn);
    harness.drive(Input::Tick);
    harness.drive(Input::Tick);

    assert!(
        probes(&harness, turn) >= before + 2,
        "a control: the reaper swept the still-open journal row on both beats"
    );
    assert!(
        harness.supervisor.world().agents[&agent].halted,
        "a control: the cause stood throughout"
    );
    assert!(
        !killed(&harness, turn),
        "a control: late answers did not recreate a child to signal"
    );
    assert_eq!(
        ends_of(&harness, turn).len(),
        1,
        "a control: and no `Gone` ended the turn either, so it stayed in the sweep"
    );

    assert_eq!(
        deferrals(&harness, turn),
        1,
        "the accepted-but-unacknowledged end left the once-per-cause guard standing"
    );
}

/// The drain the fold runs is pinned to the turn it folds — one agent's
/// acknowledged `TurnEnded` leaves another agent's once-per-cause guard standing.
///
/// **A scoping test for the drain, not a second regression test for the defect above.** It is
/// green on the tree before that fix, where the fold drained nothing at all, and it stays green
/// under the mutant that puts `deferred.remove` back into `end_turn`: this is the direction the
/// fold newly owns, and nothing single-turn can reach it. `self.deferred.clear()` in place of the
/// removal is the whole workspace suite's blind spot without it — every test above passes,
/// because with one turn in flight a drain of the map and a drain of the entry are the same
/// drain, and `the_deferral_guard_does_not_grow_for_the_life_of_the_daemon` is *more* satisfied
/// by `clear()` than by `remove`. What that mutant does with a second agent on the floor is the defect above
/// again — same rule, same record, one cause journaled twice for a
/// `(turn, cause)` pair that never changed — reached through the line the drain adds instead of the
/// line it removes. Delete this test when the drain goes, not when the defect above becomes
/// impossible.
#[test]
fn a_folded_end_drains_only_the_turn_it_folds() {
    let mut harness = Harness::new();
    let (ending_agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let ending_turn = harness.open_turn(ending_agent).expect("a turn was issued");
    let (standing_agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let standing_turn = harness
        .open_turn(standing_agent)
        .expect("a turn was issued");
    assert_ne!(
        ending_turn, standing_turn,
        "a control: two agents, each with a child of its own — a drain has something to be too \
         wide about"
    );

    // The liveness table's diagonal, for both children at once: the recorded keys name no boot identity
    // and this host produces one, so every probe from here answers `Unknown` for the rest of the
    // run and no action on this host resolves it.
    harness.host.host_boot = HostBoot::Read("boot-b".to_owned());
    harness.restart();

    for agent in [ending_agent, standing_agent] {
        harness.drive(Input::Command(Command::Halt {
            agent,
            text: Some("stop".into()),
        }));
    }
    assert_eq!(
        deferrals(&harness, ending_turn),
        1,
        "a control: the first standing halt is journaled as deferred, once"
    );
    assert_eq!(
        deferrals(&harness, standing_turn),
        1,
        "a control: and so is the second — two turns, two guards, one cause each"
    );

    // The one thing that separates the two turns: this end is acknowledged, so it folds, and the
    // fold is what drains. The other agent does nothing at all.
    harness.drive(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn: ending_turn,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: None,
    });
    assert_eq!(
        harness.supervisor.world().turns[&ending_turn].status,
        TurnStatus::Ended,
        "a control: that end reached the disk and folded, so the drain ran"
    );
    assert_eq!(
        harness.supervisor.world().turns[&standing_turn].status,
        TurnStatus::Spawned,
        "a control: the other turn is untouched — still an open child, still in the sweep"
    );

    let before = probes(&harness, standing_turn);
    harness.drive(Input::Tick);
    harness.drive(Input::Tick);

    assert!(
        probes(&harness, standing_turn) >= before + 2,
        "a control: the reaper swept the untouched turn on both beats, so `reap()` reached the \
         liveness it consults before retrying the kill"
    );
    assert!(
        harness.supervisor.world().agents[&standing_agent].halted,
        "a control: and its cause stood throughout"
    );

    assert_eq!(
        deferrals(&harness, standing_turn),
        1,
        "one turn's fold drained one turn's guard: the cause standing over the other turn never \
         changed, so it is journaled once"
    );
}

// ---- the restart consequences ------------------------------------------------------------------

/// **What closes the restart gap:** `Alive` adopts, which is the only thing that repopulates
/// the run-local child map after a restart.
///
/// Before the reaper, `self.children` was written only by `Input::Spawned`, so a standing cause
/// against a child belonging to a previous daemon run found nothing to signal and silently did
/// nothing. The assertion is the `Kill`, because adoption is not otherwise observable from
/// outside the machine.
#[test]
fn an_adopted_child_is_reachable_by_a_standing_cause() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    // The process table still holds the child: `FakeHost` outlives the restart, exactly as a real
    // process outlives the daemon that started it.
    harness.trace.clear();
    harness.restart();
    assert!(probed(&harness, turn), "the open child was asked about");

    harness.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));
    assert!(
        killed(&harness, turn),
        "a re-adopted child is signalled like any other"
    );
    assert!(
        deferrals(&harness, turn) == 0,
        "and nothing was deferred: the liveness resolved"
    );
}

/// **A restart consequence, pinned as accepted rather than left to be rediscovered as a defect.**
///
/// `self.kills` is run-local, so a turn killed before a crash and re-adopted afterwards ends
/// `Vanished` and not `Killed{Halt}`. This is the accepted imprecision — and the
/// `Halt` sitting in the journal immediately before is what explains the row to an operator.
///
/// The mutant a future reader would reach for is rebuilding `kills` from the `Halt` during the
/// fold; it turns this outcome into `Killed{Halt}` and this test red.
#[test]
fn a_turn_killed_before_a_crash_is_re_adopted_and_ends_vanished() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    harness.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));
    assert!(
        killed(&harness, turn),
        "a control: run 1 signalled the child"
    );

    // The crash: the daemon died before the child's exit was reported, and the child is gone.
    harness
        .host
        .answer(pid_of(&harness, turn), ProbeAnswer::NoSuchPid);
    harness.trace.clear();
    harness.restart();

    assert_eq!(
        ends_of(&harness, turn),
        [(
            Provenance::Observed,
            ExitStatus::UNKNOWN,
            Some(TurnOutcome::Vanished)
        )],
        "accepted imprecision: run 2's ledger never saw the kill"
    );
    assert!(
        harness
            .journal
            .iter()
            .any(|entry| matches!(entry.record, Record::Halt { .. })),
        "and the preceding Halt is what explains the row"
    );
}

#[test]
fn a_second_spawn_is_never_emitted_for_a_turn_whose_launch_is_outstanding() {
    let mut harness = Harness::new();
    // The launch is asked for and never answered: the guard's set never drains.
    harness.launch = false;
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    assert_eq!(
        harness.spawns_of(turn).len(),
        1,
        "a control: the live path asked once"
    );

    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));

    assert_eq!(
        harness.spawns_of(turn).len(),
        1,
        "one turn, one process (invariant 2)"
    );

    harness.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));
    harness.restart();
    assert_eq!(
        harness.spawns_of(turn).len(),
        1,
        "a control: the restart re-issued nothing, because the cause still stands"
    );

    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));

    assert_eq!(
        harness.spawns_of(turn).len(),
        1,
        "ordinary Resume retains the recovery barrier"
    );
    let opportunity = harness.supervisor.recovery_opportunity(agent).unwrap();
    assert_eq!(opportunity.expected.work.turn, turn);
    assert_eq!(
        opportunity.required.reason,
        dudes_proto::recovery::RecoveryReason::MissingSpawn
    );
    let report = harness.recover(dudes_supervisor::input::RecoveryCommand {
        request_id: dudes_proto::record::Hex32::new([196; 32]),
        digest: dudes_proto::record::Hex32::new([197; 32]),
        expected: opportunity.expected,
        choice: dudes_supervisor::input::RecoveryChoice::RepeatIssuedFresh,
        attribution: dudes_proto::recovery::RecoveryAttribution::LocalOperator {
            pid: 17,
            uid: None,
        },
    });
    assert_eq!(report.recovery().unwrap().refusal, None);
    assert_eq!(
        harness.spawns_of(turn).len(),
        2,
        "explicit consent releases one new run-local launch claim"
    );
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert_eq!(
        harness.spawns_of(turn).len(),
        2,
        "an outstanding authorized launch still cannot duplicate"
    );
    assert_eq!(
        harness
            .journal
            .iter()
            .filter(|entry| matches!(entry.record,
                Record::TurnIssued { agent: owner, .. } if owner == agent
            ))
            .count(),
        1
    );
}

/// **The spawn claim hands over only when `TurnSpawned` is acknowledged.**
///
/// A standing cause can be cleared on either side of that acknowledgement. In the first arm the
/// `Resume` persist is emitted before the host answers the outstanding launch, so acknowledging
/// it first is the caller's required emission order. The world still calls the turn `Issued` at
/// that instant, but the run-local claim must refuse crash rule (2)'s re-issue. In the second arm
/// `TurnSpawned` is emitted and acknowledged first; the fold then calls the turn `Spawned`, so a
/// later `Resume` has nothing to re-issue.
///
/// Removing the claim when `TurnSpawned` is *emitted* makes only the first arm red: its resume
/// emits a second `Spawn` for the same `TurnId`. Removing it in `apply_entry` after `World::apply`
/// keeps both arms green and makes the hand-over coincide with the durable fact.
#[test]
fn a_spawn_claim_survives_both_orderings_of_cause_clear_and_spawn_ack() {
    // Arm 1: Resume was emitted first, so it must be acknowledged before TurnSpawned.
    let mut clear_first = Harness::new();
    clear_first.launch = false;
    let (agent, _) = clear_first.hire_with_goal(&["npm", "test"]);
    let turn = clear_first
        .open_turn(agent)
        .expect("the launch is outstanding");
    assert_eq!(
        clear_first.spawns_of(turn).len(),
        1,
        "a control: the live path emitted the outstanding launch"
    );
    clear_first.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));
    clear_first.fsync = false;

    let resume = clear_first.step(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    let resume_token = persist_token(&resume, RecordKind::Resume);
    let key = clear_first.host.key_for(1000);
    let spawned = clear_first.step(Input::Spawned {
        turn,
        key,
        mcp_grant: None,
    });
    let spawned_token = persist_token(&spawned, RecordKind::TurnSpawned);
    assert!(
        resume_token < spawned_token,
        "a control: token order says Resume was emitted before TurnSpawned"
    );
    assert_eq!(
        clear_first.supervisor.world().turns[&turn].status,
        TurnStatus::Issued,
        "a control: before the spawn acknowledgement crash rule (2) can still see the turn"
    );

    let resume_ack = clear_first.step(clear_first.persisted(resume_token, Lsn(10_000)));
    assert!(
        resume_ack
            .iter()
            .all(|seen| !matches!(seen, Seen::Spawn { turn: spawned, .. } if *spawned == turn)),
        "the earlier cause clear cannot launch a second process while TurnSpawned is in flight"
    );
    clear_first.step(clear_first.persisted(spawned_token, Lsn(10_001)));
    assert_eq!(
        clear_first.supervisor.world().turns[&turn].status,
        TurnStatus::Spawned,
        "the later acknowledgement completes the hand-over to the fold"
    );

    // Arm 2: TurnSpawned was emitted first, so acknowledge it before emitting Resume.
    let mut ack_first = Harness::new();
    ack_first.launch = false;
    let (agent, _) = ack_first.hire_with_goal(&["npm", "test"]);
    let turn = ack_first
        .open_turn(agent)
        .expect("the launch is outstanding");
    ack_first.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));
    ack_first.fsync = false;

    let spawned = ack_first.step(Input::Spawned {
        turn,
        key: ack_first.host.key_for(1000),
        mcp_grant: None,
    });
    let spawned_token = persist_token(&spawned, RecordKind::TurnSpawned);
    ack_first.step(ack_first.persisted(spawned_token, Lsn(20_000)));
    let resume = ack_first.step(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    let resume_token = persist_token(&resume, RecordKind::Resume);
    assert!(
        spawned_token < resume_token,
        "a control: token order says TurnSpawned was emitted before Resume"
    );
    let resume_ack = ack_first.step(ack_first.persisted(resume_token, Lsn(20_001)));
    assert!(
        resume_ack
            .iter()
            .all(|seen| !matches!(seen, Seen::Spawn { turn: spawned, .. } if *spawned == turn)),
        "after the spawn acknowledgement the fold leaves no issued turn to re-issue"
    );
    assert_eq!(
        ack_first.supervisor.world().turns[&turn].status,
        TurnStatus::Spawned
    );
}

/// **The same re-issue, from the other side: the run-local teardown must not answer a launch
/// the host never answered.**
///
/// `end_turn` clears the run-local maps for a turn it is ending, and `spawning` is not one of
/// them. Nothing in the crate's contract says a `ChildExited` is preceded by a `Spawned` for the
/// same turn: `Input::Spawned` is documented as "an observation after the fact; nothing waits on
/// it", and `Input::ChildExited` states no precondition at all. So a turn whose launch is still
/// outstanding can reach `end_turn`, and a teardown that dropped it from `spawning` would hand the
/// next cleared cause a turn the world still calls `Issued` with invariant 2's run-local guard
/// already spent — a second vendor process for one `TurnId`, while the first may still be
/// starting.
///
/// `spawning`'s own doc commits to the other shape and this pins it: an unanswered launch stays in
/// the set for the rest of the run, no cleared cause re-issues it, and the next start clears the
/// set and issues it once. Stuck-but-visible is what is chosen over a second process.
///
/// The two controls are what make the assertion mean anything: the live path asked exactly once,
/// and the end is still inside the fsync window, so the fold has not moved the row out of
/// `issued_not_spawned()` and the re-issue really is reachable.
#[test]
fn an_outstanding_launch_is_not_cleared_by_a_child_exited_that_never_saw_it() {
    let mut harness = Harness::new();
    // The launch is asked for and never answered: the guard's set never drains.
    harness.launch = false;
    // And the end never reaches the disk, so the fold leaves the row `Issued`.
    harness.unfsynced.insert(RecordKind::TurnEnded);
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    assert_eq!(
        harness.spawns_of(turn).len(),
        1,
        "a control: the live path asked once"
    );

    // The adapter reports an exit for a turn this run never saw `Input::Spawned` for.
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
    assert_eq!(
        harness.supervisor.world().turns[&turn].status,
        TurnStatus::Issued,
        "a control: the end is still unacknowledged, so the turn is still re-issuable"
    );

    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));

    assert_eq!(
        harness.spawns_of(turn).len(),
        1,
        "one turn, one process (invariant 2)"
    );
}

// ---- the probe filter, both halves ------------------------------------------------------------

/// A live child of *this* run is never probed.
///
/// The left half of `reap()`'s filter, and a rule rather than an optimisation: the runtime is this
/// child's parent and will deliver `Input::ChildExited`, so a `Gone` racing an undelivered exit
/// would be a second end for one turn, which is forbidden. The set the reaper sweeps is
/// therefore *not* "every open child" — it is every open child this run does not hold, plus the
/// ones it has already answered about, which is the other half and the test below this one.
///
/// The rule was defended only by accident before this: replacing the whole filter with `true`
/// reddened one test, a kill-count assertion in another file that never mentions probing, and an
/// edit to that test would have left the rule unguarded with nothing to say so.
///
/// The control is the same row swept by a run that does *not* hold the child — a restart with the
/// probe left unanswered, so nothing adopts — which asks. So the silence above is the filter
/// refusing, not a reaper that never ran or a row it cannot see.
#[test]
fn a_live_child_of_this_run_is_never_probed() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    assert_eq!(
        harness.supervisor.world().turns[&turn].status,
        TurnStatus::Spawned,
        "a control: the row is an open child, so it is in the set `reap` filters"
    );

    harness.trace.clear();
    harness.drive(Input::Tick);

    assert!(
        !probed(&harness, turn),
        "the runtime is this child's parent and owes us its exit: asking risks a second end"
    );

    // The control: the same row, swept by a run that holds no child for it. The answer is never
    // delivered, so nothing adopts and the run stays in the state the test is about.
    harness.probe = false;
    harness.restart();
    harness.trace.clear();
    harness.drive(Input::Tick);

    assert!(
        probed(&harness, turn),
        "a control: a run that does not hold the child does ask about it"
    );
}

/// The reaper asks again about a child it has already answered about.
///
/// The right half of the same filter, and the reason it is a disjunction: once `Alive` adopts, the
/// turn *is* in this run's child map, so the left half above would exclude it from then on — and
/// an adopted child is precisely one whose exit no runtime will deliver, because this run is not
/// its parent. Dropping this half makes every re-adopted child unwatched for the rest of the run:
/// probed once at the crash rules, never asked about again, and never ended.
///
/// The two controls are what make the last assertion a re-probe rather than a first one: the crash
/// rules asked, and the answer adopted the child — which the halt reaching it is the only outside
/// evidence of, since the child map is not observable from outside the machine.
#[test]
fn the_reaper_asks_again_about_the_child_it_adopted() {
    let mut harness = Harness::new();
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    // A second run: it holds no child for this turn, so the crash rules ask, and the pid still
    // answers with the start value the fake stamped at spawn.
    harness.restart();
    assert!(
        probed(&harness, turn),
        "a control: the crash rules asked about a child this run does not hold"
    );
    assert!(
        ends_of(&harness, turn).is_empty(),
        "a control: the answer was not `Gone`, so the turn is still open"
    );
    harness.drive(Input::Command(Command::Halt {
        agent,
        text: Some("stop".into()),
    }));
    assert!(
        killed(&harness, turn),
        "a control: the answer was `Alive` and adopted the child, which is what the halt reached"
    );

    harness.trace.clear();
    harness.drive(Input::Tick);

    assert!(
        probed(&harness, turn),
        "the reaper owns this child now, so it keeps asking: nothing else will tell it the exit"
    );
}

// ---- invariant 3, over sequences ----------------------------------------------------------------

/// One step a generated run takes.
#[derive(Clone, Debug)]
enum Op {
    /// The beat.
    Tick,
    /// A daemon restart: a fresh machine over the entries that reached the disk.
    Restart,
    /// `loop.halt`.
    Halt,
    /// `loop.resume`.
    Resume,
    /// One runtime breaker signal, by index into [`RuntimeSignal::ALL`].
    Signal(usize),
    /// The adapter reports the child's exit; `true` is a clean one.
    ChildExited(bool),
    /// The goal's predicate answers.
    PredicateDone(bool),
    /// Open or close the fsync crash window.
    Fsync(bool),
    /// Open or close the launch crash window.
    Launch(bool),
    /// What the process table says about every recorded pid, by index into [`PROBE_ANSWERS`].
    Answer(usize),
    /// Whether this host produces a boot identity — the other axis of the liveness table.
    HostBoot(bool),
}

/// The four process-table answers, chosen so every cell of the liveness table is reachable: a start value
/// that matches what `FakeHost` stamps at spawn, one that does not, an absent pid, and a host that
/// could not answer.
const PROBE_ANSWERS: [ProbeAnswer; 4] = [
    ProbeAnswer::NoSuchPid,
    ProbeAnswer::StartValue(900),
    ProbeAnswer::StartValue(901),
    ProbeAnswer::CouldNotAnswer,
];

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::Tick),
        Just(Op::Restart),
        Just(Op::Halt),
        Just(Op::Resume),
        (0usize..4).prop_map(Op::Signal),
        any::<bool>().prop_map(Op::ChildExited),
        any::<bool>().prop_map(Op::PredicateDone),
        any::<bool>().prop_map(Op::Fsync),
        any::<bool>().prop_map(Op::Launch),
        (0usize..4).prop_map(Op::Answer),
        any::<bool>().prop_map(Op::HostBoot),
    ]
}

/// One value of every `Op` variant: what the control below compares the generator's output
/// against. The `variant` match keeps it honest — a variant added to `Op` fails to compile there.
const EVERY_STEP: [Op; 11] = [
    Op::Tick,
    Op::Restart,
    Op::Halt,
    Op::Resume,
    Op::Signal(0),
    Op::ChildExited(true),
    Op::PredicateDone(true),
    Op::Fsync(true),
    Op::Launch(true),
    Op::Answer(0),
    Op::HostBoot(true),
];

/// How many values the control draws from `op()`. The generator is uniform over eleven variants,
/// two of which carry a four-way index, so the rarest value asked for below arrives with
/// probability 1/44 and 2,000 draws miss it with probability under 1e-19. The runner is seeded
/// deterministically, so the draw this file makes is the same one every run.
const DRAWS: usize = 2_000;

/// Which variant this step is, payload ignored.
fn variant(op: &Op) -> &'static str {
    match op {
        Op::Tick => "Tick",
        Op::Restart => "Restart",
        Op::Halt => "Halt",
        Op::Resume => "Resume",
        Op::Signal(_) => "Signal",
        Op::ChildExited(_) => "ChildExited",
        Op::PredicateDone(_) => "PredicateDone",
        Op::Fsync(_) => "Fsync",
        Op::Launch(_) => "Launch",
        Op::Answer(_) => "Answer",
        Op::HostBoot(_) => "HostBoot",
    }
}

/// `DRAWS` values from [`op()`], the generator's own output.
fn drawn() -> Vec<Op> {
    let mut runner = TestRunner::deterministic();
    (0..DRAWS)
        .map(|_| {
            op().new_tree(&mut runner)
                .expect("the strategy produced a value")
                .current()
        })
        .collect()
}

/// The first drawn step `want` accepts, or a failure naming what the generator no longer reaches.
///
/// This is what makes the control a control **over `op()`**: the steps it drives are values the
/// strategy produced, so a variant dropped from `prop_oneof!` cannot reach the script either.
/// Naming `Op` values directly shares the driver with the property and nothing else — the
/// strategy could go vacuous underneath and the control would not notice.
fn step<'a>(drawn: &'a [Op], want: fn(&Op) -> bool, name: &str) -> &'a Op {
    drawn
        .iter()
        .find(|op| want(op))
        .unwrap_or_else(|| panic!("op() produced no {name} in {DRAWS} draws"))
}

/// Take one step. Shared by the property and by its positive control, so the control really does
/// exercise the generator's own driver.
fn apply(
    harness: &mut Harness,
    children: &mut BTreeSet<TurnId>,
    agent: AgentId,
    goal: GoalId,
    op: &Op,
) {
    let trace_start = harness.trace.len();
    match op {
        Op::Tick => harness.drive(Input::Tick),
        Op::Restart => {
            children.clear();
            harness.restart();
        }
        Op::Halt => harness.drive(Input::Command(Command::Halt { agent, text: None })),
        Op::Resume => harness.drive(Input::Command(Command::Resume {
            agent,
            browser: None,
        })),
        Op::Signal(index) => harness.drive(Input::Signal {
            agent,
            signal: RuntimeSignal::ALL[*index],
        }),
        Op::ChildExited(clean) => {
            if let Some(turn) = children.pop_first() {
                harness.drive(Input::ChildExited {
                    final_reply: None,
                    elapsed_ms: None,
                    turn,
                    exit: ExitStatus {
                        code: Some(i32::from(!*clean)),
                        signal: None,
                    },
                    usage: None,
                    model_usage: BTreeMap::new(),
                    outcome: if *clean {
                        TurnOutcome::Completed
                    } else {
                        TurnOutcome::Failed {
                            reason: "the vendor said so".into(),
                        }
                    },
                    last_message: None,
                });
            }
        }
        Op::PredicateDone(pass) => harness.drive(Input::PredicateDone {
            goal,
            pass: *pass,
            evidence: evidence(),
        }),
        Op::Fsync(on) => harness.fsync = *on,
        Op::Launch(on) => harness.launch = *on,
        Op::Answer(index) => {
            let answer = PROBE_ANSWERS[*index];
            let pids: Vec<u32> = harness
                .supervisor
                .world()
                .turns
                .values()
                .filter_map(|row| row.pid)
                .collect();
            for pid in pids {
                harness.host.answer(pid, answer);
            }
        }
        Op::HostBoot(present) => {
            // The axis is *presence*, so absence is spelled `NotApplicable` rather than
            // `Unreadable`: both produce no identity and answer this axis identically, and only
            // the first leaves the fake able to spawn — an `Unreadable` host refuses, as the real
            // one does, which would take a state out of this generator's reach rather than adding
            // one to it.
            harness.host.host_boot = if *present {
                HostBoot::Read("boot-b".to_owned())
            } else {
                HostBoot::NotApplicable
            };
        }
    }
    children.extend(spawned_turns(&harness.trace[trace_start..]));
}

fn spawned_turns(trace: &[Seen]) -> impl Iterator<Item = TurnId> + '_ {
    trace
        .iter()
        .filter_map(Seen::record)
        .filter_map(|record| match record {
            Record::TurnSpawned { turn, .. } => Some(*turn),
            _ => None,
        })
}

#[test]
fn the_reaper_driver_exits_an_owned_child_after_launch_is_disabled() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    let mut children = spawned_turns(&harness.trace).collect();
    assert_eq!(harness.host.spawned_pids, [1000]);

    apply(&mut harness, &mut children, agent, goal, &Op::Launch(false));
    apply(
        &mut harness,
        &mut children,
        agent,
        goal,
        &Op::ChildExited(true),
    );

    assert_eq!(
        ends_of(&harness, turn),
        [(
            Provenance::Observed,
            ExitStatus {
                code: Some(0),
                signal: None
            },
            Some(TurnOutcome::Completed),
        )]
    );
    assert!(children.is_empty(), "the stream exit was consumed");
    apply(
        &mut harness,
        &mut children,
        agent,
        goal,
        &Op::ChildExited(false),
    );
    assert_eq!(ends_of(&harness, turn).len(), 1);
}

#[test]
fn the_reaper_driver_does_not_exit_an_unspawned_issue() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let first = harness.open_turn(agent).expect("a turn was issued");
    let mut children = spawned_turns(&harness.trace).collect();
    for op in [
        Op::ChildExited(false),
        Op::Launch(false),
        Op::Resume,
        Op::PredicateDone(false),
        Op::Restart,
        Op::Tick,
        Op::Tick,
    ] {
        apply(&mut harness, &mut children, agent, goal, &op);
    }
    let blocked = harness
        .open_turn(agent)
        .expect("the successor is still issued");
    assert_ne!(blocked, first);
    assert_eq!(harness.host.spawned_pids, [1000]);
    assert_eq!(
        harness.supervisor.world().turns[&blocked].status,
        TurnStatus::Issued
    );
    assert_eq!(spawn_records(&harness, blocked), 0);
    assert!(children.is_empty());

    apply(
        &mut harness,
        &mut children,
        agent,
        goal,
        &Op::ChildExited(false),
    );

    assert!(ends_of(&harness, blocked).is_empty());
    assert_eq!(harness.open_turn(agent), Some(blocked));
    assert_eq!(ends_of(&harness, first).len(), 1);
}

#[test]
fn the_reaper_driver_exits_a_child_before_its_spawn_is_acknowledged() {
    let mut harness = Harness::new();
    harness.unfsynced.insert(RecordKind::TurnSpawned);
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    let mut children = spawned_turns(&harness.trace).collect();
    assert_eq!(harness.host.spawned_pids, [1000]);
    assert_eq!(spawn_records(&harness, turn), 1);
    assert_eq!(
        harness.supervisor.world().turns[&turn].status,
        TurnStatus::Issued
    );
    assert!(
        harness
            .journal
            .iter()
            .all(|entry| !matches!(entry.record, Record::TurnSpawned { .. }))
    );

    apply(
        &mut harness,
        &mut children,
        agent,
        goal,
        &Op::ChildExited(true),
    );

    assert_eq!(
        ends_of(&harness, turn),
        [(
            Provenance::Observed,
            ExitStatus {
                code: Some(0),
                signal: None
            },
            Some(TurnOutcome::Completed),
        )]
    );
    assert!(children.is_empty());
}

#[test]
fn the_reaper_driver_reaps_an_adopted_child_through_gone() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");
    let mut children = spawned_turns(&harness.trace).collect();
    apply(&mut harness, &mut children, agent, goal, &Op::Restart);
    assert!(probed(&harness, turn));
    assert!(children.is_empty());

    apply(
        &mut harness,
        &mut children,
        agent,
        goal,
        &Op::ChildExited(true),
    );
    assert!(ends_of(&harness, turn).is_empty());
    apply(&mut harness, &mut children, agent, goal, &Op::Answer(0));
    apply(&mut harness, &mut children, agent, goal, &Op::Tick);

    assert_eq!(
        ends_of(&harness, turn),
        [(
            Provenance::Observed,
            ExitStatus::UNKNOWN,
            Some(TurnOutcome::Vanished)
        )]
    );
    assert_eq!(probes(&harness, turn), 2);
    assert_eq!(harness.host.spawned_pids, [1000]);
}

/// **The positive control for the property below.** A property over generated sequences can hold
/// vacuously — because no probe was ever emitted, because every answer was the default
/// `CouldNotAnswer`, or because no turn was ever issued — and the generator would look identical
/// from the outside either way.
///
/// So the same `apply` driver is run over three hand-written scripts, one per liveness answer, and
/// each asserts the machine's observable response to that cell. If the driver stops reaching one,
/// this fails by name rather than the property quietly going green over nothing.
///
/// **And the scripts are built from the generator's own draws, not from named `Op` values.** A
/// control that shares only the driver cannot see the generator go vacuous: delete a variant from
/// `op()` and every script still runs, because it never asked the strategy for anything. Every
/// step below comes out of [`drawn`], so a variant this control drives and the strategy no longer
/// produces fails here by name. The variants no script drives are covered by
/// `the_reaper_strategy_produces_every_step`, which asks the same question of the whole space.
#[test]
fn the_reaper_generator_reaches_every_liveness() {
    let drawn = drawn();

    // Gone: a pid the process table no longer holds.
    let mut gone = Harness::new();
    let (agent, goal) = gone.hire_with_goal(&["npm", "test"]);
    let mut children = spawned_turns(&gone.trace).collect();
    let turn = gone.open_turn(agent).expect("a turn was issued");
    apply(
        &mut gone,
        &mut children,
        agent,
        goal,
        step(&drawn, |op| matches!(op, Op::Answer(0)), "Op::Answer(0)"),
    );
    gone.trace.clear();
    apply(
        &mut gone,
        &mut children,
        agent,
        goal,
        step(&drawn, |op| matches!(op, Op::Restart), "Op::Restart"),
    );
    assert!(probed(&gone, turn), "Gone: the probe was emitted");
    assert_eq!(
        ends_of(&gone, turn).first().and_then(|end| end.2.clone()),
        Some(TurnOutcome::Vanished),
        "Gone: and the answer ended the turn"
    );

    // Alive: the start value the fake stamped at spawn, which is what it answers by default.
    let mut alive = Harness::new();
    let (agent, goal) = alive.hire_with_goal(&["npm", "test"]);
    let mut children = spawned_turns(&alive.trace).collect();
    let turn = alive.open_turn(agent).expect("a turn was issued");
    apply(
        &mut alive,
        &mut children,
        agent,
        goal,
        step(&drawn, |op| matches!(op, Op::Answer(1)), "Op::Answer(1)"),
    );
    alive.trace.clear();
    apply(
        &mut alive,
        &mut children,
        agent,
        goal,
        step(&drawn, |op| matches!(op, Op::Restart), "Op::Restart"),
    );
    assert!(probed(&alive, turn), "Alive: the probe was emitted");
    assert!(
        ends_of(&alive, turn).is_empty(),
        "Alive: and nothing was ended"
    );
    apply(
        &mut alive,
        &mut children,
        agent,
        goal,
        step(&drawn, |op| matches!(op, Op::Halt), "Op::Halt"),
    );
    assert!(killed(&alive, turn), "Alive: the child was adopted");

    // Unknown: the key names no boot identity and this host produces one.
    let mut unknown = Harness::new();
    let (agent, goal) = unknown.hire_with_goal(&["npm", "test"]);
    let mut children = spawned_turns(&unknown.trace).collect();
    let turn = unknown.open_turn(agent).expect("a turn was issued");
    apply(
        &mut unknown,
        &mut children,
        agent,
        goal,
        step(
            &drawn,
            |op| matches!(op, Op::HostBoot(true)),
            "Op::HostBoot(true)",
        ),
    );
    unknown.trace.clear();
    apply(
        &mut unknown,
        &mut children,
        agent,
        goal,
        step(&drawn, |op| matches!(op, Op::Restart), "Op::Restart"),
    );
    assert!(probed(&unknown, turn), "Unknown: the probe was emitted");
    assert!(
        ends_of(&unknown, turn).is_empty(),
        "Unknown: and nothing was written"
    );
    apply(
        &mut unknown,
        &mut children,
        agent,
        goal,
        step(&drawn, |op| matches!(op, Op::Halt), "Op::Halt"),
    );
    assert_eq!(
        deferrals(&unknown, turn),
        1,
        "Unknown: the kill was deferred"
    );
}

/// `op()` still draws every variant of `Op`, so the property below explores the
/// space its doc claims and not a space that has quietly shrunk.
///
/// Separate from the control above because it is a separate rule that fails for a separate
/// reason. That control's scripts ask [`step`] for the four variants they drive — `Answer`,
/// `Restart`, `Halt`, `HostBoot` — and one of those leaving `prop_oneof!` fails there by name.
/// The other seven are guarded here and nowhere else.
///
/// `EVERY_STEP` is what the draw is compared against, and the `variant` match is what keeps that
/// array honest: a variant added to `Op` does not compile until it is named there.
#[test]
fn the_reaper_strategy_produces_every_step() {
    assert_eq!(
        drawn().iter().map(variant).collect::<BTreeSet<_>>(),
        EVERY_STEP.iter().map(variant).collect::<BTreeSet<_>>(),
        "op() no longer produces every step the property is meant to explore"
    );
}

proptest! {
    /// No input sequence makes this machine persist an `Inferred` turn end.
    ///
    /// The liveness dimension enters through `FakeHost` — the process table and the boot identity
    /// — rather than by injecting `Input::Liveness`, so the property is about a reaper that was
    /// actually reached. `the_reaper_generator_reaches_every_liveness` is the control that says it
    /// was.
    ///
    /// The second half is the sequenced form of what `breaker.rs` asserts once: a journaled beat
    /// naming `InferredTurnEnd` would be gate 2's `Hold` made permanent, and no v1 emitter writes
    /// one.
    #[test]
    fn supervisor_never_persists_an_inferred_turn_end(
        ops in prop::collection::vec(op(), 1..=24)
    ) {
        let mut harness = Harness::new();
        let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
        let mut children = spawned_turns(&harness.trace).collect();
        for step in &ops {
            apply(&mut harness, &mut children, agent, goal, step);
        }
        for record in harness.records() {
            match record {
                Record::TurnEnded { provenance, .. } => prop_assert_eq!(
                    *provenance,
                    Provenance::Observed,
                    "an Inferred turn end reached the journal: {:?}",
                    ops
                ),
                Record::BreakerBeat { signals, .. } => prop_assert!(
                    !signals.contains(&BreakerSignal::InferredTurnEnd),
                    "a beat named a signal with no emitter: {:?}",
                    ops
                ),
                _ => {}
            }
        }
    }
}
