use super::*;
use dudes_proto::recovery::{
    RecoveryAuthorized, RecoveryContext, RecoveryOrigin, RecoveryPlan, RecoveryRecap,
    RecoveryRequired,
};
use dudes_proto::{Actor, CardView, Record, RecordKind, TicketReason, TicketRow};
use dudes_types::{CardId, CardStatus, TurnId};

mod checks;

fn row(daemon: &TestDaemon, card: CardId) -> (CardView, TicketRow) {
    daemon.ctx.projections.ticket(card).unwrap().unwrap()
}

fn authorize(
    daemon: &mut TestDaemon,
    required: &RecoveryRequired,
    plan: RecoveryPlan,
    request: u8,
) {
    let (state, _) = daemon.sup.world().recovery_projection().unwrap();
    let episode = state.current(required.binding.work).unwrap().id;
    let control = daemon.sup.world().agents[&required.binding.work.agent].control_lsn;
    append_history(
        daemon,
        Record::RecoveryAuthorized(Box::new(RecoveryAuthorized {
            episode,
            binding: required.binding.clone(),
            request_id: Hex32::new([request; 32]),
            request_digest: Hex32::new([request + 1; 32]),
            attribution: RecoveryAttribution::LocalOperator {
                pid: 4242,
                uid: Some(501),
            },
            expected_control: control,
            resulting_control: control,
            resume_lsn: None,
            plan,
        })),
    );
}

#[test]
fn recovery_ticket_waits_for_new_issue_and_repeat_spawn_in_the_same_round() {
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let command = recoverable_ticket(&mut daemon, &fixture);
    let card = command.expected.ticket.unwrap().0;
    let before = row(&daemon, card);
    let required = daemon
        .sup
        .recovery_opportunity(command.expected.work.agent)
        .unwrap()
        .required;
    append_history(&mut daemon, Record::RecoveryRequired(required.clone()));
    daemon.ticket_reactions().unwrap();
    assert_eq!(row(&daemon, card).0.status, CardStatus::Blocked);
    assert_eq!(
        row(&daemon, card).1.reason,
        Some(TicketReason::RecoveryRequired)
    );
    let turn = TurnId::from_parts(917, 20);
    authorize(
        &mut daemon,
        &required,
        RecoveryPlan::New {
            turn,
            context: RecoveryContext::Fresh {
                session: None,
                recap: RecoveryRecap::default(),
            },
        },
        51,
    );
    daemon.ticket_reactions().unwrap();
    assert_eq!(
        row(&daemon, card).0.status,
        CardStatus::Blocked,
        "authorization is not admission"
    );
    issue_turn(&mut daemon, &required, turn);
    daemon.ticket_reactions().unwrap();
    assert_eq!(
        row(&daemon, card).0.status,
        CardStatus::Doing,
        "a new actual issue admits work without claiming a process ran"
    );
    let (repeated, issue) = repeat_required(&daemon, &required);
    append_history(&mut daemon, Record::RecoveryRequired(repeated.clone()));
    daemon.ticket_reactions().unwrap();
    assert_eq!(row(&daemon, card).0.status, CardStatus::Blocked);
    authorize(
        &mut daemon,
        &repeated,
        RecoveryPlan::RepeatIssued {
            issue,
            session: None,
        },
        61,
    );
    daemon.ticket_reactions().unwrap();
    assert_eq!(
        row(&daemon, card).0.status,
        CardStatus::Blocked,
        "repeat authorization only reuses the old issue"
    );
    record_spawn(&mut daemon, repeated.binding.work.agent, turn, true);
    daemon.ticket_reactions().unwrap();
    let after = row(&daemon, card);
    assert_eq!(after.0.status, CardStatus::Doing);
    assert_eq!(
        (after.0.claimed_by, after.1.goal, after.1.round),
        (before.0.claimed_by, before.1.goal, before.1.round)
    );
    assert_eq!(record_count(&fixture, RecordKind::TicketStarted), 1);
    assert_eq!(record_count(&fixture, RecordKind::TurnIssued), 2);
    let entries = fixture.entries();
    let read = daemon
        .ask(crate::runtime::Ask::Ticket(crate::ports::TicketAsk::Read(
            dudes_proto::rpc::ticket::TicketReadParams { card },
        )))
        .unwrap();
    let Answered::TicketRead(read) = read else {
        panic!("ticket read");
    };
    assert_eq!(read.context.binding.unwrap().round, 1);
    assert_eq!(
        fixture.entries(),
        entries,
        "ticket history lookup appends nothing"
    );
    let mut running = common::start(daemon);
    running.stop(crate::ExitCause::Ended);
}

