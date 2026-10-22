use super::*;
use crate::diff_stamp::TicketBinding;
use crate::recovery::{RecoveryRecap, RecoveryRequired};
use dudes_types::{AgentId, CardId, GoalId, ModelSelection, Provider, WorktreeId};

fn work() -> RecoveryWork {
    RecoveryWork {
        agent: AgentId::from_parts(1, 1),
        goal: GoalId::from_parts(1, 2),
        turn: TurnId::from_parts(1, 3),
    }
}

fn facts() -> RecoveryFacts {
    let work = work();
    let selection = Some(ModelSelection::named("recorded-model"));
    RecoveryFacts {
        attempt: super::RecoveryAttempt::default(),
        through: Lsn(100),
        binding: RecoveryBinding {
            work,
            issue_lsn: Lsn(20),
            hired_lsn: Lsn(10),
            ticket: Some(TicketBinding {
                card: CardId::from_parts(1, 4),
                round: 2,
                goal: work.goal,
                claimant: work.agent,
            }),
            worktree: Some(WorktreeId::from_bytes([5; 16])),
            provider: Provider::Codex,
            model_selection: selection.clone(),
        },
        source_issue: RecoveryIssue {
            lsn: Lsn(20),
            issued: RecoveryIssued {
                work,
                task_digest: Hex32::new([6; 32]),
                answers: vec![Lsn(15), Lsn(18)],
                model_selection: selection,
                session: None,
            },
        },
        control_lsn: Some(Lsn(90)),
        control: crate::recovery_state::history::RecoveryStandingControl::default(),
        confirmed_session: Some(SessionId::new("recorded-conversation")),
    }
}

fn required(facts: &RecoveryFacts, reason: RecoveryReason) -> RecoveryRequired {
    RecoveryRequired {
        binding: facts.binding.clone(),
        origin: if reason == RecoveryReason::HeldOrPausedFailure {
            RecoveryOrigin::Existing { lsn: Lsn(90) }
        } else {
            RecoveryOrigin::ThisBarrier
        },
        reason,
        boundary: match reason {
            RecoveryReason::HeldOrPausedFailure => {
                crate::recovery::RecoveryBoundary::Paused { end_lsn: Lsn(80) }
            }
            RecoveryReason::Vanished => crate::recovery::RecoveryBoundary::Vanished {
                spawn_lsn: Lsn(60),
                end_lsn: Lsn(80),
            },
            RecoveryReason::UnconfirmedContext => crate::recovery::RecoveryBoundary::Unconfirmed {
                spawn_lsn: Lsn(60),
                end_lsn: Lsn(80),
                requested_confirmation_lsn: Lsn(50),
            },
            RecoveryReason::MissingSpawn => crate::recovery::RecoveryBoundary::MissingSpawn,
        },
        predecessor: None,
    }
}

fn new_authorization(episode: &RecoveryEpisode, facts: &RecoveryFacts) -> RecoveryAuthorized {
    RecoveryAuthorized {
        episode: episode.id,
        binding: episode.binding.clone(),
        request_id: Hex32::new([7; 32]),
        request_digest: Hex32::new([8; 32]),
        attribution: crate::recovery::RecoveryAttribution::LocalOperator { pid: 42, uid: None },
        expected_control: facts.control_lsn,
        resulting_control: facts.control_lsn,
        resume_lsn: None,
        plan: if episode.reason == RecoveryReason::MissingSpawn {
            RecoveryPlan::RepeatIssued {
                issue: facts.source_issue.clone(),
                session: None,
            }
        } else {
            RecoveryPlan::New {
                turn: TurnId::from_parts(1, 9),
                context: RecoveryContext::Fresh {
                    session: None,
                    recap: RecoveryRecap {
                        text: "Observed failure; earlier task effects are unknown.".into(),
                        sources: vec![Lsn(20), Lsn(80)],
                        delivered_answers: Vec::new(),
                    },
                },
            }
        },
    }
}

fn barrier(reason: RecoveryReason) -> (RecoveryState, RecoveryFacts, RecoveryAuthorized) {
    let mut state = RecoveryState::default();
    let mut facts = facts();
    let required = required(&facts, reason);
    assert_eq!(
        state.validate_next(RecoveryEvent::Required(&required), &facts),
        Ok(())
    );
    state
        .apply(Lsn(200), RecoveryEvent::Required(&required), &facts)
        .unwrap();
    facts.through = Lsn(200);
    let authorization = new_authorization(state.current(work()).unwrap(), &facts);
    (state, facts, authorization)
}

