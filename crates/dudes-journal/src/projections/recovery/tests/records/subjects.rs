use super::*;
use dudes_proto::recovery::{RecoveryIssue, RecoveryWork};

fn subjects(fixture: &mut Fixture, works: &[RecoveryWork]) -> Vec<CheckedWorkSubject> {
    fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .authority_subjects(works)
        .unwrap()
}

fn subject(fixture: &mut Fixture, work: RecoveryWork) -> CheckedClaimSubject {
    let mut found = subjects(fixture, &[work]);
    assert_eq!(found.len(), 1);
    let CheckedWorkSubject::Claim(claim) = found.pop().unwrap() else {
        panic!("expected the checked claim for {work:?}");
    };
    *claim
}

fn new_issue(fixture: &mut Fixture, authorization: &RecoveryAuthorized) -> RecoveryIssue {
    let RecoveryPlan::New { turn, .. } = authorization.plan else {
        panic!("expected a new-turn plan");
    };
    let mut issued = goal_entries().1;
    let Record::TurnIssued {
        turn: actual, task, ..
    } = &mut issued.record
    else {
        unreachable!()
    };
    *actual = turn;
    *task = "the exact newly issued recovery task".into();
    let decision = append(fixture, decision_for(&issued).record);
    if let Record::TurnIssued {
        lsn_of_decision, ..
    } = &mut issued.record
    {
        *lsn_of_decision = decision;
    }
    let lsn = append(fixture, issued.record);
    fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .unwrap()
        .witness(lsn)
        .unwrap()
        .issue()
        .unwrap()
}

#[test]
fn ordinary_work_and_an_unclaimed_boundary_remain_distinct() {
    let (mut fixture, binding) = original(1);
    let through = fixture.projections.last_applied;
    assert_eq!(
        subjects(&mut fixture, &[binding.work]),
        vec![CheckedWorkSubject::NoClaim {
            work: binding.work,
            through,
        }]
    );
    let barrier = append(&mut fixture, Record::RecoveryRequired(missing(&binding)));
    let through = fixture.projections.last_applied;
    assert_eq!(
        subjects(&mut fixture, &[binding.work]),
        vec![CheckedWorkSubject::UnclaimedBoundary {
            work: binding.work,
            episode: Box::new(RecoveryEpisode {
                id: RecoveryEpisodeId {
                    work: binding.work,
                    origin: barrier,
                },
                barrier_lsn: barrier,
                binding,
                reason: RecoveryReason::MissingSpawn,
                authorization_lsn: None,
            }),
            through,
        }]
    );
}

#[test]
fn pending_reserved_work_uses_the_original_source_binding_at_one_cut() {
    for checkpoints in [0, 1] {
        let (mut fixture, authorization, authorization_lsn) = fresh(checkpoints);
        let RecoveryPlan::New { turn, .. } = authorization.plan else {
            unreachable!()
        };
        let reserved = RecoveryWork {
            turn,
            ..authorization.binding.work
        };
        let through = fixture.projections.last_applied;
        let found = subjects(&mut fixture, &[authorization.binding.work, reserved]);
        assert_eq!(found.len(), 2);
        for (queried, found) in [authorization.binding.work, reserved]
            .into_iter()
            .zip(found)
        {
            let CheckedWorkSubject::Claim(claim) = found else {
                panic!("pending work lost its claim");
            };
            assert_eq!(claim.queried_work(), queried);
            assert_eq!(claim.through(), through);
            assert_eq!(claim.receipt().authorization, authorization);
            assert_eq!(claim.receipt().claim.authorization_lsn, authorization_lsn);
            assert_eq!(claim.receipt().state, RecoveryClaimState::Pending);
            let facts = claim.current_facts().unwrap();
            assert_eq!(facts.through, through);
            assert_eq!(facts.binding, authorization.binding);
            assert_eq!(facts.source_issue.lsn, authorization.binding.issue_lsn);
            assert_eq!(facts.source_issue.issued.work, authorization.binding.work);
            assert_eq!(facts.control_lsn, authorization.resulting_control);
        }
        assert!(fixture.projections.turn(turn).unwrap().is_none());
    }
}