#[test]
fn recovery_reactions_never_reopen_ready_or_done() {
    for status in [CardStatus::Review, CardStatus::Done] {
        let fixture = common::Fixture::new();
        let mut daemon = empty(&fixture);
        let command = recoverable_ticket(&mut daemon, &fixture);
        let card = command.expected.ticket.unwrap().0;
        let required = daemon
            .sup
            .recovery_opportunity(command.expected.work.agent)
            .unwrap()
            .required;
        append_history(&mut daemon, Record::RecoveryRequired(required));
        let current = row(&daemon, card).0;
        append_history(
            &mut daemon,
            Record::CardTransition {
                card,
                from: current.status,
                to: status,
                by: Actor::System,
                expected_seq: current.seq,
                reason: None,
            },
        );
        let before = row(&daemon, card);
        daemon.ticket_reactions().unwrap();
        assert_eq!(row(&daemon, card), before);
        let mut running = common::start(daemon);
        running.stop(crate::ExitCause::Ended);
    }
}

fn observation(
    daemon: &mut TestDaemon,
    agent: dudes_types::AgentId,
    goal: dudes_types::GoalId,
    turn: TurnId,
) -> (
    dudes_proto::InstructionObservationOwner,
    dudes_proto::InstructionTicketAssociation,
    dudes_types::Lsn,
) {
    let card = daemon.sup.world().ticket_goal(goal).unwrap().0;
    let association = daemon
        .sup
        .world()
        .instructions
        .association_for_goal(goal, row(daemon, card).0.seq)
        .unwrap();
    let owner = dudes_proto::InstructionObservationOwner::Managed {
        agent,
        turn,
        issue: dudes_proto::InstructionIssueAssociation::Prospective,
    };
    let Record::InstructionObservationStarted { snapshot, .. } =
        dudes_proto::record::samples::record(RecordKind::InstructionObservationStarted)
    else {
        unreachable!();
    };
    let started = daemon.ctx.writer.next_lsn().unwrap();
    append_history(
        daemon,
        Record::InstructionObservationStarted {
            owner,
            ticket: Some(association),
            snapshot,
        },
    );
    (owner, association, started)
}

fn finish(
    daemon: &mut TestDaemon,
    observation: (
        dudes_proto::InstructionObservationOwner,
        dudes_proto::InstructionTicketAssociation,
        dudes_types::Lsn,
    ),
    turn: TurnId,
) {
    let (owner, association, started) = observation;
    let end_lsn = daemon.sup.world().turns[&turn].end_lsn.unwrap();
    let Record::InstructionObservationStarted { snapshot, .. } =
        dudes_proto::record::samples::record(RecordKind::InstructionObservationStarted)
    else {
        unreachable!();
    };
    let sampled_at = daemon.ctx.clock.now();
    append_history(
        daemon,
        Record::InstructionObservationFinished {
            owner,
            ticket: Some(association),
            baseline: dudes_proto::InstructionBaselineRef::Started { lsn: started },
            settlement: dudes_proto::InstructionSettlement::Compared {
                end: dudes_proto::InstructionEndBoundary::ManagedTurn { turn, end_lsn },
                sampled_at,
                recovery: false,
                comparison: dudes_proto::InstructionComparison {
                    changes: Vec::new(),
                    gaps: snapshot.gaps,
                    usage: snapshot.usage,
                },
            },
        },
    );
}

