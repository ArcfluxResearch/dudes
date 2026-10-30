//! The bounded, display-only read of an agent’s recorded repositories for the authenticated Floor.
//!
//! A [`GitView`] is a live read of a directory the agent writes. It is never journaled, never in a
//! projection and never evidence: nothing in it can cause `Done` or `Stop`. Its fields are a closed
//! set, and the daemon fills each section within a serialized-byte budget so the whole reply stays
//! under [`MAX_GIT_VIEW_BYTES`].

use dudes_types::Timestamp;
use serde::{Deserialize, Serialize};

/// The serialized cap on one whole reply. The section budgets below sum to 63 KiB.
pub const MAX_GIT_VIEW_BYTES: usize = 64 * 1024;
/// Serialized bytes of the head section, envelope included.
pub const MAX_GIT_HEAD_BYTES: usize = 1024;
/// Serialized bytes of the local branch section.
pub const MAX_GIT_BRANCH_BYTES: usize = 4 * 1024;
/// Serialized bytes of the commit section.
pub const MAX_GIT_COMMIT_BYTES: usize = 10 * 1024;
/// Serialized bytes of the committed-files section.
pub const MAX_GIT_COMMITTED_BYTES: usize = 24 * 1024;
/// Serialized bytes of the working-changes section.
pub const MAX_GIT_CHANGE_BYTES: usize = 24 * 1024;
/// Most local branches shown.
pub const MAX_GIT_BRANCHES: usize = 30;
/// Most commits shown.
pub const MAX_GIT_COMMITS: usize = 20;
/// Most parents shown for one commit.
pub const MAX_GIT_PARENTS: usize = 4;
/// Most bytes of a branch name.
pub const MAX_GIT_NAME_BYTES: usize = 100;
/// Most bytes of a commit subject.
pub const MAX_GIT_SUBJECT_BYTES: usize = 120;
/// Most bytes of a repository-relative path.
pub const MAX_GIT_PATH_BYTES: usize = 200;
/// The width of a shown commit id, in lowercase hexadecimal digits.
pub const GIT_ID_DIGITS: usize = 12;

/// How the read ended. Everything but `Ok` is a finite refusal that carries no repository data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitState {
    /// The repository was read.
    Ok,
    /// No current agent has this id.
    AgentUnknown,
    /// The selector does not name an admitted recorded singleton or repository set.
    InvalidSelection,
    /// Earlier members spent the aggregate response allowance. This member was not read.
    ResponseLimit,
    /// The agent's working directory has no `.git` directory at its root.
    NotACheckoutRoot,
    /// The agent's `.git` is a file and its hire names no worktree Dudes created, so this read
    /// does not follow it.
    GitfileUnsupported,
    /// The repository uses storage this read does not support.
    UnsupportedStorage,
    /// This daemon cannot run the read on its platform.
    UnsupportedPlatform,
    /// No usable `git` on the daemon's `PATH`.
    GitUnavailable,
    /// Another read is running, or this session read a moment ago.
    Busy,
    /// The read did not finish within its deadline.
    TimedOut,
    /// The repository could not be read as a repository.
    Unreadable,
    /// The agent works in a worktree Dudes created, and its link files or directories are not the
    /// ones its lease names, so nothing was read.
    WorktreeMismatch,
    /// The agent retired and its worktree is still on disk. A released worktree is not read.
    WorktreeReleased,
    /// The agent retired and its worktree was removed.
    WorktreeRemoved,
}

/// The commit a read measures `ahead`, `behind` and the listed commits against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitAgainst {
    /// The remote default branch, for a checkout Dudes did not create.
    RemoteDefaultBranch,
    /// The commit the agent's worktree was created at, as its lease records it.
    WorktreeStart,
}

/// Where the agent's checkout stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHead {
    /// The checked-out branch, or `None` when `HEAD` is detached.
    pub branch: Option<String>,
    /// The first [`GIT_ID_DIGITS`] digits of the checked-out commit, or `None` on an unborn branch.
    pub id: Option<String>,
    /// Commits on `HEAD` that the comparison commit lacks, or `None` when that commit is not in
    /// the repository.
    pub ahead: Option<u32>,
    /// Commits the comparison commit has that `HEAD` lacks, or `None` when that commit is not in
    /// the repository.
    pub behind: Option<u32>,
    /// Which commit `ahead`, `behind` and the commit list compare with.
    pub against: GitAgainst,
    /// How many untracked files `git status` listed. Names are never sent.
    pub untracked: u32,
    /// Whether the status output was cut, so the true count is at least `untracked`.
    pub untracked_cut: bool,
}

/// One local branch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitBranch {
    /// The branch name, cut at [`MAX_GIT_NAME_BYTES`] with a trailing ellipsis.
    pub name: String,
    /// The first [`GIT_ID_DIGITS`] digits of its tip.
    pub tip: String,
    /// Whether `HEAD` is on it.
    pub current: bool,
}

/// One commit the comparison commit cannot reach.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitCommit {
    /// The first [`GIT_ID_DIGITS`] digits of its id.
    pub id: String,
    /// Its parents, at most [`MAX_GIT_PARENTS`], in the same short form.
    pub parents: Vec<String>,
    /// The committer time.
    pub at: Timestamp,
    /// The subject, cut at [`MAX_GIT_SUBJECT_BYTES`] with a trailing ellipsis. No author is sent.
    pub subject: String,
}

/// What happened to one tracked path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitChangeKind {
    /// Added.
    Added,
    /// Modified.
    Modified,
    /// Deleted.
    Deleted,
    /// Renamed or copied; only the new path is sent.
    Renamed,
    /// The file type changed, for example to a symbolic link.
    TypeChanged,
    /// An unresolved merge conflict.
    Conflicted,
}

