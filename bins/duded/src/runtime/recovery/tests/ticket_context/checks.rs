use super::*;
use dudes_proto::diff_stamp::TicketBinding;
use dudes_proto::rpc::ticket::{
    TicketCheckApplicability, TicketHistoricalCheckReason, TicketReadContext,
};
use dudes_types::{AgentId, GoalId, Lsn};

fn baseline(daemon: &mut TestDaemon, agent: AgentId, goal: GoalId) -> Lsn {
    let (card, round) = daemon.sup.world().ticket_goal(goal).unwrap();
    let mut record = dudes_proto::record::samples::record(RecordKind::TicketBaselineRecorded);
    if let Record::TicketBaselineRecorded {
        card: actual,
        goal: actual_goal,
        baseline,
    } = &mut record
    {
        *actual = card;
        *actual_goal = goal;
        baseline.binding = TicketBinding {
            card,
            round,
            goal,
            claimant: agent,
        };
    }
    let position = daemon.ctx.writer.next_lsn().unwrap();
    append_history(daemon, record);
    position
}

fn predicate(daemon: &mut TestDaemon, agent: AgentId, goal: GoalId, turn: TurnId) -> Lsn {
    let dudes_proto::Predicate::Command { argv, .. } =
        &daemon.sup.world().goals[&goal].goal.predicate
    else {
        panic!("command fixture");
    };
    let record = Record::PredicateResult {
        goal,
        turn: Some(turn),
        provenance: dudes_types::Provenance::Observed,
        pass: false,
        evidence: dudes_proto::Evidence::Command {
            argv: argv.clone(),
            program: Some("/toolchain/bin/false".into()),
            exit: Some(1),
            output_tail: String::new(),
        },
    };
    let position = daemon.ctx.writer.next_lsn().unwrap();
    append_history(daemon, record);
    assert_eq!(daemon.sup.world().goals[&goal].agent, agent);
    position
}

fn stamp(daemon: &mut TestDaemon, agent: AgentId, goal: GoalId, baseline: Lsn, check: Lsn) {
    let (card, round) = daemon.sup.world().ticket_goal(goal).unwrap();
    let mut record = dudes_proto::record::samples::record(RecordKind::TicketReviewStamped);
    if let Record::TicketReviewStamped {
        card: actual,
        goal: actual_goal,
        stamp,
    } = &mut record
    {
        *actual = card;
        *actual_goal = goal;
        stamp.binding = TicketBinding {
            card,
            round,
            goal,
            claimant: agent,
        };
        stamp.baseline_lsn = baseline;
        stamp.check_lsn = check;
    }
    append_history(daemon, record);
}

fn context(daemon: &mut TestDaemon, card: CardId) -> TicketReadContext {
    reading(daemon, card).context
}

fn reading(daemon: &mut TestDaemon, card: CardId) -> dudes_proto::rpc::ticket::TicketReadResult {
    let Answered::TicketRead(read) = daemon
        .ask(crate::runtime::Ask::Ticket(crate::ports::TicketAsk::Read(
            dudes_proto::rpc::ticket::TicketReadParams { card },
        )))
        .unwrap()
    else {
        panic!("ticket context");
    };
    *read
}

