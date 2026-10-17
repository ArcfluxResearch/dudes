//! Actual required-frame prefixes at the certified journal boundary.

mod carriers;
mod matrix;
mod subjects;

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

pub(super) fn fresh_authorization(checkpoints: u64) -> (Fixture, RecoveryAuthorized) {
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
    (fixture, authorization)
}

fn fresh(checkpoints: u64) -> (Fixture, RecoveryAuthorized, Lsn) {
    let (mut fixture, authorization) = fresh_authorization(checkpoints);
    let at = append(
        &mut fixture,
        Record::RecoveryAuthorized(Box::new(authorization.clone())),
    );
    (fixture, authorization, at)
}

fn receipt(fixture: &mut Fixture) -> RecoveryReceipt {
    let gateway = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    let receipt = gateway.receipt(request(), digest()).unwrap().unwrap();
    assert_eq!(
        gateway.receipt_by_request(request()).unwrap(),
        Some(receipt.clone())
    );
    receipt
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
        let pending = receipt(&mut fixture);
        assert_eq!(pending.state, RecoveryClaimState::Pending);
        assert_eq!(pending.authorization, authorization);
        assert_canonical(&mut fixture);
        let RecoveryPlan::New { turn, .. } = &authorization.plan else {
            unreachable!()
        };
        let turn = *turn;
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
        assert_eq!(bound.authorization, authorization);
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
        assert_eq!(receipt(&mut fixture).authorization, authorization);
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
    append(
        &mut fixture,
        Record::RecoveryAuthorized(Box::new(authorization)),
    );
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
    assert_eq!(receipt(&mut fixture).authorization, authorization);
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

#[test]
fn request_only_receipt_refuses_a_dangling_claim_instead_of_reporting_absence() {
    let (mut fixture, authorization, _) = fresh(0);
    assert_eq!(receipt(&mut fixture).authorization, authorization);
    let rows = cells(&fixture);
    let claims: Vec<_> = rows
        .iter()
        .filter(|(_, value)| {
            serde_json::from_slice::<serde_json::Value>(value).unwrap()["kind"] == "claim"
        })
        .collect();
    assert_eq!(claims.len(), 1);
    let txn = fixture.projections.db.begin_write().unwrap();
    txn.open_table(RECOVERY)
        .unwrap()
        .remove(claims[0].0.as_slice())
        .unwrap();
    txn.commit().unwrap();
    let before = cells(&fixture);
    let bytes = journal_inventory(&fixture);
    let gateway = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    let counts = crate::reader::validation_counts();
    for result in [
        gateway.receipt_by_request(request()),
        gateway.receipt(request(), digest()),
    ] {
        assert!(
            matches!(
                result,
                Err(Error::RecoveryCache(RecoveryCacheError::MissingCell))
            ),
            "{result:?}"
        );
    }
    assert_eq!(crate::reader::validation_counts(), counts);
    assert_eq!(cells(&fixture), before);
    assert_eq!(journal_inventory(&fixture), bytes);
}

#[test]
fn request_only_receipt_survives_reopen_and_later_control_without_rewriting_intent() {
    let (mut fixture, authorization, authorization_lsn) = fresh(1);
    append(
        &mut fixture,
        Record::Pause {
            agent: authorization.binding.work.agent,
            browser: None,
        },
    );
    let path = fixture.dir.path().join("state.redb");
    drop(fixture.projections);
    fixture.projections = Projections::open_with(&path, Fsync::None).unwrap();
    assert!(fixture.projections.recovery_certification.is_none());
    let export = fixture.projections.export().unwrap();
    let bytes = journal_inventory(&fixture);
    let gateway = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap();
    let counts = crate::reader::validation_counts();
    for _ in 0..3 {
        let receipt = gateway.receipt_by_request(request()).unwrap().unwrap();
        assert_eq!(receipt.claim.authorization_lsn, authorization_lsn);
        assert_eq!(receipt.authorization, authorization);
        assert_eq!(receipt.state, RecoveryClaimState::Pending);
        assert_eq!(gateway.receipt(request(), digest()).unwrap(), Some(receipt));
        assert!(matches!(
            gateway.receipt(request(), Hex32::new([99; 32])),
            Err(Error::RecoveryState(RecoveryStateError::RequestConflict))
        ));
        assert_eq!(
            gateway.receipt_by_request(Hex32::new([0; 32])).unwrap(),
            None
        );
    }
    assert_eq!(crate::reader::validation_counts(), counts);
    assert_eq!(fixture.projections.export().unwrap(), export);
    assert_eq!(journal_inventory(&fixture), bytes);
}

fn damaged_root(kind: &str) {
    let (mut fixture, attempted) = if kind == "current" {
        let (fixture, required, _) = paused(0);
        (fixture, Record::RecoveryRequired(required))
    } else {
        let (fixture, authorization, _) = fresh(0);
        (fixture, Record::RecoveryAuthorized(Box::new(authorization)))
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
    let bytes = journal_inventory(&fixture);
    for lookup in [0, 1, 2] {
        let result = fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .and_then(|mut gateway| match lookup {
                0 => gateway
                    .append(&Entry {
                        at: Timestamp::UNIX_EPOCH,
                        record: attempted.clone(),
                    })
                    .map(|_| ()),
                1 => gateway.receipt(request(), digest()).map(|_| ()),
                _ => gateway.receipt_by_request(request()).map(|_| ()),
            });
        assert!(
            matches!(
                result,
                Err(Error::RecoveryCache(RecoveryCacheError::MissingCell))
            ),
            "{kind}: {result:?}"
        );
        assert_eq!(cells(&fixture), before);
        assert_eq!(journal_inventory(&fixture), bytes);
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
    let export = fixture.projections.export().unwrap();
    let bytes = journal_inventory(&fixture);
    let result = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .err()
        .unwrap();
    assert_eq!(reason(&result), RecoveryStoreError::CanonicalMismatch);
    assert_eq!(cells(&fixture), before);
    assert_eq!(fixture.projections.export().unwrap(), export);
    assert_eq!(journal_inventory(&fixture), bytes);
}

#[test]
fn recovery_cached_validation_rechecks_same_length_rewrite_and_bad_suffix() {
    for rewrite in [false, true] {
        let (fixture, mut authorization, at) = fresh(0);
        let path = crate::segment::segment_path(fixture.writer.reader().dir(), at.segment());
        let (reader, _) = crate::Reader::open(fixture.writer.reader().dir()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let suffix = Lsn::from_parts(at.segment(), u64::try_from(bytes.len()).unwrap()).unwrap();
        authorization.episode.origin = Lsn(authorization.episode.origin.0 - 1);
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::RecoveryAuthorized(Box::new(authorization)),
        };
        let payload = serde_json::to_vec(&entry).unwrap();
        let mut frame = Vec::new();
        crate::frame::encode_required(RecordKind::RecoveryAuthorized, &payload, &mut frame)
            .unwrap();
        let (bad_lsn, expected_reason) = if rewrite {
            let offset = usize::try_from(at.offset()).unwrap();
            assert_eq!(bytes.len() - offset, frame.len());
            bytes[offset..].copy_from_slice(&frame);
            (at, RecoveryStateError::Binding)
        } else {
            bytes.extend(frame);
            (suffix, RecoveryStateError::DuplicateRequest)
        };
        std::fs::write(&path, &bytes).unwrap();
        let before = cells(&fixture);
        let error = crate::replay(&reader, &mut Projections::in_memory().unwrap()).unwrap_err();
        let actual = match &error {
            Error::RecoveryRecord { lsn, reason } => Some((*lsn, *reason)),
            _ => None,
        };
        assert_eq!(actual, Some((bad_lsn, expected_reason)), "{error}");
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert_eq!(cells(&fixture), before);
    }
}

fn terminal(binding: &RecoveryBinding, outcome: dudes_proto::TurnOutcome) -> Record {
    let mut record = entry(RecordKind::TurnEnded).record;
    let Record::TurnEnded {
        agent,
        turn,
        mode,
        provenance,
        outcome: actual,
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
    *actual = Some(outcome);
    *session = None;
    *session_usage = None;
    record
}

#[test]
fn actual_launch_holds_and_vanished_sources_create_only_their_matching_boundaries() {
    for vanished in [false, true] {
        for refused in [false, true] {
            if vanished && refused {
                continue;
            }
            let (mut fixture, binding) = original(0);
            let spawned =
                vanished.then(|| append(&mut fixture, spawn(&binding, binding.work.turn, false)));
            let outcome = if vanished {
                dudes_proto::TurnOutcome::Vanished
            } else if refused {
                dudes_proto::TurnOutcome::LaunchRefused {
                    reason: "adapter refusal".into(),
                }
            } else {
                dudes_proto::TurnOutcome::NotLaunched {
                    reason: "host failure".into(),
                }
            };
            let ended = append(&mut fixture, terminal(&binding, outcome));
            let required = if let Some(spawn_lsn) = spawned {
                RecoveryRequired {
                    binding: binding.clone(),
                    origin: RecoveryOrigin::ThisBarrier,
                    reason: RecoveryReason::Vanished,
                    boundary: RecoveryBoundary::Vanished {
                        spawn_lsn,
                        end_lsn: ended,
                    },
                    predecessor: None,
                }
            } else {
                let cause = if refused {
                    dudes_proto::AutomaticHold::LaunchRefused {
                        reason: "adapter refusal".into(),
                    }
                } else {
                    dudes_proto::AutomaticHold::LaunchFailed {
                        reason: "host failure".into(),
                    }
                };
                let hold = append(
                    &mut fixture,
                    Record::AgentHeld {
                        agent: binding.work.agent,
                        provenance: Provenance::Observed,
                        cause,
                    },
                );
                RecoveryRequired {
                    binding: binding.clone(),
                    origin: RecoveryOrigin::Existing { lsn: hold },
                    reason: RecoveryReason::HeldOrPausedFailure,
                    boundary: RecoveryBoundary::Held {
                        end_lsn: ended,
                        first: None,
                    },
                    predecessor: None,
                }
            };
            append(&mut fixture, Record::RecoveryRequired(required));
            assert_canonical(&mut fixture);
        }
    }
}

#[test]
fn actual_unconfirmed_resume_requires_the_specific_prior_confirmation_and_latest_attempt() {
    use dudes_types::SessionId;
    let (mut fixture, old_binding) = original(0);
    append(
        &mut fixture,
        spawn(&old_binding, old_binding.work.turn, false),
    );
    let session = SessionId::new("recorded-context");
    let confirmed = append(
        &mut fixture,
        Record::SessionLearned {
            agent: old_binding.work.agent,
            turn: old_binding.work.turn,
            session: session.clone(),
        },
    );
    append(
        &mut fixture,
        terminal(&old_binding, dudes_proto::TurnOutcome::NoResult),
    );
    let mut issue = goal_entries().1;
    let turn = TurnId::from_parts(71, 8);
    if let Record::TurnIssued {
        turn: actual,
        session: requested,
        ..
    } = &mut issue.record
    {
        *actual = turn;
        *requested = Some(session.clone());
    }
    let decision = append(&mut fixture, decision_for(&issue).record);
    if let Record::TurnIssued {
        lsn_of_decision, ..
    } = &mut issue.record
    {
        *lsn_of_decision = decision;
    }
    let issued = append(&mut fixture, issue.record);
    let mut binding = old_binding;
    binding.work.turn = turn;
    binding.issue_lsn = issued;
    let mut spawned = spawn(&binding, turn, false);
    if let Record::TurnSpawned {
        session: requested, ..
    } = &mut spawned
    {
        *requested = Some(session);
    }
    let spawn_lsn = append(&mut fixture, spawned);
    let end_lsn = append(
        &mut fixture,
        terminal(&binding, dudes_proto::TurnOutcome::NoResult),
    );
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
    append(&mut fixture, Record::RecoveryRequired(required));
    assert_canonical(&mut fixture);
}

#[test]
fn healthy_recovery_prefix_reopens_and_rebuilds_literal_prior_cache_versions() {
    for old_version in [None, Some(47), Some(48), Some(49), Some(50)] {
        let (mut fixture, _, _) = fresh(1);
        let expected = fixture.projections.export().unwrap();
        if let Some(version) = old_version {
            let txn = fixture.projections.db.begin_write().unwrap();
            txn.open_table(META)
                .unwrap()
                .insert(SCHEMA_VERSION, &version)
                .unwrap();
            txn.commit().unwrap();
        }
        let path = fixture.dir.path().join("state.redb");
        let old = std::mem::replace(&mut fixture.projections, Projections::in_memory().unwrap());
        drop(old);
        fixture.projections = Projections::open_with(&path, Fsync::None).unwrap();
        if old_version.is_some() {
            assert_eq!(fixture.projections.last_applied_lsn(), Lsn::ZERO);
        }
        crate::replay(&fixture.writer.reader(), &mut fixture.projections).unwrap();
        assert_eq!(fixture.projections.export().unwrap(), expected);
        assert_eq!(receipt(&mut fixture).state, RecoveryClaimState::Pending);
        assert_canonical(&mut fixture);
    }
}

fn journal_inventory(fixture: &Fixture) -> Vec<(u32, Vec<u8>)> {
    let reader = fixture.writer.reader();
    crate::segment::list_segments(reader.dir())
        .unwrap()
        .into_iter()
        .map(|segment| {
            (
                segment,
                std::fs::read(crate::segment::segment_path(reader.dir(), segment)).unwrap(),
            )
        })
        .collect()
}

fn assert_repeated_receipt_reads(
    fixture: &mut Fixture,
    claims: &[RecoveryClaimRef],
    barriers: &[Lsn],
    requests: &[(Hex32, Hex32)],
    authorizations: &[RecoveryAuthorized],
    issue: &dudes_proto::recovery::RecoveryIssue,
    evidence_lsn: Lsn,
) {
    let export = fixture.projections.export().unwrap();
    let bytes = journal_inventory(fixture);
    {
        let gateway = fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap();
        let before_counts = crate::reader::validation_counts();
        for _ in 0..3 {
            for (index, (request, digest)) in requests.iter().enumerate() {
                let receipt = gateway.receipt(*request, *digest).unwrap().unwrap();
                assert_eq!(
                    gateway.receipt_by_request(*request).unwrap(),
                    Some(receipt.clone())
                );
                assert_eq!(receipt.claim, claims[index]);
                assert_eq!(receipt.authorization, authorizations[index]);
                assert!(matches!(
                    gateway.receipt(*request, Hex32::new([99; 32])),
                    Err(Error::RecoveryState(RecoveryStateError::RequestConflict))
                ));
                let expected = if index + 1 == requests.len() {
                    RecoveryClaimState::Consumed {
                        issue: issue.clone(),
                        evidence_lsn,
                    }
                } else {
                    RecoveryClaimState::Superseded {
                        issue: issue.clone(),
                        successor: claims[index + 1].episode,
                        barrier_lsn: barriers[index + 1],
                    }
                };
                assert_eq!(receipt.state, expected);
            }
            assert_eq!(
                gateway.receipt_by_request(Hex32::new([0; 32])).unwrap(),
                None
            );
            assert_eq!(
                gateway.receipt(Hex32::new([0; 32]), digest()).unwrap(),
                None
            );
        }
        assert_eq!(crate::reader::validation_counts(), before_counts);
    }
    assert_eq!(fixture.projections.export().unwrap(), export);
    assert_eq!(journal_inventory(fixture), bytes);
}

#[test]
fn repeated_missing_spawn_supersession_keeps_each_receipt_and_the_one_owed_issue() {
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
    let mut predecessor = None;
    let mut claims = Vec::new();
    let mut barriers = Vec::new();
    let mut requests = Vec::new();
    let mut authorizations = Vec::new();
    for byte in [7, 9, 11] {
        let mut required = missing(&binding);
        required.predecessor = predecessor;
        let origin = append(&mut fixture, Record::RecoveryRequired(required));
        barriers.push(origin);
        let request_id = Hex32::new([byte; 32]);
        let request_digest = Hex32::new([byte + 1; 32]);
        let authorization = RecoveryAuthorized {
            episode: RecoveryEpisodeId {
                work: binding.work,
                origin,
            },
            binding: binding.clone(),
            request_id,
            request_digest,
            attribution: RecoveryAttribution::LocalOperator {
                pid: u32::from(byte),
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
        let authorization_lsn = append(
            &mut fixture,
            Record::RecoveryAuthorized(Box::new(authorization.clone())),
        );
        authorizations.push(authorization);
        let claim = RecoveryClaimRef {
            episode: RecoveryEpisodeId {
                work: binding.work,
                origin,
            },
            authorization_lsn,
        };
        predecessor = Some(claim);
        claims.push(claim);
        requests.push((request_id, request_digest));
        assert_canonical(&mut fixture);
    }
    let evidence_lsn = append(&mut fixture, spawn(&binding, binding.work.turn, true));
    assert_repeated_receipt_reads(
        &mut fixture,
        &claims,
        &barriers,
        &requests,
        &authorizations,
        &issue,
        evidence_lsn,
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
fn actual_paused_authorization_without_a_clearing_resume_refuses_before_append() {
    let (mut fixture, required, origin) = paused(0);
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
        expected_control: Some(origin),
        resulting_control: Some(origin),
        resume_lsn: None,
        plan: RecoveryPlan::New {
            turn: TurnId::from_parts(71, 5),
            context: RecoveryContext::Fresh {
                session: None,
                recap: RecoveryRecap::default(),
            },
        },
    };
    let before = fixture.projections.export().unwrap();
    let bytes = journal_inventory(&fixture);
    let attempted = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::RecoveryAuthorized(Box::new(authorization)),
    };
    let result = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .append(&attempted);
    assert!(
        matches!(
            result,
            Err(Error::RecoveryAdmission {
                kind: RecordKind::RecoveryAuthorized,
                reason: RecoveryStateError::Control
            })
        ),
        "{result:?}"
    );
    assert_eq!(fixture.projections.export().unwrap(), before);
    assert_eq!(journal_inventory(&fixture), bytes);
}

fn fast_failure(binding: &RecoveryBinding) -> Record {
    let mut record = terminal(binding, dudes_proto::TurnOutcome::NoResult);
    if let Record::TurnEnded {
        elapsed_ms,
        exit,
        usage,
        model_usage,
        ..
    } = &mut record
    {
        *elapsed_ms = Some(1);
        *exit = dudes_proto::ExitStatus {
            code: Some(1),
            signal: None,
        };
        *usage = None;
        model_usage.clear();
    }
    assert!(dudes_proto::decision::is_fast_failure(&record));
    record
}

#[test]
fn actual_repeated_failure_boundary_joins_both_qualified_named_ends() {
    let (mut fixture, first_binding) = original(0);
    append(
        &mut fixture,
        spawn(&first_binding, first_binding.work.turn, false),
    );
    let first_end = append(&mut fixture, fast_failure(&first_binding));
    let mut issued = goal_entries().1;
    let turn = TurnId::from_parts(71, 9);
    if let Record::TurnIssued { turn: actual, .. } = &mut issued.record {
        *actual = turn;
    }
    let decision = append(&mut fixture, decision_for(&issued).record);
    if let Record::TurnIssued {
        lsn_of_decision, ..
    } = &mut issued.record
    {
        *lsn_of_decision = decision;
    }
    let issue_lsn = append(&mut fixture, issued.record);
    let mut binding = first_binding.clone();
    binding.work.turn = turn;
    binding.issue_lsn = issue_lsn;
    append(&mut fixture, spawn(&binding, turn, false));
    let end_lsn = append(&mut fixture, fast_failure(&binding));
    let origin = append(
        &mut fixture,
        Record::AgentHeld {
            agent: binding.work.agent,
            provenance: Provenance::Observed,
            cause: dudes_proto::AutomaticHold::RepeatedFastFailure {
                first: first_binding.work.turn,
                latest: turn,
            },
        },
    );
    let required = RecoveryRequired {
        binding,
        origin: RecoveryOrigin::Existing { lsn: origin },
        reason: RecoveryReason::HeldOrPausedFailure,
        boundary: RecoveryBoundary::Held {
            end_lsn,
            first: Some(dudes_proto::recovery::RecoveryFailure {
                issue_lsn: first_binding.issue_lsn,
                end_lsn: first_end,
            }),
        },
        predecessor: None,
    };
    let mut wrong = required.clone();
    if let RecoveryBoundary::Held {
        first: Some(first), ..
    } = &mut wrong.boundary
    {
        first.end_lsn = end_lsn;
    }
    let before = fixture.projections.export().unwrap();
    let bytes = journal_inventory(&fixture);
    let result = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .append(&Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::RecoveryRequired(wrong),
        });
    assert!(
        matches!(
            result,
            Err(Error::RecoveryAdmission {
                reason: RecoveryStateError::Binding,
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(fixture.projections.export().unwrap(), before);
    assert_eq!(journal_inventory(&fixture), bytes);
    append(&mut fixture, Record::RecoveryRequired(required));
    assert_canonical(&mut fixture);
}

#[test]
fn a_reserved_new_turn_refuses_an_attached_issue_without_appending_or_changing_rows() {
    let (mut fixture, authorization, _) = fresh(0);
    let RecoveryPlan::New { turn, .. } = authorization.plan else {
        unreachable!()
    };
    assert_eq!(receipt(&mut fixture).state, RecoveryClaimState::Pending);
    assert!(fixture.projections.turn(turn).unwrap().is_none());
    let mut issued = goal_entries().1;
    if let Record::TurnIssued {
        turn: actual,
        task,
        answers,
        ..
    } = &mut issued.record
    {
        *actual = turn;
        *task = "fresh recovery task".into();
        answers.clear();
    }
    let decision = append(&mut fixture, decision_for(&issued).record);
    if let Record::TurnIssued {
        lsn_of_decision,
        mode,
        ..
    } = &mut issued.record
    {
        *lsn_of_decision = decision;
        *mode = Mode::Attached;
    }
    let before = fixture.projections.export().unwrap();
    let bytes = journal_inventory(&fixture);
    let result = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .append(&issued);
    let refusal = match result {
        Err(Error::RecoveryAdmission { kind, reason }) => Some((kind, reason)),
        Err(other) => panic!("unexpected reserved-issue refusal: {other}"),
        Ok(_) => None,
    };
    let observed = (
        refusal,
        fixture.projections.export().unwrap() == before,
        journal_inventory(&fixture) == bytes,
        fixture.projections.turn(turn).unwrap().is_none(),
        receipt(&mut fixture).state,
    );
    assert_eq!(
        observed,
        (
            Some((RecordKind::TurnIssued, RecoveryStateError::Binding)),
            true,
            true,
            true,
            RecoveryClaimState::Pending,
        )
    );
}
