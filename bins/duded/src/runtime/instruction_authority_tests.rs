use super::work_test_support as common;
use super::*;
use dudes_proto::instruction::{
    InstructionBaselineRef, InstructionComparison, InstructionCoverageGap, InstructionEndBoundary,
    InstructionGapKind, InstructionObservationOwner, InstructionPathContext, InstructionSettlement,
    InstructionTicketAssociation, InstructionUnknownReason,
};
use dudes_proto::record::{RecordedWorkspace, samples};
use dudes_proto::{AgentBudget, Goal, RecordKind, TicketCheck};
use dudes_supervisor::world::AttachedTurnEndReceipt;

type TestDaemon = Daemon<
    common::SharedHost,
    common::ScriptedRunner,
    common::TestClock,
    dudes_supervisor::testing::FakeRng,
>;

fn append(daemon: &mut TestDaemon, record: Record) -> Lsn {
    let entry = Entry {
        at: daemon.ctx.clock.now(),
        record,
    };
    let lsn = daemon.ctx.append(daemon.sup.world(), &entry).unwrap();
    daemon.sup.handle(Input::Replayed { lsn, entry });
    assert_eq!(daemon.sup.world().last_applied_lsn, Some(lsn));
    assert!(daemon.sup.take_effects().is_empty());
    lsn
}

fn observed_hook_end(daemon: &mut TestDaemon, agent: AgentId) -> AttachedTurnEndReceipt {
    let event = dudes_providers::claude::translate(
        agent,
        daemon.ctx.clock.now(),
        &serde_json::json!({"hook_event_name":"Stop"}),
    )
    .unwrap();
    daemon.sup.handle(Input::Hook {
        hook: HookId(71),
        event,
    });
    let persist = daemon.sup.take_effects().into_iter().find(|effect| {
        matches!(effect, Effect::Persist { entry, .. } if matches!(entry.record,
            Record::TurnEnded { mode: Mode::Attached, provenance: dudes_types::Provenance::Observed, .. }))
    }).expect("the actual observed hook emits its end");
    let mut queue = VecDeque::new();
    daemon
        .ctx
        .execute(
            &persist,
            &mut queue,
            &mut PumpReport::Discard,
            &mut daemon.sup,
            &mut daemon.closing,
        )
        .unwrap();
    while let Some(input) = queue.pop_front() {
        daemon.sup.handle(input);
    }
    let _ = daemon.sup.take_effects();
    let (mut ends, status) = daemon.sup.world().unmatched_attached_turn_ends(1);
    assert!(matches!(
        status,
        dudes_supervisor::world::AttachedTurnEndQueryStatus::Complete
    ));
    let (lsn, captured) = ends.next().expect("captured actual end");
    assert_eq!(captured.end_lsn, *lsn);
    assert_eq!(captured.goal, None);
    assert_eq!(captured.ticket, None);
    captured.clone()
}

fn establish_ticket(daemon: &mut TestDaemon, agent: AgentId) -> InstructionTicketAssociation {
    let card = CardId::from_parts(2, 1);
    let check = TicketCheck {
        id: "test".into(),
        label: "Test check".into(),
        predicate: Predicate::Command {
            argv: vec!["test-check".into()],
            timeout_s: 1,
        },
    };
    let goal = Goal {
        id: GoalId::from_parts(2, 1),
        text: "Check the recorded work".into(),
        predicate: check.predicate.clone(),
        completion_check: crate::ticket_index::goal_check(&check),
        baseline: Some(CheckBaseline::MayAlreadyPass),
        budget: GoalBudget {
            usd: None,
            tokens: None,
        },
        hop_cap: 8,
    };
    assert!(goal.completion_check.is_some());
    append(
        daemon,
        Record::TicketCreated {
            card,
            title: "Recorded ticket".into(),
            description: "Retain the actual evidence".into(),
            department: "engineering".into(),
            project: "authority".into(),
            done_when: check,
            by: Actor::System,
        },
    );
    let assignment_lsn = append(
        daemon,
        Record::TicketAssigned {
            card,
            agent,
            by: Actor::System,
            message: None,
            expected_seq: Seq(0),
        },
    );
    append(
        daemon,
        Record::TicketStarted {
            card,
            agent,
            goal: goal.clone(),
            expected_seq: Seq(1),
        },
    );
    let association = daemon
        .sup
        .world()
        .instructions
        .recorded_association_for_goal(goal.id)
        .unwrap();
    assert_eq!(association.card, card);
    assert_eq!(association.assignment_lsn, assignment_lsn);
    assert_eq!(association.seq_at_start, Seq(2));
    assert_eq!(
        daemon.ctx.projections.card(card).unwrap().unwrap().status,
        CardStatus::Doing
    );
    assert_eq!(
        daemon.ctx.projections.goal(goal.id).unwrap().unwrap().id,
        goal.id
    );
    association
}

fn finishing(
    captured: &AttachedTurnEndReceipt,
    ticket: Option<InstructionTicketAssociation>,
    at: Timestamp,
) -> Entry {
    let InstructionObservationOwner::AttachedTurn {
        agent,
        turn,
        attachment_lsn: None,
    } = captured.owner
    else {
        panic!("actual hook owner has no invented attachment");
    };
    Entry {
        at,
        record: Record::InstructionObservationFinished {
            owner: captured.owner,
            ticket,
            baseline: InstructionBaselineRef::MissingTurnBoundary {
                turn,
                end_lsn: captured.end_lsn,
            },
            settlement: InstructionSettlement::Unknown {
                reason: InstructionUnknownReason::AttachedTurnStartMissing,
                end: Some(InstructionEndBoundary::AttachedTurn {
                    turn,
                    end_lsn: captured.end_lsn,
                }),
                sampled_at: None,
                recovery: true,
                comparison: InstructionComparison {
                    gaps: vec![InstructionCoverageGap {
                        origin: dudes_proto::InstructionGapOrigin::Observed,
                        reason: InstructionGapKind::AttachedTurnStartMissing,
                        context: InstructionPathContext::Unresolved,
                        agent: Some(agent),
                        path: None,
                    }],
                    ..InstructionComparison::default()
                },
            },
        },
    }
}