#[test]
fn issue_bound_source_and_reserved_queries_join_the_exact_durable_issue() {
    for checkpoints in [0, 1] {
        let (mut fixture, authorization, _) = fresh(checkpoints);
        let issue = new_issue(&mut fixture, &authorization);
        for queried in [authorization.binding.work, issue.issued.work] {
            let claim = subject(&mut fixture, queried);
            assert_eq!(claim.queried_work(), queried);
            assert_eq!(claim.receipt().authorization, authorization);
            assert_eq!(
                claim.receipt().state,
                RecoveryClaimState::IssueBound(issue.clone())
            );
            let facts = claim.current_facts().unwrap();
            let mut effective = authorization.binding.clone();
            effective.work = issue.issued.work;
            effective.issue_lsn = issue.lsn;
            assert_eq!(facts.binding, effective);
            assert_eq!(facts.source_issue, issue);
            assert_eq!(facts.control_lsn, authorization.resulting_control);
            assert_ne!(
                facts.binding.work,
                claim.receipt().authorization.binding.work
            );
        }
    }
}

#[test]
fn current_unclaimed_then_claimed_episode_wins_over_the_old_planned_claim() {
    let (mut fixture, authorization, old_lsn) = fresh(0);
    let issue = new_issue(&mut fixture, &authorization);
    let old = RecoveryClaimRef {
        episode: authorization.episode,
        authorization_lsn: old_lsn,
    };
    let mut binding = authorization.binding.clone();
    binding.work = issue.issued.work;
    binding.issue_lsn = issue.lsn;
    let mut required = missing(&binding);
    required.predecessor = Some(old);
    let barrier = append(&mut fixture, Record::RecoveryRequired(required));
    let found = subjects(&mut fixture, &[binding.work]);
    assert!(
        matches!(found.as_slice(), [CheckedWorkSubject::UnclaimedBoundary { episode, .. }]
        if episode.barrier_lsn == barrier && episode.authorization_lsn.is_none())
    );
    let mut next = authorization.clone();
    next.episode = RecoveryEpisodeId {
        work: binding.work,
        origin: barrier,
    };
    next.binding = binding.clone();
    next.request_id = Hex32::new([21; 32]);
    next.request_digest = Hex32::new([22; 32]);
    next.plan = RecoveryPlan::RepeatIssued {
        issue: issue.clone(),
        session: None,
    };
    let next_lsn = append(
        &mut fixture,
        Record::RecoveryAuthorized(Box::new(next.clone())),
    );
    let claim = subject(&mut fixture, binding.work);
    assert_eq!(claim.receipt().authorization, next);
    assert_eq!(claim.receipt().claim.authorization_lsn, next_lsn);
    assert_ne!(claim.receipt().claim, old);
    assert_eq!(claim.current_facts().unwrap().source_issue, issue);
    let historical = subject(&mut fixture, authorization.binding.work);
    assert_eq!(historical.receipt().authorization, authorization);
    assert!(matches!(
        historical.receipt().state,
        RecoveryClaimState::Superseded { .. }
    ));
    assert_eq!(historical.current_facts(), None);
}

#[test]
fn consumed_claim_keeps_its_receipt_after_current_control_changes() {
    let (mut fixture, authorization, _) = fresh(0);
    let issue = new_issue(&mut fixture, &authorization);
    let evidence_lsn = append(
        &mut fixture,
        spawn(&authorization.binding, issue.issued.work.turn, false),
    );
    append(
        &mut fixture,
        Record::Pause {
            agent: issue.issued.work.agent,
            browser: None,
        },
    );
    let claim = subject(&mut fixture, issue.issued.work);
    assert_eq!(claim.receipt().authorization, authorization);
    assert_eq!(
        claim.receipt().state,
        RecoveryClaimState::Consumed {
            issue,
            evidence_lsn
        }
    );
    assert_eq!(claim.current_facts(), None);
}

