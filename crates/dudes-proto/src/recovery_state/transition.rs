//! Strict joins for unregistered typed recovery preparation, without caller authentication.

use dudes_types::{Lsn, Mode, Provenance};

use super::history::{
    RecoveryEndClass, RecoveryEvidence, RecoveryHistory, RecoveryHoldWitness,
    RecoveryStandingControl, RecoveryWitness, RecoveryWitnessOwner,
};
use super::{
    RecoveryAttemptFacts, RecoveryEvent, RecoveryFacts, RecoveryState, RecoveryStateError,
};
use crate::RecordKind;
use crate::recovery::{
    RecoveryAttribution, RecoveryAuthorized, RecoveryBinding, RecoveryBoundary, RecoveryClaimRef,
    RecoveryContext, RecoveryInvalidated, RecoveryInvalidationEvidence, RecoveryInvalidationReason,
    RecoveryIssue, RecoveryOrigin, RecoveryPlan, RecoveryReason, RecoveryRequired, RecoveryWork,
};

#[cfg(test)]
mod tests;

/// Typed inputs only. Journal-kind recognition and authenticated command admission are separate.
#[derive(Clone, Copy, Debug)]
pub enum RecoveryTransition<'a> {
    /// Establish one exact ordinary-history boundary.
    Required(&'a RecoveryRequired),
    /// Record immutable consent intent after a separately authenticated serialized command.
    Authorized(&'a RecoveryAuthorized),
    /// Close unused consent on exact evidence, without needing an active binding afterward.
    Invalidated(&'a RecoveryInvalidated),
    /// Bind the actual ordinary issue produced by admission.
    Issued {
        /// Exact owner of the reserved turn.
        claim: RecoveryClaimRef,
        /// Actual issue extracted from its original entry.
        issue: &'a RecoveryIssue,
    },
    /// Consume one exact physical attempt, without fabricating its end.
    Consumed {
        /// Exact issue-bound claim.
        claim: RecoveryClaimRef,
        /// Immutable owed issue.
        issue: &'a RecoveryIssue,
        /// Actual spawn or observed managed terminal evidence with before-record context.
        evidence: &'a RecoveryWitness,
    },
}

impl RecoveryTransition<'_> {
    /// Target needed by the current existing folds, without changing the original authorization.
    pub fn binding(self, state: &RecoveryState) -> Result<RecoveryBinding, RecoveryStateError> {
        match self {
            Self::Required(record) => Ok(record.binding.clone()),
            Self::Authorized(record) => Ok(record.binding.clone()),
            Self::Invalidated(record) => Ok(state.claim(record.claim)?.0.binding.clone()),
            Self::Issued { claim, .. } => Ok(state.claim(claim)?.0.binding.clone()),
            Self::Consumed { claim, issue, .. } => {
                let mut binding = state.claim(claim)?.0.binding.clone();
                binding.work = issue.issued.work;
                binding.issue_lsn = issue.lsn;
                Ok(binding)
            }
        }
    }

    /// Fixed explicit source closure; adapters load these from their actual certified prefix.
    #[must_use]
    pub fn sources(self, facts: &RecoveryFacts) -> Vec<Lsn> {
        let mut positions = vec![facts.binding.hired_lsn, facts.source_issue.lsn];
        positions.extend(facts.control_lsn);
        positions.extend(facts.attempt.spawn_lsn);
        positions.extend(facts.attempt.end_lsn);
        positions.extend(facts.attempt.confirmation_lsn);
        match self {
            Self::Required(record) => {
                if let RecoveryOrigin::Existing { lsn } = record.origin {
                    positions.push(lsn);
                }
                match record.boundary {
                    RecoveryBoundary::Held { end_lsn, first } => {
                        positions.push(end_lsn);
                        if let Some(first) = first {
                            positions.extend([first.issue_lsn, first.end_lsn]);
                        }
                    }
                    RecoveryBoundary::Paused { end_lsn } => positions.push(end_lsn),
                    RecoveryBoundary::Vanished { spawn_lsn, end_lsn } => {
                        positions.extend([spawn_lsn, end_lsn]);
                    }
                    RecoveryBoundary::Unconfirmed {
                        spawn_lsn,
                        end_lsn,
                        requested_confirmation_lsn,
                    } => positions.extend([spawn_lsn, end_lsn, requested_confirmation_lsn]),
                    RecoveryBoundary::MissingSpawn => {}
                }
            }
            Self::Authorized(record) => {
                positions.extend(record.expected_control);
                positions.extend(record.resume_lsn);
                if let RecoveryPlan::New {
                    context: RecoveryContext::Fresh { recap, .. },
                    ..
                } = &record.plan
                {
                    positions.extend(&recap.sources);
                    for delivery in &recap.delivered_answers {
                        positions.extend([
                            delivery.answer_lsn,
                            delivery.issue_lsn,
                            delivery.spawn_lsn,
                        ]);
                    }
                }
            }
            Self::Invalidated(record) => {
                if let RecoveryInvalidationEvidence::RecordedChange { lsn, .. } = record.evidence {
                    positions.push(lsn);
                }
            }
            Self::Issued { issue, .. } => positions.extend(&issue.issued.answers),
            Self::Consumed { .. } => {}
        }
        positions.sort_unstable();
        positions.dedup();
        positions
    }
}

impl RecoveryState {
    /// Refuse inconsistent source joins before any mutable fold or append.
    /// This check cannot authenticate a client, certify a store, or grant dispatch.
    pub fn validate_transition(
        &self,
        transition: RecoveryTransition<'_>,
        facts: &RecoveryFacts,
        history: &RecoveryHistory,
    ) -> Result<(), RecoveryStateError> {
        let event = match transition {
            RecoveryTransition::Required(record) => {
                validate_required(record, facts, history)?;
                RecoveryEvent::Required(record)
            }
            RecoveryTransition::Authorized(record) => {
                validate_authorized(record, facts, history)?;
                RecoveryEvent::Authorized(record)
            }
            RecoveryTransition::Invalidated(record) => {
                return self.validate_invalidation(record, facts.through, history);
            }
            RecoveryTransition::Issued { claim, issue } => {
                if issue.lsn <= facts.through {
                    return Err(RecoveryStateError::Position);
                }
                for answer in &issue.issued.answers {
                    let seen = prior(history, facts, *answer)?;
                    if seen.reference().kind != RecordKind::HumanAnswer
                        || seen.reference().owner
                            != RecoveryWitnessOwner::Agent(issue.issued.work.agent)
                    {
                        return Err(RecoveryStateError::Binding);
                    }
                }
                RecoveryEvent::Issued {
                    claim,
                    issued: &issue.issued,
                }
            }
            RecoveryTransition::Consumed {
                claim,
                issue,
                evidence,
            } => {
                return self.validate_consumed(
                    claim,
                    issue,
                    evidence,
                    &RecoveryAttemptFacts {
                        through: facts.through,
                        issue: facts.source_issue.clone(),
                        attempt: facts.attempt,
                    },
                );
            }
        };
        self.validate_next(event, facts)
    }

    /// Apply a source-validated typed event at its actual position. No record is registered here.
    pub fn apply_transition(
        &mut self,
        lsn: Lsn,
        transition: RecoveryTransition<'_>,
        facts: &RecoveryFacts,
        history: &RecoveryHistory,
    ) -> Result<(), RecoveryStateError> {
        if lsn <= self.last_applied {
            return Ok(());
        }
        if !matches!(transition, RecoveryTransition::Invalidated(_)) {
            self.validate_transition(transition, facts, history)?;
        }
        let event = match transition {
            RecoveryTransition::Required(record) => RecoveryEvent::Required(record),
            RecoveryTransition::Authorized(record) => RecoveryEvent::Authorized(record),
            RecoveryTransition::Issued { claim, issue } => {
                if issue.lsn != lsn {
                    return Err(RecoveryStateError::Position);
                }
                RecoveryEvent::Issued {
                    claim,
                    issued: &issue.issued,
                }
            }
            RecoveryTransition::Invalidated(record) => {
                return self.apply_invalidation(lsn, record, facts.through, history);
            }
            RecoveryTransition::Consumed {
                claim,
                issue,
                evidence,
            } => {
                return self.apply_consumed(
                    lsn,
                    claim,
                    issue,
                    evidence,
                    &RecoveryAttemptFacts {
                        through: facts.through,
                        issue: facts.source_issue.clone(),
                        attempt: facts.attempt,
                    },
                );
            }
        };
        self.apply(lsn, event, facts)
    }
}

impl RecoveryState {
    /// Validate an actual physical receipt independently of active-goal admission.
    /// Stale issued work remains held for launch; this accepts no dispatch command.
    pub fn validate_consumed(
        &self,
        claim: RecoveryClaimRef,
        issue: &RecoveryIssue,
        evidence: &RecoveryWitness,
        facts: &RecoveryAttemptFacts,
    ) -> Result<(), RecoveryStateError> {
        let (authorization, state) = self.claim(claim)?;
        if facts.through < self.last_applied
            || issue.lsn == Lsn::ZERO
            || issue.lsn > facts.through
            || [
                facts.attempt.spawn_lsn,
                facts.attempt.end_lsn,
                facts.attempt.confirmation_lsn,
            ]
            .into_iter()
            .flatten()
            .any(|at| at == Lsn::ZERO || at > facts.through)
            || facts.issue != *issue
            || !matches!(state, super::RecoveryClaimState::IssueBound(bound) if bound == issue)
        {
            return Err(RecoveryStateError::Lifecycle);
        }
        validate_consumption(authorization, issue, evidence, facts)
    }

    /// Settle only the matching recorded physical attempt at its actual source position.
    pub fn apply_consumed(
        &mut self,
        lsn: Lsn,
        claim: RecoveryClaimRef,
        issue: &RecoveryIssue,
        evidence: &RecoveryWitness,
        facts: &RecoveryAttemptFacts,
    ) -> Result<(), RecoveryStateError> {
        if lsn <= self.last_applied {
            return Ok(());
        }
        if evidence.reference().lsn != lsn {
            return Err(RecoveryStateError::Position);
        }
        self.validate_consumed(claim, issue, evidence, facts)?;
        self.claims.insert(
            claim.authorization_lsn,
            super::RecoveryClaimState::Consumed {
                issue: issue.clone(),
                evidence_lsn: lsn,
            },
        );
        self.last_applied = lsn;
        Ok(())
    }

    /// Validate closure against exact retained evidence even after the active target changed.
    /// A durable browser observation is data; its daemon-owned live producer is a separate gate.
    pub fn validate_invalidation(
        &self,
        record: &RecoveryInvalidated,
        through: Lsn,
        history: &RecoveryHistory,
    ) -> Result<(), RecoveryStateError> {
        self.validate_invalidation_claim(record.claim, record.evidence, through)?;
        let (authorization, _) = self.claim(record.claim)?;
        if let RecoveryInvalidationEvidence::RecordedChange { reason, lsn } = record.evidence {
            let source = history.at(lsn).map_err(|_| RecoveryStateError::Binding)?;
            validate_change(authorization, reason, source)?;
        }
        Ok(())
    }

    /// Permanently close one pending claim at the actual invalidation position.
    pub fn apply_invalidation(
        &mut self,
        lsn: Lsn,
        record: &RecoveryInvalidated,
        through: Lsn,
        history: &RecoveryHistory,
    ) -> Result<(), RecoveryStateError> {
        if lsn <= self.last_applied {
            return Ok(());
        }
        if lsn <= through {
            return Err(RecoveryStateError::Position);
        }
        self.validate_invalidation(record, through, history)?;
        self.claims.insert(
            record.claim.authorization_lsn,
            super::RecoveryClaimState::Invalidated {
                invalidation_lsn: lsn,
                evidence: record.evidence,
            },
        );
        self.last_applied = lsn;
        Ok(())
    }
}

fn validate_change(
    authorization: &RecoveryAuthorized,
    reason: RecoveryInvalidationReason,
    source: &RecoveryWitness,
) -> Result<(), RecoveryStateError> {
    use RecordKind as K;
    use RecoveryInvalidationReason as R;
    use RecoveryWitnessOwner as O;
    let binding = &authorization.binding;
    let work = binding.work;
    let reference = source.reference();
    let valid = match reason {
        R::GoalChanged => match (reference.kind, reference.owner, source.evidence()) {
            (K::GoalSet, O::Goal { agent, goal }, _) => agent == work.agent && goal != work.goal,
            (K::GoalCleared, O::Goal { agent, goal }, _) => {
                agent == work.agent && goal == work.goal
            }
            (K::GoalReassigned, O::Goal { agent, goal }, RecoveryEvidence::Reassigned { to }) => {
                agent == work.agent && goal == work.goal && *to != agent
            }
            (K::TicketInstructionReviewed, O::Card(_), _) => source
                .context()
                .cleared_goal
                .is_some_and(|target| target.agent == work.agent && target.goal == work.goal),
            _ => false,
        },
        R::GoalCompleted => source
            .context()
            .qualified_completion
            .is_some_and(|target| target.agent == work.agent && target.goal == work.goal),
        R::TicketChanged => binding.ticket.is_some_and(|ticket| {
            match (reference.kind, reference.owner, source.evidence()) {
                (K::TicketRemoved | K::TicketAssigned, O::Card(card), _) => card == ticket.card,
                (K::TicketStarted, O::Ticket { agent, goal, card }, _) => {
                    card == ticket.card && (agent != work.agent || goal != work.goal)
                }
                _ => false,
            }
        }),
        R::LeaseChanged => {
            reference.kind == K::WorktreeRemoved
                && matches!(reference.owner, O::Worktree(worktree) if binding.worktree == Some(worktree))
        }
        R::ControlChanged => {
            reference.owner == O::Agent(work.agent)
                && matches!(
                    reference.kind,
                    K::Pause | K::Resume | K::Halt | K::AgentHeld
                )
        }
        R::AgentUnavailable => {
            reference.owner == O::Agent(work.agent)
                && matches!(reference.kind, K::AgentRetired | K::Attached)
        }
    };
    if valid {
        Ok(())
    } else {
        Err(RecoveryStateError::Binding)
    }
}

fn prior<'a>(
    history: &'a RecoveryHistory,
    facts: &RecoveryFacts,
    lsn: Lsn,
) -> Result<&'a RecoveryWitness, RecoveryStateError> {
    if lsn == Lsn::ZERO || lsn > facts.through {
        return Err(RecoveryStateError::Position);
    }
    history.at(lsn).map_err(|_| RecoveryStateError::Binding)
}

