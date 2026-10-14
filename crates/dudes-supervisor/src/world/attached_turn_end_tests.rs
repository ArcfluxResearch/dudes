use super::*;
use dudes_proto::instruction::{
    InstructionComparison, InstructionCoverageGap, InstructionEndBoundary, InstructionGapKind,
    InstructionPathContext, InstructionSettlement, InstructionUnknownReason,
};
use dudes_proto::record::Actor;
use dudes_types::{CardStatus, Mode, Seq, Timestamp};

fn agent() -> AgentId {
    AgentId::from_parts(1, 1)
}

fn card() -> CardId {
    CardId::from_parts(1, 1)
}

fn goal(id: u128) -> Goal {
    Goal {
        id: GoalId::from_parts(1, id),
        text: "complete the work".into(),
        predicate: Predicate::Command {
            argv: vec!["true".into()],
            timeout_s: 1,
        },
        completion_check: None,
        baseline: None,
        budget: dudes_proto::budget::GoalBudget {
            usd: None,
            tokens: None,
        },
        hop_cap: 1000,
    }
}

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::UNIX_EPOCH,
        record,
    }
}

fn apply(world: &mut World, lsn: u64, record: Record) {
    world.apply(Lsn(lsn), &entry(record));
}

fn prefix() -> Vec<(Lsn, Entry)> {
    [
        Record::AgentHired {
            id: agent(),
            name: "worker".into(),
            character: None,
            accent: None,
            role: "builder".into(),
            department: "engineering".into(),
            provider: Provider::Claude,
            cwd: "/work/repo".into(),
            workspace: None,
            budget: AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            trusted: true,
            worktree: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        },
        Record::TicketCreated {
            card: card(),
            title: "complete work".into(),
            description: String::new(),
            department: "engineering".into(),
            project: "repo".into(),
            done_when: dudes_proto::TicketCheck {
                id: "check".into(),
                label: "check".into(),
                predicate: goal(1).predicate,
            },
            by: Actor::System,
        },
        Record::TicketAssigned {
            card: card(),
            agent: agent(),
            by: Actor::System,
            message: None,
            expected_seq: Seq(0),
        },
        Record::TicketStarted {
            card: card(),
            agent: agent(),
            goal: goal(1),
            expected_seq: Seq(1),
        },
        Record::Attached {
            agent: agent(),
            by: Actor::System,
            hook_port: None,
            provenance: None,
        },
    ]
    .into_iter()
    .zip(1_u64..)
    .map(|(record, lsn)| (Lsn(lsn), entry(record)))
    .collect()
}

fn seeded() -> World {
    replay(&prefix())
}

fn replay(entries: &[(Lsn, Entry)]) -> World {
    let mut world = World::default();
    for (lsn, entry) in entries {
        world.apply(*lsn, entry);
    }
    world
}

fn ended(turn: TurnId) -> Record {
    Record::TurnEnded {
        agent: agent(),
        turn,
        mode: Mode::Attached,
        provenance: Provenance::Observed,
        elapsed_ms: None,
        exit: dudes_proto::record::ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        session_usage: None,
        model_usage: BTreeMap::new(),
        last_message: None,
        final_reply: None,
        session: None,
        outcome: Some(TurnOutcome::Completed),
    }
}

fn owner(turn: TurnId) -> InstructionObservationOwner {
    InstructionObservationOwner::AttachedTurn {
        agent: agent(),
        turn,
        attachment_lsn: None,
    }
}

fn finished(turn: TurnId, end_lsn: Lsn, ticket: Option<InstructionTicketAssociation>) -> Record {
    Record::InstructionObservationFinished {
        owner: owner(turn),
        ticket,
        baseline: InstructionBaselineRef::MissingTurnBoundary { turn, end_lsn },
        settlement: InstructionSettlement::Unknown {
            reason: InstructionUnknownReason::AttachedTurnStartMissing,
            end: Some(InstructionEndBoundary::AttachedTurn { turn, end_lsn }),
            sampled_at: None,
            recovery: true,
            comparison: InstructionComparison {
                gaps: vec![InstructionCoverageGap {
                    origin: dudes_proto::InstructionGapOrigin::Observed,
                    reason: InstructionGapKind::AttachedTurnStartMissing,
                    context: InstructionPathContext::Unresolved,
                    agent: Some(agent()),
                    path: None,
                }],
                ..InstructionComparison::default()
            },
        },
    }
}

fn replace_goal() -> Record {
    Record::GoalSet {
        agent: agent(),
        goal: goal(2),
        browser_onboarding: None,
        browser_goal: None,
        goal_replacement: None,
    }
}

