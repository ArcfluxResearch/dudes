//! Recovery facts read existing fold owners and immutable source witnesses at one cut.

use dudes_proto::recovery::RecoveryBinding;
use dudes_proto::recovery_state::admission::{
    RecoveryAgentCurrent, RecoveryCurrent, RecoveryGoalCurrent, RecoveryLeaseCurrent,
};
use dudes_proto::recovery_state::history::{
    RecoveryPriorControl, RecoveryWitness, RecoveryWitnessContext, task_digest,
};
use dudes_proto::recovery_state::{RecoveryFacts, RecoveryStateError};
use dudes_proto::{Entry, Record};
use dudes_types::{Lsn, Mode};

use super::World;

impl World {
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
            ticket,
            lease,
        }
        .bind(binding, &self.recovery_history)
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
        RecoveryWitnessContext {
            previous_control: agent.and_then(|agent| {
                self.agents.get(&agent).map(|row| RecoveryPriorControl {
                    agent,
                    lsn: row.control_lsn,
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
        halted: agent.halted,
        goal: agent.goal,
        control_lsn: agent.control_lsn,
        session: agent.session.clone(),
        session_lsn: agent.session_lsn,
    }
}
