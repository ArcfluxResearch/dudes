use dudes_proto::recovery::{
    RecoveryAuthorized, RecoveryBinding, RecoveryBoundary, RecoveryClaimRef, RecoveryContext,
    RecoveryEpisodeId, RecoveryFailure, RecoveryOrigin, RecoveryPlan, RecoveryReason,
    RecoveryRecap, RecoveryRequired, RecoveryWork,
};
use dudes_proto::recovery_state::history::{
    RecoveryEndClass, RecoveryEvidence, RecoveryHoldWitness,
};
use dudes_proto::recovery_state::transition::RecoveryTransition;
use dudes_proto::recovery_state::{RecoveryClaimState, RecoveryStateError};
use dudes_proto::{AutomaticHold, Entry, Record};
use dudes_types::{AgentId, Lsn, Provenance, TurnId};

use super::{Clock, ManagedLaunch, PreparedLaunch, Rng, Supervisor, Then};
use crate::input::{
    Handled, PersistToken, RecoveryAppend, RecoveryChoice, RecoveryCommand, RecoveryExpected,
    RecoveryOpportunity, RecoveryPrefix, RecoveryRefusal, RecoveryStage, RecoveryUpdate,
};
use crate::{RecoveryAvailability, RecoverySubjectKey, RecoverySubjectUse};

mod append;
#[cfg(test)]
mod tests;

pub(super) struct NewContext {
    pub(super) turn: TurnId,
    pub(super) session: dudes_providers::adapter::TurnSession,
    pub(super) recap: String,
    pub(super) model: Option<dudes_types::ModelSelection>,
}

#[derive(Debug)]
pub(super) struct Continuing {
    command: RecoveryCommand,
    required: RecoveryRequired,
    episode: Option<RecoveryEpisodeId>,
    prefix: RecoveryPrefix,
}

#[derive(Debug)]
pub(super) enum RecoveryThen {
    Required(Box<Continuing>),
    Resumed(Box<Continuing>),
    Authorized(Box<Continuing>),
}

impl RecoveryThen {
    fn continuing(&self) -> (&Continuing, RecoveryStage) {
        match self {
            Self::Required(next) => (next, RecoveryStage::Required),
            Self::Resumed(next) => (next, RecoveryStage::Resume),
            Self::Authorized(next) => (next, RecoveryStage::Authorized),
        }
    }
}

impl Continuing {
    fn update(&self, refusal: Option<RecoveryRefusal>) -> RecoveryUpdate {
        RecoveryUpdate {
            request_id: self.command.request_id,
            prefix: self.prefix,
            refusal,
        }
    }
}

impl<C: Clock, R: Rng> Supervisor<C, R> {
    /// Inspect journal-derived eligibility without pumping, minting identities or appending work.
    /// Executor liveness and persistent certification are separate requirements.
    ///
    /// # Errors
    ///
    /// Refuses an unavailable, claimed or inconsistent current boundary.
    pub fn recovery_opportunity(
        &self,
        agent: AgentId,
    ) -> Result<RecoveryOpportunity, RecoveryRefusal> {
        let view = self.recovery_boundary(agent)?;
        if view.required.reason != RecoveryReason::MissingSpawn
            && view.required.binding.model_selection.is_none()
        {
            return Err(RecoveryRefusal::NotRecoverable);
        }
        Ok(view)
    }

    fn recovery_boundary(&self, agent: AgentId) -> Result<RecoveryOpportunity, RecoveryRefusal> {
        let binding = self.recovery_binding(agent)?;
        let facts = self.world.recovery_facts(&binding).map_err(refusal)?;
        if facts.control.halted
            || matches!(
                facts.control.hold,
                Some(RecoveryHoldWitness::Unknown | RecoveryHoldWitness::WorktreeMissing(_))
            )
        {
            return Err(RecoveryRefusal::NotRecoverable);
        }
        let (state, _) = self.world.recovery_projection().map_err(refusal)?;
        let current = state.current(binding.work);
        if self.live_issues.contains(&binding.work.turn)
            && self
                .world
                .turns
                .get(&binding.work.turn)
                .is_some_and(crate::world::TurnRow::is_issued_not_spawned)
        {
            return Err(RecoveryRefusal::NotRecoverable);
        }
        let predecessor = current
            .and_then(|episode| {
                episode
                    .authorization_lsn
                    .map(|authorization_lsn| RecoveryClaimRef {
                        episode: episode.id,
                        authorization_lsn,
                    })
            })
            .or_else(|| {
                current
                    .is_none()
                    .then(|| state.planned_claim(binding.work.turn))
                    .flatten()
            });
        if let Some(claim) = predecessor
            && matches!(
                state.claim_receipt(claim).map_err(refusal)?.state,
                RecoveryClaimState::Pending
            )
        {
            return Err(RecoveryRefusal::Claimed);
        }
        let candidates = self.recovery_boundaries(&binding);
        let turn = &self.world.turns[&binding.work.turn];
        if let Some(episode) = current.filter(|episode| episode.authorization_lsn.is_none()) {
            let required = existing_required(episode, turn, &candidates)?;
            let mut view = opportunity(
                required,
                Some(episode.id),
                None,
                facts.control_lsn,
                facts.confirmed_session.is_some(),
            );
            if facts.control.paused || facts.control.hold.is_some() {
                view.expected.source_lsn =
                    facts.control_lsn.ok_or(RecoveryRefusal::StaleControl)?;
            }
            return Ok(view);
        }
        for (reason, origin, boundary) in candidates {
            let required = RecoveryRequired {
                binding: binding.clone(),
                origin,
                reason,
                boundary,
                predecessor,
            };
            if self
                .world
                .prepare_recovery(RecoveryTransition::Required(&required))
                .is_ok()
            {
                return Ok(opportunity(
                    required,
                    current.map(|episode| episode.id),
                    predecessor,
                    facts.control_lsn,
                    facts.confirmed_session.is_some(),
                ));
            }
        }
        Err(RecoveryRefusal::NotRecoverable)
    }