#[test]
fn an_exact_no_host_call_proof_does_not_invalidate_the_earlier_check() {
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let mut baseline_lsn = None;
    let (agent, turn) = issued_ticket_with(&mut daemon, &fixture, |daemon, agent, goal, _| {
        baseline_lsn = Some(baseline(daemon, agent, goal));
    });
    let goal = daemon.sup.world().turns[&turn].goal.unwrap();
    let card = daemon.sup.world().ticket_goal(goal).unwrap().0;
    let check = predicate(&mut daemon, agent, goal, turn);
    stamp(&mut daemon, agent, goal, baseline_lsn.unwrap(), check);
    let association = daemon
        .sup
        .world()
        .instructions
        .association_for_goal(goal, row(&daemon, card).0.seq)
        .unwrap();
    let owner = dudes_proto::InstructionObservationOwner::Managed {
        agent,
        turn,
        issue: dudes_proto::InstructionIssueAssociation::Issued {
            issue_lsn: daemon.sup.world().turns[&turn].issue_lsn,
        },
    };
    let Record::InstructionObservationStarted { snapshot, .. } =
        dudes_proto::record::samples::record(RecordKind::InstructionObservationStarted)
    else {
        unreachable!();
    };
    let started = daemon.ctx.writer.next_lsn().unwrap();
    append_history(
        &mut daemon,
        Record::InstructionObservationStarted {
            owner,
            ticket: Some(association),
            snapshot,
        },
    );
    assert_eq!(
        context(&mut daemon, card).check,
        TicketCheckApplicability::Unknown
    );
    let mut end = dudes_proto::record::samples::record(RecordKind::TurnEnded);
    if let Record::TurnEnded {
        agent: actual,
        turn: actual_turn,
        provenance,
        outcome,
        usage,
        session,
        session_usage,
        model_usage,
        ..
    } = &mut end
    {
        *actual = agent;
        *actual_turn = turn;
        *provenance = dudes_types::Provenance::Observed;
        *outcome = Some(dudes_proto::TurnOutcome::NotLaunched {
            reason: "fixture never called the host".into(),
        });
        *usage = None;
        *session = None;
        *session_usage = None;
        model_usage.clear();
    }
    let proof_lsn = daemon.ctx.writer.next_lsn().unwrap();
    append_history(&mut daemon, end);
    append_history(
        &mut daemon,
        Record::InstructionObservationFinished {
            owner,
            ticket: Some(association),
            baseline: dudes_proto::InstructionBaselineRef::Started { lsn: started },
            settlement: dudes_proto::InstructionSettlement::NoProcess {
                proof_lsn,
                proof: dudes_proto::InstructionNoProcessProof::NoHostCall,
            },
        },
    );
    assert_eq!(
        context(&mut daemon, card).check,
        TicketCheckApplicability::Current { check_lsn: check }
    );
    assert!(
        row(&daemon, card).1.instruction_review.is_none(),
        "a no-process interval does not invent an instruction review"
    );
    assert!(
        context(&mut daemon, card)
            .recovered_review
            .relations
            .is_empty()
    );
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn exact_check_remains_current_while_authorized_then_becomes_history_at_new_issue() {
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let mut baseline_lsn = None;
    let (agent, turn) = issued_ticket_with(&mut daemon, &fixture, |daemon, agent, goal, _| {
        baseline_lsn = Some(baseline(daemon, agent, goal));
    });
    append_vanished_attempt(&mut daemon, agent, turn);
    let goal = daemon.sup.world().turns[&turn].goal.unwrap();
    let card = daemon.sup.world().ticket_goal(goal).unwrap().0;
    let check = predicate(&mut daemon, agent, goal, turn);
    assert_eq!(
        context(&mut daemon, card).check,
        TicketCheckApplicability::Unknown,
        "a result without its exact stamp is not current"
    );
    stamp(&mut daemon, agent, goal, baseline_lsn.unwrap(), check);
    assert_eq!(
        context(&mut daemon, card).check,
        TicketCheckApplicability::Current { check_lsn: check }
    );
    let required = daemon.sup.recovery_opportunity(agent).unwrap().required;
    append_history(&mut daemon, Record::RecoveryRequired(required.clone()));
    let next = TurnId::from_parts(919, 4);
    authorize(
        &mut daemon,
        &required,
        RecoveryPlan::New {
            turn: next,
            context: RecoveryContext::Fresh {
                session: None,
                recap: RecoveryRecap::default(),
            },
        },
        81,
    );
    assert_eq!(
        context(&mut daemon, card).check,
        TicketCheckApplicability::Current { check_lsn: check },
        "a waiting plan admits no new work"
    );
    issue_turn(&mut daemon, &required, next);
    assert_eq!(
        context(&mut daemon, card).check,
        TicketCheckApplicability::Historical {
            check_lsn: check,
            reason: TicketHistoricalCheckReason::NewIssue
        }
    );
    let new_check = predicate(&mut daemon, agent, goal, next);
    assert_eq!(
        context(&mut daemon, card).check,
        TicketCheckApplicability::Unknown,
        "the old stamp cannot qualify the new result"
    );
    stamp(&mut daemon, agent, goal, baseline_lsn.unwrap(), new_check);
    assert_eq!(
        context(&mut daemon, card).check,
        TicketCheckApplicability::Current {
            check_lsn: new_check
        }
    );
    let target = context(&mut daemon, card).binding.unwrap();
    let mut wrong = target;
    wrong.round += 1;
    assert_eq!(
        crate::ticket_recovery::check(daemon.sup.world(), wrong),
        TicketCheckApplicability::Unknown
    );
    wrong = target;
    wrong.claimant = AgentId::from_parts(99, 1);
    assert_eq!(
        crate::ticket_recovery::check(daemon.sup.world(), wrong),
        TicketCheckApplicability::Unknown
    );
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn exact_later_same_turn_spawn_and_unproven_attempt_invalidate_check_applicability() {
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let mut baseline_lsn = None;
    let (agent, turn) = issued_ticket_with(&mut daemon, &fixture, |daemon, agent, goal, _| {
        baseline_lsn = Some(baseline(daemon, agent, goal));
    });
    let goal = daemon.sup.world().turns[&turn].goal.unwrap();
    let card = daemon.sup.world().ticket_goal(goal).unwrap().0;
    let check = predicate(&mut daemon, agent, goal, turn);
    stamp(&mut daemon, agent, goal, baseline_lsn.unwrap(), check);
    assert_eq!(
        context(&mut daemon, card).check,
        TicketCheckApplicability::Current { check_lsn: check }
    );
    let association = daemon
        .sup
        .world()
        .instructions
        .association_for_goal(goal, row(&daemon, card).0.seq)
        .unwrap();
    let owner = dudes_proto::InstructionObservationOwner::Managed {
        agent,
        turn,
        issue: dudes_proto::InstructionIssueAssociation::Issued {
            issue_lsn: daemon.sup.world().turns[&turn].issue_lsn,
        },
    };
    let Record::InstructionObservationStarted { snapshot, .. } =
        dudes_proto::record::samples::record(RecordKind::InstructionObservationStarted)
    else {
        unreachable!();
    };
    append_history(
        &mut daemon,
        Record::InstructionObservationStarted {
            owner,
            ticket: Some(association),
            snapshot,
        },
    );
    assert_eq!(
        context(&mut daemon, card).check,
        TicketCheckApplicability::Unknown,
        "a starting sample does not prove a host launch or no host call"
    );
    record_spawn(&mut daemon, agent, turn, true);
    assert_eq!(
        context(&mut daemon, card).check,
        TicketCheckApplicability::Historical {
            check_lsn: check,
            reason: TicketHistoricalCheckReason::LaterAttempt
        }
    );
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn uncertified_journal_advancement_keeps_a_recorded_check_unconfirmed() {
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let mut baseline_lsn = None;
    let (agent, turn) = issued_ticket_with(&mut daemon, &fixture, |daemon, agent, goal, _| {
        baseline_lsn = Some(baseline(daemon, agent, goal));
    });
    append_vanished_attempt(&mut daemon, agent, turn);
    let goal = daemon.sup.world().turns[&turn].goal.unwrap();
    let card = daemon.sup.world().ticket_goal(goal).unwrap().0;
    let check = predicate(&mut daemon, agent, goal, turn);
    stamp(&mut daemon, agent, goal, baseline_lsn.unwrap(), check);
    let before = reading(&mut daemon, card);
    assert!(
        before.ticket.check.is_some(),
        "the uncertainty test needs a retained check"
    );
    assert_eq!(
        before.context.check,
        TicketCheckApplicability::Current { check_lsn: check }
    );
    let applied = daemon.sup.world().last_applied_lsn;
    let projected = daemon.ctx.projections.last_applied_lsn();
    let position = daemon.ctx.writer.next_lsn().unwrap();
    daemon
        .ctx
        .writer
        .append(&dudes_proto::Entry {
            at: daemon.ctx.clock.now(),
            record: Record::Checkpoint {
                projection_version: 51,
                segment: position.segment(),
                offset: position.offset(),
            },
        })
        .unwrap();
    let entries = fixture.entries();
    let read = reading(&mut daemon, card);
    assert_eq!(read.context.check, TicketCheckApplicability::Unknown);
    assert_eq!(read.ticket.check, before.ticket.check);
    assert_eq!(
        read.context.recovered_review.state,
        dudes_proto::rpc::ticket::TicketHistoryState::Unavailable
    );
    assert_eq!(fixture.entries(), entries);
    assert_eq!(daemon.sup.world().last_applied_lsn, applied);
    assert_eq!(daemon.ctx.projections.last_applied_lsn(), projected);
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}
