//! Pure instruction discovery declarations. The daemon expands and samples these rules.
//!
//! Candidates include absent, inactive, and shadowed files. A declaration describes possible
//! inputs, not files a vendor read. It retains no instruction text or unfiltered configuration.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use dudes_proto::instruction::{
    InstructionValidationError, MAX_CLAUDE_IMPORT_DEPTH, MAX_INSTRUCTION_INPUTS,
    MAX_INSTRUCTION_PATH_BYTES, NativeInstructionPath,
};
use dudes_types::{IsolationMode, Mode, Provider};

use crate::adapter::AgentCtx;
use crate::claude::config::SettingSource;
use crate::launch::EnvVar;

/// A provider's discovery rules and independent reasons coverage is unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionDeclaration {
    /// Revision of this declaration vocabulary.
    pub revision: u16,
    /// The actual launch composition, independent of recorded hire selectors.
    pub mode: DeclarationMode,
    /// Rules the daemon re-expands at both ends of an interval.
    pub inputs: Vec<InstructionInput>,
    /// Named uncertainty. An empty input list alone never establishes complete coverage.
    pub gaps: Vec<DeclarationGap>,
}

impl InstructionDeclaration {
    /// An adapter without a declaration supplies unknown coverage.
    #[must_use]
    pub fn missing(provider: Provider, mode: Mode) -> Self {
        Self {
            revision: 1,
            mode: DeclarationMode::Unsupported { provider, mode },
            inputs: Vec::new(),
            gaps: vec![DeclarationGap::DeclarationMissing],
        }
    }

    fn new(mode: DeclarationMode, ctx: &AgentCtx<'_>) -> Self {
        let mut declaration = Self {
            revision: 1,
            mode,
            inputs: Vec::new(),
            gaps: Vec::new(),
        };
        if mode != DeclarationMode::CodexStreaming && ctx.daemon_env.len() > MAX_INSTRUCTION_INPUTS
        {
            declaration.gap(DeclarationGap::DeclarationLimit);
        }
        declaration
    }

    fn input(
        &mut self,
        root: InstructionRoot,
        rule: DiscoveryRule,
        activation: Activation,
        evidence: EvidenceRevision,
        imports: ImportRule,
    ) {
        if self.inputs.len() >= MAX_INSTRUCTION_INPUTS {
            self.gap(DeclarationGap::DeclarationLimit);
            return;
        }
        if let Err(gap) = validate_path(root.path()).and_then(|()| validate_rule(&rule)) {
            self.gap(gap);
            return;
        }
        self.inputs.push(InstructionInput {
            root,
            rule,
            activation,
            evidence,
            imports,
        });
    }

    fn gap(&mut self, gap: DeclarationGap) {
        if !self.gaps.contains(&gap) {
            self.gaps.push(gap);
        }
    }

    fn path(&mut self, path: &Path) -> Option<PathBuf> {
        match validate_path(path) {
            Ok(()) => Some(path.to_owned()),
            Err(gap) => {
                self.gap(gap);
                None
            }
        }
    }
}

/// The composition whose candidate rules were declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclarationMode {
    /// Claude's configured managed isolation.
    ClaudeManaged(IsolationMode),
    /// Claude's attached resume composer, which inherits discovery independently.
    ClaudeAttached,
    /// Codex's exec transport and inherited profile.
    CodexLegacy,
    /// Codex's app-server transport and daemon-admitted profile.
    CodexStreaming,
    /// Pi's fixed managed composition.
    PiManaged,
    /// No supported declaration for this provider and launch mode.
    Unsupported {
        /// The requested provider.
        provider: Provider,
        /// The requested launch mode.
        mode: Mode,
    },
}

/// One candidate family with its origin, activation, and import semantics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionInput {
    /// A supplied native path. Canonical resolution belongs to the daemon.
    pub root: InstructionRoot,
    /// Candidate enumeration and priority.
    pub rule: DiscoveryRule,
    /// Whether the composition admits the family, or which condition is unresolved.
    pub activation: Activation,
    /// The source boundary supporting the candidate inventory.
    pub evidence: EvidenceRevision,
    /// Import syntax for these candidates.
    pub imports: ImportRule,
}

/// The declared path origin, without canonical resolution or a filesystem access grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstructionRoot {
    /// The selected execution folder, including an actual lease path when supplied.
    ExecutionDirectory(PathBuf),
    /// A supplied vendor profile selector or a syntactic home fallback.
    ProfileDirectory {
        /// Which profile this is.
        profile: ProfileKind,
        /// The supplied path, without filesystem resolution.
        path: PathBuf,
    },
    /// A directory explicitly admitted in provider arguments.
    AdditionalDirectory(PathBuf),
    /// The platform's documented managed-policy directory.
    ManagedPolicyDirectory(PathBuf),
    /// Daemon-owned launch configuration, distinct from shared instruction inputs.
    DaemonDirectory(PathBuf),
    /// An explicit file or resource selector, relative to the launch cwd when relative.
    ExplicitPath(PathBuf),
    /// Home-relative import resolution. This root never permits directory enumeration.
    ImportHomeDirectory(PathBuf),
}

impl InstructionRoot {
    fn path(&self) -> &Path {
        match self {
            Self::ExecutionDirectory(path)
            | Self::AdditionalDirectory(path)
            | Self::ManagedPolicyDirectory(path)
            | Self::DaemonDirectory(path)
            | Self::ExplicitPath(path)
            | Self::ImportHomeDirectory(path)
            | Self::ProfileDirectory { path, .. } => path,
        }
    }
}

/// Vendor profile kinds whose selectors may be missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileKind {
    /// Claude's configuration root.
    Claude,
    /// Codex's inherited home.
    CodexLegacy,
    /// Codex's daemon-admitted streaming home.
    CodexManaged,
    /// Pi's coding-agent directory.
    Pi,
}

/// Rules contain every candidate, including lower-priority names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryRule {
    /// Candidates relative to exactly one directory.
    FixedCandidates {
        /// Names in priority order.
        names: Vec<PathBuf>,
        /// Selection semantics, without discarding shadowed candidates.
        selection: CandidateSelection,
    },
    /// Candidates at the execution directory and its bounded ancestor chain.
    AncestorCandidates {
        /// Names in priority order within each directory.
        names: Vec<PathBuf>,
        /// Selection within each directory.
        selection: CandidateSelection,
        /// The vendor's search boundary.
        boundary: AncestorBoundary,
    },
    /// On-demand candidates beneath an admitted project root, under daemon traversal bounds.
    DescendantCandidates {
        /// Relative names in every examined descendant directory.
        names: Vec<PathBuf>,
        /// Selection within each directory.
        selection: CandidateSelection,
    },
    /// Markdown files recursively beneath the named rules directory.
    MarkdownRules {
        /// Relative rules directory.
        directory: PathBuf,
        /// Direct rules directory, or a rules directory at each examined project directory.
        scope: RuleDirectoryScope,
    },
    /// The root itself names an explicit file.
    ExplicitFile {
        /// Why the composer admits the file.
        purpose: ExplicitFilePurpose,
    },
    /// The root supplies the base for home-relative imports, without enumerating home files.
    ImportHome,
    /// A typed resource whose instruction-producing contents are not inventoried.
    ExplicitResource(ResourceKind),
    /// A daemon-written file. It is configuration, not an alleged agent instruction change.
    GeneratedConfig {
        /// Path relative to the daemon directory.
        file: PathBuf,
        /// The composer that owns the file.
        kind: GeneratedConfigKind,
    },
}

/// Which candidate a vendor loader can select, without omitting the other candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateSelection {
    /// Every candidate can contribute.
    All,
    /// First nonempty readable candidate, as in Codex home instructions.
    FirstNonempty,
    /// First regular candidate selected before reading, as in Codex project instructions.
    FirstRegular,
    /// First readable regular candidate, as in Pi context discovery.
    FirstReadable,
}

/// The ancestor search boundary, not a daemon work limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AncestorBoundary {
    /// The filesystem root.
    FilesystemRoot,
    /// Codex's effective root markers are unknown. These are documented default candidates.
    CodexRootMarkers {
        /// Default marker names, without claiming effective configuration.
        defaults: Vec<PathBuf>,
    },
}

/// Where a recursive rules directory can occur.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleDirectoryScope {
    /// Under exactly the supplied root.
    Root,
    /// Under the execution directory and its ancestors.
    Ancestors,
    /// Under bounded descendant directories of the admitted execution root.
    Descendants,
}

/// Candidate admission, without inventing a resolved effective configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activation {
    /// The composition admits these candidates.
    Active,
    /// The composition excludes automatic discovery of this family.
    Inactive(InactiveReason),
    /// A source-qualified condition remains unresolved.
    Conditional(DiscoveryCondition),
}

/// Why automatic discovery is inactive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InactiveReason {
    /// Claude's selected settings sources exclude the family.
    ClaudeSourceExcluded,
    /// Claude's bare managed composition suppresses automatic discovery.
    ClaudeBare,
    /// The actual managed composer refuses repository settings for this context.
    ClaudeTrustRefused,
    /// Codex streaming prepares an untrusted project configuration.
    CodexUntrustedProject,
    /// Pi's fixed no-approve composition suppresses project system files.
    PiNoApprove,
}

