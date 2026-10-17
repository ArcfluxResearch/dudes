//! Durable automatic holds at the supervisor input boundary.

use std::collections::BTreeMap;

use dudes_proto::{
    AutomaticHold, Evidence, ExitStatus, GoalBudget, HoldReason, LoopDecision, Predicate, Record,
    RecordKind, TurnOutcome,
};
use dudes_providers::stream::ScopedUsage;
use dudes_supervisor::testing::{Harness, Seen};
use dudes_supervisor::{Command, Input};
use dudes_types::{AgentId, BreakerRung, GoalId, Provenance, Tokens, TurnId, Usage, Usd};

fn latest_decision(h: &Harness, agent: AgentId) -> &LoopDecision {
    h.records()
        .filter_map(|r| match r {
            Record::LoopDecided {
                agent: a, decision, ..
            } if *a == agent => Some(decision),
            _ => None,
        })
        .last()
        .expect("a decision")
}

fn end_without_usage(h: &mut Harness, agent: AgentId) {
    h.drive(Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn: h.open_turn(agent).expect("a running turn"),
        exit: ExitStatus {
            code: Some(1),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::NoResult,
        last_message: None,
    });
}

#[test]
fn replay_of_two_fast_failed_turns_holds_before_a_third_turn() {
    let mut h = Harness::new();
    let (agent, goal) = h.hire_with_goal(&["false"]);
    end_without_usage(&mut h, agent);
    h.drive(Input::PredicateDone {
        goal,
        pass: false,
        evidence: Evidence::Command {
            argv: vec!["false".into()],
            program: Some("/usr/bin/false".into()),
            exit: Some(1),
            output_tail: String::new(),
        },
    });
    end_without_usage(&mut h, agent);
    let last_end = h
        .journal
        .iter()
        .rposition(|e| matches!(e.record, Record::TurnEnded { .. }))
        .unwrap();
    h.journal.truncate(last_end + 1);
    for entry in &mut h.journal {
        if matches!(entry.record, Record::TurnEnded { .. }) {
            let mut wire = serde_json::to_value(&entry.record).unwrap();
            wire["elapsed_ms"] = 1000.into();
            entry.record = serde_json::from_value(wire).unwrap();
        }
    }
    h.trace.clear();
    h.restart();
    assert!(
        matches!(latest_decision(&h, agent), LoopDecision::Hold { .. }),
        "two fast failures must hold before a third managed turn"
    );
    assert_eq!(h.supervisor.world().agents[&agent].rung, BreakerRung::Steer);
    assert_eq!(
        h.journal
            .iter()
            .filter(|entry| matches!(entry.record, Record::TurnIssued { .. }))
            .count(),
        2
    );
    assert!(!h.trace.iter().any(|s| matches!(s, Seen::Spawn { .. })));
}

#[test]
fn an_issue_time_adapter_refusal_is_a_visible_hold_without_a_hop() {
    let mut h = Harness::new();
    let agent = h.hire();
    h.drive(Input::Command(Command::GoalSet {
        completion_check: None,
        agent,
        text: "bad\0task".into(),
        predicate: Predicate::Command {
            argv: vec!["false".into()],
            timeout_s: 1,
        },
        budget: GoalBudget {
            usd: None,
            tokens: None,
        },
        hop_cap: 3,
        browser_goal: None,
        goal_replacement: None,
        baseline: None,
    }));
    assert!(
        matches!(latest_decision(&h, agent), LoopDecision::Hold { .. }),
        "an adapter refusal must explain why no launch follows"
    );
    assert_eq!(
        h.records()
            .filter(|r| matches!(r, Record::TurnIssued { .. }))
            .count(),
        0
    );
    assert!(!h.trace.iter().any(|s| matches!(s, Seen::Spawn { .. })));
}

fn fail(
    h: &mut Harness,
    agent: AgentId,
    elapsed_ms: Option<u64>,
    code: Option<i32>,
    usage: Option<Usage>,
    model_usage: BTreeMap<String, Usage>,
) -> TurnId {
    let turn = h.open_turn(agent).expect("a running turn");
    h.drive(Input::ChildExited {
        final_reply: None,
        turn,
        elapsed_ms,
        exit: ExitStatus { code, signal: None },
        usage: usage.map(ScopedUsage::Turn),
        model_usage,
        outcome: TurnOutcome::NoResult,
        last_message: None,
    });
    turn
}