#[test]
fn subject_reads_are_inert_and_share_the_current_certified_cut() {
    let (mut fixture, authorization, _) = fresh(1);
    let export = fixture.projections.export().unwrap();
    let bytes = journal_inventory(&fixture);
    let cut = fixture.projections.last_applied;
    let first = subjects(&mut fixture, &[authorization.binding.work]);
    let counts = crate::reader::validation_counts();
    for _ in 0..3 {
        assert_eq!(subjects(&mut fixture, &[authorization.binding.work]), first);
    }
    assert_eq!(crate::reader::validation_counts(), counts);
    assert_eq!(fixture.projections.export().unwrap(), export);
    assert_eq!(journal_inventory(&fixture), bytes);
    assert_eq!(
        subject(&mut fixture, authorization.binding.work).through(),
        cut
    );
}

#[test]
fn a_planned_turn_cannot_redirect_the_original_agent_or_goal() {
    for change_agent in [false, true] {
        let (mut fixture, authorization, _) = fresh(0);
        let RecoveryPlan::New { turn, .. } = authorization.plan else {
            unreachable!()
        };
        let mut wrong = RecoveryWork {
            turn,
            ..authorization.binding.work
        };
        if change_agent {
            wrong.agent = dudes_types::AgentId::from_parts(88, 1);
        } else {
            wrong.goal = dudes_types::GoalId::from_parts(88, 2);
        }
        let gateway = fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap();
        assert!(matches!(
            gateway.authority_subjects(&[wrong]),
            Err(Error::RecoveryState(RecoveryStateError::Binding))
        ));
    }
}

#[test]
fn an_active_claim_requires_the_authorized_resulting_control() {
    for issued in [false, true] {
        let (mut fixture, authorization, _) = fresh(0);
        if issued {
            new_issue(&mut fixture, &authorization);
        }
        append(
            &mut fixture,
            Record::Pause {
                agent: authorization.binding.work.agent,
                browser: None,
            },
        );
        let gateway = fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap();
        assert_eq!(
            gateway
                .receipt(request(), digest())
                .unwrap()
                .unwrap()
                .authorization,
            authorization
        );
        assert!(matches!(
            gateway.authority_subjects(&[authorization.binding.work]),
            Err(Error::RecoveryState(RecoveryStateError::Control))
        ));
    }
}

#[test]
fn invalidated_receipts_survive_target_loss_without_active_facts() {
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
    let checked = subject(&mut fixture, authorization.binding.work);
    assert_eq!(checked.receipt().authorization, authorization);
    assert_eq!(
        checked.receipt().state,
        RecoveryClaimState::Invalidated {
            invalidation_lsn,
            evidence
        }
    );
    assert_eq!(checked.current_facts(), None);
}

#[test]
fn uncertified_stale_and_uncertain_reads_never_report_no_claim() {
    for expected in [
        RecoveryStoreError::Uncertified,
        RecoveryStoreError::JournalAdvanced,
        RecoveryStoreError::JournalUncertain,
        RecoveryStoreError::Metadata,
    ] {
        let (mut fixture, binding) = original(0);
        let gateway = fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap();
        match expected {
            RecoveryStoreError::Uncertified => gateway.projections.recovery_certification = None,
            RecoveryStoreError::JournalAdvanced => {
                gateway.writer.append(&entry(RecordKind::Pause)).unwrap();
            }
            RecoveryStoreError::JournalUncertain => gateway.writer.recovery_fenced = true,
            RecoveryStoreError::Metadata => {
                let txn = gateway.projections.db.begin_write().unwrap();
                txn.open_table(META)
                    .unwrap()
                    .insert(LAST_APPLIED_CRC, &99)
                    .unwrap();
                txn.commit().unwrap();
            }
            _ => unreachable!(),
        }
        for works in [vec![], vec![binding.work]] {
            assert_eq!(
                reason(&gateway.authority_subjects(&works).unwrap_err()),
                expected
            );
        }
    }
}