#[test]
fn current_goal_changes_and_new_hooks_keep_the_original_end_association() {
    let mut world = seeded();
    let first = TurnId::from_parts(1, 1);
    let second = TurnId::from_parts(1, 2);
    apply(&mut world, 6, ended(first));
    let original = world.unmatched_attached_turn_end(Lsn(6)).unwrap().clone();
    assert_eq!(original.owner, owner(first));
    assert_eq!(original.goal, Some(goal(1).id));
    assert_eq!(original.ticket.unwrap().assignment_lsn, Lsn(3));
    assert_eq!(original.ticket.unwrap().seq_at_start, Seq(2));
    assert_eq!(world.instructions.attachment_lsn(agent()), Some(Lsn(5)));
    apply(&mut world, 7, replace_goal());
    apply(
        &mut world,
        8,
        Record::Detached {
            agent: agent(),
            by: Actor::System,
            provenance: None,
        },
    );
    apply(&mut world, 9, ended(second));
    let retained = world.unmatched_attached_turn_end(Lsn(6)).unwrap();
    assert_eq!(retained.owner, original.owner);
    assert_eq!(retained.goal, original.goal);
    assert_eq!(retained.ticket, original.ticket);
    let successor = world.unmatched_attached_turn_end(Lsn(9)).unwrap();
    assert_eq!(successor.owner, owner(second));
    assert_eq!(successor.goal, Some(goal(2).id));
    assert_eq!(successor.ticket, None);
    assert_eq!(world.unmatched_attached_turn_end_count(), 2);
    apply(&mut world, 10, finished(first, Lsn(6), None));
    assert!(world.unmatched_attached_turn_end(Lsn(6)).is_none());
    assert_eq!(world.instructions.attempt(Lsn(6)).unwrap().ticket, None);
    assert_eq!(
        world.unmatched_attached_turn_end(Lsn(9)).unwrap().owner,
        owner(second)
    );
    apply(&mut world, 11, finished(second, Lsn(9), None));
    assert_eq!(world.unmatched_attached_turn_end_count(), 0);
}

fn other_ticket(world: &mut World) -> InstructionTicketAssociation {
    let other = CardId::from_parts(1, 2);
    apply(
        world,
        7,
        Record::TicketCreated {
            card: other,
            title: "other work".into(),
            description: String::new(),
            department: "engineering".into(),
            project: "repo".into(),
            done_when: dudes_proto::TicketCheck {
                id: "check".into(),
                label: "check".into(),
                predicate: goal(2).predicate,
            },
            by: Actor::System,
        },
    );
    apply(
        world,
        8,
        Record::TicketAssigned {
            card: other,
            agent: agent(),
            by: Actor::System,
            message: None,
            expected_seq: Seq(0),
        },
    );
    apply(
        world,
        9,
        Record::TicketStarted {
            card: other,
            agent: agent(),
            goal: goal(2),
            expected_seq: Seq(1),
        },
    );
    world
        .instructions
        .recorded_association_for_goal(goal(2).id)
        .unwrap()
}

#[test]
fn a_different_valid_ticket_cannot_consume_the_captured_attached_end() {
    let mut world = seeded();
    let turn = TurnId::from_parts(1, 1);
    apply(&mut world, 6, ended(turn));
    let captured = world
        .unmatched_attached_turn_end(Lsn(6))
        .unwrap()
        .ticket
        .unwrap();
    let other = other_ticket(&mut world);
    assert_ne!(captured, other);
    let substituted = entry(finished(turn, Lsn(6), Some(other)));
    assert!(
        world
            .instructions
            .validate_next(&substituted, None, |bytes| {
                dudes_proto::record::Hex32::new(*blake3::hash(bytes).as_bytes())
            })
            .is_ok()
    );
    let before = world.last_applied_lsn;
    assert_eq!(
        world.validate_record_authority(&substituted),
        Err("attached turn finish ticket does not match the captured end association")
    );
    assert_eq!(world.last_applied_lsn, before);
    assert!(world.instructions.attempt(Lsn(6)).is_none());
    world.apply(Lsn(10), &substituted);
    assert_eq!(world.last_applied_lsn, before);
    assert!(world.instructions.attempt(Lsn(6)).is_none());
    assert_eq!(world.unmatched_attached_turn_end_count(), 1);
    let retained = world.unmatched_attached_turn_end(Lsn(6)).unwrap();
    assert_eq!(retained.owner, owner(turn));
    assert_eq!(retained.goal, Some(goal(1).id));
    assert_eq!(retained.ticket, Some(captured));
    assert_eq!(world.goal_of(agent()).unwrap().goal.id, goal(2).id);
}

