use std::path::{Path, PathBuf};

use dudes_proto::instruction::{
    InstructionActivation, InstructionAncestorStop, InstructionCandidate, InstructionCondition,
    InstructionCoverageGap, InstructionDeclarationMode, InstructionDiscoveryEvidence,
    InstructionDiscoveryEvidenceKind, InstructionDiscoveryPlan, InstructionDiscoveryRule,
    InstructionGapKind, InstructionGapOrigin, InstructionImportPolicy, InstructionParticipant,
    InstructionPathContext, InstructionRoot, InstructionRootKind, InstructionRuleDirectoryScope,
    InstructionSelection, InstructionSharedInput, InstructionValidationError,
    MAX_CLAUDE_IMPORT_DEPTH, MAX_INSTRUCTION_INPUTS, MAX_INSTRUCTION_METADATA_BYTES,
    MAX_INSTRUCTION_PATH_BYTES, NativeInstructionPath,
};
use dudes_proto::record::Hex32;
use dudes_providers::instructions::{
    Activation, AncestorBoundary, CandidateSelection, DeclarationGap, DeclarationMode,
    DiscoveryCondition, DiscoveryRule, EvidenceRevision, ImportRule, InstructionDeclaration,
    InstructionInput, InstructionRoot as ProviderRoot, ResourceKind, RuleDirectoryScope,
};
use dudes_types::{AgentId, IsolationMode, Provider, WorktreeId};
use sha2::{Digest as _, Sha256};

pub(super) enum ProjectInstructionContext {
    OwnedWorktree(WorktreeId),
    Shared,
    UnsharedAfterCompleteCensus,
}

impl ProjectInstructionContext {
    fn path_context(&self) -> InstructionPathContext {
        match self {
            Self::OwnedWorktree(lease) => InstructionPathContext::OwnedWorktree { lease: *lease },
            Self::Shared => InstructionPathContext::SharedProject,
            Self::UnsharedAfterCompleteCensus => InstructionPathContext::UnsharedProject,
        }
    }
}

pub(super) struct InstructionEvidenceDigests {
    pub(super) claude_documentation: Hex32,
    pub(super) codex_source: Hex32,
    pub(super) pi_source: Hex32,
    pub(super) daemon_composition: Hex32,
}

impl InstructionEvidenceDigests {
    fn evidence(&self, revision: EvidenceRevision) -> InstructionDiscoveryEvidence {
        let (kind, digest) = match revision {
            EvidenceRevision::ClaudeCurrentDocumentation => (
                InstructionDiscoveryEvidenceKind::VendorDocumentation,
                self.claude_documentation,
            ),
            EvidenceRevision::Codex01601 => (
                InstructionDiscoveryEvidenceKind::VersionedSource,
                self.codex_source,
            ),
            EvidenceRevision::Pi104 => (
                InstructionDiscoveryEvidenceKind::VersionedSource,
                self.pi_source,
            ),
            EvidenceRevision::DudesComposition => (
                InstructionDiscoveryEvidenceKind::DaemonComposition,
                self.daemon_composition,
            ),
        };
        InstructionDiscoveryEvidence { kind, digest }
    }
}

/// Supplied only from actual runtime admission evidence. A configured or guarded home
/// establishes neither effective project trust nor complete configuration exclusion.
#[derive(Clone, Copy)]
pub(super) struct CodexInstructionAdmissionFacts {
    pub(super) receipt_digest: Hex32,
    /// The effective loader project is proved untrusted.
    pub(super) project_untrusted: bool,
    /// Every effective instruction-bearing configuration source is proved excluded, including
    /// model-file and inline routes. A profile-only guard must leave this false.
    pub(super) instruction_configuration_excluded: bool,
}

pub(super) struct InstructionPlanFacts<'a> {
    pub(super) canonical_execution_folder: &'a Path,
    pub(super) project_context: ProjectInstructionContext,
    pub(super) evidence: &'a InstructionEvidenceDigests,
    pub(super) daemon_configuration_digest: Option<Hex32>,
    pub(super) codex_guard: Option<CodexInstructionAdmissionFacts>,
}

pub(super) fn instruction_participant(
    agent: AgentId,
    provider: Provider,
    declaration: &InstructionDeclaration,
    facts: &InstructionPlanFacts<'_>,
) -> InstructionParticipant {
    let configuration = configuration_digest(declaration, facts);
    let mut builder = PlanBuilder::new(agent, configuration);
    let mode = if mode_provider(declaration.mode) == Some(provider) {
        declaration_mode(declaration.mode)
    } else {
        builder.gap(
            InstructionGapKind::DeclarationMissing,
            InstructionPathContext::Unresolved,
        );
        InstructionDeclarationMode::Unknown
    };
    let mut digest = DeclarationDigest::new();
    digest.text("instruction-declaration-v1");
    digest.text(&format!("{provider:?}"));
    digest.text(&format!("{:?}", declaration.mode));
    digest.bytes(&declaration.revision.to_le_bytes());
    if let Some(configuration) = configuration {
        digest.bytes(&configuration.bytes());
    } else {
        digest.text("configuration-unknown");
    }
    if declaration.mode == DeclarationMode::PiManaged {
        builder.gap_from(
            InstructionGapKind::PathUnresolved,
            InstructionPathContext::Unresolved,
            InstructionGapOrigin::Declared,
        );
        if facts
            .canonical_execution_folder
            .as_os_str()
            .as_encoded_bytes()
            .len()
            <= MAX_INSTRUCTION_PATH_BYTES
            && let Ok(path) = NativeInstructionPath::from_path(facts.canonical_execution_folder)
            && let Some(gap) = builder.plan.gaps.last_mut()
        {
            gap.path = Some(path);
        }
        builder.gap_from(
            InstructionGapKind::ConfigurationUnknown,
            shared(InstructionSharedInput::ExplicitFile),
            InstructionGapOrigin::Declared,
        );
    }
    digest.path(facts.canonical_execution_folder);
    digest.text(&format!("{:?}", facts.project_context.path_context()));
    if let Some(root) = encode_path(facts.canonical_execution_folder, &mut builder) {
        builder.root(InstructionRoot {
            path: root,
            kind: InstructionRootKind::Execution,
            context: facts.project_context.path_context(),
        });
    }
    for gap in declaration.gaps.iter().take(MAX_INSTRUCTION_INPUTS) {
        digest.text(&format!("{gap:?}"));
        if !discharged(*gap, declaration.mode, facts.codex_guard) {
            builder.gap_from(map_gap(*gap), gap_context(*gap), gap_origin(*gap));
        }
    }
    if declaration.gaps.len() > MAX_INSTRUCTION_INPUTS
        || declaration.inputs.len() > MAX_INSTRUCTION_INPUTS
    {
        builder.gap(
            InstructionGapKind::InputLimit,
            InstructionPathContext::Unresolved,
        );
    }
    digest.bytes(&(declaration.inputs.len() as u64).to_le_bytes());
    for input in declaration.inputs.iter().take(MAX_INSTRUCTION_INPUTS) {
        digest.input(input, facts.evidence);
        if digest.limited {
            builder.gap(
                InstructionGapKind::MetadataLimit,
                InstructionPathContext::Unresolved,
            );
            break;
        }
        map_input(input, declaration.mode, facts, &mut builder);
    }
    let discovery = builder.finish();
    for gap in &discovery.gaps {
        digest.text(&format!("{:?}:{:?}", gap.reason, gap.context));
    }
    InstructionParticipant {
        agent,
        provider,
        mode,
        declaration_digest: digest.finish(),
        discovery,
    }
}

