use dudes_proto::Entry;
use dudes_proto::instruction::InstructionObservationOwner;
use dudes_proto::instruction_state::InstructionStateError;
use dudes_proto::repository_instruction::{self, RepositoryInstructionFacts};
use dudes_types::Lsn;

use super::World;

impl World {
    pub(super) fn validate_instruction_authority(
        &self,
        lsn: Lsn,
        entry: &Entry,
    ) -> Result<(), InstructionStateError> {
        if let Some(owner) = repository_instruction::observation_owner(&entry.record) {
            let (agent, turn) = match owner {
                InstructionObservationOwner::Managed { agent, turn, .. }
                | InstructionObservationOwner::AttachedTurn { agent, turn, .. } => {
                    (agent, Some(turn))
                }
                InstructionObservationOwner::AttachedProcess { agent, .. } => (agent, None),
            };
            let issued = turn.and_then(|turn| self.turns.get(&turn));
            let issued_scope = issued.and_then(|turn| turn.repositories.as_ref());
            let hire = self
                .agents
                .get(&agent)
                .filter(|owner| !owner.retired)
                .map(|row| (row.mcp.hire_lsn, &row.repositories))
                .or_else(|| issued_scope.map(|scope| (scope.hired_lsn(), scope.hire())));
            repository_instruction::validate_observation(
                lsn,
                &entry.record,
                RepositoryInstructionFacts {
                    agent,
                    hire,
                    round: if repository_instruction::reads_active_round(&entry.record) {
                        self.active_repository_round(agent)
                            .map_err(|_| InstructionStateError::Boundary)?
                    } else {
                        None
                    },
                    issue: issued
                        .zip(issued_scope.and_then(|scope| scope.grant()))
                        .map(|(issued, grant)| (issued.issue_lsn, grant)),
                },
            )?;
        }
        if let Some(card) = repository_instruction::completion_card(&entry.record) {
            for (turn_id, _) in self.turns.iter().filter(|(_, turn)| {
                turn.repositories
                    .as_ref()
                    .and_then(|scope| scope.grant())
                    .and_then(|grant| grant.ticket.as_ref())
                    .is_some_and(|round| {
                        round.card == card
                            && self
                                .repository_tickets
                                .get(&card)
                                .and_then(super::repositories::Ticket::created)
                                .is_some_and(|ticket| ticket.goal == Some(round.goal))
                    })
            }) {
                if !self.instructions.repository_turn_covered(*turn_id) {
                    return Err(InstructionStateError::PendingComparison);
                }
            }
        }
        Ok(())
    }
}
