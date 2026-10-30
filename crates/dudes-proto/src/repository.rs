//! Recorded repository authority, independent of current enrollment and filesystem state.

use std::path::{Path, PathBuf};

use dudes_types::{CardId, GoalId, Lsn, WorktreeId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::record::{FileIdentity, RecordedWorkspace, TrustPin, TrustedRepository};

/// A stable repository identity within one project.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct RepoId(#[schemars(regex(pattern = "^[a-z][a-z0-9_-]{0,31}$"))] String);

impl RepoId {
    /// Parse the bounded, case-sensitive identity used by recorded bindings.
    pub fn parse(value: &str) -> Result<Self, RepositoryShapeError> {
        if value.is_empty()
            || value.len() > 32
            || !value.as_bytes()[0].is_ascii_lowercase()
            || !value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(RepositoryShapeError::Id);
        }
        Ok(Self(value.to_owned()))
    }

    /// The recorded spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RepoId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// One identified member of an ordered repository collection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "RepositoryEntry_{T}")]
pub struct RepositoryEntry<T> {
    /// Stable identity within the project.
    pub repo_id: RepoId,
    /// The member's recorded facts.
    pub value: T,
}

/// One through eight distinct repositories with a present primary member.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(rename = "RepositorySet_{T}")]
pub struct RepositorySet<T> {
    primary: RepoId,
    #[schemars(length(min = 1, max = 8))]
    entries: Vec<RepositoryEntry<T>>,
}

impl<T> RepositorySet<T> {
    /// Validate a collection without reordering its members.
    pub fn new(
        primary: RepoId,
        entries: Vec<RepositoryEntry<T>>,
    ) -> Result<Self, RepositoryShapeError> {
        if entries.is_empty() || entries.len() > 8 {
            return Err(RepositoryShapeError::Count);
        }
        let ids: std::collections::BTreeSet<_> =
            entries.iter().map(|entry| &entry.repo_id).collect();
        if ids.len() != entries.len() {
            return Err(RepositoryShapeError::Duplicate);
        }
        if !ids.contains(&primary) {
            return Err(RepositoryShapeError::Primary);
        }
        Ok(Self { primary, entries })
    }

    /// The primary member's stable identity.
    #[must_use]
    pub const fn primary_id(&self) -> &RepoId {
        &self.primary
    }

    /// Members in recorded order, independently of lease acquisition order.
    #[must_use]
    pub fn entries(&self) -> &[RepositoryEntry<T>] {
        &self.entries
    }

    /// Resolve a stable identity through this recorded collection.
    #[must_use]
    pub fn get(&self, id: &RepoId) -> Option<&T> {
        self.entries
            .iter()
            .find(|entry| entry.repo_id == *id)
            .map(|entry| &entry.value)
    }

    /// Members with the primary first, the others in recorded order.
    pub fn primary_first(&self) -> impl Iterator<Item = &RepositoryEntry<T>> {
        self.entries
            .iter()
            .filter(|entry| entry.repo_id == self.primary)
            .chain(
                self.entries
                    .iter()
                    .filter(|entry| entry.repo_id != self.primary),
            )
    }

    /// The primary member, whose presence construction and deserialization enforce.
    #[must_use]
    pub fn primary(&self) -> &T {
        self.get(&self.primary)
            .expect("validated primary repository")
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for RepositorySet<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire<T> {
            primary: RepoId,
            entries: Vec<RepositoryEntry<T>>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.primary, wire.entries).map_err(serde::de::Error::custom)
    }
}

/// A malformed bounded repository identity or collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepositoryShapeError {
    /// Identity is not a bounded lowercase identifier.
    Id,
    /// Collection is empty or exceeds eight members.
    Count,
    /// An identity occurs twice.
    Duplicate,
    /// Primary does not name a member.
    Primary,
}

impl std::fmt::Display for RepositoryShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Id => "invalid repository id",
            Self::Count => "expected one through eight repositories",
            Self::Duplicate => "duplicate repository id",
            Self::Primary => "primary repository is absent",
        })
    }
}
impl std::error::Error for RepositoryShapeError {}

/// One origin observed in a valid project configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CatalogRepository {
    /// Presentation label, never an execution selector.
    pub label: String,
    /// Immutable origin and common-directory identities.
    pub origin: TrustedRepository,
}

/// The immutable identities of one owned execution copy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionRepository {
    /// Canonical working tree root.
    pub directory: PathBuf,
    /// Working tree identity at hire.
    pub directory_identity: FileIdentity,
    /// Resolved Git administrative directory.
    pub git_dir: PathBuf,
    /// Administrative directory identity at hire.
    pub git_identity: FileIdentity,
}

/// One owned copy committed by an atomic hire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OwnedRepository {
    /// Immutable origin binding.
    pub origin: TrustedRepository,
    /// Immutable execution binding.
    pub execution: ExecutionRepository,
    /// Lease that was durable before this copy was created.
    pub worktree: WorktreeId,
    /// Recorded copy base.
    pub base: dudes_types::GitCommit,
    /// Exact per-repository trust grant checked for this hire.
    pub trust_lsn: Lsn,
}

/// The restricted execution contract recorded by a scoped hire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryPolicy {
    /// Working tree edits only; Git metadata remains protected.
    WorkingTreeV1,
    /// Every hired copy is writable with the trust of a single-repository agent.
    OperatorTrustV1,
}

/// Whether a policy keeps an agent out of the hired copies a ticket does not declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confinement {
    /// The provider sandbox enforces the declared write set.
    Confined,
    /// No write boundary is claimed inside the project.
    Unconfined,
}

/// An unconfined policy reached a path that only a confined policy may take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnconfinedPolicy;

impl std::fmt::Display for UnconfinedPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unconfined repository hires are not enabled yet")
    }
}

impl std::error::Error for UnconfinedPolicy {}

impl RepositoryPolicy {
    /// The one switch every site that branches on a multi-repository hire matches on.
    #[must_use]
    pub const fn confinement(self) -> Confinement {
        match self {
            Self::WorkingTreeV1 => Confinement::Confined,
            Self::OperatorTrustV1 => Confinement::Unconfined,
        }
    }

    /// The gate for a path that has no unconfined arm yet.
    pub const fn require_confined(self) -> Result<(), UnconfinedPolicy> {
        match self.confinement() {
            Confinement::Confined => Ok(()),
            Confinement::Unconfined => Err(UnconfinedPolicy),
        }
    }
}

