use std::cell::RefCell;

use super::*;
use crate::instruction::{
    InstructionAcceptCompletion, InstructionComparison, InstructionCoverageGap,
    InstructionEndBoundary, InstructionGapKind, InstructionGoalClear, InstructionIssueAssociation,
    InstructionReviewAction, InstructionSampleUsage, InstructionUnknownReason,
};
use crate::instruction_state::{InstructionReviewFacts, InstructionStateError};
use crate::record::{Actor, Entry, Hex32, Record, RecordKind, samples};
use dudes_types::{Mode, Seq, Timestamp};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Reads {
    keys: Vec<Vec<u8>>,
    prefixes: Vec<Vec<u8>>,
    rows: usize,
    bytes: usize,
}

#[derive(Default)]
struct Memory {
    rows: BTreeMap<Vec<u8>, Vec<u8>>,
    reads: RefCell<Reads>,
}

impl InstructionCacheReader for Memory {
    type Error = InstructionCacheError;

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        let value = self.rows.get(key).cloned();
        let mut reads = self.reads.borrow_mut();
        reads.keys.push(key.to_vec());
        if let Some(value) = &value {
            reads.rows += 1;
            reads.bytes += value.len();
        }
        Ok(value)
    }

    fn prefix(&self, prefix: &[u8]) -> Result<Vec<InstructionCacheRow>, Self::Error> {
        let rows: Vec<_> = self
            .rows
            .range(prefix.to_vec()..)
            .take_while(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let mut reads = self.reads.borrow_mut();
        reads.prefixes.push(prefix.to_vec());
        reads.rows += rows.len();
        reads.bytes += rows.iter().map(|(_, value)| value.len()).sum::<usize>();
        Ok(rows)
    }
}

fn hash(bytes: &[u8]) -> Hex32 {
    let mut value = [0_u8; 32];
    for (index, byte) in bytes.iter().enumerate() {
        value[index % 32] = value[index % 32].wrapping_mul(31).wrapping_add(*byte);
    }
    Hex32::new(value)
}

fn stamp(record: Record) -> Entry {
    Entry {
        at: Timestamp::UNIX_EPOCH,
        record,
    }
}

fn start(
    owner: InstructionObservationOwner,
    ticket: Option<InstructionTicketAssociation>,
) -> Record {
    let Record::InstructionObservationStarted { snapshot, .. } =
        samples::record(RecordKind::InstructionObservationStarted)
    else {
        unreachable!()
    };
    Record::InstructionObservationStarted {
        owner,
        ticket,
        snapshot,
    }
}

fn owner(turn: TurnId, issue: InstructionIssueAssociation) -> InstructionObservationOwner {
    InstructionObservationOwner::Managed {
        agent: samples::agent(),
        turn,
        issue,
    }
}

fn unknown(
    owner: InstructionObservationOwner,
    ticket: Option<InstructionTicketAssociation>,
    started: u64,
) -> Record {
    Record::InstructionObservationFinished {
        owner,
        ticket,
        baseline: InstructionBaselineRef::Started { lsn: Lsn(started) },
        settlement: InstructionSettlement::Unknown {
            reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
            end: None,
            sampled_at: None,
            recovery: true,
            comparison: InstructionComparison {
                changes: Vec::new(),
                gaps: vec![InstructionCoverageGap {
                    origin: crate::instruction::InstructionGapOrigin::Observed,
                    reason: InstructionGapKind::UnrecordedLaunchBoundary,
                    context: crate::InstructionPathContext::SharedProject,
                    agent: None,
                    path: None,
                }],
                usage: InstructionSampleUsage::default(),
            },
        },
    }
}

#[derive(Default)]
struct Harness {
    memory: Memory,
    full: InstructionState,
    through: Lsn,
}