fn predicate_fails(h: &mut Harness, goal: GoalId) {
    h.drive(Input::PredicateDone {
        goal,
        pass: false,
        evidence: Evidence::Command {
            argv: vec!["false".into()],
            program: Some("/usr/bin/false".into()),
            exit: Some(1),
            output_tail: String::new(),
        },
    });
}

fn issued(h: &Harness) -> usize {
    h.journal
        .iter()
        .filter(|entry| matches!(entry.record, Record::TurnIssued { .. }))
        .count()
}

fn two_failures() -> (Harness, AgentId, GoalId, TurnId, TurnId) {
    let mut h = Harness::new();
    let (agent, goal) = h.hire_with_goal(&["false"]);
    let first = fail(&mut h, agent, Some(1000), Some(1), None, BTreeMap::new());
    predicate_fails(&mut h, goal);
    let latest = fail(
        &mut h,
        agent,
        Some(5000),
        Some(1),
        Some(Usage {
            usd: Some(Usd::ZERO),
            ..Usage::ZERO
        }),
        BTreeMap::new(),
    );
    (h, agent, goal, first, latest)
}

#[test]
fn live_failures_hold_durably_before_the_third_issue_and_a_quiet_tick() {
    let (mut h, agent, _, first, latest) = two_failures();
    assert_eq!(
        latest_decision(&h, agent),
        &LoopDecision::Hold {
            reason: HoldReason::RepeatedFastFailure
        }
    );
    assert_eq!(issued(&h), 2);
    assert_eq!(
        h.supervisor.world().agents[&agent].automatic_hold,
        Some(AutomaticHold::RepeatedFastFailure { first, latest })
    );
    let (position, held) = h
        .journal
        .iter()
        .enumerate()
        .find(|(_, e)| matches!(e.record, Record::AgentHeld { .. }))
        .unwrap();
    assert!(matches!(
        held.record,
        Record::AgentHeld {
            provenance: Provenance::Inferred,
            ..
        }
    ));
    assert_eq!(
        h.supervisor.world().agents[&agent].control_lsn,
        Some(dudes_types::Lsn(position as u64 + 1))
    );
    h.drive(Input::Tick);
    assert_eq!(
        h.supervisor.world().agents[&agent].rung,
        BreakerRung::Healthy
    );
    assert!(h.supervisor.world().agents[&agent].automatic_hold.is_some());
    h.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert_eq!(issued(&h), 3);
    assert!(h.supervisor.world().agents[&agent].automatic_hold.is_none());
    fail(&mut h, agent, Some(1000), Some(1), None, BTreeMap::new());
    assert!(h.supervisor.world().agents[&agent].automatic_hold.is_none());
}

#[test]
fn nonqualifying_ends_clear_the_failure_episode() {
    let mut positive = Vec::new();
    for field in 0..5 {
        let mut usage = Usage::ZERO;
        match field {
            0 => usage.input = Tokens::new(1),
            1 => usage.output = Tokens::new(1),
            2 => usage.cache_read = Tokens::new(1),
            3 => usage.cache_creation = Tokens::new(1),
            _ => usage.usd = Some(Usd::from_micros(1)),
        }
        positive.push(usage);
    }
    let mut cases = vec![
        (None, Some(1), None, BTreeMap::new()),
        (Some(5001), Some(1), None, BTreeMap::new()),
        (Some(1000), Some(0), None, BTreeMap::new()),
        (Some(1000), None, None, BTreeMap::new()),
    ];
    for usage in positive {
        cases.push((Some(1000), Some(1), Some(usage), BTreeMap::new()));
        cases.push((
            Some(1000),
            Some(1),
            None,
            BTreeMap::from([("model".into(), usage)]),
        ));
    }
    for (elapsed, code, usage, model_usage) in cases {
        let mut h = Harness::new();
        let (agent, goal) = h.hire_with_goal(&["false"]);
        fail(&mut h, agent, Some(1000), Some(1), None, BTreeMap::new());
        predicate_fails(&mut h, goal);
        fail(&mut h, agent, elapsed, code, usage, model_usage);
        predicate_fails(&mut h, goal);
        assert_eq!(issued(&h), 3);
        assert!(h.supervisor.world().agents[&agent].fast_failures.is_none());
        fail(&mut h, agent, Some(1000), Some(1), None, BTreeMap::new());
        assert!(h.supervisor.world().agents[&agent].automatic_hold.is_none());
    }
}

