//! The worktree hire, run on the caller's connection thread as a sequence of synchronous asks to
//! the loop, with the git children in between.
//!
//! The loop thread never waits on git. It answers each step and goes back to its other jobs. Every
//! step that decides something is a loop ask, so each decision reads the world as it is then:
//!
//! 1. admission, before any git child;
//! 2. git resolves the repository and the commit to start from;
//! 3. the lease is journaled, which is what a concurrent hire's overlap check sees;
//! 4. `git worktree add`;
//! 5. admission again, then the hire into the new tree.
//!
//! A failure after step 3 asks the loop whether the lease is still pending, and only a pending
//! lease is cleaned up: a tree an agent was hired into is never touched.

use std::path::PathBuf;
use std::sync::Arc;

use dudes_proto::rpc::RpcError;
use dudes_proto::rpc::agent::{HireParams, HireResult};
use dudes_types::{GitCommit, Lsn, WorktreeId};

use super::git::{Git, GitError};
use super::trust::{TrustRefusal, Vouch};
use crate::ports::{Answered, Ask, Handle};

/// What the loop says when a project's trust grant is no longer the one a hire was admitted under.
pub(crate) const TRUST_CHANGED: &str = "the project's trust changed while the copy was being made";

/// The operator's grant a hire into a copy rests on, as the connection thread verified it.
#[derive(Clone, Debug)]
pub struct TrustTicket {
    /// The enrolled project.
    pub workspace_id: String,
    /// The record that granted it. The loop checks it is still the latest at each append.
    pub lsn: Lsn,
    /// What the grant vouched for.
    pub vouch: Vouch,
}

/// One exact current enrollment member, frozen before its settings inspection.
#[derive(Clone, Debug)]
pub struct RepositoryTrustTarget {
    /// Current project identity.
    pub project: String,
    /// Stable member identity.
    pub repo_id: dudes_proto::repository::RepoId,
    /// Actual validated current observation.
    pub catalog_lsn: Lsn,
    /// Complete current origin binding.
    pub origin: dudes_proto::record::TrustedRepository,
}

/// Why a copy was not made, beside the refusal that says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// The folder is in no git repository.
    NotARepository,
    /// The trust grant is gone or no longer matches the repository.
    TrustChanged,
    /// Anything else.
    Other,
}

/// A refused or failed copy.
#[derive(Debug)]
pub struct Failure {
    /// The reason, for the caller that words it for a person.
    pub why: Why,
    /// The refusal the operator's command line shows.
    pub error: RpcError,
}

impl Failure {
    fn other(error: RpcError) -> Self {
        let why = if error.message.ends_with(TRUST_CHANGED) {
            Why::TrustChanged
        } else {
            Why::Other
        };
        Self { why, error }
    }
}

/// A lease as the loop answers about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseView {
    /// The lease.
    pub worktree: WorktreeId,
    /// The repository the tree is added from.
    pub origin: PathBuf,
    /// The tree.
    pub path: PathBuf,
    /// The branch the tree is created on.
    pub branch: String,
    /// The commit the branch starts at.
    pub base: GitCommit,
    /// Durable cleanup authority, absent for legacy leases.
    pub identity: Option<dudes_proto::record::WorktreeDirectoryAuthority>,
}

/// What the connection thread needs to hire into a worktree.
#[derive(Clone, Debug)]
pub struct WorktreeHires {
    handle: Handle,
    git: Git,
}

fn from_git(error: &GitError) -> RpcError {
    super::unavailable(error.to_string())
}

fn unexpected(answer: &Answered) -> RpcError {
    super::unavailable(format!("the loop answered {answer:?} to a worktree step"))
}

impl WorktreeHires {
    /// A worktree hire path over `handle` and `git`.
    #[must_use]
    pub const fn new(handle: Handle, git: Git) -> Self {
        Self { handle, git }
    }

    /// The daemon's hardened git.
    #[cfg_attr(not(unix), allow(dead_code, reason = "only a Unix creation reads it"))]
    pub(crate) const fn git(&self) -> &Git {
        &self.git
    }

    /// Hire an agent into a new worktree of the repository `params.cwd` is in.
    ///
    /// # Errors
    ///
    /// The refusal of whichever step said no. Proved pending copies are cleaned up.
    /// Uncertain directories and their leases are retained for the operator.
    pub fn hire(&self, params: HireParams) -> Result<HireResult, RpcError> {
        let lease = self
            .prepare(&params, None)
            .map_err(|failure| failure.error)?;
        match self.handle.ask(Ask::WorktreeHire {
            params: Box::new(params),
            worktree: lease.worktree,
        }) {
            Ok(Answered::Hired(result)) => Ok(result),
            Ok(other) => {
                self.discard(lease.worktree);
                Err(unexpected(&other))
            }
            Err(error) => {
                self.discard(lease.worktree);
                Err(error)
            }
        }
    }

