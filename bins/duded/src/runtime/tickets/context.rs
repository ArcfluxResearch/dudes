use super::{Clock, Daemon, PredicateRunner, ProcessHost, Rng};
use dudes_journal::Reader;
use dudes_proto::diff_stamp::TicketBinding;
use dudes_proto::instruction::{
    InstructionBaselineRef, InstructionEndBoundary, InstructionIssueAssociation,
    InstructionObservationOwner, InstructionSettlement,
};
use dudes_proto::instruction_state::InstructionAttempt;
use dudes_proto::recovery::{RecoveryBoundary, RecoveryPlan};
use dudes_proto::recovery_state::{RecoveryClaimState, RecoveryEpisode};
use dudes_proto::rpc::ticket::{
    TicketAcknowledgedReview, TicketAcknowledgmentAction, TicketCheckApplicability,
    TicketHistoryState, TicketPriorBoundary, TicketPriorReview, TicketReadContext,
    TicketRecoveredRelation, TicketRecoveredReview,
};
use dudes_proto::{
    Actor, CardView, Entry, InstructionAcceptCompletion, InstructionReviewAction, Record,
    RecordKind, TicketRow,
};
use dudes_supervisor::world::World;
use dudes_types::Lsn;

#[cfg(test)]
mod tests;

fn frame(reader: &Reader, at: Lsn, through: Lsn) -> Option<Entry> {
    if at == Lsn::ZERO || at > through {
        return None;
    }
    let (actual, entry) = reader.iter_from(at).ok()?.next()?.ok()?;
    (actual == at).then_some(entry)
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    pub(super) fn ticket_context(
        &mut self,
        card: &CardView,
        ticket: &TicketRow,
    ) -> TicketReadContext {
        let Some(binding) = crate::ticket_recovery::binding(self.sup.world(), card, ticket) else {
            return TicketReadContext::default();
        };
        let mut result = TicketReadContext {
            binding: Some(binding),
            ..TicketReadContext::default()
        };
        if self.ctx.journal_writes_stopped()
            || self.sup.world().last_applied_lsn != Some(self.ctx.projections.last_applied_lsn())
        {
            return result;
        }
        let Ok((state, _)) = self.sup.world().recovery_projection() else {
            return result;
        };
        let (episodes, truncated) = state.ticket_episodes(binding, 32);
        let episodes: Vec<_> = episodes.into_iter().cloned().collect();
        let mut receipts = Vec::new();
        for episode in &episodes {
            let Some(authorization_lsn) = episode.authorization_lsn else {
                continue;
            };
            let claim = dudes_proto::recovery::RecoveryClaimRef {
                episode: episode.id,
                authorization_lsn,
            };
            let Ok(receipt) = state.claim_receipt(claim) else {
                return result;
            };
            receipts.push((claim, receipt.authorization.clone(), receipt.state.clone()));
        }
        let Ok(store) = self.ctx.writer.checked_recovery(&mut self.ctx.projections) else {
            return result;
        };
        for (claim, authorization, state) in &receipts {
            let Ok(Some(checked)) =
                store.receipt(authorization.request_id, authorization.request_digest)
            else {
                return result;
            };
            if checked.claim != *claim
                || checked.authorization != *authorization
                || checked.state != *state
            {
                return result;
            }
        }
        let through = self.ctx.projections.last_applied_lsn();
        let reader = self.ctx.writer.reader();
        result.check = crate::ticket_recovery::check(self.sup.world(), binding);
        if let TicketCheckApplicability::Current { check_lsn }
        | TicketCheckApplicability::Historical { check_lsn, .. } = result.check
        {
            let exact =
                frame(&reader, check_lsn, through).is_some_and(|entry| match entry.record {
                    Record::PredicateResult {
                        goal,
                        provenance: dudes_types::Provenance::Observed,
                        pass,
                        evidence,
                        ..
                    } => {
                        let exit = match evidence {
                            dudes_proto::Evidence::Command { exit, .. } => exit,
                            _ => None,
                        };
                        goal == binding.goal
                            && ticket.check.is_some_and(|check| {
                                check.pass == pass && check.exit == exit && check.at == entry.at
                            })
                    }
                    _ => false,
                });
            if !exact {
                result.check = TicketCheckApplicability::Unknown;
            }
        }
        result.recovered_review = recovered_review(
            self.sup.world(),
            binding,
            ticket,
            &episodes,
            &receipts,
            &reader,
            through,
        );
        if truncated {
            result.recovered_review.state = TicketHistoryState::Truncated;
        }
        result
    }
}