#[test]
fn every_protection_crash_prefix_reconciles_once() {
    let (h, agent, _, _, _) = two_failures();
    let last_end = h
        .journal
        .iter()
        .rposition(|e| matches!(e.record, Record::TurnEnded { .. }))
        .unwrap();
    let hold = h
        .journal
        .iter()
        .position(|e| matches!(e.record, Record::AgentHeld { .. }))
        .unwrap();
    for through in last_end..=hold {
        let mut replay = Harness::new();
        replay.journal = h.journal[..=through].to_vec();
        replay.restart();
        assert_eq!(issued(&replay), 2);
        assert_eq!(
            latest_decision(&replay, agent),
            &LoopDecision::Hold {
                reason: HoldReason::RepeatedFastFailure
            }
        );
        assert_eq!(
            replay
                .journal
                .iter()
                .filter(|e| matches!(e.record, Record::AgentHeld { .. }))
                .count(),
            1
        );
        assert_eq!(replay.journal.iter().filter(|e| matches!(&e.record, Record::BreakerBeat { signals, .. } if signals.contains(&dudes_types::BreakerSignal::RapidFailure))).count(), 1);
        assert!(!replay.trace.iter().any(|s| matches!(s, Seen::Spawn { .. })));
    }
}

#[test]
fn host_launch_failure_holds_with_one_issued_hop_and_unknown_exit() {
    let mut h = Harness::new();
    h.launch = false;
    let (agent, _) = h.hire_with_goal(&["false"]);
    let turn = h.open_turn(agent).unwrap();
    h.drive(Input::SpawnFailed {
        turn,
        reason: "managed process could not launch (NotFound)".into(),
    });
    assert_eq!(
        latest_decision(&h, agent),
        &LoopDecision::Hold {
            reason: HoldReason::LaunchFailed
        }
    );
    assert_eq!(issued(&h), 1);
    assert_eq!(
        h.supervisor.world().agents[&agent].rung,
        BreakerRung::Healthy
    );
    assert!(matches!(
        h.journal.iter().find_map(|e| match &e.record {
            Record::TurnEnded {
                exit, elapsed_ms, ..
            } => Some((*exit, *elapsed_ms)),
            _ => None,
        }),
        Some((ExitStatus::UNKNOWN, None))
    ));
    let cause = h.supervisor.world().agents[&agent]
        .automatic_hold
        .as_ref()
        .unwrap();
    assert_eq!(
        cause.to_string(),
        "launch failed: managed process could not launch (NotFound); fix the cause and resume"
    );
    assert!(h.journal.iter().any(|e| matches!(&e.record, Record::LoopDecided { gate_trace, .. } if gate_trace.last().unwrap().note.as_deref() == Some("launch failed: managed process could not launch (NotFound); fix the cause and resume"))));
}

