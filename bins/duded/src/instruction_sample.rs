use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use dudes_proto::instruction::{
    InstructionActivation, InstructionAncestorStop, InstructionCandidate, InstructionComparison,
    InstructionCondition, InstructionCoverageGap, InstructionDiscoveryPlan,
    InstructionDiscoveryRule, InstructionFingerprint, InstructionGapKind, InstructionImportPolicy,
    InstructionInput, InstructionInputSource, InstructionPathChange, InstructionPathContext,
    InstructionRoot, InstructionRootKind, InstructionRuleDirectoryScope, InstructionRuleRef,
    InstructionSampleUsage, InstructionSelection, InstructionSharedInput, InstructionSnapshot,
    InstructionSnapshotSource, InstructionSourceKind, InstructionValidationError,
    MAX_CLAUDE_IMPORT_DEPTH, MAX_INSTRUCTION_ANCESTORS, MAX_INSTRUCTION_DESCENDANT_DEPTH,
    MAX_INSTRUCTION_DESCENDANTS, MAX_INSTRUCTION_IMPORT_EDGES, MAX_INSTRUCTION_INPUTS,
    MAX_INSTRUCTION_PARTICIPANTS, MAX_INSTRUCTION_SOURCE_BYTES, NativeInstructionPath,
};
use dudes_proto::record::Hex32;
use dudes_types::AgentId;
use sha2::{Digest, Sha256};

pub(crate) const SAMPLE_DEADLINE: Duration = Duration::from_secs(2);

pub(crate) fn digest(bytes: &[u8]) -> Hex32 {
    Hex32::new(Sha256::digest(bytes).into())
}

#[cfg(test)]
pub(crate) fn observe(snapshot: InstructionSnapshot) -> InstructionSnapshot {
    observe_reporting(snapshot, None, Instant::now() + SAMPLE_DEADLINE)
}

pub(crate) fn observe_reporting(
    mut snapshot: InstructionSnapshot,
    progress: Option<Arc<Mutex<InstructionSnapshot>>>,
    deadline: Instant,
) -> InstructionSnapshot {
    snapshot.inputs.clear();
    snapshot.inventoried.clear();
    snapshot.usage = InstructionSampleUsage::default();
    snapshot = bounded_seed(snapshot);
    let mut sample = Sample {
        snapshot,
        deadline,
        failures: 0,
        progress,
    };
    let participants = sample.snapshot.participants.clone();
    for participant in participants {
        for gap in &participant.discovery.gaps {
            let _ = sample.snapshot.try_push_gap(gap.clone());
        }
        for (index, rule) in participant.discovery.rules.iter().enumerate() {
            let Ok(index) = u16::try_from(index) else {
                break;
            };
            let Some(root) = participant.discovery.roots.get(usize::from(rule.root)) else {
                sample.gap(
                    InstructionGapKind::DeclarationMissing,
                    participant.agent,
                    None,
                    InstructionPathContext::Unresolved,
                );
                break;
            };
            if rule.activation == InstructionActivation::Inactive {
                continue;
            }
            let before = sample.failures;
            sample.rule(participant.agent, index, root, rule, &participant.discovery);
            if sample.failures == before {
                sample.inventory(participant.agent, index);
            }
            if !sample.time(participant.agent, root.context) {
                break;
            }
        }
    }
    assert!(
        sample.snapshot.validate().is_ok(),
        "bounded instruction sample must satisfy its carrier contract"
    );
    sample.snapshot
}

pub(crate) fn bounded_seed(mut snapshot: InstructionSnapshot) -> InstructionSnapshot {
    let participants = std::mem::take(&mut snapshot.participants);
    let gaps = std::mem::take(&mut snapshot.gaps);
    if snapshot
        .execution_folder
        .as_ref()
        .is_some_and(|path| path.validate().is_err())
    {
        snapshot.execution_folder = None;
    }
    if snapshot.execution_folder.is_none() {
        let _ = snapshot.try_push_gap(InstructionCoverageGap {
            origin: dudes_proto::InstructionGapOrigin::Observed,
            reason: InstructionGapKind::PathUnresolved,
            context: InstructionPathContext::Unresolved,
            agent: None,
            path: None,
        });
    }
    if snapshot.source == InstructionSnapshotSource::MissingBaseline {
        let _ = snapshot.try_push_gap(InstructionCoverageGap {
            origin: dudes_proto::InstructionGapOrigin::Observed,
            reason: InstructionGapKind::MissingBaseline,
            context: InstructionPathContext::Unresolved,
            agent: None,
            path: None,
        });
    }
    for gap in gaps {
        if !snapshot.try_push_gap(gap) {
            break;
        }
    }
    for participant in participants {
        if snapshot.participants.len() == MAX_INSTRUCTION_PARTICIPANTS {
            let _ = snapshot.try_push_gap(InstructionCoverageGap {
                origin: dudes_proto::InstructionGapOrigin::Observed,
                reason: InstructionGapKind::ParticipantLimit,
                context: InstructionPathContext::Unresolved,
                agent: None,
                path: None,
            });
            break;
        }
        let agent = participant.agent;
        snapshot.participants.push(participant);
        if let Err(error) = snapshot.validate() {
            snapshot.participants.pop();
            let reason = if error == InstructionValidationError::MetadataTooLarge {
                InstructionGapKind::MetadataLimit
            } else {
                InstructionGapKind::DeclarationMissing
            };
            let _ = snapshot.try_push_gap(InstructionCoverageGap {
                origin: dudes_proto::InstructionGapOrigin::Observed,
                reason,
                context: InstructionPathContext::Unresolved,
                agent: Some(agent),
                path: None,
            });
        }
    }
    assert!(
        snapshot.validate().is_ok(),
        "bounded instruction seed must satisfy its carrier contract"
    );
    snapshot
}

#[derive(Clone, Copy)]
struct RuleInputs<'a> {
    root: &'a InstructionRoot,
    rule: &'a InstructionDiscoveryRule,
    plan: &'a InstructionDiscoveryPlan,
}

struct Sample {
    snapshot: InstructionSnapshot,
    deadline: Instant,
    failures: u64,
    progress: Option<Arc<Mutex<InstructionSnapshot>>>,
}

impl Sample {
    fn checkpoint(&self) {
        if let Some(progress) = &self.progress
            && self.snapshot.validate().is_ok()
        {
            *progress.lock().unwrap_or_else(PoisonError::into_inner) = self.snapshot.clone();
        }
    }
    fn inventory(&mut self, agent: AgentId, rule: u16) {
        if self.snapshot.inventoried.len() == MAX_INSTRUCTION_INPUTS {
            self.gap(
                InstructionGapKind::InputLimit,
                agent,
                None,
                InstructionPathContext::Unresolved,
            );
            return;
        }
        self.snapshot
            .inventoried
            .push(InstructionRuleRef { agent, rule });
        if self.snapshot.validate().is_err() {
            self.snapshot.inventoried.pop();
            self.gap(
                InstructionGapKind::MetadataLimit,
                agent,
                None,
                InstructionPathContext::Unresolved,
            );
        }
    }
    fn gap(
        &mut self,
        reason: InstructionGapKind,
        agent: AgentId,
        path: Option<&Path>,
        context: InstructionPathContext,
    ) {
        self.failures += 1;
        let context = path.map_or(context, |path| {
            self.path_context(agent, path, context, InstructionSharedInput::Ancestor)
        });
        let path = path.and_then(|path| NativeInstructionPath::from_path(path).ok());
        let gap = InstructionCoverageGap {
            origin: dudes_proto::InstructionGapOrigin::Observed,
            reason,
            context,
            agent: Some(agent),
            path,
        };
        if !self.snapshot.gaps.contains(&gap) {
            let _ = self.snapshot.try_push_gap(gap);
        }
        self.checkpoint();
    }

