//! Actual required-frame prefixes at the certified journal boundary.

use super::*;
use dudes_proto::recovery::{
    RecoveryAttribution, RecoveryAuthorized, RecoveryBoundary, RecoveryContext, RecoveryEpisodeId,
    RecoveryInvalidated, RecoveryInvalidationEvidence, RecoveryInvalidationReason, RecoveryOrigin,
    RecoveryPlan, RecoveryReason, RecoveryRecap, RecoveryRequired,
};
use dudes_proto::recovery_state::cache::RecoveryCacheError;
use dudes_types::{Mode, Provenance, TurnId};

fn append(fixture: &mut Fixture, record: Record) -> Lsn {
    fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .append(&Entry {
            at: Timestamp::UNIX_EPOCH,
            record,
        })
        .unwrap()
}

fn original(checkpoints: u64) -> (Fixture, RecoveryBinding) {
    let mut fixture = Fixture::new(checkpoints);
    let (installed, mut issued, mut binding) = goal_entries();
    binding.hired_lsn = fixture
        .projections
        .agent(binding.work.agent)
        .unwrap()
        .unwrap()
        .hired_lsn;
    append(&mut fixture, installed.record);
    let decision = append(&mut fixture, decision_for(&issued).record);
    let Record::TurnIssued {
        lsn_of_decision, ..
    } = &mut issued.record
    else {
        unreachable!()
    };
    *lsn_of_decision = decision;
    binding.issue_lsn = append(&mut fixture, issued.record);
    (fixture, binding)
}

fn spawn(binding: &RecoveryBinding, turn: TurnId, reissued: bool) -> Record {
    Record::TurnSpawned {
        agent: binding.work.agent,
        turn,
        pid: 42,
        start_time: 10,
        boot: None,
        session: None,
        reissued,
        mcp_grant: None,
    }
}

fn missing(binding: &RecoveryBinding) -> RecoveryRequired {
    RecoveryRequired {
        binding: binding.clone(),
        origin: RecoveryOrigin::ThisBarrier,
        reason: RecoveryReason::MissingSpawn,
        boundary: RecoveryBoundary::MissingSpawn,
        predecessor: None,
    }
}

fn paused(checkpoints: u64) -> (Fixture, RecoveryRequired, Lsn) {
    let (mut fixture, binding) = original(checkpoints);
    append(&mut fixture, spawn(&binding, binding.work.turn, false));
    let mut ended = entry(RecordKind::TurnEnded).record;
    let Record::TurnEnded {
        agent,
        turn,
        mode,
        provenance,
        outcome,
        session,
        session_usage,
        ..
    } = &mut ended
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
    let end_lsn = append(&mut fixture, ended);
    let pause = append(
        &mut fixture,
        Record::Pause {
            agent: binding.work.agent,
            browser: None,
        },
    );
    let required = RecoveryRequired {
        binding: binding.clone(),
        origin: RecoveryOrigin::Existing { lsn: pause },
        reason: RecoveryReason::HeldOrPausedFailure,
        boundary: RecoveryBoundary::Paused { end_lsn },
        predecessor: None,
    };
    append(&mut fixture, Record::RecoveryRequired(required.clone()));
    (fixture, required, pause)
}

fn fresh(checkpoints: u64) -> (Fixture, RecoveryAuthorized, Lsn) {
    let (mut fixture, required, origin) = paused(checkpoints);
    let resumed = append(
        &mut fixture,
        Record::Resume {
            agent: required.binding.work.agent,
            browser: None,
        },
    );
    let authorization = RecoveryAuthorized {
        episode: RecoveryEpisodeId {
            work: required.binding.work,
            origin,
        },
        binding: required.binding,
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
            turn: TurnId::from_parts(71, 5),
            context: RecoveryContext::Fresh {
                session: None,
                recap: RecoveryRecap::default(),
            },
        },
    };
    let at = append(
        &mut fixture,
        Record::RecoveryAuthorized(authorization.clone()),
    );
    (fixture, authorization, at)
}

fn receipt(fixture: &mut Fixture) -> RecoveryReceipt {
    fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .receipt(request(), digest())
        .unwrap()
        .unwrap()
}

fn assert_canonical(fixture: &mut Fixture) {
    let reader = fixture.writer.reader();
    let mut rebuilt = Projections::in_memory().unwrap();
    crate::replay(&reader, &mut rebuilt).unwrap();
    assert_eq!(
        fixture.projections.export().unwrap(),
        rebuilt.export().unwrap()
    );
    assert_eq!(
        authority_rows(&fixture.projections).unwrap(),
        authority_rows(&rebuilt).unwrap()
    );
}

