//! Pure, unarmed launch recipes. Their inventories describe plans, not admission evidence.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use dudes_proto::repository::{
    RepoId, RepositoryEntry, RepositoryPolicy, RepositoryTurnGrant, ScopedRepositories,
};
use dudes_types::{Mode, ModelSelection, Provider};
use serde::Serialize;

use crate::adapter::{AgentCtx, ManagedInstall};
use crate::launch::{EnvVar, LaunchSpec};

mod claude;
mod codex;

pub(crate) use claude::compose as compose_claude;
pub(crate) use codex::compose as compose_codex;

/// The native platform profile a future daemon admission must independently establish.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopedPlatform {
    /// Candidate macOS arm64 profile.
    MacOsArm64,
    /// Candidate Linux x86-64 profile.
    LinuxX86_64,
    /// No recipe exists for the supplied platform.
    Unsupported,
}

/// Exact private paths supplied by the daemon, without a filesystem observation claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ScopedPrivatePaths {
    /// Isolated operating-system home, never writable by model tools.
    pub home: PathBuf,
    /// Provider configuration/session home, never writable by model tools.
    pub profile: PathBuf,
    /// Dedicated work scratch.
    pub work: PathBuf,
    /// This agent's file-mail outbox.
    pub outbox: PathBuf,
    /// This agent's private temporary directory.
    pub temp: PathBuf,
}

/// Immutable composition inputs. The caller still has to validate their current identities.
pub struct ScopedLaunchInputs<'a> {
    /// Selection fixed by the issued turn.
    pub grant: &'a RepositoryTurnGrant,
    /// Complete frozen hire, including copies outside the declaration.
    pub hired: &'a ScopedRepositories,
    /// Resolved origin gitdirs, in the hire's recorded order.
    pub origin_git_dirs: &'a [RepositoryEntry<PathBuf>],
    /// Daemon-selected private paths.
    pub private: ScopedPrivatePaths,
    /// Attached execution has no scoped recipe.
    pub mode: Mode,
    /// Proposed native platform profile.
    pub platform: ScopedPlatform,
}

/// Why one path is writable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WritablePurpose {
    /// A selected owned checkout.
    Repository(RepoId),
    /// Private work scratch.
    Work,
    /// Private file-mail outbox.
    Outbox,
    /// Private temporary directory.
    Temporary,
}

/// One explicit write location, without an observed identity claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WritableLocation {
    /// Reason for this grant.
    pub purpose: WritablePurpose,
    /// Exact proposed path.
    pub path: PathBuf,
}

/// Complete planned roots used by both the recipe and its future preparation check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ScopedInventory {
    /// Frozen selected copies and issue authority.
    pub grant: RepositoryTurnGrant,
    /// Complete frozen hire to which protected metadata belongs.
    pub hired: ScopedRepositories,
    /// Frozen grant's execution primary; never inferred from the legacy context.
    pub cwd: PathBuf,
    /// Declared copies plus purpose-labelled private roots.
    pub writable: Vec<WritableLocation>,
    /// All-hire Git metadata, isolated homes and inbox that tools must not modify.
    pub protected: Vec<PathBuf>,
    /// Private paths shared by settings and environment composition.
    pub private: ScopedPrivatePaths,
    /// Proposed platform, still unverified.
    pub platform: ScopedPlatform,
}

impl ScopedInventory {
    /// Exact writable-root union, including cwd.
    #[must_use]
    pub fn writable_roots(&self) -> Vec<PathBuf> {
        self.writable.iter().map(|root| root.path.clone()).collect()
    }

    /// Additional Codex roots; its cwd is implicit.
    #[must_use]
    pub fn additional_roots(&self) -> Vec<PathBuf> {
        self.writable
            .iter()
            .filter(|root| root.path != self.cwd)
            .map(|root| root.path.clone())
            .collect()
    }
}

/// Missing proofs are explicit even when the pure recipe is representable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopedProofGap {
    /// The daemon has not admitted the actual build and current root identities.
    RuntimeAdmission,
    /// The generated recipe has not passed the complete native probe.
    GeneratedRecipe,
    /// Existing file-mail and control routes need validation under this policy.
    ControlTransport,
    /// Persistent Claude sessions differ from the no-persistence probe.
    ClaudePersistence,
    /// The recognized Codex profile differs from the fallback-model probe.
    CodexModelTools,
}

/// A candidate deliberately has no supported/admitted state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UnarmedScopedRecipe {
    /// CLI version used when choosing the candidate's settings.
    pub candidate_version: &'static str,
    /// Fixed profile whose tool family still requires native validation.
    pub model_profile: ScopedModelProfile,
    /// Reasons this recipe cannot authorize execution.
    pub missing_proofs: Vec<ScopedProofGap>,
}