    fn recovery_boundaries(&self, binding: &RecoveryBinding) -> Vec<Boundary> {
        let row = &self.world.agents[&binding.work.agent];
        let turn = &self.world.turns[&binding.work.turn];
        let end = turn
            .end_lsn
            .and_then(|lsn| self.world.recovery_witness(lsn).ok());
        let end_class = end.and_then(|source| match source.evidence() {
            RecoveryEvidence::End { outcome, .. } => Some(*outcome),
            _ => None,
        });
        let mut candidates = Vec::with_capacity(4);
        if let (Some(control), Some(end_lsn)) = (row.control_lsn, turn.end_lsn) {
            if let Some(cause) = &row.automatic_hold {
                let first = match cause {
                    AutomaticHold::RepeatedFastFailure { first, .. } => {
                        self.world.turns.get(first).and_then(|first| {
                            Some(RecoveryFailure {
                                issue_lsn: first.issue_lsn,
                                end_lsn: first.end_lsn?,
                            })
                        })
                    }
                    _ => None,
                };
                candidates.push((
                    RecoveryReason::HeldOrPausedFailure,
                    RecoveryOrigin::Existing { lsn: control },
                    RecoveryBoundary::Held { end_lsn, first },
                ));
            } else if row.paused {
                candidates.push((
                    RecoveryReason::HeldOrPausedFailure,
                    RecoveryOrigin::Existing { lsn: control },
                    RecoveryBoundary::Paused { end_lsn },
                ));
            }
        }
        if let (Some(spawn_lsn), Some(end_lsn)) = (turn.spawn_lsn, turn.end_lsn) {
            if end_class == Some(RecoveryEndClass::Vanished) {
                candidates.push((
                    RecoveryReason::Vanished,
                    RecoveryOrigin::ThisBarrier,
                    RecoveryBoundary::Vanished { spawn_lsn, end_lsn },
                ));
            }
            if let Some(requested_confirmation_lsn) = self
                .world
                .recovery_witness(spawn_lsn)
                .ok()
                .and_then(|source| source.context().attempt)
                .and_then(|attempt| attempt.confirmed_session_lsn)
            {
                candidates.push((
                    RecoveryReason::UnconfirmedContext,
                    RecoveryOrigin::ThisBarrier,
                    RecoveryBoundary::Unconfirmed {
                        spawn_lsn,
                        end_lsn,
                        requested_confirmation_lsn,
                    },
                ));
            }
        }
        if turn.spawn_lsn.is_none() && turn.end_lsn.is_none() {
            candidates.push((
                RecoveryReason::MissingSpawn,
                RecoveryOrigin::ThisBarrier,
                RecoveryBoundary::MissingSpawn,
            ));
        }
        candidates.sort_by_key(|(reason, _, _)| match reason {
            RecoveryReason::UnconfirmedContext => 0,
            RecoveryReason::Vanished => 1,
            RecoveryReason::MissingSpawn => 2,
            RecoveryReason::HeldOrPausedFailure => 3,
        });
        candidates
    }

    fn recovery_binding(&self, agent: AgentId) -> Result<RecoveryBinding, RecoveryRefusal> {
        let row = self
            .world
            .agents
            .get(&agent)
            .filter(|row| !row.retired && !row.attached)
            .ok_or(RecoveryRefusal::StaleWork)?;
        let goal = row
            .goal
            .and_then(|goal| self.world.goals.get(&goal))
            .filter(|goal| !goal.cleared && self.world.completed_goal(agent).is_none())
            .ok_or(RecoveryRefusal::StaleWork)?;
        let turn = goal
            .last_turn
            .and_then(|turn| self.world.turns.get(&turn).map(|row| (turn, row)))
            .ok_or(RecoveryRefusal::NotRecoverable)?;
        Ok(RecoveryBinding {
            work: RecoveryWork {
                agent,
                goal: goal.goal.id,
                turn: turn.0,
            },
            issue_lsn: turn.1.issue_lsn,
            hired_lsn: row.mcp.hire_lsn,
            ticket: self.world.ticket_goal(goal.goal.id).map(|(card, round)| {
                dudes_proto::diff_stamp::TicketBinding {
                    card,
                    round,
                    goal: goal.goal.id,
                    claimant: agent,
                }
            }),
            worktree: row.worktree,
            provider: row.provider,
            model_selection: turn.1.model_selection.clone(),
        })
    }

    /// Begin one admitted command. The executor must separately check every command-owned append.
    pub fn recover(
        &mut self,
        command: RecoveryCommand,
        _operation: &crate::RecoveryOperation<'_>,
    ) -> Handled {
        self.pending.clear();
        let request_id = command.request_id;
        let result = self.begin_recovery(command);
        let update = match result {
            Ok(next) => next,
            Err(reason) => RecoveryUpdate {
                request_id,
                prefix: RecoveryPrefix::default(),
                refusal: Some(reason),
            },
        };
        Handled::default().with_recovery(Some(update))
    }

    fn begin_recovery(
        &mut self,
        command: RecoveryCommand,
    ) -> Result<RecoveryUpdate, RecoveryRefusal> {
        if self.recovery_discovery.busy() {
            return Err(RecoveryRefusal::Unavailable);
        }
        let view = self.recovery_opportunity(command.expected.work.agent)?;
        compare_expected(&command.expected, &view.expected)?;
        let mut next = Continuing {
            command,
            required: view.required,
            episode: None,
            prefix: RecoveryPrefix::default(),
        };
        let current = self
            .world
            .recovery_projection()
            .map_err(refusal)?
            .0
            .current(next.required.binding.work);
        if let Some(episode) = current.filter(|episode| episode.authorization_lsn.is_none()) {
            next.episode = Some(episode.id);
            return Ok(self.recovery_control_ready(next));
        }
        let update = next.update(None);
        self.persist(
            Record::RecoveryRequired(next.required.clone()),
            Then::Recovery(RecoveryThen::Required(Box::new(next))),
        );
        Ok(update)
    }

    pub(super) fn recovery_after_ack(
        &mut self,
        then: RecoveryThen,
        lsn: Lsn,
        operation: &crate::RecoveryOperation<'_>,
    ) -> RecoveryUpdate {
        match then {
            RecoveryThen::Required(mut next) => {
                next.prefix.required = Some(lsn);
                next.episode = Some(RecoveryEpisodeId {
                    work: next.required.binding.work,
                    origin: match next.required.origin {
                        RecoveryOrigin::Existing { lsn } => lsn,
                        RecoveryOrigin::ThisBarrier => lsn,
                    },
                });
                self.recovery_control_ready(*next)
            }
            RecoveryThen::Resumed(mut next) => {
                next.prefix.resume = Some(lsn);
                self.recovery_authorize(*next)
            }
            RecoveryThen::Authorized(next) => {
                let work = next.required.binding.work;
                if let Some(turn) = self
                    .world
                    .turns
                    .get(&work.turn)
                    .filter(|turn| turn.is_issued_not_spawned())
                    .cloned()
                {
                    self.live_issues.insert(work.turn);
                    self.reissue_turn(
                        work.turn,
                        work.agent,
                        &turn.task,
                        turn.model_selection,
                        operation,
                    );
                } else {
                    self.decide(work.agent, operation);
                }
                next.update(None)
            }
        }
    }

