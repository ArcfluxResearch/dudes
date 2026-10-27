//! Observed origin history and atomic copy assignment, without configuration or filesystem I/O.

use std::collections::BTreeMap;
use std::path::Path;

use dudes_types::{AgentId, CardStatus, Lsn, Seq, WorktreeId};
use serde::{Deserialize, Serialize};

use crate::Record;
use crate::record::{TrustPin, TrustedRepository};
use crate::repository::{
    CatalogRepository, HireRepositories, RepoId, RepositorySet, ScopedRepositories,
    TicketDeclaration, TicketRepositories,
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
    /// The first hire that owns this lease.
    pub agent: Option<AgentId>,
    /// Whether removal has been recorded.
    pub removed: bool,
}

/// The repository class an agent identity has held, retained after retirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryHireClass {
    /// Only historical singleton hires were recorded for this identity.
    Legacy,
    /// Restricted repository authority was recorded for this identity.
    Scoped,
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
    /// Whether this identity has ever been hired, and under which repository class.
    pub hire_class: Option<RepositoryHireClass>,
    /// Actual position of the current hire.
    pub hired_lsn: Option<Lsn>,
    /// Ticket row preceding a creation, edit or start.
    pub ticket: Option<RepositoryTicketFacts<'a>>,
    /// Unresolved declarations that a new catalog must preserve.
    pub active_tickets: &'a [&'a TicketDeclaration],
    /// Whether the proposed goal identity already exists.
    pub goal_exists: bool,
    /// Any existing card at the proposed creation identity.
    pub card_exists: bool,
}

/// The ticket-owned facts required to validate a declared edit or start.
#[derive(Clone, Copy, Debug)]
pub struct RepositoryTicketFacts<'a> {
    /// Immutable declaration and current check target.
    pub repositories: &'a TicketRepositories,
    /// Current resolved check definition.
    pub check: &'a crate::TicketCheck,
    /// Current board status.
    pub status: CardStatus,
    /// Current holder.
    pub holder: Option<AgentId>,
    /// Exact current card revision.
    pub seq: Seq,
    /// Last started round.
    pub round: u32,
    /// A ticket assignment currently holds the queue position.
    pub queued: bool,
}

/// Validated replacements owned by the project and trust rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepositoryHistoryChange {
    /// No repository history replacement.
    Unchanged,
    /// Retain only the identity's hire class; execution authority stays on its agent row.
    Hired {
        /// Hired identity.
        agent: AgentId,
        /// Lifetime repository class.
        class: RepositoryHireClass,
    },
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
    /// Observed removal would strand a live hire or unresolved declaration.
    Referenced,
    /// An exact valid per-repository trust grant is absent or changed.
    Trust,
    /// A copy has no preceding unassigned, unremoved matching lease.
    Lease,
    /// Agent identity already owns a hire.
    Hire,
    /// Historical singleton carriers cannot authorize restricted repository execution.
    LegacyIssue,
    /// Ticket declaration or check target does not match its recorded catalog.
    Ticket,
    /// Start does not match the exact queued ticket, hire, round, goal or check binding.
    Start,
}

impl std::fmt::Display for RepositoryStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Binding => "invalid repository bindings",
            Self::Rebound => "repository id cannot be rebound",
            Self::Catalog => "repository catalog revision does not match",
            Self::Referenced => "repository is referenced by a live hire or unresolved ticket",
            Self::Trust => "repository trust grant does not match",
            Self::Lease => "owned repository lease does not match",
            Self::Hire => "agent already has repository authority",
            Self::LegacyIssue => "restricted repositories require a scoped issue carrier",
            Self::Ticket => "ticket repository declaration does not match",
            Self::Start => "ticket start repository authority does not match",
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
            | crate::RecordKind::TicketCreatedWithRepositories
            | crate::RecordKind::TicketEditedWithRepositories
            | crate::RecordKind::TicketStartedWithRepositories
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
        Record::AgentHiredWithRepositories(hire) => validate_hire_shape(&hire.repositories),
        Record::TicketCreatedWithRepositories(created) => {
            validate_declaration(&created.declaration)?;
            validate_check(
                &created.declaration,
                &created.check_repo,
                &created.done_when,
            )
        }
        Record::TicketEditedWithRepositories(edited) => validate_command(&edited.done_when),
        Record::TicketStartedWithRepositories(start) => {
            validate_declaration(&start.repositories.declaration)?;
            validate_check(
                &start.repositories.declaration,
                &start.repositories.check_repo,
                &start.repositories.check,
            )?;
            if start.round == 0 {
                return Err(RepositoryStateError::Start);
            }
            let root = &start.repositories.check_root;
            if !absolute(&root.directory)
                || !absolute(&root.git_dir)
                || root.directory_identity == root.git_identity
            {
                return Err(RepositoryStateError::Binding);
            }
            validate_ticket_goal(&start.goal, &start.repositories.check)
        }
        _ => Ok(()),
    }
}