#[test]
fn a_captured_ticket_or_eligibility_filtered_none_can_finish_the_exact_end() {
    for keep_ticket in [true, false] {
        let mut world = seeded();
        let turn = TurnId::from_parts(1, 1);
        apply(&mut world, 6, ended(turn));
        let captured = world
            .unmatched_attached_turn_end(Lsn(6))
            .unwrap()
            .ticket
            .unwrap();
        let supplied = keep_ticket.then_some(captured);
        let finishing = entry(finished(turn, Lsn(6), supplied));
        assert_eq!(world.validate_record_authority(&finishing), Ok(()));
        assert!(world.unmatched_attached_turn_end(Lsn(6)).is_some());
        assert!(world.instructions.attempt(Lsn(6)).is_none());
        world.apply(Lsn(7), &finishing);
        assert!(world.unmatched_attached_turn_end(Lsn(6)).is_none());
        let attempt = world.instructions.attempt(Lsn(6)).unwrap();
        assert_eq!(attempt.owner, owner(turn));
        assert_eq!(attempt.ticket, supplied);
        assert_eq!(attempt.finished.as_ref().unwrap().lsn, Lsn(7));
    }
}

#[test]
fn an_end_without_a_captured_ticket_cannot_take_a_historical_association() {
    let mut world = seeded();
    let original = world
        .instructions
        .recorded_association_for_goal(goal(1).id)
        .unwrap();
    apply(&mut world, 6, replace_goal());
    let turn = TurnId::from_parts(1, 1);
    apply(&mut world, 7, ended(turn));
    assert_eq!(
        world.unmatched_attached_turn_end(Lsn(7)).unwrap().ticket,
        None
    );
    let substituted = entry(finished(turn, Lsn(7), Some(original)));
    assert_eq!(
        world.validate_record_authority(&substituted),
        Err("attached turn finish ticket does not match the captured end association")
    );
    world.apply(Lsn(8), &substituted);
    assert!(world.instructions.attempt(Lsn(7)).is_none());
    assert!(world.unmatched_attached_turn_end(Lsn(7)).is_some());
    apply(&mut world, 9, finished(turn, Lsn(7), None));
    assert!(world.unmatched_attached_turn_end(Lsn(7)).is_none());
}

#[test]
fn a_guessed_attachment_cannot_retire_or_poison_the_exact_hook_owner() {
    let mut world = seeded();
    let turn = TurnId::from_parts(1, 1);
    apply(&mut world, 6, ended(turn));
    let mut guessed = finished(turn, Lsn(6), None);
    if let Record::InstructionObservationFinished { owner, .. } = &mut guessed {
        *owner = InstructionObservationOwner::AttachedTurn {
            agent: agent(),
            turn,
            attachment_lsn: Some(Lsn(5)),
        };
    }
    assert_eq!(
        world.validate_record_authority(&entry(guessed.clone())),
        Err("attached turn finish does not match the captured end owner")
    );
    apply(&mut world, 7, guessed);
    assert!(world.instructions.attempt(Lsn(6)).is_none());
    assert_eq!(
        world.unmatched_attached_turn_end(Lsn(6)).unwrap().owner,
        owner(turn)
    );
    apply(&mut world, 8, finished(turn, Lsn(6), None));
    assert!(world.unmatched_attached_turn_end(Lsn(6)).is_none());
    assert_eq!(
        world.instructions.attempt(Lsn(6)).unwrap().owner,
        owner(turn)
    );
}

#[test]
fn duplicate_ends_and_repeated_replay_do_not_rebind_or_restore_finished_receipts() {
    let turn = TurnId::from_parts(1, 1);
    let mut entries = prefix();
    entries.push((Lsn(6), entry(ended(turn))));
    entries.push((Lsn(7), entry(replace_goal())));
    entries.push((Lsn(8), entry(ended(turn))));
    let mut world = replay(&entries);
    assert_eq!(world.unmatched_attached_turn_end_count(), 1);
    assert!(world.unmatched_attached_turn_end(Lsn(8)).is_none());
    assert_eq!(
        world.unmatched_attached_turn_end(Lsn(6)).unwrap().goal,
        Some(goal(1).id)
    );
    world.apply(Lsn(6), &entry(ended(turn)));
    assert_eq!(world.unmatched_attached_turn_end_count(), 1);
    entries.push((Lsn(9), entry(finished(turn, Lsn(6), None))));
    for _ in 0..2 {
        let replayed = replay(&entries);
        assert_eq!(replayed.unmatched_attached_turn_end_count(), 0);
        assert!(
            replayed
                .instructions
                .attempt(Lsn(6))
                .unwrap()
                .finished
                .is_some()
        );
    }
    apply(&mut world, 9, finished(turn, Lsn(6), None));
    world.apply(Lsn(6), &entry(ended(turn)));
    assert_eq!(world.unmatched_attached_turn_end_count(), 0);
}

