mod record;

use dudes_proto::recovery::RecoveryBinding;
use dudes_proto::recovery_state::admission::{
    RecoveryAgentCurrent, RecoveryCurrent, RecoveryGoalCurrent, RecoveryLeaseCurrent,
};
use dudes_proto::recovery_state::history::{
    RecoveryAttemptContext, RecoveryHoldWitness, RecoveryPriorControl, RecoveryStandingControl,
    RecoveryWitness, RecoveryWitnessContext, task_digest,
};
use dudes_proto::recovery_state::{
    RecoveryAttempt, RecoveryAttemptFacts, RecoveryFacts, RecoveryStateError,
};
use dudes_proto::{Entry, Record};
use dudes_types::{Lsn, Mode};

use super::World;

impl World {
    /// Complete journal-derived recovery state and source evidence for comparison or inspection.
    /// The immutable view supplies neither store certification nor permission to dispatch work.
    pub fn recovery_projection(
        &self,
    ) -> Result<
        (
            &dudes_proto::recovery_state::RecoveryState,
            &dudes_proto::recovery_state::history::RecoveryHistory,
        ),
        RecoveryStateError,
    > {
        if self.recovery_history_error.is_some() {
            return Err(RecoveryStateError::Binding);
        }
        Ok((&self.recovery_state, &self.recovery_history))
    }

    /// Join one target to this confirmed prefix. This result is not operator permission.
    pub fn recovery_facts(
        &self,
        binding: &RecoveryBinding,
    ) -> Result<RecoveryFacts, RecoveryStateError> {
        if self.recovery_history_error.is_some() {
            return Err(RecoveryStateError::Binding);
        }
        let through = self.last_applied_lsn.ok_or(RecoveryStateError::Position)?;
        let agent = self
            .agents
            .get(&binding.work.agent)
            .ok_or(RecoveryStateError::Binding)?;
        let goal = self
            .goals
            .get(&binding.work.goal)
            .ok_or(RecoveryStateError::Binding)?;
        let turn = self
            .turns
            .get(&binding.work.turn)
            .ok_or(RecoveryStateError::Binding)?;
        let mut issue = self
            .recovery_history
            .at(turn.issue_lsn)
            .map_err(|_| RecoveryStateError::Binding)?
            .issue()
            .ok_or(RecoveryStateError::Binding)?;
        issue.issued.work.agent = turn.agent;
        issue.issued.work.goal = turn.goal.ok_or(RecoveryStateError::Binding)?;
        issue.issued.task_digest = task_digest(&turn.task, &hash);
        issue.issued.answers.clone_from(&turn.answers);
        issue
            .issued
            .model_selection
            .clone_from(&turn.model_selection);
        let ticket = self
            .ticket_goal(binding.work.goal)
            .and_then(|(card, round)| {
                let recorded = self
                    .instructions
                    .recorded_association_for_goal(binding.work.goal)?;
                let active = self
                    .instructions
                    .association_for_goal(binding.work.goal, recorded.seq_at_start)?;
                (active.card == card && active.goal == binding.work.goal).then_some(
                    dudes_proto::diff_stamp::TicketBinding {
                        card,
                        round,
                        goal: binding.work.goal,
                        claimant: goal.agent,
                    },
                )
            });
        let lease = agent
            .worktree
            .map(|id| {
                let row = self.worktrees.get(&id).ok_or(RecoveryStateError::Binding)?;
                Ok(RecoveryLeaseCurrent {
                    worktree: id,
                    agent: row.agent,
                    removed: row.removed,
                })
            })
            .transpose()?;
        RecoveryCurrent {
            through,
            agent: current_agent(binding.work.agent, agent),
            goal: RecoveryGoalCurrent {
                goal: goal.goal.id,
                agent: goal.agent,
                installed_lsn: goal.installed_lsn,
                last_turn: goal.last_turn,
                cleared: goal.cleared,
                done: self.completed_goals.contains_key(&binding.work.goal),
                predicate: &goal.goal.predicate,
                result_lsn: goal.predicate_lsn,
                card_change_lsn: match goal.goal.predicate {
                    dudes_proto::Predicate::BoardState { card, .. } => {
                        self.recovery_card_changes.get(&card).copied()
                    }
                    _ => None,
                },
                retired_lsn: self.recovery_last_retirement,
            },
            issue,
            mode: Mode::Managed,
            attempt: RecoveryAttempt {
                spawn_lsn: turn.spawn_lsn,
                end_lsn: turn.end_lsn,
                confirmation_lsn: turn.confirmation_lsn,
            },
            ticket,
            lease,
        }
        .bind(binding, &self.recovery_history)
    }

