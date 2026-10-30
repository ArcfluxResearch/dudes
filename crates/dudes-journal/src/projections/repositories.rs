//! Adapts observed repository history to the shared pure relationship validator.

use dudes_proto::repository::HireRepositories;
use dudes_proto::repository_state::history::{
    self, ProjectRepositoryHistory, RepositoryHireClass, RepositoryHistoryChange,
    RepositoryHistoryFacts, RepositoryLease, RepositoryTicketFacts, RepositoryTrust,
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

#[derive(Clone, Copy)]
pub(super) struct Facts<'a, 'txn> {
    pub agents: &'a RowTable<'txn>,
    pub leases: &'a RowTable<'txn>,
    pub board: &'a RowTable<'txn>,
    pub goals: &'a RowTable<'txn>,
    pub turns: &'a RowTable<'txn>,
    pub completions: &'a RowTable<'txn>,
    pub instructions: &'a super::instructions::InstructionTable<'txn>,
    pub through: Lsn,
}

#[expect(
    clippy::too_many_lines,
    reason = "one adapter borrows the preceding owning rows and commits only validated history replacements"
)]
pub(super) fn apply(
    projects: &mut Table<'_, &str, &[u8]>,
    trust: &mut Table<'_, &[u8], &[u8]>,
    hires: &mut RowTable<'_>,
    facts: Facts<'_, '_>,
    lsn: Lsn,
    entry: &Entry,
) -> Result<Option<HireRepositories>> {
    let Facts {
        agents,
        leases,
        board,
        goals,
        turns,
        completions,
        instructions,
        through,
    } = facts;
    let ticket_id = entry
        .record
        .ticket_created()
        .map(|created| created.card)
        .or_else(|| entry.record.ticket_edited().map(|edited| edited.card))
        .or_else(|| entry.record.ticket_start().map(|start| start.card));
    let ticket = ticket_id
        .map(|card| get_row::<super::board::BoardRow>(board, &card.to_bytes()))
        .transpose()?
        .flatten();
    let agent = entry
        .record
        .hire()
        .map(|hire| hire.agent)
        .or_else(|| entry.record.turn_issue().map(|issue| issue.agent))
        .or_else(|| entry.record.ticket_start().map(|start| start.agent));
    let owner = agent
        .map(|id| get_row::<AgentRow>(agents, &id.to_bytes()))
        .transpose()?
        .flatten();
    let project = match &entry.record {
        Record::ProjectRepositoriesRecorded { project, .. }
        | Record::RepositoryTrustSet { project, .. } => Some(project.as_str()),
        Record::AgentHiredWithRepositories(hire)
        | Record::AgentHiredWithUnconfinedRepositories(hire) => {
            Some(hire.repositories.project.id.as_str())
        }
        _ => entry
            .record
            .ticket_created()
            .map(|created| created.project)
            .or_else(|| {
                ticket
                    .as_ref()
                    .and_then(|row| row.ticket.as_ref())
                    .map(|ticket| ticket.repositories.project())
            })
            .or_else(|| {
                owner
                    .as_ref()
                    .and_then(|owner| owner.repositories.workspace())
                    .map(|project| project.id.as_str())
            }),
    };
    if project.is_none()
        && entry.record.turn_issue().is_none()
        && entry.record.hire().is_none()
        && entry.record.ticket_start().is_none()
        && entry.record.ticket_edited().is_none()
    {
        return Ok(None);
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
    let trust_rows: Vec<RepositoryTrust> = if entry.record.repository_hire().is_some()
        || entry.record.ticket_start().is_some()
        || matches!(entry.record, Record::TurnIssuedWithRepositories(_))
    {
        rows(trust)?
    } else {
        Vec::new()
    };
    let lease_rows: Vec<WorktreeRow> = if entry.record.repository_hire().is_some()
        || entry.record.ticket_start().is_some()
        || matches!(entry.record, Record::TurnIssuedWithRepositories(_))
    {
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
            agent: row.agent,
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
    let current_tickets: Vec<super::board::BoardRow> = if matches!(
        entry.record,
        Record::ProjectRepositoriesRecorded { .. } | Record::TurnIssuedWithRepositories(_)
    ) {
        all_rows(board)?
    } else {
        Vec::new()
    };
    let active_tickets: Vec<_> = current_tickets
        .iter()
        .filter(|row| row.view.status != dudes_types::CardStatus::Done)
        .filter_map(|row| {
            row.ticket
                .as_ref()?
                .repositories
                .declared()
                .map(|scope| &scope.declaration)
        })
        .collect();
    let mut issue_goals = Vec::new();
    let mut issue_cards = Vec::new();
    if matches!(entry.record, Record::TurnIssuedWithRepositories(_)) {
        for row in current_tickets
            .iter()
            .filter(|row| row.view.claimed_by == agent)
        {
            if let Some(goal) = row.ticket.as_ref().and_then(|ticket| ticket.goal)
                && let Some(goal) = get_row::<super::GoalRow>(goals, &goal.to_bytes())?
            {
                let completed = get_row::<Lsn>(completions, &goal.id.to_bytes())?.is_some();
                issue_goals.push((goal, completed));
                issue_cards.push(row.view.id);
            }
        }
    }
    let instruction_cache = if matches!(entry.record, Record::TurnIssuedWithRepositories(_)) {
        Some(super::instructions::load(
            instructions,
            through,
            entry,
            &issue_cards,
        )?)
    } else {
        None
    };
    let active_round = if let Some(agent) = agent {
        dudes_proto::repository_state::active_round(
            agent,
            current_tickets.iter().filter_map(|row| {
                let ticket = row.ticket.as_ref()?;
                let (goal, completed) = issue_goals
                    .iter()
                    .find(|(goal, _)| Some(goal.id) == ticket.goal)?;
                Some(dudes_proto::repository_state::ActiveRepositoryRound {
                    holder: row.view.claimed_by,
                    status: row.view.status,
                    round: goal.repository_round.as_ref()?,
                    unfinished: owner
                        .as_ref()
                        .is_some_and(|owner| owner.goal == Some(goal.id))
                        && goal.status != dudes_types::GoalStatus::Abandoned
                        && !completed,
                    instructions_pending: instruction_cache
                        .as_ref()
                        .is_some_and(|cache| cache.state().pending_for_goal(goal.id)),
                })
            }),
        )
        .map_err(|reason| Error::Repository { lsn, reason })?
    } else {
        None
    };
    let change = history::prepare(
        lsn,
        &entry.record,
        RepositoryHistoryFacts {
            project: history.as_ref(),
            trust: &trust_rows,
            leases: &leases,
            active_hires: &active_hires,
            hire: owner.as_ref().map(|row| &row.repositories),
            hired_lsn: owner.as_ref().map(|row| row.hired_lsn),
            ticket: ticket.as_ref().and_then(|row| {
                row.ticket.as_ref().map(|ticket| RepositoryTicketFacts {
                    repositories: &ticket.repositories,
                    check: &ticket.done_when,
                    status: row.view.status,
                    holder: row.view.claimed_by,
                    seq: row.view.seq,
                    round: ticket.round,
                    queued: ticket.queue.is_some(),
                })
            }),
            active_tickets: &active_tickets,
            card_exists: ticket.is_some(),
            turn_exists: entry
                .record
                .turn_issue()
                .filter(|issue| issue.repositories.is_some())
                .map(|issue| get_row::<super::TurnRow>(turns, &issue.turn.to_bytes()))
                .transpose()?
                .flatten()
                .is_some(),
            active_round,
            goal_exists: entry
                .record
                .ticket_start()
                .map(|start| get_row::<super::GoalRow>(goals, &start.goal.id.to_bytes()))
                .transpose()?
                .flatten()
                .is_some(),
            hire_class: agent
                .map(|id| get_row::<RepositoryHireClass>(hires, &id.to_bytes()))
                .transpose()?
                .flatten(),
        },
    )
    .map_err(|reason| Error::Repository { lsn, reason })?;
    match change {
        RepositoryHistoryChange::Hired {
            agent,
            class,
            repositories,
        } => {
            super::put_row(hires, &agent.to_bytes(), &class)?;
            return Ok(Some(repositories));
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
    Ok(None)
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