type Receipt = (
    dudes_proto::recovery::RecoveryClaimRef,
    dudes_proto::recovery::RecoveryAuthorized,
    RecoveryClaimState,
);

fn recovered_review(
    world: &World,
    binding: TicketBinding,
    ticket: &TicketRow,
    episodes: &[RecoveryEpisode],
    receipts: &[Receipt],
    reader: &Reader,
    through: Lsn,
) -> TicketRecoveredReview {
    if world.instructions.attempts().take(513).count() > 512 {
        return TicketRecoveredReview {
            state: TicketHistoryState::Truncated,
            ..TicketRecoveredReview::default()
        };
    }
    let mut result = TicketRecoveredReview {
        state: TicketHistoryState::Complete,
        ..TicketRecoveredReview::default()
    };
    let Some(review) = &ticket.instruction_review else {
        return result;
    };
    for interval in &review.intervals {
        let InstructionObservationOwner::Managed { agent, turn, .. } = interval.owner else {
            continue;
        };
        if agent != binding.claimant
            || interval.ticket.card != binding.card
            || interval.ticket.goal != binding.goal
        {
            continue;
        }
        if world
            .instructions
            .association_for_goal(binding.goal, interval.ticket.seq_at_start)
            != Some(interval.ticket)
        {
            result.state = TicketHistoryState::Unavailable;
            continue;
        }
        let Some(attempt) = world.instructions.attempt(baseline_key(interval.baseline)) else {
            result.state = TicketHistoryState::Unavailable;
            continue;
        };
        for (claim, authorization, state) in receipts {
            let Some(issue) = issue_of(state) else {
                continue;
            };
            if issue.issued.work.turn != turn
                || issue.issued.work.agent != agent
                || issue.issued.work.goal != binding.goal
            {
                continue;
            }
            let qualifies = match authorization.plan {
                RecoveryPlan::New { turn: reserved, .. } => {
                    reserved == turn && new_interval(world, attempt, issue.lsn, turn)
                }
                RecoveryPlan::RepeatIssued { .. } => {
                    matches!(attempt.owner, InstructionObservationOwner::Managed { issue: InstructionIssueAssociation::Issued { issue_lsn }, .. } if issue_lsn == issue.lsn)
                        && repeat_interval(world, attempt, state)
                }
            };
            if !qualifies {
                if matches!(authorization.plan, RecoveryPlan::RepeatIssued { .. })
                    && baseline_key(interval.baseline) > claim.authorization_lsn
                {
                    result.state = TicketHistoryState::Unavailable;
                }
                continue;
            }
            if result.relations.len() == 8 {
                result.state = TicketHistoryState::Truncated;
                continue;
            }
            let Some(episode) = episodes.iter().find(|episode| episode.id == claim.episode) else {
                result.state = TicketHistoryState::Unavailable;
                continue;
            };
            let earlier = earlier_attempt(world, binding, episode, reader, through);
            let mut relation = TicketRecoveredRelation {
                interval: interval.baseline,
                turn,
                authorization_lsn: claim.authorization_lsn,
                issue_lsn: issue.lsn,
                earlier: earlier.map(|attempt| attempt.baseline),
                boundary: prior_boundary(earlier),
                acknowledgment: TicketPriorReview::Unavailable,
            };
            if let Some(earlier) = earlier {
                relation.acknowledgment =
                    acknowledgment(&mut result, earlier, binding.card, reader, through);
            } else {
                result.state = TicketHistoryState::Unavailable;
            }
            result.relations.push(relation);
        }
    }
    result
}