fn turn_source(source: &RecoveryWitness, work: RecoveryWork, issue: Lsn) -> bool {
    source.reference().owner
        == RecoveryWitnessOwner::Turn {
            agent: work.agent,
            turn: work.turn,
        }
        && source
            .context()
            .attempt
            .is_some_and(|context| context.issue_lsn == issue)
}

fn observed_end(source: &RecoveryWitness) -> Result<RecoveryEndClass, RecoveryStateError> {
    match source.evidence() {
        RecoveryEvidence::End {
            mode: Mode::Managed,
            provenance: Provenance::Observed,
            outcome,
            ..
        } if source.reference().kind == RecordKind::TurnEnded => Ok(*outcome),
        _ => Err(RecoveryStateError::Binding),
    }
}

fn latest_end<'a>(
    facts: &RecoveryFacts,
    history: &'a RecoveryHistory,
    end_lsn: Lsn,
) -> Result<&'a RecoveryWitness, RecoveryStateError> {
    let end = prior(history, facts, end_lsn)?;
    if facts.attempt.end_lsn != Some(end_lsn)
        || facts
            .attempt
            .spawn_lsn
            .is_some_and(|spawn| spawn >= end_lsn)
        || !turn_source(end, facts.binding.work, facts.source_issue.lsn)
        || end
            .context()
            .attempt
            .is_none_or(|context| context.previous_spawn_lsn != facts.attempt.spawn_lsn)
    {
        return Err(RecoveryStateError::Binding);
    }
    observed_end(end)?;
    Ok(end)
}