    fn time(&mut self, agent: AgentId, context: InstructionPathContext) -> bool {
        self.checkpoint();
        if Instant::now() < self.deadline {
            return true;
        }
        self.gap(InstructionGapKind::Deadline, agent, None, context);
        false
    }

    fn rule(
        &mut self,
        agent: AgentId,
        index: u16,
        root: &InstructionRoot,
        rule: &InstructionDiscoveryRule,
        plan: &InstructionDiscoveryPlan,
    ) {
        let inputs = RuleInputs { root, rule, plan };
        let Ok(base) = root.path.to_path() else {
            self.gap(
                InstructionGapKind::PathUnresolved,
                agent,
                None,
                root.context,
            );
            return;
        };
        match &rule.candidate {
            InstructionCandidate::Fixed { paths } => self.candidates(
                agent,
                index,
                inputs,
                &base,
                paths,
                &InstructionSourceKind::Fixed,
            ),
            InstructionCandidate::Ancestor { paths, stop } => {
                for directory in self.ancestors(agent, root.context, &base, stop, plan) {
                    self.candidates(
                        agent,
                        index,
                        inputs,
                        &directory,
                        paths,
                        &InstructionSourceKind::Ancestor,
                    );
                }
            }
            InstructionCandidate::Descendant { directory, paths } => {
                let Ok(relative) = directory.to_path() else {
                    self.gap(
                        InstructionGapKind::PathUnresolved,
                        agent,
                        None,
                        root.context,
                    );
                    return;
                };
                for directory in self.directories(agent, root.context, &base.join(relative)) {
                    self.candidates(
                        agent,
                        index,
                        inputs,
                        &directory,
                        paths,
                        &InstructionSourceKind::Descendant,
                    );
                }
            }
            InstructionCandidate::MarkdownRules { directory, scope } => {
                let Ok(relative) = directory.to_path() else {
                    self.gap(
                        InstructionGapKind::PathUnresolved,
                        agent,
                        None,
                        root.context,
                    );
                    return;
                };
                let roots = match scope {
                    InstructionRuleDirectoryScope::Root => vec![base],
                    InstructionRuleDirectoryScope::Ancestors => self.ancestors(
                        agent,
                        root.context,
                        &base,
                        &InstructionAncestorStop::FileSystemRoot,
                        plan,
                    ),
                    InstructionRuleDirectoryScope::Descendants => {
                        self.directories(agent, root.context, &base)
                    }
                };
                for directory in roots {
                    self.markdown(agent, index, inputs, &directory.join(&relative));
                }
            }
            InstructionCandidate::ExplicitFile { path } => {
                let Ok(path) = path.to_path() else {
                    self.gap(
                        InstructionGapKind::PathUnresolved,
                        agent,
                        None,
                        root.context,
                    );
                    return;
                };
                let path = if path.is_absolute() {
                    path
                } else {
                    base.join(path)
                };
                let source = Self::source(agent, index, root, InstructionSourceKind::ExplicitFile);
                self.file(&path, root.context, source, inputs, false, &[]);
            }
        }
    }

    fn source(
        agent: AgentId,
        rule: u16,
        root: &InstructionRoot,
        kind: InstructionSourceKind,
    ) -> InstructionInputSource {
        InstructionInputSource {
            agent,
            rule,
            root: root.path.clone(),
            kind,
        }
    }

    fn ancestors(
        &mut self,
        agent: AgentId,
        context: InstructionPathContext,
        base: &Path,
        stop: &InstructionAncestorStop,
        plan: &InstructionDiscoveryPlan,
    ) -> Vec<PathBuf> {
        let mut directories = Vec::new();
        let mut current = Some(base);
        while let Some(directory) = current {
            if !self.time(agent, context) {
                break;
            }
            if self.snapshot.usage.ancestor_steps == MAX_INSTRUCTION_ANCESTORS {
                self.gap(
                    InstructionGapKind::AncestorLimit,
                    agent,
                    Some(directory),
                    context,
                );
                break;
            }
            self.snapshot.usage.ancestor_steps += 1;
            directories.push(directory.to_path_buf());
            let stopped = match stop {
                InstructionAncestorStop::FileSystemRoot => false,
                InstructionAncestorStop::Root { root } => plan
                    .roots
                    .get(usize::from(*root))
                    .and_then(|root| root.path.to_path().ok())
                    .is_some_and(|root| root == directory),
                InstructionAncestorStop::Marker { paths } => {
                    if !local_filesystem(directory) {
                        self.gap(
                            InstructionGapKind::PathUnresolved,
                            agent,
                            Some(directory),
                            context,
                        );
                        break;
                    }
                    let mut found = false;
                    for marker in paths {
                        let Ok(marker) = marker.to_path() else {
                            self.gap(
                                InstructionGapKind::PathUnresolved,
                                agent,
                                Some(directory),
                                context,
                            );
                            continue;
                        };
                        match fs::symlink_metadata(directory.join(marker)) {
                            Ok(_) => found = true,
                            Err(error) if error.kind() == ErrorKind::NotFound => {}
                            Err(_) => self.gap(
                                InstructionGapKind::ReadFailed,
                                agent,
                                Some(directory),
                                context,
                            ),
                        }
                    }
                    found
                }
            };
            if stopped {
                break;
            }
            current = directory.parent();
        }
        directories
    }

    fn entries(
        &mut self,
        agent: AgentId,
        context: InstructionPathContext,
        directory: &Path,
    ) -> Option<Vec<fs::DirEntry>> {
        if !self.time(agent, context) {
            return None;
        }
        if !local_filesystem(directory) {
            self.gap(
                InstructionGapKind::PathUnresolved,
                agent,
                Some(directory),
                context,
            );
            return None;
        }
        let read = match fs::read_dir(directory) {
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::NotFound => return Some(Vec::new()),
            Err(_) => {
                self.gap(
                    InstructionGapKind::ReadFailed,
                    agent,
                    Some(directory),
                    context,
                );
                return None;
            }
        };
        let mut entries = Vec::new();
        for entry in read {
            if !self.time(agent, context) {
                return None;
            }
            if self.snapshot.usage.descendant_entries == MAX_INSTRUCTION_DESCENDANTS {
                self.gap(
                    InstructionGapKind::DescendantLimit,
                    agent,
                    Some(directory),
                    context,
                );
                return None;
            }
            self.snapshot.usage.descendant_entries += 1;
            if let Ok(entry) = entry {
                entries.push(entry);
            } else {
                self.gap(
                    InstructionGapKind::ReadFailed,
                    agent,
                    Some(directory),
                    context,
                );
                return None;
            }
        }
        entries.sort_by_key(fs::DirEntry::file_name);
        Some(entries)
    }