impl Harness {
    fn apply(&mut self, position: u64, record: Record, facts: Option<InstructionReviewFacts>) {
        let entry = stamp(record);
        let lsn = Lsn(position);
        let mut cache =
            InstructionCache::load(&self.memory, self.through, &entry.record, &[]).unwrap();
        let expected = self.full.apply(lsn, &entry, facts, hash).unwrap();
        let got = cache.state_mut().apply(lsn, &entry, facts, hash).unwrap();
        assert_eq!(got, expected);
        let mut cards = got.evidence_cards;
        if let Some((card, _)) = got.reviewed {
            cards.push(card);
        }
        for card in cards {
            assert_eq!(
                cache.state().review(card, hash).unwrap(),
                self.full.review(card, hash).unwrap()
            );
        }
        for update in cache.updates().unwrap() {
            if let Some(value) = update.value {
                if matches!(
                    serde_json::from_slice::<Cell>(&value).unwrap(),
                    Cell::Snapshot { .. } | Cell::Finished { .. }
                ) {
                    assert!(
                        !self.memory.rows.contains_key(&update.key),
                        "immutable evidence was rewritten"
                    );
                }
                self.memory.rows.insert(update.key, value);
            } else {
                self.memory.rows.remove(&update.key);
            }
        }
        self.through = lsn;
        assert_eq!(
            InstructionCache::read_all(&self.memory, lsn).unwrap(),
            self.full
        );
    }

    fn ticket_prefix(&mut self) -> InstructionTicketAssociation {
        self.apply(1, samples::record(RecordKind::TicketAssigned), None);
        self.apply(2, samples::record(RecordKind::TicketStarted), None);
        self.full
            .association_for_goal(samples::goal(), Seq(2))
            .unwrap()
    }
}

#[test]
fn keyed_folds_match_full_history_across_retries_review_and_rework() {
    let mut h = Harness::default();
    let ticket = Some(h.ticket_prefix());
    let initial = owner(samples::turn(), InstructionIssueAssociation::Prospective);
    h.apply(3, start(initial, ticket), None);
    h.apply(4, samples::record(RecordKind::TurnIssued), None);
    h.apply(5, samples::record(RecordKind::TurnSpawned), None);
    h.apply(6, samples::record(RecordKind::TurnEnded), None);
    h.apply(
        7,
        Record::InstructionObservationFinished {
            owner: initial,
            ticket,
            baseline: InstructionBaselineRef::Started { lsn: Lsn(3) },
            settlement: InstructionSettlement::Compared {
                end: InstructionEndBoundary::ManagedTurn {
                    turn: samples::turn(),
                    end_lsn: Lsn(6),
                },
                sampled_at: Timestamp::UNIX_EPOCH,
                recovery: false,
                comparison: InstructionComparison::default(),
            },
        },
        None,
    );
    let next_turn = TurnId::from_parts(1, 22);
    let mut issue = samples::record(RecordKind::TurnIssued);
    if let Record::TurnIssued { turn, .. } = &mut issue {
        *turn = next_turn;
    }
    h.apply(8, issue, None);
    let retry = owner(
        next_turn,
        InstructionIssueAssociation::Issued { issue_lsn: Lsn(8) },
    );
    h.apply(9, start(retry, ticket), None);
    h.apply(
        10,
        Record::AgentHeld {
            agent: samples::agent(),
            provenance: dudes_types::Provenance::Observed,
            cause: crate::AutomaticHold::LaunchRefused {
                reason: "final admission".into(),
            },
        },
        None,
    );
    h.apply(
        11,
        Record::InstructionObservationFinished {
            owner: retry,
            ticket,
            baseline: InstructionBaselineRef::Started { lsn: Lsn(9) },
            settlement: InstructionSettlement::NoProcess {
                proof_lsn: Lsn(10),
                proof: crate::InstructionNoProcessProof::NoHostCall,
            },
        },
        None,
    );
    h.apply(12, start(retry, ticket), None);
    h.apply(13, unknown(retry, ticket, 12), None);
    let action = InstructionReviewAction::Accept {
        completion: InstructionAcceptCompletion::Continue,
    };
    review(&mut h, 14, action, None);
    h.apply(15, start(retry, ticket), None);
    h.apply(16, unknown(retry, ticket, 15), None);
    let clear_goal = Some(InstructionGoalClear {
        agent: samples::agent(),
        goal: samples::goal(),
    });
    review(
        &mut h,
        17,
        InstructionReviewAction::ChangesRequested {
            agent: samples::agent(),
            message: None,
            clear_goal,
        },
        clear_goal,
    );
    assert!(
        h.full
            .association_for_goal(samples::goal(), Seq(100))
            .is_none()
    );
    let mut next = samples::record(RecordKind::TicketStarted);
    if let Record::TicketStarted { goal, .. } = &mut next {
        goal.id = GoalId::from_parts(1, 66);
    }
    h.apply(18, next, None);
    h.apply(19, unknown(retry, ticket, 15), None);
}

