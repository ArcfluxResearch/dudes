//! only confirmed host absence ends a launch; retained uncertainty stays claimed.

use std::collections::BTreeSet;
use std::io;

use dudes_proto::goal::{Evidence, Predicate};
use dudes_proto::record::Hex32;
use dudes_proto::recovery::RecoveryAttribution;
use dudes_proto::{
    AutomaticHold, ExhaustedReason, GoalBudget, HoldReason, LoopDecision, Record, TurnOutcome,
};
use dudes_supervisor::input::{RecoveryChoice, RecoveryCommand};
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{Command, Input};
use dudes_types::{AgentId, AgentStatus, Mode, Provenance, Tokens, Usd};

fn failed_predicate() -> Evidence {
    Evidence::Command {
        argv: vec!["npm".into(), "test".into()],
        program: Some("/usr/bin/npm".into()),
        exit: Some(1),
        output_tail: "failed".into(),
    }
}

fn issued_turn(harness: &Harness) -> dudes_types::TurnId {
    harness
        .records()
        .find_map(|record| match record {
            Record::TurnIssued { turn, .. } => Some(*turn),
            _ => None,
        })
        .expect("a turn was issued")
}

fn assert_launch_failed_hold(harness: &Harness, agent: AgentId) {
    assert!(matches!(
        harness.supervisor.world().agents[&agent].automatic_hold,
        Some(AutomaticHold::LaunchFailed { .. })
    ));
    assert_eq!(
        harness
            .records()
            .filter_map(|record| match record {
                Record::LoopDecided { decision, .. } => Some(decision),
                _ => None,
            })
            .last(),
        Some(&LoopDecision::Hold {
            reason: HoldReason::LaunchFailed
        })
    );
    assert!(
        harness
            .records()
            .all(|record| !matches!(record, Record::TurnSpawned { .. }))
    );
}

fn issued_count(harness: &Harness) -> usize {
    harness
        .records()
        .filter(|record| matches!(record, Record::TurnIssued { .. }))
        .count()
}

/// Confirmed absence ends once and releases its launch claim. The visible hold admits
/// another turn only after Resume. A successful spawn writes no no-child end.
#[test]
fn confirmed_not_launched_ends_once_and_spends_one_hop() {
    let mut harness = Harness::new();
    harness.host.fail_spawn = Some(io::ErrorKind::PermissionDenied);
    let (agent, goal) = harness.hire_with_goal(&["npm", "test"]);
    let turn = issued_turn(&harness);

    let ends: Vec<&Record> = harness
        .records()
        .filter(|record| matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn))
        .collect();
    assert_eq!(ends.len(), 1, "one host answer produces one durable end");
    let Record::TurnEnded {
        mode,
        provenance,
        exit,
        usage,
        model_usage,
        last_message,
        outcome,
        ..
    } = ends[0]
    else {
        unreachable!()
    };
    assert_eq!(*mode, Mode::Managed);
    assert_eq!(*provenance, Provenance::Observed);
    assert_eq!(*exit, dudes_proto::ExitStatus::UNKNOWN);
    assert_eq!(*usage, None);
    assert!(model_usage.is_empty());
    assert_eq!(*last_message, None);
    assert!(matches!(
        outcome,
        Some(TurnOutcome::NotLaunched { reason })
            if reason == "FakeHost was told to fail this spawn"
    ));
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Idle);
    assert!(!harness.is_spawning(turn));

    harness.drive(Input::SpawnFailed {
        turn,
        reason: "duplicate".into(),
    });
    assert_eq!(
        harness
            .records()
            .filter(
                |record| matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn)
            )
            .count(),
        1,
        "the existing one-end guard rejects the duplicate answer"
    );

    harness.drive(Input::PredicateDone {
        goal,
        pass: false,
        evidence: failed_predicate(),
    });
    assert_eq!(
        issued_count(&harness),
        1,
        "a predicate result does not retry a failed launch"
    );
    assert_launch_failed_hold(&harness, agent);
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert_eq!(issued_count(&harness), 2);
    assert!(
        harness.records().any(
            |record| matches!(record, Record::TurnSpawned { turn: next, .. } if *next != turn)
        )
    );
    assert_eq!(
        harness
            .records()
            .filter(
                |record| matches!(record, Record::TurnEnded { turn: ended, .. } if *ended == turn)
            )
            .count(),
        1
    );

    let mut success = Harness::new();
    let (successful_agent, _) = success.hire_with_goal(&["npm", "test"]);
    let successful_turn = success.open_turn(successful_agent).expect("spawned turn");
    assert!(success.records().all(|record| !matches!(
        record,
        Record::TurnEnded {
            outcome: Some(TurnOutcome::NotLaunched { .. }),
            ..
        }
    )));
    assert_eq!(success.spawns_of(successful_turn).len(), 1);
}