fn latest_spawn<'a>(
    facts: &RecoveryFacts,
    history: &'a RecoveryHistory,
    spawn_lsn: Lsn,
) -> Result<&'a RecoveryWitness, RecoveryStateError> {
    let spawn = prior(history, facts, spawn_lsn)?;
    if facts.attempt.spawn_lsn != Some(spawn_lsn)
        || spawn_lsn <= facts.source_issue.lsn
        || spawn.reference().kind != RecordKind::TurnSpawned
        || !turn_source(spawn, facts.binding.work, facts.source_issue.lsn)
        || !matches!(spawn.evidence(), RecoveryEvidence::Spawn { .. })
    {
        return Err(RecoveryStateError::Binding);
    }
    Ok(spawn)
}

fn validate_required(
    record: &RecoveryRequired,
    facts: &RecoveryFacts,
    history: &RecoveryHistory,
) -> Result<(), RecoveryStateError> {
    match &record.boundary {
        RecoveryBoundary::Held { end_lsn, first } => {
            validate_hold(record, facts, history, *end_lsn, *first)
        }
        RecoveryBoundary::Paused { end_lsn } => {
            let RecoveryOrigin::Existing { lsn } = record.origin else {
                return Err(RecoveryStateError::Control);
            };
            let pause = prior(history, facts, lsn)?;
            if record.reason != RecoveryReason::HeldOrPausedFailure
                || facts.control_lsn != Some(lsn)
                || pause.reference().kind != RecordKind::Pause
                || pause.reference().owner != RecoveryWitnessOwner::Agent(record.binding.work.agent)
                || !matches!(pause.evidence(), RecoveryEvidence::Control { .. })
                || !matches!(
                    observed_end(latest_end(facts, history, *end_lsn)?)?,
                    RecoveryEndClass::Failed | RecoveryEndClass::NoResult
                )
            {
                return Err(RecoveryStateError::Binding);
            }
            Ok(())
        }
        RecoveryBoundary::Vanished { spawn_lsn, end_lsn } => {
            if record.reason != RecoveryReason::Vanished {
                return Err(RecoveryStateError::Binding);
            }
            latest_spawn(facts, history, *spawn_lsn)?;
            if observed_end(latest_end(facts, history, *end_lsn)?)? != RecoveryEndClass::Vanished {
                return Err(RecoveryStateError::Binding);
            }
            Ok(())
        }
        RecoveryBoundary::Unconfirmed {
            spawn_lsn,
            end_lsn,
            requested_confirmation_lsn,
        } => {
            if record.reason != RecoveryReason::UnconfirmedContext
                || facts.attempt.confirmation_lsn.is_some()
            {
                return Err(RecoveryStateError::Context);
            }
            let spawn = latest_spawn(facts, history, *spawn_lsn)?;
            let confirmation = prior(history, facts, *requested_confirmation_lsn)?;
            if *requested_confirmation_lsn >= *spawn_lsn
                || spawn.context().attempt.is_none_or(|context| {
                    context.confirmed_session_lsn != Some(*requested_confirmation_lsn)
                })
                || !matches!(confirmation.reference().owner, RecoveryWitnessOwner::Turn { agent, .. } if agent == record.binding.work.agent)
                || !matches!((spawn.evidence(), confirmation.evidence()), (RecoveryEvidence::Spawn { session: Some(requested), reissued: false }, RecoveryEvidence::Confirmation { session: confirmed }) if requested == confirmed)
                || !matches!(
                    observed_end(latest_end(facts, history, *end_lsn)?)?,
                    RecoveryEndClass::Failed
                        | RecoveryEndClass::NoResult
                        | RecoveryEndClass::Completed
                        | RecoveryEndClass::MaxTurns
                        | RecoveryEndClass::BudgetExhausted
                )
            {
                return Err(RecoveryStateError::Context);
            }
            Ok(())
        }
        RecoveryBoundary::MissingSpawn => {
            if record.reason != RecoveryReason::MissingSpawn
                || facts.attempt.spawn_lsn.is_some()
                || facts.attempt.end_lsn.is_some()
                || facts.attempt.confirmation_lsn.is_some()
            {
                return Err(RecoveryStateError::Binding);
            }
            Ok(())
        }
    }
}

