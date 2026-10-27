//! Adapts observed repository history to the shared pure relationship validator.

use dudes_proto::repository::HireRepositories;
use dudes_proto::repository_state::history::{
    self, ProjectRepositoryHistory, RepositoryHireClass, RepositoryHistoryChange,
    RepositoryHistoryFacts, RepositoryLease, RepositoryTrust,
};
use dudes_proto::{Entry, Record};
use dudes_types::Lsn;
use redb::{ReadableTable, Table, TableDefinition};

use super::{AgentRow, RowTable, WorktreeRow, all_rows, get_row};
use crate::error::{Error, Result};

pub(super) const HIRES: TableDefinition<'_, &[u8; 16], &[u8]> =
    TableDefinition::new("repository_hires");
pub(super) const PROJECTS: TableDefinition<'_, &str, &[u8]> =
    TableDefinition::new("repository_projects");
pub(super) const TRUST: TableDefinition<'_, &[u8], &[u8]> =
    TableDefinition::new("repository_trust");

pub(super) fn rows<K: redb::Key + 'static, T: serde::de::DeserializeOwned>(
    table: &impl ReadableTable<K, &'static [u8]>,
) -> Result<Vec<T>> {
    table
        .iter()?
        .map(|item| {
            let (_, value) = item?;
            serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })
        })
        .collect()
}

pub(super) fn apply(
    projects: &mut Table<'_, &str, &[u8]>,
    trust: &mut Table<'_, &[u8], &[u8]>,
    hires: &mut RowTable<'_>,
    agents: &RowTable<'_>,
    leases: &RowTable<'_>,
    lsn: Lsn,
    entry: &Entry,
) -> Result<()> {
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
    if project.is_none() && entry.record.turn_issue().is_none() && entry.record.hire().is_none() {
        return Ok(());
    }
    let history: Option<ProjectRepositoryHistory> = project
        .map(|project| {
            projects
                .get(project)?
                .map(|row| {
                    serde_json::from_slice(row.value()).map_err(|source| Error::Row { source })
                })
                .transpose()
        })
        .transpose()?
        .flatten();
    let trust_rows: Vec<RepositoryTrust> = if project.is_some() {
        rows(trust)?
    } else {
        Vec::new()
    };
    let lease_rows: Vec<WorktreeRow> =
        if matches!(entry.record, Record::AgentHiredWithRepositories(_)) {
            all_rows(leases)?
        } else {
            Vec::new()
        };
    let leases: Vec<_> = lease_rows
        .iter()
        .map(|row| RepositoryLease {
            worktree: row.worktree,
            leased_lsn: row.leased_lsn,
            origin: &row.origin,
            path: &row.path,
            base: row.base,
            assigned: row.agent.is_some(),
            removed: row.removed.is_some(),
        })
        .collect();
    let active: Vec<AgentRow> =
        if matches!(entry.record, Record::ProjectRepositoriesRecorded { .. }) {
            all_rows(agents)?
        } else {
            Vec::new()
        };
    let active_hires: Vec<_> = active
        .iter()
        .filter_map(|row| match &row.repositories {
            HireRepositories::Scoped(hire) => Some(hire),
            HireRepositories::Legacy(_) => None,
        })
        .collect();
    let owner = agent
        .map(|id| get_row::<AgentRow>(agents, &id.to_bytes()))
        .transpose()?
        .flatten();
    let change = history::prepare(
        lsn,
        &entry.record,
        RepositoryHistoryFacts {
            project: history.as_ref(),
            trust: &trust_rows,
            leases: &leases,
            active_hires: &active_hires,
            hire: owner.as_ref().map(|row| &row.repositories),
            hire_class: agent
                .map(|id| get_row::<RepositoryHireClass>(hires, &id.to_bytes()))
                .transpose()?
                .flatten(),
        },
    )
    .map_err(|reason| Error::Repository { lsn, reason })?;
    match change {
        RepositoryHistoryChange::Hired { agent, class } => {
            super::put_row(hires, &agent.to_bytes(), &class)?;
        }
        RepositoryHistoryChange::Project(row) => {
            let bytes = serde_json::to_vec(&row).map_err(|source| Error::Row { source })?;
            projects.insert(row.project.as_str(), bytes.as_slice())?;
        }
        RepositoryHistoryChange::Trust(row) => {
            let key = serde_json::to_vec(&(&row.project, &row.repo_id))
                .map_err(|source| Error::Row { source })?;
            let bytes = serde_json::to_vec(&row).map_err(|source| Error::Row { source })?;
            trust.insert(key.as_slice(), bytes.as_slice())?;
        }
        RepositoryHistoryChange::Unchanged => {}
    }
    Ok(())
}

pub(super) fn export(
    lines: &mut Vec<String>,
    projects: &impl ReadableTable<&'static str, &'static [u8]>,
    trust: &impl ReadableTable<&'static [u8], &'static [u8]>,
) -> Result<()> {
    for item in projects.iter()? {
        let (key, value) = item?;
        let row: ProjectRepositoryHistory =
            serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
        let json = serde_json::to_string(&row).map_err(|source| Error::Row { source })?;
        lines.push(format!(
            "repository_projects {} {json}",
            super::hex(key.value().as_bytes())
        ));
    }
    for item in trust.iter()? {
        let (key, value) = item?;
        let row: RepositoryTrust =
            serde_json::from_slice(value.value()).map_err(|source| Error::Row { source })?;
        let json = serde_json::to_string(&row).map_err(|source| Error::Row { source })?;
        lines.push(format!(
            "repository_trust {} {json}",
            super::hex(key.value())
        ));
    }
    Ok(())
}