#[test]
fn ticket_history_keeps_exact_binding_and_reports_a_scan_limit() {
    let (mut state, mut facts, authorization) = barrier(RecoveryReason::Vanished);
    authorize(&mut state, &mut facts, &authorization);
    let binding = facts.binding.ticket.unwrap();
    let (rows, truncated) = state.ticket_episodes(binding, 8);
    assert!(!truncated);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].authorization_lsn, Some(Lsn(300)));
    let mut wrong = binding;
    wrong.round += 1;
    assert!(state.ticket_episodes(wrong, 8).0.is_empty());
    assert!(state.ticket_episodes(binding, 0).1);
    assert_eq!(
        state
            .receipt_by_request(authorization.request_id)
            .unwrap()
            .unwrap()
            .claim
            .authorization_lsn,
        Lsn(300)
    );
}

#[test]
fn ticket_history_reports_incomplete_traversal_without_changing_the_state() {
    let mut state = RecoveryState::default();
    let mut facts = facts();
    let binding = facts.binding.ticket.unwrap();
    for index in 0..513 {
        facts.binding.work.turn = TurnId::from_parts(9, index);
        facts.source_issue.issued.work = facts.binding.work;
        facts.through = Lsn(199 + u64::try_from(index).unwrap());
        let boundary = required(&facts, RecoveryReason::Vanished);
        state
            .apply(
                Lsn(facts.through.0 + 1),
                RecoveryEvent::Required(&boundary),
                &facts,
            )
            .unwrap();
    }
    let before = state.clone();
    let (rows, truncated) = state.ticket_episodes(binding, 8);
    assert_eq!(rows.len(), 8);
    assert!(truncated);
    let works = || {
        (0..513).map(|index| RecoveryWork {
            turn: TurnId::from_parts(9, index),
            ..facts.binding.work
        })
    };
    assert_eq!(
        state
            .latest_ticket_episode(binding, works())
            .unwrap()
            .barrier_lsn,
        Lsn(712)
    );
    let mut wrong = binding;
    wrong.round += 1;
    assert!(state.latest_ticket_episode(wrong, works()).is_none());
    assert_eq!(state, before);
}

fn authorize(
    state: &mut RecoveryState,
    facts: &mut RecoveryFacts,
    authorization: &RecoveryAuthorized,
) -> RecoveryClaimRef {
    assert_eq!(
        state.validate_next(RecoveryEvent::Authorized(authorization), facts),
        Ok(())
    );
    state
        .apply(Lsn(300), RecoveryEvent::Authorized(authorization), facts)
        .unwrap();
    facts.through = Lsn(300);
    RecoveryClaimRef {
        episode: authorization.episode,
        authorization_lsn: Lsn(300),
    }
}

fn reject(
    state: &mut RecoveryState,
    facts: &RecoveryFacts,
    at: u64,
    event: RecoveryEvent<'_>,
    expected: RecoveryStateError,
) {
    let before = state.clone();
    assert_eq!(state.validate_next(event, facts), Err(expected));
    assert_eq!(state.apply(Lsn(at), event, facts), Err(expected));
    assert_eq!(*state, before);
}

#[test]
fn a_barrier_retains_the_offered_control_origin_at_its_actual_rotated_position() {
    let mut state = RecoveryState::default();
    let facts = facts();
    let required = required(&facts, RecoveryReason::HeldOrPausedFailure);
    let at = Lsn::from_parts(4, 512).unwrap();
    assert_eq!(
        state.validate_next(RecoveryEvent::Required(&required), &facts),
        Ok(())
    );
    assert!(state.current(work()).is_none());
    state
        .apply(at, RecoveryEvent::Required(&required), &facts)
        .unwrap();
    let episode = state.current(work()).unwrap();
    assert_eq!(
        episode.id,
        RecoveryEpisodeId {
            work: work(),
            origin: Lsn(90)
        }
    );
    assert_eq!(episode.barrier_lsn, at);
    assert_eq!(episode.authorization_lsn, None);
}

#[test]
fn an_automatic_episode_uses_the_actual_barrier_position_without_a_preview_lsn() {
    let mut state = RecoveryState::default();
    let facts = facts();
    let required = required(&facts, RecoveryReason::MissingSpawn);
    assert_eq!(
        state.validate_next(RecoveryEvent::Required(&required), &facts),
        Ok(())
    );
    state
        .apply(Lsn(904), RecoveryEvent::Required(&required), &facts)
        .unwrap();
    assert_eq!(state.current(work()).unwrap().id.origin, Lsn(904));
}