#[test]
fn required_new_issue_and_spawn_fold_at_actual_positions_across_checkpoints() {
    for checkpoints in [0, 1] {
        let (mut fixture, authorization, at) = fresh(checkpoints);
        assert_eq!(receipt(&mut fixture).state, RecoveryClaimState::Pending);
        assert_canonical(&mut fixture);
        let RecoveryPlan::New { turn, .. } = authorization.plan else {
            unreachable!()
        };
        let mut issued = goal_entries().1;
        let Record::TurnIssued {
            turn: actual,
            task,
            answers,
            ..
        } = &mut issued.record
        else {
            unreachable!()
        };
        *actual = turn;
        *task = "fresh recovery task".into();
        answers.clear();
        let decision = append(&mut fixture, decision_for(&issued).record);
        if let Record::TurnIssued {
            lsn_of_decision, ..
        } = &mut issued.record
        {
            *lsn_of_decision = decision;
        }
        let issue_lsn = append(&mut fixture, issued.record);
        let bound = receipt(&mut fixture);
        let RecoveryClaimState::IssueBound(issue) = bound.state else {
            panic!("new issue did not bind")
        };
        assert_eq!(issue.lsn, issue_lsn);
        assert_eq!(bound.claim.authorization_lsn, at);
        assert_canonical(&mut fixture);
        let evidence_lsn = append(&mut fixture, spawn(&authorization.binding, turn, false));
        assert_eq!(
            receipt(&mut fixture).state,
            RecoveryClaimState::Consumed {
                issue,
                evidence_lsn
            }
        );
        assert_canonical(&mut fixture);
    }
}

#[test]
fn required_repeat_preserves_the_original_issue_and_supersedes_only_consent() {
    let (mut fixture, binding) = original(1);
    let original = fixture
        .projections
        .turn(binding.work.turn)
        .unwrap()
        .unwrap();
    let issue = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .witness(binding.issue_lsn)
        .unwrap()
        .issue()
        .unwrap();
    let barrier = append(&mut fixture, Record::RecoveryRequired(missing(&binding)));
    let authorization = RecoveryAuthorized {
        episode: RecoveryEpisodeId {
            work: binding.work,
            origin: barrier,
        },
        binding: binding.clone(),
        request_id: request(),
        request_digest: digest(),
        attribution: RecoveryAttribution::LocalOperator {
            pid: 42,
            uid: Some(501),
        },
        expected_control: None,
        resulting_control: None,
        resume_lsn: None,
        plan: RecoveryPlan::RepeatIssued {
            issue: issue.clone(),
            session: None,
        },
    };
    append(&mut fixture, Record::RecoveryAuthorized(authorization));
    assert_eq!(
        receipt(&mut fixture).state,
        RecoveryClaimState::IssueBound(issue.clone())
    );
    assert_canonical(&mut fixture);
    let evidence_lsn = append(&mut fixture, spawn(&binding, binding.work.turn, true));
    assert_eq!(
        receipt(&mut fixture).state,
        RecoveryClaimState::Consumed {
            issue,
            evidence_lsn
        }
    );
    let after = fixture
        .projections
        .turn(binding.work.turn)
        .unwrap()
        .unwrap();
    assert_eq!(after.lsn_issued, original.lsn_issued);
    assert_eq!(after.model_selection, original.model_selection);
    assert_canonical(&mut fixture);
}

#[test]
fn required_pending_invalidation_uses_the_named_actual_change_after_target_loss() {
    let (mut fixture, authorization, at) = fresh(0);
    let changed = append(
        &mut fixture,
        Record::GoalCleared {
            agent: authorization.binding.work.agent,
            goal: authorization.binding.work.goal,
        },
    );
    let claim = RecoveryClaimRef {
        episode: authorization.episode,
        authorization_lsn: at,
    };
    let evidence = RecoveryInvalidationEvidence::RecordedChange {
        reason: RecoveryInvalidationReason::GoalChanged,
        lsn: changed,
    };
    let invalidation_lsn = append(
        &mut fixture,
        Record::RecoveryInvalidated(RecoveryInvalidated { claim, evidence }),
    );
    assert_eq!(
        receipt(&mut fixture).state,
        RecoveryClaimState::Invalidated {
            invalidation_lsn,
            evidence
        }
    );
    assert_canonical(&mut fixture);
}

fn cells(fixture: &Fixture) -> Vec<(Vec<u8>, Vec<u8>)> {
    let txn = fixture.projections.db.begin_read().unwrap();
    txn.open_table(RECOVERY)
        .unwrap()
        .iter()
        .unwrap()
        .map(|row| {
            let (key, value) = row.unwrap();
            (key.value().to_vec(), value.value().to_vec())
        })
        .collect()
}