fn validate_hire_shape(scope: &ScopedRepositories) -> Result<(), RepositoryStateError> {
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

/// Validate relationships and prepare replacements without changing any row.
#[expect(
    clippy::too_many_lines,
    reason = "one dispatcher validates the repository carrier family before preparing a replacement"
)]
pub fn prepare(
    lsn: Lsn,
    record: &Record,
    before: RepositoryHistoryFacts<'_>,
) -> Result<RepositoryHistoryChange, RepositoryStateError> {
    validate_shape(record)?;
    validate_ticket_record(lsn, record, before)?;
    if let Some(hire) = record.hire() {
        let class = match hire.repositories {
            crate::repository_state::HireRepositoryRecord::Legacy { .. } => {
                if was_scoped(before) {
                    return Err(RepositoryStateError::Hire);
                }
                RepositoryHireClass::Legacy
            }
            crate::repository_state::HireRepositoryRecord::Scoped(scope) => {
                validate_hire(lsn, scope, before)?;
                RepositoryHireClass::Scoped
            }
        };
        return Ok(RepositoryHistoryChange::Hired {
            agent: hire.agent,
            class,
        });
    }
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
            for declaration in before
                .active_tickets
                .iter()
                .filter(|ticket| ticket.project == *project)
            {
                if declaration.repositories.entries().iter().any(|entry| {
                    repositories
                        .get(&entry.repo_id)
                        .is_none_or(|repo| repo.origin != entry.value)
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
        _ if record.turn_issue().is_some() && (was_scoped(before) || expanded(before)) => {
            Err(RepositoryStateError::LegacyIssue)
        }
        _ => Ok(RepositoryHistoryChange::Unchanged),
    }
}

fn validate_ticket_record(
    lsn: Lsn,
    record: &Record,
    before: RepositoryHistoryFacts<'_>,
) -> Result<(), RepositoryStateError> {
    if let Some(created) = record.ticket_created()
        && let Some((declaration, _)) = created.repositories
    {
        if before.card_exists || before.ticket.is_some() {
            return Err(RepositoryStateError::Ticket);
        }
        let catalog = revision(
            before.project,
            &declaration.project,
            declaration.catalog_lsn,
            lsn,
        )?;
        if before
            .project
            .and_then(|history| history.revisions.last_key_value())
            .map(|(at, _)| *at)
            != Some(declaration.catalog_lsn)
            || !declaration_matches(catalog, declaration)
        {
            return Err(RepositoryStateError::Ticket);
        }
    }
    if let Some(edited) = record.ticket_edited() {
        match (
            before
                .ticket
                .and_then(|ticket| ticket.repositories.declared()),
            edited.check_repo,
        ) {
            (Some(scope), Some(repo)) => {
                validate_check(&scope.declaration, repo, edited.done_when)?;
            }
            (None, None) => {}
            _ => return Err(RepositoryStateError::Ticket),
        }
        if edited.check_repo.is_some()
            && before.ticket.is_none_or(|ticket| {
                ticket.seq != edited.expected_seq
                    || !matches!(ticket.status, CardStatus::Todo | CardStatus::Claimed)
            })
        {
            return Err(RepositoryStateError::Ticket);
        }
    }
    if let Some(start) = record.ticket_start() {
        validate_start(lsn, start, before)?;
    }
    Ok(())
}

fn validate_declaration(declaration: &TicketDeclaration) -> Result<(), RepositoryStateError> {
    if declaration.project.is_empty()
        || declaration
            .repositories
            .entries()
            .first()
            .is_none_or(|entry| &entry.repo_id != declaration.repositories.primary_id())
    {
        return Err(RepositoryStateError::Ticket);
    }
    validate_origins(
        declaration
            .repositories
            .entries()
            .iter()
            .map(|entry| &entry.value),
    )
}

fn validate_command(check: &crate::TicketCheck) -> Result<(), RepositoryStateError> {
    if !matches!(&check.predicate, crate::Predicate::Command { argv, timeout_s } if !argv.is_empty() && !argv[0].is_empty() && *timeout_s > 0)
        || check.id.is_empty()
        || check.label.is_empty()
    {
        return Err(RepositoryStateError::Ticket);
    }
    Ok(())
}

fn validate_check(
    declaration: &TicketDeclaration,
    repo: &RepoId,
    check: &crate::TicketCheck,
) -> Result<(), RepositoryStateError> {
    validate_command(check)?;
    if declaration.repositories.get(repo).is_none() {
        return Err(RepositoryStateError::Ticket);
    }
    Ok(())
}

fn validate_ticket_goal(
    goal: &crate::Goal,
    check: &crate::TicketCheck,
) -> Result<(), RepositoryStateError> {
    if goal.predicate != check.predicate
        || goal.baseline != Some(crate::CheckBaseline::MayAlreadyPass)
        || goal
            .completion_check
            .as_ref()
            .is_none_or(|reference| reference.id != check.id || reference.label != check.label)
    {
        return Err(RepositoryStateError::Start);
    }
    Ok(())
}

fn declaration_matches(
    catalog: &RepositorySet<CatalogRepository>,
    declaration: &TicketDeclaration,
) -> bool {
    let selected: Vec<_> = catalog
        .entries()
        .iter()
        .filter(|entry| declaration.repositories.get(&entry.repo_id).is_some())
        .collect();
    selected.len() == declaration.repositories.entries().len()
        && selected
            .iter()
            .zip(declaration.repositories.entries())
            .all(|(origin, declared)| {
                origin.repo_id == declared.repo_id && origin.value.origin == declared.value
            })
}

fn expanded(before: RepositoryHistoryFacts<'_>) -> bool {
    before
        .project
        .and_then(|history| history.revisions.last_key_value())
        .is_some_and(|(_, catalog)| catalog.entries().len() > 1)
}

#[expect(
    clippy::too_many_lines,
    reason = "one atomic start binds its ticket, goal, hire and check before either row changes"
)]
fn validate_start(
    lsn: Lsn,
    start: crate::repository_state::TicketStart<'_>,
    before: RepositoryHistoryFacts<'_>,
) -> Result<(), RepositoryStateError> {
    let Some((round, authority)) = start.repositories else {
        if before
            .ticket
            .is_some_and(|ticket| ticket.repositories.declared().is_some())
            || was_scoped(before)
            || expanded(before)
        {
            return Err(RepositoryStateError::Start);
        }
        return Ok(());
    };
    let ticket = before.ticket.ok_or(RepositoryStateError::Start)?;
    let scope = ticket
        .repositories
        .declared()
        .ok_or(RepositoryStateError::Start)?;
    let Some(HireRepositories::Scoped(hire)) = before.hire else {
        return Err(RepositoryStateError::Start);
    };
    if !ticket.queued
        || ticket.seq != start.expected_seq
        || ticket.status != CardStatus::Claimed
        || ticket.holder != Some(start.agent)
        || ticket.round.checked_add(1) != Some(round)
        || before.goal_exists
        || before.hired_lsn != Some(authority.hired_lsn)
        || authority.hired_lsn >= lsn
        || authority.declaration != scope.declaration
        || authority.check != *ticket.check
        || authority.check_repo != scope.check_repo
        || hire.project.id != scope.declaration.project
    {
        return Err(RepositoryStateError::Start);
    }
    validate_ticket_goal(start.goal, ticket.check)?;
    let history = before.project.ok_or(RepositoryStateError::Catalog)?;
    let catalog = revision(
        Some(history),
        &scope.declaration.project,
        scope.declaration.catalog_lsn,
        lsn,
    )?;
    let latest = history
        .revisions
        .last_key_value()
        .map(|(_, catalog)| catalog)
        .ok_or(RepositoryStateError::Catalog)?;
    if !declaration_matches(catalog, &scope.declaration)
        || !scope
            .declaration
            .repositories
            .entries()
            .iter()
            .all(|entry| {
                latest
                    .get(&entry.repo_id)
                    .is_some_and(|repo| repo.origin == entry.value)
            })
    {
        return Err(RepositoryStateError::Ticket);
    }
    for entry in latest.entries() {
        if before
            .trust
            .iter()
            .find(|trust| trust.project == hire.project.id && trust.repo_id == entry.repo_id)
            .is_none_or(|trust| trust.pin.is_none() || trust.origin != entry.value.origin)
        {
            return Err(RepositoryStateError::Trust);
        }
    }
    for entry in hire.repositories.entries() {
        let repo = &entry.value;
        if before
            .leases
            .iter()
            .find(|lease| lease.worktree == repo.worktree)
            .is_none_or(|lease| {
                lease.agent != Some(start.agent)
                    || lease.removed
                    || lease.leased_lsn >= authority.hired_lsn
                    || lease.origin != repo.origin.directory
                    || lease.path != repo.execution.directory
                    || lease.base != repo.base
            })
        {
            return Err(RepositoryStateError::Lease);
        }
        if latest
            .get(&entry.repo_id)
            .is_none_or(|repo| repo.origin != entry.value.origin)
            || before
                .trust
                .iter()
                .find(|trust| trust.project == hire.project.id && trust.repo_id == entry.repo_id)
                .is_none_or(|trust| {
                    trust.lsn != entry.value.trust_lsn
                        || trust.pin.is_none()
                        || trust.origin != entry.value.origin
                })
        {
            return Err(RepositoryStateError::Trust);
        }
    }
    for entry in scope.declaration.repositories.entries() {
        if hire
            .repositories
            .get(&entry.repo_id)
            .is_none_or(|repo| repo.origin != entry.value)
        {
            return Err(RepositoryStateError::Start);
        }
    }
    if hire
        .repositories
        .get(&authority.check_repo)
        .is_none_or(|repo| repo.execution != authority.check_root)
    {
        return Err(RepositoryStateError::Start);
    }
    Ok(())
}

fn was_scoped(before: RepositoryHistoryFacts<'_>) -> bool {
    before.hire_class == Some(RepositoryHireClass::Scoped)
        || matches!(before.hire, Some(HireRepositories::Scoped(_)))
}

fn validate_hire(
    lsn: Lsn,
    scope: &ScopedRepositories,
    before: RepositoryHistoryFacts<'_>,
) -> Result<(), RepositoryStateError> {
    if before.hire.is_some() || before.hire_class.is_some() {
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
        if lease.agent.is_some()
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
