use dudes_proto::instruction_state::cache::{
    InstructionCache, InstructionCacheReader, InstructionCacheRow,
};
use dudes_proto::instruction_state::{InstructionFold, InstructionReviewFacts, InstructionState};
use dudes_proto::{Entry, Record};
use dudes_types::{GoalStatus, Lsn};
use redb::{ReadableTable, Table};

use super::board::BoardRow;
use super::{AgentRow, GoalRow, RowTable, get_row};
use crate::error::{Error, Result};

pub(super) type InstructionTable<'txn> = Table<'txn, &'static [u8], &'static [u8]>;

struct CacheReader<'a, T>(&'a T);

impl<T: ReadableTable<&'static [u8], &'static [u8]>> InstructionCacheReader for CacheReader<'_, T> {
    type Error = Error;

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(key)?.map(|value| value.value().to_vec()))
    }

    fn prefix(&self, prefix: &[u8]) -> Result<Vec<InstructionCacheRow>> {
        let mut rows = Vec::new();
        for row in self.0.range(prefix..)? {
            let (key, value) = row?;
            if !key.value().starts_with(prefix) {
                break;
            }
            rows.push((key.value().to_vec(), value.value().to_vec()));
        }
        Ok(rows)
    }
}

pub(super) fn hash(bytes: &[u8]) -> dudes_proto::record::Hex32 {
    dudes_proto::record::Hex32::new(*blake3::hash(bytes).as_bytes())
}

pub(super) fn read(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    through: Lsn,
) -> Result<InstructionState> {
    InstructionCache::read_all(&CacheReader(table), through)
}

pub(super) fn load(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    through: Lsn,
    entry: &Entry,
    cards: &[dudes_types::CardId],
) -> Result<InstructionCache> {
    InstructionCache::load(&CacheReader(table), through, &entry.record, cards)
}

pub(super) fn facts(
    agents: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    board: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    goals: &impl ReadableTable<&'static [u8; 16], &'static [u8]>,
    entry: &Entry,
) -> Result<Option<InstructionReviewFacts>> {
    let Record::TicketInstructionReviewed { card, .. } = &entry.record else {
        return Ok(None);
    };
    let Some(row) = get_row::<BoardRow>(board, &card.to_bytes())? else {
        return Ok(None);
    };
    let Some(ticket) = row.ticket else {
        return Ok(None);
    };
    let goal = match ticket.goal {
        Some(goal) => get_row::<GoalRow>(goals, &goal.to_bytes())?,
        None => None,
    };
    let completed_lsn = goal
        .as_ref()
        .and_then(|row| row.first_done.map(|done| done.lsn));
    let clear_goal = match (row.view.claimed_by, goal.as_ref()) {
        (Some(agent), Some(goal))
            if goal.agent == agent
                && goal.status != GoalStatus::Abandoned
                && completed_lsn.is_none()
                && get_row::<AgentRow>(agents, &agent.to_bytes())?
                    .is_some_and(|row| row.goal == Some(goal.id)) =>
        {
            Some(dudes_proto::InstructionGoalClear {
                agent,
                goal: goal.id,
            })
        }
        _ => None,
    };
    Ok(Some(InstructionReviewFacts {
        card: *card,
        seq: row.view.seq,
        goal: ticket.goal,
        completed_lsn,
        clear_goal,
    }))
}