#[test]
fn missing_receipt_dependencies_are_errors_without_mutation() {
    for kind in ["request", "claim", "authorization", "planned"] {
        let (mut fixture, authorization, _) = fresh(0);
        subject(&mut fixture, authorization.binding.work);
        let selected: Vec<_> = cells(&fixture)
            .into_iter()
            .filter(|(_, value)| {
                serde_json::from_slice::<serde_json::Value>(value).unwrap()["kind"] == kind
            })
            .collect();
        assert_eq!(selected.len(), 1);
        let txn = fixture.projections.db.begin_write().unwrap();
        txn.open_table(RECOVERY)
            .unwrap()
            .remove(selected[0].0.as_slice())
            .unwrap();
        txn.commit().unwrap();
        let before = cells(&fixture);
        let bytes = journal_inventory(&fixture);
        let gateway = fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap();
        assert!(
            matches!(
                gateway.authority_subjects(&[authorization.binding.work]),
                Err(Error::RecoveryCache(RecoveryCacheError::MissingCell))
            ),
            "{kind}"
        );
        assert_eq!(cells(&fixture), before);
        assert_eq!(journal_inventory(&fixture), bytes);
    }
}

#[test]
fn an_absent_optional_root_in_an_uncertified_cache_is_not_ordinary_work() {
    let (mut fixture, authorization, _) = fresh(0);
    let selected: Vec<_> = cells(&fixture)
        .into_iter()
        .filter(|(_, value)| {
            serde_json::from_slice::<serde_json::Value>(value).unwrap()["kind"] == "current"
        })
        .collect();
    assert_eq!(selected.len(), 1);
    let txn = fixture.projections.db.begin_write().unwrap();
    txn.open_table(RECOVERY)
        .unwrap()
        .remove(selected[0].0.as_slice())
        .unwrap();
    txn.commit().unwrap();
    fixture.projections.recovery_certification = None;
    let result = fixture
        .writer
        .checked_recovery(&mut fixture.projections)
        .and_then(|gateway| gateway.authority_subjects(&[authorization.binding.work]));
    assert!(result.is_err(), "{result:?}");
}

#[test]
fn an_issue_bound_receipt_must_equal_the_actual_task_answers_and_position() {
    for field in ["task", "answers", "position"] {
        let (mut fixture, authorization, _) = fresh(1);
        let issue = new_issue(&mut fixture, &authorization);
        subject(&mut fixture, issue.issued.work);
        let selected: Vec<_> = cells(&fixture)
            .into_iter()
            .filter(|(_, value)| {
                serde_json::from_slice::<serde_json::Value>(value).unwrap()["kind"] == "claim"
            })
            .collect();
        assert_eq!(selected.len(), 1);
        let mut cell: serde_json::Value = serde_json::from_slice(&selected[0].1).unwrap();
        let mut changed: RecoveryClaimState =
            serde_json::from_value(cell["value"].clone()).unwrap();
        let RecoveryClaimState::IssueBound(ref mut wrong) = changed else {
            unreachable!()
        };
        match field {
            "task" => wrong.issued.task_digest = Hex32::new([99; 32]),
            "answers" => wrong.issued.answers = vec![authorization.binding.issue_lsn],
            "position" => {
                wrong.lsn = fixture.projections.last_applied;
                assert_ne!(wrong.lsn, issue.lsn);
            }
            _ => unreachable!(),
        }
        cell["value"] = serde_json::to_value(changed).unwrap();
        let encoded = serde_json::to_vec(&cell).unwrap();
        let txn = fixture.projections.db.begin_write().unwrap();
        txn.open_table(RECOVERY)
            .unwrap()
            .insert(selected[0].0.as_slice(), encoded.as_slice())
            .unwrap();
        txn.commit().unwrap();
        let gateway = fixture
            .writer
            .checked_recovery(&mut fixture.projections)
            .unwrap();
        let result = gateway.authority_subjects(&[issue.issued.work]);
        if field == "position" {
            assert!(
                matches!(
                    result,
                    Err(Error::RecoveryCache(RecoveryCacheError::Position))
                ),
                "{result:?}"
            );
        } else {
            assert!(
                matches!(
                    result,
                    Err(Error::RecoveryState(RecoveryStateError::Binding))
                ),
                "{field}: {result:?}"
            );
        }
    }
}

