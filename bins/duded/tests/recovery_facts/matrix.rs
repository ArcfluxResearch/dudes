use super::*;
use dudes_proto::record::Hex32;
use dudes_proto::recovery::{
    RecoveryAttribution, RecoveryAuthorized, RecoveryBoundary, RecoveryClaimRef, RecoveryContext,
    RecoveryEpisodeId, RecoveryInvalidated, RecoveryInvalidationEvidence,
    RecoveryInvalidationReason, RecoveryOrigin, RecoveryPlan, RecoveryReason, RecoveryRecap,
    RecoveryRequired,
};
use dudes_proto::recovery_state::RecoveryClaimState;
use dudes_proto::recovery_state::transition::RecoveryTransition;

fn issued(prefix: &mut Prefix, work: RecoveryWork) -> Lsn {
    let task = "immutable ordinary task";
    let mut decided = samples::record(RecordKind::LoopDecided);
    if let Record::LoopDecided {
        agent, decision, ..
    } = &mut decided
    {
        *agent = work.agent;
        *decision = LoopDecision::Continue { task: task.into() };
    }
    let decided = prefix.recovery_append(decided);
    prefix.recovery_append(Record::TurnIssued {
        agent: work.agent,
        goal: work.goal,
        turn: work.turn,
        mode: Mode::Managed,
        lsn_of_decision: decided,
        task: task.into(),
        session: None,
        model_selection: Some(ModelSelection::provider_default()),
        answers: Vec::new(),
    })
}

fn end(prefix: &mut Prefix, binding: &RecoveryBinding) -> Lsn {
    let mut record = samples::record(RecordKind::TurnEnded);
    let Record::TurnEnded {
        agent,
        turn,
        mode,
        provenance,
        outcome,
        session,
        session_usage,
        ..
    } = &mut record
    else {
        unreachable!()
    };
    *agent = binding.work.agent;
    *turn = binding.work.turn;
    *mode = Mode::Managed;
    *provenance = Provenance::Observed;
    *outcome = Some(dudes_proto::TurnOutcome::NoResult);
    *session = None;
    *session_usage = None;
    prefix.recovery_append(record)
}

fn ordinary_prefix(same_agent: bool) -> (Prefix, RecoveryBinding, RecoveryWork) {
    let mut prefix = Prefix::new();
    let hired_lsn = prefix.hired(agent(), None);
    prefix.recovery_append(Record::GoalSet {
        agent: agent(),
        goal: definition(command()),
        browser_onboarding: None,
        browser_goal: None,
        goal_replacement: None,
    });
    let work = RecoveryWork {
        agent: agent(),
        goal: goal(),
        turn: turn(),
    };
    let mut binding = RecoveryBinding {
        work,
        hired_lsn,
        issue_lsn: Lsn::ZERO,
        ticket: None,
        worktree: None,
        provider: Provider::Codex,
        model_selection: Some(ModelSelection::provider_default()),
    };
    let old = if same_agent {
        let work = RecoveryWork {
            turn: TurnId::from_parts(76, 4),
            ..work
        };
        let mut previous = binding.clone();
        previous.work = work;
        previous.issue_lsn = issued(&mut prefix, work);
        prefix.recovery_append(recovery_spawn(&previous, work.turn, false));
        end(&mut prefix, &previous);
        work
    } else {
        let work = RecoveryWork {
            agent: AgentId::from_parts(76, 1),
            goal: GoalId::from_parts(76, 2),
            turn: TurnId::from_parts(76, 3),
        };
        prefix.hired(work.agent, None);
        let mut other_goal = definition(command());
        other_goal.id = work.goal;
        prefix.recovery_append(Record::GoalSet {
            agent: work.agent,
            goal: other_goal,
            browser_onboarding: None,
            browser_goal: None,
            goal_replacement: None,
        });
        issued(&mut prefix, work);
        work
    };
    binding.issue_lsn = issued(&mut prefix, work);
    (prefix, binding, old)
}