fn validate_hold(
    record: &RecoveryRequired,
    facts: &RecoveryFacts,
    history: &RecoveryHistory,
    end_lsn: Lsn,
    first: Option<crate::recovery::RecoveryFailure>,
) -> Result<(), RecoveryStateError> {
    let RecoveryOrigin::Existing { lsn } = record.origin else {
        return Err(RecoveryStateError::Control);
    };
    let hold = prior(history, facts, lsn)?;
    let end = latest_end(facts, history, end_lsn)?;
    let outcome = observed_end(end)?;
    if record.reason != RecoveryReason::HeldOrPausedFailure
        || facts.control_lsn != Some(lsn)
        || end_lsn >= lsn
        || hold.reference().kind != RecordKind::AgentHeld
        || hold.reference().owner != RecoveryWitnessOwner::Agent(record.binding.work.agent)
    {
        return Err(RecoveryStateError::Binding);
    }
    match hold.evidence() {
        RecoveryEvidence::Hold {
            provenance: Provenance::Observed | Provenance::Inferred,
            cause:
                RecoveryHoldWitness::RepeatedFastFailure {
                    first: first_turn,
                    latest,
                },
        } => {
            let first = first.ok_or(RecoveryStateError::Binding)?;
            let first_source = prior(history, facts, first.issue_lsn)?;
            if !matches!(
                first_source.evidence(),
                RecoveryEvidence::Issue {
                    mode: Mode::Managed,
                    carrier: super::history::RecoveryIssueCarrier::Ordinary
                        | super::history::RecoveryIssueCarrier::BrowserNote { .. },
                    ..
                }
            ) {
                return Err(RecoveryStateError::Binding);
            }
            let first_issue = first_source.issue().ok_or(RecoveryStateError::Binding)?;
            let first_end = prior(history, facts, first.end_lsn)?;
            observed_end(first_end)?;
            if !matches!(
                first_end.evidence(),
                RecoveryEvidence::End {
                    fast_failure: true,
                    ..
                }
            ) || !matches!(
                end.evidence(),
                RecoveryEvidence::End {
                    fast_failure: true,
                    ..
                }
            ) || *latest != record.binding.work.turn
                || first_issue.issued.work.turn != *first_turn
                || first_issue.issued.work.agent != record.binding.work.agent
                || first_issue.issued.work.goal != record.binding.work.goal
                || first.issue_lsn >= first.end_lsn
                || first.end_lsn >= end_lsn
                || !turn_source(first_end, first_issue.issued.work, first.issue_lsn)
            {
                return Err(RecoveryStateError::Binding);
            }
        }
        RecoveryEvidence::Hold {
            provenance: Provenance::Observed,
            cause,
        } if first.is_none() => {
            if !matches!(
                (cause, outcome),
                (
                    RecoveryHoldWitness::LaunchFailed,
                    RecoveryEndClass::NotLaunched
                ) | (
                    RecoveryHoldWitness::LaunchRefused,
                    RecoveryEndClass::LaunchRefused
                )
            ) {
                return Err(RecoveryStateError::Binding);
            }
        }
        _ => return Err(RecoveryStateError::Binding),
    }
    Ok(())
}

