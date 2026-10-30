use super::*;

fn drive(harness: &mut Harness, input: Input) {
    let mut pending = std::collections::VecDeque::from([input]);
    for _ in 0..96 {
        let Some(input) = pending.pop_front() else {
            return;
        };
        for seen in harness.step(input) {
            if let Some(input) = harness.follow_up(seen) {
                pending.push_back(input);
            }
        }
    }
    panic!("bounded replay fixture did not settle");
}

fn ended_scope_and_ready_neighbor() -> (Harness, AgentId, GoalId, RepositoryTurnGrant, AgentId) {
    let (mut harness, agent, goal, grant) = fixture();
    drive(
        &mut harness,
        Input::Spend {
            agent,
            view: crate::SpendView::default(),
        },
    );
    drive(&mut harness, Input::Started);
    let turn = harness.supervisor.world().goals[&goal].last_turn.unwrap();
    drive(
        &mut harness,
        Input::ChildExited {
            report: dudes_proto::TurnReport::default(),
            turn,
            elapsed_ms: None,
            exit: dudes_proto::ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: None,
            model_usage: std::collections::BTreeMap::new(),
            outcome: dudes_proto::TurnOutcome::Completed,
            last_message: Some("fake process seam".into()),
            final_reply: None,
        },
    );
    assert!(harness.supervisor.world().turns[&turn].has_ended());
    let other = AgentId::from_parts(20, 1);
    let mut neighbor = Harness::new();
    neighbor.hire();
    let mut hired = neighbor
        .records()
        .find(|record| matches!(record, Record::AgentHired { .. }))
        .cloned()
        .unwrap();
    let Record::AgentHired { id, .. } = &mut hired else {
        unreachable!()
    };
    *id = other;
    append(&mut harness, hired);
    append(
        &mut harness,
        Record::GoalSet {
            agent: other,
            goal: Goal {
                id: GoalId::from_parts(7, 99),
                text: "unrelated ready work".into(),
                predicate: Predicate::Command {
                    argv: vec!["/usr/bin/false".into()],
                    timeout_s: 1,
                },
                baseline: None,
                completion_check: None,
                budget: GoalBudget {
                    usd: None,
                    tokens: None,
                },
                hop_cap: 4,
            },
            browser_onboarding: None,
            browser_goal: None,
            goal_replacement: None,
        },
    );
    (harness, agent, goal, grant, other)
}

fn replay(harness: &Harness, agent: AgentId, grant: &RepositoryTurnGrant) -> Harness {
    let mut restarted = Harness::from_prefix(harness.journal.clone());
    restarted
        .supervisor
        .observe_repository_admission(crate::RepositoryAdmission::new([(
            agent,
            grant.hired_lsn,
            true,
        )]));
    restarted
}

fn predicate_result(harness: &mut Harness, goal: GoalId) {
    let Predicate::Command { argv, .. } = harness.supervisor.world().goals[&goal]
        .goal
        .predicate
        .clone()
    else {
        unreachable!()
    };
    drive(
        harness,
        Input::PredicateDone {
            goal,
            pass: true,
            evidence: dudes_proto::Evidence::Command {
                argv,
                program: Some("/usr/bin/true".into()),
                exit: Some(0),
                output_tail: "fake predicate seam".into(),
            },
        },
    );
}

fn stamp_request(
    harness: &Harness,
) -> (
    Box<crate::effect::RepositoryCapture>,
    Lsn,
    Box<dudes_proto::repository_review::RepositoryBaselineBatch>,
    Lsn,
) {
    harness
        .trace
        .iter()
        .rev()
        .find_map(|seen| match seen {
            Seen::CaptureRepositoryStamp {
                capture,
                baseline_lsn,
                baseline,
                check_lsn,
            } => Some((capture.clone(), *baseline_lsn, baseline.clone(), *check_lsn)),
            _ => None,
        })
        .expect("the exact recorded check requests its current stamp")
}

