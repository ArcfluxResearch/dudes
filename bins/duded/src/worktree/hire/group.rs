//! One connection owns every pending copy until the loop atomically assigns the group.

use std::collections::BTreeMap;

use dudes_proto::record::{RecordedWorkspace, TrustPin, TrustedRepository};
#[cfg(unix)]
use dudes_proto::repository::ExecutionRepository;
use dudes_proto::repository::{OwnedRepository, RepoId, RepositoryEntry, RepositorySet};
use dudes_types::{GitCommit, Lsn};

use super::{Answered, Ask, HireParams, HireResult, LeaseView, RpcError, WorktreeHires};

/// Original immutable authority for one member of a pending hire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryHireOrigin {
    /// Exact current origin and common directory identities.
    pub origin: TrustedRepository,
    /// The original grant position, never refreshed within a group.
    pub trust_lsn: Lsn,
    /// Complete settings pins from that original grant.
    pub pin: TrustPin,
}

/// A bounded whole-project admission captured before the first lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryHirePlan {
    /// Ordinary hire options and original explicit project target.
    pub params: HireParams,
    /// Recorded project identity and label.
    pub project: RecordedWorkspace,
    /// Exact current observed catalog.
    pub catalog_lsn: Lsn,
    /// Original grant vector in configured order with its configured primary.
    pub repositories: RepositorySet<RepositoryHireOrigin>,
}

/// Every actual copy, ready for one atomic assignment.
#[derive(Clone, Debug)]
pub struct PreparedRepositoryGroup {
    /// Original admission, retained without refreshing grants.
    pub plan: RepositoryHirePlan,
    /// Complete execution identities in configured order.
    pub repositories: RepositorySet<OwnedRepository>,
}

enum PreparationFailure {
    Refused(RpcError),
    Uncertain(RpcError),
}

impl From<RpcError> for PreparationFailure {
    fn from(error: RpcError) -> Self {
        Self::Refused(error)
    }
}

struct PendingCopy {
    lease: LeaseView,
    owned: Option<OwnedRepository>,
}

impl WorktreeHires {
    pub(super) fn hire_project(&self, params: HireParams) -> Result<HireResult, RpcError> {
        let Answered::RepositoryHireAdmitted(plan) = self
            .handle
            .ask(Ask::RepositoryHireAdmit(Box::new(params)))?
        else {
            return Err(super::super::unavailable(
                "the loop did not admit the project hire",
            ));
        };
        self.git
            .check_version()
            .map_err(|error| super::from_git(&error))?;
        let bases = self.repository_bases(&plan)?;
        let mut pending = Vec::new();
        let prepared = match self.prepare_group(&plan, &bases, &mut pending) {
            Ok(prepared) => prepared,
            Err(PreparationFailure::Refused(error)) => {
                self.discard_group(&pending);
                return Err(error);
            }
            Err(PreparationFailure::Uncertain(error)) => return Err(error),
        };
        match self
            .handle
            .ask(Ask::RepositoryHireCommit(Box::new(prepared)))
        {
            Ok(Answered::Hired(result)) => Ok(result),
            Ok(Answered::RepositoryHireRejected(error)) => {
                self.discard_group(&pending);
                Err(error)
            }
            // The writer may have committed before losing its acknowledgment. Replay owns the
            // decision and copies remain intact until it can establish whether they were hired.
            Err(error) => Err(error),
            Ok(other) => Err(super::unexpected(&other)),
        }
    }

    fn repository_bases(
        &self,
        plan: &RepositoryHirePlan,
    ) -> Result<BTreeMap<RepoId, GitCommit>, RpcError> {
        plan.repositories
            .entries()
            .iter()
            .map(|entry| {
                self.git
                    .commit(&entry.value.origin.directory, "HEAD")
                    .map(|base| (entry.repo_id.clone(), base))
                    .map_err(|error| super::from_git(&error))
            })
            .collect()
    }

    fn verify_group(
        &self,
        plan: &RepositoryHirePlan,
        bases: &BTreeMap<RepoId, GitCommit>,
        pending: &[PendingCopy],
    ) -> Result<(), RpcError> {
        for entry in plan.repositories.entries() {
            let origin = &entry.value.origin.directory;
            self.refuse_configured_hooks(origin)?;
            let current = self
                .git
                .commit(origin, "HEAD")
                .map_err(|error| super::from_git(&error))?;
            let vouch = super::Vouch {
                repository: entry.value.origin.clone(),
                pin: entry.value.pin.clone(),
            };
            let original_base = bases[&entry.repo_id];
            for base in
                std::iter::once(original_base).chain((current != original_base).then_some(current))
            {
                super::super::trust::verify(&self.git, origin, base, &vouch)
                    .map_err(|error| super::super::unavailable(error.to_string()))?;
            }
        }
        for copy in pending {
            let owned = copy.owned.as_ref().ok_or_else(|| {
                super::super::unavailable("a pending repository copy is incomplete")
            })?;
            if !super::super::is_owned_repository(owned)
                || self
                    .git
                    .commit(&copy.lease.path, "HEAD")
                    .map_err(|error| super::from_git(&error))?
                    != copy.lease.base
            {
                return Err(super::super::unavailable(
                    "a pending repository copy changed",
                ));
            }
        }
        Ok(())
    }