fn collision_prefix(same_agent: bool) -> (Prefix, RecoveryAuthorized, RecoveryWork) {
    let (mut prefix, binding, old) = ordinary_prefix(same_agent);
    prefix.recovery_append(recovery_spawn(&binding, binding.work.turn, false));
    let end_lsn = end(&mut prefix, &binding);
    let origin = prefix.recovery_append(Record::Pause {
        agent: binding.work.agent,
        browser: None,
    });
    prefix.recovery_append(Record::RecoveryRequired(RecoveryRequired {
        binding: binding.clone(),
        origin: RecoveryOrigin::Existing { lsn: origin },
        reason: RecoveryReason::HeldOrPausedFailure,
        boundary: RecoveryBoundary::Paused { end_lsn },
        predecessor: None,
    }));
    let resumed = prefix.recovery_append(Record::Resume {
        agent: binding.work.agent,
        browser: None,
    });
    let authorization = RecoveryAuthorized {
        episode: RecoveryEpisodeId {
            work: binding.work,
            origin,
        },
        binding,
        request_id: Hex32::new([7; 32]),
        request_digest: Hex32::new([8; 32]),
        attribution: RecoveryAttribution::LocalOperator {
            pid: 42,
            uid: Some(501),
        },
        expected_control: Some(resumed),
        resulting_control: Some(resumed),
        resume_lsn: None,
        plan: RecoveryPlan::New {
            turn: TurnId::from_parts(76, 5),
            context: RecoveryContext::Fresh {
                session: None,
                recap: RecoveryRecap::default(),
            },
        },
    };
    (prefix, authorization, old)
}

type Preparation = (Option<RecoveryStateError>, Result<(), RecoveryStateError>);
type Refusals = (Preparation, Preparation);
type Preservation = (bool, bool, bool, bool, bool, bool);

fn preparations(prefix: &mut Prefix, authorization: &RecoveryAuthorized) -> Preparation {
    let persistent = match prefix
        .writer
        .checked_recovery(&mut prefix.persistent)
        .unwrap()
        .prepare_transition(RecoveryTransition::Authorized(authorization))
    {
        Ok(()) => None,
        Err(Error::RecoveryState(reason)) => Some(reason),
        Err(other) => panic!("unexpected preparation error: {other}"),
    };
    let world = prefix
        .world
        .prepare_recovery(RecoveryTransition::Authorized(authorization));
    (persistent, world)
}

fn observe_collision(same_agent: bool, collision: bool) -> (Refusals, Preservation) {
    let (mut prefix, mut authorization, old) = collision_prefix(same_agent);
    if collision {
        let RecoveryPlan::New { turn, .. } = &mut authorization.plan else {
            unreachable!()
        };
        *turn = old.turn;
    }
    let before = prefix.persistent.export().unwrap();
    let bytes = journal_bytes(&prefix.dir.path().join("journal"));
    let recovery = prefix
        .world
        .recovery_projection()
        .map(|(state, history)| (state.clone(), history.clone()))
        .unwrap();
    let old_row = prefix.world.turns[&old.turn].clone();
    let accounting = (
        prefix.world.goals[&authorization.binding.work.goal].turns_on_goal,
        prefix.world.goals[&old.goal].turns_on_goal,
    );
    let frontier = prefix.world.last_applied_lsn;
    let prepared = preparations(&mut prefix, &authorization);
    assert_eq!(prefix.persistent.export().unwrap(), before);
    assert_eq!(journal_bytes(&prefix.dir.path().join("journal")), bytes);
    assert_eq!(
        prefix.world.recovery_projection().unwrap(),
        (&recovery.0, &recovery.1)
    );
    let original_goal = authorization.binding.work.goal;
    let entry = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::RecoveryAuthorized(Box::new(authorization)),
    };
    let attempted = prefix.writer.next_lsn().unwrap();
    let appended = match prefix
        .writer
        .checked_recovery(&mut prefix.persistent)
        .unwrap()
        .append(&entry)
    {
        Ok(_) => None,
        Err(Error::RecoveryAdmission {
            kind: RecordKind::RecoveryAuthorized,
            reason,
        }) => Some(reason),
        Err(other) => panic!("unexpected append error: {other}"),
    };
    let applied = prefix.world.apply_checked(attempted, &entry);
    let after = &prefix.world.turns[&old.turn];
    let old_same = (
        after.agent,
        after.goal,
        after.issue_lsn,
        &after.task,
        &after.answers,
        &after.model_selection,
    ) == (
        old_row.agent,
        old_row.goal,
        old_row.issue_lsn,
        &old_row.task,
        &old_row.answers,
        &old_row.model_selection,
    );
    let preservation = (
        prefix.persistent.export().unwrap() == before,
        journal_bytes(&prefix.dir.path().join("journal")) == bytes,
        prefix.world.recovery_projection().unwrap() == (&recovery.0, &recovery.1),
        prefix.world.last_applied_lsn == frontier,
        old_same,
        (
            prefix.world.goals[&original_goal].turns_on_goal,
            prefix.world.goals[&old.goal].turns_on_goal,
        ) == accounting,
    );
    prefix.assert_recovery_folds();
    ((prepared, (appended, applied)), preservation)
}