#[test]
fn another_pause_or_hold_cannot_create_an_alias_episode_for_the_same_work() {
    let (mut state, facts, _) = barrier(RecoveryReason::HeldOrPausedFailure);
    let mut alias = required(&facts, RecoveryReason::HeldOrPausedFailure);
    alias.origin = RecoveryOrigin::Existing { lsn: Lsn(95) };
    reject(
        &mut state,
        &facts,
        300,
        RecoveryEvent::Required(&alias),
        RecoveryStateError::EpisodeAlias,
    );
}

#[test]
fn distinct_requests_are_excluded_while_the_first_claim_waits_with_unchanged_control() {
    let (mut state, mut facts, authorization) = barrier(RecoveryReason::HeldOrPausedFailure);
    authorize(&mut state, &mut facts, &authorization);
    assert_eq!(facts.control_lsn, Some(Lsn(90)));
    assert_eq!(
        state
            .receipt(authorization.request_id, authorization.request_digest)
            .unwrap()
            .unwrap()
            .state,
        &RecoveryClaimState::Pending
    );
    let mut other = authorization.clone();
    other.request_id = Hex32::new([10; 32]);
    reject(
        &mut state,
        &facts,
        400,
        RecoveryEvent::Authorized(&other),
        RecoveryStateError::Claimed,
    );
}

#[test]
fn request_lookup_is_inert_and_changed_digest_conflicts_in_every_claim_phase() {
    let (mut state, mut facts, authorization) = barrier(RecoveryReason::MissingSpawn);
    let claim = authorize(&mut state, &mut facts, &authorization);
    let before = state.clone();
    for _ in 0..2 {
        let receipt = state
            .receipt(authorization.request_id, authorization.request_digest)
            .unwrap()
            .unwrap();
        assert_eq!(receipt.claim, claim);
        assert_eq!(receipt.authorization, &authorization);
        assert_eq!(
            receipt.state,
            &RecoveryClaimState::IssueBound(facts.source_issue.clone())
        );
        assert_eq!(
            state.receipt(authorization.request_id, Hex32::new([99; 32])),
            Err(RecoveryStateError::RequestConflict)
        );
        assert_eq!(
            state.receipt(Hex32::new([0; 32]), authorization.request_digest),
            Ok(None)
        );
    }
    assert_eq!(state, before);
    reject(
        &mut state,
        &facts,
        400,
        RecoveryEvent::Authorized(&authorization),
        RecoveryStateError::DuplicateRequest,
    );
}

fn assert_original_receipt(
    state: &RecoveryState,
    claim: RecoveryClaimRef,
    authorization: &RecoveryAuthorized,
    expected: &RecoveryClaimState,
) {
    let before = state.clone();
    for _ in 0..3 {
        let receipt = state
            .receipt_by_request(authorization.request_id)
            .unwrap()
            .unwrap();
        assert_eq!(receipt.claim, claim);
        assert_eq!(receipt.authorization, authorization);
        assert_eq!(receipt.state, expected);
        assert_eq!(
            state.receipt(authorization.request_id, authorization.request_digest),
            Ok(Some(receipt))
        );
        assert_eq!(
            state.receipt(authorization.request_id, Hex32::new([99; 32])),
            Err(RecoveryStateError::RequestConflict)
        );
        assert_eq!(state.receipt_by_request(Hex32::new([0; 32])), Ok(None));
        assert_eq!(
            state.receipt(Hex32::new([0; 32]), authorization.request_digest),
            Ok(None)
        );
    }
    assert_eq!(*state, before);
}

#[test]
fn request_only_receipts_keep_invalidated_intent_after_control_changes() {
    let (mut state, mut facts, authorization) = barrier(RecoveryReason::HeldOrPausedFailure);
    let claim = authorize(&mut state, &mut facts, &authorization);
    let evidence = crate::recovery::RecoveryInvalidationEvidence::RecordedChange {
        reason: crate::recovery::RecoveryInvalidationReason::ControlChanged,
        lsn: Lsn(350),
    };
    facts.through = Lsn(350);
    facts.control_lsn = Some(Lsn(350));
    state
        .apply(
            Lsn(400),
            RecoveryEvent::Invalidated { claim, evidence },
            &facts,
        )
        .unwrap();
    assert_original_receipt(
        &state,
        claim,
        &authorization,
        &RecoveryClaimState::Invalidated {
            invalidation_lsn: Lsn(400),
            evidence,
        },
    );
}

