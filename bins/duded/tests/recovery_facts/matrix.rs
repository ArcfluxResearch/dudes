use super::*;
use dudes_proto::record::Hex32;
use dudes_proto::recovery::{
    RecoveryAttribution, RecoveryAuthorized, RecoveryBoundary, RecoveryContext, RecoveryEpisodeId,
    RecoveryOrigin, RecoveryPlan, RecoveryReason, RecoveryRecap, RecoveryRequired,
};
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