#[test]
fn completion_keeps_historical_metadata_after_current_goal_eligibility_disappears() {
    let mut world = seeded();
    let turn = TurnId::from_parts(1, 1);
    apply(&mut world, 6, ended(turn));
    let original = world.unmatched_attached_turn_end(Lsn(6)).unwrap().ticket;
    apply(
        &mut world,
        7,
        Record::LoopDecided {
            agent: agent(),
            input_digest: String::new(),
            decision: LoopDecision::Done,
            gate_trace: Vec::new(),
        },
    );
    apply(
        &mut world,
        8,
        Record::CardTransition {
            card: card(),
            from: CardStatus::Doing,
            to: CardStatus::Done,
            by: Actor::System,
            expected_seq: Seq(2),
            reason: None,
        },
    );
    apply(
        &mut world,
        9,
        Record::GoalCleared {
            agent: agent(),
            goal: goal(1).id,
        },
    );
    assert_eq!(world.completed_goal_lsn(goal(1).id), Some(Lsn(7)));
    assert!(world.goal_of(agent()).is_none());
    assert_eq!(
        world.unmatched_attached_turn_end(Lsn(6)).unwrap().ticket,
        original
    );
    assert_eq!(
        world.unmatched_attached_turn_end(Lsn(6)).unwrap().goal,
        Some(goal(1).id)
    );
    apply(&mut world, 10, finished(turn, Lsn(6), None));
    assert_eq!(world.unmatched_attached_turn_end_count(), 0);
}

#[test]
fn a_stale_recorded_ticket_start_is_not_a_current_end_association() {
    let mut world = seeded();
    let recorded = world
        .instructions
        .recorded_association_for_goal(goal(1).id)
        .unwrap();
    apply(
        &mut world,
        6,
        Record::TicketAssigned {
            card: card(),
            agent: agent(),
            by: Actor::System,
            message: None,
            expected_seq: Seq(2),
        },
    );
    assert_eq!(
        world.instructions.recorded_association_for_goal(goal(1).id),
        Some(recorded)
    );
    assert_eq!(
        world
            .instructions
            .association_for_goal(goal(1).id, recorded.seq_at_start),
        None
    );
    apply(&mut world, 7, ended(TurnId::from_parts(1, 1)));
    let captured = world.unmatched_attached_turn_end(Lsn(7)).unwrap();
    assert_eq!(captured.goal, Some(goal(1).id));
    assert_eq!(captured.ticket, None);
}

#[test]
fn bounded_queries_report_overflow_and_exact_finishes_release_current_capacity() {
    let mut world = seeded();
    for ordinal in 0..520_u64 {
        apply(
            &mut world,
            6 + ordinal,
            ended(TurnId::from_parts(2, u128::from(ordinal) + 1)),
        );
    }
    assert_eq!(world.unmatched_attached_turn_end_count(), 520);
    let (inspected, status) = world.unmatched_attached_turn_ends(usize::MAX);
    assert_eq!(inspected.len(), 512);
    assert_eq!(status, AttachedTurnEndQueryStatus::Truncated { omitted: 8 });
    drop(inspected);
    assert!(
        world
            .unmatched_attached_turn_end(Lsn(525))
            .unwrap()
            .ticket
            .is_some()
    );
    let (small, status) = world.unmatched_attached_turn_ends(3);
    assert_eq!(small.len(), 3);
    assert_eq!(
        status,
        AttachedTurnEndQueryStatus::Truncated { omitted: 517 }
    );
    drop(small);
    for ordinal in 0..520_u64 {
        apply(
            &mut world,
            526 + ordinal,
            finished(
                TurnId::from_parts(2, u128::from(ordinal) + 1),
                Lsn(6 + ordinal),
                None,
            ),
        );
    }
    let (current, status) = world.unmatched_attached_turn_ends(512);
    assert_eq!(current.len(), 0);
    assert_eq!(status, AttachedTurnEndQueryStatus::Complete);
    assert_eq!(world.unmatched_attached_turn_end_count(), 0);
    assert_eq!(world.instructions.attempts().count(), 520);
}

#[test]
fn only_actual_observed_attached_ends_enter_the_index() {
    let mut world = seeded();
    for (lsn, mode, provenance) in [
        (6, Mode::Attached, Provenance::Inferred),
        (7, Mode::Managed, Provenance::Observed),
    ] {
        let mut record = ended(TurnId::from_parts(3, u128::from(lsn)));
        if let Record::TurnEnded {
            mode: value,
            provenance: author,
            ..
        } = &mut record
        {
            *value = mode;
            *author = provenance;
        }
        apply(&mut world, lsn, record);
    }
    assert_eq!(world.unmatched_attached_turn_end_count(), 0);
}