fn issue_with_observation(
    daemon: &mut TestDaemon,
    required: &RecoveryRequired,
    turn: TurnId,
) -> (
    dudes_proto::InstructionObservationOwner,
    dudes_proto::InstructionTicketAssociation,
    dudes_types::Lsn,
) {
    let work = required.binding.work;
    let observed = observation(daemon, work.agent, work.goal, turn);
    issue_turn(daemon, required, turn);
    observed
}

fn issue_turn(
    daemon: &mut TestDaemon,
    required: &RecoveryRequired,
    turn: TurnId,
) -> dudes_types::Lsn {
    let work = required.binding.work;
    let decision = daemon.ctx.writer.next_lsn().unwrap();
    append_history(
        daemon,
        Record::LoopDecided {
            agent: work.agent,
            input_digest: "ab".repeat(32),
            decision: dudes_proto::LoopDecision::Continue {
                task: "recover the recorded work".into(),
            },
            gate_trace: Vec::new(),
        },
    );
    let issued = daemon.ctx.writer.next_lsn().unwrap();
    append_history(
        daemon,
        Record::TurnIssued {
            agent: work.agent,
            turn,
            goal: work.goal,
            mode: dudes_types::Mode::Managed,
            task: "recover the recorded work".into(),
            session: None,
            model_selection: required.binding.model_selection.clone(),
            lsn_of_decision: decision,
            answers: Vec::new(),
        },
    );
    issued
}

#[test]
fn recovered_review_links_the_exact_earlier_ack_and_keeps_settlement_separate() {
    use dudes_proto::rpc::ticket::{
        TicketAcknowledgmentAction, TicketHistoryState, TicketPriorBoundary, TicketPriorReview,
    };
    for settled in [false, true] {
        let (fixture, mut daemon, expected) = history_fixture(settled);
        let HistoryExpected {
            original,
            next,
            card,
            acknowledgment_lsn,
            watermark,
            settlement,
        } = expected;
        let entries = fixture.entries();
        let Answered::TicketRead(read) = daemon
            .ask(crate::runtime::Ask::Ticket(crate::ports::TicketAsk::Read(
                dudes_proto::rpc::ticket::TicketReadParams { card },
            )))
            .unwrap()
        else {
            panic!("ticket read");
        };
        let history = read.context.recovered_review;
        assert_eq!(history.state, TicketHistoryState::Complete);
        assert_eq!(history.relations.len(), 1);
        let relation = &history.relations[0];
        assert_eq!(
            relation.interval,
            dudes_proto::InstructionBaselineRef::Started { lsn: next.2 }
        );
        assert_eq!(
            relation.earlier,
            Some(dudes_proto::InstructionBaselineRef::Started { lsn: original.2 })
        );
        assert_eq!(
            relation.acknowledgment,
            TicketPriorReview::Found {
                lsn: acknowledgment_lsn
            }
        );
        assert_eq!(
            relation.boundary,
            settlement.map_or(TicketPriorBoundary::Unsettled, |lsn| {
                TicketPriorBoundary::Settled { lsn }
            })
        );
        assert_eq!(history.acknowledgments.len(), 1);
        assert_eq!(history.acknowledgments[0].watermark, watermark);
        assert_eq!(
            history.acknowledgments[0].action,
            TicketAcknowledgmentAction::AcceptedContinuing
        );
        assert_eq!(fixture.entries(), entries);
        let mut running = common::start(daemon);
        running.stop(crate::ExitCause::Ended);
    }
}