    /// Steps 1 to 4: admit the hire, resolve the repository, journal a lease and make the tree.
    /// The tree waits, pending, for the hire into it. With a `trust` ticket the repository is
    /// first checked against the grant it names, and the loop checks the grant again when it
    /// journals the lease.
    ///
    /// # Errors
    ///
    /// The refusal of whichever step said no. Cleanup retains directories whose identity
    /// or Git registration cannot be proved.
    pub fn prepare(
        &self,
        params: &HireParams,
        trust: Option<&TrustTicket>,
    ) -> Result<LeaseView, Failure> {
        let refuse = |error: RpcError| Failure::other(error);
        let Answered::WorktreeAdmitted(from) = self
            .handle
            .ask(Ask::WorktreeAdmit(Box::new(params.clone())))
            .map_err(refuse)?
        else {
            return Err(refuse(super::unavailable(
                "the loop did not admit the hire",
            )));
        };
        let git_failed = |error: &GitError| refuse(from_git(error));
        self.git
            .check_version()
            .map_err(|error| git_failed(&error))?;
        let origin = self.git.toplevel(&params.cwd).map_err(|error| Failure {
            why: Why::NotARepository,
            error: from_git(&error),
        })?;
        // `core.worktree` in the repository's own configuration can make git report a root that is
        // somewhere else entirely, so the answer is accepted only when it is the directory the
        // caller named or one above it.
        if !super::contains(&origin, &params.cwd) {
            return Err(refuse(super::unavailable(
                "git reported a repository root that does not contain the directory named",
            )));
        }
        if super::plain_git_dir(&origin).is_none() {
            return Err(Failure {
                why: Why::NotARepository,
                error: super::unavailable(
                    "the repository's .git is not a plain directory (a linked worktree, a submodule or \
                     a separate git directory), so its worktrees cannot be told apart safely",
                ),
            });
        }
        // Before any other git runs in the repository: it must not be at, under or above a tree an
        // agent works in.
        self.handle
            .ask(Ask::WorktreeCheckOrigin {
                params: Box::new(params.clone()),
                origin: origin.clone(),
            })
            .map_err(refuse)?;
        if self
            .git
            .has_hook_config(&origin)
            .map_err(|error| git_failed(&error))?
        {
            return Err(refuse(super::unavailable(
                "the repository's configuration has `hook.*` keys, which git runs as hooks whatever \
                 core.hooksPath says; remove them or hire without a worktree",
            )));
        }
        if let Some(lease) = &from
            && !super::same_directory(&lease.origin, &origin)
        {
            return Err(refuse(super::unavailable(
                "the worktree to start from belongs to another repository",
            )));
        }
        let base = match &from {
            Some(lease) => self
                .git
                .commit(&lease.origin, &format!("refs/heads/{}", lease.branch)),
            None => self.git.commit(&origin, "HEAD"),
        }
        .map_err(|error| git_failed(&error))?;
        if let Some(ticket) = trust {
            super::trust::verify(&self.git, &origin, base, &ticket.vouch).map_err(|refusal| {
                Failure {
                    why: match refusal {
                        TrustRefusal::Moved | TrustRefusal::Changed => Why::TrustChanged,
                        _ => Why::Other,
                    },
                    error: super::unavailable(refusal.to_string()),
                }
            })?;
        }
        let Answered::WorktreeLeased(lease) = self
            .handle
            .ask(Ask::WorktreeLease {
                params: Box::new(params.clone()),
                origin,
                base,
                from: params.from,
                trust: trust.cloned().map(Box::new),
            })
            .map_err(refuse)?
        else {
            return Err(refuse(super::unavailable(
                "the loop did not lease a worktree",
            )));
        };
        if let Err(error) = self.add_copy(&lease) {
            self.discard(lease.worktree);
            return Err(refuse(error));
        }
        Ok(lease)
    }

    fn add_copy(&self, lease: &LeaseView) -> Result<(), RpcError> {
        super::pending::verify_prepared(lease).map_err(super::unavailable)?;
        self.git
            .add(&lease.origin, &lease.path, &lease.branch, lease.base)
            .map_err(|error| from_git(&error))?;
        super::pending::verify(lease).map_err(super::unavailable)
    }