#[test]
fn request_only_receipts_keep_original_intent_through_issue_consumption_and_supersession() {
    let (mut state, mut facts, authorization) = barrier(RecoveryReason::HeldOrPausedFailure);
    let claim = authorize(&mut state, &mut facts, &authorization);
    assert_original_receipt(&state, claim, &authorization, &RecoveryClaimState::Pending);

    let mut issued = facts.source_issue.issued.clone();
    issued.work.turn = TurnId::from_parts(1, 9);
    issued.task_digest = Hex32::new([11; 32]);
    issued.answers.push(Lsn(250));
    state
        .apply(
            Lsn(400),
            RecoveryEvent::Issued {
                claim,
                issued: &issued,
            },
            &facts,
        )
        .unwrap();
    facts.through = Lsn(400);
    let issue = RecoveryIssue {
        lsn: Lsn(400),
        issued,
    };
    assert_original_receipt(
        &state,
        claim,
        &authorization,
        &RecoveryClaimState::IssueBound(issue.clone()),
    );

    let mut consumed = state.clone();
    consumed
        .apply(
            Lsn(500),
            RecoveryEvent::Consumed {
                claim,
                issue: &issue,
                session: None,
            },
            &facts,
        )
        .unwrap();
    assert_original_receipt(
        &consumed,
        claim,
        &authorization,
        &RecoveryClaimState::Consumed {
            issue: issue.clone(),
            evidence_lsn: Lsn(500),
        },
    );

    facts.binding.work = issue.issued.work;
    facts.binding.issue_lsn = issue.lsn;
    facts.source_issue = issue.clone();
    let mut next = required(&facts, RecoveryReason::MissingSpawn);
    next.predecessor = Some(claim);
    state
        .apply(Lsn(500), RecoveryEvent::Required(&next), &facts)
        .unwrap();
    facts.through = Lsn(500);
    let successor = state.current(facts.binding.work).unwrap().id;
    let expected = RecoveryClaimState::Superseded {
        issue,
        successor,
        barrier_lsn: Lsn(500),
    };
    assert_original_receipt(&state, claim, &authorization, &expected);
    let mut replacement = new_authorization(state.current(facts.binding.work).unwrap(), &facts);
    replacement.request_id = Hex32::new([13; 32]);
    replacement.request_digest = Hex32::new([14; 32]);
    replacement.attribution = crate::recovery::RecoveryAttribution::LocalOperator {
        pid: 99,
        uid: Some(501),
    };
    state
        .apply(Lsn(600), RecoveryEvent::Authorized(&replacement), &facts)
        .unwrap();
    assert_original_receipt(&state, claim, &authorization, &expected);
    assert_eq!(
        state
            .receipt_by_request(replacement.request_id)
            .unwrap()
            .unwrap()
            .authorization,
        &replacement
    );
}

#[test]
fn new_work_binds_only_the_planned_turn_model_and_context_then_consumes_once() {
    let (mut state, mut facts, authorization) = barrier(RecoveryReason::HeldOrPausedFailure);
    let claim = authorize(&mut state, &mut facts, &authorization);
    let mut issued = facts.source_issue.issued.clone();
    issued.work.turn = TurnId::from_parts(1, 9);
    issued.answers = vec![Lsn(15), Lsn(18), Lsn(250)];
    issued.task_digest = Hex32::new([11; 32]);
    let mut wrong = issued.clone();
    wrong.model_selection = None;
    reject(
        &mut state,
        &facts,
        400,
        RecoveryEvent::Issued {
            claim,
            issued: &wrong,
        },
        RecoveryStateError::Binding,
    );
    wrong = issued.clone();
    wrong.session = Some(SessionId::new("other-context"));
    reject(
        &mut state,
        &facts,
        400,
        RecoveryEvent::Issued {
            claim,
            issued: &wrong,
        },
        RecoveryStateError::Binding,
    );
    state
        .apply(
            Lsn(400),
            RecoveryEvent::Issued {
                claim,
                issued: &issued,
            },
            &facts,
        )
        .unwrap();
    facts.through = Lsn(400);
    let issue = RecoveryIssue {
        lsn: Lsn(400),
        issued,
    };
    state
        .apply(
            Lsn(500),
            RecoveryEvent::Consumed {
                claim,
                issue: &issue,
                session: None,
            },
            &facts,
        )
        .unwrap();
    facts.through = Lsn(500);
    assert_eq!(
        state
            .receipt(authorization.request_id, authorization.request_digest)
            .unwrap()
            .unwrap()
            .state,
        &RecoveryClaimState::Consumed {
            issue: issue.clone(),
            evidence_lsn: Lsn(500)
        }
    );
    reject(
        &mut state,
        &facts,
        600,
        RecoveryEvent::Consumed {
            claim,
            issue: &issue,
            session: None,
        },
        RecoveryStateError::Lifecycle,
    );
    let mut other = authorization.clone();
    other.request_id = Hex32::new([12; 32]);
    reject(
        &mut state,
        &facts,
        600,
        RecoveryEvent::Authorized(&other),
        RecoveryStateError::Claimed,
    );
}