fn leased_ticket_original() -> (Fixture, RecoveryBinding) {
    let dir = tempfile::tempdir().unwrap();
    let (mut writer, _) = crate::open(
        &dir.path().join("journal"),
        JournalOptions {
            fsync: Fsync::None,
            rotate_at: 16 * 1024 * 1024,
            checkpoint_every: 0,
        },
    )
    .unwrap();
    let leased = entry(RecordKind::WorktreeLeased);
    let Record::WorktreeLeased { worktree, path, .. } = &leased.record else {
        unreachable!()
    };
    let worktree = *worktree;
    let path = path.clone();
    writer.append(&leased).unwrap();
    let mut hired = entry(RecordKind::AgentHired);
    if let Record::AgentHired {
        worktree: actual,
        cwd,
        ..
    } = &mut hired.record
    {
        *actual = Some(worktree);
        *cwd = path;
    }
    writer.append(&hired).unwrap();
    let mut projections =
        Projections::open_with(&dir.path().join("state.redb"), Fsync::None).unwrap();
    crate::replay(&writer.reader(), &mut projections).unwrap();
    let mut fixture = Fixture {
        dir,
        writer,
        projections,
    };
    append(&mut fixture, entry(RecordKind::TicketCreated).record);
    let mut assigned = entry(RecordKind::TicketAssigned).record;
    if let Record::TicketAssigned { expected_seq, .. } = &mut assigned {
        *expected_seq = dudes_types::Seq(0);
    }
    append(&mut fixture, assigned);
    let mut started = entry(RecordKind::TicketStarted).record;
    let Record::TicketStarted {
        card, expected_seq, ..
    } = &mut started
    else {
        unreachable!()
    };
    let card = *card;
    *expected_seq = dudes_types::Seq(1);
    append(&mut fixture, started);
    let (_, mut issued, mut binding) = goal_entries();
    binding.hired_lsn = fixture
        .projections
        .agent(binding.work.agent)
        .unwrap()
        .unwrap()
        .hired_lsn;
    binding.worktree = Some(worktree);
    binding.ticket = Some(dudes_proto::diff_stamp::TicketBinding {
        card,
        round: 1,
        goal: binding.work.goal,
        claimant: binding.work.agent,
    });
    let decision = append(&mut fixture, decision_for(&issued).record);
    if let Record::TurnIssued {
        lsn_of_decision, ..
    } = &mut issued.record
    {
        *lsn_of_decision = decision;
    }
    binding.issue_lsn = append(&mut fixture, issued.record);
    (fixture, binding)
}

#[test]
fn new_issued_work_preserves_nonempty_original_ticket_lease_hire_provider_and_model() {
    let (mut fixture, binding) = leased_ticket_original();
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
    let control = append(
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
        binding: binding.clone(),
        request_id: request(),
        request_digest: digest(),
        attribution: RecoveryAttribution::LocalOperator {
            pid: 42,
            uid: Some(501),
        },
        expected_control: Some(control),
        resulting_control: Some(control),
        resume_lsn: None,
        plan: RecoveryPlan::New {
            turn: TurnId::from_parts(71, 5),
            context: RecoveryContext::Fresh {
                session: None,
                recap: RecoveryRecap::default(),
            },
        },
    };
    append(
        &mut fixture,
        Record::RecoveryAuthorized(Box::new(authorization.clone())),
    );
    let pending = subject(&mut fixture, binding.work);
    assert_eq!(pending.current_facts().unwrap().binding, binding);
    let issue = new_issue(&mut fixture, &authorization);
    let bound = subject(&mut fixture, binding.work);
    let effective = &bound.current_facts().unwrap().binding;
    assert_eq!(bound.receipt().authorization, authorization);
    assert_eq!(effective.work, issue.issued.work);
    assert_eq!(effective.issue_lsn, issue.lsn);
    assert_eq!(effective.hired_lsn, binding.hired_lsn);
    assert_eq!(effective.provider, binding.provider);
    assert_eq!(effective.model_selection, binding.model_selection);
    assert_eq!(effective.worktree, binding.worktree);
    assert_eq!(effective.ticket, binding.ticket);
    assert!(effective.worktree.is_some());
    assert!(effective.ticket.is_some());
    assert_eq!(bound.current_facts().unwrap().control_lsn, Some(control));
}

