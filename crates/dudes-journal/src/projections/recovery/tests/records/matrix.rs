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

fn pending_authorization(
    fixture: &mut Fixture,
    required: RecoveryRequired,
    origin: Lsn,
    turn: TurnId,
    byte: u8,
) -> RecoveryAuthorized {
    let resumed = append(
        fixture,
        Record::Resume {
            agent: required.binding.work.agent,
            browser: None,
        },
    );
    RecoveryAuthorized {
        episode: RecoveryEpisodeId {
            work: required.binding.work,
            origin,
        },
        binding: required.binding,
        request_id: Hex32::new([byte; 32]),
        request_digest: Hex32::new([byte + 1; 32]),
        attribution: RecoveryAttribution::LocalOperator {
            pid: 42,
            uid: Some(501),
        },
        expected_control: Some(resumed),
        resulting_control: Some(resumed),
        resume_lsn: None,
        plan: RecoveryPlan::New {
            turn,
            context: RecoveryContext::Fresh {
                session: None,
                recap: RecoveryRecap::default(),
            },
        },
    }
}

fn independent_authorization(fixture: &mut Fixture) -> RecoveryAuthorized {
    let work = other_owner(fixture);
    let mut binding = goal_entries().2;
    binding.work = work;
    binding.hired_lsn = fixture
        .projections
        .agent(work.agent)
        .unwrap()
        .unwrap()
        .hired_lsn;
    binding.issue_lsn = fixture
        .projections
        .turn(work.turn)
        .unwrap()
        .unwrap()
        .lsn_issued;
    append(fixture, spawn(&binding, work.turn, false));
    let end_lsn = append(
        fixture,
        terminal(&binding, dudes_proto::TurnOutcome::NoResult),
    );
    let origin = append(
        fixture,
        Record::Pause {
            agent: work.agent,
            browser: None,
        },
    );
    let required = RecoveryRequired {
        binding,
        origin: RecoveryOrigin::Existing { lsn: origin },
        reason: RecoveryReason::HeldOrPausedFailure,
        boundary: RecoveryBoundary::Paused { end_lsn },
        predecessor: None,
    };
    append(fixture, Record::RecoveryRequired(required.clone()));
    pending_authorization(fixture, required, origin, TurnId::from_parts(73, 5), 41)
}

fn authorization_collision(kind: &str) -> (Fixture, Record, RecoveryStateError) {
    let (mut fixture, original, _) = fresh(0);
    let mut other = independent_authorization(&mut fixture);
    let reason = if kind == "planned" {
        let RecoveryPlan::New { turn, .. } = original.plan else {
            unreachable!()
        };
        let RecoveryPlan::New {
            turn: other_turn, ..
        } = &mut other.plan
        else {
            unreachable!()
        };
        *other_turn = turn;
        RecoveryStateError::PlannedTurn
    } else {
        other.request_id = original.request_id;
        RecoveryStateError::DuplicateRequest
    };
    (fixture, Record::RecoveryAuthorized(Box::new(other)), reason)
}

fn assert_checked_refusal(fixture: &mut Fixture, attempted: Record, expected: RecoveryStateError) {
    let export = fixture.projections.export().unwrap();
    let rows = cells(fixture);
    let bytes = journal_inventory(fixture);
    let result = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .append(&Entry {
            at: Timestamp::UNIX_EPOCH,
            record: attempted,
        });
    assert!(
        matches!(result, Err(Error::RecoveryAdmission { reason, .. }) if reason == expected),
        "{result:?}"
    );
    assert_eq!(fixture.projections.export().unwrap(), export);
    assert_eq!(cells(fixture), rows);
    assert_eq!(journal_inventory(fixture), bytes);
}

fn reopened_root_refuses(kind: &str) {
    let (mut fixture, attempted, expected) = if kind == "current" {
        let (fixture, required, _) = paused(0);
        (
            fixture,
            Record::RecoveryRequired(required),
            RecoveryStateError::EpisodeAlias,
        )
    } else {
        let (mut positive, _, _) = fresh(0);
        let other = independent_authorization(&mut positive);
        append(
            &mut positive,
            Record::RecoveryAuthorized(Box::new(other.clone())),
        );
        let found = positive
            .writer
            .checked_recovery(&mut positive.projections)
            .unwrap()
            .receipt(other.request_id, other.request_digest)
            .unwrap()
            .unwrap();
        assert_eq!(found.state, RecoveryClaimState::Pending);
        assert_canonical(&mut positive);
        authorization_collision(kind)
    };
    assert_canonical(&mut fixture);
    assert_checked_refusal(&mut fixture, attempted.clone(), expected);
    let keys: Vec<_> = cells(&fixture)
        .into_iter()
        .filter(|(_, value)| {
            serde_json::from_slice::<serde_json::Value>(value).unwrap()["kind"] == kind
        })
        .map(|(key, _)| key)
        .collect();
    assert_eq!(keys.len(), 1);
    let txn = fixture.projections.db.begin_write().unwrap();
    {
        let mut table = txn.open_table(RECOVERY).unwrap();
        table.remove(keys[0].as_slice()).unwrap();
    }
    txn.commit().unwrap();
    let old = std::mem::replace(&mut fixture.projections, Projections::in_memory().unwrap());
    drop(old);
    fixture.projections =
        Projections::open_with(&fixture.dir.path().join("state.redb"), Fsync::None).unwrap();
    let export = fixture.projections.export().unwrap();
    let rows = cells(&fixture);
    let bytes = journal_inventory(&fixture);
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
        assert_eq!(fixture.projections.export().unwrap(), export);
        assert_eq!(cells(&fixture), rows);
        assert_eq!(journal_inventory(&fixture), bytes);
    }
}