#[test]
fn existing_ordinary_turns_refuse_new_authority_in_both_folds_and_preparations() {
    let mut observed = Vec::new();
    for same_agent in [false, true] {
        for collision in [false, true] {
            observed.push((
                same_agent,
                collision,
                observe_collision(same_agent, collision),
            ));
        }
    }
    for (same_agent, collision, actual) in observed {
        let reason = collision.then_some(RecoveryStateError::Binding);
        let result = if collision {
            Err(RecoveryStateError::Binding)
        } else {
            Ok(())
        };
        assert_eq!(
            actual,
            (
                ((reason, result), (reason, result)),
                (collision, collision, collision, collision, true, true)
            ),
            "same_agent={same_agent}, collision={collision}"
        );
    }
}

fn options(checkpoints: u64) -> JournalOptions {
    JournalOptions {
        fsync: Fsync::None,
        checkpoint_every: checkpoints,
        rotate_at: 16 * 1024 * 1024,
    }
}

fn storage_prefix(checkpoints: u64) -> (Prefix, RecoveryBinding) {
    let dir = tempfile::tempdir().unwrap();
    let (writer, _) =
        dudes_journal::open(&dir.path().join("journal"), options(checkpoints)).unwrap();
    let persistent = Projections::open_with(&dir.path().join("state.redb"), Fsync::None).unwrap();
    let mut prefix = Prefix {
        dir,
        writer,
        persistent,
        world: World::default(),
    };
    let hired_lsn = prefix.hired(agent(), None);
    prefix.recovery_append(Record::GoalSet {
        agent: agent(),
        goal: definition(command()),
        browser_onboarding: None,
        browser_goal: None,
        goal_replacement: None,
    });
    let issue_lsn = prefix.issue(Vec::new());
    let binding = RecoveryBinding {
        work: RecoveryWork {
            agent: agent(),
            goal: goal(),
            turn: turn(),
        },
        hired_lsn,
        issue_lsn,
        ticket: None,
        worktree: None,
        provider: Provider::Codex,
        model_selection: Some(ModelSelection::provider_default()),
    };
    (prefix, binding)
}

fn claim_after_pause(
    prefix: &mut Prefix,
    binding: RecoveryBinding,
    end_lsn: Lsn,
) -> (RecoveryAuthorized, Lsn, Lsn) {
    let origin = prefix.recovery_append(Record::Pause {
        agent: binding.work.agent,
        browser: None,
    });
    let barrier = prefix.recovery_append(Record::RecoveryRequired(RecoveryRequired {
        binding: binding.clone(),
        origin: RecoveryOrigin::Existing { lsn: origin },
        reason: RecoveryReason::HeldOrPausedFailure,
        boundary: RecoveryBoundary::Paused { end_lsn },
        predecessor: None,
    }));
    let resumed = prefix.recovery_append(Record::Resume {
        agent: binding.work.agent,
        browser: None,
    });
    let authorization = RecoveryAuthorized {
        episode: RecoveryEpisodeId {
            work: binding.work,
            origin,
        },
        binding,
        request_id: Hex32::new([7; 32]),
        request_digest: Hex32::new([8; 32]),
        attribution: RecoveryAttribution::LocalOperator {
            pid: 42,
            uid: Some(501),
        },
        expected_control: Some(resumed),
        resulting_control: Some(resumed),
        resume_lsn: None,
        plan: RecoveryPlan::New {
            turn: TurnId::from_parts(78, 5),
            context: RecoveryContext::Fresh {
                session: None,
                recap: RecoveryRecap::default(),
            },
        },
    };
    let at = prefix.recovery_append(Record::RecoveryAuthorized(Box::new(authorization.clone())));
    (authorization, barrier, at)
}

fn claim(authorization: &RecoveryAuthorized, at: Lsn) -> RecoveryClaimRef {
    RecoveryClaimRef {
        episode: authorization.episode,
        authorization_lsn: at,
    }
}

