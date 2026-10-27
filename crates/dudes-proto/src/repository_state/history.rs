//! Observed origin history and atomic copy assignment, without configuration or filesystem I/O.

use std::collections::BTreeMap;
use std::path::Path;

use dudes_types::{Lsn, WorktreeId};
use serde::{Deserialize, Serialize};

use crate::Record;
use crate::record::{TrustPin, TrustedRepository};
use crate::repository::{
    CatalogRepository, HireRepositories, RepoId, RepositorySet, ScopedRepositories,
};

/// Immutable observed revisions and permanent id bindings for one project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRepositoryHistory {
    /// Stable project identity.
    pub project: String,
    /// Every observed valid revision. This is history, not current configuration.
    pub revisions: BTreeMap<Lsn, RepositorySet<CatalogRepository>>,
    /// First origin binding, retained even when a later revision removes the member.
    pub bindings: BTreeMap<RepoId, TrustedRepository>,
}

/// The latest per-repository grant or revocation, retaining its original binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryTrust {
    /// Stable project identity.
    pub project: String,
    /// Stable member identity.
    pub repo_id: RepoId,
    /// Actual trust record position.
    pub lsn: Lsn,
    /// Catalog revision observed by the action.
    pub catalog_lsn: Lsn,
    /// Exact origin binding, including after revocation.
    pub origin: TrustedRepository,
    /// Settings grant, absent after revocation.
    pub pin: Option<TrustPin>,
}

/// Borrowed lease facts used by both projection adapters.
#[derive(Clone, Copy, Debug)]
pub struct RepositoryLease<'a> {
    /// Lease identity.
    pub worktree: WorktreeId,
    /// Actual pre-copy lease position.
    pub leased_lsn: Lsn,
    /// Recorded origin directory.
    pub origin: &'a Path,
    /// Recorded copy directory.
    pub path: &'a Path,
    /// Recorded base commit.
    pub base: dudes_types::GitCommit,
    /// Whether any hire already owns this lease.
    pub assigned: bool,
    /// Whether removal has been recorded.
    pub removed: bool,
}

/// Only the preceding row components an observed repository event may depend on.
#[derive(Clone, Copy, Debug, Default)]
pub struct RepositoryHistoryFacts<'a> {
    /// History for the affected project.
    pub project: Option<&'a ProjectRepositoryHistory>,
    /// Latest trust rows for the affected project.
    pub trust: &'a [RepositoryTrust],
    /// Leases named by a hire.
    pub leases: &'a [RepositoryLease<'a>],
    /// Live restricted hires that a newly observed revision must preserve.
    pub active_hires: &'a [&'a ScopedRepositories],
    /// Existing authority for the affected agent, when present.
    pub hire: Option<&'a HireRepositories>,
}

/// Validated replacements owned by the project and trust rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepositoryHistoryChange {
    /// No catalog or trust replacement. A hire's authority remains on its agent row.
    Unchanged,
    /// A new immutable observed revision and its retained first bindings.
    Project(ProjectRepositoryHistory),
    /// The latest grant or revocation.
    Trust(RepositoryTrust),
}

/// A recorded repository relationship cannot be established from its preceding journal prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepositoryStateError {
    /// Recorded paths or directory identities overlap or are structurally invalid.
    Binding,
    /// A stable id was rebound to another origin.
    Rebound,
    /// Referenced observed configuration is absent or has different membership.
    Catalog,
    /// Observed removal would strand a live hire.
    Referenced,
    /// An exact valid per-repository trust grant is absent or changed.
    Trust,
    /// A copy has no preceding unassigned, unremoved matching lease.
    Lease,
    /// Agent identity already owns a hire.
    Hire,
    /// Historical singleton carriers cannot authorize restricted repository execution.
    LegacyIssue,
}

impl std::fmt::Display for RepositoryStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Binding => "invalid repository bindings",
            Self::Rebound => "repository id cannot be rebound",
            Self::Catalog => "repository catalog revision does not match",
            Self::Referenced => "repository is referenced by a live hire",
            Self::Trust => "repository trust grant does not match",
            Self::Lease => "owned repository lease does not match",
            Self::Hire => "agent already has repository authority",
            Self::LegacyIssue => "restricted repositories require a scoped issue carrier",
        })
    }
}
impl std::error::Error for RepositoryStateError {}

/// Whether this kind requires full-prefix repository validation and an exact required frame.
#[must_use]
pub const fn is_kind(kind: crate::RecordKind) -> bool {
    matches!(
        kind,
        crate::RecordKind::ProjectRepositoriesRecorded
            | crate::RecordKind::RepositoryTrustSet
            | crate::RecordKind::AgentHiredWithRepositories
    )
}

