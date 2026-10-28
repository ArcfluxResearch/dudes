//! The `worktrees` table: one row per leased worktree, kept after removal.
//!
//! The row holds the recorded facts. Whether a lease is pending, active, released or removed is
//! derived from them, and the supervisor's fold is what also knows an agent's recorded children.

use std::path::PathBuf;

use dudes_proto::{Entry, Record, WorktreeRemoval};
use dudes_types::{AgentId, GitCommit, Lsn, WorktreeId};
use serde::{Deserialize, Serialize};

use super::{RowTable, all_rows, get_row, put_row};
use crate::error::Result;

/// One lease as the journal records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRow {
    /// The lease.
    pub worktree: WorktreeId,
    /// Actual position of the durable lease before Git creates its copy.
    pub leased_lsn: Lsn,
    /// The repository the tree was added from.
    pub origin: PathBuf,
    /// The tree.
    pub path: PathBuf,
    /// The branch the tree was created on.
    pub branch: String,
    /// The commit the branch was created at.
    pub base: GitCommit,
    /// The lease this one was based on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<WorktreeId>,
    /// The agent hired into the tree. The first hire that names the lease owns it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
    /// `AgentRetired` was recorded for [`WorktreeRow::agent`].
    #[serde(default)]
    pub agent_retired: bool,
    /// How the lease ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed: Option<WorktreeRemoval>,
}

impl WorktreeRow {
    /// The state the Floor shows. A retirement is recorded for the owner, so this reads `Released`
    /// from that record alone; the supervisor's fold also waits for the agent's recorded children.
    #[must_use]
    pub fn view_state(&self) -> dudes_proto::WorktreeViewState {
        use dudes_proto::WorktreeViewState::{Active, Pending, Released, Removed};
        if self.removed.is_some() {
            Removed
        } else if self.agent.is_none() {
            Pending
        } else if self.agent_retired {
            Released
        } else {
            Active
        }
    }

    /// What the Floor may know of this tree: never its path.
    #[must_use]
    pub fn view(&self) -> dudes_proto::WorktreeView {
        dudes_proto::WorktreeView {
            branch: self.branch.clone(),
            state: self.view_state(),
            from: self.from,
        }
    }
}

pub(super) fn apply(table: &mut RowTable<'_>, lsn: Lsn, entry: &Entry) -> Result<()> {
    if let Some(hire) = entry.record.hire() {
        for worktree in hire.repositories.worktrees() {
            if let Some(mut row) = get_row::<WorktreeRow>(table, &worktree.to_bytes())?
                && row.agent.is_none()
            {
                row.agent = Some(hire.agent);
                put_row(table, &worktree.to_bytes(), &row)?;
            }
        }
    }
    match &entry.record {
        Record::WorktreeLeased {
            worktree,
            origin,
            path,
            branch,
            base,
            from,
        } => {
            if get_row::<WorktreeRow>(table, &worktree.to_bytes())?.is_none() {
                put_row(
                    table,
                    &worktree.to_bytes(),
                    &WorktreeRow {
                        worktree: *worktree,
                        leased_lsn: lsn,
                        origin: origin.clone(),
                        path: path.clone(),
                        branch: branch.clone(),
                        base: *base,
                        from: *from,
                        agent: None,
                        agent_retired: false,
                        removed: None,
                    },
                )?;
            }
        }
        Record::AgentRetired { id, .. } => {
            for mut row in all_rows::<WorktreeRow>(table)? {
                if row.agent == Some(*id) && !row.agent_retired {
                    row.agent_retired = true;
                    put_row(table, &row.worktree.to_bytes(), &row)?;
                }
            }
        }
        Record::WorktreeRemoved { worktree, how } => {
            if let Some(mut row) = get_row::<WorktreeRow>(table, &worktree.to_bytes())?
                && row.removed.is_none()
            {
                row.removed = Some(*how);
                put_row(table, &worktree.to_bytes(), &row)?;
            }
        }
        _ => {}
    }
    Ok(())
}
