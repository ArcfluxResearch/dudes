//! Adapts goal-owned review evidence to the shared pure fold.

use dudes_proto::Entry;
use dudes_proto::repository_review::{self, ReviewFacts, TicketReviewState};
use dudes_proto::repository_state::history::RepositoryStateError;
use dudes_types::{GoalId, Lsn};

use super::World;

impl World {
    /// The recorded directory for a goal's check, with scoped tickets using their frozen member.
    #[must_use]
    pub fn predicate_directory(&self, goal: GoalId) -> Option<&std::path::Path> {
        let owner = self.agents.get(&self.goals.get(&goal)?.agent)?;
        match &owner.repositories {
            dudes_proto::repository::HireRepositories::Legacy(repository) => Some(repository.cwd()),
            dudes_proto::repository::HireRepositories::Scoped(hire) => {
                match self
                    .active_repository_round(self.goals.get(&goal)?.agent)
                    .ok()?
                {
                    Some(round) if round.goal == goal => {
                        Some(&round.authority.as_ref()?.check_root.directory)
                    }
                    None if self.ticket_repository_round(goal).is_none() => {
                        Some(&hire.repositories.primary().recorded.execution.directory)
                    }
                    _ => None,
                }
            }
        }
    }

    /// Validate a prospective review observation through the common historical fold.
    pub fn validate_review_record(&self, entry: &Entry) -> Result<(), RepositoryStateError> {
        let next = self
            .last_applied_lsn
            .unwrap_or(Lsn::ZERO)
            .0
            .checked_add(1)
            .ok_or(RepositoryStateError::Review)?;
        self.prepare_review_record(Lsn(next), entry).map(|_| ())
    }

    pub(super) fn prepare_review_record(
        &self,
        lsn: Lsn,
        entry: &Entry,
    ) -> Result<Option<(GoalId, TicketReviewState)>, RepositoryStateError> {
        let Some(goal) = repository_review::affected_goal(&entry.record) else {
            return Ok(None);
        };
        let row = self.goals.get(&goal);
        let owner = row
            .and_then(|row| self.agents.get(&row.agent))
            .filter(|owner| !owner.retired);
        let review = repository_review::prepare(
            lsn,
            &entry.record,
            row.map(|row| ReviewFacts {
                goal: row.goal.id,
                agent: row.agent,
                predicate: &row.goal.predicate,
                check_baseline: row.goal.baseline.as_ref(),
                round: self.ticket_goals.get(&goal),
                hire: owner.map(|owner| (owner.mcp.hire_lsn, &owner.repositories)),
                review: row.review.as_ref(),
            }),
        )?;
        Ok(review.map(|review| (goal, review)))
    }
}
