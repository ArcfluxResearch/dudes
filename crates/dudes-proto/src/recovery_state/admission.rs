//! Bounded factual joins against one before-record prefix, without operator permission.

use dudes_types::{AgentId, GoalId, Lsn, Mode, Provider, SessionId, TurnId, WorktreeId};

use super::history::{
    RecoveryEvidence, RecoveryHistory, RecoveryIssueCarrier, RecoveryStandingControl,
    RecoveryWitnessOwner,
};
use super::{RecoveryAttempt, RecoveryFacts, RecoveryStateError};
use crate::diff_stamp::TicketBinding;
use crate::recovery::{RecoveryBinding, RecoveryIssue};
use crate::{Predicate, RecordKind, WorktreeRemoval};

#[cfg(test)]
mod tests;

/// Current agent values supplied by its existing fold owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryAgentCurrent {
    /// Row identity.
    pub agent: AgentId,
    /// Actual immutable hire position.
    pub hired_lsn: Lsn,
    /// Current recorded provider.
    pub provider: Provider,
    /// Original lease identity.
    pub worktree: Option<WorktreeId>,
    /// Current operating mode.
    pub mode: Mode,
    /// Retirement is standing.
    pub retired: bool,
    /// Existing standing causes, separate from their latest control identity.
    pub control: RecoveryStandingControl,
    /// Current goal pointer.
    pub goal: Option<GoalId>,
    /// Current control source, if any.
    pub control_lsn: Option<Lsn>,
    /// Current vendor-confirmed session, if any.
    pub session: Option<SessionId>,
    /// Actual confirmation source for `session`, including an earlier completed turn.
    pub session_lsn: Option<Lsn>,
}

/// Current goal values, without reimplementing its lifecycle or predicate policy.
#[derive(Clone, Copy, Debug)]
pub struct RecoveryGoalCurrent<'a> {
    /// Row identity.
    pub goal: GoalId,
    /// Current owner.
    pub agent: AgentId,
    /// Exact installing record.
    pub installed_lsn: Lsn,
    /// Latest managed turn issued for this goal.
    pub last_turn: Option<TurnId>,
    /// Existing goal-clear state.
    pub cleared: bool,
    /// Existing recorded Done state.
    pub done: bool,
    /// Existing predicate definition.
    pub predicate: &'a Predicate,
    /// Actual latest predicate result position.
    pub result_lsn: Option<Lsn>,
    /// Latest matching card mutation, required for a board predicate.
    pub card_change_lsn: Option<Lsn>,
    /// Latest retirement that reset unfinished board predicate input.
    pub retired_lsn: Option<Lsn>,
}

/// Current lease values read by identity from its existing fold owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryLeaseCurrent {
    /// Lease identity.
    pub worktree: WorktreeId,
    /// First recorded owner.
    pub agent: Option<AgentId>,
    /// Recorded removal, if any.
    pub removed: Option<WorktreeRemoval>,
}

/// A bounded snapshot copied from existing folds at one committed cut.
///
/// These values are factual inputs, not a store certificate or operator permission.
#[derive(Clone, Debug)]
pub struct RecoveryCurrent<'a> {
    /// Exact before-record cut.
    pub through: Lsn,
    /// Current agent row values.
    pub agent: RecoveryAgentCurrent,
    /// Current goal row values.
    pub goal: RecoveryGoalCurrent<'a>,
    /// Current turn's immutable payload witness.
    pub issue: RecoveryIssue,
    /// Recorded source mode.
    pub mode: Mode,
    /// Actual latest attempt positions from the existing turn fold.
    pub attempt: RecoveryAttempt,
    /// Current active card association and original round.
    pub ticket: Option<TicketBinding>,
    /// Current original lease row, if any.
    pub lease: Option<RecoveryLeaseCurrent>,
}

