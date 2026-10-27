//! Row-owned repository history and the common pure relationship fold.

use dudes_proto::repository::HireRepositories;
use dudes_proto::repository_state::history::{
    self, RepositoryHistoryChange, RepositoryHistoryFacts, RepositoryLease, RepositoryStateError,
};
use dudes_proto::{Entry, Record};
use dudes_types::Lsn;

use super::World;

impl World {
    pub(super) fn prepare_repository_record(
        &self,
        lsn: Lsn,
        entry: &Entry,
    ) -> Result<RepositoryHistoryChange, RepositoryStateError> {
        let project = match &entry.record {
            Record::ProjectRepositoriesRecorded { project, .. }
            | Record::RepositoryTrustSet { project, .. } => Some(project.as_str()),
            Record::AgentHiredWithRepositories(hire) => Some(hire.repositories.project.id.as_str()),
            _ => None,
        };
        let agent = entry
            .record
            .hire()
            .map(|hire| hire.agent)
            .or_else(|| entry.record.turn_issue().map(|issue| issue.agent));
        if project.is_none() {
            return history::prepare(
                lsn,
                &entry.record,
                RepositoryHistoryFacts {
                    hire: agent
                        .and_then(|id| self.agents.get(&id))
                        .map(|row| &row.repositories),
                    hire_class: agent.and_then(|id| self.repository_hires.get(&id)).copied(),
                    ..RepositoryHistoryFacts::default()
                },
            );
        }
        let trust: Vec<_> = self
            .repository_trust
            .values()
            .filter(|row| Some(row.project.as_str()) == project)
            .cloned()
            .collect();
        let leases: Vec<_> = self
            .worktrees
            .iter()
            .map(|(id, row)| RepositoryLease {
                worktree: *id,
                leased_lsn: row.leased_lsn,
                origin: &row.origin,
                path: &row.path,
                base: row.base,
                assigned: row.agent.is_some(),
                removed: row.removed.is_some(),
            })
            .collect();
        let active_hires: Vec<_> = self
            .agents
            .values()
            .filter(|row| !row.retired)
            .filter_map(|row| match &row.repositories {
                HireRepositories::Scoped(hire) => Some(hire),
                HireRepositories::Legacy(_) => None,
            })
            .collect();
        history::prepare(
            lsn,
            &entry.record,
            RepositoryHistoryFacts {
                project: project.and_then(|id| self.repository_projects.get(id)),
                trust: &trust,
                leases: &leases,
                active_hires: &active_hires,
                hire: agent
                    .and_then(|id| self.agents.get(&id))
                    .map(|row| &row.repositories),
                hire_class: agent.and_then(|id| self.repository_hires.get(&id)).copied(),
            },
        )
    }

    pub(super) fn apply_repository_change(&mut self, change: RepositoryHistoryChange) {
        match change {
            RepositoryHistoryChange::Hired { agent, class } => {
                self.repository_hires.insert(agent, class);
            }
            RepositoryHistoryChange::Project(row) => {
                self.repository_projects.insert(row.project.clone(), row);
            }
            RepositoryHistoryChange::Trust(row) => {
                self.repository_trust
                    .insert((row.project.clone(), row.repo_id.clone()), row);
            }
            RepositoryHistoryChange::Unchanged => {}
        }
    }
}