fn damaged_root(kind: &str) {
    let (mut fixture, attempted) = if kind == "current" {
        let (fixture, required, _) = paused(0);
        (fixture, Record::RecoveryRequired(required))
    } else {
        let (fixture, authorization, _) = fresh(0);
        (fixture, Record::RecoveryAuthorized(authorization))
    };
    if kind != "current" {
        assert_eq!(receipt(&mut fixture).state, RecoveryClaimState::Pending);
    }
    assert_canonical(&mut fixture);
    let rows = cells(&fixture);
    let selected: Vec<_> = rows
        .iter()
        .filter(|(_, value)| {
            serde_json::from_slice::<serde_json::Value>(value).unwrap()["kind"] == kind
        })
        .map(|(key, _)| key.clone())
        .collect();
    assert_eq!(selected.len(), 1);
    let txn = fixture.projections.db.begin_write().unwrap();
    {
        let mut table = txn.open_table(RECOVERY).unwrap();
        table.remove(selected[0].as_slice()).unwrap();
    }
    txn.commit().unwrap();
    fixture.projections.recovery_certification = None;
    let before = cells(&fixture);
    let bytes = std::fs::read(crate::segment::segment_path(
        fixture.writer.reader().dir(),
        1,
    ))
    .unwrap();
    for lookup in [false, true] {
        let result = fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .and_then(|mut gateway| {
                if lookup {
                    gateway.receipt(request(), digest()).map(|_| ())
                } else {
                    gateway
                        .append(&Entry {
                            at: Timestamp::UNIX_EPOCH,
                            record: attempted.clone(),
                        })
                        .map(|_| ())
                }
            });
        assert!(
            matches!(
                result,
                Err(Error::RecoveryCache(RecoveryCacheError::MissingCell))
            ),
            "{kind}: {result:?}"
        );
        assert_eq!(cells(&fixture), before);
        assert_eq!(
            std::fs::read(crate::segment::segment_path(
                fixture.writer.reader().dir(),
                1
            ))
            .unwrap(),
            bytes
        );
    }
}

#[test]
fn missing_current_root_cannot_recreate_a_required_episode_at_the_certified_boundary() {
    damaged_root("current");
}
#[test]
fn missing_planned_root_cannot_release_a_reserved_turn_at_the_certified_boundary() {
    damaged_root("planned");
}
#[test]
fn missing_request_root_cannot_report_absent_or_reuse_consent_at_the_certified_boundary() {
    damaged_root("request");
}

#[test]
fn coherent_recovery_subtree_deletion_refuses_canonical_recertification() {
    let (mut fixture, _, _) = fresh(0);
    assert_eq!(receipt(&mut fixture).state, RecoveryClaimState::Pending);
    let keys: Vec<_> = cells(&fixture).into_iter().map(|(key, _)| key).collect();
    let txn = fixture.projections.db.begin_write().unwrap();
    {
        let mut table = txn.open_table(RECOVERY).unwrap();
        for key in keys {
            table.remove(key.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();
    fixture.projections.recovery_certification = None;
    let before = cells(&fixture);
    let result = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .err()
        .unwrap();
    assert_eq!(reason(&result), RecoveryStoreError::CanonicalMismatch);
    assert_eq!(cells(&fixture), before);
}

#[test]
fn recovery_cached_validation_rechecks_same_length_rewrite_and_bad_suffix() {
    for rewrite in [false, true] {
        let (mut fixture, mut authorization, at) = fresh(0);
        let path = crate::segment::segment_path(fixture.writer.reader().dir(), 1);
        let (reader, _) = crate::Reader::open(fixture.writer.reader().dir()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        authorization.episode.origin = Lsn(authorization.episode.origin.0 - 1);
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::RecoveryAuthorized(authorization),
        };
        let payload = serde_json::to_vec(&entry).unwrap();
        let mut frame = Vec::new();
        crate::frame::encode_required(RecordKind::RecoveryAuthorized, &payload, &mut frame)
            .unwrap();
        if rewrite {
            let offset = usize::try_from(at.offset()).unwrap();
            assert_eq!(bytes.len() - offset, frame.len());
            bytes[offset..].copy_from_slice(&frame);
        } else {
            bytes.extend(frame);
        }
        std::fs::write(&path, &bytes).unwrap();
        let before = cells(&fixture);
        let error = crate::replay(&reader, &mut Projections::in_memory().unwrap()).unwrap_err();
        assert!(
            matches!(
                error,
                Error::RecoveryRecord {
                    reason: RecoveryStateError::Binding,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert_eq!(cells(&fixture), before);
    }
}