/// Vendor conditions that need configuration or path evidence from the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryCondition {
    /// Claude's settings-source selector could not be inspected within the declaration bound.
    ClaudeEffectiveConfiguration,
    /// Claude AGENTS fallback depends on enabled plugin options and hierarchy candidates.
    ClaudeAgentsFallback,
    /// Additional directories require the vendor's instruction-discovery switch.
    ClaudeAdditionalDirectories,
    /// Claude managed policy may provide file or inline instructions.
    ClaudeManagedPolicy,
    /// Codex legacy project trust is not determined by the recorded Dudes trust bit.
    CodexEffectiveTrust,
}

/// Import semantics the sampler must implement, distinct from its own work bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportRule {
    /// No import expansion established by the inspected loader.
    None,
    /// Claude memory import candidates, including targets with unresolved external-project
    /// approval. Approval controls activation, not membership of this bounded candidate closure.
    /// Resolve relative paths from the importing file, support absolute and home-relative paths
    /// and escaped spaces, and exclude quoted paths, code spans, and fences.
    ClaudeMemory {
        /// The documented vendor recursion limit, in hops.
        max_hops: u8,
    },
}

/// Evidence for an inventory, without claiming the vendor read a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceRevision {
    /// Current Claude memory and CLI documentation, without executable-source verification.
    ClaudeCurrentDocumentation,
    /// Codex `rust-v0.160.1` home and project loaders.
    Codex01601,
    /// Pi `v1.0.4` resource loader.
    Pi104,
    /// The actual Dudes launch composer.
    DudesComposition,
}

/// Explicit instruction file selectors parsed from typed arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplicitFilePurpose {
    /// Replace the system prompt with file contents.
    SystemPrompt,
    /// Append file contents to the system prompt.
    AppendSystemPrompt,
    /// Append file contents to subagent system prompts.
    AppendSubagentSystemPrompt,
    /// Supply an agent-definition document that may select further resources.
    AgentDefinitions,
}

/// Resources with an incomplete instruction inventory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    /// An explicitly admitted Claude plugin directory, archive, or folder of plugins.
    ClaudePlugin,
    /// Claude agent definitions may refer to instruction-producing resources.
    ClaudeAgentDefinitions,
    /// An argument outside the supported instruction-selector grammar.
    UnrecognizedArgument,
}

/// Daemon-owned settings projected by a launch composer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeneratedConfigKind {
    /// Claude managed model and isolation settings.
    ClaudeManaged,
    /// Claude attached hook settings.
    ClaudeAttached,
}

/// Reasons a declaration cannot establish complete instruction coverage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclarationGap {
    /// This adapter has no declaration implementation.
    DeclarationMissing,
    /// Candidate rules or supplied selectors exceed the fixed declaration scan bound.
    DeclarationLimit,
    /// An argument or encoded native path exceeds the fixed metadata inspection bound.
    MetadataLimit,
    /// Native path bytes cannot be admitted to the observation contract.
    PathUnresolved,
    /// No supplied profile selector or home fallback determines this root.
    ProfileUnavailable(ProfileKind),
    /// Claude's configured settings-source selection is refused by its composer.
    ClaudeSettingSourcesRefused,
    /// Effective instruction-bearing Claude configuration is not supplied.
    ClaudeEffectiveConfiguration,
    /// Enabled Claude AGENTS plugin options are not supplied.
    ClaudeAgentsPlugin,
    /// Managed policy may contain inline instructions, without a safe supplied selector.
    ClaudeManagedInlinePolicy,
    /// Explicit import-admission uncertainty retained from a declaration that lacks a comparable
    /// candidate closure. Approval alone is not a gap when the bounded candidate superset is known.
    ClaudeImportApproval,
    /// No supplied home path determines home-relative Claude import targets.
    ClaudeImportHomeUnavailable,
    /// The additional-directory instruction switch is not a supplied typed selector.
    ClaudeAdditionalDirectoriesSwitch,
    /// Inline prompt contents are outside file-based observation.
    ClaudeInlinePrompt,
    /// Inline agent definitions are outside file-based observation.
    ClaudeInlineAgentDefinitions,
    /// A selected resource has no complete discovery grammar.
    ExplicitResourceUnknown(ResourceKind),
    /// A supported file selector lacks a path value.
    ExplicitFileValueMissing(ExplicitFilePurpose),
    /// A directory selector lacks a path value.
    ExplicitDirectoryValueMissing,
    /// An inline argument cannot be interpreted without losing native path bytes.
    ArgumentEncoding,
    /// Codex's effective fallback filenames are not supplied.
    CodexFallbackNames,
    /// Codex's effective project root markers are not supplied.
    CodexRootMarkers,
    /// Codex's effective `model_instructions_file` selector is not supplied.
    CodexModelInstructionsFile,
    /// Effective instruction-bearing Codex configuration is not supplied.
    CodexEffectiveConfiguration,
    /// Codex legacy effective project trust is not supplied.
    CodexEffectiveTrust,
    /// Effective instruction-bearing Pi configuration is not supplied.
    PiEffectiveConfiguration,
}

#[derive(Clone, Copy)]
pub(crate) enum ClaudeSources<'a> {
    Selected(&'a [SettingSource]),
    Inherit,
    Bare,
    Refused,
    Unresolved,
}

pub(crate) fn validate_path(path: &Path) -> Result<(), DeclarationGap> {
    if path.as_os_str().as_encoded_bytes().len() > MAX_INSTRUCTION_PATH_BYTES {
        return Err(DeclarationGap::MetadataLimit);
    }
    NativeInstructionPath::from_path(path)
        .map(|_| ())
        .map_err(|error| match error {
            InstructionValidationError::PathTooLarge => DeclarationGap::MetadataLimit,
            _ => DeclarationGap::PathUnresolved,
        })
}

fn validate_rule(rule: &DiscoveryRule) -> Result<(), DeclarationGap> {
    match rule {
        DiscoveryRule::FixedCandidates { names, .. }
        | DiscoveryRule::DescendantCandidates { names, .. } => validate_names(names),
        DiscoveryRule::AncestorCandidates {
            names, boundary, ..
        } => {
            validate_names(names)?;
            if let AncestorBoundary::CodexRootMarkers { defaults } = boundary {
                validate_names(defaults)?;
            }
            Ok(())
        }
        DiscoveryRule::MarkdownRules { directory, .. } => validate_path(directory),
        DiscoveryRule::GeneratedConfig { file, .. } => validate_path(file),
        DiscoveryRule::ExplicitFile { .. }
        | DiscoveryRule::ExplicitResource(_)
        | DiscoveryRule::ImportHome => Ok(()),
    }
}

fn validate_names(names: &[PathBuf]) -> Result<(), DeclarationGap> {
    if names.len() > MAX_INSTRUCTION_INPUTS {
        return Err(DeclarationGap::DeclarationLimit);
    }
    names.iter().try_for_each(|path| validate_path(path))
}

fn names(values: &[&str]) -> Vec<PathBuf> {
    values.iter().map(PathBuf::from).collect()
}

fn path_selector<'a>(env: &'a [EnvVar], name: &str) -> Result<Option<&'a Path>, DeclarationGap> {
    let mut selected: Option<&EnvVar> = None;
    for var in env.iter().take(MAX_INSTRUCTION_INPUTS).filter(|var| {
        if cfg!(windows) {
            var.name()
                .as_encoded_bytes()
                .eq_ignore_ascii_case(name.as_bytes())
        } else {
            var.name() == OsStr::new(name)
        }
    }) {
        if var.expose_value().as_encoded_bytes().len() > MAX_INSTRUCTION_PATH_BYTES {
            return Err(DeclarationGap::MetadataLimit);
        }
        if selected.is_none_or(|previous| var > previous) {
            selected = Some(var);
        }
    }
    Ok(selected
        .filter(|var| !var.expose_value().is_empty())
        .map(|var| Path::new(var.expose_value())))
}

fn profile(
    ctx: &AgentCtx<'_>,
    selector: &str,
    fallback: &str,
) -> Result<Option<PathBuf>, DeclarationGap> {
    if let Some(path) = path_selector(ctx.daemon_env, selector)? {
        validate_path(path)?;
        return Ok(Some(path.to_owned()));
    }
    home(ctx)?
        .map(|home| {
            let path = home.join(fallback);
            validate_path(&path)?;
            Ok(path)
        })
        .transpose()
}

fn home(ctx: &AgentCtx<'_>) -> Result<Option<PathBuf>, DeclarationGap> {
    path_selector(
        ctx.daemon_env,
        if cfg!(windows) { "USERPROFILE" } else { "HOME" },
    )?
    .map(|path| {
        validate_path(path)?;
        Ok(path.to_owned())
    })
    .transpose()
}

fn claude_activation(sources: &ClaudeSources<'_>, source: SettingSource) -> Activation {
    match sources {
        ClaudeSources::Selected(selected) if selected.contains(&source) => Activation::Active,
        ClaudeSources::Selected(_) => Activation::Inactive(InactiveReason::ClaudeSourceExcluded),
        ClaudeSources::Inherit => Activation::Active,
        ClaudeSources::Bare => Activation::Inactive(InactiveReason::ClaudeBare),
        ClaudeSources::Refused => Activation::Inactive(InactiveReason::ClaudeTrustRefused),
        ClaudeSources::Unresolved => {
            Activation::Conditional(DiscoveryCondition::ClaudeEffectiveConfiguration)
        }
    }
}