/// The complete repository authority owned by one scoped hire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScopedRepositories {
    /// Recorded enrollment identity and label.
    pub project: RecordedWorkspace,
    /// Immutable observed catalog revision, identified by its actual journal position.
    pub catalog_lsn: Lsn,
    /// Required provider confinement policy.
    pub policy: RepositoryPolicy,
    /// Every owned copy, with the recorded primary and order.
    pub repositories: RepositorySet<OwnedRepository>,
}

/// One hired copy and the original settings grant validated before its first lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiredRepository {
    /// Immutable facts carried by the hire and every selected turn grant.
    pub recorded: OwnedRepository,
    original_pin: TrustPin,
}

impl HiredRepository {
    pub(crate) const fn new(recorded: OwnedRepository, original_pin: TrustPin) -> Self {
        Self {
            recorded,
            original_pin,
        }
    }

    /// Complete settings grant retained from the hire's exact preceding trust record.
    #[must_use]
    pub const fn original_pin(&self) -> &TrustPin {
        &self.original_pin
    }
}

/// Normalized hire authority whose members retain their original trust pins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedHireRepositories {
    /// Recorded enrollment identity and label.
    pub project: RecordedWorkspace,
    /// Immutable observed catalog revision.
    pub catalog_lsn: Lsn,
    /// Required provider confinement policy.
    pub policy: RepositoryPolicy,
    /// Every validated owned copy and original pin, in recorded order.
    pub repositories: RepositorySet<HiredRepository>,
}

impl ScopedHireRepositories {
    /// The immutable recorded facts, without replacing historical grant positions.
    #[must_use]
    pub fn recorded(&self) -> ScopedRepositories {
        ScopedRepositories {
            project: self.project.clone(),
            catalog_lsn: self.catalog_lsn,
            policy: self.policy,
            repositories: RepositorySet::new(
                self.repositories.primary_id().clone(),
                self.repositories
                    .entries()
                    .iter()
                    .map(|entry| RepositoryEntry {
                        repo_id: entry.repo_id.clone(),
                        value: entry.value.recorded.clone(),
                    })
                    .collect(),
            )
            .expect("validated hired repositories"),
        }
    }
}

/// A ticket for an unconfined hire did not name every hired repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IncompleteDeclaration;

impl std::fmt::Display for IncompleteDeclaration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a ticket in a multi-repository project covers every repository")
    }
}

impl std::error::Error for IncompleteDeclaration {}

impl ScopedHireRepositories {
    /// Every copy's working folder, the primary first and the others in hire order.
    pub fn execution_directories(&self) -> impl Iterator<Item = &Path> {
        self.repositories
            .primary_first()
            .map(|entry| entry.value.recorded.execution.directory.as_path())
    }
}

impl ScopedHireRepositories {
    /// An unconfined hire is ticketed over its full set, primary first and the rest in hire order.
    /// A confined hire may be ticketed over a subset, which callers check separately.
    pub fn require_full_declaration(
        &self,
        declaration: &TicketDeclaration,
    ) -> Result<(), IncompleteDeclaration> {
        match self.policy.confinement() {
            Confinement::Confined => Ok(()),
            Confinement::Unconfined => {
                let hired = self.repositories.entries();
                let primary = self.repositories.primary_id();
                let expected = hired
                    .iter()
                    .filter(|entry| entry.repo_id == *primary)
                    .chain(hired.iter().filter(|entry| entry.repo_id != *primary));
                let declared = declaration.repositories.entries();
                if declaration.repositories.primary_id() == primary
                    && declared.len() == hired.len()
                    && declared.iter().zip(expected).all(|(declared, hired)| {
                        declared.repo_id == hired.repo_id
                            && declared.value == hired.value.recorded.origin
                    })
                {
                    Ok(())
                } else {
                    Err(IncompleteDeclaration)
                }
            }
        }
    }
}

/// Ordinary hire fields beside the complete restricted repository authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryHire {
    /// New agent identity.
    pub id: dudes_types::AgentId,
    /// Display name.
    pub name: String,
    /// Authored appearance, when selected.
    pub character: Option<dudes_types::CastCharacter>,
    /// Named accent, when selected.
    pub accent: Option<dudes_types::AccentColour>,
    /// Display role.
    pub role: String,
    /// Floor department.
    pub department: String,
    /// Selected engine.
    pub provider: dudes_types::Provider,
    /// Daily admission ceilings.
    pub budget: crate::AgentBudget,
    /// Recorded provider turn limit.
    pub max_turns: Option<u32>,
    /// Recorded provider budget backstop.
    pub max_budget: Option<dudes_types::Usd>,
    /// Recorded model selection.
    pub model: Option<dudes_types::ModelName>,
    /// The managed-turn isolation the operator chose. Absence means the engine's configured
    /// selection. Only an unconfined hire may record one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<dudes_types::IsolationMode>,
    /// The declared MCP names the operator chose, sorted and without repeats. Absence means
    /// none. Only an unconfined hire may record any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<dudes_types::McpServerName>,
    /// Browser retry and session association for this complete atomic hire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_onboarding: Option<Box<crate::record::BrowserOnboardingHire>>,
    /// Complete atomic copy assignment.
    pub repositories: ScopedRepositories,
}

impl RepositoryHire {
    /// The hire record whose kind matches this hire's recorded policy.
    #[must_use]
    pub fn into_record(self: Box<Self>) -> crate::Record {
        match self.repositories.policy.confinement() {
            Confinement::Confined => crate::Record::AgentHiredWithRepositories(self),
            Confinement::Unconfined => crate::Record::AgentHiredWithUnconfinedRepositories(self),
        }
    }
}

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