fn repeat_required(
    daemon: &TestDaemon,
    required: &RecoveryRequired,
) -> (RecoveryRequired, dudes_proto::recovery::RecoveryIssue) {
    let (state, _) = daemon.sup.world().recovery_projection().unwrap();
    let receipt = state
        .receipt_by_request(Hex32::new([51; 32]))
        .unwrap()
        .unwrap();
    let RecoveryClaimState::IssueBound(issue) = receipt.state else {
        panic!("actual issue-bound receipt");
    };
    let issue = issue.clone();
    let predecessor = receipt.claim;
    let mut binding = required.binding.clone();
    binding.work = issue.issued.work;
    binding.issue_lsn = issue.lsn;
    let repeated = RecoveryRequired {
        binding,
        origin: RecoveryOrigin::ThisBarrier,
        reason: dudes_proto::recovery::RecoveryReason::MissingSpawn,
        boundary: dudes_proto::recovery::RecoveryBoundary::MissingSpawn,
        predecessor: Some(predecessor),
    };
    (repeated, issue)
}

fn record_spawn(
    daemon: &mut TestDaemon,
    owner: dudes_types::AgentId,
    turn: TurnId,
    is_repeat: bool,
) {
    let mut spawn = dudes_proto::record::samples::record(RecordKind::TurnSpawned);
    if let Record::TurnSpawned {
        agent,
        turn: actual,
        session,
        reissued,
        ..
    } = &mut spawn
    {
        *agent = owner;
        *actual = turn;
        *session = None;
        *reissued = is_repeat;
    }
    append_history(daemon, spawn);
}

struct HistoryExpected {
    original: (
        dudes_proto::InstructionObservationOwner,
        dudes_proto::InstructionTicketAssociation,
        dudes_types::Lsn,
    ),
    next: (
        dudes_proto::InstructionObservationOwner,
        dudes_proto::InstructionTicketAssociation,
        dudes_types::Lsn,
    ),
    card: CardId,
    acknowledgment_lsn: dudes_types::Lsn,
    watermark: dudes_proto::InstructionReviewWatermark,
    settlement: Option<dudes_types::Lsn>,
}
fn history_fixture(settled: bool) -> (common::Fixture, TestDaemon, HistoryExpected) {
    let fixture = common::Fixture::new();
    let mut daemon = empty(&fixture);
    let mut original = None;
    let (agent, turn) = issued_ticket_with(&mut daemon, &fixture, |daemon, agent, goal, turn| {
        original = Some(observation(daemon, agent, goal, turn));
    });
    let original = original.unwrap();
    append_vanished_attempt(&mut daemon, agent, turn);
    finish(&mut daemon, original, turn);
    let card = original.1.card;
    let (_, ticket) = row(&daemon, card);
    let watermark = ticket.instruction_review.unwrap().watermark;
    let acknowledgment_lsn = daemon.ctx.writer.next_lsn().unwrap();
    let expected_seq = row(&daemon, card).0.seq;
    append_history(
        &mut daemon,
        Record::TicketInstructionReviewed {
            card,
            by: Actor::Peer {
                pid: 4242,
                uid: Some(501),
            },
            expected_seq,
            watermark,
            action: dudes_proto::InstructionReviewAction::Accept {
                completion: dudes_proto::InstructionAcceptCompletion::Continue,
            },
        },
    );
    let settlement = if settled {
        let lsn = daemon.ctx.writer.next_lsn().unwrap();
        let spawn = daemon
            .sup
            .world()
            .instructions
            .attempt(original.2)
            .unwrap()
            .spawn_lsn
            .unwrap();
        append_history(
            &mut daemon,
            Record::InstructionAttemptBoundarySettled {
                owner: original.0,
                started_lsn: original.2,
                proof: dudes_proto::InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                    receipt: dudes_proto::InstructionNativeBoundaryReceipt::Spawned { lsn: spawn },
                },
            },
        );
        Some(lsn)
    } else {
        None
    };
    let required = daemon.sup.recovery_opportunity(agent).unwrap().required;
    append_history(&mut daemon, Record::RecoveryRequired(required.clone()));
    daemon.ticket_reactions().unwrap();
    let new_turn = TurnId::from_parts(918, 30);
    authorize(
        &mut daemon,
        &required,
        RecoveryPlan::New {
            turn: new_turn,
            context: RecoveryContext::Fresh {
                session: None,
                recap: RecoveryRecap::default(),
            },
        },
        71,
    );
    let next = issue_with_observation(&mut daemon, &required, new_turn);
    append_vanished_attempt(&mut daemon, agent, new_turn);
    finish(&mut daemon, next, new_turn);
    (
        fixture,
        daemon,
        HistoryExpected {
            original,
            next,
            card,
            acknowledgment_lsn,
            watermark,
            settlement,
        },
    )
}