fn configuration_digest(
    declaration: &InstructionDeclaration,
    facts: &InstructionPlanFacts<'_>,
) -> Option<Hex32> {
    let mut digest = DeclarationDigest::new();
    digest.text("instruction-configuration-v1");
    let mut present = false;
    if let Some(composition) = facts.daemon_configuration_digest {
        digest.bytes(&composition.bytes());
        present = true;
    }
    if declaration.mode == DeclarationMode::CodexStreaming
        && let Some(guard) = facts.codex_guard
    {
        digest.bytes(&guard.receipt_digest.bytes());
        digest.bytes(&[
            u8::from(guard.project_untrusted),
            u8::from(guard.instruction_configuration_excluded),
        ]);
        present = true;
    }
    for input in declaration.inputs.iter().take(MAX_INSTRUCTION_INPUTS) {
        if matches!(input.rule, DiscoveryRule::GeneratedConfig { .. }) {
            digest.input(input, facts.evidence);
        }
    }
    present.then(|| digest.finish())
}

struct DeclarationDigest {
    hasher: Sha256,
    remaining: usize,
    limited: bool,
}

impl DeclarationDigest {
    fn new() -> Self {
        Self {
            hasher: Sha256::new(),
            remaining: MAX_INSTRUCTION_METADATA_BYTES,
            limited: false,
        }
    }

    fn bytes(&mut self, value: &[u8]) {
        if self.limited {
            return;
        }
        if value.len() + 8 > self.remaining {
            self.hasher.update(b"metadata-limit");
            self.limited = true;
            return;
        }
        self.hasher.update((value.len() as u64).to_le_bytes());
        self.hasher.update(value);
        self.remaining -= value.len() + 8;
    }