fn claude_project(
    declaration: &mut InstructionDeclaration,
    root: &InstructionRoot,
    project: Activation,
    local: Activation,
) {
    let evidence = EvidenceRevision::ClaudeCurrentDocumentation;
    let imports = ImportRule::ClaudeMemory {
        max_hops: MAX_CLAUDE_IMPORT_DEPTH,
    };
    for (candidates, activation) in [
        (names(&["CLAUDE.md", ".claude/CLAUDE.md"]), project),
        (names(&["CLAUDE.local.md"]), local),
        (
            names(&["AGENTS.md"]),
            Activation::Conditional(DiscoveryCondition::ClaudeAgentsFallback),
        ),
    ] {
        declaration.input(
            root.clone(),
            DiscoveryRule::AncestorCandidates {
                names: candidates.clone(),
                selection: CandidateSelection::All,
                boundary: AncestorBoundary::FilesystemRoot,
            },
            activation,
            evidence,
            imports,
        );
        declaration.input(
            root.clone(),
            DiscoveryRule::DescendantCandidates {
                names: candidates,
                selection: CandidateSelection::All,
            },
            activation,
            evidence,
            imports,
        );
    }
    declaration.input(
        root.clone(),
        DiscoveryRule::FixedCandidates {
            names: names(&[".claude/settings.json", ".claude/settings.local.json"]),
            selection: CandidateSelection::All,
        },
        Activation::Active,
        evidence,
        ImportRule::None,
    );
    for scope in [
        RuleDirectoryScope::Ancestors,
        RuleDirectoryScope::Descendants,
    ] {
        declaration.input(
            root.clone(),
            DiscoveryRule::MarkdownRules {
                directory: ".claude/rules".into(),
                scope,
            },
            project,
            evidence,
            imports,
        );
    }
}

fn claude_profile(
    declaration: &mut InstructionDeclaration,
    ctx: &AgentCtx<'_>,
    sources: &ClaudeSources<'_>,
) {
    match profile(ctx, "CLAUDE_CONFIG_DIR", ".claude") {
        Ok(Some(path)) => {
            let root = InstructionRoot::ProfileDirectory {
                profile: ProfileKind::Claude,
                path,
            };
            let activation = claude_activation(sources, SettingSource::User);
            declaration.input(
                root.clone(),
                DiscoveryRule::FixedCandidates {
                    names: names(&["CLAUDE.md"]),
                    selection: CandidateSelection::All,
                },
                activation,
                EvidenceRevision::ClaudeCurrentDocumentation,
                ImportRule::ClaudeMemory {
                    max_hops: MAX_CLAUDE_IMPORT_DEPTH,
                },
            );
            declaration.input(
                root,
                DiscoveryRule::MarkdownRules {
                    directory: "rules".into(),
                    scope: RuleDirectoryScope::Root,
                },
                activation,
                EvidenceRevision::ClaudeCurrentDocumentation,
                ImportRule::ClaudeMemory {
                    max_hops: MAX_CLAUDE_IMPORT_DEPTH,
                },
            );
        }
        Ok(None) => declaration.gap(DeclarationGap::ProfileUnavailable(ProfileKind::Claude)),
        Err(gap) => declaration.gap(gap),
    }
}

pub(crate) fn claude_manifest(
    ctx: &AgentCtx<'_>,
    mode: DeclarationMode,
    sources: ClaudeSources<'_>,
    extra_args: &[OsString],
    generated_settings: Result<Option<PathBuf>, DeclarationGap>,
) -> InstructionDeclaration {
    let mut declaration = InstructionDeclaration::new(mode, ctx);
    let project = claude_activation(&sources, SettingSource::Project);
    let local = claude_activation(&sources, SettingSource::Local);
    match home(ctx) {
        Ok(Some(path)) => {
            declaration.input(
                InstructionRoot::ImportHomeDirectory(path),
                DiscoveryRule::ImportHome,
                Activation::Active,
                EvidenceRevision::ClaudeCurrentDocumentation,
                ImportRule::None,
            );
        }
        Ok(None) => declaration.gap(DeclarationGap::ClaudeImportHomeUnavailable),
        Err(gap) => declaration.gap(gap),
    }
    if let Some(path) = declaration.path(ctx.cwd) {
        claude_project(
            &mut declaration,
            &InstructionRoot::ExecutionDirectory(path),
            project,
            local,
        );
    }
    claude_profile(&mut declaration, ctx, &sources);
    let policy = if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode")
    } else if cfg!(windows) {
        PathBuf::from(r"C:\Program Files\ClaudeCode")
    } else {
        PathBuf::from("/etc/claude-code")
    };
    declaration.input(
        InstructionRoot::ManagedPolicyDirectory(policy),
        DiscoveryRule::FixedCandidates {
            names: names(&["CLAUDE.md"]),
            selection: CandidateSelection::All,
        },
        Activation::Conditional(DiscoveryCondition::ClaudeManagedPolicy),
        EvidenceRevision::ClaudeCurrentDocumentation,
        ImportRule::ClaudeMemory {
            max_hops: MAX_CLAUDE_IMPORT_DEPTH,
        },
    );
    match generated_settings {
        Ok(Some(file)) => {
            if let Some(path) = declaration.path(ctx.agent_dir) {
                declaration.input(
                    InstructionRoot::DaemonDirectory(path),
                    DiscoveryRule::GeneratedConfig {
                        file,
                        kind: if mode == DeclarationMode::ClaudeAttached {
                            GeneratedConfigKind::ClaudeAttached
                        } else {
                            GeneratedConfigKind::ClaudeManaged
                        },
                    },
                    Activation::Active,
                    EvidenceRevision::DudesComposition,
                    ImportRule::None,
                );
            }
        }
        Ok(None) => {}
        Err(gap) => declaration.gap(gap),
    }
    if matches!(sources, ClaudeSources::Refused) {
        declaration.gap(DeclarationGap::ClaudeSettingSourcesRefused);
    }
    if matches!(sources, ClaudeSources::Unresolved) {
        declaration.gap(DeclarationGap::DeclarationLimit);
    }
    for gap in [
        DeclarationGap::ClaudeEffectiveConfiguration,
        DeclarationGap::ClaudeAgentsPlugin,
        DeclarationGap::ClaudeManagedInlinePolicy,
    ] {
        declaration.gap(gap);
    }
    claude_explicit(&mut declaration, extra_args, local);
    declaration
}

#[derive(Clone, Copy)]
enum ExplicitSelector {
    File(ExplicitFilePurpose),
    AdditionalDirectory,
    Plugin,
    InlinePrompt,
}

impl ExplicitSelector {
    fn parse(flag: &str) -> Option<Self> {
        match flag {
            "--system-prompt-file" => Some(Self::File(ExplicitFilePurpose::SystemPrompt)),
            "--append-system-prompt-file" => {
                Some(Self::File(ExplicitFilePurpose::AppendSystemPrompt))
            }
            "--append-subagent-system-prompt-file" => {
                Some(Self::File(ExplicitFilePurpose::AppendSubagentSystemPrompt))
            }
            "--agents" => Some(Self::File(ExplicitFilePurpose::AgentDefinitions)),
            "--add-dir" => Some(Self::AdditionalDirectory),
            "--plugin-dir" => Some(Self::Plugin),
            "--system-prompt" | "--append-system-prompt" | "--append-subagent-system-prompt" => {
                Some(Self::InlinePrompt)
            }
            _ => None,
        }
    }
}

fn claude_explicit(declaration: &mut InstructionDeclaration, args: &[OsString], local: Activation) {
    if args.len() > MAX_INSTRUCTION_INPUTS {
        declaration.gap(DeclarationGap::DeclarationLimit);
    }
    let args = &args[..args.len().min(MAX_INSTRUCTION_INPUTS)];
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if declaration.inputs.len() >= MAX_INSTRUCTION_INPUTS {
            declaration.gap(DeclarationGap::DeclarationLimit);
            break;
        }
        index += 1;
        if arg.as_encoded_bytes().len()
            > MAX_INSTRUCTION_PATH_BYTES + "--append-subagent-system-prompt-file=".len()
        {
            declaration.gap(DeclarationGap::MetadataLimit);
            continue;
        }
        let Some(text) = arg.to_str() else {
            declaration.gap(DeclarationGap::ArgumentEncoding);
            continue;
        };
        let (flag, inline) = text
            .split_once('=')
            .map_or((text, None), |(flag, value)| (flag, Some(value)));
        let Some(selector) = ExplicitSelector::parse(flag) else {
            declaration.gap(DeclarationGap::ExplicitResourceUnknown(
                ResourceKind::UnrecognizedArgument,
            ));
            continue;
        };
        let value = inline.map(OsStr::new).or_else(|| {
            let value = args.get(index)?;
            if value.as_encoded_bytes().starts_with(b"-") {
                return None;
            }
            index += 1;
            Some(value.as_os_str())
        });
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            declaration.gap(match selector {
                ExplicitSelector::File(purpose) => {
                    DeclarationGap::ExplicitFileValueMissing(purpose)
                }
                ExplicitSelector::AdditionalDirectory | ExplicitSelector::Plugin => {
                    DeclarationGap::ExplicitDirectoryValueMissing
                }
                ExplicitSelector::InlinePrompt => DeclarationGap::ClaudeInlinePrompt,
            });
            continue;
        };
        if value.as_encoded_bytes().len() > MAX_INSTRUCTION_PATH_BYTES {
            declaration.gap(DeclarationGap::MetadataLimit);
            continue;
        }
        claude_explicit_value(declaration, selector, value, local);
    }
}