fn prior_boundary(attempt: Option<&InstructionAttempt>) -> TicketPriorBoundary {
    let Some(attempt) = attempt else {
        return TicketPriorBoundary::Unavailable;
    };
    if let Some(InstructionSettlement::NoProcess {
        proof_lsn,
        proof: dudes_proto::InstructionNoProcessProof::NoHostCall,
    }) = attempt
        .finished
        .as_ref()
        .map(|finished| &finished.settlement)
    {
        return TicketPriorBoundary::NoHostCall {
            proof_lsn: *proof_lsn,
        };
    }
    attempt
        .boundary_settled
        .as_ref()
        .map_or(TicketPriorBoundary::Unsettled, |settled| {
            TicketPriorBoundary::Settled { lsn: settled.lsn }
        })
}

fn issue_of(state: &RecoveryClaimState) -> Option<&dudes_proto::recovery::RecoveryIssue> {
    match state {
        RecoveryClaimState::IssueBound(issue)
        | RecoveryClaimState::Consumed { issue, .. }
        | RecoveryClaimState::Superseded { issue, .. } => Some(issue),
        _ => None,
    }
}

fn baseline_key(baseline: InstructionBaselineRef) -> Lsn {
    match baseline {
        InstructionBaselineRef::Started { lsn } => lsn,
        InstructionBaselineRef::MissingTurnBoundary { end_lsn, .. } => end_lsn,
        InstructionBaselineRef::MissingIssueBoundary { issue_lsn, .. } => issue_lsn,
    }
}

fn new_interval(
    world: &World,
    attempt: &InstructionAttempt,
    issue: Lsn,
    turn: dudes_types::TurnId,
) -> bool {
    let InstructionBaselineRef::Started { lsn } = attempt.baseline else {
        return false;
    };
    if lsn >= issue
        || matches!(
            attempt
                .finished
                .as_ref()
                .map(|finished| &finished.settlement),
            Some(InstructionSettlement::NoIssuedTurn)
        )
    {
        return false;
    }
    world.instructions.attempts().filter(|(started, row)| **started < issue && matches!(row.owner, InstructionObservationOwner::Managed { turn: actual, issue: InstructionIssueAssociation::Prospective, .. } if actual == turn)
        && !matches!(row.finished.as_ref().map(|finished| &finished.settlement), Some(InstructionSettlement::NoIssuedTurn)))
        .max_by_key(|(started, _)| **started).is_some_and(|(_, row)| row.baseline == attempt.baseline)
}

fn repeat_interval(
    world: &World,
    attempt: &InstructionAttempt,
    state: &RecoveryClaimState,
) -> bool {
    let RecoveryClaimState::Consumed { evidence_lsn, .. } = state else {
        return false;
    };
    if attempt.spawn_lsn == Some(*evidence_lsn) {
        return world
            .recovery_witness(*evidence_lsn)
            .is_ok_and(|source| source.reference().kind == RecordKind::TurnSpawned);
    }
    attempt
        .finished
        .as_ref()
        .is_some_and(|finished| match &finished.settlement {
            InstructionSettlement::Compared { end, .. }
            | InstructionSettlement::Unknown { end: Some(end), .. } => {
                end_lsn(*end) == *evidence_lsn
            }
            InstructionSettlement::NoProcess { proof_lsn, .. } => *proof_lsn == *evidence_lsn,
            _ => false,
        })
}

fn end_lsn(end: InstructionEndBoundary) -> Lsn {
    match end {
        InstructionEndBoundary::ManagedTurn { end_lsn, .. }
        | InstructionEndBoundary::AttachedProcess { end_lsn, .. }
        | InstructionEndBoundary::AttachedTurn { end_lsn, .. } => end_lsn,
    }
}