#[test]
fn a_keyed_start_cannot_reopen_an_observed_end() {
    let mut h = Harness::default();
    h.apply(1, samples::record(RecordKind::TurnIssued), None);
    h.apply(2, samples::record(RecordKind::TurnEnded), None);
    let late = stamp(start(
        owner(
            samples::turn(),
            InstructionIssueAssociation::Issued { issue_lsn: Lsn(1) },
        ),
        None,
    ));
    let mut cache = InstructionCache::load(&h.memory, h.through, &late.record, &[]).unwrap();
    assert_eq!(
        cache.state_mut().apply(Lsn(3), &late, None, hash),
        Err(InstructionStateError::Boundary)
    );
    assert!(cache.updates().unwrap().is_empty());
}

fn review(
    h: &mut Harness,
    lsn: u64,
    action: InstructionReviewAction,
    clear_goal: Option<InstructionGoalClear>,
) {
    let seq = Seq(100);
    h.apply(
        lsn,
        Record::TicketInstructionReviewed {
            card: samples::card(),
            by: Actor::Peer { pid: 77, uid: None },
            expected_seq: seq,
            watermark: h.full.watermark(samples::card(), hash).unwrap().unwrap(),
            action,
        },
        Some(InstructionReviewFacts {
            card: samples::card(),
            seq,
            goal: Some(samples::goal()),
            completed_lsn: None,
            clear_goal,
        }),
    );
}

fn closed_attachment() -> (Harness, InstructionObservationOwner) {
    let mut h = Harness::default();
    h.apply(1, samples::record(RecordKind::Attached), None);
    let process = InstructionObservationOwner::AttachedProcess {
        agent: samples::agent(),
        attachment_lsn: Lsn(1),
    };
    h.apply(2, start(process, None), None);
    h.apply(3, samples::record(RecordKind::PtySpawned), None);
    h.apply(4, samples::record(RecordKind::PtyCloseRequested), None);
    h.apply(
        5,
        Record::PtyCloseCompleted {
            request_lsn: Lsn(4),
            outcome: crate::record::PtyCloseOutcome::Closed,
        },
        None,
    );
    h.apply(
        6,
        Record::InstructionObservationFinished {
            owner: process,
            ticket: None,
            baseline: InstructionBaselineRef::Started { lsn: Lsn(2) },
            settlement: InstructionSettlement::Compared {
                end: InstructionEndBoundary::AttachedProcess {
                    attachment_lsn: Lsn(1),
                    end_lsn: Lsn(5),
                },
                sampled_at: Timestamp::UNIX_EPOCH,
                recovery: false,
                comparison: InstructionComparison::default(),
            },
        },
        None,
    );
    h.apply(
        7,
        Record::InstructionAttemptBoundarySettled {
            owner: process,
            started_lsn: Lsn(2),
            proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                receipt: InstructionNativeBoundaryReceipt::Spawned { lsn: Lsn(3) },
            },
        },
        None,
    );
    (h, process)
}

#[test]
fn keyed_attached_process_and_missing_turn_evidence_reconstruct_exactly() {
    let (mut h, _) = closed_attachment();
    let mut end = samples::record(RecordKind::TurnEnded);
    if let Record::TurnEnded { mode, .. } = &mut end {
        *mode = Mode::Attached;
    }
    h.apply(8, end, None);
    let end = InstructionEndBoundary::AttachedTurn {
        turn: samples::turn(),
        end_lsn: Lsn(8),
    };
    h.apply(
        9,
        Record::InstructionObservationFinished {
            owner: InstructionObservationOwner::AttachedTurn {
                agent: samples::agent(),
                turn: samples::turn(),
                attachment_lsn: None,
            },
            ticket: None,
            baseline: InstructionBaselineRef::MissingTurnBoundary {
                turn: samples::turn(),
                end_lsn: Lsn(8),
            },
            settlement: InstructionSettlement::Unknown {
                reason: InstructionUnknownReason::AttachedTurnStartMissing,
                end: Some(end),
                sampled_at: None,
                recovery: false,
                comparison: InstructionComparison {
                    changes: Vec::new(),
                    gaps: vec![InstructionCoverageGap {
                        origin: crate::instruction::InstructionGapOrigin::Observed,
                        reason: InstructionGapKind::AttachedTurnStartMissing,
                        context: crate::InstructionPathContext::SharedProject,
                        agent: None,
                        path: None,
                    }],
                    usage: InstructionSampleUsage::default(),
                },
            },
        },
        None,
    );
}

