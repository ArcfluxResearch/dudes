use std::cell::RefCell;

use super::*;
use crate::record::samples;
use dudes_types::{ModelSelection, Timestamp};

fn entry(record: Record) -> Entry {
    Entry {
        at: Timestamp::UNIX_EPOCH,
        record,
    }
}
fn hash(_: &[u8]) -> Hex32 {
    Hex32::new([0x55; 32])
}

#[test]
fn an_issue_retains_exact_identity_answers_and_hash_input_without_task_text() {
    let agent = AgentId::from_parts(17, 1);
    let goal = GoalId::from_parts(17, 2);
    let turn = TurnId::from_parts(17, 3);
    let work = RecoveryWork { agent, goal, turn };
    let record = Record::TurnIssued {
        agent,
        goal,
        turn,
        task: "Keep π\r\n".into(),
        session: Some(SessionId::new("announced")),
        mode: Mode::Managed,
        model_selection: None,
        lsn_of_decision: Lsn(19),
        answers: vec![Lsn(11), Lsn(18)],
    };
    let inputs = RefCell::new(Vec::new());
    let mut history = RecoveryHistory::default();
    history
        .observe(Lsn(20), &entry(record), |bytes| {
            inputs.borrow_mut().push(bytes.to_vec());
            hash(bytes)
        })
        .unwrap();
    assert_eq!(
        &*inputs.borrow(),
        &[b"dudes.recovery.task.v1\0Keep \xcf\x80\r\n".to_vec()]
    );
    let reference = RecoveryWitnessRef {
        lsn: Lsn(20),
        kind: RecordKind::TurnIssued,
        owner: RecoveryWitnessOwner::Work(work),
    };
    let witness = history.get(reference).unwrap();
    assert_eq!(
        witness.issue(),
        Some(RecoveryIssue {
            lsn: Lsn(20),
            issued: RecoveryIssued {
                work,
                task_digest: Hex32::new([0x55; 32]),
                answers: vec![Lsn(11), Lsn(18)],
                model_selection: None,
                session: Some(SessionId::new("announced"))
            }
        })
    );
    let encoded = serde_json::to_string(witness).unwrap();
    assert!(!encoded.contains("Keep"));
    assert!(!encoded.contains("task\":"));
}

#[test]
fn missing_or_differently_owned_source_evidence_refuses_without_mutation() {
    let mut history = RecoveryHistory::default();
    let record = samples::record(RecordKind::AgentHired);
    history.observe(Lsn(10), &entry(record), hash).unwrap();
    let reference = RecoveryWitnessRef {
        lsn: Lsn(10),
        kind: RecordKind::AgentHired,
        owner: RecoveryWitnessOwner::Agent(samples::agent()),
    };
    let before = history.clone();
    assert!(matches!(
        history.get(reference).unwrap().evidence(),
        RecoveryEvidence::Hire { .. }
    ));
    assert_eq!(
        history.get(RecoveryWitnessRef {
            lsn: Lsn(11),
            ..reference
        }),
        Err(RecoveryHistoryError::MissingWitness)
    );
    assert_eq!(
        history.get(RecoveryWitnessRef {
            kind: RecordKind::Resume,
            ..reference
        }),
        Err(RecoveryHistoryError::Identity)
    );
    assert_eq!(
        history.get(RecoveryWitnessRef {
            owner: RecoveryWitnessOwner::Agent(AgentId::from_parts(99, 99)),
            ..reference
        }),
        Err(RecoveryHistoryError::Identity)
    );
    assert_eq!(history, before);
}

#[test]
fn an_actual_position_is_immutable_and_equal_redelivery_is_inert() {
    let mut history = RecoveryHistory::default();
    let record = entry(samples::record(RecordKind::AgentHired));
    history.observe(Lsn(10), &record, hash).unwrap();
    let before = history.clone();
    history.observe(Lsn(10), &record, hash).unwrap();
    assert_eq!(history, before);
    let changed = entry(Record::Pause {
        agent: samples::agent(),
        browser: None,
    });
    assert_eq!(
        history.observe(Lsn(10), &changed, hash),
        Err(RecoveryHistoryError::ConflictingWitness)
    );
    assert_eq!(
        history.observe(Lsn::ZERO, &changed, hash),
        Err(RecoveryHistoryError::Position)
    );
    assert_eq!(history, before);
}

