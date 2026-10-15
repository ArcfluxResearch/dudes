//! A ticket round records a baseline before its first turn and a stamp after each predicate
//! result, and neither is recaptured for work already issued.

use std::collections::BTreeMap;

use dudes_proto::diff_stamp::{
    BaselineOutcome, CaptureInterval, DigestHex, StampOutcome, StampPayload,
};
use dudes_proto::goal::{Evidence, Predicate};
use dudes_proto::{ExitStatus, GoalBudget, Record, RecordKind, TurnOutcome};
use dudes_supervisor::testing::{
    Harness, Seen, journal_with_issued_turn, unavailable_baseline, unavailable_stamp,
};
use dudes_supervisor::{Command, Input};
use dudes_types::{AgentId, CardId, GoalId, Lsn, Seq, Usd};

fn card() -> CardId {
    CardId::from_parts(5, 1)
}

fn start(harness: &mut Harness, agent: AgentId, seq: u64) {
    harness.drive(Input::Command(Command::TicketStart {
        card: card(),
        agent,
        text: "ship it".into(),
        predicate: Predicate::Command {
            argv: vec!["true".into()],
            timeout_s: 600,
        },
        budget: GoalBudget {
            usd: Some(Usd::from_dollars(5)),
            tokens: None,
        },
        hop_cap: 8,
        completion_check: None,
        expected_seq: Seq(seq),
    }));
}

fn kinds(harness: &Harness) -> Vec<RecordKind> {
    harness.records().map(Record::kind).collect()
}

fn count(harness: &Harness, kind: RecordKind) -> usize {
    kinds(harness).iter().filter(|k| **k == kind).count()
}

fn durable(harness: &Harness, kind: RecordKind) -> usize {
    harness
        .journal
        .iter()
        .filter(|entry| entry.record.kind() == kind)
        .count()
}

fn position(harness: &Harness, kind: RecordKind) -> usize {
    kinds(harness)
        .iter()
        .position(|k| *k == kind)
        .unwrap_or_else(|| panic!("{kind:?} was not journaled"))
}

fn baseline_requests(harness: &Harness) -> Vec<(AgentId, u32, GoalId)> {
    harness
        .trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::CaptureBaseline {
                agent, round, goal, ..
            } => Some((*agent, *round, *goal)),
            _ => None,
        })
        .collect()
}

fn started_goal(harness: &Harness, round: usize) -> GoalId {
    harness
        .records()
        .filter_map(|record| match record {
            Record::TicketStarted { goal, .. } => Some(goal.id),
            _ => None,
        })
        .nth(round)
        .expect("the ticket start was journaled")
}

fn held() -> (Harness, AgentId) {
    let mut harness = Harness::new();
    harness.answer_captures = false;
    let agent = harness.hire();
    (harness, agent)
}

#[test]
fn the_first_turn_waits_for_the_baseline_record() {
    let (mut harness, agent) = held();
    start(&mut harness, agent, 1);
    let goal = started_goal(&harness, 0);

    assert_eq!(baseline_requests(&harness), vec![(agent, 1, goal)]);
    assert_eq!(count(&harness, RecordKind::TurnIssued), 0);
    assert_eq!(harness.open_turn(agent), None);

    let baseline = unavailable_baseline(agent, card(), 1, goal);
    harness.drive(Input::BaselineCaptured { goal, baseline });

    assert!(harness.records().any(|record| *record
        == Record::TicketBaselineRecorded {
            card: card(),
            goal,
            baseline,
        }));
    assert_eq!(count(&harness, RecordKind::TurnIssued), 1);
    assert!(
        position(&harness, RecordKind::TicketStarted)
            < position(&harness, RecordKind::TicketBaselineRecorded)
    );
    assert!(
        position(&harness, RecordKind::TicketBaselineRecorded)
            < position(&harness, RecordKind::TurnIssued)
    );
}

#[test]
fn another_decision_does_not_ask_again() {
    let (mut harness, agent) = held();
    start(&mut harness, agent, 1);
    harness.drive(Input::Tick);
    harness.drive(Input::Tick);
    assert_eq!(baseline_requests(&harness).len(), 1);
    assert_eq!(count(&harness, RecordKind::TurnIssued), 0);
}