    fn directories(
        &mut self,
        agent: AgentId,
        context: InstructionPathContext,
        root: &Path,
    ) -> Vec<PathBuf> {
        let mut pending = VecDeque::from([(root.to_path_buf(), 0_u8)]);
        let mut directories = Vec::new();
        while let Some((directory, depth)) = pending.pop_front() {
            if !self.time(agent, context) {
                break;
            }
            directories.push(directory.clone());
            self.snapshot.usage.descendant_depth = self.snapshot.usage.descendant_depth.max(depth);
            let Some(entries) = self.entries(agent, context, &directory) else {
                break;
            };
            for entry in entries {
                match entry.file_type() {
                    Ok(kind) if kind.is_symlink() => self.gap(
                        InstructionGapKind::PathUnresolved,
                        agent,
                        Some(&entry.path()),
                        context,
                    ),
                    Ok(kind) if kind.is_dir() => {
                        if depth == MAX_INSTRUCTION_DESCENDANT_DEPTH {
                            self.gap(
                                InstructionGapKind::DescendantLimit,
                                agent,
                                Some(&entry.path()),
                                context,
                            );
                        } else {
                            pending.push_back((entry.path(), depth + 1));
                        }
                    }
                    Ok(_) => {}
                    Err(_) => self.gap(
                        InstructionGapKind::ReadFailed,
                        agent,
                        Some(&entry.path()),
                        context,
                    ),
                }
            }
        }
        directories
    }