#[test]
fn a_new_goal_resets_the_episode_and_ignores_an_old_turn_end() {
    let mut h = Harness::new();
    let (agent, old_goal) = h.hire_with_goal(&["false"]);
    fail(&mut h, agent, Some(1000), Some(1), None, BTreeMap::new());
    predicate_fails(&mut h, old_goal);
    let old_turn = h.open_turn(agent).unwrap();
    h.drive(Input::Command(Command::GoalSet {
        completion_check: None,
        agent,
        text: "replacement".into(),
        predicate: Predicate::Command {
            argv: vec!["false".into()],
            timeout_s: 1,
        },
        budget: GoalBudget {
            usd: None,
            tokens: None,
        },
        hop_cap: 8,
        browser_goal: None,
        goal_replacement: None,
        baseline: None,
    }));
    let new_goal = h.supervisor.world().agents[&agent].goal.unwrap();
    fail(&mut h, agent, Some(1000), Some(1), None, BTreeMap::new());
    assert!(
        h.supervisor.world().agents[&agent].fast_failures.is_none(),
        "old turn {old_turn} must not seed the replacement goal"
    );
    predicate_fails(&mut h, old_goal);
    fail(&mut h, agent, Some(1000), Some(1), None, BTreeMap::new());
    assert!(h.supervisor.world().agents[&agent].automatic_hold.is_none());
    predicate_fails(&mut h, new_goal);
    fail(&mut h, agent, Some(1000), Some(1), None, BTreeMap::new());
    assert!(h.supervisor.world().agents[&agent].automatic_hold.is_some());
}

fn hold_a_pending_token() -> (Harness, AgentId, GoalId, TurnId) {
    let mut h = Harness::new();
    h.unfsynced.insert(RecordKind::HookTokenMinted);
    let (agent, goal) = h.hire_with_goal(&["false"]);
    let turn = h.open_turn(agent).unwrap();
    let (started, attempt) = h
        .supervisor
        .world()
        .instructions
        .current_for_turn(turn)
        .unwrap();
    let owner = attempt.owner;
    let mint = h
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::Persist { token, entry }
                if matches!(entry.record, Record::HookTokenMinted { .. }) =>
            {
                Some(*token)
            }
            _ => None,
        })
        .unwrap();
    h.unfsynced.insert(RecordKind::AgentHeld);
    let before = h.trace.len();
    h.drive(Input::LaunchAdmissionRefused {
        agent,
        turn: Some(turn),
        refusal: dudes_supervisor::SharedFolderRefusal::CandidateFolderUnresolved,
        dispatch_token: None,
    });
    let held = h.trace[before..]
        .iter()
        .find_map(|seen| match seen {
            Seen::Persist { token, entry } if matches!(entry.record, Record::AgentHeld { .. }) => {
                Some(*token)
            }
            _ => None,
        })
        .unwrap();
    assert!(h.supervisor.world().agents[&agent].automatic_hold.is_none());
    h.acknowledge(mint);
    assert!(
        !h.trace
            .iter()
            .any(|seen| matches!(seen, Seen::Spawn { .. }))
    );
    assert!(h.host.spawned.is_empty());
    let proof = h.acknowledge(held);
    assert!(h.journal.iter().any(|entry| matches!(&entry.record,
        Record::InstructionObservationFinished {
            owner: captured,
            baseline: dudes_proto::InstructionBaselineRef::Started { lsn },
            settlement: dudes_proto::InstructionSettlement::NoProcess {
                proof_lsn,
                proof: dudes_proto::InstructionNoProcessProof::NoHostCall,
            },
            ..
        } if *captured == owner && *lsn == started && *proof_lsn == proof)));
    assert!(h.supervisor.world().agents[&agent].automatic_hold.is_some());
    assert!(!h.supervisor.recovery_launch_allowed(
        agent,
        turn,
        &dudes_supervisor::RecoveryOperation::empty()
    ));
    assert_eq!(issued(&h), 1);
    (h, agent, goal, turn)
}

#[test]
fn an_unacknowledged_hold_refuses_a_pending_token_spawn() {
    let (mut h, agent, goal, turn) = hold_a_pending_token();
    let original = h.supervisor.world().turns[&turn].clone();
    h.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    assert_eq!(h.spawns_of(turn).len(), 1);
    assert_eq!(h.host.spawned.len(), 1);
    assert_eq!(h.supervisor.world().agents[&agent].goal, Some(goal));
    assert!(h.supervisor.world().agents[&agent].automatic_hold.is_none());
    let resumed = &h.supervisor.world().turns[&turn];
    assert_eq!(resumed.issue_lsn, original.issue_lsn);
    assert_eq!(resumed.task, original.task);
    assert_eq!(resumed.goal, original.goal);
    assert_eq!(issued(&h), 1);
}