fn receipt(prefix: &mut Prefix, authorization: &RecoveryAuthorized, at: Lsn) -> RecoveryClaimState {
    let found = prefix
        .writer
        .checked_recovery(&mut prefix.persistent)
        .unwrap()
        .receipt(authorization.request_id, authorization.request_digest)
        .unwrap()
        .unwrap();
    assert_eq!(found.claim, claim(authorization, at));
    assert_eq!(found.authorization, *authorization);
    found.state
}

fn reopen(prefix: Prefix, checkpoints: u64) -> Prefix {
    let Prefix {
        dir,
        writer,
        persistent,
        world,
    } = prefix;
    drop(writer);
    drop(persistent);
    let (writer, _) =
        dudes_journal::open(&dir.path().join("journal"), options(checkpoints)).unwrap();
    let persistent = Projections::open_with(&dir.path().join("state.redb"), Fsync::None).unwrap();
    Prefix {
        dir,
        writer,
        persistent,
        world,
    }
}

#[test]
fn invalidation_story_survives_automatic_checkpoints_rotation_and_physical_reopen() {
    for checkpoints in [0, 1] {
        let (mut prefix, binding) = storage_prefix(checkpoints);
        prefix.recovery_append(recovery_spawn(&binding, binding.work.turn, false));
        let ended = end(&mut prefix, &binding);
        assert_eq!(prefix.writer.segment(), 1);
        let (authorization, barrier, at) = claim_after_pause(&mut prefix, binding.clone(), ended);
        assert_eq!(barrier, Lsn::from_parts(2, 0).unwrap());
        assert_eq!(
            receipt(&mut prefix, &authorization, at),
            RecoveryClaimState::Pending
        );
        let changed = prefix.recovery_append(Record::GoalCleared {
            agent: binding.work.agent,
            goal: binding.work.goal,
        });
        let evidence = RecoveryInvalidationEvidence::RecordedChange {
            reason: RecoveryInvalidationReason::GoalChanged,
            lsn: changed,
        };
        let invalidation_lsn =
            prefix.recovery_append(Record::RecoveryInvalidated(RecoveryInvalidated {
                claim: claim(&authorization, at),
                evidence,
            }));
        let expected = RecoveryClaimState::Invalidated {
            invalidation_lsn,
            evidence,
        };
        assert_eq!(receipt(&mut prefix, &authorization, at), expected);
        assert_eq!(
            prefix.world.turns[&binding.work.turn].issue_lsn,
            binding.issue_lsn
        );
        assert_eq!(prefix.world.goals[&binding.work.goal].turns_on_goal, 1);
        let frames: Vec<_> = prefix
            .writer
            .reader()
            .iter_from(Lsn::ZERO)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for actual in [barrier, at, invalidation_lsn] {
            let index = frames.iter().position(|(lsn, _)| *lsn == actual).unwrap();
            if checkpoints == 1 {
                assert_eq!(frames[index + 1].1.record.kind(), RecordKind::Checkpoint);
            }
        }
        assert_eq!(
            frames
                .iter()
                .filter(|(_, entry)| entry.record.kind() == RecordKind::Checkpoint)
                .count()
                == 0,
            checkpoints == 0
        );
        let export = prefix.persistent.export().unwrap();
        prefix = reopen(prefix, checkpoints);
        assert_eq!(prefix.persistent.export().unwrap(), export);
        assert_eq!(receipt(&mut prefix, &authorization, at), expected);
        prefix.assert_recovery_folds();
    }
}

fn refuse_recovery(prefix: &mut Prefix, record: Record, expected: RecoveryStateError) {
    let before = prefix.persistent.export().unwrap();
    let bytes = journal_bytes(&prefix.dir.path().join("journal"));
    let recovery = prefix
        .world
        .recovery_projection()
        .map(|(state, history)| (state.clone(), history.clone()))
        .unwrap();
    let frontier = prefix.world.last_applied_lsn;
    let transition = match &record {
        Record::RecoveryRequired(value) => RecoveryTransition::Required(value),
        Record::RecoveryAuthorized(value) => RecoveryTransition::Authorized(value),
        Record::RecoveryInvalidated(value) => RecoveryTransition::Invalidated(value),
        _ => unreachable!(),
    };
    let prepared = prefix
        .writer
        .checked_recovery(&mut prefix.persistent)
        .unwrap()
        .prepare_transition(transition);
    assert!(
        matches!(prepared, Err(Error::RecoveryState(reason)) if reason == expected),
        "{prepared:?}"
    );
    assert_eq!(prefix.world.prepare_recovery(transition), Err(expected));
    let kind = record.kind();
    let appended = prefix
        .writer
        .checked_recovery(&mut prefix.persistent)
        .unwrap()
        .append(&Entry {
            at: Timestamp::UNIX_EPOCH,
            record,
        });
    assert!(
        matches!(appended, Err(Error::RecoveryAdmission { kind: actual, reason }) if actual == kind && reason == expected),
        "{appended:?}"
    );
    assert_eq!(prefix.persistent.export().unwrap(), before);
    assert_eq!(journal_bytes(&prefix.dir.path().join("journal")), bytes);
    assert_eq!(
        prefix.world.recovery_projection().unwrap(),
        (&recovery.0, &recovery.1)
    );
    assert_eq!(prefix.world.last_applied_lsn, frontier);
}