fn boot_attached(fixture: &common::Fixture, project: &std::path::Path) -> (TestDaemon, AgentId) {
    let seams = common::seams(
        common::SharedHost::new(),
        Arc::new(common::ScriptedRunner::default()),
        Arc::new(common::TestClock::new()),
        Witness::default(),
    );
    let mut daemon = crate::boot(&fixture.settings(), seams).unwrap();
    let agent = AgentId::from_parts(2, 1);
    let mut hired = samples::record(RecordKind::AgentHired);
    let Record::AgentHired {
        id,
        cwd,
        workspace,
        budget,
        sandbox,
        isolation,
        max_turns,
        max_budget,
        mcp_servers,
        ..
    } = &mut hired
    else {
        panic!("canonical hire sample");
    };
    *id = agent;
    *cwd = project.to_path_buf();
    *workspace = Some(RecordedWorkspace {
        id: "authority".into(),
        label: "Authority".into(),
    });
    *budget = AgentBudget {
        usd_per_day: None,
        tokens_per_day: None,
    };
    *sandbox = None;
    *isolation = None;
    *max_turns = None;
    *max_budget = None;
    mcp_servers.clear();
    append(&mut daemon, hired);
    append(
        &mut daemon,
        Record::Attached {
            agent,
            by: Actor::System,
            hook_port: None,
            provenance: None,
        },
    );
    (daemon, agent)
}

#[test]
fn a_later_ticket_cannot_rebind_an_actual_attached_hook_end_before_append() {
    let fixture = common::Fixture::new();
    let project = tempfile::tempdir().unwrap();
    let (mut daemon, agent) = boot_attached(&fixture, project.path());
    let captured = observed_hook_end(&mut daemon, agent);
    append(
        &mut daemon,
        Record::Detached {
            agent,
            by: Actor::System,
            provenance: None,
        },
    );
    let association = establish_ticket(&mut daemon, agent);
    assert!(association.assignment_lsn > captured.end_lsn);
    let fabricated = finishing(&captured, Some(association), daemon.ctx.clock.now());
    daemon
        .ctx
        .projections
        .validate_next_instruction(&fabricated)
        .expect("pure instruction validation accepts the later recorded association");
    daemon
        .ctx
        .goal_index
        .validate(daemon.sup.world(), &fabricated.record)
        .unwrap();
    let journal = fixture.entries();
    let tail = journal.last().unwrap().0;
    let writer_next = daemon.ctx.writer.next_lsn().unwrap();
    let projected_lsn = daemon.ctx.projections.last_applied_lsn();
    let projected_crc = daemon.ctx.projections.last_applied_crc();
    let published_lsn = daemon.ctx.last_lsn.load(Ordering::Relaxed);
    let projected_ticket = daemon.ctx.projections.ticket(association.card).unwrap();
    let projected_goal = daemon.ctx.projections.goal(association.goal).unwrap();
    let evidence = daemon.ctx.projections.instruction_state().unwrap();
    let live_world = format!("{:?}", daemon.sup.world());
    let refused = daemon
        .ctx
        .append(daemon.sup.world(), &fabricated)
        .unwrap_err();
    assert_eq!(
        refused.to_string(),
        "attached turn finish ticket does not match the captured end association"
    );
    assert_eq!(fixture.entries(), journal);
    assert_eq!(fixture.entries().len(), journal.len());
    assert_eq!(fixture.entries().last().unwrap().0, tail);
    assert_eq!(daemon.ctx.writer.next_lsn().unwrap(), writer_next);
    assert_eq!(daemon.ctx.writer.poisoned(), None);
    assert_eq!(daemon.ctx.projections.last_applied_lsn(), projected_lsn);
    assert_eq!(daemon.ctx.projections.last_applied_crc(), projected_crc);
    assert_eq!(daemon.ctx.last_lsn.load(Ordering::Relaxed), published_lsn);
    assert_eq!(
        daemon.ctx.projections.ticket(association.card).unwrap(),
        projected_ticket
    );
    assert_eq!(
        daemon.ctx.projections.goal(association.goal).unwrap(),
        projected_goal
    );
    assert_eq!(
        daemon.ctx.projections.instruction_state().unwrap(),
        evidence
    );
    assert_eq!(format!("{:?}", daemon.sup.world()), live_world);
    let valid = finishing(&captured, None, daemon.ctx.clock.now());
    let lsn = daemon.ctx.append(daemon.sup.world(), &valid).unwrap();
    daemon.sup.handle(Input::Replayed { lsn, entry: valid });
    assert_eq!(fixture.entries().len(), journal.len() + 1);
    assert_eq!(fixture.entries().last().unwrap().0, lsn);
    assert_eq!(daemon.ctx.projections.last_applied_lsn(), lsn);
    assert!(
        daemon
            .sup
            .world()
            .unmatched_attached_turn_end(captured.end_lsn)
            .is_none()
    );
    let actual = daemon.ctx.projections.instruction_state().unwrap();
    assert!(actual.attempt(captured.end_lsn).unwrap().finished.is_some());
    assert_eq!(actual.attempt(captured.end_lsn).unwrap().ticket, None);
    assert!(!actual.pending_for_goal(association.goal));
}