    fn markdown(&mut self, agent: AgentId, index: u16, inputs: RuleInputs<'_>, directory: &Path) {
        for directory in self.directories(agent, inputs.root.context, directory) {
            let Some(entries) = self.entries(agent, inputs.root.context, &directory) else {
                continue;
            };
            for entry in entries {
                if entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "md")
                {
                    let source = Self::source(
                        agent,
                        index,
                        inputs.root,
                        InstructionSourceKind::MarkdownRules,
                    );
                    self.file(
                        &entry.path(),
                        inputs.root.context,
                        source,
                        inputs,
                        false,
                        &[],
                    );
                }
            }
        }
    }

    fn candidates(
        &mut self,
        agent: AgentId,
        index: u16,
        inputs: RuleInputs<'_>,
        directory: &Path,
        names: &[NativeInstructionPath],
        kind: &InstructionSourceKind,
    ) {
        let mut selected = false;
        for name in names {
            let Ok(name) = name.to_path() else {
                self.gap(
                    InstructionGapKind::PathUnresolved,
                    agent,
                    Some(directory),
                    inputs.root.context,
                );
                continue;
            };
            if name.is_absolute()
                || name
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
            {
                self.gap(
                    InstructionGapKind::PathUnresolved,
                    agent,
                    Some(directory),
                    inputs.root.context,
                );
                continue;
            }
            let source = Self::source(agent, index, inputs.root, kind.clone());
            let fingerprint = self.file(
                &directory.join(name),
                inputs.root.context,
                source,
                inputs,
                selected,
                &[],
            );
            if inputs.rule.selection != InstructionSelection::All {
                selected |= match fingerprint {
                    Some(InstructionFingerprint::File { bytes, .. }) => {
                        inputs.rule.selection != InstructionSelection::FirstNonempty || bytes != 0
                    }
                    _ => false,
                };
            }
        }
    }

    fn file(
        &mut self,
        path: &Path,
        context: InstructionPathContext,
        source: InstructionInputSource,
        inputs: RuleInputs<'_>,
        shadowed: bool,
        ancestors: &[NativeInstructionPath],
    ) -> Option<InstructionFingerprint> {
        let context = self.context(path, context, &source);
        let activation = if matches!(source.kind, InstructionSourceKind::Import { .. })
            && inputs.rule.activation != InstructionActivation::Inactive
        {
            InstructionActivation::Conditional {
                condition: InstructionCondition::Configuration,
            }
        } else {
            inputs.rule.activation
        };
        if !self.time(source.agent, context) {
            return None;
        }
        let Ok(native) = NativeInstructionPath::from_path(path) else {
            self.gap(
                InstructionGapKind::MetadataLimit,
                source.agent,
                None,
                context,
            );
            return None;
        };
        if !local_filesystem(path) {
            self.gap(
                InstructionGapKind::PathUnresolved,
                source.agent,
                Some(path),
                context,
            );
            return None;
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let _ = self.snapshot.try_push_input(InstructionInput {
                    path: native,
                    resolved_path: None,
                    context,
                    source,
                    activation,
                    shadowed,
                    fingerprint: InstructionFingerprint::Absent,
                });
                return Some(InstructionFingerprint::Absent);
            }
            Err(_) => {
                self.gap(
                    InstructionGapKind::ReadFailed,
                    source.agent,
                    Some(path),
                    context,
                );
                return None;
            }
        };
        let (resolved, bytes, remaining) =
            self.read_regular_file(path, context, source.agent, &metadata)?;
        let fingerprint = InstructionFingerprint::File {
            digest: digest(&bytes),
            bytes: u64::try_from(bytes.len()).unwrap_or(remaining),
        };
        if !self.snapshot.try_push_input(InstructionInput {
            path: native,
            resolved_path: Some(resolved.clone()),
            context: self.context(path, context, &source),
            source: source.clone(),
            activation,
            shadowed,
            fingerprint,
        }) {
            self.failures += 1;
            return None;
        }
        self.checkpoint();
        if inputs.rule.imports == InstructionImportPolicy::ClaudeFourHops && !shadowed {
            let mut chain = ancestors.to_vec();
            if chain.contains(&resolved) {
                self.gap(
                    InstructionGapKind::ImportCycle,
                    source.agent,
                    Some(path),
                    context,
                );
            } else {
                chain.push(resolved);
                self.imports(path, &bytes, context, &source, inputs, &chain);
            }
        }
        Some(fingerprint)
    }

    fn read_regular_file(
        &mut self,
        path: &Path,
        context: InstructionPathContext,
        agent: AgentId,
        metadata: &fs::Metadata,
    ) -> Option<(NativeInstructionPath, Vec<u8>, u64)> {
        if metadata.file_type().is_symlink() || !metadata.is_file() || has_parent_symlink(path) {
            self.gap(
                InstructionGapKind::PathUnresolved,
                agent,
                Some(path),
                context,
            );
            return None;
        }
        if !local_filesystem(path) {
            self.gap(
                InstructionGapKind::PathUnresolved,
                agent,
                Some(path),
                context,
            );
            return None;
        }
        let Some(resolved) = fs::canonicalize(path)
            .ok()
            .and_then(|path| NativeInstructionPath::from_path(&path).ok())
        else {
            self.gap(
                InstructionGapKind::PathUnresolved,
                agent,
                Some(path),
                context,
            );
            return None;
        };
        let remaining =
            MAX_INSTRUCTION_SOURCE_BYTES.saturating_sub(self.snapshot.usage.source_bytes);
        if metadata.len() > remaining {
            self.gap(
                InstructionGapKind::SourceBytesLimit,
                agent,
                Some(path),
                context,
            );
            return None;
        }
        let bytes = self.read_file(path, context, agent, metadata, &resolved, remaining)?;
        Some((resolved, bytes, remaining))
    }

    fn read_file(
        &mut self,
        path: &Path,
        context: InstructionPathContext,
        agent: AgentId,
        metadata: &fs::Metadata,
        resolved: &NativeInstructionPath,
        remaining: u64,
    ) -> Option<Vec<u8>> {
        let Ok(mut file) = open_regular(path) else {
            self.gap(InstructionGapKind::ReadFailed, agent, Some(path), context);
            return None;
        };
        let before = match file.metadata() {
            Ok(before) if before.is_file() && same_file(metadata, &before) => before,
            _ => {
                self.gap(
                    InstructionGapKind::ChangedDuringRead,
                    agent,
                    Some(path),
                    context,
                );
                return None;
            }
        };
        if !local_open_file(&file) {
            self.gap(
                InstructionGapKind::PathUnresolved,
                agent,
                Some(path),
                context,
            );
            return None;
        }
        let mut bytes = Vec::new();
        let read = file.by_ref().take(remaining).read_to_end(&mut bytes);
        self.snapshot.usage.source_bytes += u64::try_from(bytes.len()).unwrap_or(remaining);
        if read.is_err() {
            self.gap(InstructionGapKind::ReadFailed, agent, Some(path), context);
            return None;
        }
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != before.len() {
            self.gap(
                InstructionGapKind::ChangedDuringRead,
                agent,
                Some(path),
                context,
            );
            return None;
        }
        let stable = file
            .metadata()
            .is_ok_and(|after| same_file(&before, &after))
            && fs::symlink_metadata(path).is_ok_and(|after| same_file(&before, &after))
            && fs::canonicalize(path)
                .ok()
                .and_then(|path| NativeInstructionPath::from_path(&path).ok())
                .as_ref()
                == Some(resolved);
        if !stable {
            self.gap(
                InstructionGapKind::ChangedDuringRead,
                agent,
                Some(path),
                context,
            );
            return None;
        }
        Some(bytes)
    }

    fn context(
        &self,
        path: &Path,
        context: InstructionPathContext,
        source: &InstructionInputSource,
    ) -> InstructionPathContext {
        let origin = match &source.kind {
            InstructionSourceKind::Import { .. } => InstructionSharedInput::Import,
            InstructionSourceKind::Ancestor => InstructionSharedInput::Ancestor,
            _ => InstructionSharedInput::ExplicitFile,
        };
        self.path_context(source.agent, path, context, origin)
    }

    fn path_context(
        &self,
        agent: AgentId,
        path: &Path,
        mut context: InstructionPathContext,
        origin: InstructionSharedInput,
    ) -> InstructionPathContext {
        if context
            == (InstructionPathContext::SharedInput {
                source: InstructionSharedInput::Ancestor,
            })
            && let Some(execution) = self
                .snapshot
                .participants
                .iter()
                .find(|participant| participant.agent == agent)
                .and_then(|participant| {
                    participant.discovery.roots.iter().find(|root| {
                        root.kind == InstructionRootKind::Execution
                            && Some(&root.path) == self.snapshot.execution_folder.as_ref()
                    })
                })
        {
            context = execution.context;
        }
        if matches!(
            context,
            InstructionPathContext::SharedInput { .. } | InstructionPathContext::Unresolved
        ) {
            return context;
        }
        if !supported_path_spelling(path) {
            return InstructionPathContext::Unresolved;
        }
        let folder = self
            .snapshot
            .execution_folder
            .as_ref()
            .and_then(|folder| folder.to_path().ok());
        match (folder, resolved_candidate(path)) {
            (Some(folder), Some(resolved)) if resolved.starts_with(&folder) => context,
            (Some(_), Some(_)) => InstructionPathContext::SharedInput { source: origin },
            _ => InstructionPathContext::Unresolved,
        }
    }

    fn imports(
        &mut self,
        file: &Path,
        bytes: &[u8],
        context: InstructionPathContext,
        source: &InstructionInputSource,
        inputs: RuleInputs<'_>,
        chain: &[NativeInstructionPath],
    ) {
        let Ok(text) = std::str::from_utf8(bytes) else {
            self.gap(
                InstructionGapKind::ImportUnresolved,
                source.agent,
                Some(file),
                context,
            );
            return;
        };
        let (imports, overflow) = imports(text);
        if overflow {
            self.gap(
                InstructionGapKind::ImportLimit,
                source.agent,
                Some(file),
                context,
            );
        }
        for import in imports {
            if chain.len() > usize::from(MAX_CLAUDE_IMPORT_DEPTH)
                || self.snapshot.usage.import_edges == MAX_INSTRUCTION_IMPORT_EDGES
            {
                self.gap(
                    InstructionGapKind::ImportLimit,
                    source.agent,
                    Some(file),
                    context,
                );
                break;
            }
            self.snapshot.usage.import_edges += 1;
            let path = if let Some(relative) = import.strip_prefix("~/") {
                let Some(home) = inputs
                    .plan
                    .roots
                    .iter()
                    .find(|root| root.kind == InstructionRootKind::ImportHome)
                    .and_then(|root| root.path.to_path().ok())
                else {
                    self.gap(
                        InstructionGapKind::ImportUnresolved,
                        source.agent,
                        Some(file),
                        context,
                    );
                    continue;
                };
                home.join(relative)
            } else {
                let path = PathBuf::from(import);
                if path.is_absolute() {
                    path
                } else {
                    file.parent().unwrap_or(Path::new("/")).join(path)
                }
            };
            let Ok(from) = NativeInstructionPath::from_path(file) else {
                break;
            };
            let depth = u8::try_from(chain.len()).unwrap_or(MAX_CLAUDE_IMPORT_DEPTH);
            let mut imported = source.clone();
            imported.kind = InstructionSourceKind::Import { from, depth };
            self.file(&path, context, imported, inputs, false, chain);
        }
    }
}

fn imports(text: &str) -> (Vec<String>, bool) {
    let mut imports = Vec::new();
    let mut fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        let mut chars = line.chars().peekable();
        let mut code = false;
        let mut quote = None;
        while let Some(ch) = chars.next() {
            if ch == '`' {
                code = !code;
                continue;
            }
            if code {
                continue;
            }
            if let Some(active) = quote {
                if ch == active {
                    quote = None;
                }
                continue;
            }
            if ch == '"' || ch == '\'' {
                quote = Some(ch);
                continue;
            }
            if ch != '@' {
                continue;
            }
            let mut path = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() || ch == '`' || ch == '"' || ch == '\'' {
                    break;
                }
                chars.next();
                if ch == '\\' {
                    if let Some(ch) = chars.next() {
                        path.push(ch);
                    }
                } else {
                    path.push(ch);
                }
            }
            if !path.is_empty() {
                imports.push(path);
            }
            if imports.len() > usize::from(MAX_INSTRUCTION_IMPORT_EDGES) {
                imports.pop();
                return (imports, true);
            }
        }
    }
    (imports, false)
}

