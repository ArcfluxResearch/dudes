use std::collections::BTreeSet;

use anyhow::{Context as _, ensure};
use dudes_proto::repository::{
    HireRepositories, RepoId, RepositoryEntry, RepositorySet, TicketDeclaration,
};
use dudes_proto::repository_state::history::{RepositoryLease, validate_execution_hire};
use dudes_proto::{Record, TicketRow};
use dudes_supervisor::World;
use dudes_types::AgentId;

use super::super::repository_admission::CurrentHireAdmission;

#[cfg(all(test, unix))]
mod tests;

pub(super) fn declaration_of(
    admission: &CurrentHireAdmission<'_>,
    world: &World,
    profile: &crate::floor_actions::Profile,
    request: &dudes_proto::rpc::ticket::TicketCreateParams,
    check: &crate::floor_actions::ResolvedProjectCheck,
) -> Result<Option<TicketDeclaration>, dudes_proto::rpc::RpcError> {
    if profile.repositories.is_none() {
        if request.repos.is_some() {
            return Err(super::invalid(
                "repository selection requires an explicitly configured project",
            ));
        }
        return Ok(None);
    }
    if request.priority.is_some() {
        return Err(super::invalid(
            "scoped ticket creation does not support priority yet",
        ));
    }
    let declaration = admission
        .ticket_declaration(world, &profile.id, request.repos.as_deref())
        .map_err(|error| super::error_refusal(&error))?;
    if check
        .repo()
        .is_none_or(|repo| declaration.repositories.get(repo).is_none())
    {
        return Err(super::invalid(
            "the configured check repository is outside the declaration",
        ));
    }
    Ok(Some(declaration))
}

impl CurrentHireAdmission<'_> {
    pub(in crate::runtime) fn require_ticket_append(
        &self,
        world: &World,
        record: &Record,
        ticket: Option<&TicketRow>,
    ) -> anyhow::Result<()> {
        match record {
            Record::TicketCreatedWithRepositories(created) => {
                self.current_ticket_declaration(world, &created.declaration)
            }
            Record::TicketEditedWithRepositories(_) => {
                let scope = ticket
                    .and_then(|ticket| ticket.repositories.declared())
                    .context("the immutable ticket declaration is unavailable")?;
                self.current_ticket_declaration(world, &scope.declaration)
            }
            Record::TicketAssigned { agent, .. } => {
                let ticket = ticket.context("the ticket projection is unavailable")?;
                if let Some(scope) = ticket.repositories.declared() {
                    self.current_ticket_hire(world, *agent, &scope.declaration)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub(in crate::runtime) fn ticket_declaration(
        &self,
        world: &World,
        project: &str,
        selected: Option<&[RepoId]>,
    ) -> anyhow::Result<TicketDeclaration> {
        self.require_repository_capability()?;
        let (_, observed) = self.current_project_repositories(world, project)?;
        let wanted = selected.map(|ids| ids.iter().collect::<BTreeSet<_>>());
        if let Some(ids) = selected {
            ensure!(
                !ids.is_empty() && wanted.as_ref().is_some_and(|set| set.len() == ids.len()),
                "repository selection must be nonempty and unique"
            );
            ensure!(
                ids.iter().all(|id| observed.repositories.get(id).is_some()),
                "repository selection names an unknown member"
            );
        }
        let entries: Vec<_> = observed
            .repositories
            .entries()
            .iter()
            .filter(|entry| {
                wanted
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&entry.repo_id))
            })
            .map(|entry| RepositoryEntry {
                repo_id: entry.repo_id.clone(),
                value: entry.value.origin.clone(),
            })
            .collect();
        let primary = entries
            .first()
            .context("repository selection is empty")?
            .repo_id
            .clone();
        Ok(TicketDeclaration {
            project: project.to_owned(),
            catalog_lsn: observed
                .lsn
                .context("repository enrollment is not available yet")?,
            repositories: RepositorySet::new(primary, entries)?,
        })
    }

    pub(in crate::runtime) fn current_ticket_declaration(
        &self,
        world: &World,
        declaration: &TicketDeclaration,
    ) -> anyhow::Result<()> {
        self.require_repository_capability()?;
        let (_, current) = self.current_project_repositories(world, &declaration.project)?;
        ensure!(
            declaration.repositories.entries().iter().all(|entry| {
                current
                    .repositories
                    .get(&entry.repo_id)
                    .is_some_and(|repo| repo.origin == entry.value)
            }),
            "the declared repository authority is no longer current"
        );
        Ok(())
    }

    pub(in crate::runtime) fn current_ticket_hire(
        &self,
        world: &World,
        agent: AgentId,
        declaration: &TicketDeclaration,
    ) -> anyhow::Result<()> {
        self.current_ticket_declaration(world, declaration)?;
        let (_, current) = self.current_project_repositories(world, &declaration.project)?;
        let row = world
            .agents
            .get(&agent)
            .filter(|row| !row.retired)
            .context("the ticket agent is unavailable")?;
        let HireRepositories::Scoped(recorded) = &row.repositories else {
            anyhow::bail!("the ticket requires a scoped hire");
        };
        ensure!(
            recorded.project.id == declaration.project,
            "the agent belongs to another project"
        );
        let trust: Vec<_> = world
            .repository_trust
            .values()
            .filter(|grant| grant.project == declaration.project)
            .cloned()
            .collect();
        let leases: Vec<_> = world
            .worktrees
            .iter()
            .map(|(id, lease)| RepositoryLease {
                worktree: *id,
                leased_lsn: lease.leased_lsn,
                origin: &lease.origin,
                path: &lease.path,
                base: lease.base,
                agent: lease.agent,
                removed: lease.removed.is_some(),
            })
            .collect();
        validate_execution_hire(
            agent,
            row.mcp.hire_lsn,
            recorded,
            &current.repositories,
            &trust,
            &leases,
        )?;
        ensure!(
            recorded
                .repositories
                .entries()
                .iter()
                .all(|entry| crate::worktree::is_owned_repository(&entry.value.recorded)),
            "a hired repository copy is unavailable"
        );
        ensure!(
            declaration
                .repositories
                .entries()
                .iter()
                .all(|entry| recorded
                    .repositories
                    .get(&entry.repo_id)
                    .is_some_and(|repo| repo.recorded.origin == entry.value)),
            "the hire does not cover the immutable declaration"
        );
        Ok(())
    }
}