fn record_count(fixture: &common::Fixture, kind: RecordKind) -> usize {
    fixture
        .entries()
        .iter()
        .filter(|(_, entry)| entry.record.kind() == kind)
        .count()
}

fn finish_uncertain(
    daemon: &mut TestDaemon,
    observation: (
        dudes_proto::InstructionObservationOwner,
        dudes_proto::InstructionTicketAssociation,
        dudes_types::Lsn,
    ),
) {
    let (owner, ticket, started) = observation;
    let Record::InstructionObservationStarted { snapshot, .. } =
        dudes_proto::record::samples::record(RecordKind::InstructionObservationStarted)
    else {
        unreachable!();
    };
    let mut gaps = snapshot.gaps;
    gaps.push(dudes_proto::InstructionCoverageGap {
        reason: dudes_proto::InstructionGapKind::UnrecordedLaunchBoundary,
        context: dudes_proto::InstructionPathContext::Unresolved,
        agent: Some(match owner {
            dudes_proto::InstructionObservationOwner::Managed { agent, .. } => agent,
            _ => unreachable!(),
        }),
        path: None,
        origin: dudes_proto::InstructionGapOrigin::Observed,
    });
    append_history(
        daemon,
        Record::InstructionObservationFinished {
            owner,
            ticket: Some(ticket),
            baseline: dudes_proto::InstructionBaselineRef::Started { lsn: started },
            settlement: dudes_proto::InstructionSettlement::Unknown {
                reason: dudes_proto::InstructionUnknownReason::UnrecordedLaunchBoundary,
                end: None,
                sampled_at: None,
                recovery: true,
                comparison: dudes_proto::InstructionComparison {
                    changes: Vec::new(),
                    gaps,
                    usage: snapshot.usage,
                },
            },
        },
    );
}

fn acknowledge_current(daemon: &mut TestDaemon, card: CardId) -> dudes_types::Lsn {
    let (view, ticket) = row(daemon, card);
    let lsn = daemon.ctx.writer.next_lsn().unwrap();
    append_history(
        daemon,
        Record::TicketInstructionReviewed {
            card,
            by: Actor::Peer {
                pid: 4242,
                uid: Some(501),
            },
            expected_seq: view.seq,
            watermark: ticket.instruction_review.unwrap().watermark,
            action: dudes_proto::InstructionReviewAction::Accept {
                completion: dudes_proto::InstructionAcceptCompletion::Continue,
            },
        },
    );
    lsn
}

fn begin_repeated_review(
    daemon: &mut TestDaemon,
    original: &RecoveryRequired,
    turn: TurnId,
) -> (
    dudes_proto::InstructionObservationOwner,
    dudes_proto::InstructionTicketAssociation,
    dudes_types::Lsn,
) {
    let (required, issue) = repeat_required(daemon, original);
    append_history(daemon, Record::RecoveryRequired(required.clone()));
    authorize(
        daemon,
        &required,
        RecoveryPlan::RepeatIssued {
            issue,
            session: None,
        },
        61,
    );
    let work = required.binding.work;
    let association = daemon
        .sup
        .world()
        .instructions
        .association_for_goal(
            work.goal,
            row(daemon, original.binding.ticket.unwrap().card).0.seq,
        )
        .unwrap();
    let owner = dudes_proto::InstructionObservationOwner::Managed {
        agent: work.agent,
        turn,
        issue: dudes_proto::InstructionIssueAssociation::Issued {
            issue_lsn: required.binding.issue_lsn,
        },
    };
    let Record::InstructionObservationStarted { snapshot, .. } =
        dudes_proto::record::samples::record(RecordKind::InstructionObservationStarted)
    else {
        unreachable!();
    };
    let started = daemon.ctx.writer.next_lsn().unwrap();
    append_history(
        daemon,
        Record::InstructionObservationStarted {
            owner,
            ticket: Some(association),
            snapshot,
        },
    );
    (owner, association, started)
}