fn invalidation(
    authorization: &RecoveryAuthorized,
    at: Lsn,
    reason: RecoveryInvalidationReason,
    changed: Lsn,
) -> Record {
    Record::RecoveryInvalidated(RecoveryInvalidated {
        claim: claim(authorization, at),
        evidence: RecoveryInvalidationEvidence::RecordedChange {
            reason,
            lsn: changed,
        },
    })
}

fn observed_result(prefix: &mut Prefix, provenance: Provenance) -> Lsn {
    prefix.recovery_append(Record::PredicateResult {
        goal: goal(),
        turn: Some(turn()),
        provenance,
        pass: true,
        evidence: Evidence::Command {
            argv: vec!["check".into()],
            program: Some("/tools/check".into()),
            exit: Some(0),
            output_tail: "ok".into(),
        },
    })
}

#[test]
fn a_pending_claim_closes_only_on_the_actual_qualified_completion_fact_in_all_folds() {
    let (mut prefix, authorization, at) = recovery_new_prefix();
    let inferred = observed_result(&mut prefix, Provenance::Inferred);
    assert!(
        source(&mut prefix, inferred)
            .context()
            .qualified_completion
            .is_none()
    );
    refuse_recovery(
        &mut prefix,
        invalidation(
            &authorization,
            at,
            RecoveryInvalidationReason::GoalCompleted,
            inferred,
        ),
        RecoveryStateError::Binding,
    );
    assert_eq!(
        receipt(&mut prefix, &authorization, at),
        RecoveryClaimState::Pending
    );
    let qualified = observed_result(&mut prefix, Provenance::Observed);
    let target = source(&mut prefix, qualified)
        .context()
        .qualified_completion
        .unwrap();
    assert_eq!((target.agent, target.goal), (agent(), goal()));
    let closed = prefix.recovery_append(invalidation(
        &authorization,
        at,
        RecoveryInvalidationReason::GoalCompleted,
        qualified,
    ));
    assert_eq!(
        receipt(&mut prefix, &authorization, at),
        RecoveryClaimState::Invalidated {
            invalidation_lsn: closed,
            evidence: RecoveryInvalidationEvidence::RecordedChange {
                reason: RecoveryInvalidationReason::GoalCompleted,
                lsn: qualified
            },
        }
    );
    prefix.assert_recovery_folds();
}

fn instruction_failure_prefix() -> (Prefix, RecoveryBinding) {
    let (template, binding) = instruction_review_prefix();
    let mut prefix = Prefix::new();
    for frame in template.writer.reader().iter_from(Lsn::ZERO).unwrap() {
        let (at, mut entry) = frame.unwrap();
        if let Record::TurnEnded {
            outcome,
            session,
            session_usage,
            ..
        } = &mut entry.record
        {
            prefix.recovery_append(recovery_spawn(&binding, binding.work.turn, false));
            *outcome = Some(dudes_proto::TurnOutcome::NoResult);
            *session = None;
            *session_usage = None;
        }
        let actual = prefix.recovery_append(entry.record);
        if at == binding.issue_lsn {
            assert_eq!(actual, binding.issue_lsn);
        }
    }
    (prefix, binding)
}