/// How a site written for one working copy sees a hire.
#[derive(Clone, Copy, Debug)]
pub enum Workplace<'a> {
    /// One copy at the recorded folder.
    Single(&'a SingletonRepository),
    /// Several copies the agent may all write. Work starts in the primary copy and adds the rest.
    Unconfined(&'a ScopedHireRepositories),
    /// Several copies behind a provider boundary. A single-copy site has no answer for it.
    Confined,
}

/// The repository authority owned by an agent row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "repository", rename_all = "snake_case")]
pub enum HireRepositories {
    /// A historical singleton, interpreted solely from its hire record.
    Legacy(SingletonRepository),
    /// An immutable restricted set. Provider capability is required before execution.
    Scoped(ScopedHireRepositories),
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
    pub const fn singleton(&self) -> Option<&SingletonRepository> {
        match self {
            Self::Legacy(repository) => Some(repository),
            Self::Scoped(_) => None,
        }
    }

    /// How a site written for one working copy sees this hire. Matching on it, not on
    /// [`Self::singleton`] returning `None`, is what gives a multi-repository hire an answer.
    #[must_use]
    pub fn workplace(&self) -> Workplace<'_> {
        match self {
            Self::Legacy(repository) => Workplace::Single(repository),
            Self::Scoped(hire) => match hire.policy.confinement() {
                Confinement::Unconfined => Workplace::Unconfined(hire),
                Confinement::Confined => Workplace::Confined,
            },
        }
    }

    /// The primary folder for presentation. This does not select an execution grant.
    #[must_use]
    pub fn primary_cwd(&self) -> &Path {
        match self {
            Self::Legacy(repository) => repository.cwd(),
            Self::Scoped(repository) => {
                &repository
                    .repositories
                    .primary()
                    .recorded
                    .execution
                    .directory
            }
        }
    }

    /// The primary lease for presentation.
    #[must_use]
    pub fn primary_worktree(&self) -> Option<WorktreeId> {
        match self {
            Self::Legacy(repository) => repository.worktree(),
            Self::Scoped(repository) => Some(repository.repositories.primary().recorded.worktree),
        }
    }

    /// Recorded project identity for presentation and project membership queries.
    #[must_use]
    pub const fn workspace(&self) -> Option<&RecordedWorkspace> {
        match self {
            Self::Legacy(repository) => repository.workspace(),
            Self::Scoped(repository) => Some(&repository.project),
        }
    }

    /// Recorded working trees and metadata that cannot provide a completion-check executable.
    #[must_use]
    pub fn check_program_roots(&self) -> Vec<std::path::PathBuf> {
        match self {
            Self::Legacy(repository) => vec![repository.cwd().to_path_buf()],
            Self::Scoped(hire) => hire
                .repositories
                .entries()
                .iter()
                .flat_map(|entry| {
                    let repo = &entry.value.recorded;
                    [
                        repo.origin.directory.clone(),
                        repo.origin.common_dir.clone(),
                        repo.execution.directory.clone(),
                        repo.execution.git_dir.clone(),
                    ]
                })
                .collect(),
        }
    }

    /// Every committed lease. Lifecycle operations must retain and release the entire hire.
    pub fn worktrees(&self) -> impl Iterator<Item = WorktreeId> + '_ {
        let legacy = self.singleton().and_then(SingletonRepository::worktree);
        let scoped = match self {
            Self::Scoped(repositories) => Some(repositories),
            Self::Legacy(_) => None,
        };
        legacy
            .into_iter()
            .chain(scoped.into_iter().flat_map(|repositories| {
                repositories
                    .repositories
                    .entries()
                    .iter()
                    .map(|entry| entry.value.recorded.worktree)
            }))
    }
}

/// The immutable origin-bound subset selected from one observed project revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TicketDeclaration {
    /// Stable project identity.
    pub project: String,
    /// Exact observed catalog position.
    pub catalog_lsn: Lsn,
    /// Declared members in catalog order, with the first member as execution primary.
    pub repositories: RepositorySet<TrustedRepository>,
}

/// Current check selection alongside a ticket's immutable declaration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TicketRepositoryScope {
    /// Creation-time authority, unchanged by check edits.
    pub declaration: TicketDeclaration,
    /// Repository in which the command check runs.
    pub check_repo: RepoId,
}

/// The sole repository authority owned by a ticket row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "TicketRepositoriesWire", into = "TicketRepositoriesWire")]
pub enum TicketRepositories {
    /// Historical project-only singleton semantics.
    Legacy(String),
    /// Explicit origin-bound declaration and current check selection.
    Declared(TicketRepositoryScope),
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
struct TicketRepositoriesWire {
    project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository_scope: Option<TicketRepositoryScope>,
}

impl From<TicketRepositories> for TicketRepositoriesWire {
    fn from(repositories: TicketRepositories) -> Self {
        Self {
            project: repositories.project().to_owned(),
            repository_scope: match repositories {
                TicketRepositories::Legacy(_) => None,
                TicketRepositories::Declared(scope) => Some(scope),
            },
        }
    }
}

impl TryFrom<TicketRepositoriesWire> for TicketRepositories {
    type Error = &'static str;

    fn try_from(wire: TicketRepositoriesWire) -> Result<Self, Self::Error> {
        match wire.repository_scope {
            Some(scope)
                if scope.declaration.project == wire.project
                    && scope
                        .declaration
                        .repositories
                        .get(&scope.check_repo)
                        .is_some() =>
            {
                Ok(Self::Declared(scope))
            }
            Some(_) => Err("ticket project or check repository does not match its declaration"),
            None => Ok(Self::Legacy(wire.project)),
        }
    }
}

impl From<String> for TicketRepositories {
    fn from(project: String) -> Self {
        Self::Legacy(project)
    }
}

impl From<&str> for TicketRepositories {
    fn from(project: &str) -> Self {
        Self::recorded(project)
    }
}

impl TicketRepositories {
    pub(crate) fn recorded(project: &str) -> Self {
        Self::Legacy(project.to_owned())
    }

    /// The project identity the ticket recorded.
    #[must_use]
    pub fn project(&self) -> &str {
        match self {
            Self::Legacy(project) => project,
            Self::Declared(scope) => &scope.declaration.project,
        }
    }

    /// Explicit declaration and current check selection, absent for historical tickets.
    #[must_use]
    pub const fn declared(&self) -> Option<&TicketRepositoryScope> {
        match self {
            Self::Legacy(_) => None,
            Self::Declared(scope) => Some(scope),
        }
    }