    /// `workspace.trust`: read the repository of an enrolled project and journal the operator's
    /// grant, or journal its revocation.
    ///
    /// # Errors
    ///
    /// The project is not enrolled, or the repository cannot be vouched for.
    pub fn trust(
        &self,
        params: &dudes_proto::rpc::workspace::TrustParams,
    ) -> Result<dudes_proto::rpc::workspace::TrustResult, RpcError> {
        if let Some(repo_id) = &params.repo_id {
            return self.trust_repository(params, repo_id);
        }
        let refused = |text: String| RpcError {
            code: dudes_proto::rpc::ErrorCode::Refused.code(),
            message: format!("project trust: {text}"),
            data: None,
        };
        // A revocation needs no enrolled project: a removed project's grant must still be undone.
        let (directory, vouch) = if params.trusted {
            let Answered::TrustTarget(directory) = self
                .handle
                .ask(Ask::TrustTarget(params.workspace_id.clone()))?
            else {
                return Err(refused(
                    "the loop did not name the project's folder".to_owned(),
                ));
            };
            self.git
                .check_version()
                .map_err(|error| refused(error.to_string()))?;
            let vouch = Box::new(
                super::trust::inspect(&self.git, &directory)
                    .map_err(|refusal| refused(refusal.to_string()))?,
            );
            (Some(directory), Some(vouch))
        } else {
            (None, None)
        };
        let Answered::TrustSet(changed) = self.handle.ask(Ask::TrustSet {
            workspace_id: params.workspace_id.clone(),
            directory,
            vouch,
        })?
        else {
            return Err(refused("the loop did not answer the trust call".to_owned()));
        };
        Ok(dudes_proto::rpc::workspace::TrustResult {
            trusted: params.trusted,
            changed,
        })
    }

    fn trust_repository(
        &self,
        params: &dudes_proto::rpc::workspace::TrustParams,
        repo_id: &dudes_proto::repository::RepoId,
    ) -> Result<dudes_proto::rpc::workspace::TrustResult, RpcError> {
        let refused = |message: String| RpcError {
            code: dudes_proto::rpc::ErrorCode::Refused.code(),
            message: format!("project trust: {message}"),
            data: None,
        };
        let (target, vouch) = if params.trusted {
            let Answered::RepositoryTrustTarget(target) =
                self.handle.ask(Ask::RepositoryTrustTarget {
                    project: params.workspace_id.clone(),
                    repo_id: repo_id.clone(),
                })?
            else {
                return Err(refused("current repository is unavailable".to_owned()));
            };
            self.git
                .check_version()
                .map_err(|error| refused(error.to_string()))?;
            let vouch = super::trust::inspect(&self.git, &target.origin.directory)
                .map_err(|error| refused(error.to_string()))?;
            (Some(target), Some(Box::new(vouch)))
        } else {
            (None, None)
        };
        let Answered::TrustSet(changed) = self.handle.ask(Ask::RepositoryTrustSet {
            target,
            project: params.workspace_id.clone(),
            repo_id: repo_id.clone(),
            vouch,
        })?
        else {
            return Err(refused("the trust action was not answered".to_owned()));
        };
        Ok(dudes_proto::rpc::workspace::TrustResult {
            trusted: params.trusted,
            changed,
        })
    }

    /// Remove a proved pending tree, retain its branch, then settle definite absence.
    ///
    /// A lease that is not pending, because an agent was hired into it after all, is left alone.
    pub fn discard(&self, worktree: WorktreeId) {
        let Ok(Answered::WorktreeInfo(Some(lease))) = self.handle.ask(Ask::WorktreeInfo(worktree))
        else {
            return;
        };
        match super::pending::discard(&lease) {
            Ok(()) => {
                if let Err(error) = self.handle.ask(Ask::WorktreeNeverAssigned(worktree)) {
                    tracing::warn!(%worktree, reason = %error.message, "pending worktree could not be settled");
                }
            }
            Err(reason) => {
                tracing::warn!(%worktree, %reason, path = %lease.path.display(),
                    cleanup = ?super::pending::cleanup_path(&lease).ok(),
                    "pending worktree retained for the operator");
            }
        }
    }

    /// Clean up every lease that was still pending when the daemon started: a crash after the
    /// lease was journaled and before the hire leaves one, and nothing else can.
    pub fn clean_up_after_a_crash(self: &Arc<Self>, pending: Vec<LeaseView>) {
        for lease in pending {
            self.discard(lease.worktree);
        }
    }
}
