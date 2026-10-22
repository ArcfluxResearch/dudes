//! The lease table: every worktree the journal has leased, and what became of it.
//!
//! The fold reads recorded values only. It stats nothing and runs no `git`: whether a directory is
//! at, under or above a lease's path is a question about the filesystem, which `bins/duded`
//! answers at admission and again at the append.

use std::path::PathBuf;

use dudes_proto::record::{TrustPin, TrustedRepository};
use dudes_proto::{Record, WorktreeRemoval};
use dudes_types::{AgentId, GitCommit, Lsn, WorktreeId};

use super::World;

/// One lease, as the confirmed entries describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeRow {
    /// The repository the tree was added from.
    pub origin: PathBuf,
    /// The tree.
    pub path: PathBuf,
    /// The branch the tree was created on.
    pub branch: String,
    /// The commit the branch was created at.
    pub base: GitCommit,
    /// The lease this one was based on.
    pub from: Option<WorktreeId>,
    /// The agent hired into the tree. The first hire that names the lease is its owner.
    pub agent: Option<AgentId>,
    /// How the lease ended, once a `WorktreeRemoved` is recorded.
    pub removed: Option<WorktreeRemoval>,
}

/// What the operator vouched for when they trusted a project, and where in the journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustGrant {
    /// The record that granted it.
    pub lsn: Lsn,
    /// The repository vouched for.
    pub repository: TrustedRepository,
    /// The settings vouched for.
    pub pin: TrustPin,
}

/// Where a lease is in its life. Derived from the fold, never stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorktreeState {
    /// Leased, and no agent hired into it yet.
    Pending,
    /// Its agent is not retired, or is retired and still has a recorded child.
    Active,
    /// Its agent is retired and has no recorded child. Nothing has removed the tree.
    Released,
    /// A `WorktreeRemoved` is recorded.
    Removed,
}

impl World {
    pub(super) fn fold_worktree(&mut self, record: &Record) {
        match record {
            Record::WorktreeLeased {
                worktree,
                origin,
                path,
                branch,
                base,
                from,
            } => {
                self.worktrees
                    .entry(*worktree)
                    .or_insert_with(|| WorktreeRow {
                        origin: origin.clone(),
                        path: path.clone(),
                        branch: branch.clone(),
                        base: *base,
                        from: *from,
                        agent: None,
                        removed: None,
                    });
            }
            Record::WorktreeRemoved { worktree, how } => {
                if let Some(row) = self.worktrees.get_mut(worktree) {
                    row.removed.get_or_insert(*how);
                }
            }
            _ => {}
        }
    }

    pub(super) fn fold_workspace_trust(&mut self, record: &Record, lsn: Lsn) {
        let Record::WorkspaceTrustSet {
            workspace_id,
            trusted,
            repository,
            pin,
        } = record
        else {
            return;
        };
        match (trusted, repository, pin) {
            (true, Some(repository), Some(pin)) => {
                self.workspace_trust.insert(
                    workspace_id.clone(),
                    TrustGrant {
                        lsn,
                        repository: repository.clone(),
                        pin: pin.clone(),
                    },
                );
            }
            _ => {
                self.workspace_trust.remove(workspace_id);
            }
        }
    }

    /// Name `agent` as the owner of the lease its hire records, unless another hire has it.
    pub(super) fn assign_worktree(&mut self, worktree: WorktreeId, agent: AgentId) {
        if let Some(row) = self.worktrees.get_mut(&worktree) {
            row.agent.get_or_insert(agent);
        }
    }

    /// The lease's state, or `None` for a lease no record has created.
    #[must_use]
    pub fn worktree_state(&self, worktree: WorktreeId) -> Option<WorktreeState> {
        let row = self.worktrees.get(&worktree)?;
        Some(if row.removed.is_some() {
            WorktreeState::Removed
        } else if let Some(agent) = row.agent {
            let retired = self.agents.get(&agent).is_none_or(|agent| agent.retired);
            if retired && !self.recorded_children().any(|(owner, _)| owner == agent) {
                WorktreeState::Released
            } else {
                WorktreeState::Active
            }
        } else {
            WorktreeState::Pending
        })
    }

    /// Every lease that is not removed, in id order: the trees a plain hire may not overlap. A
    /// released tree is still on disk, so "released" means only that no agent works there, and the
    /// operator can never be offered its removal under a live agent.
    pub fn unremoved_worktrees(&self) -> impl Iterator<Item = (WorktreeId, &WorktreeRow)> + '_ {
        self.worktrees
            .iter()
            .filter(|(_, row)| row.removed.is_none())
            .map(|(id, row)| (*id, row))
    }

    /// Every lease that is pending or active, in id order.
    pub fn live_worktrees(&self) -> impl Iterator<Item = (WorktreeId, &WorktreeRow)> + '_ {
        self.worktrees.iter().filter_map(|(id, row)| {
            matches!(
                self.worktree_state(*id),
                Some(WorktreeState::Pending | WorktreeState::Active)
            )
            .then_some((*id, row))
        })
    }
}
