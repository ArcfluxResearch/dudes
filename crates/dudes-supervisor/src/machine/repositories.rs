use dudes_proto::issue::ManagedIssuePayload;
use dudes_proto::repository::{
    HireRepositories, RepositoryTurnGrant, RepositoryTurnIssued, SingletonRepository,
};

use super::{AgentId, Clock, Entry, Record, Rng, Supervisor, TurnId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum IssueRepositories {
    Legacy(SingletonRepository),
    Scoped(Box<RepositoryTurnGrant>),
}

impl IssueRepositories {
    pub(super) fn record(&self, payload: ManagedIssuePayload) -> Record {
        match self {
            Self::Legacy(_) => payload.into_legacy_record(),
            Self::Scoped(grant) => {
                Record::TurnIssuedWithRepositories(Box::new(RepositoryTurnIssued {
                    grant: *grant.clone(),
                    payload,
                }))
            }
        }
    }
}

impl<C: Clock, R: Rng> Supervisor<C, R> {
    /// Check a scoped physical session against this issue and its admitted repeat context.
    #[must_use]
    pub fn scoped_launch_session_matches(
        &self,
        turn: TurnId,
        session: &dudes_providers::adapter::TurnSession,
    ) -> bool {
        use dudes_providers::adapter::TurnSession;
        let Some(issued) = self.world.turns.get(&turn) else {
            return false;
        };
        let Some(agent) = self.world.agents.get(&issued.agent) else {
            return false;
        };
        let expected = self
            .recovery_repeat_context(issued.agent, turn)
            .unwrap_or_else(|| {
                if let Some(session) = self.reissued.get(&turn) {
                    return session
                        .clone()
                        .map_or(TurnSession::Assigned, TurnSession::First);
                }
                issued
                    .announced
                    .clone()
                    .map_or(TurnSession::Assigned, |id| {
                        if agent.session.as_ref() == Some(&id) {
                            TurnSession::Resume(id)
                        } else {
                            TurnSession::First(id)
                        }
                    })
            });
        &expected == session
    }

    pub(super) fn issue_repositories(&self, agent: AgentId) -> Option<IssueRepositories> {
        if !self.repository_admitted(agent) {
            return None;
        }
        let row = self.world.agents.get(&agent)?;
        match &row.repositories {
            HireRepositories::Legacy(repository) => {
                Some(IssueRepositories::Legacy(repository.clone()))
            }
            HireRepositories::Scoped(hire) => {
                if hire.policy.require_confined().is_err() {
                    return None;
                }
                let round = self.world.active_repository_round(agent).ok()?;
                if round.is_some_and(|round| !self.repository_baseline_ready(round.goal)) {
                    return None;
                }
                RepositoryTurnGrant::select(row.mcp.hire_lsn, hire, round)
                    .ok()
                    .map(Box::new)
                    .map(IssueRepositories::Scoped)
            }
        }
    }

    pub(super) fn issued_repositories(
        &self,
        agent: AgentId,
        turn: TurnId,
    ) -> Option<IssueRepositories> {
        if !self.repository_admitted(agent) {
            return None;
        }
        let row = self.world.agents.get(&agent)?;
        let issued = self
            .world
            .turns
            .get(&turn)
            .filter(|row| row.agent == agent)?;
        let authority = issued.repositories.as_ref()?;
        if !authority.matches_hire(row.mcp.hire_lsn, &row.repositories) {
            return None;
        }
        match authority.grant() {
            Some(grant) => Some(IssueRepositories::Scoped(Box::new(grant.clone()))),
            None => authority
                .singleton()
                .cloned()
                .map(IssueRepositories::Legacy),
        }
    }

    pub(super) fn issue_repositories_current(&self, agent: AgentId, entry: &Entry) -> bool {
        let Some(issue) = entry
            .record
            .turn_issue()
            .filter(|issue| issue.agent == agent)
        else {
            return false;
        };
        if !self.repository_admitted(agent) || self.world.validate_repository_record(entry).is_err()
        {
            return false;
        }
        issue.repositories.is_none_or(|grant| {
            grant
                .ticket
                .as_ref()
                .is_none_or(|round| self.repository_baseline_ready(round.goal))
        })
    }

    fn repository_baseline_ready(&self, goal: super::GoalId) -> bool {
        self.world.repository_review(goal).is_some_and(|review| {
            review
                .baseline
                .as_ref()
                .is_some_and(|(_, batch)| batch.complete())
        })
    }

    pub(super) fn hold_for_issue_baseline(&mut self, agent: AgentId) -> bool {
        if self
            .world
            .agents
            .get(&agent)
            .is_some_and(|row| matches!(row.repositories, HireRepositories::Scoped(_)))
        {
            return self.hold_for_repository_baseline(agent);
        }
        self.hold_for_ticket_baseline(agent)
    }
}