#[test]
fn changed_control_cannot_admit_a_pending_authorization() {
    let (mut state, mut facts, authorization) = barrier(RecoveryReason::HeldOrPausedFailure);
    let claim = authorize(&mut state, &mut facts, &authorization);
    let mut issued = facts.source_issue.issued.clone();
    issued.work.turn = TurnId::from_parts(1, 9);
    facts.control_lsn = Some(Lsn(350));
    facts.through = Lsn(350);
    reject(
        &mut state,
        &facts,
        400,
        RecoveryEvent::Issued {
            claim,
            issued: &issued,
        },
        RecoveryStateError::Control,
    );
}

#[test]
fn lost_recorded_confirmation_cannot_admit_a_pending_authorization() {
    let (mut state, mut facts, mut authorization) = barrier(RecoveryReason::HeldOrPausedFailure);
    let session = facts.confirmed_session.clone().unwrap();
    authorization.plan = RecoveryPlan::New {
        turn: TurnId::from_parts(1, 9),
        context: RecoveryContext::TryRecorded {
            session: session.clone(),
        },
    };
    let claim = authorize(&mut state, &mut facts, &authorization);
    let mut issued = facts.source_issue.issued.clone();
    issued.work.turn = TurnId::from_parts(1, 9);
    issued.session = Some(session);
    facts.confirmed_session = None;
    reject(
        &mut state,
        &facts,
        400,
        RecoveryEvent::Issued {
            claim,
            issued: &issued,
        },
        RecoveryStateError::Context,
    );
}

#[test]
fn owed_work_cannot_choose_new_work_or_change_its_task_witness_or_answers() {
    let (mut state, facts, authorization) = barrier(RecoveryReason::MissingSpawn);
    let mut changed = authorization.clone();
    changed.plan = RecoveryPlan::New {
        turn: TurnId::from_parts(1, 9),
        context: RecoveryContext::TryRecorded {
            session: facts.confirmed_session.clone().unwrap(),
        },
    };
    reject(
        &mut state,
        &facts,
        300,
        RecoveryEvent::Authorized(&changed),
        RecoveryStateError::Context,
    );
    for change_task in [true, false] {
        changed = authorization.clone();
        let RecoveryPlan::RepeatIssued { issue, .. } = &mut changed.plan else {
            unreachable!()
        };
        if change_task {
            issue.issued.task_digest = Hex32::new([99; 32]);
        } else {
            issue.issued.answers.push(Lsn(19));
        }
        reject(
            &mut state,
            &facts,
            300,
            RecoveryEvent::Authorized(&changed),
            RecoveryStateError::Context,
        );
    }
}

#[test]
fn an_owed_repeat_uses_its_new_launch_announcement_instead_of_the_old_issue_session() {
    let (mut state, mut facts, mut authorization) = barrier(RecoveryReason::MissingSpawn);
    facts.source_issue.issued.session = Some(SessionId::new("old-announcement"));
    authorization.plan = RecoveryPlan::RepeatIssued {
        issue: facts.source_issue.clone(),
        session: None,
    };
    let claim = authorize(&mut state, &mut facts, &authorization);
    reject(
        &mut state,
        &facts,
        400,
        RecoveryEvent::Consumed {
            claim,
            issue: &facts.source_issue,
            session: facts.source_issue.issued.session.as_ref(),
        },
        RecoveryStateError::Context,
    );
    state
        .apply(
            Lsn(400),
            RecoveryEvent::Consumed {
                claim,
                issue: &facts.source_issue,
                session: None,
            },
            &facts,
        )
        .unwrap();
}