#[test]
fn settled_attachment_keeps_its_exact_attempt_when_a_successor_starts() {
    let (mut h, process) = closed_attachment();
    assert!(h.full.current_for_attachment(Lsn(1)).is_none());
    let (started, retained) = h.full.attempt_for_attachment(Lsn(1)).unwrap();
    assert_eq!(started, Lsn(2));
    assert_eq!(retained.owner, process);
    assert_eq!(retained.finished.as_ref().unwrap().lsn, Lsn(6));
    assert_eq!(retained.boundary_settled.as_ref().unwrap().lsn, Lsn(7));
    h.apply(8, samples::record(RecordKind::Attached), None);
    let successor = InstructionObservationOwner::AttachedProcess {
        agent: samples::agent(),
        attachment_lsn: Lsn(8),
    };
    h.apply(9, start(successor, None), None);
    assert_eq!(h.full.attachment_lsn(samples::agent()), Some(Lsn(8)));
    let (old_started, old) = h.full.attempt_for_attachment(Lsn(1)).unwrap();
    assert_eq!(old_started, Lsn(2));
    assert_eq!(old.owner, process);
    assert!(old.finished.is_some());
    let (new_started, new) = h.full.attempt_for_attachment(Lsn(8)).unwrap();
    assert_eq!(new_started, Lsn(9));
    assert_eq!(new.owner, successor);
    assert!(new.finished.is_none());
}

#[test]
fn live_no_host_assertion_can_reference_its_exact_not_launched_end() {
    for spawned in [false, true] {
        let mut h = Harness::default();
        let owner = owner(samples::turn(), InstructionIssueAssociation::Prospective);
        h.apply(1, start(owner, None), None);
        h.apply(2, samples::record(RecordKind::TurnIssued), None);
        if spawned {
            h.apply(3, samples::record(RecordKind::TurnSpawned), None);
        }
        let mut end = samples::record(RecordKind::TurnEnded);
        if let Record::TurnEnded { outcome, .. } = &mut end {
            *outcome = Some(crate::record::TurnOutcome::NotLaunched {
                reason: "program resolution failed".into(),
            });
        }
        h.apply(4, end, None);
        assert!(h.full.attempt(Lsn(1)).unwrap().finished.is_none());
        let record = Record::InstructionObservationFinished {
            owner,
            ticket: None,
            baseline: InstructionBaselineRef::Started { lsn: Lsn(1) },
            settlement: InstructionSettlement::NoProcess {
                proof_lsn: Lsn(4),
                proof: crate::InstructionNoProcessProof::NoHostCall,
            },
        };
        if spawned {
            let before = h.full.clone();
            let mut cache = InstructionCache::load(&h.memory, h.through, &record, &[]).unwrap();
            let entry = stamp(record);
            assert_eq!(
                cache.state_mut().apply(Lsn(5), &entry, None, hash),
                Err(InstructionStateError::Boundary)
            );
            assert!(cache.updates().unwrap().is_empty());
            assert_eq!(
                h.full.apply(Lsn(5), &entry, None, hash),
                Err(InstructionStateError::Boundary)
            );
            assert_eq!(h.full, before);
        } else {
            h.apply(5, record, None);
            assert!(matches!(
                h.full
                    .attempt(Lsn(1))
                    .unwrap()
                    .finished
                    .as_ref()
                    .unwrap()
                    .settlement,
                InstructionSettlement::NoProcess {
                    proof_lsn: Lsn(4),
                    ..
                }
            ));
        }
    }
}