#[test]
fn a_crash_after_the_start_owes_exactly_one_baseline() {
    let (mut harness, agent) = held();
    start(&mut harness, agent, 1);
    let goal = started_goal(&harness, 0);
    harness.restart();

    assert_eq!(
        baseline_requests(&harness),
        vec![(agent, 1, goal), (agent, 1, goal)]
    );
    assert_eq!(count(&harness, RecordKind::TurnIssued), 0);
    assert_eq!(count(&harness, RecordKind::TicketBaselineRecorded), 0);
}

#[test]
fn a_crash_after_the_baseline_and_before_the_turn_does_not_recapture() {
    let mut harness = Harness::new();
    harness.unfsynced.insert(RecordKind::TurnIssued);
    let agent = harness.hire();
    start(&mut harness, agent, 1);
    assert_eq!(durable(&harness, RecordKind::TicketBaselineRecorded), 1);
    assert_eq!(durable(&harness, RecordKind::TurnIssued), 0);

    harness.unfsynced.clear();
    harness.restart();

    assert_eq!(baseline_requests(&harness).len(), 1);
    assert_eq!(durable(&harness, RecordKind::TicketBaselineRecorded), 1);
    assert_eq!(durable(&harness, RecordKind::TurnIssued), 1);
}

#[test]
fn an_issued_turn_is_reissued_unchanged_and_gains_no_baseline() {
    let (mut entries, agent, turn) = journal_with_issued_turn(None, "ship it");
    let Record::GoalSet { goal, .. } = entries[1].record.clone() else {
        panic!("the second entry sets the goal")
    };
    entries[1].record = Record::TicketStarted {
        card: card(),
        agent,
        goal,
        expected_seq: Seq(1),
    };
    let mut harness = Harness::new();
    harness.journal = entries;
    harness.restart();

    assert!(baseline_requests(&harness).is_empty());
    assert_eq!(count(&harness, RecordKind::TicketBaselineRecorded), 0);
    let spawned: Vec<_> = harness
        .trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::Spawn { turn, .. } => Some(*turn),
            _ => None,
        })
        .collect();
    assert_eq!(spawned, vec![turn]);
}

#[test]
fn a_rework_start_drops_the_old_rounds_late_answer() {
    let (mut harness, agent) = held();
    start(&mut harness, agent, 1);
    start(&mut harness, agent, 3);
    let first = started_goal(&harness, 0);
    let second = started_goal(&harness, 1);
    assert_eq!(
        baseline_requests(&harness),
        vec![(agent, 1, first), (agent, 2, second)]
    );

    harness.drive(Input::BaselineCaptured {
        goal: first,
        baseline: unavailable_baseline(agent, card(), 1, first),
    });
    assert_eq!(count(&harness, RecordKind::TicketBaselineRecorded), 0);
    assert_eq!(count(&harness, RecordKind::TurnIssued), 0);

    let baseline = unavailable_baseline(agent, card(), 2, second);
    harness.drive(Input::BaselineCaptured {
        goal: second,
        baseline,
    });
    assert!(harness.records().any(|record| *record
        == Record::TicketBaselineRecorded {
            card: card(),
            goal: second,
            baseline,
        }));
    assert_eq!(count(&harness, RecordKind::TicketBaselineRecorded), 1);
    assert_eq!(count(&harness, RecordKind::TurnIssued), 1);
}

#[test]
fn a_baseline_naming_another_round_is_not_recorded() {
    let (mut harness, agent) = held();
    start(&mut harness, agent, 1);
    let goal = started_goal(&harness, 0);
    harness.drive(Input::BaselineCaptured {
        goal,
        baseline: unavailable_baseline(agent, card(), 9, goal),
    });
    assert_eq!(count(&harness, RecordKind::TicketBaselineRecorded), 0);
    assert_eq!(count(&harness, RecordKind::TurnIssued), 0);
}

fn end_turn_and_fail_predicate(harness: &mut Harness, agent: AgentId, goal: GoalId) {
    let turn = harness.open_turn(agent).expect("a turn is running");
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
    harness.drive(Input::PredicateDone {
        goal,
        pass: false,
        evidence: Evidence::Command {
            argv: vec!["true".into()],
            program: Some("/usr/bin/true".into()),
            exit: Some(1),
            output_tail: String::new(),
        },
    });
}