    fn recovery_control_ready(&mut self, next: Continuing) -> RecoveryUpdate {
        let facts = match self.world.recovery_facts(&next.required.binding) {
            Ok(facts) => facts,
            Err(error) => return next.update(Some(refusal(error))),
        };
        if facts.control_lsn != next.command.expected.control_lsn || facts.control.halted {
            return next.update(Some(RecoveryRefusal::StaleControl));
        }
        if facts.control.paused || facts.control.hold.is_some() {
            if matches!(
                facts.control.hold,
                Some(RecoveryHoldWitness::Unknown | RecoveryHoldWitness::WorktreeMissing(_))
            ) {
                return next.update(Some(RecoveryRefusal::NotRecoverable));
            }
            let update = next.update(None);
            self.persist(
                Record::Resume {
                    agent: next.required.binding.work.agent,
                    browser: match next.command.attribution {
                        dudes_proto::recovery::RecoveryAttribution::LocalOperator { .. } => None,
                        dudes_proto::recovery::RecoveryAttribution::BrowserSession {
                            session_digest,
                        } => Some(dudes_proto::record::BrowserControl {
                            request_id: next.command.request_id,
                            request_digest: next.command.digest,
                            control_lsn: next.command.expected.control_lsn,
                            session_digest,
                        }),
                    },
                },
                Then::Recovery(RecoveryThen::Resumed(Box::new(next))),
            );
            update
        } else {
            self.recovery_authorize(next)
        }
    }

    fn recovery_authorize(&mut self, next: Continuing) -> RecoveryUpdate {
        let result = self.recovery_authorization(&next);
        match result {
            Ok(authorization) => {
                let update = next.update(None);
                self.persist(
                    Record::RecoveryAuthorized(Box::new(authorization)),
                    Then::Recovery(RecoveryThen::Authorized(Box::new(next))),
                );
                update
            }
            Err(reason) => next.update(Some(reason)),
        }
    }

    fn recovery_authorization(
        &mut self,
        next: &Continuing,
    ) -> Result<RecoveryAuthorized, RecoveryRefusal> {
        let facts = self
            .world
            .recovery_facts(&next.required.binding)
            .map_err(refusal)?;
        let episode = next.episode.ok_or(RecoveryRefusal::StaleEpisode)?;
        if self
            .world
            .recovery_projection()
            .map_err(refusal)?
            .0
            .current(episode.work)
            .is_none_or(|current| current.id != episode || current.authorization_lsn.is_some())
        {
            return Err(RecoveryRefusal::Claimed);
        }
        if facts.control_lsn != next.prefix.resume.or(next.command.expected.control_lsn)
            || facts.control.halted
            || facts.control.paused
            || facts.control.hold.is_some()
        {
            return Err(RecoveryRefusal::StaleControl);
        }
        let session = match next.command.choice {
            RecoveryChoice::TryRecorded
                if next.required.reason != RecoveryReason::UnconfirmedContext
                    && next.required.reason != RecoveryReason::MissingSpawn =>
            {
                Some(crate::input::RecoveryChoice::TryRecorded)
            }
            RecoveryChoice::FreshWithRecap
                if next.required.reason != RecoveryReason::MissingSpawn =>
            {
                None
            }
            RecoveryChoice::RepeatIssuedFresh
                if next.required.reason == RecoveryReason::MissingSpawn =>
            {
                None
            }
            _ => return Err(RecoveryRefusal::ChoiceUnavailable),
        };
        if session.is_some() && facts.confirmed_session.is_none() {
            return Err(RecoveryRefusal::ChoiceUnavailable);
        }
        if next.required.reason != RecoveryReason::MissingSpawn
            && next.required.binding.model_selection.is_none()
        {
            return Err(RecoveryRefusal::ChoiceUnavailable);
        }
        let plan = if next.required.reason == RecoveryReason::MissingSpawn {
            let session = self
                .announce_session(episode.work.agent, true)
                .id()
                .cloned();
            RecoveryPlan::RepeatIssued {
                issue: facts.source_issue,
                session,
            }
        } else {
            let turn = TurnId::from_ulid(self.rng.ulid());
            let context = if session.is_some() {
                RecoveryContext::TryRecorded {
                    session: facts
                        .confirmed_session
                        .ok_or(RecoveryRefusal::ChoiceUnavailable)?,
                }
            } else {
                RecoveryContext::Fresh {
                    session: self
                        .announce_session(episode.work.agent, true)
                        .id()
                        .cloned(),
                    recap: self.recovery_recap(&next.required.binding),
                }
            };
            RecoveryPlan::New { turn, context }
        };
        let authorization = RecoveryAuthorized {
            episode,
            binding: next.required.binding.clone(),
            request_id: next.command.request_id,
            request_digest: next.command.digest,
            attribution: next.command.attribution,
            expected_control: next.command.expected.control_lsn,
            resulting_control: facts.control_lsn,
            resume_lsn: next.prefix.resume,
            plan,
        };
        self.world
            .prepare_recovery(RecoveryTransition::Authorized(&authorization))
            .map_err(refusal)?;
        Ok(authorization)
    }

