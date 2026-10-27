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
