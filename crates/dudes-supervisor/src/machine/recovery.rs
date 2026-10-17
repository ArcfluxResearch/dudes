use dudes_proto::recovery::{
    RecoveryAuthorized, RecoveryBinding, RecoveryBoundary, RecoveryClaimRef, RecoveryContext,
    RecoveryEpisodeId, RecoveryFailure, RecoveryOrigin, RecoveryPlan, RecoveryReason,
    RecoveryRecap, RecoveryRequired, RecoveryWork,
};
use dudes_proto::recovery_state::history::{RecoveryEndClass, RecoveryEvidence};
use dudes_proto::recovery_state::transition::RecoveryTransition;
use dudes_proto::recovery_state::{RecoveryClaimState, RecoveryStateError};
use dudes_proto::{AutomaticHold, Entry, Record};
use dudes_types::{AgentId, Lsn, TurnId};

use super::{Clock, Rng, Supervisor, Then};
use crate::input::{
    Handled, PersistToken, RecoveryAppend, RecoveryChoice, RecoveryCommand, RecoveryExpected,
    RecoveryOpportunity, RecoveryPrefix, RecoveryRefusal, RecoveryStage, RecoveryUpdate,
};

#[cfg(test)]
mod tests;

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
    pub fn recovery_opportunity(
        &self,
        agent: AgentId,
    ) -> Result<RecoveryOpportunity, RecoveryRefusal> {
        let binding = self.recovery_binding(agent)?;
        let facts = self.world.recovery_facts(&binding).map_err(refusal)?;
        if facts.control.halted {
            return Err(RecoveryRefusal::NotRecoverable);
        }
        let (state, _) = self.world.recovery_projection().map_err(refusal)?;
        let current = state.current(binding.work);
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
            return Ok(opportunity(
                required,
                Some(episode.id),
                None,
                facts.control_lsn,
                facts.confirmed_session.is_some(),
            ));
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
    pub fn recover(&mut self, command: RecoveryCommand) -> Handled {
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
        let view = self.recovery_opportunity(command.expected.work.agent)?;
        compare_expected(command.expected, view.expected)?;
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

    pub(super) fn recovery_after_ack(&mut self, then: RecoveryThen, lsn: Lsn) -> RecoveryUpdate {
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
                    self.reissue_turn(work.turn, work.agent, &turn.task, turn.model_selection);
                } else {
                    self.decide(work.agent);
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
                Some(dudes_proto::recovery_state::history::RecoveryHoldWitness::Unknown)
            ) {
                return next.update(Some(RecoveryRefusal::NotRecoverable));
            }
            let update = next.update(None);
            self.persist(
                Record::Resume {
                    agent: next.required.binding.work.agent,
                    browser: None,
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
                    recap: RecoveryRecap::default(),
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

    pub(super) fn recovery_admission_allowed(&self, agent: AgentId) -> bool {
        let Ok(binding) = self.recovery_binding(agent) else {
            return true;
        };
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
                return self.recovery_opportunity(agent).is_err();
            }
            if self
                .world
                .recovery_facts(&receipt.authorization.binding)
                .is_err()
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
                RecoveryClaimState::Pending => true,
                RecoveryClaimState::IssueBound(issue) => {
                    self.live_issues.contains(&issue.issued.work.turn)
                }
                RecoveryClaimState::Consumed { .. } => self.recovery_opportunity(agent).is_err(),
                RecoveryClaimState::Invalidated { .. } | RecoveryClaimState::Superseded { .. } => {
                    false
                }
            };
        }
        self.live_issues.contains(&binding.work.turn) || self.recovery_opportunity(agent).is_err()
    }

    pub(super) fn ensure_recovery_admission(&mut self, agent: AgentId) -> bool {
        if self.recovery_admission_allowed(agent) {
            return true;
        }
        if let Ok(view) = self.recovery_opportunity(agent) {
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
    pub fn recovery_launch_allowed(&self, agent: AgentId, turn: TurnId) -> bool {
        let Some(row) = self.world.turns.get(&turn).filter(|row| row.agent == agent) else {
            return false;
        };
        if !row.is_issued_not_spawned() {
            return false;
        }
        if self.world.agents.get(&agent).is_none_or(|owner| {
            owner.retired
                || owner.halted
                || owner.paused
                || owner.attached
                || owner.open_question.is_some()
        }) || self.launch_held(agent)
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
        let Some(goal) = row.goal else {
            return self.recovery_admission_allowed(agent);
        };
        if self
            .world
            .agents
            .get(&agent)
            .is_none_or(|owner| owner.goal != Some(goal))
        {
            return false;
        }
        if !self.live_issues.contains(&turn) {
            return false;
        }
        let Ok((state, _)) = self.world.recovery_projection() else {
            return false;
        };
        let work = RecoveryWork { agent, goal, turn };
        let claim = match state.work_claim(work) {
            Ok(claim) => claim,
            Err(_) => return false,
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

    pub(super) fn recovery_new_context(
        &self,
        agent: AgentId,
    ) -> Result<Option<(TurnId, dudes_providers::adapter::TurnSession, String)>, ()> {
        let Ok(binding) = self.recovery_binding(agent) else {
            return Ok(None);
        };
        let (state, _) = self.world.recovery_projection().map_err(|_| ())?;
        let Some(episode) = state.current(binding.work) else {
            return Ok(None);
        };
        let claim = RecoveryClaimRef {
            episode: episode.id,
            authorization_lsn: episode.authorization_lsn.ok_or(())?,
        };
        let receipt = state.claim_receipt(claim).map_err(|_| ())?;
        if matches!(receipt.state, RecoveryClaimState::Consumed { .. }) {
            return Ok(None);
        }
        if !matches!(receipt.state, RecoveryClaimState::Pending) {
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
        Ok(Some((*turn, session, recap)))
    }

    fn recovery_repeat_context(
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
    pub fn recovery_append(&self, token: PersistToken, entry: &Entry) -> Option<RecoveryAppend> {
        let parked = self.ledger.get(&token)?;
        if &parked.entry != entry {
            return None;
        }
        let Then::Recovery(then) = &parked.then else {
            return None;
        };
        let (next, stage) = then.continuing();
        Some(RecoveryAppend {
            request_id: next.command.request_id,
            work: next.required.binding.work,
            stage,
        })
    }

    /// Complete only a matching command append proven by the executor not to have written.
    /// Uncertain writes must retain the ordinary fence and replay path.
    pub fn recovery_refused(
        &mut self,
        token: PersistToken,
        entry: &Entry,
        expected: RecoveryAppend,
        reason: RecoveryRefusal,
    ) -> Handled {
        self.pending.clear();
        if self.recovery_append(token, entry) != Some(expected) {
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
    expected: RecoveryExpected,
    actual: RecoveryExpected,
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