#[test]
fn new_no_spawn_provenance_and_repeat_attribution_keep_physical_attempt_uncertainty() {
    use dudes_proto::rpc::ticket::{TicketHistoryState, TicketPriorReview};
    for launched in [false, true] {
        let fixture = common::Fixture::new();
        let mut daemon = empty(&fixture);
        let command = recoverable_ticket(&mut daemon, &fixture);
        let card = command.expected.ticket.unwrap().0;
        let required = daemon
            .sup
            .recovery_opportunity(command.expected.work.agent)
            .unwrap()
            .required;
        append_history(&mut daemon, Record::RecoveryRequired(required.clone()));
        let turn = TurnId::from_parts(920, 4);
        authorize(
            &mut daemon,
            &required,
            RecoveryPlan::New {
                turn,
                context: RecoveryContext::Fresh {
                    session: None,
                    recap: RecoveryRecap::default(),
                },
            },
            51,
        );
        let original = issue_with_observation(&mut daemon, &required, turn);
        finish_uncertain(&mut daemon, original);
        let Answered::TicketRead(read) = daemon
            .ask(crate::runtime::Ask::Ticket(crate::ports::TicketAsk::Read(
                dudes_proto::rpc::ticket::TicketReadParams { card },
            )))
            .unwrap()
        else {
            panic!("ticket read");
        };
        assert_eq!(
            read.context.recovered_review.relations.len(),
            1,
            "the exact reserved New issue remains attributable without a Spawn"
        );
        assert_eq!(
            read.context.recovered_review.relations[0].interval,
            dudes_proto::InstructionBaselineRef::Started { lsn: original.2 }
        );
        let earlier_ack = acknowledge_current(&mut daemon, card);
        let retry = begin_repeated_review(&mut daemon, &required, turn);
        if launched {
            record_spawn(&mut daemon, required.binding.work.agent, turn, true);
            let mut end = dudes_proto::record::samples::record(RecordKind::TurnEnded);
            if let Record::TurnEnded {
                agent,
                turn: actual,
                session,
                ..
            } = &mut end
            {
                *agent = required.binding.work.agent;
                *actual = turn;
                *session = None;
            }
            append_history(&mut daemon, end);
            finish(&mut daemon, retry, turn);
        } else {
            finish_uncertain(&mut daemon, retry);
        }
        let Answered::TicketRead(read) = daemon
            .ask(crate::runtime::Ask::Ticket(crate::ports::TicketAsk::Read(
                dudes_proto::rpc::ticket::TicketReadParams { card },
            )))
            .unwrap()
        else {
            panic!("ticket read");
        };
        let history = read.context.recovered_review;
        if launched {
            assert_eq!(history.state, TicketHistoryState::Complete);
            assert_eq!(history.relations.len(), 1);
            assert_eq!(
                history.relations[0].interval,
                dudes_proto::InstructionBaselineRef::Started { lsn: retry.2 }
            );
            assert_eq!(
                history.relations[0].earlier,
                Some(dudes_proto::InstructionBaselineRef::Started { lsn: original.2 })
            );
            assert_eq!(
                history.relations[0].acknowledgment,
                TicketPriorReview::Found { lsn: earlier_ack }
            );
        } else {
            assert_eq!(history.state, TicketHistoryState::Unavailable);
            assert!(
                history.relations.is_empty(),
                "a sampling recovery flag and same TurnId cannot identify the physical attempt"
            );
        }
        let mut running = common::start(daemon);
        running.stop(crate::ExitCause::Ended);
    }
}