#[test]
fn a_pending_claim_uses_only_the_validated_instruction_derived_goal_clear() {
    use dudes_proto::instruction::{InstructionGoalClear, InstructionReviewAction};
    let (mut prefix, binding) = instruction_failure_prefix();
    let end_lsn = prefix.world.turns[&binding.work.turn].end_lsn.unwrap();
    let (authorization, _, at) = claim_after_pause(&mut prefix, binding, end_lsn);
    let (view, ticket) = prefix.persistent.ticket(card()).unwrap().unwrap();
    let clear = InstructionGoalClear {
        agent: agent(),
        goal: goal(),
    };
    let changed = prefix.recovery_append(Record::TicketInstructionReviewed {
        card: card(),
        by: peer(),
        expected_seq: view.seq,
        watermark: ticket.instruction_review.unwrap().watermark,
        action: InstructionReviewAction::ChangesRequested {
            agent: agent(),
            message: Some("rework".into()),
            clear_goal: Some(clear),
        },
    });
    assert_eq!(
        source(&mut prefix, changed).context().cleared_goal,
        Some(clear)
    );
    assert_eq!(prefix.world.agents[&agent()].goal, None);
    refuse_recovery(
        &mut prefix,
        invalidation(
            &authorization,
            at,
            RecoveryInvalidationReason::GoalCompleted,
            changed,
        ),
        RecoveryStateError::Binding,
    );
    assert_eq!(
        receipt(&mut prefix, &authorization, at),
        RecoveryClaimState::Pending
    );
    let closed = prefix.recovery_append(invalidation(
        &authorization,
        at,
        RecoveryInvalidationReason::GoalChanged,
        changed,
    ));
    assert_eq!(
        receipt(&mut prefix, &authorization, at),
        RecoveryClaimState::Invalidated {
            invalidation_lsn: closed,
            evidence: RecoveryInvalidationEvidence::RecordedChange {
                reason: RecoveryInvalidationReason::GoalChanged,
                lsn: changed
            },
        }
    );
    prefix.assert_recovery_folds();
}

fn terminal(work: RecoveryWork, outcome: dudes_proto::TurnOutcome) -> Record {
    let mut record = samples::record(RecordKind::TurnEnded);
    let Record::TurnEnded {
        agent,
        turn,
        mode,
        provenance,
        outcome: actual,
        session,
        session_usage,
        usage,
        model_usage,
        exit,
        final_reply,
        last_message,
        elapsed_ms,
    } = &mut record
    else {
        unreachable!()
    };
    *agent = work.agent;
    *turn = work.turn;
    *mode = Mode::Managed;
    *provenance = Provenance::Observed;
    *actual = Some(outcome);
    *session = None;
    *session_usage = None;
    *usage = None;
    model_usage.clear();
    exit.code = None;
    exit.signal = None;
    *final_reply = None;
    *last_message = None;
    *elapsed_ms = None;
    record
}

fn refuse_terminal(prefix: &mut Prefix, record: Record, work: RecoveryWork) {
    let before = prefix.persistent.export().unwrap();
    let bytes = journal_bytes(&prefix.dir.path().join("journal"));
    let recovery = prefix
        .world
        .recovery_projection()
        .map(|(state, history)| (state.clone(), history.clone()))
        .unwrap();
    let frontier = prefix.world.last_applied_lsn;
    let ordinary = |world: &World| {
        let row = &world.turns[&work.turn];
        (
            row.issue_lsn,
            row.spawn_lsn,
            row.end_lsn,
            world.agents[&work.agent].open_turn,
            world.goals[&work.goal].turns_on_goal,
        )
    };
    let selected = ordinary(&prefix.world);
    let attempted = prefix.writer.next_lsn().unwrap();
    let entry = Entry {
        at: Timestamp::UNIX_EPOCH,
        record,
    };
    let error = prefix
        .writer
        .checked_recovery(&mut prefix.persistent)
        .unwrap()
        .append(&entry);
    assert!(
        matches!(
            error,
            Err(Error::RecoveryAdmission {
                kind: RecordKind::TurnEnded,
                reason: RecoveryStateError::Binding
            })
        ),
        "{error:?}"
    );
    assert_eq!(
        prefix.world.apply_checked(attempted, &entry),
        Err(RecoveryStateError::Binding)
    );
    assert_eq!(prefix.persistent.export().unwrap(), before);
    assert_eq!(journal_bytes(&prefix.dir.path().join("journal")), bytes);
    assert_eq!(
        prefix.world.recovery_projection().unwrap(),
        (&recovery.0, &recovery.1)
    );
    assert_eq!(prefix.world.last_applied_lsn, frontier);
    assert_eq!(ordinary(&prefix.world), selected);
}