/// The supervisor half of uncertainty: no follow-up input retains the current run's launch claim
/// and leaves only the issued prefix for the next supervisor. The joined real-host/production
/// runtime check lives in `bins/duded/tests/spawn_result.rs`.
#[test]
fn an_unanswered_launch_retains_the_supervisor_claim() {
    let mut harness = Harness::new();
    // The host/runtime mapping is exercised at the daemon seam. Here, withholding the follow-up
    // input isolates the supervisor state that `CleanupUnconfirmed` must preserve.
    harness.launch = false;
    let (agent, _) = harness.hire_with_goal(&["npm", "test"]);
    let turn = harness
        .open_turn(agent)
        .expect("the issued turn remains open");

    assert_eq!(harness.spawns_of(turn).len(), 1);
    assert!(harness.is_spawning(turn));
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Running);
    assert!(harness.records().all(|record| !matches!(
        record,
        Record::TurnEnded { turn: ended, .. } if *ended == turn
    )));

    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert_eq!(
        harness.spawns_of(turn).len(),
        1,
        "the retained spawning claim prevents a second launch in this run"
    );

    harness.trace.clear();
    let recovery_from = harness.trace.len();
    harness.restart();
    assert!(
        harness.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "restart alone does not repeat interrupted goal work"
    );
    harness.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert!(
        harness.trace[recovery_from..]
            .iter()
            .all(|seen| !matches!(seen, dudes_supervisor::testing::Seen::Spawn { .. })),
        "plain Resume does not supply recovery consent"
    );
    let expected = harness
        .supervisor
        .recovery_opportunity(agent)
        .unwrap()
        .expected;
    let handled = harness.recover(RecoveryCommand {
        request_id: Hex32::new([41; 32]),
        digest: Hex32::new([42; 32]),
        expected,
        choice: RecoveryChoice::RepeatIssuedFresh,
        attribution: RecoveryAttribution::LocalOperator { pid: 17, uid: None },
    });
    assert_eq!(handled.recovery().expect("command progress").refusal, None);
    assert!(harness.journal.iter().any(|entry| matches!(&entry.record,
        Record::RecoveryAuthorized(authorization) if authorization.request_id == Hex32::new([41; 32]))));

    assert_eq!(
        harness.spawns_of(turn).len(),
        1,
        "after restart, explicit consent repeats the original issued prefix"
    );
}

/// A failed launch holds before the hop cap. Explicit retries spend one hop each,
/// and Resume at the cap reaches Exhausted without a fourth launch.
#[test]
fn confirmed_launch_failures_hold_until_explicit_retries_reach_the_hop_cap() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    harness.host.fail_spawn = Some(io::ErrorKind::Other);
    harness.drive(Input::Command(Command::GoalSet {
        completion_check: None,
        agent,
        text: "make the tests pass".into(),
        predicate: Predicate::Command {
            argv: vec!["npm".into(), "test".into()],
            timeout_s: 60,
        },
        budget: GoalBudget {
            usd: Some(Usd::from_dollars(5)),
            tokens: Some(Tokens::new(10_000)),
        },
        hop_cap: 3,
        browser_goal: None,
        goal_replacement: None,
        baseline: None,
    }));
    let goal = harness.supervisor.world().agents[&agent]
        .goal
        .expect("goal");

    for issued in 1..=3 {
        harness.drive(Input::PredicateDone {
            goal,
            pass: false,
            evidence: failed_predicate(),
        });
        harness.drive(Input::Tick);
        assert_eq!(
            issued_count(&harness),
            issued,
            "the hold prevents an automatic retry"
        );
        assert_launch_failed_hold(&harness, agent);
        if issued < 3 {
            harness.host.fail_spawn = Some(io::ErrorKind::Other);
        }
        harness.drive(Input::Command(Command::Resume {
            agent,
            browser: None,
        }));
    }

    assert_eq!(
        harness
            .records()
            .filter(|record| matches!(
                record,
                Record::TurnEnded {
                    outcome: Some(TurnOutcome::NotLaunched { .. }),
                    ..
                }
            ))
            .count(),
        3
    );
    let ended: BTreeSet<_> = harness
        .records()
        .filter_map(|record| match record {
            Record::TurnEnded {
                turn,
                outcome: Some(TurnOutcome::NotLaunched { .. }),
                ..
            } => Some(*turn),
            _ => None,
        })
        .collect();
    assert_eq!(
        ended.len(),
        3,
        "each of the three issued turns ends exactly once"
    );
    assert_eq!(issued_count(&harness), 3);
    assert!(
        harness
            .records()
            .all(|record| !matches!(record, Record::TurnSpawned { .. }))
    );
    assert!(harness.records().any(|record| matches!(
        record,
        Record::LoopDecided {
            decision: LoopDecision::Exhausted {
                reason: ExhaustedReason::Hops
            },
            ..
        }
    )));
    assert_eq!(harness.supervisor.status_of(agent), AgentStatus::Exhausted);
    assert_eq!(
        harness
            .trace
            .iter()
            .filter(|seen| matches!(seen, Seen::Spawn { .. }))
            .count(),
        3
    );
}
