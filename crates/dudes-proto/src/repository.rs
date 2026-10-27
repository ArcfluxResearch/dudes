//! Recorded repository authority, independent of current enrollment and filesystem state.

use std::path::{Path, PathBuf};

use dudes_types::{CardId, GoalId, Lsn, WorktreeId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::record::RecordedWorkspace;

/// The one repository an older hire recorded. Missing enrollment remains unknown.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SingletonRepository {
    cwd: PathBuf,
    workspace: Option<RecordedWorkspace>,
    worktree: Option<WorktreeId>,
}

impl SingletonRepository {
    pub(crate) fn recorded(
        cwd: &Path,
        workspace: Option<&RecordedWorkspace>,
        worktree: Option<WorktreeId>,
    ) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            workspace: workspace.cloned(),
            worktree,
        }
    }

    /// Recorded execution folder, without configuration lookup or origin fallback.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The immutable enrollment, when the hire recorded one.
    #[must_use]
    pub const fn workspace(&self) -> Option<&RecordedWorkspace> {
        self.workspace.as_ref()
    }

    /// The recorded own-copy lease, when present.
    #[must_use]
    pub const fn worktree(&self) -> Option<WorktreeId> {
        self.worktree
    }
}

/// The repository authority owned by an agent row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "repository", rename_all = "snake_case")]
pub enum HireRepositories {
    /// A historical singleton, interpreted solely from its hire record.
    Legacy(SingletonRepository),
}

impl Default for HireRepositories {
    fn default() -> Self {
        Self::Legacy(SingletonRepository::default())
    }
}

impl HireRepositories {
    /// Select an execution root only when the recorded authority is a singleton.
    /// Callers must refuse an unsupported authority rather than choosing a display root.
    #[must_use]
    #[expect(
        clippy::unnecessary_wraps,
        reason = "execution callers must handle unsupported repository authority explicitly"
    )]
    pub const fn singleton(&self) -> Option<&SingletonRepository> {
        match self {
            Self::Legacy(repository) => Some(repository),
        }
    }

    /// The primary folder for presentation. This does not select an execution grant.
    #[must_use]
    pub fn primary_cwd(&self) -> &Path {
        match self {
            Self::Legacy(repository) => repository.cwd(),
        }
    }

    /// The primary lease for presentation.
    #[must_use]
    pub const fn primary_worktree(&self) -> Option<WorktreeId> {
        match self {
            Self::Legacy(repository) => repository.worktree(),
        }
    }

    /// Recorded project identity for presentation and project membership queries.
    #[must_use]
    pub const fn workspace(&self) -> Option<&RecordedWorkspace> {
        match self {
            Self::Legacy(repository) => repository.workspace(),
        }
    }
}

/// A legacy ticket's singleton project declaration. Its wire representation stays a string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct TicketRepositories(String);

impl From<String> for TicketRepositories {
    fn from(project: String) -> Self {
        Self(project)
    }
}

impl From<&str> for TicketRepositories {
    fn from(project: &str) -> Self {
        Self::recorded(project)
    }
}

impl TicketRepositories {
    pub(crate) fn recorded(project: &str) -> Self {
        Self(project.to_owned())
    }

    /// The project identity the ticket recorded.
    #[must_use]
    pub fn project(&self) -> &str {
        &self.0
    }
}

/// The immutable ticket round selected at a start, retained by every issue in that round.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketRepositoryRound {
    /// The ticket.
    pub card: CardId,
    /// The goal installed atomically with this start.
    pub goal: GoalId,
    /// Actual start record position.
    pub start_lsn: Lsn,
    /// Recorded round number.
    pub round: u32,
    /// The declaration, absent only when the historical prefix has no ticket creation.
    pub repositories: Option<TicketRepositories>,
}

/// The frozen repository selection owned by an issued turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRepositories {
    hired_lsn: Lsn,
    hire: HireRepositories,
    ticket: Option<TicketRepositoryRound>,
}

impl TurnRepositories {
    pub(crate) fn recorded(
        hired_lsn: Lsn,
        hire: &HireRepositories,
        ticket: Option<&TicketRepositoryRound>,
    ) -> Self {
        Self {
            hired_lsn,
            hire: hire.clone(),
            ticket: ticket.cloned(),
        }
    }

    /// Immutable hire position selected by the issue.
    #[must_use]
    pub const fn hired_lsn(&self) -> Lsn {
        self.hired_lsn
    }

    /// Whether the current row still names the exact hire the issue selected.
    #[must_use]
    pub fn matches_hire(&self, hired_lsn: Lsn, hire: &HireRepositories) -> bool {
        self.hired_lsn == hired_lsn && self.hire == *hire
    }

    /// Select execution authority only when the frozen hire is a singleton.
    #[must_use]
    pub const fn singleton(&self) -> Option<&SingletonRepository> {
        self.hire.singleton()
    }

    /// The frozen ticket round, when this issue belongs to one.
    #[must_use]
    pub const fn ticket(&self) -> Option<&TicketRepositoryRound> {
        self.ticket.as_ref()
    }
}