fn validate_authorized(
    record: &RecoveryAuthorized,
    facts: &RecoveryFacts,
    history: &RecoveryHistory,
) -> Result<(), RecoveryStateError> {
    if let RecoveryAttribution::LocalOperator { pid: 0, .. } = record.attribution {
        return Err(RecoveryStateError::Binding);
    }
    if facts.control.halted || facts.control.paused || facts.control.hold.is_some() {
        return Err(RecoveryStateError::Control);
    }
    if let Some(at) = record.resume_lsn {
        let resume = prior(history, facts, at)?;
        let previous = resume
            .context()
            .previous_control
            .ok_or(RecoveryStateError::Control)?;
        if resume.reference().kind != RecordKind::Resume
            || resume.reference().owner != RecoveryWitnessOwner::Agent(record.binding.work.agent)
            || previous.agent != record.binding.work.agent
            || previous.lsn != record.expected_control
            || !releasable_control(previous.standing, &record.binding)
        {
            return Err(RecoveryStateError::Control);
        }
        match (resume.evidence(), record.attribution) {
            (
                RecoveryEvidence::Control { browser: None },
                RecoveryAttribution::LocalOperator { .. },
            ) => {}
            (
                RecoveryEvidence::Control {
                    browser: Some(browser),
                },
                RecoveryAttribution::BrowserSession { session_digest },
            ) if browser.session_digest == session_digest
                && browser.request_id == record.request_id
                && browser.request_digest == record.request_digest
                && browser.control_lsn == record.expected_control => {}
            _ => return Err(RecoveryStateError::Control),
        }
    }
    if let RecoveryPlan::New {
        context: RecoveryContext::Fresh { recap, .. },
        ..
    } = &record.plan
    {
        validate_recap(record, recap, facts, history)?;
    }
    Ok(())
}