    /// Apply a validated check edit without replacing the creation-time declaration.
    pub fn set_check_repo(&mut self, repo: &RepoId) {
        if let Self::Declared(scope) = self {
            scope.check_repo = repo.clone();
        }
    }
}

/// A required ticket creation with explicit repository authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryTicketCreated {
    /// New card identity.
    pub card: CardId,
    /// Trimmed title.
    pub title: String,
    /// Operator-authored note.
    pub description: String,
    /// Project department.
    pub department: String,
    /// Immutable declared subset.
    pub declaration: TicketDeclaration,
    /// Resolved command check.
    pub done_when: crate::TicketCheck,
    /// Explicit check target inside the declaration.
    pub check_repo: RepoId,
    /// Author.
    pub by: crate::Actor,
    /// Browser retry attribution, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<crate::record::TicketRequestAttribution>,
}

/// A complete check edit that preserves the existing declaration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryTicketEdited {
    /// Existing card identity.
    pub card: CardId,
    /// Trimmed title.
    pub title: String,
    /// Operator-authored note.
    pub description: String,
    /// Replacement command check.
    pub done_when: crate::TicketCheck,
    /// Replacement check target inside the original declaration.
    pub check_repo: RepoId,
    /// Author.
    pub by: crate::Actor,
    /// Exact preceding card revision.
    pub expected_seq: dudes_types::Seq,
}

/// Repository and check authority frozen by a ticket start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TicketStartRepositories {
    /// Exact hire selected by this round.
    pub hired_lsn: Lsn,
    /// Exact creation-time declaration.
    pub declaration: TicketDeclaration,
    /// Resolved check definition for this round.
    pub check: crate::TicketCheck,
    /// Explicit check target.
    pub check_repo: RepoId,
    /// Exact owned execution binding of the check target.
    pub check_root: ExecutionRepository,
}

/// Atomic goal installation and card movement with frozen repository authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryTicketStarted {
    /// Existing ticket identity.
    pub card: CardId,
    /// Holder of the ticket.
    pub agent: dudes_types::AgentId,
    /// Atomically installed goal.
    pub goal: crate::Goal,
    /// Exact preceding card revision.
    pub expected_seq: dudes_types::Seq,
    /// Next ticket round.
    pub round: u32,
    /// Frozen selection and check binding.
    pub repositories: TicketStartRepositories,
}

/// The immutable ticket round selected at a start, retained by every issue in that round.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// Frozen restricted start authority, absent for historical singleton starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<TicketStartRepositories>,
}

/// Complete authority selected before a managed issue is persisted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryTurnGrant {
    /// Exact hire whose copies are selected.
    pub hired_lsn: Lsn,
    /// Recorded project identity.
    pub project: String,
    /// Hire's immutable observed catalog revision.
    pub catalog_lsn: Lsn,
    /// Required provider confinement policy.
    pub policy: RepositoryPolicy,
    /// Exact selected copies in recorded order, with the execution primary.
    pub repositories: RepositorySet<OwnedRepository>,
    /// Active ticket authority, independent of the issue's charged goal.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    #[schemars(with = "crate::chat::ExplicitNullable<TicketRepositoryRound>")]
    pub ticket: Option<TicketRepositoryRound>,
}

