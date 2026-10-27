//! Adapts goal-owned review evidence to the shared pure fold.

use dudes_proto::Entry;
use dudes_proto::repository_review::{self, ReviewFacts, TicketReviewState};
use dudes_proto::repository_state::history::RepositoryStateError;
use dudes_types::{GoalId, Lsn};

use super::World;

impl World {
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