/// Closed model profiles used only for candidate composition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopedModelProfile {
    /// Explicit Sonnet selector with the proposed Bash/Read/Edit list.
    ClaudeSonnet46,
    /// The unarmed profile's pinned native catalog declares `unified_exec` and freeform patch and does not disable Node REPL.
    /// Its tool family differs from the fallback metadata used by the original probe.
    CodexGpt55,
}

/// One pure candidate bundle. Returning it never authorizes a lease or child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopedManagedLaunch {
    /// Typed process projection.
    pub spec: LaunchSpec,
    /// Files/directories belonging to this exact candidate.
    pub install: ManagedInstall,
    /// Root plan requiring independent daemon admission.
    pub inventory: ScopedInventory,
    /// No production support can be inferred from composition.
    pub unarmed: UnarmedScopedRecipe,
}

/// Fixed, non-secret refusal categories for scoped composition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ScopedError {
    /// No scoped implementation exists.
    #[error("provider has no scoped recipe")]
    Provider,
    /// Mode/platform is outside the candidate set.
    #[error("execution mode or platform has no scoped recipe")]
    ModeOrPlatform,
    /// The supplied frozen sets disagree.
    #[error("frozen repository authority is inconsistent")]
    Authority,
    /// A path spelling or role is not representable safely.
    #[error("scoped path plan is invalid or overlapping")]
    Paths,
    /// A configurable escape hatch conflicts with the closed recipe.
    #[error("configured overrides conflict with the scoped recipe")]
    Options,
    /// The session kind does not match this provider.
    #[error("session mode has no scoped candidate")]
    Session,
    /// A default or other model has no validated tool profile.
    #[error("model selection has no scoped candidate profile")]
    Model,
    /// Ambiguous environment inputs cannot be selected.
    #[error("scoped environment selection is ambiguous")]
    Environment,
}

fn path(path: &Path) -> Result<(), ScopedError> {
    // Edit(...) embeds this spelling; broader literal grammars need independent validation.
    let text = path.to_str().ok_or(ScopedError::Paths)?;
    if !text.starts_with('/')
        || text == "/"
        || text.ends_with('/')
        || !text.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'.' | b' ')
        })
        || text[1..].split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || part.starts_with(' ')
                || part.ends_with(' ')
        })
    {
        return Err(ScopedError::Paths);
    }
    Ok(())
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn protected_repositories(scope: &ScopedLaunchInputs<'_>) -> Result<Vec<PathBuf>, ScopedError> {
    let hired = scope.hired;
    let grant = scope.grant;
    if grant.hired_lsn == dudes_types::Lsn::ZERO
        || grant.catalog_lsn == dudes_types::Lsn::ZERO
        || grant.policy != RepositoryPolicy::WorkingTreeV1
        || grant.project != hired.project.id
        || grant.catalog_lsn != hired.catalog_lsn
        || grant.policy != hired.policy
        || scope.origin_git_dirs.len() != hired.repositories.entries().len()
        || grant
            .repositories
            .entries()
            .iter()
            .any(|entry| hired.repositories.get(&entry.repo_id) != Some(&entry.value))
    {
        return Err(ScopedError::Authority);
    }
    let mut protected = Vec::new();
    for (index, (entry, origin_git)) in hired
        .repositories
        .entries()
        .iter()
        .zip(scope.origin_git_dirs)
        .enumerate()
    {
        if entry.repo_id != origin_git.repo_id {
            return Err(ScopedError::Authority);
        }
        if hired.repositories.entries()[..index].iter().any(|other| {
            overlaps(
                &entry.value.execution.directory,
                &other.value.execution.directory,
            ) || entry.value.origin.common_identity == other.value.origin.common_identity
        }) {
            return Err(ScopedError::Authority);
        }
        for root in [
            &entry.value.origin.directory,
            &entry.value.execution.directory,
            &entry.value.origin.common_dir,
            &entry.value.execution.git_dir,
            &origin_git.value,
        ] {
            path(root)?;
        }
        protected.extend([
            entry.value.origin.directory.join(".git"),
            entry.value.execution.directory.join(".git"),
            entry.value.execution.git_dir.clone(),
            origin_git.value.clone(),
            entry.value.origin.common_dir.clone(),
        ]);
    }
    Ok(protected)
}