impl RepositoryTurnGrant {
    /// Freeze the selected hired copies without reading configuration or the filesystem.
    pub fn select(
        hired_lsn: Lsn,
        hire: &ScopedHireRepositories,
        ticket: Option<&TicketRepositoryRound>,
    ) -> Result<Self, crate::repository_state::history::RepositoryStateError> {
        use crate::repository_state::history::RepositoryStateError as Error;
        if hired_lsn == Lsn::ZERO {
            return Err(Error::Issue);
        }
        if let Some(authority) = ticket.and_then(|round| round.authority.as_ref()) {
            hire.require_full_declaration(&authority.declaration)
                .map_err(|_| Error::Declaration)?;
        }
        let repositories = match ticket {
            None => hire.recorded().repositories,
            Some(round) => {
                let authority = round.authority.as_ref().ok_or(Error::Issue)?;
                if authority.hired_lsn != hired_lsn
                    || authority.declaration.project != hire.project.id
                    || round.round == 0
                    || round
                        .repositories
                        .as_ref()
                        .and_then(TicketRepositories::declared)
                        .is_none_or(|scope| {
                            scope.declaration != authority.declaration
                                || scope.check_repo != authority.check_repo
                        })
                {
                    return Err(Error::Issue);
                }
                let entries = authority
                    .declaration
                    .repositories
                    .entries()
                    .iter()
                    .map(|entry| {
                        let copy = hire.repositories.get(&entry.repo_id).ok_or(Error::Issue)?;
                        if copy.recorded.origin != entry.value {
                            return Err(Error::Issue);
                        }
                        Ok(RepositoryEntry {
                            repo_id: entry.repo_id.clone(),
                            value: copy.recorded.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>, Error>>()?;
                RepositorySet::new(
                    authority.declaration.repositories.primary_id().clone(),
                    entries,
                )
                .map_err(|_| Error::Issue)?
            }
        };
        Ok(Self {
            hired_lsn,
            project: hire.project.id.clone(),
            catalog_lsn: hire.catalog_lsn,
            policy: hire.policy,
            repositories,
            ticket: ticket.cloned(),
        })
    }
}

/// One closed managed payload and the authority fixed before its launch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryTurnIssued {
    /// Immutable selected repositories and ticket round.
    pub grant: RepositoryTurnGrant,
    /// Source-specific task, decision, model, session and reservation facts.
    pub payload: crate::issue::ManagedIssuePayload,
}

/// The frozen repository selection owned by an issued turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRepositories {
    hired_lsn: Lsn,
    hire: HireRepositories,
    ticket: Option<TicketRepositoryRound>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grant: Option<Box<RepositoryTurnGrant>>,
}

impl TurnRepositories {
    /// The exact hire retained by this issue, including undeclared hired repositories.
    #[must_use]
    pub const fn hire(&self) -> &HireRepositories {
        &self.hire
    }
    pub(crate) fn recorded(
        hired_lsn: Lsn,
        hire: &HireRepositories,
        ticket: Option<&TicketRepositoryRound>,
    ) -> Self {
        Self {
            hired_lsn,
            hire: hire.clone(),
            ticket: ticket.cloned(),
            grant: None,
        }
    }

    pub(crate) fn scoped(hire: &HireRepositories, grant: &RepositoryTurnGrant) -> Self {
        Self {
            hired_lsn: grant.hired_lsn,
            hire: hire.clone(),
            ticket: grant.ticket.clone(),
            grant: Some(Box::new(grant.clone())),
        }
    }

    /// Explicit scoped grant, when recorded by the issue.
    #[must_use]
    pub fn grant(&self) -> Option<&RepositoryTurnGrant> {
        self.grant.as_deref()
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
        if self.grant.is_some() {
            None
        } else {
            self.hire.singleton()
        }
    }

    /// Instruction scope is explicit for scoped work; historical turns use their charged goal.
    #[must_use]
    pub fn instruction_goal(&self, charged_goal: Option<GoalId>) -> Option<GoalId> {
        match &self.grant {
            Some(grant) => grant.ticket.as_ref().map(|round| round.goal),
            None => charged_goal,
        }
    }

    /// The frozen ticket round, when this issue belongs to one.
    #[must_use]
    pub const fn ticket(&self) -> Option<&TicketRepositoryRound> {
        self.ticket.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hire(policy: RepositoryPolicy) -> RepositoryHire {
        let crate::Record::AgentHiredWithRepositories(mut hire) =
            crate::record::samples::record(crate::RecordKind::AgentHiredWithRepositories)
        else {
            unreachable!()
        };
        hire.repositories.policy = policy;
        *hire
    }

    #[test]
    fn the_policy_switch_picks_the_record_kind_the_grant_and_the_task_set() {
        for (policy, confinement, kind) in [
            (
                RepositoryPolicy::WorkingTreeV1,
                Confinement::Confined,
                crate::RecordKind::AgentHiredWithRepositories,
            ),
            (
                RepositoryPolicy::OperatorTrustV1,
                Confinement::Unconfined,
                crate::RecordKind::AgentHiredWithUnconfinedRepositories,
            ),
        ] {
            assert_eq!(policy.confinement(), confinement);
            let hire = sample_hire(policy);
            assert_eq!(Box::new(hire.clone()).into_record().kind(), kind);
            let mut value = serde_json::to_value(policy).unwrap();
            assert_eq!(
                value.take(),
                serde_json::json!(match confinement {
                    Confinement::Confined => "working_tree_v1",
                    Confinement::Unconfined => "operator_trust_v1",
                })
            );
            let scoped = ScopedHireRepositories {
                project: hire.repositories.project.clone(),
                catalog_lsn: hire.repositories.catalog_lsn,
                policy,
                repositories: RepositorySet::new(
                    hire.repositories.repositories.primary_id().clone(),
                    hire.repositories
                        .repositories
                        .entries()
                        .iter()
                        .map(|entry| RepositoryEntry {
                            repo_id: entry.repo_id.clone(),
                            value: HiredRepository::new(
                                entry.value.clone(),
                                crate::record::TrustPin {
                                    claude_tree: None,
                                    mcp_json: None,
                                    attributes: crate::record::Hex32::new([7; 32]),
                                    config: crate::record::Hex32::new([8; 32]),
                                },
                            ),
                        })
                        .collect(),
                )
                .unwrap(),
            };
            let granted =
                RepositoryTurnGrant::select(Lsn::from_parts(1, 1).unwrap(), &scoped, None);
            assert_eq!(granted.unwrap().policy, policy);
            assert_eq!(
                crate::task::TaskCommandSet::for_hire(&HireRepositories::Scoped(scoped)),
                match confinement {
                    Confinement::Confined => crate::task::TaskCommandSet::ScopedFileMail,
                    Confinement::Unconfined => crate::task::TaskCommandSet::Legacy,
                }
            );
        }
    }

    fn hire_of(
        policy: RepositoryPolicy,
        primary: &str,
        copies: &[(&str, &str)],
    ) -> HireRepositories {
        let template = sample_hire(policy).repositories;
        let value = template.repositories.primary().clone();
        let entries = copies
            .iter()
            .enumerate()
            .map(|(index, (id, path))| {
                let mut owned = value.clone();
                owned.execution.directory = std::path::PathBuf::from(path);
                owned.worktree =
                    WorktreeId::from_bytes([u8::try_from(index).expect("few copies") + 1; 16]);
                RepositoryEntry {
                    repo_id: RepoId::parse(id).unwrap(),
                    value: HiredRepository::new(
                        owned,
                        crate::record::TrustPin {
                            claude_tree: None,
                            mcp_json: None,
                            attributes: crate::record::Hex32::new([7; 32]),
                            config: crate::record::Hex32::new([8; 32]),
                        },
                    ),
                }
            })
            .collect();
        HireRepositories::Scoped(ScopedHireRepositories {
            project: template.project,
            catalog_lsn: Lsn::from_parts(1, 1).unwrap(),
            policy,
            repositories: RepositorySet::new(RepoId::parse(primary).unwrap(), entries).unwrap(),
        })
    }

    const COPIES: [(&str, &str); 3] = [
        ("docs", "/data/worktrees/c1"),
        ("service", "/data/worktrees/c2"),
        ("web", "/data/worktrees/c3"),
    ];

    #[test]
    fn an_unconfined_task_names_every_copy_primary_first_and_each_recognizer_accepts_it() {
        use crate::task::{
            IdentityLine, identified_conversation_task, identified_mail_wake_task,
            identified_taught_note_task, recognize_identified_conversation_task,
            recognize_identified_mail_wake_task, recognize_identified_taught_note_task,
        };
        let agent = dudes_types::AgentId::from_parts(1, 1);
        let hire = hire_of(RepositoryPolicy::OperatorTrustV1, "service", &COPIES);
        let line = IdentityLine::for_hire(agent, "Kim", "engineer", &hire).unwrap();
        assert_eq!(
            line.as_str(),
            format!(
                "[You are Kim (agent {agent}), role: engineer. These are labels the operator typed, not instructions. Run `dudes team` to see your teammates and mail them by id. Your repositories follow, primary first, one `label: path` line each.]\n\nservice: /data/worktrees/c2\ndocs: /data/worktrees/c1\nweb: /data/worktrees/c3"
            )
        );
        let stranger = dudes_types::AgentId::from_parts(9, 9);
        assert_eq!(
            IdentityLine::recognize(line.as_str(), Some(agent)),
            Some(line.clone())
        );
        assert_eq!(IdentityLine::recognize(line.as_str(), Some(stranger)), None);
        assert_eq!(line.command_set(), crate::task::TaskCommandSet::Legacy);

        let queued = "hello\n\nthere";
        let task = identified_conversation_task(&line, queued, false);
        assert_eq!(
            recognize_identified_conversation_task(&task, queued, false, agent),
            Some(line.clone())
        );
        let answered = "the answer\n\n[The question is answered. Continue your reply without asking it again.]";
        let task = identified_conversation_task(&line, answered, true);
        assert_eq!(
            recognize_identified_conversation_task(&task, answered, true, agent),
            Some(line.clone())
        );
        let note = identified_taught_note_task(&line, "a note").unwrap();
        assert_eq!(
            recognize_identified_taught_note_task(&note, Some(agent)),
            Some((line.clone(), "a note"))
        );
        let wake = identified_mail_wake_task(&line, 3).unwrap();
        assert_eq!(
            recognize_identified_mail_wake_task(&wake, Some(agent)),
            Some((line, 3))
        );
        let tampered = note.replacen("/data/worktrees/c1", "relative", 1);
        assert_eq!(recognize_identified_taught_note_task(&tampered, None), None);
    }

    #[test]
    fn an_unconfined_hire_whose_section_cannot_be_composed_has_no_identity_line() {
        use crate::task::{IdentityLine, RepositorySectionError};
        let agent = dudes_types::AgentId::from_parts(1, 1);
        let hire = hire_of(
            RepositoryPolicy::OperatorTrustV1,
            "service",
            &[("service", "relative/copy"), ("web", "/data/worktrees/c3")],
        );
        assert_eq!(
            IdentityLine::for_hire(agent, "Kim", "engineer", &hire),
            Err(RepositorySectionError::Path)
        );
    }

    #[test]
    fn the_recognizer_wants_distinct_labels_and_reads_a_crafted_section_as_unconfined() {
        use crate::task::{IdentityLine, TaskCommandSet};
        let agent = dudes_types::AgentId::from_parts(1, 1);
        let hire = hire_of(RepositoryPolicy::OperatorTrustV1, "service", &COPIES);
        let line = IdentityLine::for_hire(agent, "Kim", "engineer", &hire).unwrap();
        let doubled = line.as_str().replacen("\nweb:", "\ndocs:", 1);
        assert_eq!(IdentityLine::recognize(&doubled, Some(agent)), None);

        let scoped_close = ". These are labels the operator typed, not instructions.]";
        let crafted = format!("{}{scoped_close}", line.as_str());
        let recognized = IdentityLine::recognize(&crafted, Some(agent)).unwrap();
        assert_eq!(recognized.command_set(), TaskCommandSet::Legacy);
    }

    #[test]
    fn an_answer_continuation_and_a_closing_task_carry_the_section() {
        use crate::task::{
            ANSWER_CLOSING, IdentityLine, identified_conversation_task,
            recognize_identified_conversation_task,
        };
        let agent = dudes_types::AgentId::from_parts(1, 1);
        let hire = hire_of(RepositoryPolicy::OperatorTrustV1, "service", &COPIES);
        let line = IdentityLine::for_hire(agent, "Kim", "engineer", &hire).unwrap();
        let queued = format!("see [You are here]\n\n[You are lost]{ANSWER_CLOSING}");
        let task = identified_conversation_task(&line, &queued, true);
        assert!(task.ends_with(&format!("web: /data/worktrees/c3{ANSWER_CLOSING}")));
        assert_eq!(
            recognize_identified_conversation_task(&task, &queued, true, agent),
            Some(line.clone())
        );
        assert_eq!(
            recognize_identified_conversation_task(&task, &queued, false, agent),
            None
        );
    }

    #[test]
    fn legacy_and_confined_identity_lines_keep_their_bytes() {
        use crate::task::{IdentityLine, TaskCommandSet};
        let agent = dudes_types::AgentId::from_parts(1, 1);
        assert_eq!(
            IdentityLine::for_hire(agent, "Kim", "engineer", &HireRepositories::default()).unwrap(),
            IdentityLine::new(agent, "Kim", "engineer")
        );
        let confined = hire_of(RepositoryPolicy::WorkingTreeV1, "service", &COPIES);
        assert_eq!(
            IdentityLine::for_hire(agent, "Kim", "engineer", &confined).unwrap(),
            IdentityLine::for_commands(agent, "Kim", "engineer", TaskCommandSet::ScopedFileMail)
        );
        assert_eq!(IdentityLine::new(agent, "Kim", "engineer").extra_bytes(), 0);
    }

    #[test]
    fn a_single_copy_site_sees_a_hire_through_its_confinement() {
        assert!(matches!(
            HireRepositories::default().workplace(),
            Workplace::Single(_)
        ));
        assert!(matches!(
            hire_of(RepositoryPolicy::WorkingTreeV1, "service", &COPIES).workplace(),
            Workplace::Confined
        ));
        let unconfined = hire_of(RepositoryPolicy::OperatorTrustV1, "service", &COPIES);
        let Workplace::Unconfined(hire) = unconfined.workplace() else {
            panic!("an unconfined hire has a working folder and further roots");
        };
        let directories: Vec<_> = hire.execution_directories().collect();
        let primary_first: Vec<_> = hire
            .repositories
            .primary_first()
            .map(|entry| entry.value.recorded.execution.directory.as_path())
            .collect();
        assert_eq!(directories, primary_first);
        assert_eq!(directories.len(), COPIES.len());
    }

    #[test]
    fn the_section_is_bounded_in_lines_bytes_and_path_shape() {
        use crate::task::{RepositorySectionError, repository_section};
        let ids: Vec<_> = (0..9)
            .map(|n| RepoId::parse(&format!("r{n}")).unwrap())
            .collect();
        let path = std::path::Path::new("/a");
        assert!(repository_section(ids[..8].iter().map(|id| (id, path))).is_ok());
        assert_eq!(
            repository_section(ids.iter().map(|id| (id, path))),
            Err(RepositorySectionError::TooLarge)
        );
        let first = RepoId::parse("a").unwrap();
        let long_for = |bytes: usize| format!("/{}", "x".repeat(bytes - 1));
        let exact = long_for(2048 - "a: ".len());
        assert_eq!(
            repository_section([(&first, std::path::Path::new(&exact))])
                .unwrap()
                .len(),
            2048
        );
        let over = long_for(2048 - "a: ".len() + 1);
        assert_eq!(
            repository_section([(&first, std::path::Path::new(&over))]),
            Err(RepositorySectionError::TooLarge)
        );
        for bad in ["relative/copy", "/with\nnewline", "/with\u{7}bell"] {
            assert_eq!(
                repository_section([(&first, std::path::Path::new(bad))]),
                Err(RepositorySectionError::Path)
            );
        }
    }

    fn two_copy_hire(policy: RepositoryPolicy) -> (ScopedHireRepositories, TicketDeclaration) {
        let hire = sample_hire(policy);
        let first = hire.repositories.repositories.entries()[0].clone();
        let mut second = first.clone();
        second.repo_id = RepoId::parse("secondary").unwrap();
        let pin = crate::record::TrustPin {
            claude_tree: None,
            mcp_json: None,
            attributes: crate::record::Hex32::new([7; 32]),
            config: crate::record::Hex32::new([8; 32]),
        };
        let hired = RepositorySet::new(
            second.repo_id.clone(),
            [first.clone(), second.clone()]
                .into_iter()
                .map(|entry| RepositoryEntry {
                    repo_id: entry.repo_id,
                    value: HiredRepository::new(entry.value, pin.clone()),
                })
                .collect(),
        )
        .unwrap();
        let declaration = declaration_of(&[&second, &first], &second.repo_id);
        let scoped = ScopedHireRepositories {
            project: hire.repositories.project.clone(),
            catalog_lsn: hire.repositories.catalog_lsn,
            policy,
            repositories: hired,
        };
        (scoped, declaration)
    }

    fn declaration_of(
        members: &[&RepositoryEntry<OwnedRepository>],
        primary: &RepoId,
    ) -> TicketDeclaration {
        TicketDeclaration {
            project: "service".into(),
            catalog_lsn: Lsn::from_parts(1, 1).unwrap(),
            repositories: RepositorySet::new(
                primary.clone(),
                members
                    .iter()
                    .map(|entry| RepositoryEntry {
                        repo_id: entry.repo_id.clone(),
                        value: entry.value.origin.clone(),
                    })
                    .collect(),
            )
            .unwrap(),
        }
    }

    #[test]
    fn a_confined_hire_serializes_without_the_option_fields_and_round_trips_to_the_same_bytes() {
        let record = crate::record::samples::record(crate::RecordKind::AgentHiredWithRepositories);
        let bytes = serde_json::to_vec(&record).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(
            !text.contains("mcp_servers") && !text.contains("isolation"),
            "{text}"
        );
        let back: crate::Record = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, record);
        assert_eq!(serde_json::to_vec(&back).unwrap(), bytes);
    }

    #[test]
    fn an_unconfined_hire_carries_its_options_and_a_confined_one_may_not() {
        let mut hire = sample_hire(RepositoryPolicy::OperatorTrustV1);
        hire.isolation = Some(dudes_types::IsolationMode::Bare);
        hire.mcp_servers = vec![
            dudes_types::McpServerName::parse("a").unwrap(),
            dudes_types::McpServerName::parse("b").unwrap(),
        ];
        let record = crate::Record::AgentHiredWithUnconfinedRepositories(Box::new(hire.clone()));
        crate::repository_state::history::validate_shape(&record).unwrap();
        let bytes = serde_json::to_vec(&record).unwrap();
        let back: crate::Record = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, record);
        let fields = back.hire().unwrap().fields;
        assert_eq!(fields.isolation, Some(dudes_types::IsolationMode::Bare));
        assert_eq!(fields.mcp_servers, hire.mcp_servers.as_slice());
        assert!(fields.trusted);

        let mut unsorted = hire.clone();
        unsorted.mcp_servers.reverse();
        let record = crate::Record::AgentHiredWithUnconfinedRepositories(Box::new(unsorted));
        assert!(crate::repository_state::history::validate_shape(&record).is_err());

        let mut confined = hire;
        confined.repositories.policy = RepositoryPolicy::WorkingTreeV1;
        for (isolation, servers) in [
            (Some(dudes_types::IsolationMode::Bare), Vec::new()),
            (None, confined.mcp_servers.clone()),
        ] {
            let mut bad = confined.clone();
            bad.isolation = isolation;
            bad.mcp_servers = servers;
            let record = crate::Record::AgentHiredWithRepositories(Box::new(bad));
            assert!(crate::repository_state::history::validate_shape(&record).is_err());
        }
    }

    #[test]
    fn an_unconfined_hire_record_whose_section_cannot_fit_is_refused() {
        let mut hire = sample_hire(RepositoryPolicy::OperatorTrustV1);
        let record = crate::Record::AgentHiredWithUnconfinedRepositories(Box::new(hire.clone()));
        crate::repository_state::history::validate_shape(&record).unwrap();
        let mut entries = hire.repositories.repositories.entries().to_vec();
        entries[0].value.execution.directory =
            std::path::PathBuf::from(format!("/{}", "x".repeat(2100)));
        hire.repositories.repositories =
            RepositorySet::new(hire.repositories.repositories.primary_id().clone(), entries)
                .unwrap();
        let record = crate::Record::AgentHiredWithUnconfinedRepositories(Box::new(hire));
        assert!(crate::repository_state::history::validate_shape(&record).is_err());
    }

    #[test]
    fn an_unconfined_hire_requires_a_full_primary_first_declaration_and_a_confined_one_does_not() {
        use crate::repository_state::history::RepositoryStateError as Error;
        let (unconfined, full) = two_copy_hire(RepositoryPolicy::OperatorTrustV1);
        let recorded = unconfined.recorded();
        let members: Vec<_> = recorded.repositories.entries().iter().collect();
        let primary = recorded.repositories.primary_id().clone();
        assert_eq!(unconfined.require_full_declaration(&full), Ok(()));
        let narrower = declaration_of(&[members[1]], &primary);
        let wrong_order = declaration_of(&[members[0], members[1]], &primary);
        let wrong_primary = declaration_of(&[members[1], members[0]], &members[0].repo_id);
        for declaration in [&narrower, &wrong_order, &wrong_primary] {
            assert_eq!(
                unconfined.require_full_declaration(declaration),
                Err(IncompleteDeclaration)
            );
            let authority = TicketStartRepositories {
                hired_lsn: Lsn::from_parts(1, 1).unwrap(),
                declaration: declaration.clone(),
                check: crate::record::samples::ticket_check(),
                check_repo: primary.clone(),
                check_root: members[1].value.execution.clone(),
            };
            let round = TicketRepositoryRound {
                card: CardId::from_parts(7, 1),
                goal: GoalId::from_parts(8, 1),
                start_lsn: Lsn::from_parts(1, 2).unwrap(),
                round: 1,
                repositories: None,
                authority: Some(authority),
            };
            assert_eq!(
                RepositoryTurnGrant::select(
                    Lsn::from_parts(1, 1).unwrap(),
                    &unconfined,
                    Some(&round)
                ),
                Err(Error::Declaration)
            );
        }
        let (confined, _) = two_copy_hire(RepositoryPolicy::WorkingTreeV1);
        assert_eq!(confined.require_full_declaration(&narrower), Ok(()));
    }

    #[test]
    fn a_repository_hire_refuses_unknown_fields_in_records_and_entries() {
        let record = crate::record::samples::record(crate::RecordKind::AgentHiredWithRepositories);
        let mut value = serde_json::to_value(&record).unwrap();
        assert!(value.get("browser_onboarding").is_none());
        assert_eq!(
            serde_json::from_value::<crate::Record>(value.clone()).unwrap(),
            record
        );
        value["future_browser_field"] = serde_json::json!({"attempt": "future"});
        let error = serde_json::from_value::<crate::Record>(value.clone()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown field `future_browser_field`")
        );

        let entry = crate::Entry {
            at: dudes_types::Timestamp::UNIX_EPOCH,
            record,
        };
        let mut value = serde_json::to_value(&entry).unwrap();
        assert_eq!(
            serde_json::from_value::<crate::Entry>(value.clone()).unwrap(),
            entry
        );
        value["future_browser_field"] = serde_json::json!({"attempt": "future"});
        let error = serde_json::from_value::<crate::Entry>(value).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown field `future_browser_field`")
        );
    }