#[test]
fn an_unavailable_scoped_stamp_after_restart_holds_its_goal_and_releases_other_work() {
    let (prefix, agent, goal, grant, other) = ended_scope_and_ready_neighbor();
    for admission_lost in [false, true] {
        let mut restarted = replay(&prefix, agent, &grant);
        drive(&mut restarted, Input::Started);
        assert!(
            restarted
                .trace
                .iter()
                .any(|seen| matches!(seen, Seen::RunPredicate { goal: found } if *found == goal))
        );
        predicate_result(&mut restarted, goal);
        let (capture, baseline_lsn, baseline, check_lsn) = stamp_request(&restarted);
        let mut stamp = complete_review_stamp(&capture, baseline_lsn, &baseline, check_lsn);
        let expected = if admission_lost {
            restarted
                .supervisor
                .observe_repository_admission(crate::RepositoryAdmission::new([(
                    agent,
                    grant.hired_lsn,
                    false,
                )]));
            dudes_proto::repository_review::RepositoryReviewReadiness::MissingStamp
        } else {
            stamp.members[0].outcome =
                dudes_proto::repository_review::RepositoryStampOutcome::Unavailable {
                    reason: dudes_proto::DiffRefusal::Unreadable,
                };
            dudes_proto::repository_review::RepositoryReviewReadiness::UnknownStamp
        };
        drive(
            &mut restarted,
            Input::RepositoryStampCaptured { capture, stamp },
        );
        assert_eq!(
            restarted
                .supervisor
                .world()
                .repository_review(goal)
                .unwrap()
                .readiness(),
            expected
        );
        assert!(
            !restarted.supervisor.instructions.recovering(),
            "global instruction recovery must finish while this ticket stays held"
        );
        assert!(
            restarted.supervisor.world().agents[&other]
                .open_turn
                .is_some(),
            "the unrelated ready agent must progress"
        );
        assert!(
            restarted
                .supervisor
                .world()
                .completed_goal_lsn(goal)
                .is_none()
        );
    }
}

#[test]
fn a_crash_after_the_scoped_predicate_reconstructs_only_its_missing_current_stamp() {
    let (prefix, agent, goal, grant, other) = ended_scope_and_ready_neighbor();
    let mut checking = replay(&prefix, agent, &grant);
    drive(&mut checking, Input::Started);
    predicate_result(&mut checking, goal);
    let (expected, baseline_lsn, baseline, check_lsn) = stamp_request(&checking);
    assert!(matches!(
        checking.journal.last().unwrap().record,
        Record::PredicateResult { .. }
    ));
    for complete in [false, true] {
        let mut restarted = replay(&checking, agent, &grant);
        drive(&mut restarted, Input::Started);
        let (capture, found_baseline_lsn, found_baseline, found_check_lsn) =
            stamp_request(&restarted);
        assert_eq!(
            (
                capture.as_ref(),
                found_baseline_lsn,
                found_baseline.as_ref(),
                found_check_lsn
            ),
            (
                expected.as_ref(),
                baseline_lsn,
                baseline.as_ref(),
                check_lsn
            )
        );
        assert!(
            !restarted
                .trace
                .iter()
                .any(|seen| matches!(seen, Seen::RunPredicate { goal: found } if *found == goal))
        );
        assert!(
            restarted
                .supervisor
                .world()
                .completed_goal_lsn(goal)
                .is_none()
        );
        let requests = restarted
            .trace
            .iter()
            .filter(|seen| matches!(seen, Seen::CaptureRepositoryStamp { .. }))
            .count();
        resume_and_tick(&mut restarted, agent);
        assert_eq!(
            requests,
            restarted
                .trace
                .iter()
                .filter(|seen| matches!(seen, Seen::CaptureRepositoryStamp { .. }))
                .count()
        );
        let mut stamp = complete_review_stamp(&capture, baseline_lsn, &baseline, check_lsn);
        if !complete {
            stamp.members[0].outcome =
                dudes_proto::repository_review::RepositoryStampOutcome::Unavailable {
                    reason: dudes_proto::DiffRefusal::Unreadable,
                };
        }
        drive(
            &mut restarted,
            Input::RepositoryStampCaptured { capture, stamp },
        );
        assert!(!restarted.supervisor.instructions.recovering());
        assert!(
            restarted.supervisor.world().agents[&other]
                .open_turn
                .is_some()
        );
        assert_eq!(
            restarted
                .supervisor
                .world()
                .completed_goal_lsn(goal)
                .is_some(),
            complete
        );
    }
}

#[test]
fn a_scoped_check_refused_at_restart_does_not_hold_unrelated_work() {
    let (prefix, agent, goal, grant, other) = ended_scope_and_ready_neighbor();
    let mut restarted = replay(&prefix, agent, &grant);
    restarted
        .supervisor
        .observe_repository_admission(crate::RepositoryAdmission::new([(
            agent,
            grant.hired_lsn,
            false,
        )]));
    drive(&mut restarted, Input::Started);
    assert!(!restarted.supervisor.instructions.recovering());
    assert!(
        restarted.supervisor.world().agents[&other]
            .open_turn
            .is_some()
    );
    assert!(!restarted.records().any(
        |record| matches!(record, Record::PredicateResult { goal: found, .. } if *found == goal)
    ));
    assert!(
        restarted
            .supervisor
            .world()
            .completed_goal_lsn(goal)
            .is_none()
    );
}