/// One tracked path the agent committed since the comparison commit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitCommittedFile {
    /// Repository-relative, never absolute and never containing `..`.
    pub path: String,
    /// Whether `path` was cut at [`MAX_GIT_PATH_BYTES`], so it is no longer the real path.
    pub cut: bool,
    /// What happened to it.
    pub kind: GitChangeKind,
}

/// One tracked path that differs from `HEAD` in the index or the working tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitWorkingChange {
    /// Repository-relative, never absolute and never containing `..`.
    pub path: String,
    /// Whether `path` was cut at [`MAX_GIT_PATH_BYTES`], so it is no longer the real path.
    pub cut: bool,
    /// What happened to it.
    pub kind: GitChangeKind,
    /// Whether the change is in the index.
    pub staged: bool,
}

/// One list of entries, filled until its serialized-byte budget is spent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSection<T> {
    /// The entries kept.
    pub items: Vec<T>,
    /// Whether entries were left out for the entry or byte cap.
    pub truncated: bool,
    /// How many entries were left out because a string in them held a control character, a
    /// format character or a line separator.
    pub dropped: u32,
}

impl<T> Default for GitSection<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            truncated: false,
            dropped: 0,
        }
    }
}

/// The reply to `POST /floor/git`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitView {
    /// Recorded repository groups for an explicit aggregate request. Empty on a singleton read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<GitRepositoryView>,
    /// Opaque identity of the complete recorded read selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_id: Option<String>,
    /// How the read ended. Every list below is empty unless this is `ok`.
    pub state: GitState,
    /// When the daemon read the repository.
    pub read_at: Timestamp,
    /// Where the checkout stands. `None` unless `state` is `ok`.
    pub head: Option<GitHead>,
    /// Local branches, the current one first.
    pub branches: GitSection<GitBranch>,
    /// The agent's commits the comparison commit cannot reach, newest first.
    pub commits: GitSection<GitCommit>,
    /// Tracked paths those commits changed.
    pub committed: GitSection<GitCommittedFile>,
    /// Tracked paths with uncommitted changes.
    pub changes: GitSection<GitWorkingChange>,
}

/// One bounded member of a recorded hire or ticket selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitRepositoryView {
    /// Stable recorded repository identity.
    pub repo_id: crate::repository::RepoId,
    /// Finite metadata result for this member. Nested groups are never produced.
    pub view: Box<GitView>,
}

impl GitView {
    /// A refusal: `state` and the time, and nothing read.
    #[must_use]
    pub fn refused(state: GitState, read_at: Timestamp) -> Self {
        Self {
            repositories: Vec::new(),
            binding_id: None,
            state,
            read_at,
            head: None,
            branches: GitSection::default(),
            commits: GitSection::default(),
            committed: GitSection::default(),
            changes: GitSection::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_section_budgets_leave_room_under_the_reply_cap() {
        let sum = MAX_GIT_HEAD_BYTES
            + MAX_GIT_BRANCH_BYTES
            + MAX_GIT_COMMIT_BYTES
            + MAX_GIT_COMMITTED_BYTES
            + MAX_GIT_CHANGE_BYTES;
        assert_eq!(sum, 63 * 1024);
        assert!(sum < MAX_GIT_VIEW_BYTES);
    }

    #[test]
    fn a_refusal_serializes_to_one_literal() {
        let view = GitView::refused(GitState::GitfileUnsupported, Timestamp::from_unix_millis(7));
        assert_eq!(
            serde_json::to_string(&view).unwrap(),
            r#"{"state":"gitfile_unsupported","read_at":7,"head":null,"branches":{"items":[],"truncated":false,"dropped":0},"commits":{"items":[],"truncated":false,"dropped":0},"committed":{"items":[],"truncated":false,"dropped":0},"changes":{"items":[],"truncated":false,"dropped":0}}"#
        );
    }

    #[test]
    fn a_head_names_the_commit_it_compares_with() {
        let head = GitHead {
            branch: Some("dudes/0123456789ab".into()),
            id: Some("aaaaaaaaaaaa".into()),
            ahead: None,
            behind: None,
            against: GitAgainst::WorktreeStart,
            untracked: 0,
            untracked_cut: false,
        };
        let wire = r#"{"branch":"dudes/0123456789ab","id":"aaaaaaaaaaaa","ahead":null,"behind":null,"against":"worktree_start","untracked":0,"untracked_cut":false}"#;
        assert_eq!(serde_json::to_string(&head).unwrap(), wire);
        assert_eq!(
            serde_json::to_string(&GitAgainst::RemoteDefaultBranch).unwrap(),
            r#""remote_default_branch""#
        );
        let missing = wire.replace(r#""against":"worktree_start","#, "");
        assert!(serde_json::from_str::<GitHead>(&missing).is_err());
    }

    #[test]
    fn the_worktree_states_serialize_to_their_names() {
        for (state, name) in [
            (GitState::WorktreeMismatch, "\"worktree_mismatch\""),
            (GitState::WorktreeReleased, "\"worktree_released\""),
            (GitState::WorktreeRemoved, "\"worktree_removed\""),
        ] {
            assert_eq!(serde_json::to_string(&state).unwrap(), name);
        }
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let wire = r#"{"state":"ok","read_at":1,"head":null,"extra":1,"branches":{"items":[],"truncated":false,"dropped":0},"commits":{"items":[],"truncated":false,"dropped":0},"committed":{"items":[],"truncated":false,"dropped":0},"changes":{"items":[],"truncated":false,"dropped":0}}"#;
        assert!(serde_json::from_str::<GitView>(wire).is_err());
    }
}