fn releasable_control(control: RecoveryStandingControl, binding: &RecoveryBinding) -> bool {
    !control.halted
        && match control.hold {
            Some(RecoveryHoldWitness::Unknown) => false,
            Some(RecoveryHoldWitness::WorktreeMissing(worktree)) => {
                binding.worktree == Some(worktree)
            }
            _ => true,
        }
}

fn validate_recap(
    record: &RecoveryAuthorized,
    recap: &crate::recovery::RecoveryRecap,
    facts: &RecoveryFacts,
    history: &RecoveryHistory,
) -> Result<(), RecoveryStateError> {
    let work = record.binding.work;
    let mut delivered = std::collections::BTreeSet::new();
    for delivery in &recap.delivered_answers {
        if !delivered.insert(delivery.answer_lsn) || !recap.sources.contains(&delivery.answer_lsn) {
            return Err(RecoveryStateError::Context);
        }
        let answer = prior(history, facts, delivery.answer_lsn)?;
        let issue = prior(history, facts, delivery.issue_lsn)?
            .issue()
            .ok_or(RecoveryStateError::Context)?;
        let spawn = prior(history, facts, delivery.spawn_lsn)?;
        if answer.reference().kind != RecordKind::HumanAnswer
            || answer.reference().owner != RecoveryWitnessOwner::Agent(work.agent)
            || issue.issued.work.agent != work.agent
            || issue.issued.work.goal != work.goal
            || !issue.issued.answers.contains(&delivery.answer_lsn)
            || delivery.answer_lsn >= delivery.issue_lsn
            || delivery.issue_lsn >= delivery.spawn_lsn
            || spawn.reference().kind != RecordKind::TurnSpawned
            || !turn_source(spawn, issue.issued.work, issue.lsn)
            || !matches!(spawn.evidence(), RecoveryEvidence::Spawn { .. })
        {
            return Err(RecoveryStateError::Context);
        }
    }
    for at in &recap.sources {
        let source = prior(history, facts, *at)?;
        let eligible = match (source.reference().kind, source.reference().owner) {
            (RecordKind::AgentHired, RecoveryWitnessOwner::Agent(agent)) => {
                agent == work.agent && *at == record.binding.hired_lsn
            }
            (RecordKind::AgentLookChanged, RecoveryWitnessOwner::Agent(agent)) => {
                agent == work.agent
            }
            (
                RecordKind::GoalSet | RecordKind::BrowserNoteQueued,
                RecoveryWitnessOwner::Goal { agent, goal },
            ) => agent == work.agent && goal == work.goal,
            (RecordKind::TicketStarted, RecoveryWitnessOwner::Ticket { agent, goal, card }) => {
                agent == work.agent
                    && goal == work.goal
                    && record
                        .binding
                        .ticket
                        .is_some_and(|ticket| ticket.card == card)
            }
            (RecordKind::TicketCreated, RecoveryWitnessOwner::Card(card)) => record
                .binding
                .ticket
                .is_some_and(|ticket| ticket.card == card),
            (RecordKind::PredicateResult, RecoveryWitnessOwner::Predicate { goal, .. }) => {
                goal == work.goal
            }
            (RecordKind::HumanAnswer, RecoveryWitnessOwner::Agent(agent)) => {
                agent == work.agent && delivered.contains(at)
            }
            _ => false,
        };
        if !eligible {
            return Err(RecoveryStateError::Context);
        }
    }
    Ok(())
}

