use dudes_proto::{Entry, MAX_RETIRED_AGENT_BYTES, MAX_RETIRED_AGENTS, Record, RetiredAgentView};
use dudes_types::AgentId;
use redb::{ReadableTable, Table};

use super::worktrees::WorktreeRow;
use super::{AgentRow, RowTable, budget, get_row, turns};
use crate::error::{Error, Result};

const WINDOW: &str = "window";
type ArchiveTable<'txn> = Table<'txn, &'static str, &'static [u8]>;

pub(super) fn read(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<Vec<RetiredAgentView>> {
    table
        .get(WINDOW)?
        .map(|value| serde_json::from_slice(value.value()).map_err(|source| Error::Row { source }))
        .transpose()
        .map(Option::unwrap_or_default)
}

pub(super) fn capture(
    table: &mut ArchiveTable<'_>,
    agents: &RowTable<'_>,
    by_day: &budget::DayTable<'_>,
    turns: &RowTable<'_>,
    leases: &RowTable<'_>,
    entry: &Entry,
) -> Result<()> {
    let Record::AgentRetired { id, .. } = entry.record else {
        return Ok(());
    };
    let Some(agent) = get_row::<AgentRow>(agents, &id.to_bytes())? else {
        return Ok(());
    };
    let mut rows = read(table)?;
    if rows.iter().any(|row| row.id == id) {
        return Ok(());
    }
    // Stored at retirement so the window's size bound counts it; `released` is the state a retired
    // agent's tree is in until the operator removes it, and the read below corrects that one case.
    let worktree = match agent.repositories.primary_worktree() {
        Some(lease) => get_row::<WorktreeRow>(leases, &lease.to_bytes())?.map(|lease| {
            let mut view = lease.view();
            view.state = dudes_proto::WorktreeViewState::Released;
            view
        }),
        None => None,
    };
    let row = RetiredAgentView {
        worktree,
        id,
        name: agent.name,
        role: agent.role,
        character: agent.character,
        accent: agent.accent,
        provider: agent.provider,
        workspace_label: agent
            .repositories
            .workspace()
            .map(|workspace| workspace.label.clone()),
        hired_at: agent.hired_at,
        retired_at: entry.at,
        turns: turns::count_for_agent(turns, id)?,
        spend: budget::agent_total(by_day, id)?,
    };
    if serialize(std::slice::from_ref(&row))?.len() > MAX_RETIRED_AGENT_BYTES {
        return Ok(());
    }
    rows.push(row);
    write_bounded(table, rows)
}

pub(super) fn refresh_totals(
    table: &mut ArchiveTable<'_>,
    by_day: &budget::DayTable<'_>,
    turns: &RowTable<'_>,
    id: AgentId,
) -> Result<()> {
    let mut rows = read(table)?;
    let Some(row) = rows.iter_mut().find(|row| row.id == id) else {
        return Ok(());
    };
    row.spend = budget::agent_total(by_day, id)?;
    row.turns = turns::count_for_agent(turns, id)?;
    write_bounded(table, rows)
}

fn serialize(rows: &[RetiredAgentView]) -> Result<Vec<u8>> {
    serde_json::to_vec(rows).map_err(|source| Error::Row { source })
}

fn write_bounded(table: &mut ArchiveTable<'_>, mut rows: Vec<RetiredAgentView>) -> Result<()> {
    let mut bytes = serialize(&rows)?;
    while rows.len() > MAX_RETIRED_AGENTS || bytes.len() > MAX_RETIRED_AGENT_BYTES {
        rows.remove(0);
        bytes = serialize(&rows)?;
    }
    table.insert(WINDOW, bytes.as_slice())?;
    Ok(())
}