    fn text(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn path(&mut self, path: &Path) {
        if self.limited {
            return;
        }
        if path.as_os_str().as_encoded_bytes().len() > MAX_INSTRUCTION_PATH_BYTES {
            self.text("path-limit");
            self.bytes(&(path.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
            return;
        }
        if let Ok(path) = NativeInstructionPath::from_path(path) {
            self.bytes(&serde_json::to_vec(&path).expect("native path serializes"));
        } else {
            self.text("invalid-path");
            self.bytes(path.as_os_str().as_encoded_bytes());
        }
    }

    fn paths(&mut self, paths: &[PathBuf]) {
        if self.limited {
            return;
        }
        self.bytes(&(paths.len() as u64).to_le_bytes());
        for path in paths.iter().take(MAX_INSTRUCTION_INPUTS) {
            self.path(path);
            if self.limited {
                break;
            }
        }
    }

    fn input(&mut self, input: &InstructionInput, evidence: &InstructionEvidenceDigests) {
        if self.limited {
            return;
        }
        let root = match &input.root {
            ProviderRoot::ExecutionDirectory(path) => {
                self.text("execution");
                path
            }
            ProviderRoot::ProfileDirectory { profile, path } => {
                self.text(&format!("profile:{profile:?}"));
                path
            }
            ProviderRoot::AdditionalDirectory(path) => {
                self.text("additional");
                path
            }
            ProviderRoot::ManagedPolicyDirectory(path) => {
                self.text("policy");
                path
            }
            ProviderRoot::DaemonDirectory(path) => {
                self.text("daemon");
                path
            }
            ProviderRoot::ExplicitPath(path) => {
                self.text("explicit");
                path
            }
            ProviderRoot::ImportHomeDirectory(path) => {
                self.text("import-home");
                path
            }
        };
        self.path(root);
        self.text(&format!(
            "{:?}:{:?}:{:?}",
            input.activation, input.imports, input.evidence
        ));
        self.bytes(&evidence.evidence(input.evidence).digest.bytes());
        match &input.rule {
            DiscoveryRule::FixedCandidates { names, selection } => {
                self.text(&format!("fixed:{selection:?}"));
                self.paths(names);
            }
            DiscoveryRule::AncestorCandidates {
                names,
                selection,
                boundary,
            } => {
                self.text(&format!("ancestors:{selection:?}"));
                self.paths(names);
                match boundary {
                    AncestorBoundary::FilesystemRoot => self.text("filesystem-root"),
                    AncestorBoundary::CodexRootMarkers { defaults } => {
                        self.text("unknown-effective-markers");
                        self.paths(defaults);
                    }
                }
            }
            DiscoveryRule::DescendantCandidates { names, selection } => {
                self.text(&format!("descendants:{selection:?}"));
                self.paths(names);
            }
            DiscoveryRule::MarkdownRules { directory, scope } => {
                self.text(&format!("markdown:{scope:?}"));
                self.path(directory);
            }
            DiscoveryRule::ExplicitFile { purpose } => self.text(&format!("file:{purpose:?}")),
            DiscoveryRule::ExplicitResource(kind) => self.text(&format!("resource:{kind:?}")),
            DiscoveryRule::GeneratedConfig { file, kind } => {
                self.text(&format!("generated:{kind:?}"));
                self.path(file);
            }
            DiscoveryRule::ImportHome => self.text("import-home-resolution-only"),
        }
    }

    fn finish(self) -> Hex32 {
        Hex32::new(self.hasher.finalize().into())
    }
}

struct PlanBuilder {
    agent: AgentId,
    plan: InstructionDiscoveryPlan,
    overflow: bool,
}

impl PlanBuilder {
    fn new(agent: AgentId, configuration_digest: Option<Hex32>) -> Self {
        Self {
            agent,
            plan: InstructionDiscoveryPlan {
                roots: Vec::new(),
                rules: Vec::new(),
                configuration_digest,
                gaps: vec![InstructionCoverageGap {
                    agent: Some(agent),
                    ..InstructionCoverageGap::metadata_limit()
                }],
            },
            overflow: false,
        }
    }

    fn gap(&mut self, reason: InstructionGapKind, context: InstructionPathContext) {
        self.gap_from(reason, context, InstructionGapOrigin::Observed);
    }

    fn gap_from(
        &mut self,
        reason: InstructionGapKind,
        context: InstructionPathContext,
        origin: InstructionGapOrigin,
    ) {
        let gap = InstructionCoverageGap {
            origin,
            reason,
            context,
            agent: Some(self.agent),
            path: None,
        };
        if reason == InstructionGapKind::MetadataLimit {
            self.overflow = true;
        }
        if !self.plan.gaps.contains(&gap) {
            self.plan.gaps.push(gap);
        }
    }

    fn root(&mut self, root: InstructionRoot) -> Option<u16> {
        if let Some(index) = self
            .plan
            .roots
            .iter()
            .position(|existing| existing == &root)
        {
            return u16::try_from(index).ok();
        }
        if self.plan.roots.len() >= MAX_INSTRUCTION_INPUTS {
            self.gap(
                InstructionGapKind::InputLimit,
                InstructionPathContext::Unresolved,
            );
            return None;
        }
        self.plan.roots.push(root);
        if self.plan.validate().is_err() {
            self.plan.roots.pop();
            self.gap(
                InstructionGapKind::MetadataLimit,
                InstructionPathContext::Unresolved,
            );
            return None;
        }
        u16::try_from(self.plan.roots.len() - 1).ok()
    }

    fn rule(&mut self, rule: InstructionDiscoveryRule) {
        if self.plan.rules.len() >= MAX_INSTRUCTION_INPUTS {
            self.gap(
                InstructionGapKind::InputLimit,
                InstructionPathContext::Unresolved,
            );
            return;
        }
        self.plan.rules.push(rule);
        if let Err(error) = self.plan.validate() {
            self.plan.rules.pop();
            self.gap(validation_gap(error), InstructionPathContext::Unresolved);
        }
    }

    fn finish(mut self) -> InstructionDiscoveryPlan {
        if !self.overflow {
            self.plan.gaps.remove(0);
        }
        while self.plan.validate().is_err() {
            self.overflow = true;
            self.gap(
                InstructionGapKind::MetadataLimit,
                InstructionPathContext::Unresolved,
            );
            if self.plan.rules.pop().is_none() && self.plan.roots.pop().is_none() {
                break;
            }
        }
        self.plan
    }
}

fn encode_path(path: &Path, builder: &mut PlanBuilder) -> Option<NativeInstructionPath> {
    if path.as_os_str().as_encoded_bytes().len() > MAX_INSTRUCTION_PATH_BYTES {
        builder.gap(
            InstructionGapKind::MetadataLimit,
            InstructionPathContext::Unresolved,
        );
        return None;
    }
    match NativeInstructionPath::from_path(path) {
        Ok(path) => Some(path),
        Err(error) => {
            builder.gap(validation_gap(error), InstructionPathContext::Unresolved);
            None
        }
    }
}

fn encoded_names(paths: &[PathBuf], builder: &mut PlanBuilder) -> Vec<NativeInstructionPath> {
    if paths.len() > MAX_INSTRUCTION_INPUTS {
        builder.gap(
            InstructionGapKind::InputLimit,
            InstructionPathContext::Unresolved,
        );
    }
    paths
        .iter()
        .take(MAX_INSTRUCTION_INPUTS)
        .filter_map(|path| encode_path(path, builder))
        .collect()
}

fn shared(source: InstructionSharedInput) -> InstructionPathContext {
    InstructionPathContext::SharedInput { source }
}

fn map_input(
    input: &InstructionInput,
    mode: DeclarationMode,
    facts: &InstructionPlanFacts<'_>,
    builder: &mut PlanBuilder,
) {
    let (path, kind, context) = input_root(input, facts);
    let Some(path) = encode_path(path, builder) else {
        return;
    };
    let Some(root) = builder.root(InstructionRoot {
        path,
        kind,
        context,
    }) else {
        return;
    };
    if matches!(input.rule, DiscoveryRule::ImportHome) {
        return;
    }
    if kind == InstructionRootKind::ImportHome {
        builder.gap(
            InstructionGapKind::DeclarationMissing,
            shared(InstructionSharedInput::Import),
        );
        return;
    }
    let Some((candidate, selection)) = input_candidate(input, facts, builder, context) else {
        return;
    };
    let activation = if mode == DeclarationMode::CodexStreaming
        && !facts
            .codex_guard
            .is_some_and(|guard| guard.project_untrusted)
        && input.activation
            == Activation::Inactive(
                dudes_providers::instructions::InactiveReason::CodexUntrustedProject,
            ) {
        InstructionActivation::Conditional {
            condition: InstructionCondition::Trust,
        }
    } else {
        map_activation(input.activation)
    };
    let imports = match input.imports {
        ImportRule::None => InstructionImportPolicy::None,
        ImportRule::ClaudeMemory { max_hops } if max_hops == MAX_CLAUDE_IMPORT_DEPTH => {
            InstructionImportPolicy::ClaudeFourHops
        }
        ImportRule::ClaudeMemory { .. } => {
            builder.gap(InstructionGapKind::ImportLimit, context);
            InstructionImportPolicy::None
        }
    };
    builder.rule(InstructionDiscoveryRule {
        root,
        candidate,
        selection,
        activation,
        imports,
        evidence: facts.evidence.evidence(input.evidence),
    });
}

fn input_root<'a>(
    input: &'a InstructionInput,
    facts: &InstructionPlanFacts<'a>,
) -> (&'a Path, InstructionRootKind, InstructionPathContext) {
    let (path, kind, context) = match &input.root {
        ProviderRoot::ExecutionDirectory(_) => (
            facts.canonical_execution_folder,
            InstructionRootKind::Execution,
            facts.project_context.path_context(),
        ),
        ProviderRoot::ProfileDirectory { path, .. } => (
            path.as_path(),
            InstructionRootKind::Profile,
            shared(InstructionSharedInput::Profile),
        ),
        ProviderRoot::AdditionalDirectory(path) => (
            path.as_path(),
            InstructionRootKind::Extra,
            shared(InstructionSharedInput::ExplicitFile),
        ),
        ProviderRoot::ManagedPolicyDirectory(path) => (
            path.as_path(),
            InstructionRootKind::ManagedPolicy,
            InstructionPathContext::Unresolved,
        ),
        ProviderRoot::DaemonDirectory(path) => (
            path.as_path(),
            InstructionRootKind::DaemonConfiguration,
            InstructionPathContext::Unresolved,
        ),
        ProviderRoot::ExplicitPath(path)
            if matches!(input.rule, DiscoveryRule::ExplicitResource(_)) =>
        {
            (
                path.as_path(),
                InstructionRootKind::Extra,
                shared(InstructionSharedInput::ExplicitFile),
            )
        }
        ProviderRoot::ExplicitPath(_) => (
            facts.canonical_execution_folder,
            InstructionRootKind::ExplicitFile,
            shared(InstructionSharedInput::ExplicitFile),
        ),
        ProviderRoot::ImportHomeDirectory(path) => (
            path.as_path(),
            InstructionRootKind::ImportHome,
            shared(InstructionSharedInput::Import),
        ),
    };
    if matches!(
        input.rule,
        DiscoveryRule::AncestorCandidates { .. }
            | DiscoveryRule::MarkdownRules {
                scope: RuleDirectoryScope::Ancestors,
                ..
            }
    ) {
        (
            path,
            InstructionRootKind::Ancestor,
            shared(InstructionSharedInput::Ancestor),
        )
    } else {
        (path, kind, context)
    }
}

fn input_candidate(
    input: &InstructionInput,
    facts: &InstructionPlanFacts<'_>,
    builder: &mut PlanBuilder,
    context: InstructionPathContext,
) -> Option<(InstructionCandidate, InstructionSelection)> {
    Some(match &input.rule {
        DiscoveryRule::FixedCandidates { names, selection } => (
            InstructionCandidate::Fixed {
                paths: encoded_names(names, builder),
            },
            map_selection(*selection),
        ),
        DiscoveryRule::AncestorCandidates {
            names,
            selection,
            boundary,
        } => {
            let stop = match boundary {
                AncestorBoundary::FilesystemRoot => InstructionAncestorStop::FileSystemRoot,
                AncestorBoundary::CodexRootMarkers { defaults } => {
                    InstructionAncestorStop::Marker {
                        paths: encoded_names(defaults, builder),
                    }
                }
            };
            (
                InstructionCandidate::Ancestor {
                    paths: encoded_names(names, builder),
                    stop,
                },
                map_selection(*selection),
            )
        }
        DiscoveryRule::DescendantCandidates { names, selection } => (
            InstructionCandidate::Descendant {
                directory: NativeInstructionPath::from_path(Path::new("."))
                    .expect("fixed relative directory is valid"),
                paths: encoded_names(names, builder),
            },
            map_selection(*selection),
        ),
        DiscoveryRule::MarkdownRules { directory, scope } => {
            let directory = encode_path(directory, builder)?;
            let scope = match scope {
                RuleDirectoryScope::Root => InstructionRuleDirectoryScope::Root,
                RuleDirectoryScope::Ancestors => InstructionRuleDirectoryScope::Ancestors,
                RuleDirectoryScope::Descendants => InstructionRuleDirectoryScope::Descendants,
            };
            (
                InstructionCandidate::MarkdownRules { directory, scope },
                InstructionSelection::All,
            )
        }
        DiscoveryRule::ExplicitFile { .. } => {
            let ProviderRoot::ExplicitPath(path) = &input.root else {
                builder.gap(InstructionGapKind::DeclarationMissing, context);
                return None;
            };
            let path = encode_path(path, builder)?;
            (
                InstructionCandidate::ExplicitFile { path },
                InstructionSelection::All,
            )
        }
        DiscoveryRule::ExplicitResource(kind) => {
            builder.gap(resource_gap(*kind), context);
            return None;
        }
        DiscoveryRule::GeneratedConfig { .. } => {
            if facts.daemon_configuration_digest.is_none() {
                builder.gap_from(
                    InstructionGapKind::ConfigurationUnknown,
                    context,
                    InstructionGapOrigin::Declared,
                );
            }
            return None;
        }
        DiscoveryRule::ImportHome => return None,
    })
}

fn declaration_mode(mode: DeclarationMode) -> InstructionDeclarationMode {
    match mode {
        DeclarationMode::ClaudeManaged(IsolationMode::Default) => {
            InstructionDeclarationMode::ClaudeDefault
        }
        DeclarationMode::ClaudeManaged(IsolationMode::Inherit) => {
            InstructionDeclarationMode::ClaudeInherit
        }
        DeclarationMode::ClaudeManaged(IsolationMode::Bare) => {
            InstructionDeclarationMode::ClaudeBare
        }
        DeclarationMode::ClaudeAttached => InstructionDeclarationMode::ClaudeAttached,
        DeclarationMode::CodexLegacy => InstructionDeclarationMode::CodexLegacy,
        DeclarationMode::CodexStreaming => InstructionDeclarationMode::CodexStreaming,
        DeclarationMode::PiManaged => InstructionDeclarationMode::Pi,
        DeclarationMode::Unsupported { .. } => InstructionDeclarationMode::Unknown,
    }
}

fn mode_provider(mode: DeclarationMode) -> Option<Provider> {
    match mode {
        DeclarationMode::ClaudeManaged(_) | DeclarationMode::ClaudeAttached => {
            Some(Provider::Claude)
        }
        DeclarationMode::CodexLegacy | DeclarationMode::CodexStreaming => Some(Provider::Codex),
        DeclarationMode::PiManaged => Some(Provider::Pi),
        DeclarationMode::Unsupported { .. } => None,
    }
}

fn map_selection(selection: CandidateSelection) -> InstructionSelection {
    match selection {
        CandidateSelection::All => InstructionSelection::All,
        CandidateSelection::FirstNonempty => InstructionSelection::FirstNonempty,
        CandidateSelection::FirstRegular => InstructionSelection::FirstRegular,
        CandidateSelection::FirstReadable => InstructionSelection::FirstReadable,
    }
}

fn map_activation(activation: Activation) -> InstructionActivation {
    match activation {
        Activation::Active => InstructionActivation::Active,
        Activation::Inactive(_) => InstructionActivation::Inactive,
        Activation::Conditional(condition) => InstructionActivation::Conditional {
            condition: match condition {
                DiscoveryCondition::ClaudeEffectiveConfiguration
                | DiscoveryCondition::ClaudeAgentsFallback
                | DiscoveryCondition::ClaudeManagedPolicy => InstructionCondition::Configuration,
                DiscoveryCondition::ClaudeAdditionalDirectories => {
                    InstructionCondition::ExplicitOption
                }
                DiscoveryCondition::CodexEffectiveTrust => InstructionCondition::Trust,
            },
        },
    }
}

fn discharged(
    gap: DeclarationGap,
    mode: DeclarationMode,
    guard: Option<CodexInstructionAdmissionFacts>,
) -> bool {
    let Some(guard) = guard.filter(|_| mode == DeclarationMode::CodexStreaming) else {
        return false;
    };
    match gap {
        DeclarationGap::CodexFallbackNames | DeclarationGap::CodexRootMarkers => {
            guard.project_untrusted
        }
        DeclarationGap::CodexModelInstructionsFile
        | DeclarationGap::CodexEffectiveConfiguration => guard.instruction_configuration_excluded,
        _ => false,
    }
}

fn resource_gap(kind: ResourceKind) -> InstructionGapKind {
    match kind {
        ResourceKind::ClaudePlugin => InstructionGapKind::PluginUnknown,
        ResourceKind::ClaudeAgentDefinitions | ResourceKind::UnrecognizedArgument => {
            InstructionGapKind::ConfigurationUnknown
        }
    }
}

fn gap_context(gap: DeclarationGap) -> InstructionPathContext {
    match gap {
        DeclarationGap::ProfileUnavailable(_)
        | DeclarationGap::CodexModelInstructionsFile
        | DeclarationGap::PiEffectiveConfiguration => shared(InstructionSharedInput::Profile),
        DeclarationGap::ClaudeImportApproval | DeclarationGap::ClaudeImportHomeUnavailable => {
            shared(InstructionSharedInput::Import)
        }
        DeclarationGap::ExplicitResourceUnknown(_)
        | DeclarationGap::ExplicitFileValueMissing(_)
        | DeclarationGap::ExplicitDirectoryValueMissing => {
            shared(InstructionSharedInput::ExplicitFile)
        }
        _ => InstructionPathContext::Unresolved,
    }
}

/// Only the inputs an engine reads from outside the project, which Dudes can never observe, are
/// standing limits. Every other declaration gap, and every gap added to the declaration later,
/// is observed until it is listed here.
fn gap_origin(gap: DeclarationGap) -> InstructionGapOrigin {
    match gap {
        DeclarationGap::CodexFallbackNames
        | DeclarationGap::CodexRootMarkers
        | DeclarationGap::CodexModelInstructionsFile
        | DeclarationGap::CodexEffectiveConfiguration
        | DeclarationGap::CodexEffectiveTrust
        | DeclarationGap::PiEffectiveConfiguration
        | DeclarationGap::ClaudeEffectiveConfiguration
        | DeclarationGap::ClaudeAgentsPlugin
        | DeclarationGap::ClaudeManagedInlinePolicy => InstructionGapOrigin::Declared,
        _ => InstructionGapOrigin::Observed,
    }
}

fn map_gap(gap: DeclarationGap) -> InstructionGapKind {
    match gap {
        DeclarationGap::DeclarationMissing => InstructionGapKind::DeclarationMissing,
        DeclarationGap::DeclarationLimit => InstructionGapKind::InputLimit,
        DeclarationGap::MetadataLimit => InstructionGapKind::MetadataLimit,
        DeclarationGap::PathUnresolved | DeclarationGap::ArgumentEncoding => {
            InstructionGapKind::PathUnresolved
        }
        DeclarationGap::ProfileUnavailable(_) => InstructionGapKind::ProfileUnknown,
        DeclarationGap::ClaudeAgentsPlugin => InstructionGapKind::PluginUnknown,
        DeclarationGap::ClaudeManagedInlinePolicy => InstructionGapKind::PolicyUnknown,
        DeclarationGap::ClaudeImportApproval | DeclarationGap::ClaudeImportHomeUnavailable => {
            InstructionGapKind::ImportUnresolved
        }
        DeclarationGap::CodexFallbackNames => InstructionGapKind::FallbackUnknown,
        DeclarationGap::CodexRootMarkers => InstructionGapKind::RootMarkerUnknown,
        DeclarationGap::ExplicitResourceUnknown(kind) => resource_gap(kind),
        DeclarationGap::ClaudeSettingSourcesRefused
        | DeclarationGap::ClaudeEffectiveConfiguration
        | DeclarationGap::ClaudeAdditionalDirectoriesSwitch
        | DeclarationGap::ClaudeInlinePrompt
        | DeclarationGap::ClaudeInlineAgentDefinitions
        | DeclarationGap::ExplicitFileValueMissing(_)
        | DeclarationGap::ExplicitDirectoryValueMissing
        | DeclarationGap::CodexModelInstructionsFile
        | DeclarationGap::CodexEffectiveConfiguration
        | DeclarationGap::CodexEffectiveTrust
        | DeclarationGap::PiEffectiveConfiguration => InstructionGapKind::ConfigurationUnknown,
    }
}

fn validation_gap(error: InstructionValidationError) -> InstructionGapKind {
    match error {
        InstructionValidationError::PathTooLarge | InstructionValidationError::MetadataTooLarge => {
            InstructionGapKind::MetadataLimit
        }
        InstructionValidationError::CollectionLimit => InstructionGapKind::InputLimit,
        _ => InstructionGapKind::PathUnresolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dudes_providers::instructions::{ExplicitFilePurpose, GeneratedConfigKind, InactiveReason};

    const FOLDER: &str = if cfg!(windows) {
        r"C:\project\worktree"
    } else {
        "/project/worktree"
    };

    fn agent() -> AgentId {
        AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()
    }

    fn evidence() -> InstructionEvidenceDigests {
        InstructionEvidenceDigests {
            claude_documentation: Hex32::new([1; 32]),
            codex_source: Hex32::new([2; 32]),
            pi_source: Hex32::new([3; 32]),
            daemon_composition: Hex32::new([4; 32]),
        }
    }

    fn facts(evidence: &InstructionEvidenceDigests) -> InstructionPlanFacts<'_> {
        InstructionPlanFacts {
            canonical_execution_folder: Path::new(FOLDER),
            project_context: ProjectInstructionContext::UnsharedAfterCompleteCensus,
            evidence,
            daemon_configuration_digest: Some(Hex32::new([5; 32])),
            codex_guard: None,
        }
    }

    fn input(root: ProviderRoot, rule: DiscoveryRule) -> InstructionInput {
        InstructionInput {
            root,
            rule,
            activation: Activation::Active,
            evidence: EvidenceRevision::ClaudeCurrentDocumentation,
            imports: ImportRule::ClaudeMemory { max_hops: 4 },
        }
    }

    fn declaration(mode: DeclarationMode, inputs: Vec<InstructionInput>) -> InstructionDeclaration {
        InstructionDeclaration {
            revision: 1,
            mode,
            inputs,
            gaps: Vec::new(),
        }
    }

    #[test]
    fn preserves_markdown_scopes_and_separates_ancestor_inputs_from_project_ownership() {
        let inputs = [
            RuleDirectoryScope::Root,
            RuleDirectoryScope::Ancestors,
            RuleDirectoryScope::Descendants,
        ]
        .map(|scope| {
            input(
                ProviderRoot::ExecutionDirectory("original-alias".into()),
                DiscoveryRule::MarkdownRules {
                    directory: ".claude/rules".into(),
                    scope,
                },
            )
        })
        .to_vec();
        let native = declaration(
            DeclarationMode::ClaudeManaged(IsolationMode::Default),
            inputs,
        );
        let evidence = evidence();
        let mut facts = facts(&evidence);
        facts.project_context =
            ProjectInstructionContext::OwnedWorktree(WorktreeId::from_bytes([7; 16]));
        let mapped = instruction_participant(agent(), Provider::Claude, &native, &facts);
        let scopes: Vec<_> = mapped
            .discovery
            .rules
            .iter()
            .map(|rule| match rule.candidate {
                InstructionCandidate::MarkdownRules { scope, .. } => scope,
                _ => panic!("expected markdown family"),
            })
            .collect();
        assert_eq!(
            scopes,
            vec![
                InstructionRuleDirectoryScope::Root,
                InstructionRuleDirectoryScope::Ancestors,
                InstructionRuleDirectoryScope::Descendants
            ]
        );
        assert_eq!(
            mapped.discovery.roots[0].context,
            InstructionPathContext::OwnedWorktree {
                lease: WorktreeId::from_bytes([7; 16])
            }
        );
        let ancestor = &mapped.discovery.roots[usize::from(mapped.discovery.rules[1].root)];
        assert_eq!(ancestor.kind, InstructionRootKind::Ancestor);
        assert_eq!(ancestor.context, shared(InstructionSharedInput::Ancestor));
        assert_eq!(
            mapped.discovery.rules[0].imports,
            InstructionImportPolicy::ClaudeFourHops
        );
        assert_eq!(
            mapped.discovery.rules[0].evidence.kind,
            InstructionDiscoveryEvidenceKind::VendorDocumentation
        );
        assert_eq!(mapped.discovery.validate(), Ok(()));
    }

    #[test]
    fn nonenumerable_facets_affect_digests_without_becoming_instruction_reads() {
        let home = input(
            ProviderRoot::ImportHomeDirectory("home".into()),
            DiscoveryRule::ImportHome,
        );
        let generated = input(
            ProviderRoot::DaemonDirectory("daemon".into()),
            DiscoveryRule::GeneratedConfig {
                file: "settings.json".into(),
                kind: GeneratedConfigKind::ClaudeManaged,
            },
        );
        let resource = input(
            ProviderRoot::ExplicitPath("plugin.zip".into()),
            DiscoveryRule::ExplicitResource(ResourceKind::ClaudePlugin),
        );
        let file = input(
            ProviderRoot::ExplicitPath("prompt.md".into()),
            DiscoveryRule::ExplicitFile {
                purpose: ExplicitFilePurpose::SystemPrompt,
            },
        );
        let mut native = declaration(
            DeclarationMode::ClaudeAttached,
            vec![home, generated, resource, file],
        );
        let evidence = evidence();
        let facts = facts(&evidence);
        let before = instruction_participant(agent(), Provider::Claude, &native, &facts);
        assert_eq!(before.discovery.rules.len(), 1);
        assert!(
            before
                .discovery
                .roots
                .iter()
                .any(|root| root.kind == InstructionRootKind::ImportHome)
        );
        assert!(
            before
                .discovery
                .roots
                .iter()
                .any(|root| root.kind == InstructionRootKind::DaemonConfiguration)
        );
        assert!(
            before.discovery.roots.iter().any(|root| root.path
                == NativeInstructionPath::from_path(Path::new("plugin.zip")).unwrap())
        );
        assert!(
            before
                .discovery
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::PluginUnknown)
        );
        native.inputs[3].rule = DiscoveryRule::ExplicitFile {
            purpose: ExplicitFilePurpose::AppendSystemPrompt,
        };
        let purpose_changed = instruction_participant(agent(), Provider::Claude, &native, &facts);
        assert_ne!(
            purpose_changed.declaration_digest,
            before.declaration_digest
        );
        assert_eq!(purpose_changed.discovery.rules, before.discovery.rules);
        native.inputs[1].rule = DiscoveryRule::GeneratedConfig {
            file: "settings.json".into(),
            kind: GeneratedConfigKind::ClaudeAttached,
        };
        native.inputs[1].activation = Activation::Inactive(InactiveReason::ClaudeSourceExcluded);
        let generated_changed = instruction_participant(agent(), Provider::Claude, &native, &facts);
        assert_ne!(
            generated_changed.discovery.configuration_digest,
            purpose_changed.discovery.configuration_digest
        );
        assert_eq!(
            generated_changed.discovery.rules,
            purpose_changed.discovery.rules
        );
        assert_eq!(generated_changed.discovery.validate(), Ok(()));
    }

    fn streaming_declaration() -> InstructionDeclaration {
        let mut native = declaration(
            DeclarationMode::CodexStreaming,
            vec![input(
                ProviderRoot::ExecutionDirectory("alias".into()),
                DiscoveryRule::AncestorCandidates {
                    names: vec!["AGENTS.override.md".into(), "AGENTS.md".into()],
                    selection: CandidateSelection::FirstRegular,
                    boundary: AncestorBoundary::CodexRootMarkers {
                        defaults: vec![".git".into()],
                    },
                },
            )],
        );
        native.inputs[0].activation = Activation::Inactive(InactiveReason::CodexUntrustedProject);
        native.inputs[0].imports = ImportRule::None;
        native.inputs[0].evidence = EvidenceRevision::Codex01601;
        native.gaps = vec![
            DeclarationGap::CodexFallbackNames,
            DeclarationGap::CodexRootMarkers,
            DeclarationGap::CodexModelInstructionsFile,
            DeclarationGap::CodexEffectiveConfiguration,
        ];
        native
    }

    #[test]
    fn guarded_home_alone_does_not_clear_configuration_or_model_selector_gaps() {
        let native = streaming_declaration();
        let evidence = evidence();
        let mut facts = facts(&evidence);
        let unknown = instruction_participant(agent(), Provider::Codex, &native, &facts);
        assert_eq!(
            unknown.discovery.rules[0].activation,
            InstructionActivation::Conditional {
                condition: InstructionCondition::Trust
            }
        );
        assert!(
            unknown
                .discovery
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::FallbackUnknown)
        );
        facts.codex_guard = Some(CodexInstructionAdmissionFacts {
            receipt_digest: Hex32::new([8; 32]),
            project_untrusted: true,
            instruction_configuration_excluded: false,
        });
        let guarded = instruction_participant(agent(), Provider::Codex, &native, &facts);
        assert_eq!(
            guarded.discovery.rules[0].activation,
            InstructionActivation::Inactive
        );
        assert!(!guarded.discovery.gaps.iter().any(|gap| matches!(
            gap.reason,
            InstructionGapKind::FallbackUnknown | InstructionGapKind::RootMarkerUnknown
        )));
        assert_eq!(
            guarded
                .discovery
                .gaps
                .iter()
                .filter(|gap| gap.reason == InstructionGapKind::ConfigurationUnknown)
                .count(),
            2
        );
        assert_ne!(guarded.declaration_digest, unknown.declaration_digest);
        assert_eq!(guarded.discovery.validate(), Ok(()));
    }

