use dudes_proto::diff_stamp::TicketBinding;
use dudes_proto::instruction::{
    InstructionIssueAssociation, InstructionObservationOwner, InstructionSettlement,
};
use dudes_proto::recovery::{RecoveryPlan, RecoveryWork};
use dudes_proto::recovery_state::{RecoveryClaimState, RecoveryEpisode};
use dudes_proto::rpc::ticket::{TicketCheckApplicability, TicketHistoricalCheckReason};
use dudes_proto::{CardView, RecordKind, TicketRow};
use dudes_supervisor::world::World;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryProgress {
    None,
    Waiting,
    Admitted,
    Unavailable,
}

pub(crate) fn binding(world: &World, card: &CardView, ticket: &TicketRow) -> Option<TicketBinding> {
    let target = TicketBinding {
        card: card.id,
        round: ticket.round,
        goal: ticket.goal?,
        claimant: card.claimed_by?,
    };
    let goal = world.goals.get(&target.goal)?;
    (world.ticket_goal(target.goal) == Some((target.card, target.round))
        && goal.agent == target.claimant
        && world.agents.get(&target.claimant)?.goal == Some(target.goal))
    .then_some(target)
}

pub(crate) fn progress(world: &World, target: TicketBinding) -> RecoveryProgress {
    let Ok((state, _)) = world.recovery_projection() else {
        return RecoveryProgress::Unavailable;
    };
    let works = world
        .turns
        .iter()
        .filter(|(_, turn)| turn.agent == target.claimant && turn.goal == Some(target.goal))
        .map(|(turn, _)| RecoveryWork {
            agent: target.claimant,
            goal: target.goal,
            turn: *turn,
        });
    let Some(episode) = state.latest_ticket_episode(target, works) else {
        return RecoveryProgress::None;
    };
    episode_progress(world, episode)
}

fn episode_progress(world: &World, episode: &RecoveryEpisode) -> RecoveryProgress {
    let Some(authorization_lsn) = episode.authorization_lsn else {
        return RecoveryProgress::Waiting;
    };
    let Ok((state, _)) = world.recovery_projection() else {
        return RecoveryProgress::Unavailable;
    };
    let Ok(receipt) = state.claim_receipt(dudes_proto::recovery::RecoveryClaimRef {
        episode: episode.id,
        authorization_lsn,
    }) else {
        return RecoveryProgress::Unavailable;
    };
    let (RecoveryClaimState::IssueBound(issue) | RecoveryClaimState::Consumed { issue, .. }) =
        receipt.state
    else {
        return RecoveryProgress::Waiting;
    };
    if matches!(receipt.authorization.plan, RecoveryPlan::New { .. }) {
        return if world
            .recovery_witness(issue.lsn)
            .ok()
            .and_then(dudes_proto::recovery_state::history::RecoveryWitness::issue)
            .as_ref()
            == Some(issue)
        {
            RecoveryProgress::Admitted
        } else {
            RecoveryProgress::Unavailable
        };
    }
    let RecoveryClaimState::Consumed { evidence_lsn, .. } = receipt.state else {
        return RecoveryProgress::Waiting;
    };
    if world
        .recovery_witness(*evidence_lsn)
        .is_ok_and(|source| source.reference().kind == RecordKind::TurnSpawned)
    {
        RecoveryProgress::Admitted
    } else {
        RecoveryProgress::Waiting
    }
}

pub(crate) fn check(world: &World, target: TicketBinding) -> TicketCheckApplicability {
    use TicketCheckApplicability::{Current, Historical, Unknown};
    if world.turns.len() > 512 || world.instructions.attempts().take(513).count() > 512 {
        return Unknown;
    }
    let Some(goal) = world
        .goals
        .get(&target.goal)
        .filter(|goal| goal.agent == target.claimant)
    else {
        return Unknown;
    };
    let Some(check_lsn) = goal.predicate_lsn else {
        return Unknown;
    };
    let Some(review) = world.ticket_review(target.goal) else {
        return Unknown;
    };
    let Some((_, stamp)) = &review.stamp else {
        return Unknown;
    };
    if review.baseline.binding != target
        || stamp.binding != target
        || stamp.baseline_lsn != review.baseline_lsn
        || stamp.check_lsn != check_lsn
    {
        return Unknown;
    }
    let turns = world
        .turns
        .values()
        .filter(|turn| turn.agent == target.claimant && turn.goal == Some(target.goal));
    let mut uncertain = false;
    for turn in turns {
        if turn.issue_lsn > check_lsn {
            return Historical {
                check_lsn,
                reason: TicketHistoricalCheckReason::NewIssue,
            };
        }
        if turn.spawn_lsn.is_some_and(|lsn| lsn > check_lsn) {
            return Historical {
                check_lsn,
                reason: TicketHistoricalCheckReason::LaterAttempt,
            };
        }
    }
    for (_, attempt) in world.instructions.attempts() {
        let Some(ticket) = attempt.ticket else {
            continue;
        };
        if ticket.card != target.card || ticket.goal != target.goal {
            continue;
        }
        if world
            .instructions
            .association_for_goal(target.goal, ticket.seq_at_start)
            != Some(ticket)
        {
            return Unknown;
        }
        let InstructionObservationOwner::Managed {
            agent,
            issue: InstructionIssueAssociation::Issued { issue_lsn },
            ..
        } = attempt.owner
        else {
            continue;
        };
        if agent != target.claimant || issue_lsn > check_lsn {
            continue;
        }
        let dudes_proto::InstructionBaselineRef::Started { lsn } = attempt.baseline else {
            continue;
        };
        if lsn <= check_lsn {
            continue;
        }
        if attempt.spawn_lsn.is_some_and(|spawn| spawn > check_lsn) {
            return Historical {
                check_lsn,
                reason: TicketHistoricalCheckReason::LaterAttempt,
            };
        }
        if !matches!(
            attempt.finished.as_ref().map(|end| &end.settlement),
            Some(InstructionSettlement::NoProcess {
                proof: dudes_proto::InstructionNoProcessProof::NoHostCall,
                ..
            })
        ) {
            uncertain = true;
        }
    }
    if uncertain {
        Unknown
    } else {
        Current { check_lsn }
    }
}