#[test]
fn a_failed_launch_crash_prefix_reconciles_before_new_work() {
    let mut h = Harness::new();
    h.launch = false;
    let (agent, _) = h.hire_with_goal(&["false"]);
    h.drive(Input::SpawnFailed {
        turn: h.open_turn(agent).unwrap(),
        reason: "missing program".into(),
    });
    let end = h
        .journal
        .iter()
        .position(|e| matches!(e.record, Record::TurnEnded { .. }))
        .unwrap();
    h.journal.truncate(end + 1);
    h.trace.clear();
    h.restart();
    assert_eq!(
        latest_decision(&h, agent),
        &LoopDecision::Hold {
            reason: HoldReason::LaunchFailed
        }
    );
    assert_eq!(issued(&h), 1);
    assert_eq!(
        h.journal
            .iter()
            .filter(|e| matches!(e.record, Record::AgentHeld { .. }))
            .count(),
        1
    );
    assert!(!h.trace.iter().any(|s| matches!(s, Seen::Spawn { .. })));
}

#[test]
fn rapid_failure_is_isolated_from_pending_breaker_signals() {
    let mut h = Harness::new();
    let (agent, goal) = h.hire_with_goal(&["false"]);
    fail(&mut h, agent, Some(1000), Some(1), None, BTreeMap::new());
    predicate_fails(&mut h, goal);
    h.drive(Input::Signal {
        agent,
        signal: dudes_supervisor::RuntimeSignal::HookTimeout,
    });
    fail(&mut h, agent, Some(1000), Some(1), None, BTreeMap::new());
    let signals = h
        .journal
        .iter()
        .rev()
        .find_map(|e| match &e.record {
            Record::BreakerBeat { signals, .. } => Some(signals.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(signals, vec![dudes_types::BreakerSignal::RapidFailure]);
    assert_eq!(h.supervisor.world().agents[&agent].rung, BreakerRung::Steer);
    h.drive(Input::Tick);
    assert_eq!(
        h.supervisor.world().agents[&agent].rung,
        BreakerRung::Constrain
    );
    assert_eq!(issued(&h), 2);
}

#[test]
fn a_killed_turn_never_seeds_the_failure_episode() {
    let mut h = Harness::new();
    let (agent, goal) = h.hire_with_goal(&["false"]);
    h.drive(Input::Command(Command::Halt { agent, text: None }));
    fail(&mut h, agent, Some(1), Some(1), None, BTreeMap::new());
    assert!(h.supervisor.world().agents[&agent].fast_failures.is_none());
    h.drive(Input::Command(Command::Resume {
        agent,
        browser: None,
    }));
    predicate_fails(&mut h, goal);
    fail(&mut h, agent, Some(1), Some(1), None, BTreeMap::new());
    assert!(h.supervisor.world().agents[&agent].automatic_hold.is_none());
}

#[test]
fn attached_and_inferred_ends_never_qualify_as_managed_failures() {
    let (h, agent, _, _, _) = two_failures();
    let last = h
        .journal
        .iter()
        .rposition(|e| matches!(e.record, Record::TurnEnded { .. }))
        .unwrap();
    for (mode, provenance) in [
        (dudes_types::Mode::Attached, Provenance::Observed),
        (dudes_types::Mode::Managed, Provenance::Inferred),
    ] {
        let mut replay = Harness::new();
        replay.journal = h.journal[..=last].to_vec();
        let Record::TurnEnded {
            mode: recorded_mode,
            provenance: recorded_provenance,
            ..
        } = &mut replay.journal[last].record
        else {
            unreachable!()
        };
        *recorded_mode = mode;
        *recorded_provenance = provenance;
        replay.restart();
        assert!(
            replay.supervisor.world().agents[&agent]
                .automatic_hold
                .is_none()
        );
        assert!(
            replay.supervisor.world().agents[&agent]
                .fast_failures
                .is_none()
        );
        assert!(
            replay
                .journal
                .iter()
                .all(|e| !matches!(e.record, Record::AgentHeld { .. }))
        );
    }
}

#[test]
fn a_new_goal_clears_the_hold_without_reopening_the_old_issued_turn() {
    let (mut h, agent, old_goal, turn) = hold_a_pending_token();
    let original = h.supervisor.world().turns[&turn].clone();
    h.drive(Input::Command(Command::GoalSet {
        completion_check: None,
        agent,
        text: "replacement".into(),
        predicate: Predicate::Command {
            argv: vec!["false".into()],
            timeout_s: 1,
        },
        budget: GoalBudget {
            usd: None,
            tokens: None,
        },
        hop_cap: 8,
        browser_goal: None,
        goal_replacement: None,
        baseline: None,
    }));
    let new_goal = h.supervisor.world().agents[&agent].goal.unwrap();
    assert_ne!(new_goal, old_goal);
    assert_eq!(h.supervisor.world().goals[&new_goal].last_turn, None);
    assert!(h.supervisor.world().agents[&agent].automatic_hold.is_none());
    assert!(!h.supervisor.recovery_launch_allowed(
        agent,
        turn,
        &dudes_supervisor::RecoveryOperation::empty()
    ));
    assert_eq!(
        h.supervisor.recovery_opportunity(agent).err(),
        Some(dudes_supervisor::input::RecoveryRefusal::NotRecoverable)
    );
    assert!(
        !h.trace
            .iter()
            .any(|seen| matches!(seen, Seen::Spawn { .. }))
    );
    assert!(h.host.spawned.is_empty());
    let unchanged = &h.supervisor.world().turns[&turn];
    assert_eq!(unchanged.issue_lsn, original.issue_lsn);
    assert_eq!(unchanged.task, original.task);
    assert_eq!(unchanged.goal, Some(old_goal));
    assert_eq!(issued(&h), 1);
    assert!(!h.journal.iter().any(|entry| matches!(
        entry.record,
        Record::RecoveryAuthorized(_) | Record::TurnEnded { .. }
    )));
}

#[test]
fn replay_of_an_old_launch_failure_excludes_execution_inputs_from_its_hold() {
    let mut h = Harness::new();
    h.launch = false;
    let (agent, _) = h.hire_with_goal(&["false"]);
    h.drive(Input::SpawnFailed {
        turn: h.open_turn(agent).unwrap(),
        reason: format!("missing /private/workspace/cli\n{}", "x".repeat(3000)),
    });
    let cause = h.supervisor.world().agents[&agent]
        .automatic_hold
        .as_ref()
        .unwrap();
    assert_eq!(
        cause,
        &AutomaticHold::LaunchFailed {
            reason: "managed process could not launch (unknown error)".into()
        }
    );
    let end = h
        .journal
        .iter()
        .position(|e| matches!(e.record, Record::TurnEnded { .. }))
        .unwrap();
    h.journal.truncate(end + 1);
    h.trace.clear();
    h.restart();
    assert_eq!(
        h.supervisor.world().agents[&agent]
            .automatic_hold
            .as_ref()
            .unwrap(),
        &AutomaticHold::LaunchFailed {
            reason: "managed process could not launch (unknown error)".into()
        }
    );
}

#[test]
fn the_actual_inferred_repeated_failure_hold_is_a_valid_recovery_source() {
    let (h, agent, _, first, latest) = two_failures();
    assert!(h.records().any(|record| matches!(record, Record::AgentHeld { agent: owner, provenance: Provenance::Inferred, cause: AutomaticHold::RepeatedFastFailure { first: named_first, latest: named_latest } } if *owner == agent && *named_first == first && *named_latest == latest)));
    let view = h.supervisor.recovery_opportunity(agent).unwrap();
    assert_eq!(
        view.required.reason,
        dudes_proto::recovery::RecoveryReason::HeldOrPausedFailure
    );
    assert!(matches!(
        view.required.boundary,
        dudes_proto::recovery::RecoveryBoundary::Held { first: Some(_), .. }
    ));
    assert_eq!(
        h.supervisor.world().prepare_recovery(
            dudes_proto::recovery_state::transition::RecoveryTransition::Required(&view.required)
        ),
        Ok(())
    );
    assert_eq!(issued(&h), 2);
}