fn has_parent_symlink(path: &Path) -> bool {
    path.ancestors().skip(1).any(|parent| {
        fs::symlink_metadata(parent).map_or(true, |metadata| metadata.file_type().is_symlink())
    })
}

fn same_file(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        before.len() == after.len()
            && before.modified().ok() == after.modified().ok()
            && before.created().ok() == after.created().ok()
    }
}

fn open_regular(path: &Path) -> std::io::Result<File> {
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(
            (rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::NOFOLLOW)
                .bits()
                .cast_signed(),
        );
    }
    options.open(path)
}

fn supported_path_spelling(path: &Path) -> bool {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return false;
    }
    let bytes = path.as_os_str().as_encoded_bytes();
    path.is_absolute()
        && !bytes.starts_with(b"//")
        && !bytes.starts_with(b"\\")
        && ![Path::new("/dev"), Path::new("/proc"), Path::new("/sys")]
            .iter()
            .any(|root| path.starts_with(root))
}

fn resolved_candidate(path: &Path) -> Option<PathBuf> {
    if !supported_path_spelling(path) {
        return None;
    }
    for parent in path
        .ancestors()
        .take(usize::from(MAX_INSTRUCTION_ANCESTORS))
    {
        match fs::canonicalize(parent) {
            Ok(mut resolved) => {
                for component in path.strip_prefix(parent).ok()?.components() {
                    match component {
                        Component::Normal(value) => resolved.push(value),
                        Component::ParentDir => {
                            if !resolved.pop() {
                                return None;
                            }
                        }
                        Component::CurDir => {}
                        _ => return None,
                    }
                }
                return Some(resolved);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    None
}

fn local_filesystem(path: &Path) -> bool {
    if !supported_path_spelling(path) {
        return false;
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let Some(path) = path
            .ancestors()
            .take(usize::from(MAX_INSTRUCTION_ANCESTORS))
            .find(|candidate| fs::symlink_metadata(candidate).is_ok())
        else {
            return false;
        };
        rustix::fs::statfs(path).is_ok_and(|stat| local_statfs(&stat))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = path;
        false
    }
}

fn local_open_file(file: &File) -> bool {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        rustix::fs::fstatfs(file).is_ok_and(|stat| local_statfs(&stat))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = file;
        false
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn local_statfs(stat: &rustix::fs::StatFs) -> bool {
    #[cfg(target_os = "macos")]
    {
        let name: Vec<u8> = stat
            .f_fstypename
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| byte.to_ne_bytes()[0])
            .collect();
        matches!(name.as_slice(), b"apfs" | b"hfs")
    }
    #[cfg(target_os = "linux")]
    {
        matches!(
            stat.f_type,
            0xef53 | 0x0102_1994 | 0x9123_683e | 0x5846_5342
        )
    }
}

pub(crate) fn compare(
    before: &InstructionSnapshot,
    after: &InstructionSnapshot,
) -> InstructionComparison {
    let mut comparison = InstructionComparison {
        changes: Vec::new(),
        gaps: Vec::new(),
        usage: after.usage,
    };
    for gap in before.gaps.iter().chain(&after.gaps) {
        if !comparison.gaps.contains(gap) {
            let _ = comparison.try_push_gap(gap.clone());
        }
    }
    compare_participants(&mut comparison, before, after);
    let same_source = |left: &InstructionInput, right: &InstructionInput| {
        left.path == right.path && left.source == right.source
    };
    let mut pairs = Vec::new();
    for old in &before.inputs {
        pairs.push((
            Some(old),
            after
                .inputs
                .iter()
                .find(|current| same_source(old, current)),
        ));
    }
    for current in &after.inputs {
        if !before.inputs.iter().any(|old| same_source(old, current)) {
            pairs.push((None, Some(current)));
        }
    }
    for snapshot in [before, after] {
        compare_path_fingerprints(&mut comparison, snapshot);
    }
    for (old, current) in pairs {
        compare_input_pair(&mut comparison, before, after, old, current);
    }
    comparison
}

fn compare_participants(
    comparison: &mut InstructionComparison,
    before: &InstructionSnapshot,
    after: &InstructionSnapshot,
) {
    for participant in &after.participants {
        match before
            .participants
            .iter()
            .find(|old| old.agent == participant.agent)
        {
            None => {
                let _ = comparison.try_push_gap(InstructionCoverageGap {
                    origin: dudes_proto::InstructionGapOrigin::Observed,
                    reason: InstructionGapKind::ParticipantChanged,
                    context: InstructionPathContext::SharedProject,
                    agent: Some(participant.agent),
                    path: None,
                });
            }
            Some(old) if old.declaration_digest != participant.declaration_digest => {
                let _ = comparison.try_push_gap(InstructionCoverageGap {
                    origin: dudes_proto::InstructionGapOrigin::Observed,
                    reason: InstructionGapKind::DeclarationChanged,
                    context: InstructionPathContext::SharedProject,
                    agent: Some(participant.agent),
                    path: None,
                });
            }
            Some(_) => {}
        }
    }
    for participant in &before.participants {
        if !after
            .participants
            .iter()
            .any(|current| current.agent == participant.agent)
        {
            let _ = comparison.try_push_gap(InstructionCoverageGap {
                origin: dudes_proto::InstructionGapOrigin::Observed,
                reason: InstructionGapKind::ParticipantChanged,
                context: InstructionPathContext::SharedProject,
                agent: Some(participant.agent),
                path: None,
            });
        }
    }
}

fn compare_path_fingerprints(
    comparison: &mut InstructionComparison,
    snapshot: &InstructionSnapshot,
) {
    let mut paths: BTreeMap<&NativeInstructionPath, &InstructionInput> = BTreeMap::new();
    for input in &snapshot.inputs {
        if let Some(prior) = paths.insert(&input.path, input)
            && (prior.fingerprint != input.fingerprint
                || prior.resolved_path != input.resolved_path)
        {
            let _ = comparison.try_push_gap(InstructionCoverageGap {
                origin: dudes_proto::InstructionGapOrigin::Observed,
                reason: InstructionGapKind::ChangedDuringRead,
                context: merged_context(prior.context, input.context),
                agent: Some(input.source.agent),
                path: Some(input.path.clone()),
            });
        }
    }
}

fn compare_input_pair(
    comparison: &mut InstructionComparison,
    before: &InstructionSnapshot,
    after: &InstructionSnapshot,
    old: Option<&InstructionInput>,
    current: Option<&InstructionInput>,
) {
    let representative = current
        .or(old)
        .expect("a provenance member has an endpoint");
    let path = &representative.path;
    let family = InstructionRuleRef {
        agent: representative.source.agent,
        rule: representative.source.rule,
    };
    let known_absent = |snapshot: &InstructionSnapshot| {
        snapshot.inventoried.contains(&family)
            && !matches!(
                representative.source.kind,
                InstructionSourceKind::Import { .. }
            )
            && snapshot
                .participants
                .iter()
                .find(|participant| participant.agent == family.agent)
                .zip(
                    if std::ptr::eq(snapshot, before) {
                        after
                    } else {
                        before
                    }
                    .participants
                    .iter()
                    .find(|participant| participant.agent == family.agent),
                )
                .is_some_and(|(left, right)| {
                    left.discovery.rules.get(usize::from(family.rule))
                        == right.discovery.rules.get(usize::from(family.rule))
                        && left.discovery.roots == right.discovery.roots
                })
            && !snapshot
                .gaps
                .iter()
                .any(|gap| gap.path.as_ref() == Some(path))
    };
    let prior = old
        .map(|input| input.fingerprint)
        .or_else(|| known_absent(before).then_some(InstructionFingerprint::Absent));
    let next = current
        .map(|input| input.fingerprint)
        .or_else(|| known_absent(after).then_some(InstructionFingerprint::Absent));
    let context = old
        .zip(current)
        .map_or(representative.context, |(old, current)| {
            merged_context(old.context, current.context)
        });
    match (prior, next) {
        (Some(prior), Some(next)) if prior != next => {
            let _ = comparison.try_push_change(InstructionPathChange {
                path: path.clone(),
                context,
                source: representative.source.clone(),
                before: prior,
                after: next,
            });
        }
        (None, _) | (_, None) => {
            let _ = comparison.try_push_gap(InstructionCoverageGap {
                origin: dudes_proto::InstructionGapOrigin::Observed,
                reason: InstructionGapKind::UninventoriedMember,
                context,
                agent: Some(representative.source.agent),
                path: Some(path.clone()),
            });
        }
        _ => {}
    }
    if let (Some(old), Some(current)) = (old, current)
        && old.resolved_path != current.resolved_path
        && !matches!(
            (old.fingerprint, current.fingerprint),
            (InstructionFingerprint::Absent, _) | (_, InstructionFingerprint::Absent)
        )
    {
        let _ = comparison.try_push_gap(InstructionCoverageGap {
            origin: dudes_proto::InstructionGapOrigin::Observed,
            reason: InstructionGapKind::ChangedDuringRead,
            context,
            agent: Some(representative.source.agent),
            path: Some(path.clone()),
        });
    }
}

fn merged_context(
    before: InstructionPathContext,
    after: InstructionPathContext,
) -> InstructionPathContext {
    if before.needs_shared_review() {
        before
    } else {
        after
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dudes_proto::instruction::{
        InstructionDeclarationMode, InstructionDiscoveryEvidence, InstructionDiscoveryEvidenceKind,
        InstructionIssueAssociation, InstructionObservationOwner, InstructionParticipant,
        validate_instruction_record,
    };
    use dudes_types::{Provider, Timestamp};

    fn native(path: impl AsRef<Path>) -> NativeInstructionPath {
        NativeInstructionPath::from_path(path.as_ref()).unwrap()
    }

    fn fixture(directory: &Path) -> InstructionSnapshot {
        let directory = fs::canonicalize(directory).unwrap();
        let agent = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
        InstructionSnapshot {
            sampled_at: Timestamp::UNIX_EPOCH,
            source: InstructionSnapshotSource::Fresh,
            execution_folder: Some(native(&directory)),
            participants: vec![InstructionParticipant {
                agent,
                provider: Provider::Claude,
                mode: InstructionDeclarationMode::ClaudeDefault,
                declaration_digest: digest(b"fixed fixture"),
                discovery: InstructionDiscoveryPlan {
                    roots: vec![InstructionRoot {
                        path: native(&directory),
                        kind: InstructionRootKind::Execution,
                        context: InstructionPathContext::SharedProject,
                    }],
                    rules: vec![InstructionDiscoveryRule {
                        root: 0,
                        candidate: InstructionCandidate::Fixed {
                            paths: vec![native("CLAUDE.md"), native("AGENTS.md")],
                        },
                        selection: InstructionSelection::All,
                        activation: InstructionActivation::Active,
                        imports: InstructionImportPolicy::None,
                        evidence: InstructionDiscoveryEvidence {
                            kind: InstructionDiscoveryEvidenceKind::VersionedSource,
                            digest: digest(b"fixture evidence"),
                        },
                    }],
                    configuration_digest: None,
                    gaps: Vec::new(),
                },
            }],
            inputs: Vec::new(),
            inventoried: Vec::new(),
            gaps: Vec::new(),
            usage: InstructionSampleUsage::default(),
            boot: None,
        }
    }

    #[test]
    fn ancestor_discovery_keeps_lease_local_and_solo_files_local_but_parent_files_shared() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        fs::create_dir(&project).unwrap();
        fs::create_dir(directory.path().join(".git")).unwrap();
        fs::write(project.join("CLAUDE.md"), b"project one").unwrap();
        fs::write(directory.path().join("CLAUDE.md"), b"parent one").unwrap();
        for context in [
            InstructionPathContext::OwnedWorktree {
                lease: dudes_types::WorktreeId::from_bytes([7; 16]),
            },
            InstructionPathContext::UnsharedProject,
        ] {
            let mut plan = fixture(&project);
            plan.participants[0].discovery.roots[0].context = context;
            plan.participants[0].discovery.roots.push(InstructionRoot {
                path: plan.execution_folder.clone().unwrap(),
                kind: InstructionRootKind::Ancestor,
                context: InstructionPathContext::SharedInput {
                    source: InstructionSharedInput::Ancestor,
                },
            });
            let rule = &mut plan.participants[0].discovery.rules[0];
            rule.root = 1;
            rule.candidate = InstructionCandidate::Ancestor {
                paths: vec![native("CLAUDE.md")],
                stop: InstructionAncestorStop::Marker {
                    paths: vec![native(".git")],
                },
            };
            let before = observe(plan.clone());
            fs::write(project.join("CLAUDE.md"), b"project two").unwrap();
            let comparison = compare(&before, &observe(plan.clone()));
            assert!(!comparison.changes.is_empty());
            assert!(
                comparison
                    .changes
                    .iter()
                    .all(|change| change.context == context)
            );
            assert!(comparison.gaps.is_empty(), "{:?}", comparison.gaps);
            fs::write(directory.path().join("CLAUDE.md"), b"parent two").unwrap();
            assert!(
                compare(&before, &observe(plan))
                    .changes
                    .iter()
                    .any(|change| change.context
                        == (InstructionPathContext::SharedInput {
                            source: InstructionSharedInput::Ancestor
                        }))
            );
            fs::write(project.join("CLAUDE.md"), b"project one").unwrap();
            fs::write(directory.path().join("CLAUDE.md"), b"parent one").unwrap();
        }
    }

    #[test]
    fn inventoried_import_candidates_keep_conditional_loading_without_coverage_uncertainty() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("CLAUDE.md"), b"@import.md").unwrap();
        fs::write(directory.path().join("import.md"), b"candidate").unwrap();
        let mut plan = fixture(directory.path());
        plan.participants[0].discovery.rules[0].imports = InstructionImportPolicy::ClaudeFourHops;
        let sample = observe(plan);
        assert!(sample.gaps.is_empty(), "{:?}", sample.gaps);
        assert!(sample.inputs.iter().any(|input| matches!(
            input.source.kind,
            InstructionSourceKind::Import { .. }
        ) && input.activation
            == (InstructionActivation::Conditional {
                condition: InstructionCondition::Configuration
            })));
        assert!(
            sample
                .inputs
                .iter()
                .any(|input| input.source.kind == InstructionSourceKind::Fixed
                    && input.activation == InstructionActivation::Active)
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn content_read_admission_checks_the_open_descriptor_filesystem() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("instruction");
        fs::write(&path, b"owned source").unwrap();
        let file = open_regular(&path).unwrap();
        assert_eq!(local_open_file(&file), local_filesystem(&path));
        assert!(file.metadata().unwrap().is_file());
    }

    #[test]
    fn net_creation_deletion_and_edit_restore_have_distinct_endpoint_answers() {
        let directory = tempfile::tempdir().unwrap();
        let plan = fixture(directory.path());
        let empty = observe(plan.clone());
        assert!(empty.validate().is_ok());
        assert!(empty.gaps.is_empty(), "{:?}", empty.gaps);
        fs::write(directory.path().join("CLAUDE.md"), b"one").unwrap();
        let created = observe(plan.clone());
        assert_eq!(compare(&empty, &created).changes.len(), 1);
        assert!(compare(&created, &created).is_unchanged());
        fs::write(directory.path().join("CLAUDE.md"), b"two").unwrap();
        fs::write(directory.path().join("CLAUDE.md"), b"one").unwrap();
        assert!(compare(&created, &observe(plan.clone())).is_unchanged());
        fs::remove_file(directory.path().join("CLAUDE.md")).unwrap();
        assert_eq!(compare(&created, &observe(plan)).changes.len(), 1);
    }

    #[test]
    fn a_created_descendant_member_has_absence_only_after_complete_family_inventory() {
        let directory = tempfile::tempdir().unwrap();
        let mut plan = fixture(directory.path());
        plan.participants[0].discovery.rules[0].candidate = InstructionCandidate::Descendant {
            directory: native("."),
            paths: vec![native("CLAUDE.md")],
        };
        let before = observe(plan.clone());
        fs::create_dir(directory.path().join("new")).unwrap();
        fs::write(directory.path().join("new/CLAUDE.md"), b"instruction").unwrap();
        let after = observe(plan);
        assert_eq!(compare(&before, &after).changes.len(), 1);
        let mut uninventoried = before;
        uninventoried.inventoried.clear();
        let comparison = compare(&uninventoried, &after);
        assert!(comparison.changes.is_empty());
        assert!(
            comparison
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::UninventoriedMember)
        );
    }

    #[test]
    fn known_changes_survive_a_later_nonregular_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let plan = fixture(directory.path());
        fs::write(directory.path().join("CLAUDE.md"), b"one").unwrap();
        let before = observe(plan.clone());
        fs::write(directory.path().join("CLAUDE.md"), b"two").unwrap();
        fs::create_dir(directory.path().join("AGENTS.md")).unwrap();
        let comparison = compare(&before, &observe(plan));
        assert_eq!(comparison.changes.len(), 1);
        assert!(comparison.has_shared_gaps());
        assert!(!comparison.is_unchanged());
    }

    #[test]
    fn selection_preserves_shadowed_inputs_and_imports_skip_quoted_code() {
        let directory = tempfile::tempdir().unwrap();
        let mut plan = fixture(directory.path());
        plan.participants[0].discovery.rules[0].selection = InstructionSelection::FirstRegular;
        fs::write(directory.path().join("CLAUDE.md"), b"one").unwrap();
        fs::write(directory.path().join("AGENTS.md"), b"two").unwrap();
        let before = observe(plan.clone());
        assert!(before.inputs[1].shadowed);
        fs::write(directory.path().join("AGENTS.md"), b"three").unwrap();
        assert_eq!(compare(&before, &observe(plan)).changes.len(), 1);
        assert_eq!(
            imports("@one.md `@code.md` \"@quoted.md\"\n```\n@fence.md\n```\n@with\\ space.md").0,
            ["one.md", "with space.md"]
        );
    }

    #[test]
    fn imports_preserve_digest_and_name_cycles_and_new_edges() {
        let directory = tempfile::tempdir().unwrap();
        let mut plan = fixture(directory.path());
        plan.participants[0].discovery.rules[0].imports = InstructionImportPolicy::ClaudeFourHops;
        fs::write(directory.path().join("CLAUDE.md"), b"@imported.md").unwrap();
        fs::write(directory.path().join("imported.md"), b"original").unwrap();
        let before = observe(plan.clone());
        fs::write(directory.path().join("imported.md"), b"@CLAUDE.md").unwrap();
        let comparison = compare(&before, &observe(plan.clone()));
        assert_eq!(comparison.changes.len(), 1);
        assert!(
            comparison
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::ImportCycle)
        );
        fs::write(
            directory.path().join("new.md"),
            b"already existed outside prior inventory",
        )
        .unwrap();
        fs::write(directory.path().join("CLAUDE.md"), b"@new.md").unwrap();
        assert!(
            compare(&before, &observe(plan))
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::UninventoriedMember)
        );
    }

    #[test]
    fn source_byte_and_deadline_gaps_do_not_certify_absence() {
        let directory = tempfile::tempdir().unwrap();
        let plan = fixture(directory.path());
        fs::write(
            directory.path().join("CLAUDE.md"),
            vec![b'x'; usize::try_from(MAX_INSTRUCTION_SOURCE_BYTES).unwrap() + 1],
        )
        .unwrap();
        assert!(
            observe(plan.clone())
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::SourceBytesLimit)
        );
        let mut sample = Sample {
            snapshot: plan,
            deadline: Instant::now(),
            failures: 0,
            progress: None,
        };
        let root = sample.snapshot.participants[0].discovery.roots[0].clone();
        let agent = sample.snapshot.participants[0].agent;
        assert!(!sample.time(agent, root.context));
        assert!(
            sample
                .snapshot
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::Deadline)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_explicit_unknown_instead_of_unmeasured_coverage() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("target"), b"instruction").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("target"),
            directory.path().join("CLAUDE.md"),
        )
        .unwrap();
        assert!(
            observe(fixture(directory.path()))
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::PathUnresolved)
        );
    }

    #[test]
    fn a_new_participant_cannot_supply_a_recomputed_starting_baseline() {
        let directory = tempfile::tempdir().unwrap();
        let plan = fixture(directory.path());
        let before = observe(plan.clone());
        let mut grown = plan;
        let mut peer = grown.participants[0].clone();
        peer.agent = "01ARZ3NDEKTSV4RRFFQ69G5FAW".parse().unwrap();
        grown.participants.push(peer);
        assert!(
            compare(&before, &observe(grown))
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::ParticipantChanged)
        );
    }

    #[test]
    fn external_imports_keep_shared_scope_for_changes_absence_and_failed_reads() {
        let directory = tempfile::tempdir().unwrap();
        let tree = directory.path().join("tree");
        fs::create_dir(&tree).unwrap();
        let mut plan = fixture(&tree);
        plan.participants[0].discovery.roots[0].context = InstructionPathContext::OwnedWorktree {
            lease: dudes_types::WorktreeId::from_bytes(1_u128.to_be_bytes()),
        };
        plan.participants[0].discovery.rules[0].imports = InstructionImportPolicy::ClaudeFourHops;
        fs::write(tree.join("CLAUDE.md"), b"@../shared.md").unwrap();
        let before = observe(plan.clone());
        let absent = before
            .inputs
            .iter()
            .find(|input| matches!(input.source.kind, InstructionSourceKind::Import { .. }))
            .unwrap();
        assert_eq!(
            absent.context,
            InstructionPathContext::SharedInput {
                source: InstructionSharedInput::Import
            }
        );
        assert_eq!(absent.fingerprint, InstructionFingerprint::Absent);
        fs::write(directory.path().join("shared.md"), b"one").unwrap();
        let created = observe(plan.clone());
        assert!(compare(&before, &created).has_shared_changes());
        fs::write(directory.path().join("shared.md"), b"two").unwrap();
        assert!(compare(&created, &observe(plan.clone())).has_shared_changes());
        fs::remove_file(directory.path().join("shared.md")).unwrap();
        fs::create_dir(directory.path().join("shared.md")).unwrap();
        let failed = observe(plan);
        assert!(failed.gaps.iter().any(|gap| gap.context
            == (InstructionPathContext::SharedInput {
                source: InstructionSharedInput::Import
            })));
        assert!(compare(&created, &failed).has_shared_gaps());
    }

    #[test]
    fn a_solo_project_context_never_makes_an_external_import_local() {
        let directory = tempfile::tempdir().unwrap();
        let tree = directory.path().join("tree");
        fs::create_dir(&tree).unwrap();
        let mut plan = fixture(&tree);
        plan.participants[0].discovery.roots[0].context = InstructionPathContext::UnsharedProject;
        plan.participants[0].discovery.rules[0].imports = InstructionImportPolicy::ClaudeFourHops;
        fs::write(tree.join("CLAUDE.md"), b"@../shared.md").unwrap();
        let before = observe(plan.clone());
        fs::write(directory.path().join("shared.md"), b"outside").unwrap();
        assert!(compare(&before, &observe(plan)).has_shared_changes());
    }

    #[test]
    fn duplicate_paths_keep_each_sources_scope_and_conflicting_fingerprints() {
        let directory = tempfile::tempdir().unwrap();
        let mut before = observe(fixture(directory.path()));
        let lease = dudes_types::WorktreeId::from_bytes(1_u128.to_be_bytes());
        let mut peer = before.participants[0].clone();
        peer.agent = "01ARZ3NDEKTSV4RRFFQ69G5FAW".parse().unwrap();
        peer.discovery.roots[0].context = InstructionPathContext::OwnedWorktree { lease };
        before.participants.push(peer.clone());
        let mut duplicate = before.inputs[0].clone();
        duplicate.source.agent = peer.agent;
        duplicate.context = InstructionPathContext::OwnedWorktree { lease };
        before.inputs.push(duplicate);
        let mut after = before.clone();
        for input in &mut after.inputs {
            if input.path == before.inputs[0].path {
                input.fingerprint = InstructionFingerprint::File {
                    digest: digest(b"created"),
                    bytes: 7,
                };
            }
        }
        let comparison = compare(&before, &after);
        assert!(comparison.has_shared_changes());
        assert_eq!(comparison.changes.len(), 2);
        after.inputs.last_mut().unwrap().fingerprint = InstructionFingerprint::File {
            digest: digest(b"different read"),
            bytes: 14,
        };
        let comparison = compare(&before, &after);
        assert!(comparison.has_shared_gaps());
        assert!(
            comparison
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::ChangedDuringRead)
        );
    }

    #[test]
    fn thirty_two_large_valid_plans_produce_a_valid_bounded_seed() {
        let directory = tempfile::tempdir().unwrap();
        let mut plan = fixture(directory.path());
        let mut participant = plan.participants[0].clone();
        participant.discovery.rules[0].activation = InstructionActivation::Inactive;
        participant.discovery.rules[0].candidate = InstructionCandidate::Fixed {
            paths: (0..10)
                .map(|index| native(format!("{index}{}", "x".repeat(2000))))
                .collect(),
        };
        assert!(participant.discovery.validate().is_ok());
        plan.participants = (0..MAX_INSTRUCTION_PARTICIPANTS)
            .map(|index| {
                let mut participant = participant.clone();
                participant.agent =
                    AgentId::from_ulid(ulid::Ulid::from(u128::try_from(index + 1).unwrap()));
                participant
            })
            .collect();
        let observed = observe(plan);
        assert!(observed.validate().is_ok());
        assert!(observed.participants.len() < MAX_INSTRUCTION_PARTICIPANTS);
        assert!(
            observed
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::MetadataLimit)
        );
        let record = dudes_proto::Record::InstructionObservationStarted {
            owner: InstructionObservationOwner::Managed {
                agent: AgentId::from_ulid(ulid::Ulid::from(1_u128)),
                turn: dudes_types::TurnId::from_ulid(ulid::Ulid::from(2_u128)),
                issue: InstructionIssueAssociation::Prospective,
            },
            ticket: None,
            snapshot: observed,
        };
        assert!(validate_instruction_record(&record).is_ok());
    }

    #[test]
    fn remote_device_and_unsupported_paths_are_rejected_before_probe_admission() {
        for path in [
            "//server/share/input",
            r"\\server\share\input",
            r"\\.\pipe\input",
            "/dev/zero",
            "/proc/self/environ",
            "/sys/input",
            "relative",
        ] {
            assert!(!supported_path_spelling(Path::new(path)));
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        assert!(!local_filesystem(Path::new(r"\\unreachable\share\input")));
    }

    #[test]
    fn an_inactive_family_does_not_invent_absence_when_an_existing_file_becomes_active() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("CLAUDE.md"), b"already present").unwrap();
        let mut inactive = fixture(directory.path());
        inactive.participants[0].discovery.rules[0].activation = InstructionActivation::Inactive;
        let before = observe(inactive.clone());
        assert!(before.inventoried.is_empty());
        assert!(compare(&before, &observe(inactive.clone())).is_unchanged());
        inactive.participants[0].discovery.rules[0].activation = InstructionActivation::Active;
        inactive.participants[0].declaration_digest = digest(b"active declaration");
        let comparison = compare(&before, &observe(inactive));
        assert!(comparison.changes.is_empty());
        assert!(
            comparison
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::UninventoriedMember)
        );
        assert!(
            comparison
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::DeclarationChanged)
        );
    }
}