fn different_binding(binding: &RecoveryBinding, field: &str) -> RecoveryBinding {
    let mut changed = binding.clone();
    match field {
        "hire" => changed.hired_lsn = Lsn(binding.hired_lsn.0 + 1),
        "provider" => {
            changed.provider = if binding.provider == dudes_types::Provider::Claude {
                dudes_types::Provider::Codex
            } else {
                dudes_types::Provider::Claude
            };
        }
        "model" => changed.model_selection = None,
        "lease" => changed.worktree = Some(dudes_types::WorktreeId::from_bytes([89; 16])),
        "ticket" => {
            changed.ticket = Some(dudes_proto::diff_stamp::TicketBinding {
                card: dudes_types::CardId::from_parts(89, 2),
                round: 7,
                goal: binding.work.goal,
                claimant: binding.work.agent,
            });
        }
        _ => unreachable!(),
    }
    assert_ne!(changed, *binding);
    changed
}

#[test]
fn active_facts_cannot_replace_the_original_hire_provider_model_lease_or_ticket() {
    for issued in [false, true] {
        for field in ["hire", "provider", "model", "lease", "ticket"] {
            let (mut fixture, authorization, _) = fresh(0);
            if issued {
                new_issue(&mut fixture, &authorization);
            }
            subject(&mut fixture, authorization.binding.work);
            let binding = different_binding(&authorization.binding, field);
            let rows = cells(&fixture);
            let txn = fixture.projections.db.begin_write().unwrap();
            let mut replaced = 0;
            {
                let mut table = txn.open_table(RECOVERY).unwrap();
                for (key, raw) in rows {
                    let mut value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
                    if value["kind"] == "episode" || value["kind"] == "authorization" {
                        value["value"]["binding"] = serde_json::to_value(&binding).unwrap();
                        let encoded = serde_json::to_vec(&value).unwrap();
                        table.insert(key.as_slice(), encoded.as_slice()).unwrap();
                        replaced += 1;
                    }
                }
            }
            txn.commit().unwrap();
            assert_eq!(replaced, 2);
            let gateway = fixture
                .writer
                .checked_recovery(&mut fixture.projections)
                .unwrap();
            let result = gateway.authority_subjects(&[authorization.binding.work]);
            if issued && field == "model" {
                assert!(
                    matches!(
                        result,
                        Err(Error::RecoveryCache(RecoveryCacheError::Identity))
                    ),
                    "{result:?}"
                );
            } else {
                assert!(
                    matches!(
                        result,
                        Err(Error::RecoveryState(RecoveryStateError::Binding))
                    ),
                    "{field}: {result:?}"
                );
            }
        }
    }
}

#[test]
fn unrelated_journal_progress_updates_the_cut_without_changing_the_exact_subject() {
    let (mut fixture, authorization, _) = fresh(0);
    let before = subject(&mut fixture, authorization.binding.work);
    append(
        &mut fixture,
        Record::AgentMcpServersChanged {
            agent: authorization.binding.work.agent,
            mcp_servers: Vec::new(),
        },
    );
    let after = subject(&mut fixture, authorization.binding.work);
    assert!(after.through() > before.through());
    assert_eq!(after.receipt(), before.receipt());
    let mut expected = before.current_facts().unwrap().clone();
    expected.through = after.through();
    assert_eq!(after.current_facts(), Some(&expected));
}