    fn prepare_group(
        &self,
        plan: &RepositoryHirePlan,
        bases: &BTreeMap<RepoId, GitCommit>,
        pending: &mut Vec<PendingCopy>,
    ) -> Result<PreparedRepositoryGroup, PreparationFailure> {
        let mut order: Vec<_> = plan.repositories.entries().iter().collect();
        order.sort_by(|left, right| left.repo_id.cmp(&right.repo_id));
        let mut copies = BTreeMap::new();
        for entry in order {
            self.verify_group(plan, bases, pending)?;
            let lease = match self.handle.ask(Ask::RepositoryHireLease {
                plan: Box::new(plan.clone()),
                repo_id: entry.repo_id.clone(),
                base: bases[&entry.repo_id],
            }) {
                Ok(Answered::WorktreeLeased(lease)) => lease,
                Ok(Answered::RepositoryHireRejected(error)) => return Err(error.into()),
                Err(error) => return Err(PreparationFailure::Uncertain(error)),
                Ok(other) => return Err(PreparationFailure::Uncertain(super::unexpected(&other))),
            };
            pending.push(PendingCopy {
                lease: lease.clone(),
                owned: None,
            });
            self.add_copy(&lease)?;
            let owned = self.inspect_copy(&lease, &entry.value)?;
            pending
                .last_mut()
                .expect("just recorded pending lease")
                .owned = Some(owned.clone());
            copies.insert(entry.repo_id.clone(), owned);
        }
        self.verify_group(plan, bases, pending)?;
        let repositories = RepositorySet::new(
            plan.repositories.primary_id().clone(),
            plan.repositories
                .entries()
                .iter()
                .map(|entry| RepositoryEntry {
                    repo_id: entry.repo_id.clone(),
                    value: copies
                        .remove(&entry.repo_id)
                        .expect("every configured copy was prepared"),
                })
                .collect(),
        )
        .expect("same validated repository membership");
        Ok(PreparedRepositoryGroup {
            plan: plan.clone(),
            repositories,
        })
    }

    #[cfg(unix)]
    fn inspect_copy(
        &self,
        lease: &LeaseView,
        original: &RepositoryHireOrigin,
    ) -> Result<OwnedRepository, RpcError> {
        let git_dir = super::super::worktree_git_dir(&lease.path).ok_or_else(|| {
            super::super::unavailable("the repository copy has no valid Git identity")
        })?;
        let execution = ExecutionRepository {
            directory: lease.path.clone(),
            directory_identity: super::super::trust::identity(&lease.path)
                .map_err(|error| super::super::unavailable(error.to_string()))?,
            git_identity: super::super::trust::identity(&git_dir)
                .map_err(|error| super::super::unavailable(error.to_string()))?,
            git_dir,
        };
        let owned = OwnedRepository {
            origin: original.origin.clone(),
            execution,
            worktree: lease.worktree,
            base: lease.base,
            trust_lsn: original.trust_lsn,
        };
        if !super::super::is_owned_repository(&owned)
            || self
                .git
                .toplevel(&lease.path)
                .map_err(|error| super::from_git(&error))?
                != lease.path
            || self
                .git
                .commit(&lease.path, "HEAD")
                .map_err(|error| super::from_git(&error))?
                != lease.base
        {
            return Err(super::super::unavailable(
                "the repository copy does not match its lease",
            ));
        }
        Ok(owned)
    }

    #[cfg(not(unix))]
    #[expect(
        clippy::unused_self,
        reason = "the Unix implementation inspects through this instance's Git runner"
    )]
    fn inspect_copy(
        &self,
        _lease: &LeaseView,
        _original: &RepositoryHireOrigin,
    ) -> Result<OwnedRepository, RpcError> {
        Err(super::super::unavailable(
            "repository copies are unavailable on this platform",
        ))
    }

    fn discard_group(&self, pending: &[PendingCopy]) {
        for copy in pending {
            self.discard(copy.lease.worktree);
        }
    }
}
