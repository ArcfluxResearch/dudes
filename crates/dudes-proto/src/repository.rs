//! Recorded repository authority, independent of current enrollment and filesystem state.

use std::path::{Path, PathBuf};

use dudes_types::{CardId, GoalId, Lsn, WorktreeId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::record::{FileIdentity, RecordedWorkspace, TrustedRepository};

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

/// Ordinary hire fields beside the complete restricted repository authority.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// Complete atomic copy assignment.
    pub repositories: ScopedRepositories,
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

/// The repository authority owned by an agent row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "repository", rename_all = "snake_case")]
pub enum HireRepositories {
    /// A historical singleton, interpreted solely from its hire record.
    Legacy(SingletonRepository),
    /// An immutable restricted set. Provider capability is required before execution.
    Scoped(ScopedRepositories),
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

    /// The primary folder for presentation. This does not select an execution grant.
    #[must_use]
    pub fn primary_cwd(&self) -> &Path {
        match self {
            Self::Legacy(repository) => repository.cwd(),
            Self::Scoped(repository) => &repository.repositories.primary().execution.directory,
        }
    }

    /// The primary lease for presentation.
    #[must_use]
    pub fn primary_worktree(&self) -> Option<WorktreeId> {
        match self {
            Self::Legacy(repository) => repository.worktree(),
            Self::Scoped(repository) => Some(repository.repositories.primary().worktree),
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
                    let repo = &entry.value;
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
                    .map(|entry| entry.value.worktree)
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
        hire: &ScopedRepositories,
        ticket: Option<&TicketRepositoryRound>,
    ) -> Result<Self, crate::repository_state::history::RepositoryStateError> {
        use crate::repository_state::history::RepositoryStateError as Error;
        if hired_lsn == Lsn::ZERO {
            return Err(Error::Issue);
        }
        let repositories = match ticket {
            None => hire.repositories.clone(),
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
                        if copy.origin != entry.value {
                            return Err(Error::Issue);
                        }
                        Ok(RepositoryEntry {
                            repo_id: entry.repo_id.clone(),
                            value: copy.clone(),
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

    #[test]
    fn a_repository_hire_refuses_unknown_fields_in_records_and_entries() {
        let record = crate::record::samples::record(crate::RecordKind::AgentHiredWithRepositories);
        let mut value = serde_json::to_value(&record).unwrap();
        assert_eq!(
            serde_json::from_value::<crate::Record>(value.clone()).unwrap(),
            record
        );
        value["browser_onboarding"] = serde_json::json!({"attempt": "future"});
        let error = serde_json::from_value::<crate::Record>(value.clone()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown field `browser_onboarding`")
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
        value["browser_onboarding"] = serde_json::json!({"attempt": "future"});
        let error = serde_json::from_value::<crate::Entry>(value).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown field `browser_onboarding`")
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