fn validate_consumption(
    authorization: &RecoveryAuthorized,
    issue: &RecoveryIssue,
    source: &RecoveryWitness,
    facts: &RecoveryAttemptFacts,
) -> Result<(), RecoveryStateError> {
    let at = source.reference().lsn;
    let context = source
        .context()
        .attempt
        .ok_or(RecoveryStateError::Binding)?;
    if at <= facts.through
        || at <= authorization.episode.origin
        || issue.lsn >= at
        || !turn_source(source, issue.issued.work, issue.lsn)
        || context.previous_spawn_lsn != facts.attempt.spawn_lsn
        || context.previous_end_lsn != facts.attempt.end_lsn
    {
        return Err(RecoveryStateError::Binding);
    }
    let selected = super::planned_session(&authorization.plan);
    match source.evidence() {
        RecoveryEvidence::Spawn { session, reissued }
            if source.reference().kind == RecordKind::TurnSpawned
                && session.as_ref() == selected
                && *reissued == matches!(authorization.plan, RecoveryPlan::RepeatIssued { .. }) =>
        {
            Ok(())
        }
        RecoveryEvidence::End {
            mode: Mode::Managed,
            provenance: Provenance::Observed,
            outcome:
                RecoveryEndClass::Completed
                | RecoveryEndClass::MaxTurns
                | RecoveryEndClass::BudgetExhausted
                | RecoveryEndClass::Failed
                | RecoveryEndClass::NoResult
                | RecoveryEndClass::Other
                | RecoveryEndClass::NotLaunched
                | RecoveryEndClass::LaunchRefused,
            ..
        } if source.reference().kind == RecordKind::TurnEnded
            && context.previous_spawn_lsn.is_none()
            && context.previous_end_lsn.is_none() =>
        {
            Ok(())
        }
        _ => Err(RecoveryStateError::Binding),
    }
}