    /// Validate a typed transition before any mutable world fold.
    /// This historical preparation cannot authenticate a command or grant dispatch.
    pub fn prepare_recovery(
        &self,
        transition: dudes_proto::recovery_state::transition::RecoveryTransition<'_>,
    ) -> Result<(), RecoveryStateError> {
        if self.recovery_history_error.is_some() {
            return Err(RecoveryStateError::Binding);
        }
        if let dudes_proto::recovery_state::transition::RecoveryTransition::Invalidated(record) =
            transition
        {
            let through = self.last_applied_lsn.ok_or(RecoveryStateError::Position)?;
            return self.recovery_state.validate_invalidation(
                record,
                through,
                &self.recovery_history,
            );
        }
        if let dudes_proto::recovery_state::transition::RecoveryTransition::Consumed {
            claim,
            issue,
            evidence,
        } = transition
        {
            let facts = self.recovery_attempt_facts(issue)?;
            return self
                .recovery_state
                .validate_consumed(claim, issue, evidence, &facts);
        }
        let binding = transition.binding(&self.recovery_state)?;
        let facts = self.recovery_facts(&binding)?;
        self.recovery_state
            .validate_transition(transition, &facts, &self.recovery_history)?;
        self.validate_recovery_new_turn(transition)
    }

    fn validate_recovery_new_turn(
        &self,
        transition: dudes_proto::recovery_state::transition::RecoveryTransition<'_>,
    ) -> Result<(), RecoveryStateError> {
        if let dudes_proto::recovery_state::transition::RecoveryTransition::Authorized(record) =
            transition
            && let dudes_proto::recovery::RecoveryPlan::New { turn, .. } = &record.plan
            && self.turns.contains_key(turn)
        {
            return Err(RecoveryStateError::Binding);
        }
        Ok(())
    }

    fn recovery_attempt_facts(
        &self,
        issue: &dudes_proto::recovery::RecoveryIssue,
    ) -> Result<RecoveryAttemptFacts, RecoveryStateError> {
        let through = self.last_applied_lsn.ok_or(RecoveryStateError::Position)?;
        let row = self
            .turns
            .get(&issue.issued.work.turn)
            .ok_or(RecoveryStateError::Binding)?;
        let recorded = self
            .recovery_history
            .at(row.issue_lsn)
            .map_err(|_| RecoveryStateError::Binding)?
            .issue()
            .ok_or(RecoveryStateError::Binding)?;
        if row.agent != issue.issued.work.agent
            || row.goal != Some(issue.issued.work.goal)
            || row.model_selection != issue.issued.model_selection
            || row.answers != issue.issued.answers
            || task_digest(&row.task, &hash) != issue.issued.task_digest
            || recorded != *issue
        {
            return Err(RecoveryStateError::Binding);
        }
        Ok(RecoveryAttemptFacts {
            through,
            issue: recorded,
            attempt: RecoveryAttempt {
                spawn_lsn: row.spawn_lsn,
                end_lsn: row.end_lsn,
                confirmation_lsn: row.confirmation_lsn,
            },
        })
    }