/// Validate the position-independent portion before serializing or accepting a carrier.
pub fn validate_shape(record: &Record) -> Result<(), RepositoryStateError> {
    match record {
        Record::ProjectRepositoriesRecorded {
            project,
            repositories,
        } => {
            if project.is_empty() {
                return Err(RepositoryStateError::Catalog);
            }
            validate_origins(
                repositories
                    .entries()
                    .iter()
                    .map(|entry| &entry.value.origin),
            )
        }
        Record::RepositoryTrustSet {
            project, origin, ..
        } => {
            if project.is_empty() {
                return Err(RepositoryStateError::Catalog);
            }
            validate_origins(std::iter::once(origin))
        }
        Record::AgentHiredWithRepositories(hire) => {
            let scope = &hire.repositories;
            if scope.project.id.is_empty() {
                return Err(RepositoryStateError::Catalog);
            }
            validate_origins(
                scope
                    .repositories
                    .entries()
                    .iter()
                    .map(|entry| &entry.value.origin),
            )?;
            let mut roots: Vec<&Path> = Vec::new();
            let mut identities = Vec::new();
            let mut leases = Vec::new();
            let mut git_ids = Vec::new();
            let origins: Vec<_> = scope
                .repositories
                .entries()
                .iter()
                .map(|entry| &entry.value.origin)
                .collect();
            for entry in scope.repositories.entries() {
                let repo = &entry.value;
                if !absolute(&repo.execution.directory)
                    || !absolute(&repo.execution.git_dir)
                    || !repo
                        .execution
                        .git_dir
                        .starts_with(repo.origin.common_dir.join("worktrees"))
                    || repo.execution.git_dir == repo.origin.common_dir.join("worktrees")
                    || origins.iter().any(|origin| {
                        overlaps(&origin.directory, &repo.execution.directory)
                            || overlaps(&origin.common_dir, &repo.execution.directory)
                            || origin.directory_identity == repo.execution.directory_identity
                            || origin.common_identity == repo.execution.directory_identity
                            || origin.common_identity == repo.execution.git_identity
                    })
                    || roots
                        .iter()
                        .any(|root| overlaps(root, &repo.execution.directory))
                    || identities.contains(&repo.execution.directory_identity)
                    || leases.contains(&repo.worktree)
                    || git_ids.contains(&repo.execution.git_identity)
                    || git_ids.contains(&repo.execution.directory_identity)
                    || identities.contains(&repo.execution.git_identity)
                    || repo.execution.directory_identity == repo.execution.git_identity
                {
                    return Err(RepositoryStateError::Binding);
                }
                roots.push(repo.execution.directory.as_path());
                identities.push(repo.execution.directory_identity);
                leases.push(repo.worktree);
                git_ids.push(repo.execution.git_identity);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Validate relationships and prepare replacements without changing any row.
pub fn prepare(
    lsn: Lsn,
    record: &Record,
    before: RepositoryHistoryFacts<'_>,
) -> Result<RepositoryHistoryChange, RepositoryStateError> {
    validate_shape(record)?;
    match record {
        Record::ProjectRepositoriesRecorded {
            project,
            repositories,
        } => {
            let mut history = before
                .project
                .cloned()
                .unwrap_or_else(|| ProjectRepositoryHistory {
                    project: project.clone(),
                    revisions: BTreeMap::new(),
                    bindings: BTreeMap::new(),
                });
            if history.project != *project || history.revisions.contains_key(&lsn) {
                return Err(RepositoryStateError::Catalog);
            }
            for entry in repositories.entries() {
                if history
                    .bindings
                    .get(&entry.repo_id)
                    .is_some_and(|origin| *origin != entry.value.origin)
                {
                    return Err(RepositoryStateError::Rebound);
                }
                history
                    .bindings
                    .insert(entry.repo_id.clone(), entry.value.origin.clone());
            }
            for hire in before
                .active_hires
                .iter()
                .filter(|hire| hire.project.id == *project)
            {
                if hire.repositories.entries().iter().any(|entry| {
                    repositories
                        .get(&entry.repo_id)
                        .is_none_or(|repo| repo.origin != entry.value.origin)
                }) {
                    return Err(RepositoryStateError::Referenced);
                }
            }
            history.revisions.insert(lsn, repositories.clone());
            Ok(RepositoryHistoryChange::Project(history))
        }
        Record::RepositoryTrustSet {
            project,
            repo_id,
            catalog_lsn,
            origin,
            pin,
        } => {
            let revision = revision(before.project, project, *catalog_lsn, lsn)?;
            if revision
                .get(repo_id)
                .is_none_or(|repo| repo.origin != *origin)
            {
                return Err(RepositoryStateError::Trust);
            }
            Ok(RepositoryHistoryChange::Trust(RepositoryTrust {
                project: project.clone(),
                repo_id: repo_id.clone(),
                lsn,
                catalog_lsn: *catalog_lsn,
                origin: origin.clone(),
                pin: pin.clone(),
            }))
        }
        Record::AgentHiredWithRepositories(hire) => {
            validate_hire(lsn, &hire.repositories, before)?;
            Ok(RepositoryHistoryChange::Unchanged)
        }
        _ if record.turn_issue().is_some()
            && matches!(before.hire, Some(HireRepositories::Scoped(_))) =>
        {
            Err(RepositoryStateError::LegacyIssue)
        }
        _ => Ok(RepositoryHistoryChange::Unchanged),
    }
}

fn validate_hire(
    lsn: Lsn,
    scope: &ScopedRepositories,
    before: RepositoryHistoryFacts<'_>,
) -> Result<(), RepositoryStateError> {
    if before.hire.is_some() {
        return Err(RepositoryStateError::Hire);
    }
    let catalog = revision(before.project, &scope.project.id, scope.catalog_lsn, lsn)?;
    let latest = before
        .project
        .and_then(|history| history.revisions.last_key_value())
        .map(|(_, revision)| revision)
        .ok_or(RepositoryStateError::Catalog)?;
    if !matches_catalog(catalog, scope) || !matches_catalog(latest, scope) {
        return Err(RepositoryStateError::Catalog);
    }
    let first_lease = scope
        .repositories
        .entries()
        .iter()
        .try_fold(lsn, |first, entry| {
            before
                .leases
                .iter()
                .find(|lease| lease.worktree == entry.value.worktree)
                .map(|lease| first.min(lease.leased_lsn))
                .ok_or(RepositoryStateError::Lease)
        })?;
    for entry in scope.repositories.entries() {
        let repo = &entry.value;
        let trust = before
            .trust
            .iter()
            .find(|trust| trust.project == scope.project.id && trust.repo_id == entry.repo_id)
            .ok_or(RepositoryStateError::Trust)?;
        if trust.lsn != repo.trust_lsn
            || trust.lsn >= first_lease
            || trust.lsn >= lsn
            || trust.pin.is_none()
            || trust.origin != repo.origin
        {
            return Err(RepositoryStateError::Trust);
        }
        let lease = before
            .leases
            .iter()
            .find(|lease| lease.worktree == repo.worktree)
            .ok_or(RepositoryStateError::Lease)?;
        if lease.assigned
            || lease.removed
            || lease.origin != repo.origin.directory
            || lease.path != repo.execution.directory
            || lease.base != repo.base
            || lease.leased_lsn <= trust.lsn
            || lease.leased_lsn >= lsn
        {
            return Err(RepositoryStateError::Lease);
        }
    }
    Ok(())
}

fn matches_catalog(catalog: &RepositorySet<CatalogRepository>, scope: &ScopedRepositories) -> bool {
    catalog.primary_id() == scope.repositories.primary_id()
        && catalog.entries().len() == scope.repositories.entries().len()
        && catalog
            .entries()
            .iter()
            .zip(scope.repositories.entries())
            .all(|(origin, copy)| {
                origin.repo_id == copy.repo_id && origin.value.origin == copy.value.origin
            })
}

fn revision<'a>(
    history: Option<&'a ProjectRepositoryHistory>,
    project: &str,
    catalog: Lsn,
    at: Lsn,
) -> Result<&'a RepositorySet<CatalogRepository>, RepositoryStateError> {
    history
        .filter(|history| history.project == project && catalog < at)
        .and_then(|history| history.revisions.get(&catalog))
        .ok_or(RepositoryStateError::Catalog)
}

fn absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|part| {
            !matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn validate_origins<'a>(
    origins: impl Iterator<Item = &'a TrustedRepository>,
) -> Result<(), RepositoryStateError> {
    let origins: Vec<_> = origins.collect();
    for (index, origin) in origins.iter().enumerate() {
        if !absolute(&origin.directory)
            || !absolute(&origin.common_dir)
            || origin.directory_identity == origin.common_identity
            || origins[..index].iter().any(|other| {
                overlaps(&origin.directory, &other.directory)
                    || origin.directory_identity == other.directory_identity
                    || origin.common_identity == other.common_identity
                    || origin.common_dir == other.common_dir
            })
        {
            return Err(RepositoryStateError::Binding);
        }
    }
    Ok(())
}