fn claude_explicit_value(
    declaration: &mut InstructionDeclaration,
    selector: ExplicitSelector,
    value: &OsStr,
    local: Activation,
) {
    match selector {
        ExplicitSelector::InlinePrompt => declaration.gap(DeclarationGap::ClaudeInlinePrompt),
        ExplicitSelector::AdditionalDirectory => {
            if let Some(path) = declaration.path(Path::new(value)) {
                claude_additional(declaration, path, local);
            }
        }
        ExplicitSelector::Plugin => {
            let Some(path) = declaration.path(Path::new(value)) else {
                return;
            };
            declaration.input(
                InstructionRoot::ExplicitPath(path),
                DiscoveryRule::ExplicitResource(ResourceKind::ClaudePlugin),
                Activation::Active,
                EvidenceRevision::ClaudeCurrentDocumentation,
                ImportRule::None,
            );
            declaration.gap(DeclarationGap::ExplicitResourceUnknown(
                ResourceKind::ClaudePlugin,
            ));
        }
        ExplicitSelector::File(purpose) => {
            if purpose == ExplicitFilePurpose::AgentDefinitions
                && value
                    .to_str()
                    .is_some_and(|text| text.trim_start().starts_with('{'))
            {
                declaration.gap(DeclarationGap::ClaudeInlineAgentDefinitions);
                return;
            }
            let Some(path) = declaration.path(Path::new(value)) else {
                return;
            };
            declaration.input(
                InstructionRoot::ExplicitPath(path),
                DiscoveryRule::ExplicitFile { purpose },
                Activation::Active,
                EvidenceRevision::ClaudeCurrentDocumentation,
                ImportRule::None,
            );
            if purpose == ExplicitFilePurpose::AgentDefinitions {
                declaration.gap(DeclarationGap::ExplicitResourceUnknown(
                    ResourceKind::ClaudeAgentDefinitions,
                ));
            }
        }
    }
}

fn claude_additional(declaration: &mut InstructionDeclaration, path: PathBuf, local: Activation) {
    let activation = Activation::Conditional(DiscoveryCondition::ClaudeAdditionalDirectories);
    let root = InstructionRoot::AdditionalDirectory(path);
    for (candidates, activation) in [
        (names(&["CLAUDE.md", ".claude/CLAUDE.md"]), activation),
        (
            names(&["CLAUDE.local.md"]),
            if local == Activation::Inactive(InactiveReason::ClaudeSourceExcluded) {
                local
            } else {
                activation
            },
        ),
    ] {
        declaration.input(
            root.clone(),
            DiscoveryRule::FixedCandidates {
                names: candidates,
                selection: CandidateSelection::All,
            },
            activation,
            EvidenceRevision::ClaudeCurrentDocumentation,
            ImportRule::ClaudeMemory {
                max_hops: MAX_CLAUDE_IMPORT_DEPTH,
            },
        );
    }
    declaration.input(
        root,
        DiscoveryRule::MarkdownRules {
            directory: ".claude/rules".into(),
            scope: RuleDirectoryScope::Root,
        },
        activation,
        EvidenceRevision::ClaudeCurrentDocumentation,
        ImportRule::ClaudeMemory {
            max_hops: MAX_CLAUDE_IMPORT_DEPTH,
        },
    );
    declaration.gap(DeclarationGap::ClaudeAdditionalDirectoriesSwitch);
}

pub(crate) fn codex_manifest(
    ctx: &AgentCtx<'_>,
    mode: DeclarationMode,
    managed_home: Option<&Path>,
) -> InstructionDeclaration {
    let mut declaration = InstructionDeclaration::new(mode, ctx);
    let (kind, home, activation) = if mode == DeclarationMode::CodexStreaming {
        (
            ProfileKind::CodexManaged,
            managed_home
                .map(|path| {
                    validate_path(path)?;
                    Ok(path.to_owned())
                })
                .transpose(),
            Activation::Inactive(InactiveReason::CodexUntrustedProject),
        )
    } else {
        declaration.gap(DeclarationGap::CodexEffectiveTrust);
        (
            ProfileKind::CodexLegacy,
            profile(ctx, "CODEX_HOME", ".codex"),
            Activation::Conditional(DiscoveryCondition::CodexEffectiveTrust),
        )
    };
    match home {
        Ok(Some(path)) => {
            declaration.input(
                InstructionRoot::ProfileDirectory {
                    profile: kind,
                    path,
                },
                DiscoveryRule::FixedCandidates {
                    names: names(&["AGENTS.override.md", "AGENTS.md"]),
                    selection: CandidateSelection::FirstNonempty,
                },
                Activation::Active,
                EvidenceRevision::Codex01601,
                ImportRule::None,
            );
        }
        Ok(None) => declaration.gap(DeclarationGap::ProfileUnavailable(kind)),
        Err(gap) => declaration.gap(gap),
    }
    if let Some(path) = declaration.path(ctx.cwd) {
        declaration.input(
            InstructionRoot::ExecutionDirectory(path.clone()),
            DiscoveryRule::AncestorCandidates {
                names: names(&["AGENTS.override.md", "AGENTS.md"]),
                selection: CandidateSelection::FirstRegular,
                boundary: AncestorBoundary::CodexRootMarkers {
                    defaults: names(&[".git"]),
                },
            },
            activation,
            EvidenceRevision::Codex01601,
            ImportRule::None,
        );
        declaration.input(
            InstructionRoot::ExecutionDirectory(path),
            DiscoveryRule::AncestorCandidates {
                names: names(&[".codex/config.toml"]),
                selection: CandidateSelection::All,
                boundary: AncestorBoundary::CodexRootMarkers {
                    defaults: names(&[".git"]),
                },
            },
            Activation::Active,
            EvidenceRevision::Codex01601,
            ImportRule::None,
        );
    }
    for gap in [
        DeclarationGap::CodexFallbackNames,
        DeclarationGap::CodexRootMarkers,
        DeclarationGap::CodexModelInstructionsFile,
        DeclarationGap::CodexEffectiveConfiguration,
    ] {
        declaration.gap(gap);
    }
    declaration
}

pub(crate) fn pi_manifest(ctx: &AgentCtx<'_>) -> InstructionDeclaration {
    let mut declaration = InstructionDeclaration::new(DeclarationMode::PiManaged, ctx);
    let candidates = names(&[
        "AGENTS.override.md",
        "AGENTS.md",
        "AGENTS.MD",
        "CLAUDE.md",
        "CLAUDE.MD",
    ]);
    match pi_profile(ctx) {
        Ok(Some(path)) => {
            let root = InstructionRoot::ProfileDirectory {
                profile: ProfileKind::Pi,
                path,
            };
            declaration.input(
                root.clone(),
                DiscoveryRule::FixedCandidates {
                    names: candidates.clone(),
                    selection: CandidateSelection::FirstReadable,
                },
                Activation::Active,
                EvidenceRevision::Pi104,
                ImportRule::None,
            );
            declaration.input(
                root,
                DiscoveryRule::FixedCandidates {
                    names: names(&["SYSTEM.md", "APPEND_SYSTEM.md"]),
                    selection: CandidateSelection::All,
                },
                Activation::Active,
                EvidenceRevision::Pi104,
                ImportRule::None,
            );
        }
        Ok(None) => declaration.gap(DeclarationGap::ProfileUnavailable(ProfileKind::Pi)),
        Err(gap) => declaration.gap(gap),
    }
    if let Some(path) = declaration.path(ctx.cwd) {
        let root = InstructionRoot::ExecutionDirectory(path);
        declaration.input(
            root.clone(),
            DiscoveryRule::AncestorCandidates {
                names: candidates,
                selection: CandidateSelection::FirstReadable,
                boundary: AncestorBoundary::FilesystemRoot,
            },
            Activation::Active,
            EvidenceRevision::Pi104,
            ImportRule::None,
        );
        declaration.input(
            root.clone(),
            DiscoveryRule::FixedCandidates {
                names: names(&[".pi/settings.json"]),
                selection: CandidateSelection::All,
            },
            Activation::Active,
            EvidenceRevision::Pi104,
            ImportRule::None,
        );
        declaration.input(
            root,
            DiscoveryRule::FixedCandidates {
                names: names(&[".pi/SYSTEM.md", ".pi/APPEND_SYSTEM.md"]),
                selection: CandidateSelection::All,
            },
            Activation::Inactive(InactiveReason::PiNoApprove),
            EvidenceRevision::Pi104,
            ImportRule::None,
        );
    }
    declaration.gap(DeclarationGap::PiEffectiveConfiguration);
    declaration
}