fn refuse_unproven_terminal(prefix: &mut Prefix, work: RecoveryWork) {
    for mode in [Mode::Managed, Mode::Attached] {
        let mut record = terminal(work, dudes_proto::TurnOutcome::NoResult);
        let Record::TurnEnded {
            mode: actual,
            provenance,
            ..
        } = &mut record
        else {
            unreachable!()
        };
        *actual = mode;
        if mode == Mode::Managed {
            *provenance = Provenance::Inferred;
        }
        refuse_terminal(prefix, record, work);
    }
    refuse_terminal(
        prefix,
        terminal(work, dudes_proto::TurnOutcome::Vanished),
        work,
    );
    let mut record = terminal(work, dudes_proto::TurnOutcome::NoResult);
    let Record::TurnEnded { outcome, .. } = &mut record else {
        unreachable!()
    };
    *outcome = None;
    refuse_terminal(prefix, record, work);
}

#[test]
fn issue_bound_claims_consume_only_matching_no_spawn_terminal_evidence() {
    for outcome in [
        dudes_proto::TurnOutcome::Completed,
        dudes_proto::TurnOutcome::MaxTurns,
        dudes_proto::TurnOutcome::BudgetExhausted,
        dudes_proto::TurnOutcome::Failed {
            reason: "observed child failure".into(),
        },
        dudes_proto::TurnOutcome::NoResult,
        dudes_proto::TurnOutcome::Killed {
            by: dudes_proto::KillReason::Halt,
        },
        dudes_proto::TurnOutcome::NotLaunched {
            reason: "host failure".into(),
        },
        dudes_proto::TurnOutcome::LaunchRefused {
            reason: "adapter refusal".into(),
        },
    ] {
        let (mut prefix, authorization, other) = collision_prefix(false);
        let at =
            prefix.recovery_append(Record::RecoveryAuthorized(Box::new(authorization.clone())));
        let RecoveryPlan::New { turn, .. } = authorization.plan else {
            unreachable!()
        };
        let work = RecoveryWork {
            turn,
            ..authorization.binding.work
        };
        let issue_lsn = issued(&mut prefix, work);
        let RecoveryClaimState::IssueBound(issue) = receipt(&mut prefix, &authorization, at) else {
            panic!("new recovery issue was not bound")
        };
        assert_eq!(issue.lsn, issue_lsn);
        assert_eq!(prefix.world.turns[&turn].spawn_lsn, None);
        refuse_unproven_terminal(&mut prefix, work);
        let mut unrelated = terminal(work, outcome.clone());
        if let Record::TurnEnded { agent, .. } = &mut unrelated {
            *agent = other.agent;
        }
        refuse_terminal(&mut prefix, unrelated, work);
        assert_eq!(
            receipt(&mut prefix, &authorization, at),
            RecoveryClaimState::IssueBound(issue.clone())
        );
        let accounting = (
            prefix.world.goals[&work.goal].turns_on_goal,
            prefix.world.goals[&other.goal].turns_on_goal,
        );
        let evidence_lsn = prefix.recovery_append(terminal(work, outcome));
        assert_eq!(
            receipt(&mut prefix, &authorization, at),
            RecoveryClaimState::Consumed {
                issue,
                evidence_lsn
            }
        );
        assert_eq!(
            (
                prefix.world.goals[&work.goal].turns_on_goal,
                prefix.world.goals[&other.goal].turns_on_goal
            ),
            accounting
        );
        assert_eq!(prefix.world.turns[&other.turn].agent, other.agent);
        prefix.assert_recovery_folds();
    }
}

fn held_required() -> (Prefix, RecoveryRequired) {
    let (mut prefix, binding) = ordinary(command());
    let ended = prefix.recovery_append(terminal(
        binding.work,
        dudes_proto::TurnOutcome::NotLaunched {
            reason: "host failure".into(),
        },
    ));
    let held = prefix.recovery_append(Record::AgentHeld {
        agent: binding.work.agent,
        provenance: Provenance::Observed,
        cause: dudes_proto::AutomaticHold::LaunchFailed {
            reason: "host failure".into(),
        },
    });
    let required = RecoveryRequired {
        binding,
        origin: RecoveryOrigin::Existing { lsn: held },
        reason: RecoveryReason::HeldOrPausedFailure,
        boundary: RecoveryBoundary::Held {
            end_lsn: ended,
            first: None,
        },
        predecessor: None,
    };
    (prefix, required)
}