fn stamp_requests(harness: &Harness) -> Vec<(GoalId, Lsn, Lsn)> {
    harness
        .trace
        .iter()
        .filter_map(|seen| match seen {
            Seen::CaptureStamp {
                goal,
                baseline_lsn,
                check_lsn,
                ..
            } => Some((*goal, *baseline_lsn, *check_lsn)),
            _ => None,
        })
        .collect()
}

fn requested_baseline(harness: &Harness) -> dudes_proto::diff_stamp::BaselinePayload {
    harness
        .trace
        .iter()
        .find_map(|seen| match seen {
            Seen::CaptureStamp { baseline, .. } => Some(*baseline),
            _ => None,
        })
        .expect("a stamp was requested")
}

fn lsn_of(harness: &Harness, kind: RecordKind) -> Lsn {
    Lsn(u64::try_from(position(harness, kind)).unwrap() + 1)
}

fn round_with_failed_check() -> (Harness, AgentId, GoalId) {
    let mut harness = Harness::new();
    harness.answer_captures = false;
    let agent = harness.hire();
    start(&mut harness, agent, 1);
    let goal = started_goal(&harness, 0);
    harness.drive(Input::BaselineCaptured {
        goal,
        baseline: unavailable_baseline(agent, card(), 1, goal),
    });
    end_turn_and_fail_predicate(&mut harness, agent, goal);
    (harness, agent, goal)
}

#[test]
fn a_failed_check_is_stamped_before_the_next_turn_is_issued() {
    let (mut harness, _, goal) = round_with_failed_check();

    let check_lsn = lsn_of(&harness, RecordKind::PredicateResult);
    let baseline_lsn = lsn_of(&harness, RecordKind::TicketBaselineRecorded);
    assert_eq!(
        stamp_requests(&harness),
        vec![(goal, baseline_lsn, check_lsn)]
    );
    assert_eq!(count(&harness, RecordKind::TurnIssued), 1);
    assert_eq!(count(&harness, RecordKind::TicketReviewStamped), 0);

    let baseline = requested_baseline(&harness);
    let stamp = StampPayload::new(
        baseline.binding,
        baseline_lsn,
        check_lsn,
        CaptureInterval {
            started_at: baseline.interval.started_at,
            ended_at: baseline.interval.ended_at,
        },
        baseline.versions,
        StampOutcome::Sampled {
            complete: true,
            digest: DigestHex::parse(&"ab".repeat(32)).unwrap(),
            omitted: 0,
            omitted_lower_bound: false,
            untracked: 2,
            untracked_lower_bound: false,
        },
    )
    .unwrap();
    harness.drive(Input::StampCaptured {
        goal,
        check_lsn,
        stamp,
    });

    assert!(harness.records().any(|record| *record
        == Record::TicketReviewStamped {
            card: card(),
            goal,
            stamp,
        }));
    assert!(
        position(&harness, RecordKind::PredicateResult)
            < position(&harness, RecordKind::TicketReviewStamped)
    );
    assert_eq!(count(&harness, RecordKind::TurnIssued), 2);
    let review = harness.supervisor.world().ticket_review(goal).unwrap();
    assert_eq!(
        review.stamp,
        Some((lsn_of(&harness, RecordKind::TicketReviewStamped), stamp))
    );
}

#[test]
fn an_unavailable_stamp_does_not_block_the_decision() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    start(&mut harness, agent, 1);
    let goal = started_goal(&harness, 0);
    end_turn_and_fail_predicate(&mut harness, agent, goal);

    assert_eq!(count(&harness, RecordKind::TicketReviewStamped), 1);
    assert_eq!(count(&harness, RecordKind::TurnIssued), 2);
}