    pub(super) fn recovery_admission_allowed(
        &self,
        agent: AgentId,
        operation: &crate::RecoveryOperation<'_>,
    ) -> bool {
        if self.recovery_discovery.busy() {
            return false;
        }
        let Ok(binding) = self.recovery_binding(agent) else {
            return true;
        };
        if !self.recovery_dispatch_attribution_allowed(binding.work, operation) {
            return false;
        }
        if self.recovery_owed_saving(agent, binding.work.turn) {
            return true;
        }
        let Ok((state, _)) = self.world.recovery_projection() else {
            return false;
        };
        if let Some(episode) = state.current(binding.work) {
            let Some(authorization_lsn) = episode.authorization_lsn else {
                return false;
            };
            let claim = RecoveryClaimRef {
                episode: episode.id,
                authorization_lsn,
            };
            let Ok(receipt) = state.claim_receipt(claim) else {
                return false;
            };
            if matches!(receipt.state, RecoveryClaimState::Consumed { .. }) {
                return self.recovery_boundary(agent).is_err();
            }
            let Ok(facts) = self.world.recovery_facts(&receipt.authorization.binding) else {
                return false;
            };
            if let RecoveryPlan::New {
                context: RecoveryContext::TryRecorded { session },
                ..
            } = &receipt.authorization.plan
                && facts.confirmed_session.as_ref() != Some(session)
            {
                return false;
            }
            if self
                .world
                .agents
                .get(&agent)
                .is_none_or(|row| row.control_lsn != receipt.authorization.resulting_control)
            {
                return false;
            }
            return match receipt.state {
                RecoveryClaimState::Pending => {
                    !self.closing_active && receipt.authorization.binding.model_selection.is_some()
                }
                RecoveryClaimState::IssueBound(issue) => {
                    self.live_issues.contains(&issue.issued.work.turn)
                }
                RecoveryClaimState::Consumed { .. } => self.recovery_boundary(agent).is_err(),
                RecoveryClaimState::Invalidated { .. } | RecoveryClaimState::Superseded { .. } => {
                    false
                }
            };
        }
        if self.live_issues.contains(&binding.work.turn)
            && self
                .world
                .turns
                .get(&binding.work.turn)
                .is_some_and(crate::world::TurnRow::is_issued_not_spawned)
        {
            return true;
        }
        match self.recovery_boundary(agent) {
            Ok(view) => view.required.reason == RecoveryReason::HeldOrPausedFailure,
            Err(_) => true,
        }
    }

    pub(super) fn ensure_recovery_admission(
        &mut self,
        agent: AgentId,
        operation: &crate::RecoveryOperation<'_>,
    ) -> bool {
        if self.recovery_admission_allowed(agent, operation) {
            return true;
        }
        if let Ok(binding) = self.recovery_binding(agent)
            && let Ok((state, _)) = self.world.recovery_projection()
            && let Ok(Some(claim)) = state.work_claim(binding.work)
            && let Ok(receipt) = state.claim_receipt(claim)
            && matches!(
                receipt.authorization.attribution,
                dudes_proto::recovery::RecoveryAttribution::BrowserSession { .. }
            )
            && match receipt.state {
                RecoveryClaimState::Pending => true,
                RecoveryClaimState::IssueBound(issue) => {
                    self.live_issues.contains(&issue.issued.work.turn)
                }
                _ => false,
            }
        {
            return false;
        }
        if self.closing_active {
            return false;
        }
        if let Ok(view) = self.recovery_boundary(agent) {
            let existing = self
                .world
                .recovery_projection()
                .ok()
                .and_then(|(state, _)| state.current(view.required.binding.work));
            if existing.is_none_or(|episode| episode.authorization_lsn.is_some())
                && !self.ledger.values().any(|parked| matches!(&parked.entry.record, Record::RecoveryRequired(required) if required.binding.work == view.required.binding.work)) {
                self.persist(Record::RecoveryRequired(view.required), Then::Fold);
            }
        }
        false
    }

    /// Common historical barrier at the final managed dispatch seam.
    /// Live native absence and checkout admission remain executor-owned checks.
    #[must_use]
    pub fn recovery_launch_allowed(
        &self,
        agent: AgentId,
        turn: TurnId,
        operation: &crate::RecoveryOperation<'_>,
    ) -> bool {
        let Some(row) = self.world.turns.get(&turn).filter(|row| row.agent == agent) else {
            return false;
        };
        if !row.is_issued_not_spawned() {
            return false;
        }
        let finite = self.world.recovery_witness(row.issue_lsn).is_ok_and(|source| {
            matches!(source.evidence(), RecoveryEvidence::Issue {
                carrier: dudes_proto::recovery_state::history::RecoveryIssueCarrier::CompletedNote { .. }
                    | dudes_proto::recovery_state::history::RecoveryIssueCarrier::MailWake { .. },
                ..
            })
        });
        if self.world.agents.get(&agent).is_none_or(|owner| {
            owner.retired
                || owner.halted
                || (owner.paused && !finite)
                || owner.attached
                || owner.open_question.is_some()
        }) || self.launch_held(agent, operation)
        {
            return false;
        }
        if let Some(input) = self.loop_input_for(agent)
            && matches!(
                dudes_policy::admit(
                    &input.budget_state(),
                    &input.ceilings(),
                    input.per_turn_floor
                ),
                dudes_policy::Admission::Refused { .. }
            )
        {
            return false;
        }
        if finite {
            return true;
        }
        let Some(goal) = row.goal else {
            return self.recovery_admission_allowed(agent, operation);
        };
        if self
            .world
            .agents
            .get(&agent)
            .is_none_or(|owner| owner.goal != Some(goal))
        {
            return false;
        }
        if !self
            .recovery_dispatch_attribution_allowed(RecoveryWork { agent, goal, turn }, operation)
        {
            return false;
        }
        if self.recovery_owed_saving(agent, turn) {
            return true;
        }
        if !self.live_issues.contains(&turn) {
            return false;
        }
        let Ok((state, _)) = self.world.recovery_projection() else {
            return false;
        };
        let work = RecoveryWork { agent, goal, turn };
        let Ok(claim) = state.work_claim(work) else {
            return false;
        };
        let Some(claim) = claim else {
            return true;
        };
        let Ok(receipt) = state.claim_receipt(claim) else {
            return false;
        };
        matches!(receipt.state, RecoveryClaimState::IssueBound(issue) if issue.issued.work == work && issue.lsn == row.issue_lsn)
            && {
                let mut binding = receipt.authorization.binding.clone();
                binding.work = work;
                binding.issue_lsn = row.issue_lsn;
                self.world.recovery_facts(&binding).is_ok()
            }
            && self
                .world
                .agents
                .get(&agent)
                .is_some_and(|owner| owner.control_lsn == receipt.authorization.resulting_control)
    }

    fn recovery_dispatch_attribution_allowed(
        &self,
        work: RecoveryWork,
        operation: &crate::RecoveryOperation<'_>,
    ) -> bool {
        let Ok((state, _)) = self.world.recovery_projection() else {
            return false;
        };
        let claim = match state.work_claim(work) {
            Ok(Some(claim)) => claim,
            Ok(None) => return true,
            Err(_) => return false,
        };
        let Ok(receipt) = state.claim_receipt(claim) else {
            return false;
        };
        match receipt.state {
            RecoveryClaimState::Consumed { .. } => true,
            RecoveryClaimState::Invalidated { .. } | RecoveryClaimState::Superseded { .. } => false,
            RecoveryClaimState::Pending | RecoveryClaimState::IssueBound(_) => {
                matches!(
                    receipt.authorization.attribution,
                    dudes_proto::recovery::RecoveryAttribution::LocalOperator { .. }
                ) || self.recovery_browser_availability(work, operation)
                    == RecoveryAvailability::Live
            }
        }
    }