fn pi_profile(ctx: &AgentCtx<'_>) -> Result<Option<PathBuf>, DeclarationGap> {
    let Some(path) = profile(ctx, "PI_CODING_AGENT_DIR", ".pi/agent")? else {
        return Ok(None);
    };
    let Some(text) = path.to_str() else {
        return Err(DeclarationGap::PathUnresolved);
    };
    if text == "~"
        || text.starts_with("~/")
        || text.starts_with("file://")
        || (cfg!(windows) && (text.starts_with(r"~\") || pi_shell_drive(text)))
    {
        return Err(DeclarationGap::PathUnresolved);
    }
    Ok(Some(path))
}

fn pi_shell_drive(text: &str) -> bool {
    let Some(tail) = text.as_bytes().strip_prefix(b"/") else {
        return false;
    };
    if tail.starts_with(b"/") || tail.contains(&b'\\') {
        return false;
    }
    let drive = if tail
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"mnt/"))
    {
        &tail[4..]
    } else if tail
        .get(..9)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"cygdrive/"))
    {
        &tail[9..]
    } else {
        tail
    };
    drive.first().is_some_and(u8::is_ascii_alphabetic)
        && (drive.len() == 1 || drive.get(1) == Some(&b'/'))
}

#[cfg(test)]
mod tests {
    use dudes_types::{AgentId, ModelSelection};

    use super::*;
    use crate::adapter::{
        Adapter, Isolation, ManagedInstall, SessionAcquisition, TurnSession, UsageSource,
    };
    use crate::claude::{ClaudeAdapter, config::ClaudeConfig};
    use crate::codex::{CodexAdapter, config::CodexConfig};
    use crate::launch::{LaunchError, LaunchSpec};
    use crate::pi::PiAdapter;
    use crate::stream::TurnReader;