#[test]
fn a_missing_spawn_atomically_supersedes_old_context_consent_and_keeps_owed_work() {
    let (mut state, mut facts, authorization) = barrier(RecoveryReason::HeldOrPausedFailure);
    let claim = authorize(&mut state, &mut facts, &authorization);
    let mut issued = facts.source_issue.issued.clone();
    issued.work.turn = TurnId::from_parts(1, 9);
    state
        .apply(
            Lsn(400),
            RecoveryEvent::Issued {
                claim,
                issued: &issued,
            },
            &facts,
        )
        .unwrap();
    facts.through = Lsn(400);
    facts.binding.work = issued.work;
    facts.binding.issue_lsn = Lsn(400);
    facts.source_issue = RecoveryIssue {
        lsn: Lsn(400),
        issued,
    };
    let mut next = required(&facts, RecoveryReason::MissingSpawn);
    next.predecessor = Some(claim);
    let before = state.clone();
    assert_eq!(
        state.validate_next(RecoveryEvent::Required(&next), &facts),
        Ok(())
    );
    assert_eq!(state, before);
    state
        .apply(Lsn(500), RecoveryEvent::Required(&next), &facts)
        .unwrap();
    facts.through = Lsn(500);
    let episode = state.current(facts.binding.work).unwrap();
    assert_eq!(episode.id.origin, Lsn(500));
    assert_eq!(episode.authorization_lsn, None);
    assert_eq!(
        state
            .receipt(authorization.request_id, authorization.request_digest)
            .unwrap()
            .unwrap()
            .state,
        &RecoveryClaimState::Superseded {
            issue: facts.source_issue.clone(),
            successor: episode.id,
            barrier_lsn: Lsn(500)
        }
    );
    let mut repeat = new_authorization(episode, &facts);
    repeat.request_id = Hex32::new([13; 32]);
    state
        .apply(Lsn(600), RecoveryEvent::Authorized(&repeat), &facts)
        .unwrap();
    facts.through = Lsn(600);
    reject(
        &mut state,
        &facts,
        700,
        RecoveryEvent::Issued {
            claim,
            issued: &facts.source_issue.issued,
        },
        RecoveryStateError::Lifecycle,
    );
    assert_eq!(facts.source_issue.issued.answers, vec![Lsn(15), Lsn(18)]);
    let repeat_claim = RecoveryClaimRef {
        episode: repeat.episode,
        authorization_lsn: Lsn(600),
    };
    let mut again = required(&facts, RecoveryReason::MissingSpawn);
    again.predecessor = Some(repeat_claim);
    state
        .apply(Lsn(700), RecoveryEvent::Required(&again), &facts)
        .unwrap();
    let successor = state.current(facts.binding.work).unwrap();
    assert_eq!(successor.id.origin, Lsn(700));
    assert_eq!(successor.binding.issue_lsn, Lsn(400));
    assert_eq!(
        state
            .receipt(repeat.request_id, repeat.request_digest)
            .unwrap()
            .unwrap()
            .state,
        &RecoveryClaimState::Superseded {
            issue: facts.source_issue.clone(),
            successor: successor.id,
            barrier_lsn: Lsn(700),
        }
    );
}

#[test]
fn missing_predecessor_cannot_open_a_second_episode_for_a_recovery_issue() {
    let (mut state, mut facts, authorization) = barrier(RecoveryReason::HeldOrPausedFailure);
    let claim = authorize(&mut state, &mut facts, &authorization);
    let mut issued = facts.source_issue.issued.clone();
    issued.work.turn = TurnId::from_parts(1, 9);
    state
        .apply(
            Lsn(400),
            RecoveryEvent::Issued {
                claim,
                issued: &issued,
            },
            &facts,
        )
        .unwrap();
    facts.through = Lsn(400);
    facts.binding.work = issued.work;
    facts.binding.issue_lsn = Lsn(400);
    facts.source_issue = RecoveryIssue {
        lsn: Lsn(400),
        issued,
    };
    let next = required(&facts, RecoveryReason::MissingSpawn);
    reject(
        &mut state,
        &facts,
        500,
        RecoveryEvent::Required(&next),
        RecoveryStateError::Binding,
    );
}