    fn recovery_browser_availability(
        &self,
        work: RecoveryWork,
        operation: &crate::RecoveryOperation<'_>,
    ) -> RecoveryAvailability {
        let Ok((state, _)) = self.world.recovery_projection() else {
            return RecoveryAvailability::Indeterminate;
        };
        let Ok(Some(claim)) = state.work_claim(work) else {
            return RecoveryAvailability::Indeterminate;
        };
        let Ok(receipt) = state.claim_receipt(claim) else {
            return RecoveryAvailability::Indeterminate;
        };
        if !matches!(
            receipt.authorization.attribution,
            dudes_proto::recovery::RecoveryAttribution::BrowserSession { .. }
        ) || state
            .receipt(
                receipt.authorization.request_id,
                receipt.authorization.request_digest,
            )
            .ok()
            .flatten()
            != Some(receipt.clone())
        {
            return RecoveryAvailability::Indeterminate;
        }
        let mut binding = receipt.authorization.binding.clone();
        let purpose = match receipt.state {
            RecoveryClaimState::Pending if work == binding.work => RecoverySubjectUse::AdmitNew,
            RecoveryClaimState::IssueBound(issue)
                if work == binding.work || work == issue.issued.work =>
            {
                binding.work = issue.issued.work;
                binding.issue_lsn = issue.lsn;
                RecoverySubjectUse::DispatchIssued(issue.clone())
            }
            _ => return RecoveryAvailability::Indeterminate,
        };
        let Ok(facts) = self.world.recovery_facts(&binding) else {
            return RecoveryAvailability::Indeterminate;
        };
        if facts.control_lsn != receipt.authorization.resulting_control
            || matches!(&purpose, RecoverySubjectUse::DispatchIssued(issue) if facts.source_issue != *issue)
        {
            return RecoveryAvailability::Indeterminate;
        }
        operation.availability_for(&RecoverySubjectKey::new(
            claim,
            receipt.authorization.clone(),
            purpose,
        ))
    }

    fn recovery_owed_saving(&self, agent: AgentId, turn: TurnId) -> bool {
        self.closing_active
            && self.world.closing.as_ref().is_some_and(|session| {
                !session.is_complete()
                    && session
                        .participants
                        .get(&agent)
                        .is_some_and(|participant| participant.saving_turn == Some(turn))
            })
    }

    pub(super) fn recovery_new_context(
        &self,
        agent: AgentId,
        operation: &crate::RecoveryOperation<'_>,
    ) -> Result<Option<NewContext>, ()> {
        let Ok(binding) = self.recovery_binding(agent) else {
            return Ok(None);
        };
        let (state, _) = self.world.recovery_projection().map_err(|_| ())?;
        let Some(claim) = state.work_claim(binding.work).map_err(|_| ())? else {
            return Ok(None);
        };
        let receipt = state.claim_receipt(claim).map_err(|_| ())?;
        if matches!(receipt.state, RecoveryClaimState::Consumed { .. }) {
            return Ok(None);
        }
        if !matches!(receipt.state, RecoveryClaimState::Pending) {
            return Err(());
        }
        if !self.recovery_dispatch_attribution_allowed(binding.work, operation) {
            return Err(());
        }
        if self.closing_active {
            return Err(());
        }
        let RecoveryPlan::New { turn, context } = &receipt.authorization.plan else {
            return Err(());
        };
        let (session, recap) = match context {
            RecoveryContext::TryRecorded { session } => (
                dudes_providers::adapter::TurnSession::Resume(session.clone()),
                String::new(),
            ),
            RecoveryContext::Fresh { session, recap } => (
                session.clone().map_or(
                    dudes_providers::adapter::TurnSession::Assigned,
                    dudes_providers::adapter::TurnSession::First,
                ),
                recap.text.clone(),
            ),
        };
        Ok(Some(NewContext {
            turn: *turn,
            session,
            recap,
            model: receipt.authorization.binding.model_selection.clone(),
        }))
    }