fn inventory(
    ctx: &AgentCtx<'_>,
    scope: &ScopedLaunchInputs<'_>,
    selection: &ModelSelection,
    model: &str,
) -> Result<ScopedInventory, ScopedError> {
    if scope.mode != Mode::Managed
        || !matches!(
            scope.platform,
            ScopedPlatform::MacOsArm64 | ScopedPlatform::LinuxX86_64
        )
    {
        return Err(ScopedError::ModeOrPlatform);
    }
    if ctx.sandbox == Some(false) || !ctx.mcp_servers.is_empty() {
        return Err(ScopedError::Options);
    }
    if selection.selector() != Some(model) {
        return Err(ScopedError::Model);
    }
    let grant = scope.grant;
    let hired = scope.hired;
    let mut protected = protected_repositories(scope)?;
    let declared = grant.repositories.entries().to_vec();
    let cwd = grant.repositories.primary().execution.directory.clone();
    let private = &scope.private;
    let controls = [
        private.home.clone(),
        private.profile.clone(),
        ctx.agent_dir.join("inbox"),
    ];
    if private.work != ctx.agent_dir.join("work")
        || private.outbox != ctx.agent_dir.join("outbox")
        || private.temp != ctx.agent_dir.join("tmp")
    {
        return Err(ScopedError::Paths);
    }
    let mut writable: Vec<_> = declared
        .iter()
        .map(|entry| WritableLocation {
            purpose: WritablePurpose::Repository(entry.repo_id.clone()),
            path: entry.value.execution.directory.clone(),
        })
        .collect();
    writable.extend(
        [
            (WritablePurpose::Work, &private.work),
            (WritablePurpose::Outbox, &private.outbox),
            (WritablePurpose::Temporary, &private.temp),
        ]
        .into_iter()
        .map(|(purpose, path)| WritableLocation {
            purpose,
            path: path.clone(),
        }),
    );
    for (index, root) in writable.iter().enumerate() {
        path(&root.path)?;
        if writable[..index]
            .iter()
            .any(|other| overlaps(&root.path, &other.path))
            || controls.iter().any(|control| overlaps(&root.path, control))
        {
            return Err(ScopedError::Paths);
        }
    }
    for root in [&private.work, &private.outbox, &private.temp] {
        if hired.repositories.entries().iter().any(|entry| {
            overlaps(root, &entry.value.execution.directory)
                || overlaps(root, &entry.value.origin.directory)
        }) || protected.iter().any(|other| overlaps(root, other))
        {
            return Err(ScopedError::Paths);
        }
    }
    for control in &controls {
        path(control)?;
        if hired.repositories.entries().iter().any(|entry| {
            overlaps(control, &entry.value.origin.directory)
                || overlaps(control, &entry.value.execution.directory)
        }) || protected.iter().any(|root| overlaps(control, root))
        {
            return Err(ScopedError::Paths);
        }
    }
    protected.extend(controls);
    let mut seen = BTreeSet::new();
    protected.retain(|root| seen.insert(root.clone()));
    Ok(ScopedInventory {
        grant: grant.clone(),
        hired: hired.clone(),
        cwd,
        writable,
        protected,
        private: private.clone(),
        platform: scope.platform,
    })
}

fn environment(
    ctx: &AgentCtx<'_>,
    inventory: &ScopedInventory,
    provider: Provider,
) -> Result<Vec<EnvVar>, ScopedError> {
    let selected: &[&str] = match provider {
        Provider::Claude => &["PATH", "LANG", "ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL"],
        Provider::Codex => &["PATH", "LANG", "OPENAI_API_KEY", "CODEX_API_KEY"],
        _ => return Err(ScopedError::Provider),
    };
    let mut env = Vec::new();
    let mut names = BTreeSet::new();
    for value in ctx
        .daemon_env
        .iter()
        .filter(|var| selected.iter().any(|name| var.name() == *name))
    {
        if !names.insert(value.name()) {
            return Err(ScopedError::Environment);
        }
        env.push(value.clone());
    }
    let p = &inventory.private;
    env.extend([
        EnvVar::new("HOME", &p.home),
        EnvVar::new("TMPDIR", &p.temp),
        EnvVar::new("TMP", &p.temp),
        EnvVar::new("TEMP", &p.temp),
        EnvVar::new("DUDES_MAILBOX", ctx.agent_dir),
        EnvVar::new("DUDES_COMMAND_TRANSPORT", "file-mail"),
    ]);
    env.sort();
    Ok(env)
}

fn unarmed(version: &'static str, gap: ScopedProofGap) -> UnarmedScopedRecipe {
    UnarmedScopedRecipe {
        candidate_version: version,
        model_profile: match gap {
            ScopedProofGap::ClaudePersistence => ScopedModelProfile::ClaudeSonnet46,
            _ => ScopedModelProfile::CodexGpt55,
        },
        missing_proofs: vec![
            ScopedProofGap::RuntimeAdmission,
            ScopedProofGap::GeneratedRecipe,
            ScopedProofGap::ControlTransport,
            gap,
        ],
    }
}