impl RecoveryCurrent<'_> {
    /// Exact source positions needed by this snapshot, with no unrelated history scan.
    #[must_use]
    pub fn sources(&self) -> Vec<Lsn> {
        let mut sources = vec![
            self.agent.hired_lsn,
            self.goal.installed_lsn,
            self.issue.lsn,
        ];
        sources.extend(self.attempt.spawn_lsn);
        sources.extend(self.attempt.end_lsn);
        sources.extend(self.attempt.confirmation_lsn);
        sources.extend(self.agent.control_lsn);
        sources.extend(self.agent.session_lsn);
        sources.extend(self.goal.result_lsn);
        sources.extend(self.goal.card_change_lsn);
        sources.extend(self.goal.retired_lsn);
        sources.extend(&self.issue.issued.answers);
        sources.sort_unstable();
        sources.dedup();
        sources
    }

    /// Bind one immutable target to actual source evidence and current fold values.
    ///
    /// The journal adapter must separately own a certified generation; this pure join cannot
    /// create certification. Neither this result nor a confirmed session authorizes dispatch.
    pub fn bind(
        &self,
        requested: &RecoveryBinding,
        history: &RecoveryHistory,
    ) -> Result<RecoveryFacts, RecoveryStateError> {
        let current = &self.agent;
        let goal = &self.goal;
        if self.through == Lsn::ZERO
            || self
                .sources()
                .iter()
                .any(|at| *at == Lsn::ZERO || *at > self.through)
            || current.hired_lsn >= self.issue.lsn
            || goal.installed_lsn >= self.issue.lsn
        {
            return Err(RecoveryStateError::Position);
        }
        if current.agent != requested.work.agent
            || current.hired_lsn != requested.hired_lsn
            || current.provider != requested.provider
            || current.worktree != requested.worktree
            || current.mode != Mode::Managed
            || current.retired
            || current.control.halted
            || current.goal != Some(requested.work.goal)
            || goal.goal != requested.work.goal
            || goal.agent != current.agent
            || goal.last_turn != Some(requested.work.turn)
            || goal.cleared
            || goal.done
        {
            return Err(RecoveryStateError::Binding);
        }
        Self::validate_hire(requested, history)?;
        self.validate_goal(requested, history)?;
        self.validate_issue(requested, history)?;
        self.validate_lease(requested)?;
        self.validate_control(history)?;
        self.validate_session(history)?;
        if self.qualified_pass(history)? {
            return Err(RecoveryStateError::Binding);
        }
        Ok(RecoveryFacts {
            through: self.through,
            binding: requested.clone(),
            source_issue: self.issue.clone(),
            attempt: self.attempt,
            control_lsn: current.control_lsn,
            control: current.control,
            confirmed_session: current.session.clone(),
        })
    }

    fn validate_hire(
        requested: &RecoveryBinding,
        history: &RecoveryHistory,
    ) -> Result<(), RecoveryStateError> {
        let source = history
            .at(requested.hired_lsn)
            .map_err(|_| RecoveryStateError::Binding)?;
        if source.reference().kind != RecordKind::AgentHired
            || source.reference().owner != RecoveryWitnessOwner::Agent(requested.work.agent)
            || !matches!(source.evidence(), RecoveryEvidence::Hire { provider, worktree, .. }
                if *provider == requested.provider && *worktree == requested.worktree)
        {
            return Err(RecoveryStateError::Binding);
        }
        Ok(())
    }

    fn validate_goal(
        &self,
        requested: &RecoveryBinding,
        history: &RecoveryHistory,
    ) -> Result<(), RecoveryStateError> {
        let source = history
            .at(self.goal.installed_lsn)
            .map_err(|_| RecoveryStateError::Binding)?;
        let matches_install = match (
            source.reference().kind,
            source.reference().owner,
            self.ticket,
        ) {
            (RecordKind::GoalSet, RecoveryWitnessOwner::Goal { goal, .. }, None) => {
                goal == requested.work.goal
            }
            (
                RecordKind::TicketStarted,
                RecoveryWitnessOwner::Ticket { agent, goal, card },
                Some(ticket),
            ) => {
                agent == requested.work.agent
                    && goal == requested.work.goal
                    && card == ticket.card
                    && ticket.goal == goal
                    && ticket.claimant == agent
                    && ticket.round > 0
            }
            _ => false,
        };
        if !matches_install || self.ticket != requested.ticket {
            return Err(RecoveryStateError::Binding);
        }
        Ok(())
    }

    fn validate_issue(
        &self,
        requested: &RecoveryBinding,
        history: &RecoveryHistory,
    ) -> Result<(), RecoveryStateError> {
        let source = history
            .at(requested.issue_lsn)
            .map_err(|_| RecoveryStateError::Binding)?;
        if self.mode != Mode::Managed
            || self.issue.lsn != requested.issue_lsn
            || self.issue.issued.work != requested.work
            || self.issue.issued.model_selection != requested.model_selection
            || source.reference().owner != RecoveryWitnessOwner::Work(requested.work)
            || source.issue().as_ref() != Some(&self.issue)
            || !matches!(
                source.evidence(),
                RecoveryEvidence::Issue {
                    mode: Mode::Managed,
                    carrier: RecoveryIssueCarrier::Ordinary
                        | RecoveryIssueCarrier::BrowserNote { .. },
                    ..
                }
            )
            || !super::ordered_positions(&self.issue.issued.answers)
        {
            return Err(RecoveryStateError::Binding);
        }
        for answer in &self.issue.issued.answers {
            let source = history
                .at(*answer)
                .map_err(|_| RecoveryStateError::Binding)?;
            if *answer >= self.issue.lsn
                || source.reference().kind != RecordKind::HumanAnswer
                || source.reference().owner != RecoveryWitnessOwner::Agent(requested.work.agent)
            {
                return Err(RecoveryStateError::Binding);
            }
        }
        Ok(())
    }

    fn validate_lease(&self, requested: &RecoveryBinding) -> Result<(), RecoveryStateError> {
        match (requested.worktree, self.lease) {
            (None, None) => Ok(()),
            (Some(expected), Some(actual))
                if expected == actual.worktree
                    && actual.agent == Some(requested.work.agent)
                    && actual.removed.is_none() =>
            {
                Ok(())
            }
            _ => Err(RecoveryStateError::Binding),
        }
    }

    fn validate_control(&self, history: &RecoveryHistory) -> Result<(), RecoveryStateError> {
        let Some(at) = self.agent.control_lsn else {
            return Ok(());
        };
        let source = history.at(at).map_err(|_| RecoveryStateError::Control)?;
        if source.reference().owner != RecoveryWitnessOwner::Agent(self.agent.agent)
            || !matches!(
                source.evidence(),
                RecoveryEvidence::Control { .. } | RecoveryEvidence::Hold { .. }
            )
        {
            return Err(RecoveryStateError::Control);
        }
        Ok(())
    }

    fn validate_session(&self, history: &RecoveryHistory) -> Result<(), RecoveryStateError> {
        match (&self.agent.session, self.agent.session_lsn) {
            (None, None) => Ok(()),
            (Some(session), Some(at)) => {
                let source = history.at(at).map_err(|_| RecoveryStateError::Context)?;
                if !matches!(source.reference().owner, RecoveryWitnessOwner::Turn { agent, .. }
                    if agent == self.agent.agent)
                    || !matches!(source.evidence(), RecoveryEvidence::Confirmation { session: seen } if seen == session)
                {
                    return Err(RecoveryStateError::Context);
                }
                Ok(())
            }
            _ => Err(RecoveryStateError::Context),
        }
    }

    fn qualified_pass(&self, history: &RecoveryHistory) -> Result<bool, RecoveryStateError> {
        let invalidated = if let Predicate::BoardState { card, .. } = self.goal.predicate {
            let at = self
                .goal
                .card_change_lsn
                .ok_or(RecoveryStateError::Binding)?;
            let source = history.at(at).map_err(|_| RecoveryStateError::Binding)?;
            let matches_card = matches!(source.reference().owner,
                RecoveryWitnessOwner::Card(seen) | RecoveryWitnessOwner::Ticket { card: seen, .. } if seen == *card);
            if !matches_card
                || !matches!(
                    source.reference().kind,
                    RecordKind::CardCreated
                        | RecordKind::CardTransition
                        | RecordKind::Claim
                        | RecordKind::TicketCreated
                        | RecordKind::TicketAssigned
                        | RecordKind::TicketStarted
                )
            {
                return Err(RecoveryStateError::Binding);
            }
            if let Some(retired) = self.goal.retired_lsn {
                let source = history
                    .at(retired)
                    .map_err(|_| RecoveryStateError::Binding)?;
                if source.reference().kind != RecordKind::AgentRetired
                    || !matches!(source.reference().owner, RecoveryWitnessOwner::Agent(_))
                {
                    return Err(RecoveryStateError::Binding);
                }
            }
            Some(self.goal.retired_lsn.map_or(at, |retired| retired.max(at)))
        } else {
            None
        };
        let Some(at) = self.goal.result_lsn else {
            return Ok(false);
        };
        let source = history.at(at).map_err(|_| RecoveryStateError::Binding)?;
        if !matches!(source.reference().owner, RecoveryWitnessOwner::Predicate { goal, .. } if goal == self.goal.goal)
        {
            return Err(RecoveryStateError::Binding);
        }
        let RecoveryEvidence::Predicate {
            provenance,
            reported_pass,
            card,
        } = source.evidence()
        else {
            return Err(RecoveryStateError::Binding);
        };
        Ok(invalidated.is_none_or(|boundary| at > boundary)
            && *reported_pass
            && provenance.is_observed()
            && crate::goal::predicate_card_evidence_eligible(
                self.goal.predicate,
                card.as_ref().map(|seen| (seen.card, seen.status, &seen.by)),
            ))
    }
}