fn vanished_required() -> (Prefix, RecoveryRequired) {
    let (mut prefix, binding) = ordinary(command());
    let spawn_lsn = prefix.recovery_append(recovery_spawn(&binding, binding.work.turn, false));
    let end_lsn =
        prefix.recovery_append(terminal(binding.work, dudes_proto::TurnOutcome::Vanished));
    let required = RecoveryRequired {
        binding,
        origin: RecoveryOrigin::ThisBarrier,
        reason: RecoveryReason::Vanished,
        boundary: RecoveryBoundary::Vanished { spawn_lsn, end_lsn },
        predecessor: None,
    };
    (prefix, required)
}

fn unconfirmed_required() -> (Prefix, RecoveryRequired) {
    let (mut prefix, mut binding) = ordinary(command());
    prefix.recovery_append(recovery_spawn(&binding, binding.work.turn, false));
    let session = dudes_types::SessionId::new("recorded-context");
    let confirmed = prefix.recovery_append(Record::SessionLearned {
        agent: binding.work.agent,
        turn: binding.work.turn,
        session: session.clone(),
    });
    end(&mut prefix, &binding);
    let work = RecoveryWork {
        turn: TurnId::from_parts(78, 6),
        ..binding.work
    };
    let mut decided = samples::record(RecordKind::LoopDecided);
    if let Record::LoopDecided {
        agent, decision, ..
    } = &mut decided
    {
        *agent = work.agent;
        *decision = LoopDecision::Continue {
            task: "try recorded context".into(),
        };
    }
    let decided = prefix.recovery_append(decided);
    binding.issue_lsn = prefix.recovery_append(Record::TurnIssued {
        agent: work.agent,
        goal: work.goal,
        turn: work.turn,
        mode: Mode::Managed,
        lsn_of_decision: decided,
        task: "try recorded context".into(),
        session: Some(session.clone()),
        model_selection: binding.model_selection.clone(),
        answers: Vec::new(),
    });
    binding.work = work;
    let mut spawned = recovery_spawn(&binding, work.turn, false);
    if let Record::TurnSpawned {
        session: requested, ..
    } = &mut spawned
    {
        *requested = Some(session);
    }
    let spawn_lsn = prefix.recovery_append(spawned);
    let end_lsn = end(&mut prefix, &binding);
    let required = RecoveryRequired {
        binding,
        origin: RecoveryOrigin::ThisBarrier,
        reason: RecoveryReason::UnconfirmedContext,
        boundary: RecoveryBoundary::Unconfirmed {
            spawn_lsn,
            end_lsn,
            requested_confirmation_lsn: confirmed,
        },
        predecessor: None,
    };
    (prefix, required)
}

#[test]
fn held_vanished_and_unconfirmed_required_records_use_actual_sources_in_all_folds() {
    for scenario in ["held", "vanished", "unconfirmed"] {
        let (mut prefix, required) = match scenario {
            "held" => held_required(),
            "vanished" => vanished_required(),
            "unconfirmed" => unconfirmed_required(),
            _ => unreachable!(),
        };
        let mut wrong = required.clone();
        match &mut wrong.boundary {
            RecoveryBoundary::Held { end_lsn, .. }
            | RecoveryBoundary::Vanished { end_lsn, .. }
            | RecoveryBoundary::Unconfirmed { end_lsn, .. } => {
                *end_lsn = required.binding.issue_lsn;
            }
            _ => unreachable!(),
        }
        refuse_recovery(
            &mut prefix,
            Record::RecoveryRequired(wrong),
            RecoveryStateError::Binding,
        );
        assert!(
            prefix
                .world
                .recovery_projection()
                .unwrap()
                .0
                .current(required.binding.work)
                .is_none()
        );
        let barrier_lsn = prefix.recovery_append(Record::RecoveryRequired(required.clone()));
        let episode = prefix
            .world
            .recovery_projection()
            .unwrap()
            .0
            .current(required.binding.work)
            .unwrap();
        assert_eq!(episode.binding, required.binding, "{scenario}");
        assert_eq!(episode.reason, required.reason, "{scenario}");
        assert_eq!(episode.barrier_lsn, barrier_lsn, "{scenario}");
        let origin = match required.origin {
            RecoveryOrigin::ThisBarrier => barrier_lsn,
            RecoveryOrigin::Existing { lsn } => lsn,
        };
        assert_eq!(episode.id.origin, origin, "{scenario}");
        prefix.assert_recovery_folds();
    }
}