pub(super) fn apply(
    table: &mut InstructionTable<'_>,
    agents: &RowTable<'_>,
    board: &RowTable<'_>,
    goals: &RowTable<'_>,
    through: Lsn,
    lsn: Lsn,
    entry: &Entry,
) -> Result<(InstructionCache, InstructionFold)> {
    let mut cards = Vec::new();
    if let Record::AgentRetired { id, .. } = &entry.record {
        for row in super::all_rows::<BoardRow>(board)? {
            if row.view.claimed_by == Some(*id)
                && row
                    .ticket
                    .as_ref()
                    .is_some_and(|ticket| ticket.instruction_review.is_some())
            {
                cards.push(row.view.id);
            }
        }
    }
    let mut cache = load(table, through, entry, &cards)?;
    let folded = cache
        .state_mut()
        .apply(lsn, entry, facts(agents, board, goals, entry)?, hash)
        .map_err(|reason| Error::Instruction { lsn, reason })?;
    for update in cache.updates()? {
        match update.value {
            Some(value) => {
                table.insert(update.key.as_slice(), value.as_slice())?;
            }
            None => {
                table.remove(update.key.as_slice())?;
            }
        }
    }
    Ok((cache, folded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Projections;
    use dudes_proto::record::samples;
    use dudes_proto::{
        Actor, InstructionAcceptCompletion, InstructionBaselineRef, InstructionComparison,
        InstructionCoverageGap, InstructionGapKind, InstructionObservationOwner,
        InstructionPathContext, InstructionReviewAction, InstructionSettlement,
        InstructionTicketAssociation, InstructionUnknownReason, RecordKind,
    };
    use dudes_types::{CardStatus, Seq, Timestamp};

    fn stamp(record: Record) -> Entry {
        Entry {
            at: Timestamp::UNIX_EPOCH,
            record,
        }
    }

    fn prefix() -> (
        Projections,
        dudes_types::CardId,
        InstructionObservationOwner,
    ) {
        let mut projections = Projections::in_memory().unwrap();
        let created = samples::record(RecordKind::TicketCreated);
        let Record::TicketCreated { card, .. } = created else {
            unreachable!()
        };
        let mut assigned = samples::record(RecordKind::TicketAssigned);
        if let Record::TicketAssigned { expected_seq, .. } = &mut assigned {
            *expected_seq = Seq(0);
        }
        let mut started = samples::record(RecordKind::TicketStarted);
        let goal = if let Record::TicketStarted {
            expected_seq, goal, ..
        } = &mut started
        {
            *expected_seq = Seq(1);
            goal.id
        } else {
            unreachable!()
        };
        for (position, record) in [
            (1, samples::record(RecordKind::AgentHired)),
            (2, created),
            (3, assigned),
            (4, started),
        ] {
            projections.apply(Lsn(position), 7, &stamp(record)).unwrap();
        }
        let mut observation = samples::record(RecordKind::InstructionObservationStarted);
        let owner =
            if let Record::InstructionObservationStarted { owner, ticket, .. } = &mut observation {
                *ticket = Some(InstructionTicketAssociation {
                    card,
                    goal,
                    assignment_lsn: Lsn(3),
                    seq_at_start: Seq(2),
                });
                *owner
            } else {
                unreachable!()
            };
        projections.apply(Lsn(5), 7, &stamp(observation)).unwrap();
        projections
            .apply(Lsn(6), 7, &stamp(samples::record(RecordKind::TurnIssued)))
            .unwrap();
        projections
            .apply(Lsn(7), 7, &stamp(samples::record(RecordKind::TurnEnded)))
            .unwrap();
        let ended = Record::InstructionObservationFinished {
            owner,
            ticket: Some(InstructionTicketAssociation {
                card,
                goal,
                assignment_lsn: Lsn(3),
                seq_at_start: Seq(2),
            }),
            baseline: InstructionBaselineRef::Started { lsn: Lsn(5) },
            settlement: InstructionSettlement::Unknown {
                reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                end: None,
                sampled_at: None,
                recovery: true,
                comparison: InstructionComparison {
                    changes: Vec::new(),
                    gaps: vec![InstructionCoverageGap {
                        reason: InstructionGapKind::UnrecordedLaunchBoundary,
                        context: InstructionPathContext::SharedProject,
                        agent: None,
                        path: None,
                    }],
                    usage: dudes_proto::InstructionSampleUsage::default(),
                },
            },
        };
        projections.apply(Lsn(8), 7, &stamp(ended)).unwrap();
        (projections, card, owner)
    }

    #[test]
    fn atomic_acknowledgement_advances_sequence_and_keeps_unresolved_native_boundary() {
        let (mut projections, card, _) = prefix();
        let (view, ticket) = projections.ticket(card).unwrap().unwrap();
        assert_eq!(view.status, CardStatus::Blocked);
        assert_eq!(view.seq, Seq(5));
        assert_eq!(
            ticket.reason,
            Some(dudes_proto::TicketReason::SharedInstructionsUnknown)
        );
        let watermark = ticket.instruction_review.unwrap().watermark;
        let entry = stamp(Record::TicketInstructionReviewed {
            card,
            by: Actor::Peer { pid: 77, uid: None },
            expected_seq: Seq(5),
            watermark,
            action: InstructionReviewAction::Accept {
                completion: InstructionAcceptCompletion::Continue,
            },
        });
        let before = projections.export().unwrap();
        projections.preflight_instruction(Lsn(9), &entry).unwrap();
        assert_eq!(projections.export().unwrap(), before);
        projections.apply(Lsn(9), 7, &entry).unwrap();
        let (view, ticket) = projections.ticket(card).unwrap().unwrap();
        assert_eq!(view.status, CardStatus::Doing);
        assert_eq!(view.seq, Seq(6));
        assert_eq!(ticket.instruction_review, None);
        let state = projections.instruction_state().unwrap();
        let InstructionObservationOwner::Managed { agent, .. } =
            state.attempts().next().unwrap().1.owner
        else {
            unreachable!();
        };
        assert_eq!(state.unresolved_launch_attempts(agent).count(), 1);
        assert!(matches!(
            projections.preflight_instruction(Lsn(10), &entry),
            Err(Error::Instruction {
                reason: dudes_proto::instruction_state::InstructionStateError::StaleCard,
                ..
            })
        ));
    }

    #[test]
    fn qualified_completion_remains_recorded_while_instruction_promotion_waits() {
        let (mut projections, card, _) = prefix();
        let mut predicate = samples::record(RecordKind::PredicateResult);
        if let Record::PredicateResult {
            pass, provenance, ..
        } = &mut predicate
        {
            *pass = true;
            *provenance = dudes_types::Provenance::Observed;
        }
        projections.apply(Lsn(9), 7, &stamp(predicate)).unwrap();
        let mut done = samples::record(RecordKind::LoopDecided);
        if let Record::LoopDecided { decision, .. } = &mut done {
            *decision = dudes_proto::LoopDecision::Done;
        }
        projections.apply(Lsn(10), 7, &stamp(done)).unwrap();
        let (view, ticket) = projections.ticket(card).unwrap().unwrap();
        assert_eq!(view.status, CardStatus::Blocked);
        assert_eq!(
            projections
                .goal(ticket.goal.unwrap())
                .unwrap()
                .unwrap()
                .first_done
                .unwrap()
                .lsn,
            Lsn(10)
        );
        let entry = stamp(Record::TicketInstructionReviewed {
            card,
            by: Actor::Peer { pid: 77, uid: None },
            expected_seq: view.seq,
            watermark: ticket.instruction_review.unwrap().watermark,
            action: InstructionReviewAction::Accept {
                completion: InstructionAcceptCompletion::Completed {
                    completion_lsn: Lsn(10),
                },
            },
        });
        projections.apply(Lsn(11), 7, &entry).unwrap();
        assert_eq!(
            projections.card(card).unwrap().unwrap().status,
            CardStatus::Done
        );
    }

    #[test]
    fn atomic_changes_requested_uses_the_existing_rework_queue_semantics() {
        let (mut projections, card, _) = prefix();
        let (view, ticket) = projections.ticket(card).unwrap().unwrap();
        let holder = view.claimed_by.unwrap();
        let old_goal = ticket.goal.unwrap();
        let entry = stamp(Record::TicketInstructionReviewed {
            card,
            by: Actor::Peer { pid: 77, uid: None },
            expected_seq: view.seq,
            watermark: ticket.instruction_review.unwrap().watermark,
            action: InstructionReviewAction::ChangesRequested {
                agent: holder,
                message: Some("Check the shared file.".into()),
                clear_goal: Some(dudes_proto::InstructionGoalClear {
                    agent: holder,
                    goal: old_goal,
                }),
            },
        });
        projections.apply(Lsn(9), 7, &entry).unwrap();
        let (view, ticket) = projections.ticket(card).unwrap().unwrap();
        assert_eq!(view.status, CardStatus::Claimed);
        assert_eq!(view.seq, Seq(6));
        assert_eq!(ticket.goal, None);
        assert_eq!(
            ticket.queue.unwrap(),
            dudes_proto::QueueKey {
                class: dudes_proto::QueueClass::Rework,
                lsn: Lsn(9)
            }
        );
        assert_eq!(ticket.message.as_deref(), Some("Check the shared file."));
        assert_eq!(
            projections.goal(old_goal).unwrap().unwrap().status,
            GoalStatus::Abandoned
        );
        assert_eq!(projections.agent(holder).unwrap().unwrap().goal, None);
    }

    #[test]
    fn rework_refuses_stale_goal_clearing_without_changing_any_projection() {
        let (mut projections, card, _) = prefix();
        let (view, ticket) = projections.ticket(card).unwrap().unwrap();
        let goal = ticket.goal.unwrap();
        let holder = view.claimed_by.unwrap();
        let successor = dudes_types::AgentId::from_parts(1, 99);
        let mut hire = samples::record(RecordKind::AgentHired);
        if let Record::AgentHired { id, .. } = &mut hire {
            *id = successor;
        }
        projections.apply(Lsn(9), 7, &stamp(hire)).unwrap();
        projections
            .apply(
                Lsn(10),
                7,
                &stamp(Record::GoalReassigned {
                    goal,
                    from: holder,
                    to: successor,
                    reason: None,
                }),
            )
            .unwrap();
        let before = projections.export().unwrap();
        let mut entry = stamp(Record::TicketInstructionReviewed {
            card,
            by: Actor::Peer { pid: 77, uid: None },
            expected_seq: view.seq,
            watermark: ticket.instruction_review.unwrap().watermark,
            action: InstructionReviewAction::ChangesRequested {
                agent: holder,
                message: None,
                clear_goal: Some(dudes_proto::InstructionGoalClear {
                    agent: successor,
                    goal,
                }),
            },
        });
        assert!(projections.apply(Lsn(11), 7, &entry).is_err());
        assert_eq!(projections.export().unwrap(), before);
        if let Record::TicketInstructionReviewed {
            action: InstructionReviewAction::ChangesRequested { clear_goal, .. },
            ..
        } = &mut entry.record
        {
            *clear_goal = None;
        }
        projections.apply(Lsn(11), 7, &entry).unwrap();
        assert_eq!(
            projections.agent(successor).unwrap().unwrap().goal,
            Some(goal)
        );
        assert_ne!(
            projections.goal(goal).unwrap().unwrap().status,
            GoalStatus::Abandoned
        );
    }

    #[test]
    fn rework_preserves_an_already_observed_completion() {
        let (mut projections, card, _) = prefix();
        let mut predicate = samples::record(RecordKind::PredicateResult);
        if let Record::PredicateResult {
            pass, provenance, ..
        } = &mut predicate
        {
            *pass = true;
            *provenance = dudes_types::Provenance::Observed;
        }
        projections.apply(Lsn(9), 7, &stamp(predicate)).unwrap();
        let mut done = samples::record(RecordKind::LoopDecided);
        if let Record::LoopDecided { decision, .. } = &mut done {
            *decision = dudes_proto::LoopDecision::Done;
        }
        projections.apply(Lsn(10), 7, &stamp(done)).unwrap();
        let (view, ticket) = projections.ticket(card).unwrap().unwrap();
        let goal = ticket.goal.unwrap();
        let holder = view.claimed_by.unwrap();
        let before = projections.goal(goal).unwrap().unwrap();
        assert!(before.first_done.is_some());
        let entry = stamp(Record::TicketInstructionReviewed {
            card,
            by: Actor::Peer { pid: 77, uid: None },
            expected_seq: view.seq,
            watermark: ticket.instruction_review.unwrap().watermark,
            action: InstructionReviewAction::ChangesRequested {
                agent: holder,
                message: None,
                clear_goal: None,
            },
        });
        projections.apply(Lsn(11), 7, &entry).unwrap();
        assert_eq!(projections.goal(goal).unwrap().unwrap(), before);
        assert_eq!(projections.agent(holder).unwrap().unwrap().goal, Some(goal));
    }
}