    /// Read retained source evidence without interpreting it as a recovery permission.
    pub fn recovery_witness(&self, lsn: Lsn) -> Result<&RecoveryWitness, RecoveryStateError> {
        if self.recovery_history_error.is_some() {
            return Err(RecoveryStateError::Binding);
        }
        if lsn == Lsn::ZERO || self.last_applied_lsn.is_none_or(|through| lsn > through) {
            return Err(RecoveryStateError::Position);
        }
        self.recovery_history
            .at(lsn)
            .map_err(|_| RecoveryStateError::Binding)
    }

    pub(super) fn recovery_before(&self, entry: &Entry) -> RecoveryWitnessContext {
        let agent = match &entry.record {
            Record::Pause { agent, .. }
            | Record::Resume { agent, .. }
            | Record::Halt { agent, .. }
            | Record::AgentHeld { agent, .. } => Some(*agent),
            _ => None,
        };
        let attempt = match &entry.record {
            Record::TurnSpawned { agent, turn, .. }
            | Record::SessionLearned { agent, turn, .. }
            | Record::TurnEnded { agent, turn, .. } => self
                .turns
                .get(turn)
                .filter(|row| row.agent == *agent)
                .map(|row| RecoveryAttemptContext {
                    issue_lsn: row.issue_lsn,
                    previous_spawn_lsn: row.spawn_lsn,
                    previous_end_lsn: row.end_lsn,
                    confirmed_session_lsn: self
                        .agents
                        .get(agent)
                        .and_then(|owner| owner.session_lsn),
                }),
            _ => None,
        };
        let qualified_completion = match &entry.record {
            Record::PredicateResult { goal, .. } => self
                .goals
                .get(goal)
                .filter(|row| !row.cleared && !self.completed_goals.contains_key(goal))
                .and_then(|row| {
                    dudes_proto::recovery_state::history::qualified_completion(
                        row.agent,
                        row.goal.id,
                        &row.goal.predicate,
                        &entry.record,
                    )
                }),
            _ => None,
        };
        RecoveryWitnessContext {
            qualified_completion,
            attempt,
            previous_control: agent.and_then(|agent| {
                self.agents.get(&agent).map(|row| RecoveryPriorControl {
                    agent,
                    lsn: row.control_lsn,
                    standing: standing_control(row),
                })
            }),
            cleared_goal: None,
        }
    }

    pub(super) fn capture_recovery_positions(&mut self, lsn: Lsn, entry: &Entry) {
        match &entry.record {
            Record::CardCreated { card, .. } | Record::TicketCreated { card, .. } => {
                self.recovery_card_changes.insert(*card, lsn);
            }
            Record::CardTransition { card, .. }
            | Record::Claim { card, .. }
            | Record::TicketAssigned { card, .. }
            | Record::TicketStarted { card, .. } => {
                if let Some(last) = self.recovery_card_changes.get_mut(card) {
                    *last = lsn;
                }
            }
            Record::TicketRemoved { card, .. } => {
                self.recovery_card_changes.remove(card);
            }
            Record::AgentRetired { .. } => self.recovery_last_retirement = Some(lsn),
            _ => {}
        }
    }
}

pub(super) fn hash(bytes: &[u8]) -> dudes_proto::record::Hex32 {
    dudes_proto::record::Hex32::new(*blake3::hash(bytes).as_bytes())
}

fn standing_control(agent: &super::AgentRow) -> RecoveryStandingControl {
    RecoveryStandingControl {
        paused: agent.paused,
        halted: agent.halted,
        hold: agent.automatic_hold.as_ref().map(RecoveryHoldWitness::from),
    }
}

fn current_agent(id: dudes_types::AgentId, agent: &super::AgentRow) -> RecoveryAgentCurrent {
    RecoveryAgentCurrent {
        agent: id,
        hired_lsn: agent.mcp.hire_lsn,
        provider: agent.provider,
        worktree: agent.worktree,
        mode: if agent.attached {
            Mode::Attached
        } else {
            Mode::Managed
        },
        retired: agent.retired,
        control: standing_control(agent),
        goal: agent.goal,
        control_lsn: agent.control_lsn,
        session: agent.session.clone(),
        session_lsn: agent.session_lsn,
    }
}