fn earlier_attempt<'a>(
    world: &'a World,
    binding: TicketBinding,
    episode: &RecoveryEpisode,
    reader: &Reader,
    through: Lsn,
) -> Option<&'a InstructionAttempt> {
    let entry = frame(reader, episode.barrier_lsn, through)?;
    let Record::RecoveryRequired(required) = entry.record else {
        return None;
    };
    if required.binding != episode.binding {
        return None;
    }
    let candidates: Vec<_> = world
        .instructions
        .attempts()
        .filter_map(|(_, attempt)| {
            let association = attempt.ticket?;
            if association.card != binding.card || association.goal != binding.goal {
                return None;
            }
            if world
                .instructions
                .association_for_goal(binding.goal, association.seq_at_start)
                != Some(association)
            {
                return None;
            }
            let InstructionObservationOwner::Managed { agent, turn, .. } = attempt.owner else {
                return None;
            };
            if agent != binding.claimant || turn != episode.binding.work.turn {
                return None;
            }
            let matches = match required.boundary {
                RecoveryBoundary::Vanished { spawn_lsn, .. }
                | RecoveryBoundary::Unconfirmed { spawn_lsn, .. } => {
                    attempt.spawn_lsn == Some(spawn_lsn)
                }
                RecoveryBoundary::Held { end_lsn: at, .. }
                | RecoveryBoundary::Paused { end_lsn: at } => attempt
                    .finished
                    .as_ref()
                    .is_some_and(|finished| match &finished.settlement {
                        InstructionSettlement::Compared { end, .. }
                        | InstructionSettlement::Unknown { end: Some(end), .. } => {
                            end_lsn(*end) == at
                        }
                        InstructionSettlement::NoProcess { proof_lsn, .. } => *proof_lsn == at,
                        _ => false,
                    }),
                RecoveryBoundary::MissingSpawn => {
                    baseline_key(attempt.baseline) < episode.barrier_lsn
                        && attempt.spawn_lsn.is_none()
                        && match attempt.owner {
                            InstructionObservationOwner::Managed {
                                issue: InstructionIssueAssociation::Issued { issue_lsn },
                                ..
                            } => issue_lsn == episode.binding.issue_lsn,
                            InstructionObservationOwner::Managed {
                                issue: InstructionIssueAssociation::Prospective,
                                ..
                            } => new_interval(world, attempt, episode.binding.issue_lsn, turn),
                            _ => false,
                        }
                }
            };
            matches.then_some(attempt)
        })
        .collect();
    (candidates.len() == 1).then(|| candidates[0])
}

fn acknowledgment(
    result: &mut TicketRecoveredReview,
    attempt: &InstructionAttempt,
    card: dudes_types::CardId,
    reader: &Reader,
    through: Lsn,
) -> TicketPriorReview {
    let Some(lsn) = attempt.reviewed_lsn else {
        return TicketPriorReview::None;
    };
    if let Some(existing) = result.acknowledgments.iter_mut().find(|ack| ack.lsn == lsn) {
        if !existing.intervals.contains(&attempt.baseline) {
            existing.intervals.push(attempt.baseline);
        }
        return TicketPriorReview::Found { lsn };
    }
    if result.acknowledgments.len() == 4 {
        result.state = TicketHistoryState::Truncated;
        return TicketPriorReview::Truncated;
    }
    let Some(entry) = frame(reader, lsn, through) else {
        result.state = TicketHistoryState::Unavailable;
        return TicketPriorReview::Unavailable;
    };
    let Record::TicketInstructionReviewed {
        card: actual,
        by,
        expected_seq,
        watermark,
        action,
    } = entry.record
    else {
        result.state = TicketHistoryState::Unavailable;
        return TicketPriorReview::Unavailable;
    };
    if actual != card || !matches!(by, Actor::Peer { .. } | Actor::BrowserSession { .. }) {
        result.state = TicketHistoryState::Unavailable;
        return TicketPriorReview::Unavailable;
    }
    let action = match action {
        InstructionReviewAction::Accept {
            completion: InstructionAcceptCompletion::Continue,
        } => TicketAcknowledgmentAction::AcceptedContinuing,
        InstructionReviewAction::Accept {
            completion: InstructionAcceptCompletion::Completed { .. },
        } => TicketAcknowledgmentAction::AcceptedCompleted,
        InstructionReviewAction::ChangesRequested { .. } => {
            TicketAcknowledgmentAction::ChangesRequested
        }
    };
    result.acknowledgments.push(TicketAcknowledgedReview {
        lsn,
        expected_seq,
        watermark,
        at: entry.at,
        action,
        intervals: vec![attempt.baseline],
    });
    TicketPriorReview::Found { lsn }
}