    fn ctx(env: &[EnvVar], trusted: bool) -> AgentCtx<'_> {
        AgentCtx {
            agent: AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            cwd: Path::new("project/selected-worktree"),
            agent_dir: Path::new("state/agents/owner"),
            daemon_env: env,
            sandbox: None,
            trusted,
            max_turns: None,
            max_budget: None,
            mcp_servers: &[],
        }
    }

    fn declaration(
        adapter: &dyn Adapter,
        ctx: &AgentCtx<'_>,
        mode: Mode,
    ) -> InstructionDeclaration {
        adapter.instruction_manifest(ctx, mode, &ModelSelection::provider_default())
    }

    fn project_ancestors(
        declaration: &InstructionDeclaration,
    ) -> Vec<(Vec<PathBuf>, Activation, ImportRule)> {
        declaration
            .inputs
            .iter()
            .filter_map(|input| {
                if !matches!(input.root, InstructionRoot::ExecutionDirectory(_)) {
                    return None;
                }
                match &input.rule {
                    DiscoveryRule::AncestorCandidates { names, .. } => {
                        Some((names.clone(), input.activation, input.imports))
                    }
                    _ => None,
                }
            })
            .collect()
    }

    #[test]
    fn claude_sources_follow_the_actual_composer_and_keep_conditional_candidates() {
        let adapter = ClaudeAdapter::default();
        let supplied = [EnvVar::new("CLAUDE_CONFIG_DIR", "profiles/claude")];
        let trusted = declaration(&adapter, &ctx(&supplied, true), Mode::Managed);
        assert_eq!(
            trusted.mode,
            DeclarationMode::ClaudeManaged(IsolationMode::Default)
        );
        assert_eq!(
            project_ancestors(&trusted),
            vec![
                (
                    names(&["CLAUDE.md", ".claude/CLAUDE.md"]),
                    Activation::Active,
                    ImportRule::ClaudeMemory { max_hops: 4 }
                ),
                (
                    names(&["CLAUDE.local.md"]),
                    Activation::Inactive(InactiveReason::ClaudeSourceExcluded),
                    ImportRule::ClaudeMemory { max_hops: 4 }
                ),
                (
                    names(&["AGENTS.md"]),
                    Activation::Conditional(DiscoveryCondition::ClaudeAgentsFallback),
                    ImportRule::ClaudeMemory { max_hops: 4 }
                ),
            ]
        );
        let untrusted = declaration(&adapter, &ctx(&supplied, false), Mode::Managed);
        assert_eq!(
            project_ancestors(&untrusted)[0].1,
            Activation::Inactive(InactiveReason::ClaudeSourceExcluded)
        );
        assert!(trusted.gaps.contains(&DeclarationGap::ClaudeAgentsPlugin));
        assert!(
            trusted
                .gaps
                .contains(&DeclarationGap::ClaudeManagedInlinePolicy)
        );
        assert!(trusted.inputs.iter().any(|input| input.rule
            == DiscoveryRule::MarkdownRules {
                directory: ".claude/rules".into(),
                scope: RuleDirectoryScope::Descendants,
            }
            && input.imports == ImportRule::ClaudeMemory { max_hops: 4 }));
        assert!(trusted.inputs.iter().any(|input| matches!(&input.rule, DiscoveryRule::DescendantCandidates { names, .. } if names == &[PathBuf::from("CLAUDE.local.md")])));
    }

    #[test]
    fn selected_user_and_local_sources_preserve_the_selected_profile() {
        let adapter = ClaudeAdapter::new(ClaudeConfig {
            setting_sources: Some(vec![SettingSource::User, SettingSource::Local]),
            ..ClaudeConfig::default()
        });
        let supplied = [EnvVar::new("CLAUDE_CONFIG_DIR", "profiles/selected")];
        let manifest = declaration(&adapter, &ctx(&supplied, true), Mode::Managed);
        assert_eq!(
            project_ancestors(&manifest)[0].1,
            Activation::Inactive(InactiveReason::ClaudeSourceExcluded)
        );
        assert_eq!(project_ancestors(&manifest)[1].1, Activation::Active);
        assert!(manifest.inputs.iter().any(|input| input.root
            == InstructionRoot::ProfileDirectory {
                profile: ProfileKind::Claude,
                path: PathBuf::from("profiles/selected"),
            }
            && input.activation == Activation::Active));
        assert!(
            manifest
                .gaps
                .contains(&DeclarationGap::ClaudeImportHomeUnavailable)
        );
    }

    #[test]
    fn unknown_import_approval_keeps_candidates_without_an_inventory_gap() {
        let supplied = [EnvVar::new(
            if cfg!(windows) { "USERPROFILE" } else { "HOME" },
            "operator-home",
        )];
        for isolation in [
            IsolationMode::Default,
            IsolationMode::Inherit,
            IsolationMode::Bare,
        ] {
            let adapter = ClaudeAdapter::new(ClaudeConfig {
                isolation,
                ..ClaudeConfig::default()
            });
            for mode in [Mode::Managed, Mode::Attached] {
                let manifest = declaration(&adapter, &ctx(&supplied, true), mode);
                assert!(
                    !manifest
                        .gaps
                        .contains(&DeclarationGap::ClaudeImportApproval)
                );
                assert!(manifest.inputs.iter().any(|input| {
                    input.imports == ImportRule::ClaudeMemory { max_hops: 4 }
                        && matches!(input.rule, DiscoveryRule::AncestorCandidates { .. })
                }));
                for gap in [
                    DeclarationGap::ClaudeEffectiveConfiguration,
                    DeclarationGap::ClaudeAgentsPlugin,
                    DeclarationGap::ClaudeManagedInlinePolicy,
                ] {
                    assert!(manifest.gaps.contains(&gap));
                }
            }
        }
    }

    #[test]
    fn attached_declarations_ignore_managed_config_and_inherit_their_own_context() {
        let configured = ClaudeAdapter::new(ClaudeConfig {
            isolation: IsolationMode::Bare,
            setting_sources: Some(Vec::new()),
            extra_args: vec!["--system-prompt-file".into(), "managed-prompt.md".into()],
            model: Some("configured-model".into()),
            ..ClaudeConfig::default()
        });
        let supplied = [EnvVar::new("CLAUDE_CONFIG_DIR", "profiles/claude")];
        let attached = declaration(&configured, &ctx(&supplied, false), Mode::Attached);
        assert_eq!(
            attached,
            declaration(
                &ClaudeAdapter::default(),
                &ctx(&supplied, false),
                Mode::Attached
            )
        );
        assert_eq!(attached.mode, DeclarationMode::ClaudeAttached);
        assert_eq!(project_ancestors(&attached)[0].1, Activation::Active);
        assert_eq!(project_ancestors(&attached)[1].1, Activation::Active);
        assert!(attached.inputs.iter().any(|input| input.root
            == InstructionRoot::DaemonDirectory(PathBuf::from("state/agents/owner"))
            && input.rule
                == DiscoveryRule::GeneratedConfig {
                    file: PathBuf::from("hooks/settings.json"),
                    kind: GeneratedConfigKind::ClaudeAttached,
                }));
        let managed = declaration(&configured, &ctx(&supplied, false), Mode::Managed);
        assert_eq!(
            project_ancestors(&managed)[0].1,
            Activation::Inactive(InactiveReason::ClaudeBare)
        );
        assert!(
            managed.inputs.iter().any(|input| input.root
                == InstructionRoot::ExplicitPath(PathBuf::from("managed-prompt.md")))
        );
        assert!(
            !managed
                .inputs
                .iter()
                .any(|input| matches!(input.rule, DiscoveryRule::GeneratedConfig { .. }))
        );
        let pinned = configured.instruction_manifest(
            &ctx(&supplied, false),
            Mode::Managed,
            &ModelSelection {
                selector: Some("recorded-model".into()),
            },
        );
        assert!(pinned.inputs.iter().any(|input| input.rule
            == DiscoveryRule::GeneratedConfig {
                file: PathBuf::from("managed-settings.json"),
                kind: GeneratedConfigKind::ClaudeManaged,
            }));
    }

    #[test]
    fn refused_inherit_keeps_candidates_and_a_named_gap() {
        let adapter = ClaudeAdapter::new(ClaudeConfig {
            isolation: IsolationMode::Inherit,
            ..ClaudeConfig::default()
        });
        let refused = declaration(&adapter, &ctx(&[], false), Mode::Managed);
        let admitted = declaration(&adapter, &ctx(&[], true), Mode::Managed);
        assert_eq!(
            project_ancestors(&refused)[0].1,
            Activation::Inactive(InactiveReason::ClaudeTrustRefused)
        );
        assert!(
            refused
                .gaps
                .contains(&DeclarationGap::ClaudeSettingSourcesRefused)
        );
        assert_eq!(project_ancestors(&admitted)[0].1, Activation::Active);
        assert_eq!(project_ancestors(&admitted)[1].1, Activation::Active);
    }

    #[test]
    fn typed_explicit_inputs_survive_unknown_resources_without_retaining_inline_text() {
        let adapter = ClaudeAdapter::new(ClaudeConfig {
            extra_args: [
                "--system-prompt-file",
                "prompts/system.txt",
                "--append-system-prompt-file=prompts/append.txt",
                "--append-subagent-system-prompt-file",
                "prompts/subagents.txt",
                "--agents",
                "agents.json",
                "--add-dir",
                "additional",
                "--plugin-dir",
                "plugin.zip",
                "--system-prompt",
                "private-inline-prompt",
                "--agents",
                "{\"prompt\":\"private-agent-definition\"}",
                "--unknown-resource",
                "private-resource-value",
            ]
            .map(OsString::from)
            .to_vec(),
            ..ClaudeConfig::default()
        });
        let manifest = declaration(&adapter, &ctx(&[], true), Mode::Managed);
        let files: Vec<_> = manifest
            .inputs
            .iter()
            .filter_map(|input| match (&input.root, &input.rule) {
                (InstructionRoot::ExplicitPath(path), DiscoveryRule::ExplicitFile { purpose }) => {
                    Some((path.clone(), *purpose))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            files,
            vec![
                (
                    PathBuf::from("prompts/system.txt"),
                    ExplicitFilePurpose::SystemPrompt
                ),
                (
                    PathBuf::from("prompts/append.txt"),
                    ExplicitFilePurpose::AppendSystemPrompt
                ),
                (
                    PathBuf::from("prompts/subagents.txt"),
                    ExplicitFilePurpose::AppendSubagentSystemPrompt
                ),
                (
                    PathBuf::from("agents.json"),
                    ExplicitFilePurpose::AgentDefinitions
                ),
            ]
        );
        assert!(manifest.inputs.iter().any(|input| input.root
            == InstructionRoot::AdditionalDirectory("additional".into())
            && matches!(input.rule, DiscoveryRule::FixedCandidates { .. })
            && input.activation
                == Activation::Conditional(DiscoveryCondition::ClaudeAdditionalDirectories)));
        for gap in [
            DeclarationGap::ClaudeInlinePrompt,
            DeclarationGap::ClaudeInlineAgentDefinitions,
            DeclarationGap::ExplicitResourceUnknown(ResourceKind::UnrecognizedArgument),
            DeclarationGap::ClaudeAdditionalDirectoriesSwitch,
        ] {
            assert!(manifest.gaps.contains(&gap));
        }
        let formatted = format!("{manifest:?}");
        assert!(!formatted.contains("private-inline-prompt"));
        assert!(!formatted.contains("private-agent-definition"));
        assert!(!formatted.contains("private-resource-value"));
    }

    #[test]
    fn missing_explicit_values_do_not_consume_the_next_selector() {
        let adapter = ClaudeAdapter::new(ClaudeConfig {
            extra_args: [
                "--system-prompt-file",
                "--append-system-prompt-file=append.md",
                "--plugin-dir",
            ]
            .map(OsString::from)
            .to_vec(),
            ..ClaudeConfig::default()
        });
        let manifest = declaration(&adapter, &ctx(&[], true), Mode::Managed);
        assert!(
            manifest
                .gaps
                .contains(&DeclarationGap::ExplicitFileValueMissing(
                    ExplicitFilePurpose::SystemPrompt
                ))
        );
        assert!(
            manifest
                .gaps
                .contains(&DeclarationGap::ExplicitDirectoryValueMissing)
        );
        assert!(
            manifest
                .inputs
                .iter()
                .any(|input| input.root == InstructionRoot::ExplicitPath("append.md".into()))
        );
    }

    #[test]
    fn codex_declares_both_priorities_and_distinguishes_legacy_from_streaming() {
        let supplied = [EnvVar::new("CODEX_HOME", "operator/codex")];
        let context = ctx(&supplied, true);
        let legacy = declaration(
            &CodexAdapter::default().with_managed_home("unused-home".into()),
            &context,
            Mode::Managed,
        );
        let streaming = declaration(
            &CodexAdapter::new(CodexConfig {
                streaming: true,
                ..CodexConfig::default()
            })
            .with_managed_home("state/providers/codex".into()),
            &context,
            Mode::Managed,
        );
        assert_eq!(legacy.mode, DeclarationMode::CodexLegacy);
        assert_eq!(streaming.mode, DeclarationMode::CodexStreaming);
        assert_eq!(
            legacy.inputs[0].root,
            InstructionRoot::ProfileDirectory {
                profile: ProfileKind::CodexLegacy,
                path: "operator/codex".into()
            }
        );
        assert_eq!(
            streaming.inputs[0].root,
            InstructionRoot::ProfileDirectory {
                profile: ProfileKind::CodexManaged,
                path: "state/providers/codex".into()
            }
        );
        assert_eq!(
            legacy.inputs[0].rule,
            DiscoveryRule::FixedCandidates {
                names: names(&["AGENTS.override.md", "AGENTS.md"]),
                selection: CandidateSelection::FirstNonempty,
            }
        );
        assert_eq!(
            legacy.inputs[1].rule,
            DiscoveryRule::AncestorCandidates {
                names: names(&["AGENTS.override.md", "AGENTS.md"]),
                selection: CandidateSelection::FirstRegular,
                boundary: AncestorBoundary::CodexRootMarkers {
                    defaults: names(&[".git"])
                },
            }
        );
        assert_eq!(
            legacy.inputs[1].activation,
            Activation::Conditional(DiscoveryCondition::CodexEffectiveTrust)
        );
        assert_eq!(
            streaming.inputs[1].activation,
            Activation::Inactive(InactiveReason::CodexUntrustedProject)
        );
        for gap in [
            DeclarationGap::CodexFallbackNames,
            DeclarationGap::CodexRootMarkers,
            DeclarationGap::CodexModelInstructionsFile,
            DeclarationGap::CodexEffectiveConfiguration,
        ] {
            assert!(legacy.gaps.contains(&gap));
            assert!(streaming.gaps.contains(&gap));
        }
        let missing = declaration(
            &CodexAdapter::new(CodexConfig {
                streaming: true,
                ..CodexConfig::default()
            }),
            &context,
            Mode::Managed,
        );
        assert!(missing.gaps.contains(&DeclarationGap::ProfileUnavailable(
            ProfileKind::CodexManaged
        )));
    }

    #[test]
    fn project_engine_settings_files_are_fingerprinted_inputs() {
        let supplied = [EnvVar::new("HOME", "/home/operator")];
        let settings = |manifest: &InstructionDeclaration, files: &[&str]| {
            manifest.inputs.iter().any(|input| {
                matches!(input.root, InstructionRoot::ExecutionDirectory(_))
                    && input.activation == Activation::Active
                    && matches!(&input.rule,
                        DiscoveryRule::FixedCandidates { names: found, .. }
                        | DiscoveryRule::AncestorCandidates { names: found, .. }
                        if found == &names(files))
            })
        };
        let claude = declaration(
            &ClaudeAdapter::default(),
            &ctx(&supplied, true),
            Mode::Managed,
        );
        assert!(settings(
            &claude,
            &[".claude/settings.json", ".claude/settings.local.json"]
        ));
        let codex = declaration(
            &CodexAdapter::default(),
            &ctx(&supplied, true),
            Mode::Managed,
        );
        assert!(settings(&codex, &[".codex/config.toml"]));
        let pi = declaration(&PiAdapter::default(), &ctx(&supplied, true), Mode::Managed);
        assert!(settings(&pi, &[".pi/settings.json"]));
    }

    #[test]
    fn pi_retains_all_five_priority_names_and_global_system_candidates() {
        let supplied = [EnvVar::new("PI_CODING_AGENT_DIR", "profiles/pi")];
        let manifest = declaration(&PiAdapter::default(), &ctx(&supplied, true), Mode::Managed);
        assert_eq!(manifest.mode, DeclarationMode::PiManaged);
        assert_eq!(
            manifest.inputs[0].rule,
            DiscoveryRule::FixedCandidates {
                names: names(&[
                    "AGENTS.override.md",
                    "AGENTS.md",
                    "AGENTS.MD",
                    "CLAUDE.md",
                    "CLAUDE.MD"
                ]),
                selection: CandidateSelection::FirstReadable,
            }
        );
        assert_eq!(
            manifest.inputs[1].rule,
            DiscoveryRule::FixedCandidates {
                names: names(&["SYSTEM.md", "APPEND_SYSTEM.md"]),
                selection: CandidateSelection::All,
            }
        );
        assert_eq!(
            manifest.inputs[4].rule,
            DiscoveryRule::FixedCandidates {
                names: names(&[".pi/SYSTEM.md", ".pi/APPEND_SYSTEM.md"]),
                selection: CandidateSelection::All,
            }
        );
        assert_eq!(
            manifest.inputs[4].activation,
            Activation::Inactive(InactiveReason::PiNoApprove)
        );
        assert!(matches!(
            &manifest.inputs[2].rule,
            DiscoveryRule::AncestorCandidates {
                boundary: AncestorBoundary::FilesystemRoot,
                ..
            }
        ));
        assert!(
            manifest
                .gaps
                .contains(&DeclarationGap::PiEffectiveConfiguration)
        );
    }

    #[test]
    fn pi_normalized_selector_forms_are_unresolved_instead_of_literal_roots() {
        for selector in ["~", "~/.pi/agent", "file:///selected/pi"] {
            let supplied = [EnvVar::new("PI_CODING_AGENT_DIR", selector)];
            let manifest = declaration(&PiAdapter::default(), &ctx(&supplied, true), Mode::Managed);
            assert!(manifest.gaps.contains(&DeclarationGap::PathUnresolved));
            assert!(matches!(
                manifest.inputs[0].rule,
                DiscoveryRule::AncestorCandidates { .. }
            ));
            assert!(
                !manifest
                    .inputs
                    .iter()
                    .any(|input| matches!(input.root, InstructionRoot::ProfileDirectory { .. }))
            );
        }
        for selector in ["profiles/pi", "~literal", "file:literal"] {
            let supplied = [EnvVar::new("PI_CODING_AGENT_DIR", selector)];
            let manifest = declaration(&PiAdapter::default(), &ctx(&supplied, true), Mode::Managed);
            assert_eq!(
                manifest.inputs[0].root,
                InstructionRoot::ProfileDirectory {
                    profile: ProfileKind::Pi,
                    path: selector.into(),
                }
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn pi_windows_shell_drive_selectors_are_not_treated_as_rooted_host_paths() {
        for selector in [
            "/c/selected/pi",
            "/mnt/c/selected/pi",
            "/CYGDRIVE/C/selected/pi",
            r"~\selected\pi",
        ] {
            let supplied = [EnvVar::new("PI_CODING_AGENT_DIR", selector)];
            let manifest = declaration(&PiAdapter::default(), &ctx(&supplied, true), Mode::Managed);
            assert!(manifest.gaps.contains(&DeclarationGap::PathUnresolved));
            assert!(matches!(
                manifest.inputs[0].rule,
                DiscoveryRule::AncestorCandidates { .. }
            ));
            assert!(
                !manifest
                    .inputs
                    .iter()
                    .any(|input| matches!(input.root, InstructionRoot::ProfileDirectory { .. }))
            );
        }
    }

    #[test]
    fn home_fallbacks_and_missing_profiles_use_only_supplied_path_selectors() {
        let home_name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let supplied = [
            EnvVar::new(home_name, "operator-home"),
            EnvVar::new("ANTHROPIC_API_KEY", "private-credential"),
        ];
        let manifest = declaration(
            &ClaudeAdapter::default(),
            &ctx(&supplied, true),
            Mode::Managed,
        );
        assert!(manifest.inputs.iter().any(|input| input.root
            == InstructionRoot::ImportHomeDirectory("operator-home".into())
            && input.rule == DiscoveryRule::ImportHome));
        assert!(manifest.inputs.iter().any(|input| input.root
            == InstructionRoot::ProfileDirectory {
                profile: ProfileKind::Claude,
                path: PathBuf::from("operator-home").join(".claude"),
            }));
        assert!(!format!("{manifest:?}").contains("private-credential"));
        for (adapter, kind) in [
            (
                &CodexAdapter::default() as &dyn Adapter,
                ProfileKind::CodexLegacy,
            ),
            (&PiAdapter::default() as &dyn Adapter, ProfileKind::Pi),
        ] {
            let absent = declaration(adapter, &ctx(&[], true), Mode::Managed);
            assert!(
                absent
                    .gaps
                    .contains(&DeclarationGap::ProfileUnavailable(kind))
            );
            let supplied = declaration(adapter, &ctx(&supplied, true), Mode::Managed);
            assert!(supplied.inputs.iter().any(|input| matches!(&input.root, InstructionRoot::ProfileDirectory { profile, .. } if *profile == kind)));
        }
    }

    #[test]
    fn duplicate_profile_selectors_follow_the_sorted_child_environment() {
        let supplied = [
            EnvVar::new("CODEX_HOME", "z-selected"),
            EnvVar::new("CODEX_HOME", "a-shadowed"),
        ];
        let manifest = declaration(
            &CodexAdapter::default(),
            &ctx(&supplied, true),
            Mode::Managed,
        );
        assert_eq!(
            manifest.inputs[0].root,
            InstructionRoot::ProfileDirectory {
                profile: ProfileKind::CodexLegacy,
                path: PathBuf::from("z-selected"),
            }
        );
    }

    #[test]
    fn argument_scan_preserves_known_inputs_and_does_not_inspect_the_omitted_tail() {
        let mut args = vec!["--system-prompt-file".into(), "known.md".into()];
        args.extend(std::iter::repeat_n(OsString::from("--unrecognized"), 510));
        args.push(OsString::from("x".repeat(MAX_INSTRUCTION_PATH_BYTES + 100)));
        let adapter = ClaudeAdapter::new(ClaudeConfig {
            extra_args: args,
            ..ClaudeConfig::default()
        });
        let manifest = declaration(&adapter, &ctx(&[], true), Mode::Managed);
        assert!(
            manifest
                .inputs
                .iter()
                .any(|input| { input.root == InstructionRoot::ExplicitPath("known.md".into()) })
        );
        assert!(manifest.gaps.contains(&DeclarationGap::DeclarationLimit));
        assert!(!manifest.gaps.contains(&DeclarationGap::MetadataLimit));

        let inspected = ClaudeAdapter::new(ClaudeConfig {
            extra_args: vec![
                "--system-prompt-file".into(),
                "x".repeat(MAX_INSTRUCTION_PATH_BYTES + 1).into(),
            ],
            ..ClaudeConfig::default()
        });
        assert!(
            declaration(&inspected, &ctx(&[], true), Mode::Managed)
                .gaps
                .contains(&DeclarationGap::MetadataLimit)
        );
    }

    #[test]
    fn multi_rule_arguments_stop_at_the_shared_input_cap_and_keep_the_admitted_prefix() {
        let args = (0..170)
            .flat_map(|index| {
                [
                    OsString::from("--add-dir"),
                    OsString::from(format!("additional/{index}")),
                ]
            })
            .collect();
        let adapter = ClaudeAdapter::new(ClaudeConfig {
            extra_args: args,
            ..ClaudeConfig::default()
        });
        let manifest = declaration(&adapter, &ctx(&[], true), Mode::Managed);
        assert_eq!(manifest.inputs.len(), MAX_INSTRUCTION_INPUTS);
        assert!(manifest.gaps.contains(&DeclarationGap::DeclarationLimit));
        assert_eq!(
            manifest.inputs.last().unwrap().root,
            InstructionRoot::AdditionalDirectory("additional/166".into())
        );
        assert!(manifest.inputs.iter().any(|input| {
            input.root == InstructionRoot::AdditionalDirectory("additional/0".into())
                && input.rule
                    == DiscoveryRule::MarkdownRules {
                        directory: ".claude/rules".into(),
                        scope: RuleDirectoryScope::Root,
                    }
        }));
    }

    #[test]
    fn escaped_path_overflow_retains_other_known_explicit_inputs() {
        let oversized = "\u{1}".repeat(800);
        assert!(oversized.len() < MAX_INSTRUCTION_PATH_BYTES);
        let adapter = ClaudeAdapter::new(ClaudeConfig {
            extra_args: vec![
                "--system-prompt-file".into(),
                "before.md".into(),
                "--system-prompt-file".into(),
                oversized.into(),
                "--append-system-prompt-file".into(),
                "after.md".into(),
            ],
            ..ClaudeConfig::default()
        });
        let manifest = declaration(&adapter, &ctx(&[], true), Mode::Managed);
        let files: Vec<_> = manifest
            .inputs
            .iter()
            .filter_map(|input| match &input.root {
                InstructionRoot::ExplicitPath(path) => Some(path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            files,
            vec![PathBuf::from("before.md"), PathBuf::from("after.md")]
        );
        assert!(manifest.gaps.contains(&DeclarationGap::MetadataLimit));
    }

    #[test]
    fn inline_selector_prefix_does_not_spend_the_encoded_path_budget() {
        let overhead = serde_json::to_vec(&NativeInstructionPath::Utf8 {
            value: String::new(),
        })
        .unwrap()
        .len();
        let path = "x".repeat(MAX_INSTRUCTION_PATH_BYTES - overhead);
        let adapter = ClaudeAdapter::new(ClaudeConfig {
            extra_args: vec![format!("--append-subagent-system-prompt-file={path}").into()],
            ..ClaudeConfig::default()
        });
        let manifest = declaration(&adapter, &ctx(&[], true), Mode::Managed);
        assert!(
            manifest
                .inputs
                .iter()
                .any(|input| input.root == InstructionRoot::ExplicitPath(PathBuf::from(&path)))
        );
        assert!(!manifest.gaps.contains(&DeclarationGap::MetadataLimit));
    }

    #[test]
    fn oversized_profiles_and_execution_paths_are_omitted_without_losing_other_roots() {
        let oversized = "x".repeat(MAX_INSTRUCTION_PATH_BYTES + 1);
        let supplied = [EnvVar::new("CLAUDE_CONFIG_DIR", oversized.clone())];
        let manifest = declaration(
            &ClaudeAdapter::default(),
            &ctx(&supplied, true),
            Mode::Managed,
        );
        assert!(manifest.gaps.contains(&DeclarationGap::MetadataLimit));
        assert_eq!(
            project_ancestors(&manifest)[0].0,
            names(&["CLAUDE.md", ".claude/CLAUDE.md"])
        );
        assert!(
            !manifest
                .inputs
                .iter()
                .any(|input| matches!(input.root, InstructionRoot::ProfileDirectory { .. }))
        );

        let supplied = [EnvVar::new("CLAUDE_CONFIG_DIR", "profiles/claude")];
        let context = AgentCtx {
            cwd: Path::new(&oversized),
            agent_dir: Path::new(&oversized),
            ..ctx(&supplied, true)
        };
        let attached = declaration(&ClaudeAdapter::default(), &context, Mode::Attached);
        assert!(attached.gaps.contains(&DeclarationGap::MetadataLimit));
        assert!(attached.inputs.iter().any(|input| input.root
            == InstructionRoot::ProfileDirectory {
                profile: ProfileKind::Claude,
                path: "profiles/claude".into()
            }));
        assert!(!attached.inputs.iter().any(|input| matches!(
            input.root,
            InstructionRoot::ExecutionDirectory(_) | InstructionRoot::DaemonDirectory(_)
        )));
    }

    #[test]
    fn environment_scan_keeps_a_bounded_candidate_and_names_uninspected_selectors() {
        let mut supplied = vec![EnvVar::new("CODEX_HOME", "a-known")];
        supplied.extend(std::iter::repeat_n(EnvVar::new("IGNORED", "unused"), 511));
        supplied.push(EnvVar::new("CODEX_HOME", "z-omitted"));
        let manifest = declaration(
            &CodexAdapter::default(),
            &ctx(&supplied, true),
            Mode::Managed,
        );
        assert_eq!(
            manifest.inputs[0].root,
            InstructionRoot::ProfileDirectory {
                profile: ProfileKind::CodexLegacy,
                path: "a-known".into()
            }
        );
        assert!(manifest.gaps.contains(&DeclarationGap::DeclarationLimit));
    }

    #[test]
    fn oversized_duplicate_selectors_are_uncertain_before_value_comparison() {
        let supplied = [
            EnvVar::new("CODEX_HOME", "x".repeat(MAX_INSTRUCTION_PATH_BYTES + 1)),
            EnvVar::new("CODEX_HOME", "z-known"),
        ];
        let manifest = declaration(
            &CodexAdapter::default(),
            &ctx(&supplied, true),
            Mode::Managed,
        );
        assert!(manifest.gaps.contains(&DeclarationGap::MetadataLimit));
        assert_eq!(
            project_ancestors(&manifest)[0].0,
            names(&["AGENTS.override.md", "AGENTS.md"])
        );
        assert!(
            !manifest
                .inputs
                .iter()
                .any(|input| matches!(input.root, InstructionRoot::ProfileDirectory { .. }))
        );
    }

    #[test]
    fn oversized_setting_sources_are_unknown_only_when_the_composer_uses_them() {
        let configured = vec![SettingSource::Project; MAX_INSTRUCTION_INPUTS + 1];
        let default = ClaudeAdapter::new(ClaudeConfig {
            setting_sources: Some(configured.clone()),
            ..ClaudeConfig::default()
        });
        let manifest = declaration(&default, &ctx(&[], true), Mode::Managed);
        assert!(manifest.gaps.contains(&DeclarationGap::DeclarationLimit));
        assert_eq!(
            project_ancestors(&manifest)[0].1,
            Activation::Conditional(DiscoveryCondition::ClaudeEffectiveConfiguration)
        );

        let bare = ClaudeAdapter::new(ClaudeConfig {
            isolation: IsolationMode::Bare,
            setting_sources: Some(configured),
            ..ClaudeConfig::default()
        });
        let manifest = declaration(&bare, &ctx(&[], true), Mode::Managed);
        assert_eq!(
            project_ancestors(&manifest)[0].1,
            Activation::Inactive(InactiveReason::ClaudeBare)
        );
        assert!(!manifest.gaps.contains(&DeclarationGap::DeclarationLimit));
    }

    #[cfg(unix)]
    #[test]
    fn separate_native_path_values_are_lossless_and_inline_encoding_is_unknown() {
        use std::os::unix::ffi::OsStringExt;

        let path = OsString::from_vec(b"prompt-\xff.md".to_vec());
        let adapter = ClaudeAdapter::new(ClaudeConfig {
            extra_args: vec![
                "--system-prompt-file".into(),
                path.clone(),
                OsString::from_vec(b"--append-system-prompt-file=\xff.md".to_vec()),
            ],
            ..ClaudeConfig::default()
        });
        let manifest = declaration(&adapter, &ctx(&[], true), Mode::Managed);
        assert!(
            manifest
                .inputs
                .iter()
                .any(|input| input.root
                    == InstructionRoot::ExplicitPath(PathBuf::from(path.clone())))
        );
        assert!(manifest.gaps.contains(&DeclarationGap::ArgumentEncoding));
    }

    #[cfg(unix)]
    #[test]
    fn native_byte_encoding_overflow_is_metadata_uncertainty() {
        use std::os::unix::ffi::OsStringExt;

        let adapter = ClaudeAdapter::new(ClaudeConfig {
            extra_args: vec![
                "--system-prompt-file".into(),
                OsString::from_vec(vec![0xff; 2050]),
                "--append-system-prompt-file".into(),
                "known.md".into(),
            ],
            ..ClaudeConfig::default()
        });
        let manifest = declaration(&adapter, &ctx(&[], true), Mode::Managed);
        assert!(manifest.gaps.contains(&DeclarationGap::MetadataLimit));
        assert!(
            manifest
                .inputs
                .iter()
                .any(|input| { input.root == InstructionRoot::ExplicitPath("known.md".into()) })
        );
    }

    struct Undeclared;

    impl Adapter for Undeclared {
        fn provider(&self) -> Provider {
            Provider::Pi
        }
        fn program(&self) -> &OsStr {
            OsStr::new("stand-in")
        }
        fn tier(&self) -> dudes_types::Tier {
            dudes_types::Tier::ObservedOnly
        }
        fn isolation(&self) -> Isolation {
            Isolation::None
        }
        fn session_acquisition(&self) -> SessionAcquisition {
            crate::pi::SESSION_ACQUISITION
        }
        fn model_selection(&self) -> ModelSelection {
            ModelSelection::provider_default()
        }
        fn managed(
            &self,
            _ctx: &AgentCtx<'_>,
            _task: &str,
            _session: &TurnSession,
            _selection: &ModelSelection,
        ) -> Result<LaunchSpec, LaunchError> {
            Err(LaunchError::EmptyTask)
        }
        fn turn_reader(&self) -> Box<dyn TurnReader> {
            PiAdapter::default().turn_reader()
        }
        fn managed_install(
            &self,
            _ctx: &AgentCtx<'_>,
            _selection: &ModelSelection,
        ) -> ManagedInstall {
            ManagedInstall::default()
        }
        fn usage_source(&self) -> UsageSource {
            UsageSource::None
        }
        fn sandbox_flag(&self) -> Option<&'static [&'static str]> {
            None
        }
        fn billing_env_vars(&self) -> &'static [&'static str] {
            &[]
        }
    }

    #[test]
    fn default_and_unsupported_modes_are_named_unknown_declarations() {
        for adapter in [
            &Undeclared as &dyn Adapter,
            &CodexAdapter::default() as &dyn Adapter,
            &PiAdapter::default() as &dyn Adapter,
        ] {
            let manifest = declaration(adapter, &ctx(&[], true), Mode::Attached);
            assert_eq!(
                manifest.mode,
                DeclarationMode::Unsupported {
                    provider: adapter.provider(),
                    mode: Mode::Attached
                }
            );
            assert_eq!(manifest.gaps, vec![DeclarationGap::DeclarationMissing]);
        }
        let default = declaration(&Undeclared, &ctx(&[], true), Mode::Managed);
        assert_eq!(default.gaps, vec![DeclarationGap::DeclarationMissing]);
    }
}