#[test]
fn invalidation_keeps_the_old_planned_turn_unusable_and_never_rearms_its_episode() {
    let (mut state, mut facts, authorization) = barrier(RecoveryReason::HeldOrPausedFailure);
    let claim = authorize(&mut state, &mut facts, &authorization);
    facts.through = Lsn(350);
    state
        .apply(
            Lsn(400),
            RecoveryEvent::Invalidated {
                claim,
                evidence: crate::recovery::RecoveryInvalidationEvidence::RecordedChange {
                    reason: crate::recovery::RecoveryInvalidationReason::ControlChanged,
                    lsn: Lsn(350),
                },
            },
            &facts,
        )
        .unwrap();
    facts.through = Lsn(400);
    let mut issued = facts.source_issue.issued.clone();
    issued.work.turn = TurnId::from_parts(1, 9);
    reject(
        &mut state,
        &facts,
        500,
        RecoveryEvent::Issued {
            claim,
            issued: &issued,
        },
        RecoveryStateError::Lifecycle,
    );
    let mut changed = authorization.clone();
    changed.request_id = Hex32::new([14; 32]);
    reject(
        &mut state,
        &facts,
        500,
        RecoveryEvent::Authorized(&changed),
        RecoveryStateError::Claimed,
    );
    let mut next = required(&facts, RecoveryReason::HeldOrPausedFailure);
    next.origin = RecoveryOrigin::Existing { lsn: Lsn(450) };
    next.predecessor = Some(claim);
    facts.through = Lsn(450);
    state
        .apply(Lsn(500), RecoveryEvent::Required(&next), &facts)
        .unwrap();
    facts.through = Lsn(500);
    let mut replacement = new_authorization(state.current(work()).unwrap(), &facts);
    replacement.request_id = Hex32::new([15; 32]);
    reject(
        &mut state,
        &facts,
        600,
        RecoveryEvent::Authorized(&replacement),
        RecoveryStateError::PlannedTurn,
    );
    assert_eq!(
        state
            .receipt(authorization.request_id, authorization.request_digest)
            .unwrap()
            .unwrap()
            .state,
        &RecoveryClaimState::Invalidated {
            invalidation_lsn: Lsn(400),
            evidence: crate::recovery::RecoveryInvalidationEvidence::RecordedChange {
                reason: crate::recovery::RecoveryInvalidationReason::ControlChanged,
                lsn: Lsn(350)
            }
        }
    );
}

#[test]
fn current_binding_changes_refuse_before_any_claim_mutation() {
    let (state, facts, authorization) = barrier(RecoveryReason::HeldOrPausedFailure);
    for case in 0..8 {
        let mut state = state.clone();
        let mut facts = facts.clone();
        match case {
            0 => facts.binding.work.goal = GoalId::from_parts(2, 2),
            1 => facts.binding.work.agent = AgentId::from_parts(2, 1),
            2 => facts.binding.ticket.as_mut().unwrap().round += 1,
            3 => facts.binding.worktree = None,
            4 => facts.binding.provider = Provider::Pi,
            5 => facts.binding.model_selection = None,
            6 => facts.binding.hired_lsn = Lsn(11),
            7 => facts.binding.issue_lsn = Lsn(21),
            _ => unreachable!(),
        }
        reject(
            &mut state,
            &facts,
            300,
            RecoveryEvent::Authorized(&authorization),
            RecoveryStateError::Binding,
        );
    }
}

#[test]
fn context_choice_requires_current_confirmation_and_bounded_ordered_recap_sources() {
    let (state, facts, authorization) = barrier(RecoveryReason::UnconfirmedContext);
    let mut recorded = authorization.clone();
    recorded.plan = RecoveryPlan::New {
        turn: TurnId::from_parts(1, 9),
        context: RecoveryContext::TryRecorded {
            session: facts.confirmed_session.clone().unwrap(),
        },
    };
    reject(
        &mut state.clone(),
        &facts,
        300,
        RecoveryEvent::Authorized(&recorded),
        RecoveryStateError::Context,
    );
    for text in ["é".repeat(4_096), "x".repeat(8_192)] {
        let mut authorization = authorization.clone();
        let RecoveryPlan::New {
            context: RecoveryContext::Fresh { recap, .. },
            ..
        } = &mut authorization.plan
        else {
            unreachable!()
        };
        recap.text = text;
        assert_eq!(
            state.validate_next(RecoveryEvent::Authorized(&authorization), &facts),
            Ok(())
        );
        let RecoveryPlan::New {
            context: RecoveryContext::Fresh { recap, .. },
            ..
        } = &mut authorization.plan
        else {
            unreachable!()
        };
        recap.text.push('x');
        reject(
            &mut state.clone(),
            &facts,
            300,
            RecoveryEvent::Authorized(&authorization),
            RecoveryStateError::Context,
        );
    }
    for sources in [
        vec![Lsn::ZERO],
        vec![Lsn(20), Lsn(20)],
        vec![Lsn(80), Lsn(20)],
        vec![Lsn(201)],
    ] {
        let mut authorization = authorization.clone();
        let RecoveryPlan::New {
            context: RecoveryContext::Fresh { recap, .. },
            ..
        } = &mut authorization.plan
        else {
            unreachable!()
        };
        recap.sources = sources;
        reject(
            &mut state.clone(),
            &facts,
            300,
            RecoveryEvent::Authorized(&authorization),
            RecoveryStateError::Context,
        );
    }
}