    fn recovery_recap(&self, binding: &RecoveryBinding) -> RecoveryRecap {
        let goal = &self.world.goals[&binding.work.goal];
        let role = &self.world.agents[&binding.work.agent].role;
        let mut text = format!(
            "Prior execution effects are unknown. This recap does not restore conversation history.\n\nRecorded goal: {}",
            goal.goal.text
        );
        let mut sources = vec![goal.installed_lsn];
        if text.len() < dudes_proto::recovery::MAX_RECOVERY_RECAP_BYTES && !role.is_empty() {
            text.push_str("\n\nRecorded role: ");
            text.push_str(role);
            sources.push(binding.hired_lsn);
        }
        if text.len() > dudes_proto::recovery::MAX_RECOVERY_RECAP_BYTES {
            let mut end = dudes_proto::recovery::MAX_RECOVERY_RECAP_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        sources.sort_unstable();
        sources.dedup();
        RecoveryRecap {
            text,
            sources,
            delivered_answers: Vec::new(),
        }
    }

    pub(super) fn prepare_new_issue(
        &mut self,
        agent: AgentId,
        row: &crate::world::AgentRow,
        task: &str,
        operation: &crate::RecoveryOperation<'_>,
    ) -> Option<(TurnId, Box<PreparedLaunch>)> {
        let recovery = self.recovery_new_context(agent, operation).ok()?;
        if recovery
            .as_ref()
            .is_some_and(|context| context.model.is_none())
        {
            return None;
        }
        let turn = recovery.as_ref().map_or_else(
            || TurnId::from_ulid(self.rng.ulid()),
            |context| context.turn,
        );
        let session = recovery.as_ref().map_or_else(
            || self.announce_session(agent, false),
            |context| context.session.clone(),
        );
        let recovered_task = with_recap(
            task,
            recovery
                .as_ref()
                .map_or("", |context| context.recap.as_str()),
        );
        let task = recovered_task.as_str();
        let selection = recovery
            .as_ref()
            .and_then(|context| context.model.clone())
            .or_else(|| self.model_selection(row))?;
        let launch = match self.launch(agent, row, task, &session, &selection) {
            ManagedLaunch::Ready(launch) => launch,
            ManagedLaunch::Unavailable => return None,
            ManagedLaunch::Refused(error) => {
                self.persist(
                    Record::AgentHeld {
                        agent,
                        provenance: Provenance::Observed,
                        cause: dudes_proto::AutomaticHold::LaunchRefused {
                            reason: error.to_string(),
                        },
                    },
                    Then::Decide { agent },
                );
                return None;
            }
        };
        Some((turn, launch))
    }

    pub(super) fn recovery_task(
        &self,
        agent: AgentId,
        task: &str,
        operation: &crate::RecoveryOperation<'_>,
    ) -> Option<String> {
        let context = self.recovery_new_context(agent, operation).ok()?;
        Some(match context {
            Some(context) => with_recap(task, &context.recap),
            None => task.to_owned(),
        })
    }

    pub(super) fn recovery_launch_context_allowed(
        &self,
        agent: AgentId,
        turn: TurnId,
        session: &dudes_providers::adapter::TurnSession,
        operation: &crate::RecoveryOperation<'_>,
    ) -> bool {
        self.recovery_launch_allowed(agent, turn, operation)
            && self
                .recovery_repeat_context(agent, turn)
                .is_none_or(|expected| &expected == session)
    }

    pub(super) fn recovery_repeat_context(
        &self,
        agent: AgentId,
        turn: TurnId,
    ) -> Option<dudes_providers::adapter::TurnSession> {
        let goal = self.world.turns.get(&turn)?.goal?;
        let (state, _) = self.world.recovery_projection().ok()?;
        let claim = state
            .work_claim(RecoveryWork { agent, goal, turn })
            .ok()??;
        let receipt = state.claim_receipt(claim).ok()?;
        match &receipt.authorization.plan {
            RecoveryPlan::RepeatIssued { session, .. } => Some(session.clone().map_or(
                dudes_providers::adapter::TurnSession::Assigned,
                dudes_providers::adapter::TurnSession::First,
            )),
            RecoveryPlan::New { context, .. } => match context {
                RecoveryContext::TryRecorded { session } => Some(
                    dudes_providers::adapter::TurnSession::Resume(session.clone()),
                ),
                RecoveryContext::Fresh { session, .. } => Some(session.clone().map_or(
                    dudes_providers::adapter::TurnSession::Assigned,
                    dudes_providers::adapter::TurnSession::First,
                )),
            },
        }
    }

    /// Identify only the actual parked command-owned entry; metadata is not authority.
    pub fn recovery_append(
        &self,
        token: PersistToken,
        entry: &Entry,
    ) -> Result<Option<RecoveryAppend>, RecoveryRefusal> {
        let parked = self
            .ledger
            .get(&token)
            .ok_or(RecoveryRefusal::Unavailable)?;
        if &parked.entry != entry {
            return Err(RecoveryRefusal::Unavailable);
        }
        let Then::Recovery(then) = &parked.then else {
            return Ok(None);
        };
        let (next, stage) = then.continuing();
        Ok(Some(RecoveryAppend {
            request_id: next.command.request_id,
            work: next.required.binding.work,
            stage,
            digest: next.command.digest,
            expected: next.command.expected,
            choice: next.command.choice,
            attribution: next.command.attribution,
            prefix: next.prefix,
        }))
    }

    /// Complete only a matching command append proven by the executor not to have written.
    /// Uncertain writes must retain the ordinary fence and replay path.
    pub fn recovery_refused(
        &mut self,
        token: PersistToken,
        entry: &Entry,
        expected: &RecoveryAppend,
        reason: RecoveryRefusal,
    ) -> Handled {
        self.pending.clear();
        if self.recovery_append(token, entry).as_ref() != Ok(&Some(*expected)) {
            return Handled::default();
        }
        let Some(super::Parked {
            then: Then::Recovery(then),
            ..
        }) = self.ledger.remove(&token)
        else {
            return Handled::default();
        };
        Handled::default().with_recovery(Some(then.continuing().0.update(Some(reason))))
    }
}

type Boundary = (RecoveryReason, RecoveryOrigin, RecoveryBoundary);

fn existing_required(
    episode: &dudes_proto::recovery_state::RecoveryEpisode,
    turn: &crate::world::TurnRow,
    candidates: &[Boundary],
) -> Result<RecoveryRequired, RecoveryRefusal> {
    let boundary = match episode.reason {
        RecoveryReason::HeldOrPausedFailure => RecoveryBoundary::Paused {
            end_lsn: turn.end_lsn.ok_or(RecoveryRefusal::NotRecoverable)?,
        },
        RecoveryReason::MissingSpawn => RecoveryBoundary::MissingSpawn,
        _ => candidates
            .iter()
            .find(|(reason, _, _)| *reason == episode.reason)
            .map(|(_, _, boundary)| boundary.clone())
            .ok_or(RecoveryRefusal::NotRecoverable)?,
    };
    let required = RecoveryRequired {
        binding: episode.binding.clone(),
        origin: if episode.reason == RecoveryReason::HeldOrPausedFailure {
            RecoveryOrigin::Existing {
                lsn: episode.id.origin,
            }
        } else {
            RecoveryOrigin::ThisBarrier
        },
        reason: episode.reason,
        boundary,
        predecessor: None,
    };
    Ok(required)
}

fn compare_expected(
    expected: &RecoveryExpected,
    actual: &RecoveryExpected,
) -> Result<(), RecoveryRefusal> {
    if expected.work != actual.work || expected.issue_lsn != actual.issue_lsn {
        return Err(RecoveryRefusal::StaleWork);
    }
    if expected.source_lsn != actual.source_lsn {
        return Err(RecoveryRefusal::StaleSource);
    }
    if expected.episode != actual.episode {
        return Err(RecoveryRefusal::StaleEpisode);
    }
    if expected.predecessor != actual.predecessor {
        return Err(RecoveryRefusal::StalePredecessor);
    }
    if expected.control_lsn != actual.control_lsn {
        return Err(RecoveryRefusal::StaleControl);
    }
    if expected.ticket != actual.ticket {
        return Err(RecoveryRefusal::StaleTicket);
    }
    Ok(())
}

fn opportunity(
    required: RecoveryRequired,
    episode: Option<RecoveryEpisodeId>,
    predecessor: Option<RecoveryClaimRef>,
    control_lsn: Option<Lsn>,
    recorded: bool,
) -> RecoveryOpportunity {
    let source_lsn = match required.origin {
        RecoveryOrigin::Existing { lsn } => lsn,
        RecoveryOrigin::ThisBarrier => match required.boundary {
            RecoveryBoundary::Vanished { end_lsn, .. }
            | RecoveryBoundary::Unconfirmed { end_lsn, .. } => end_lsn,
            _ => required.binding.issue_lsn,
        },
    };
    RecoveryOpportunity {
        expected: RecoveryExpected {
            work: required.binding.work,
            issue_lsn: required.binding.issue_lsn,
            source_lsn,
            episode,
            predecessor,
            control_lsn,
            ticket: required
                .binding
                .ticket
                .map(|ticket| (ticket.card, ticket.round)),
        },
        recorded_available: recorded
            && required.reason != RecoveryReason::UnconfirmedContext
            && required.reason != RecoveryReason::MissingSpawn,
        required,
    }
}

pub(super) const fn refusal(error: RecoveryStateError) -> RecoveryRefusal {
    match error {
        RecoveryStateError::Claimed | RecoveryStateError::Lifecycle => RecoveryRefusal::Claimed,
        RecoveryStateError::RequestConflict | RecoveryStateError::DuplicateRequest => {
            RecoveryRefusal::RequestConflict
        }
        RecoveryStateError::Context => RecoveryRefusal::ChoiceUnavailable,
        RecoveryStateError::Control => RecoveryRefusal::StaleControl,
        _ => RecoveryRefusal::StaleWork,
    }
}

pub(super) fn with_recap(task: &str, recap: &str) -> String {
    const SEPARATOR: &str = "\n\nPrevious work context:\n";
    if !recap.is_empty()
        && task
            .len()
            .saturating_add(SEPARATOR.len())
            .saturating_add(recap.len())
            <= dudes_policy::r#loop::NEXT_WORK_TASK_MAX_BYTES
    {
        format!("{task}{SEPARATOR}{recap}")
    } else {
        task.to_owned()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum InvalidationSubject {
    GoalSet(AgentId),
    GoalCleared(AgentId, dudes_types::GoalId),
    GoalReassigned(AgentId, dudes_types::GoalId),
    GoalReviewed(AgentId, dudes_types::GoalId),
    GoalCompleted(AgentId, dudes_types::GoalId),
    Control(AgentId),
    AgentUnavailable(AgentId),
    TicketAssigned(dudes_types::CardId),
    TicketStarted(dudes_types::CardId),
    TicketRemoved(dudes_types::CardId),
    Lease(dudes_types::WorktreeId),
}

impl InvalidationSubject {
    fn for_binding(binding: &RecoveryBinding) -> Vec<Self> {
        let work = binding.work;
        let mut keys = vec![
            Self::GoalSet(work.agent),
            Self::GoalCleared(work.agent, work.goal),
            Self::GoalReassigned(work.agent, work.goal),
            Self::GoalReviewed(work.agent, work.goal),
            Self::GoalCompleted(work.agent, work.goal),
            Self::Control(work.agent),
            Self::AgentUnavailable(work.agent),
        ];
        if let Some(ticket) = binding.ticket {
            keys.extend([
                Self::TicketAssigned(ticket.card),
                Self::TicketStarted(ticket.card),
                Self::TicketRemoved(ticket.card),
            ]);
        }
        if let Some(lease) = binding.worktree {
            keys.push(Self::Lease(lease));
        }
        keys
    }

    fn reason(self) -> dudes_proto::recovery::RecoveryInvalidationReason {
        use dudes_proto::recovery::RecoveryInvalidationReason as R;
        match self {
            Self::GoalSet(_)
            | Self::GoalCleared(..)
            | Self::GoalReassigned(..)
            | Self::GoalReviewed(..) => R::GoalChanged,
            Self::GoalCompleted(..) => R::GoalCompleted,
            Self::Control(_) => R::ControlChanged,
            Self::AgentUnavailable(_) => R::AgentUnavailable,
            Self::TicketAssigned(_) | Self::TicketStarted(_) | Self::TicketRemoved(_) => {
                R::TicketChanged
            }
            Self::Lease(_) => R::LeaseChanged,
        }
    }
}

#[derive(Default)]
pub(super) struct Discovery {
    claims: std::collections::BTreeMap<Lsn, RecoveryClaimRef>,
    subjects: std::collections::BTreeMap<InvalidationSubject, usize>,
    evidence: std::collections::BTreeMap<InvalidationSubject, [Option<Lsn>; 2]>,
    cursor: Option<Lsn>,
    dirty: bool,
    again: bool,
    inflight: Option<Lsn>,
}

impl Discovery {
    fn busy(&self) -> bool {
        self.dirty || self.inflight.is_some()
    }

    fn retire(&mut self, authorization: &RecoveryAuthorized, lsn: Lsn) {
        if self.claims.remove(&lsn).is_none() {
            return;
        }
        for key in InvalidationSubject::for_binding(&authorization.binding) {
            if let Some(count) = self.subjects.get_mut(&key) {
                *count -= 1;
                if *count == 0 {
                    self.subjects.remove(&key);
                    self.evidence.remove(&key);
                }
            }
        }
        if self.inflight == Some(lsn) {
            self.inflight = None;
        }
        if self.claims.is_empty() {
            self.cursor = None;
            self.dirty = false;
            self.again = false;
        }
    }

    fn next(&mut self) -> Option<RecoveryClaimRef> {
        if !self.dirty || self.inflight.is_some() {
            return None;
        }
        let after = self.cursor.unwrap_or(Lsn::ZERO);
        if let Some((&lsn, &claim)) = self
            .claims
            .range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
            .next()
        {
            self.cursor = Some(lsn);
            return Some(claim);
        }
        self.cursor = None;
        self.dirty = self.again;
        self.again = false;
        None
    }

    pub(super) fn folded(&mut self, world: &crate::World, lsn: Lsn, record: &Record) {
        let Ok((state, _)) = world.recovery_projection() else {
            return;
        };
        match record {
            Record::RecoveryAuthorized(authorization) => {
                let claim = RecoveryClaimRef {
                    episode: authorization.episode,
                    authorization_lsn: lsn,
                };
                if state
                    .claim_receipt(claim)
                    .is_ok_and(|receipt| matches!(receipt.state, RecoveryClaimState::Pending))
                    && self.claims.insert(lsn, claim).is_none()
                {
                    for key in InvalidationSubject::for_binding(&authorization.binding) {
                        *self.subjects.entry(key).or_default() += 1;
                    }
                }
            }
            Record::RecoveryInvalidated(invalidated) => {
                if let Ok(receipt) = state.claim_receipt(invalidated.claim) {
                    self.retire(receipt.authorization, invalidated.claim.authorization_lsn);
                }
            }
            _ => {
                if let Ok(source) = world.recovery_witness(lsn)
                    && let Some(issue) = source.issue()
                    && let Some(claim) = state.planned_claim(issue.issued.work.turn)
                    && let Ok(receipt) = state.claim_receipt(claim)
                    && !matches!(receipt.state, RecoveryClaimState::Pending)
                {
                    self.retire(receipt.authorization, claim.authorization_lsn);
                }
            }
        }
        self.capture(world, lsn);
    }

    fn capture(&mut self, world: &crate::World, lsn: Lsn) {
        use dudes_proto::RecordKind as K;
        use dudes_proto::recovery_state::history::RecoveryWitnessOwner as O;
        let Ok(source) = world.recovery_witness(lsn) else {
            return;
        };
        let reference = source.reference();
        let key = match (reference.kind, reference.owner) {
            (K::GoalSet, O::Goal { agent, .. }) => InvalidationSubject::GoalSet(agent),
            (K::GoalCleared, O::Goal { agent, goal }) => {
                InvalidationSubject::GoalCleared(agent, goal)
            }
            (K::GoalReassigned, O::Goal { agent, goal }) if matches!(source.evidence(), RecoveryEvidence::Reassigned { to } if *to != agent) => {
                InvalidationSubject::GoalReassigned(agent, goal)
            }
            (K::TicketInstructionReviewed, _) => {
                let Some(work) = source.context().cleared_goal else {
                    return;
                };
                InvalidationSubject::GoalReviewed(work.agent, work.goal)
            }
            (K::PredicateResult, _) => {
                let Some(work) = source.context().qualified_completion else {
                    return;
                };
                InvalidationSubject::GoalCompleted(work.agent, work.goal)
            }
            (K::Pause | K::Resume | K::Halt | K::AgentHeld, O::Agent(agent)) => {
                InvalidationSubject::Control(agent)
            }
            (K::AgentRetired | K::Attached, O::Agent(agent)) => {
                InvalidationSubject::AgentUnavailable(agent)
            }
            (K::TicketAssigned, O::Card(card)) => InvalidationSubject::TicketAssigned(card),
            (K::TicketRemoved, O::Card(card)) => InvalidationSubject::TicketRemoved(card),
            (K::TicketStarted, O::Ticket { card, .. }) => InvalidationSubject::TicketStarted(card),
            (K::WorktreeRemoved, O::Worktree(lease)) => InvalidationSubject::Lease(lease),
            _ => return,
        };
        if !self.subjects.contains_key(&key) {
            return;
        }
        let old = self.evidence.get(&key).copied().unwrap_or([None; 2]);
        let distinct = matches!(
            key,
            InvalidationSubject::TicketStarted(_) | InvalidationSubject::GoalSet(_)
        ) && old[0]
            .and_then(|at| world.recovery_witness(at).ok())
            .is_some_and(|old| old.reference().owner != reference.owner);
        self.evidence
            .insert(key, [Some(lsn), if distinct { old[0] } else { old[1] }]);
        if self.dirty {
            self.again = true;
        } else {
            self.dirty = true;
            self.cursor = None;
        }
    }

    fn candidates(
        &self,
        binding: &RecoveryBinding,
    ) -> Vec<dudes_proto::recovery::RecoveryInvalidationEvidence> {
        InvalidationSubject::for_binding(binding)
            .into_iter()
            .flat_map(|key| {
                self.evidence
                    .get(&key)
                    .copied()
                    .unwrap_or([None; 2])
                    .into_iter()
                    .flatten()
                    .map(move |lsn| {
                        dudes_proto::recovery::RecoveryInvalidationEvidence::RecordedChange {
                            reason: key.reason(),
                            lsn,
                        }
                    })
            })
            .collect()
    }
}

impl<C: Clock, R: Rng> Supervisor<C, R> {
    pub(super) fn drain_recovery_invalidations(
        &mut self,
        operation: &crate::RecoveryOperation<'_>,
    ) {
        if operation.unavailable_subjects().any(|key| {
            matches!(key.purpose(), RecoverySubjectUse::AdmitNew)
                && self.recovery_browser_availability(key.claim().episode.work, operation)
                    == RecoveryAvailability::EstablishedUnavailable
        }) {
            self.recovery_discovery.again |= self.recovery_discovery.dirty;
            self.recovery_discovery.dirty = true;
        }
        for _ in 0..16 {
            let Some(claim) = self.recovery_discovery.next() else {
                return;
            };
            let Ok((state, history)) = self.world.recovery_projection() else {
                return;
            };
            let Ok(receipt) = state.claim_receipt(claim) else {
                return;
            };
            if !matches!(receipt.state, RecoveryClaimState::Pending) {
                self.recovery_discovery
                    .retire(receipt.authorization, claim.authorization_lsn);
                continue;
            }
            let Some(through) = self.world.last_applied_lsn else {
                return;
            };
            let candidate = self
                .recovery_discovery
                .candidates(&receipt.authorization.binding)
                .into_iter()
                .find_map(|evidence| {
                    let record = dudes_proto::recovery::RecoveryInvalidated { claim, evidence };
                    state
                        .validate_invalidation(&record, through, history)
                        .is_ok()
                        .then_some(record)
                }).or_else(|| {
                    if self.recovery_browser_availability(receipt.authorization.binding.work, operation)
                        != RecoveryAvailability::EstablishedUnavailable {
                        return None;
                    }
                    let dudes_proto::recovery::RecoveryAttribution::BrowserSession { session_digest } = receipt.authorization.attribution else {
                        return None;
                    };
                    let record = dudes_proto::recovery::RecoveryInvalidated {
                        claim,
                        evidence: dudes_proto::recovery::RecoveryInvalidationEvidence::BrowserAuthorityUnavailable { session_digest },
                    };
                    state.validate_invalidation(&record, through, history).is_ok().then_some(record)
                });
            if let Some(record) = candidate {
                let agent = receipt.authorization.binding.work.agent;
                self.recovery_discovery.inflight = Some(claim.authorization_lsn);
                self.persist(Record::RecoveryInvalidated(record), Then::Decide { agent });
                return;
            }
        }
    }
}