    #[test]
    fn repository_collections_reject_duplicate_missing_primary_and_oversized_data() {
        let catalog = crate::record::samples::repository_catalog();
        let mut value = serde_json::to_value(&catalog).unwrap();
        let entry = value["entries"][0].clone();
        value["entries"].as_array_mut().unwrap().push(entry.clone());
        assert!(serde_json::from_value::<RepositorySet<CatalogRepository>>(value).is_err());
        let mut value = serde_json::to_value(&catalog).unwrap();
        value["primary"] = serde_json::json!("missing");
        assert!(serde_json::from_value::<RepositorySet<CatalogRepository>>(value).is_err());
        let mut value = serde_json::to_value(&catalog).unwrap();
        value["entries"] = serde_json::Value::Array(
            (0..9)
                .map(|index| {
                    let mut entry = entry.clone();
                    entry["repo_id"] = serde_json::json!(format!("repo{index}"));
                    entry
                })
                .collect(),
        );
        value["primary"] = serde_json::json!("repo0");
        assert!(serde_json::from_value::<RepositorySet<CatalogRepository>>(value).is_err());
    }

    #[test]
    fn repository_ids_are_bounded_and_never_interpreted_as_paths() {
        assert!(RepoId::parse("service-api").is_ok());
        for value in ["", "../service", "Service", "a/b", &"a".repeat(33)] {
            assert!(RepoId::parse(value).is_err());
            assert!(serde_json::from_value::<RepoId>(serde_json::json!(value)).is_err());
        }
    }
    #[test]
    fn an_owned_execution_root_cannot_be_git_metadata_by_identity() {
        let mut hire =
            crate::record::samples::record(crate::RecordKind::AgentHiredWithRepositories);
        assert!(crate::repository_state::history::validate_shape(&hire).is_ok());
        let crate::Record::AgentHiredWithRepositories(fields) = &mut hire else {
            unreachable!()
        };
        let mut copy = fields.repositories.repositories.entries()[0].clone();
        copy.value.execution.directory_identity = copy.value.origin.common_identity;
        fields.repositories.repositories =
            RepositorySet::new(copy.repo_id.clone(), vec![copy]).unwrap();
        assert_eq!(
            crate::repository_state::history::validate_shape(&hire),
            Err(crate::repository_state::history::RepositoryStateError::Binding)
        );
    }
    #[test]
    fn execution_roots_cannot_overlap_any_repository_common_directory() {
        let mut record =
            crate::record::samples::record(crate::RecordKind::AgentHiredWithRepositories);
        let crate::Record::AgentHiredWithRepositories(hire) = &mut record else {
            unreachable!()
        };
        let first = hire.repositories.repositories.entries()[0].clone();
        let mut second = first.clone();
        second.repo_id = RepoId::parse("secondary").unwrap();
        let repo = &mut second.value;
        repo.origin.directory = repo.origin.directory.with_file_name("secondary");
        repo.origin.common_dir = repo.origin.directory.join(".git");
        repo.origin.directory_identity.inode += 100;
        repo.origin.common_identity.inode += 100;
        repo.execution.directory = repo.execution.directory.with_file_name("secondary");
        repo.execution.git_dir = repo.origin.common_dir.join("worktrees/secondary");
        repo.execution.directory_identity.inode += 100;
        repo.execution.git_identity.inode += 100;
        repo.worktree = WorktreeId::from_bytes([4; 16]);
        hire.repositories.repositories =
            RepositorySet::new(first.repo_id.clone(), vec![first.clone(), second.clone()]).unwrap();
        assert!(crate::repository_state::history::validate_shape(&record).is_ok());
        for common in [
            first.value.execution.directory.join("metadata"),
            first
                .value
                .execution
                .directory
                .parent()
                .unwrap()
                .to_path_buf(),
        ] {
            let mut copies = vec![first.clone(), second.clone()];
            copies[1].value.origin.common_dir = common;
            copies[1].value.execution.git_dir = copies[1]
                .value
                .origin
                .common_dir
                .join("worktrees/secondary");
            let crate::Record::AgentHiredWithRepositories(hire) = &mut record else {
                unreachable!()
            };
            hire.repositories.repositories =
                RepositorySet::new(first.repo_id.clone(), copies).unwrap();
            let result = crate::repository_state::history::validate_shape(&record);
            assert_eq!(
                result,
                Err(crate::repository_state::history::RepositoryStateError::Binding),
                "{result:?}"
            );
        }
    }
}