#[test]
fn a_stamp_naming_another_check_is_not_recorded_and_the_loop_continues() {
    let (mut harness, _, goal) = round_with_failed_check();
    let (_, baseline_lsn, check_lsn) = stamp_requests(&harness)[0];
    let baseline = requested_baseline(&harness);
    harness.drive(Input::StampCaptured {
        goal,
        check_lsn,
        stamp: unavailable_stamp(&baseline, baseline_lsn, Lsn(999)),
    });
    assert_eq!(count(&harness, RecordKind::TicketReviewStamped), 0);
    assert_eq!(count(&harness, RecordKind::TurnIssued), 2);
}

#[test]
fn a_crash_between_the_check_and_its_stamp_takes_no_late_sample() {
    let (mut harness, _, _) = round_with_failed_check();
    assert_eq!(stamp_requests(&harness).len(), 1);

    harness.restart();

    assert_eq!(stamp_requests(&harness).len(), 1);
    assert_eq!(baseline_requests(&harness).len(), 1);
    assert_eq!(count(&harness, RecordKind::TicketReviewStamped), 0);
}

#[test]
fn a_goal_without_a_baseline_is_never_stamped() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["true"]);
    end_turn_and_fail_predicate(&mut harness, agent, goal);
    assert!(stamp_requests(&harness).is_empty());
    assert_eq!(count(&harness, RecordKind::TicketReviewStamped), 0);
}

#[test]
fn baseline_outcomes_survive_replay_unchanged() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    start(&mut harness, agent, 1);
    let goal = started_goal(&harness, 0);
    let before = harness.supervisor.world().ticket_review(goal).cloned();
    harness.restart();
    assert_eq!(
        harness.supervisor.world().ticket_review(goal).cloned(),
        before
    );
    let Some(review) = before else {
        panic!("a baseline was recorded")
    };
    assert!(matches!(
        review.baseline.outcome,
        BaselineOutcome::Unavailable { .. }
    ));
}

#[test]
fn a_replayed_baseline_naming_another_claimant_is_ignored() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    start(&mut harness, agent, 1);
    let goal = started_goal(&harness, 0);
    let other = AgentId::from_parts(9, 9);
    for entry in &mut harness.journal {
        if let Record::TicketBaselineRecorded { baseline, .. } = &mut entry.record {
            *baseline = unavailable_baseline(other, card(), 1, goal);
        }
    }
    harness.restart();
    assert!(harness.supervisor.world().ticket_review(goal).is_none());
}

#[test]
fn a_replayed_stamp_naming_another_baseline_is_ignored() {
    let (mut harness, _, goal) = round_with_failed_check();
    let check_lsn = stamp_requests(&harness)[0].2;
    let baseline_lsn = stamp_requests(&harness)[0].1;
    let baseline = requested_baseline(&harness);
    harness.drive(Input::StampCaptured {
        goal,
        check_lsn,
        stamp: unavailable_stamp(&baseline, baseline_lsn, check_lsn),
    });
    assert_eq!(count(&harness, RecordKind::TicketReviewStamped), 1);
    for entry in &mut harness.journal {
        if let Record::TicketReviewStamped { stamp, .. } = &mut entry.record {
            stamp.baseline_lsn = Lsn(777);
        }
    }
    harness.restart();
    let review = harness.supervisor.world().ticket_review(goal).unwrap();
    assert_eq!(review.stamp, None);
}

#[test]
fn a_passing_check_is_stamped_before_the_loop_decides_done() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    start(&mut harness, agent, 1);
    let goal = started_goal(&harness, 0);
    let turn = harness.open_turn(agent).expect("a turn is running");
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
    harness.drive(Input::PredicateDone {
        goal,
        pass: true,
        evidence: Evidence::Command {
            argv: vec!["true".into()],
            program: Some("/usr/bin/true".into()),
            exit: Some(0),
            output_tail: String::new(),
        },
    });

    let kinds = kinds(&harness);
    let result = position(&harness, RecordKind::PredicateResult);
    let stamp = position(&harness, RecordKind::TicketReviewStamped);
    let done = harness
        .records()
        .position(|record| {
            matches!(
                record,
                Record::LoopDecided {
                    decision: dudes_proto::LoopDecision::Done,
                    ..
                }
            )
        })
        .unwrap_or_else(|| panic!("the loop never decided Done: {kinds:?}"));
    assert!(result < stamp && stamp < done, "{kinds:?}");
}