#[test]
fn one_attempt_cost_is_independent_of_terminal_history_on_its_card() {
    let mut h = Harness::default();
    let ticket = Some(h.ticket_prefix());
    let current = owner(samples::turn(), InstructionIssueAssociation::Prospective);
    h.apply(3, start(current, ticket), None);
    let small = Memory {
        rows: cells(&h.full).unwrap(),
        ..Memory::default()
    };
    for index in 0..128_u64 {
        let lsn = 10 + index * 2;
        let owner = owner(
            TurnId::from_parts(1, u128::from(100 + index)),
            InstructionIssueAssociation::Prospective,
        );
        h.full
            .apply(Lsn(lsn), &stamp(start(owner, ticket)), None, hash)
            .unwrap();
        h.full
            .apply(
                Lsn(lsn + 1),
                &stamp(Record::InstructionObservationFinished {
                    owner,
                    ticket,
                    baseline: InstructionBaselineRef::Started { lsn: Lsn(lsn) },
                    settlement: InstructionSettlement::NoIssuedTurn,
                }),
                None,
                hash,
            )
            .unwrap();
    }
    let large = Memory {
        rows: cells(&h.full).unwrap(),
        ..Memory::default()
    };
    assert!(large.rows.len() > small.rows.len() * 20);
    let entry = stamp(Record::InstructionObservationFinished {
        owner: current,
        ticket,
        baseline: InstructionBaselineRef::Started { lsn: Lsn(3) },
        settlement: InstructionSettlement::NoIssuedTurn,
    });
    let measure = |memory: &Memory| {
        let mut cache = InstructionCache::load(memory, Lsn(300), &entry.record, &[]).unwrap();
        cache
            .state_mut()
            .apply(Lsn(301), &entry, None, hash)
            .unwrap();
        cache
            .updates()
            .unwrap()
            .into_iter()
            .map(|row| (row.key, row.value))
            .collect::<Vec<_>>()
    };
    assert_eq!(measure(&small), measure(&large));
    assert_eq!(*small.reads.borrow(), *large.reads.borrow());
    *large.reads.borrow_mut() = Reads::default();
    let irrelevant = stamp(samples::record(RecordKind::LiveActivityObserved));
    let mut cache = InstructionCache::load(&large, Lsn(300), &irrelevant.record, &[]).unwrap();
    cache
        .state_mut()
        .apply(Lsn(301), &irrelevant, None, hash)
        .unwrap();
    assert!(cache.updates().unwrap().is_empty());
    assert_eq!(*large.reads.borrow(), Reads::default());
}

#[test]
fn malformed_cache_identity_and_missing_baseline_refuse() {
    let mut h = Harness::default();
    let ticket = Some(h.ticket_prefix());
    h.apply(
        3,
        start(
            owner(samples::turn(), InstructionIssueAssociation::Prospective),
            ticket,
        ),
        None,
    );
    let snapshot_key = Key::Snapshot(Lsn(3)).bytes();
    let snapshot = h.memory.rows.remove(&snapshot_key).unwrap();
    assert!(matches!(
        InstructionCache::read_all(&h.memory, Lsn(3)),
        Err(InstructionCacheError::MissingCell)
    ));
    h.memory
        .rows
        .insert(Key::Snapshot(Lsn(4)).bytes(), snapshot);
    assert!(matches!(
        InstructionCache::read_all(&h.memory, Lsn(3)),
        Err(InstructionCacheError::Identity)
    ));
}

#[test]
fn keyed_legacy_issue_keeps_a_single_snapshotless_interval_after_acknowledgment() {
    let mut h = Harness::default();
    let ticket = Some(h.ticket_prefix());
    h.apply(3, samples::record(RecordKind::TurnIssued), None);
    let owner = owner(
        samples::turn(),
        InstructionIssueAssociation::Issued { issue_lsn: Lsn(3) },
    );
    let mut missing = unknown(owner, ticket, 3);
    if let Record::InstructionObservationFinished {
        baseline,
        settlement: InstructionSettlement::Unknown { comparison, .. },
        ..
    } = &mut missing
    {
        *baseline = InstructionBaselineRef::MissingIssueBoundary {
            turn: samples::turn(),
            issue_lsn: Lsn(3),
        };
        comparison.gaps.push(InstructionCoverageGap {
            origin: crate::instruction::InstructionGapOrigin::Observed,
            reason: InstructionGapKind::MissingBaseline,
            context: crate::InstructionPathContext::Unresolved,
            agent: None,
            path: None,
        });
    }
    h.apply(4, missing.clone(), None);
    review(
        &mut h,
        5,
        InstructionReviewAction::Accept {
            completion: InstructionAcceptCompletion::Continue,
        },
        None,
    );
    h.apply(6, missing, None);
    assert_eq!(h.full.attempts().count(), 1);
    assert!(h.full.attempt(Lsn(3)).unwrap().snapshot.is_none());
    assert!(h.full.has_attempt_for_turn(samples::turn()));
    assert_eq!(
        h.full.unresolved_launch_attempts(samples::agent()).count(),
        1
    );
    assert!(!h.memory.rows.contains_key(&Key::Snapshot(Lsn(3)).bytes()));
}