    #[test]
    fn fully_proved_effective_configuration_exclusion_clears_its_specific_gaps() {
        let native = streaming_declaration();
        let evidence = evidence();
        let mut facts = facts(&evidence);
        facts.codex_guard = Some(CodexInstructionAdmissionFacts {
            receipt_digest: Hex32::new([9; 32]),
            project_untrusted: true,
            instruction_configuration_excluded: true,
        });
        let mapped = instruction_participant(agent(), Provider::Codex, &native, &facts);
        assert_eq!(
            mapped.discovery.rules[0].activation,
            InstructionActivation::Inactive
        );
        assert_eq!(
            mapped.discovery.rules[0].selection,
            InstructionSelection::FirstRegular
        );
        assert_eq!(mapped.discovery.gaps, Vec::<InstructionCoverageGap>::new());
        assert_eq!(mapped.discovery.validate(), Ok(()));
    }

    #[test]
    fn pi_keeps_loader_cwd_and_resource_uncertainty_even_in_an_unshared_folder() {
        let native = declaration(DeclarationMode::PiManaged, Vec::new());
        let evidence = evidence();
        let facts = facts(&evidence);
        let mapped = instruction_participant(agent(), Provider::Pi, &native, &facts);
        assert_eq!(
            mapped.discovery.roots[0].context,
            InstructionPathContext::UnsharedProject
        );
        assert!(
            mapped.discovery.gaps.iter().any(|gap| gap.reason
                == InstructionGapKind::PathUnresolved
                && gap.path == Some(NativeInstructionPath::from_path(Path::new(FOLDER)).unwrap()))
        );
        assert!(
            mapped
                .discovery
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::ConfigurationUnknown
                    && gap.context == shared(InstructionSharedInput::ExplicitFile))
        );
        assert_eq!(mapped.discovery.validate(), Ok(()));
    }

    fn engine_participant(
        adapter: &dyn dudes_providers::adapter::Adapter,
        folder: &Path,
    ) -> InstructionParticipant {
        use dudes_providers::adapter::AgentCtx;
        use dudes_types::{Mode, ModelSelection};
        let env = [dudes_providers::launch::EnvVar::new(
            "HOME",
            "/home/operator",
        )];
        let ctx = AgentCtx {
            agent: agent(),
            cwd: folder,
            agent_dir: Path::new("/state/agents/owner"),
            daemon_env: &env,
            sandbox: None,
            trusted: true,
            max_turns: None,
            max_budget: None,
            mcp_servers: &[],
        };
        let declaration =
            adapter.instruction_manifest(&ctx, Mode::Managed, &ModelSelection::provider_default());
        let evidence = evidence();
        let mut facts = facts(&evidence);
        facts.canonical_execution_folder = folder;
        facts.project_context = ProjectInstructionContext::Shared;
        facts.daemon_configuration_digest = None;
        instruction_participant(agent(), adapter.provider(), &declaration, &facts)
    }

    #[test]
    fn every_gap_a_stock_engine_declaration_carries_is_a_standing_limit() {
        use dudes_providers::{
            claude::ClaudeAdapter, claude::config::ClaudeConfig, codex::CodexAdapter,
            codex::config::CodexConfig, pi::PiAdapter,
        };
        let streaming = CodexConfig {
            streaming: true,
            ..CodexConfig::default()
        };
        let adapters: [Box<dyn dudes_providers::adapter::Adapter>; 4] = [
            Box::new(ClaudeAdapter::new(ClaudeConfig::default())),
            Box::new(CodexAdapter::new(CodexConfig::default())),
            Box::new(CodexAdapter::new(streaming).with_managed_home("/state/codex-home".into())),
            Box::new(PiAdapter::default()),
        ];
        for adapter in adapters {
            let participant = engine_participant(adapter.as_ref(), Path::new(FOLDER));
            let comparison = dudes_proto::instruction::InstructionComparison {
                changes: Vec::new(),
                gaps: participant.discovery.gaps.clone(),
                usage: dudes_proto::InstructionSampleUsage::default(),
            };
            let holding: Vec<_> = comparison.gaps.iter().filter(|gap| gap.holds()).collect();
            assert!(
                holding.is_empty(),
                "{:?} declaration leaves gaps that hold: {holding:?}",
                adapter.provider()
            );
            assert!(!comparison.has_shared_gaps());
            let note = dudes_proto::TicketCoverageNote::from_comparison(
                &comparison,
                Some(adapter.provider()),
            )
            .expect("a stock declaration always carries standing limits");
            assert!(note.saw_no_change);
        }
    }

    #[test]
    fn editing_a_project_settings_file_in_a_shared_project_is_a_known_shared_change() {
        use dudes_providers::{
            claude::ClaudeAdapter, claude::config::ClaudeConfig, codex::CodexAdapter,
            codex::config::CodexConfig, pi::PiAdapter,
        };
        let cases: [(Box<dyn dudes_providers::adapter::Adapter>, &str); 4] = [
            (
                Box::new(ClaudeAdapter::new(ClaudeConfig::default())),
                ".claude/settings.json",
            ),
            (
                Box::new(ClaudeAdapter::new(ClaudeConfig::default())),
                ".claude/settings.local.json",
            ),
            (
                Box::new(CodexAdapter::new(CodexConfig::default())),
                ".codex/config.toml",
            ),
            (Box::new(PiAdapter::default()), ".pi/settings.json"),
        ];
        for (adapter, file) in cases {
            let directory = tempfile::tempdir().unwrap();
            let folder = std::fs::canonicalize(directory.path()).unwrap();
            std::fs::create_dir(folder.join(".git")).unwrap();
            let target = folder.join(file);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(&target, "one").unwrap();
            let participant = engine_participant(adapter.as_ref(), &folder);
            let snapshot = dudes_proto::instruction::InstructionSnapshot {
                sampled_at: dudes_types::Timestamp::UNIX_EPOCH,
                source: dudes_proto::instruction::InstructionSnapshotSource::Fresh,
                pid_namespace: None,
                boot: None,
                execution_folder: Some(NativeInstructionPath::from_path(&folder).unwrap()),
                participants: vec![participant],
                inputs: Vec::new(),
                inventoried: Vec::new(),
                gaps: Vec::new(),
                usage: dudes_proto::InstructionSampleUsage::default(),
            };
            let before = crate::instruction_sample::observe(snapshot.clone());
            std::fs::write(&target, "two").unwrap();
            let after = crate::instruction_sample::observe(snapshot);
            let comparison = crate::instruction_sample::compare(&before, &after);
            assert!(
                comparison.has_shared_changes()
                    && comparison.changes.iter().any(|change| change
                        .path
                        .to_path()
                        .ok()
                        .as_deref()
                        == Some(target.as_path())),
                "{file}: {:?}",
                comparison.changes
            );
        }
    }

    #[test]
    fn a_declared_gap_never_absorbs_an_observed_gap_that_differs_only_in_origin() {
        let mut builder = PlanBuilder::new(agent(), None);
        let context = shared(InstructionSharedInput::Profile);
        builder.gap_from(
            InstructionGapKind::ConfigurationUnknown,
            context,
            InstructionGapOrigin::Declared,
        );
        builder.gap(InstructionGapKind::ConfigurationUnknown, context);
        let held: Vec<_> = builder
            .finish()
            .gaps
            .into_iter()
            .filter(|gap| gap.reason == InstructionGapKind::ConfigurationUnknown)
            .map(|gap| gap.origin)
            .collect();
        assert_eq!(
            held,
            [
                InstructionGapOrigin::Declared,
                InstructionGapOrigin::Observed
            ]
        );
    }

    #[test]
    fn only_inputs_the_engine_reads_outside_the_project_are_declared() {
        use dudes_providers::instructions::{ExplicitFilePurpose, ProfileKind, ResourceKind};
        let observed = [
            DeclarationGap::DeclarationMissing,
            DeclarationGap::DeclarationLimit,
            DeclarationGap::MetadataLimit,
            DeclarationGap::PathUnresolved,
            DeclarationGap::ProfileUnavailable(ProfileKind::CodexLegacy),
            DeclarationGap::ClaudeSettingSourcesRefused,
            DeclarationGap::ClaudeImportApproval,
            DeclarationGap::ClaudeImportHomeUnavailable,
            DeclarationGap::ClaudeAdditionalDirectoriesSwitch,
            DeclarationGap::ClaudeInlinePrompt,
            DeclarationGap::ClaudeInlineAgentDefinitions,
            DeclarationGap::ExplicitResourceUnknown(ResourceKind::ClaudePlugin),
            DeclarationGap::ExplicitFileValueMissing(ExplicitFilePurpose::SystemPrompt),
            DeclarationGap::ExplicitDirectoryValueMissing,
            DeclarationGap::ArgumentEncoding,
        ];
        for gap in observed {
            assert_eq!(gap_origin(gap), InstructionGapOrigin::Observed, "{gap:?}");
        }
    }

    #[test]
    fn metadata_overflow_keeps_a_known_rule_and_a_bounded_honest_gap() {
        let mut inputs = vec![input(
            ProviderRoot::ExplicitPath("known.md".into()),
            DiscoveryRule::ExplicitFile {
                purpose: ExplicitFilePurpose::SystemPrompt,
            },
        )];
        inputs.extend((0..100).map(|index| {
            input(
                ProviderRoot::ExplicitPath(format!("{index}-{}", "x".repeat(2000)).into()),
                DiscoveryRule::ExplicitFile {
                    purpose: ExplicitFilePurpose::AppendSystemPrompt,
                },
            )
        }));
        let native = declaration(DeclarationMode::ClaudeAttached, inputs);
        let evidence = evidence();
        let facts = facts(&evidence);
        let mapped = instruction_participant(agent(), Provider::Claude, &native, &facts);
        assert_eq!(
            mapped.discovery.rules[0].candidate,
            InstructionCandidate::ExplicitFile {
                path: NativeInstructionPath::from_path(Path::new("known.md")).unwrap()
            }
        );
        assert!(
            mapped
                .discovery
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::MetadataLimit)
        );
        assert!(mapped.discovery.rules.len() < 101);
        assert_eq!(mapped.discovery.validate(), Ok(()));
    }
}