#[test]
fn only_the_commands_exact_own_resume_can_change_the_reviewed_control_identity() {
    let (mut state, mut facts, mut authorization) = barrier(RecoveryReason::HeldOrPausedFailure);
    facts.through = Lsn(250);
    facts.control_lsn = Some(Lsn(250));
    authorization.resulting_control = facts.control_lsn;
    reject(
        &mut state,
        &facts,
        300,
        RecoveryEvent::Authorized(&authorization),
        RecoveryStateError::Control,
    );
    authorization.resume_lsn = Some(Lsn(250));
    assert_eq!(
        state.validate_next(RecoveryEvent::Authorized(&authorization), &facts),
        Ok(())
    );
    authorization.resume_lsn = Some(Lsn(190));
    authorization.resulting_control = Some(Lsn(190));
    reject(
        &mut state,
        &facts,
        300,
        RecoveryEvent::Authorized(&authorization),
        RecoveryStateError::Control,
    );
}

#[test]
fn new_authority_fields_are_required_even_when_their_value_is_null() {
    let (state, facts, authorization) = barrier(RecoveryReason::HeldOrPausedFailure);
    let value = serde_json::to_value(&authorization).unwrap();
    assert_eq!(
        serde_json::from_value::<RecoveryAuthorized>(value.clone()).unwrap(),
        authorization
    );
    for key in [
        "episode",
        "binding",
        "request_id",
        "request_digest",
        "expected_control",
        "resulting_control",
        "resume_lsn",
        "plan",
    ] {
        let mut value = value.clone();
        value.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<RecoveryAuthorized>(value).is_err(),
            "missing {key}"
        );
    }
    let mut value = value;
    value["binding"].as_object_mut().unwrap().remove("worktree");
    assert!(serde_json::from_value::<RecoveryAuthorized>(value).is_err());
    let mut value = serde_json::to_value(required(&facts, RecoveryReason::MissingSpawn)).unwrap();
    value.as_object_mut().unwrap().remove("predecessor");
    assert!(serde_json::from_value::<RecoveryRequired>(value).is_err());
    assert_eq!(state.current(work()).unwrap().authorization_lsn, None);
}

#[test]
fn a_stale_prefix_or_nonincreasing_actual_position_cannot_add_authority() {
    let (mut state, mut facts, authorization) = barrier(RecoveryReason::HeldOrPausedFailure);
    facts.through = Lsn(199);
    reject(
        &mut state,
        &facts,
        300,
        RecoveryEvent::Authorized(&authorization),
        RecoveryStateError::Position,
    );
    facts.through = Lsn(300);
    let before = state.clone();
    assert_eq!(
        state.apply(Lsn(300), RecoveryEvent::Authorized(&authorization), &facts),
        Err(RecoveryStateError::Position)
    );
    assert_eq!(state, before);
}

#[test]
fn a_control_boundary_cannot_precede_the_interrupted_issue() {
    let mut state = RecoveryState::default();
    let facts = facts();
    let mut required = required(&facts, RecoveryReason::HeldOrPausedFailure);
    required.origin = RecoveryOrigin::Existing { lsn: Lsn(15) };
    reject(
        &mut state,
        &facts,
        200,
        RecoveryEvent::Required(&required),
        RecoveryStateError::Position,
    );
}

#[test]
fn the_recorded_hire_must_precede_the_original_issue() {
    let mut state = RecoveryState::default();
    let mut facts = facts();
    facts.binding.hired_lsn = Lsn(30);
    let required = required(&facts, RecoveryReason::HeldOrPausedFailure);
    reject(
        &mut state,
        &facts,
        200,
        RecoveryEvent::Required(&required),
        RecoveryStateError::Position,
    );
}

#[test]
fn authorization_control_references_must_be_within_the_committed_prefix() {
    let (state, facts, authorization) = barrier(RecoveryReason::HeldOrPausedFailure);
    for invalid in [Lsn(999), Lsn::ZERO] {
        let mut state = state.clone();
        let mut facts = facts.clone();
        let mut authorization = authorization.clone();
        facts.control_lsn = Some(invalid);
        authorization.expected_control = Some(invalid);
        authorization.resulting_control = Some(invalid);
        reject(
            &mut state,
            &facts,
            300,
            RecoveryEvent::Authorized(&authorization),
            RecoveryStateError::Position,
        );
    }
}