#[test]
fn actual_carrier_kinds_remain_distinct_and_finite_work_has_no_invented_goal() {
    let mut history = RecoveryHistory::default();
    for (position, kind) in [
        (10, RecordKind::TurnIssued),
        (20, RecordKind::TurnIssuedWithBrowserNote),
        (30, RecordKind::NoteOnlyTurnIssued),
        (40, RecordKind::TaughtNoteOnlyTurnIssued),
        (50, RecordKind::MailWakeOnlyTurnIssued),
        (60, RecordKind::ConversationTurnIssued),
    ] {
        let source = entry(samples::record(kind));
        history.observe(Lsn(position), &source, hash).unwrap();
        let witness = &history.witnesses[&Lsn(position)];
        assert_eq!(witness.reference().kind, kind);
        if kind == RecordKind::ConversationTurnIssued {
            assert!(matches!(
                witness.reference().owner,
                RecoveryWitnessOwner::Turn { .. }
            ));
            assert!(witness.issue().is_none());
        } else {
            assert!(matches!(
                witness.reference().owner,
                RecoveryWitnessOwner::Work(_)
            ));
            assert!(witness.issue().is_some());
        }
    }
    assert!(matches!(
        history.witnesses[&Lsn(10)].evidence(),
        RecoveryEvidence::Issue {
            carrier: RecoveryIssueCarrier::Ordinary,
            ..
        }
    ));
    assert!(matches!(
        history.witnesses[&Lsn(20)].evidence(),
        RecoveryEvidence::Issue {
            carrier: RecoveryIssueCarrier::BrowserNote { .. },
            ..
        }
    ));
    assert!(matches!(
        history.witnesses[&Lsn(30)].evidence(),
        RecoveryEvidence::Issue {
            carrier: RecoveryIssueCarrier::CompletedNote { .. },
            ..
        }
    ));
    assert!(matches!(
        history.witnesses[&Lsn(50)].evidence(),
        RecoveryEvidence::Issue {
            carrier: RecoveryIssueCarrier::MailWake { .. },
            ..
        }
    ));
}

#[test]
fn an_old_absent_model_choice_differs_from_an_explicit_provider_default() {
    let mut older = samples::record(RecordKind::TurnIssued);
    let Record::TurnIssued {
        model_selection, ..
    } = &mut older
    else {
        unreachable!()
    };
    *model_selection = None;
    let mut explicit = older.clone();
    let Record::TurnIssued {
        model_selection, ..
    } = &mut explicit
    else {
        unreachable!()
    };
    *model_selection = Some(ModelSelection::provider_default());
    let a = witness(Lsn(10), &older, hash)
        .unwrap()
        .unwrap()
        .issue()
        .unwrap();
    let b = witness(Lsn(10), &explicit, hash)
        .unwrap()
        .unwrap()
        .issue()
        .unwrap();
    assert_eq!(a.issued.model_selection, None);
    assert_eq!(
        b.issued.model_selection,
        Some(ModelSelection::provider_default())
    );
    assert_ne!(a, b);
}

#[test]
fn a_review_is_only_a_source_marker_and_does_not_derive_goal_or_control_changes() {
    let mut history = RecoveryHistory::default();
    let review = entry(samples::record(RecordKind::TicketInstructionReviewed));
    history.observe(Lsn(50), &review, hash).unwrap();
    let witness = history.witnesses[&Lsn(50)].clone();
    assert_eq!(
        witness.reference().kind,
        RecordKind::TicketInstructionReviewed
    );
    assert_eq!(witness.evidence(), &RecoveryEvidence::Marker);
    assert_eq!(history.witnesses.len(), 1);
    let pause = entry(Record::Pause {
        agent: samples::agent(),
        browser: None,
    });
    history.observe(Lsn(60), &pause, hash).unwrap();
    assert_eq!(
        history.witnesses[&Lsn(60)].evidence(),
        &RecoveryEvidence::Control { browser: None }
    );
    assert_eq!(history.witnesses[&Lsn(50)], witness);
}

#[test]
fn reported_predicate_success_keeps_raw_author_evidence_without_qualifying_done() {
    let mut source = samples::record(RecordKind::PredicateResult);
    let card = CardId::from_parts(17, 4);
    if let Record::PredicateResult { pass, evidence, .. } = &mut source {
        *pass = true;
        *evidence = Evidence::Card {
            card,
            status: dudes_types::CardStatus::Done,
            by: Actor::Agent {
                agent: samples::agent(),
            },
        };
    }
    let evidence = witness(Lsn(20), &source, hash).unwrap().unwrap();
    assert!(matches!(
        evidence.evidence(),
        RecoveryEvidence::Predicate {
            reported_pass: true,
            card: Some(RecoveryCardEvidence {
                by: Actor::Agent { .. },
                ..
            }),
            ..
        }
    ));
    assert_eq!(evidence.reference().kind, RecordKind::PredicateResult);
}

#[test]
fn cached_witnesses_require_explicit_nullable_fields_and_reject_unknown_fields() {
    let source = samples::record(RecordKind::TurnSpawned);
    let value = witness(Lsn(20), &source, hash).unwrap().unwrap();
    let mut cell = serde_json::to_value(&value).unwrap();
    let spawn = cell["evidence"]["Spawn"].as_object_mut().unwrap();
    spawn.remove("session");
    assert!(serde_json::from_value::<RecoveryWitness>(cell).is_err());

    let mut cell = serde_json::to_value(&value).unwrap();
    cell.as_object_mut()
        .unwrap()
        .insert("authorized".into(), serde_json::Value::Bool(true));
    assert!(serde_json::from_value::<RecoveryWitness>(cell).is_err());
}

#[test]
fn an_ignored_record_cannot_replace_a_retained_source_at_the_same_position() {
    let mut history = RecoveryHistory::default();
    history
        .observe(
            Lsn(10),
            &entry(samples::record(RecordKind::AgentHired)),
            hash,
        )
        .unwrap();
    let before = history.clone();
    let irrelevant = entry(samples::record(RecordKind::HumanQuestion));
    assert_eq!(
        history.observe(Lsn(10), &irrelevant, hash),
        Err(RecoveryHistoryError::ConflictingWitness)
    );
    assert_eq!(history, before);
    history.observe(Lsn(11), &irrelevant, hash).unwrap();
    assert_eq!(history, before);
}