#[test]
fn reopened_missing_current_root_preserves_the_clean_episode_alias_oracle() {
    reopened_root_refuses("current");
}

#[test]
fn reopened_missing_planned_root_preserves_an_independent_episode_collision_oracle() {
    reopened_root_refuses("planned");
}

#[test]
fn reopened_missing_request_root_preserves_an_independent_episode_collision_oracle() {
    reopened_root_refuses("request");
}

#[test]
fn required_record_interprets_sources_earlier_in_the_same_actual_batch() {
    let mut fixture = Fixture::new(0);
    let (installed, mut issued, mut binding) = goal_entries();
    binding.hired_lsn = fixture
        .projections
        .agent(binding.work.agent)
        .unwrap()
        .unwrap()
        .hired_lsn;
    let installed_at = fixture.writer.append_witnessed(&installed).unwrap();
    let decision = decision_for(&issued);
    let decision_at = fixture.writer.append_witnessed(&decision).unwrap();
    if let Record::TurnIssued {
        lsn_of_decision, ..
    } = &mut issued.record
    {
        *lsn_of_decision = decision_at.0;
    }
    let issued_at = fixture.writer.append_witnessed(&issued).unwrap();
    binding.issue_lsn = issued_at.0;
    let required = Entry {
        at: Timestamp::UNIX_EPOCH,
        record: Record::RecoveryRequired(missing(&binding)),
    };
    let required_at = fixture.writer.append_required_witnessed(&required).unwrap();
    assert_eq!(required_at.0.offset(), 0);
    assert!(required_at.0.segment() > issued_at.0.segment());
    assert!(
        fixture
            .projections
            .turn(binding.work.turn)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture
            .projections
            .apply_batch([
                (installed_at.0, installed_at.1, &installed),
                (decision_at.0, decision_at.1, &decision),
                (issued_at.0, issued_at.1, &issued),
                (required_at.0, required_at.1, &required),
            ])
            .unwrap(),
        4
    );
    {
        let txn = fixture.projections.db.begin_read().unwrap();
        let table = txn.open_table(RECOVERY).unwrap();
        let (state, _) = RecoveryCache::read_all(&CacheReader(&table), required_at.0).unwrap();
        let current = state.current(binding.work).unwrap();
        assert_eq!(current.binding, binding);
        assert_eq!(current.barrier_lsn, required_at.0);
    }
    assert_canonical(&mut fixture);
}

#[test]
fn actual_reader_and_writer_stage_utf8_recap_at_8192_and_refuse_8193_bytes() {
    for overflow in [false, true] {
        let (mut fixture, required, origin) = paused(0);
        let mut authorization =
            pending_authorization(&mut fixture, required, origin, TurnId::from_parts(74, 5), 7);
        let RecoveryPlan::New {
            context: RecoveryContext::Fresh { recap, .. },
            ..
        } = &mut authorization.plan
        else {
            unreachable!()
        };
        recap.text = "é".repeat(4096);
        if overflow {
            recap.text.push('x');
        }
        assert_eq!(recap.text.len(), 8192 + usize::from(overflow));
        let attempted = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::RecoveryAuthorized(Box::new(authorization)),
        };
        let export = fixture.projections.export().unwrap();
        let bytes = journal_inventory(&fixture);
        let (segment, last) = bytes.last().unwrap();
        let at = Lsn::from_parts(*segment, u64::try_from(last.len()).unwrap()).unwrap();
        assert_eq!(fixture.writer.next_lsn().unwrap(), at);
        let raw = tempfile::tempdir().unwrap();
        for (segment, bytes) in &bytes {
            std::fs::write(crate::segment::segment_path(raw.path(), *segment), bytes).unwrap();
        }
        let mut frame = Vec::new();
        crate::frame::encode_required(
            RecordKind::RecoveryAuthorized,
            &serde_json::to_vec(&attempted).unwrap(),
            &mut frame,
        )
        .unwrap();
        let mut raw_tail = last.clone();
        raw_tail.extend(frame);
        let raw_path = crate::segment::segment_path(raw.path(), *segment);
        std::fs::write(&raw_path, &raw_tail).unwrap();
        let result = fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap()
            .append(&attempted);
        if overflow {
            assert!(
                matches!(
                    result,
                    Err(Error::RecoveryAdmission {
                        kind: RecordKind::RecoveryAuthorized,
                        reason: RecoveryStateError::Context
                    })
                ),
                "{result:?}"
            );
            assert_eq!(fixture.projections.export().unwrap(), export);
            assert_eq!(journal_inventory(&fixture), bytes);
            assert!(
                matches!(crate::Reader::open(raw.path()), Err(Error::RecoveryRecord { lsn, reason: RecoveryStateError::Context }) if lsn == at)
            );
        } else {
            assert_eq!(result.unwrap(), at);
            let (reader, _) = crate::Reader::open(raw.path()).unwrap();
            assert_eq!(
                reader
                    .iter_from(Lsn::ZERO)
                    .unwrap()
                    .last()
                    .unwrap()
                    .unwrap(),
                (at, attempted)
            );
            assert_canonical(&mut fixture);
        }
        assert_eq!(std::fs::read(raw_path).unwrap(), raw_tail);
    }
}
