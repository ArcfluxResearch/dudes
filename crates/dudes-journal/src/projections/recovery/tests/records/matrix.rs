use super::*;
use dudes_proto::recovery::RecoveryWork;
use dudes_types::{AgentId, GoalId};

fn issue(fixture: &mut Fixture, work: RecoveryWork) -> Lsn {
    let mut issued = goal_entries().1;
    if let Record::TurnIssued {
        agent, goal, turn, ..
    } = &mut issued.record
    {
        *agent = work.agent;
        *goal = work.goal;
        *turn = work.turn;
    }
    let decision = append(fixture, decision_for(&issued).record);
    if let Record::TurnIssued {
        lsn_of_decision, ..
    } = &mut issued.record
    {
        *lsn_of_decision = decision;
    }
    append(fixture, issued.record)
}

fn other_owner(fixture: &mut Fixture) -> RecoveryWork {
    let work = RecoveryWork {
        agent: AgentId::from_parts(75, 1),
        goal: GoalId::from_parts(75, 2),
        turn: TurnId::from_parts(75, 3),
    };
    let mut hired = entry(RecordKind::AgentHired).record;
    if let Record::AgentHired { id, name, .. } = &mut hired {
        *id = work.agent;
        *name = "independent-issued-owner".into();
    }
    append(fixture, hired);
    let mut installed = goal_entries().0.record;
    if let Record::GoalSet { agent, goal, .. } = &mut installed {
        *agent = work.agent;
        goal.id = work.goal;
    }
    append(fixture, installed);
    issue(fixture, work);
    work
}

fn collision_prefix(same_agent: bool) -> (Fixture, RecoveryAuthorized, RecoveryWork) {
    let mut fixture = Fixture::new(0);
    let (installed, _, mut binding) = goal_entries();
    binding.hired_lsn = fixture
        .projections
        .agent(binding.work.agent)
        .unwrap()
        .unwrap()
        .hired_lsn;
    append(&mut fixture, installed.record);
    let old = if same_agent {
        let work = RecoveryWork {
            turn: TurnId::from_parts(75, 4),
            ..binding.work
        };
        let mut previous = binding.clone();
        previous.work = work;
        previous.issue_lsn = issue(&mut fixture, work);
        append(&mut fixture, spawn(&previous, work.turn, false));
        append(
            &mut fixture,
            terminal(&previous, dudes_proto::TurnOutcome::NoResult),
        );
        work
    } else {
        other_owner(&mut fixture)
    };
    binding.issue_lsn = issue(&mut fixture, binding.work);
    append(&mut fixture, spawn(&binding, binding.work.turn, false));
    let end_lsn = append(
        &mut fixture,
        terminal(&binding, dudes_proto::TurnOutcome::NoResult),
    );
    let origin = append(
        &mut fixture,
        Record::Pause {
            agent: binding.work.agent,
            browser: None,
        },
    );
    append(
        &mut fixture,
        Record::RecoveryRequired(RecoveryRequired {
            binding: binding.clone(),
            origin: RecoveryOrigin::Existing { lsn: origin },
            reason: RecoveryReason::HeldOrPausedFailure,
            boundary: RecoveryBoundary::Paused { end_lsn },
            predecessor: None,
        }),
    );
    let resumed = append(
        &mut fixture,
        Record::Resume {
            agent: binding.work.agent,
            browser: None,
        },
    );
    let authorization = RecoveryAuthorized {
        episode: RecoveryEpisodeId {
            work: binding.work,
            origin,
        },
        binding,
        request_id: request(),
        request_digest: digest(),
        attribution: RecoveryAttribution::LocalOperator {
            pid: 42,
            uid: Some(501),
        },
        expected_control: Some(resumed),
        resulting_control: Some(resumed),
        resume_lsn: None,
        plan: RecoveryPlan::New {
            turn: TurnId::from_parts(75, 5),
            context: RecoveryContext::Fresh {
                session: None,
                recap: RecoveryRecap::default(),
            },
        },
    };
    assert_canonical(&mut fixture);
    (fixture, authorization, old)
}

#[test]
fn new_authority_refuses_existing_ordinary_turns_in_preparation_and_append() {
    let mut observed = Vec::new();
    for same_agent in [false, true] {
        for collision in [false, true] {
            let (mut fixture, mut authorization, old) = collision_prefix(same_agent);
            if collision {
                let RecoveryPlan::New { turn, .. } = &mut authorization.plan else {
                    unreachable!()
                };
                *turn = old.turn;
            }
            let export = fixture.projections.export().unwrap();
            let bytes = journal_inventory(&fixture);
            let old_row =
                serde_json::to_value(fixture.projections.turn(old.turn).unwrap()).unwrap();
            let accounts = serde_json::to_value(
                fixture
                    .projections
                    .goal(authorization.binding.work.goal)
                    .unwrap(),
            )
            .unwrap();
            let prepared = match fixture
                .writer
                .checked_recovery(&mut fixture.projections)
                .unwrap()
                .prepare_transition(RecoveryTransition::Authorized(&authorization))
            {
                Ok(()) => None,
                Err(Error::RecoveryState(reason)) => Some(reason),
                Err(other) => panic!("unexpected preparation error: {other}"),
            };
            assert_eq!(fixture.projections.export().unwrap(), export);
            assert_eq!(journal_inventory(&fixture), bytes);
            let appended = match fixture
                .writer
                .checked_recovery(&mut fixture.projections)
                .unwrap()
                .append(&Entry {
                    at: Timestamp::UNIX_EPOCH,
                    record: Record::RecoveryAuthorized(Box::new(authorization.clone())),
                }) {
                Ok(_) => None,
                Err(Error::RecoveryAdmission {
                    kind: RecordKind::RecoveryAuthorized,
                    reason,
                }) => Some(reason),
                Err(other) => panic!("unexpected append error: {other}"),
            };
            observed.push((
                same_agent,
                collision,
                prepared,
                appended,
                fixture.projections.export().unwrap() == export,
                journal_inventory(&fixture) == bytes,
                serde_json::to_value(fixture.projections.turn(old.turn).unwrap()).unwrap()
                    == old_row,
                serde_json::to_value(
                    fixture
                        .projections
                        .goal(authorization.binding.work.goal)
                        .unwrap(),
                )
                .unwrap()
                    == accounts,
            ));
            assert_canonical(&mut fixture);
        }
    }
    for (
        same_agent,
        collision,
        prepared,
        appended,
        rows_same,
        journal_same,
        old_same,
        accounts_same,
    ) in observed
    {
        let expected = collision.then_some(RecoveryStateError::Binding);
        assert_eq!(
            (
                prepared,
                appended,
                rows_same,
                journal_same,
                old_same,
                accounts_same
            ),
            (expected, expected, collision, collision, true, true),
            "same_agent={same_agent}, collision={collision}"
        );
    }
}
