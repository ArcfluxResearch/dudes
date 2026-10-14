//! Durable instruction observations and the exact set an operator reviews.
//!
//! Fingerprints describe sampled endpoints of an owned interval. They establish neither an
//! exclusive writer nor the absence of edits restored between those endpoints. Sampling and
//! lease-backed path classification belong to the daemon; these validators perform no I/O.

use std::fmt::{self, Write as _};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use dudes_types::{AgentId, CardId, GoalId, Lsn, Provider, Seq, Timestamp, TurnId, WorktreeId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::record::{Actor, Entry, Hex32, Record};

/// Maximum encoded JSON bytes for one instruction record, including its timestamp.
pub const MAX_INSTRUCTION_METADATA_BYTES: usize = 65_536;
/// Maximum encoded JSON bytes for one lossless path.
pub const MAX_INSTRUCTION_PATH_BYTES: usize = 4_096;
/// Maximum fingerprinted inputs per sample.
pub const MAX_INSTRUCTION_INPUTS: usize = 512;
/// Maximum recorded folder participants per sample.
pub const MAX_INSTRUCTION_PARTICIPANTS: usize = 32;
/// Maximum ancestor steps per sample.
pub const MAX_INSTRUCTION_ANCESTORS: u16 = 64;
/// Maximum examined descendant entries per sample.
pub const MAX_INSTRUCTION_DESCENDANTS: u32 = 4_096;
/// Maximum descendant traversal depth.
pub const MAX_INSTRUCTION_DESCENDANT_DEPTH: u8 = 16;
/// Maximum import edges per sample.
pub const MAX_INSTRUCTION_IMPORT_EDGES: u16 = 512;
/// Claude's admitted import depth.
pub const MAX_CLAUDE_IMPORT_DEPTH: u8 = 4;
/// Maximum symlink hops per path resolution.
pub const MAX_INSTRUCTION_SYMLINK_HOPS: u8 = 8;
/// Maximum aggregate source bytes read per sample.
pub const MAX_INSTRUCTION_SOURCE_BYTES: u64 = 1_048_576;

const RECORD_ENVELOPE_RESERVE: usize = 1_024;
const OVERFLOW_MARKER_RESERVE: usize = 256;
const BODY_LIMIT: usize = MAX_INSTRUCTION_METADATA_BYTES - RECORD_ENVELOPE_RESERVE;
const COLLECTION_LIMIT: usize = BODY_LIMIT - OVERFLOW_MARKER_RESERVE;

/// Lossless native path bytes. Its escaped display is derived, never an identity.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(tag = "encoding", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeInstructionPath {
    /// A path with a UTF-8 spelling.
    Utf8 {
        /// Exact UTF-8 bytes, including characters that must be escaped for display.
        value: String,
    },
    /// Non-UTF-8 Unix path bytes.
    UnixBytes {
        /// Exact bytes as lowercase hexadecimal pairs.
        hex: String,
    },
    /// Non-UTF-8 Windows path units.
    WindowsWide {
        /// Exact UTF-16 units, including unpaired surrogates.
        units: Vec<u16>,
    },
}

impl NativeInstructionPath {
    /// Encode a native path without filesystem access or lossy conversion.
    ///
    /// # Errors
    /// Refuses empty paths, NUL and paths exceeding the encoded byte limit.
    pub fn from_path(path: &Path) -> Result<Self, InstructionValidationError> {
        let encoded = if let Some(value) = path.to_str() {
            Self::Utf8 {
                value: value.to_owned(),
            }
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt as _;
                let mut hex = String::new();
                for byte in path.as_os_str().as_bytes() {
                    write!(&mut hex, "{byte:02x}").expect("writing into String");
                }
                Self::UnixBytes { hex }
            }
            #[cfg(windows)]
            {
                use std::os::windows::ffi::OsStrExt as _;
                Self::WindowsWide {
                    units: path.as_os_str().encode_wide().collect(),
                }
            }
            #[cfg(not(any(unix, windows)))]
            {
                return Err(InstructionValidationError::UnsupportedPathEncoding);
            }
        };
        encoded.validate()?;
        Ok(encoded)
    }

    /// Decode only encodings native to this platform.
    ///
    /// # Errors
    /// Refuses invalid, oversized or unsupported path encodings.
    pub fn to_path(&self) -> Result<PathBuf, InstructionValidationError> {
        self.validate()?;
        match self {
            Self::Utf8 { value } => Ok(PathBuf::from(value)),
            Self::UnixBytes { hex } => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt as _;
                    let bytes = hex
                        .as_bytes()
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| {
                            let digit = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
                            (digit(pair[0]) << 4) | digit(pair[1])
                        })
                        .collect();
                    Ok(std::ffi::OsString::from_vec(bytes).into())
                }
                #[cfg(not(unix))]
                {
                    let _ = hex;
                    Err(InstructionValidationError::UnsupportedPathEncoding)
                }
            }
            Self::WindowsWide { units } => {
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStringExt as _;
                    Ok(std::ffi::OsString::from_wide(units).into())
                }
                #[cfg(not(windows))]
                {
                    let _ = units;
                    Err(InstructionValidationError::UnsupportedPathEncoding)
                }
            }
        }
    }

    /// Validate native bytes and encoded JSON size, including escaping and the encoding tag.
    ///
    /// # Errors
    /// Refuses invalid bytes or an encoded path over 4 KiB.
    pub fn validate(&self) -> Result<(), InstructionValidationError> {
        let valid = match self {
            Self::Utf8 { value } => !value.is_empty() && !value.contains('\0'),
            Self::UnixBytes { hex } => {
                !hex.is_empty()
                    && hex.len().is_multiple_of(2)
                    && hex
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    && !hex
                        .as_bytes()
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .any(|pair| pair == b"00")
            }
            Self::WindowsWide { units } => !units.is_empty() && !units.contains(&0),
        };
        if !valid {
            return Err(InstructionValidationError::InvalidPath);
        }
        if !encoded_fits(self, MAX_INSTRUCTION_PATH_BYTES) {
            return Err(InstructionValidationError::PathTooLarge);
        }
        Ok(())
    }

    /// A bounded display with controls and bidirectional formatting escaped.
    #[must_use]
    pub fn display(&self) -> String {
        self.display_bound().0
    }

    /// Whether the safe display omits native spelling because it reached its fixed byte bound.
    #[must_use]
    pub fn display_truncated(&self) -> bool {
        self.display_bound().1
    }

    fn display_bound(&self) -> (String, bool) {
        let mut display = String::new();
        let mut truncated = false;
        match self {
            Self::Utf8 { value } => {
                for c in value.chars() {
                    let part = if c.is_control() || crate::ticket::is_bidi_control(c) {
                        c.escape_default().to_string()
                    } else {
                        c.to_string()
                    };
                    if display.len() + part.len() > MAX_INSTRUCTION_PATH_BYTES {
                        truncated = true;
                        break;
                    }
                    display.push_str(&part);
                }
            }
            Self::UnixBytes { hex } => {
                display.push_str("unix:");
                truncated = hex.chars().count() > MAX_INSTRUCTION_PATH_BYTES - 5;
                for c in hex.chars().take(MAX_INSTRUCTION_PATH_BYTES - 5) {
                    if c.is_ascii_hexdigit() {
                        display.push(c);
                    }
                }
            }
            Self::WindowsWide { units } => {
                display.push_str("windows:");
                for unit in units {
                    if display.len() + 4 > MAX_INSTRUCTION_PATH_BYTES {
                        truncated = true;
                        break;
                    }
                    write!(&mut display, "{unit:04x}").expect("writing into String");
                }
            }
        }
        (display, truncated)
    }
}

/// Daemon-established path context. A path spelling alone proves no lease ownership.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionPathContext {
    /// Project file classified from an actual owned worktree lease.
    OwnedWorktree {
        /// The lease that proves this classification.
        lease: WorktreeId,
    },
    /// Project file in a shared execution folder.
    SharedProject,
    /// An input that is shared independently of the execution folder's lease.
    SharedInput {
        /// Where this shared input enters discovery.
        source: InstructionSharedInput,
    },
    /// No supported evidence establishes the classification.
    Unresolved,
    /// Project file with a complete single-user folder census at this sample.
    UnsharedProject,
}

impl InstructionPathContext {
    /// Whether this context can require special shared-instruction review.
    #[must_use]
    pub const fn needs_shared_review(self) -> bool {
        !matches!(self, Self::OwnedWorktree { .. } | Self::UnsharedProject)
    }
}

/// Origin of a shared input outside lease-local project files.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum InstructionSharedInput {
    /// Engine profile input.
    Profile,
    /// An ancestor input.
    Ancestor,
    /// An imported input.
    Import,
    /// An explicitly admitted file.
    ExplicitFile,
}

/// Actual provider composition used for instruction discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionDeclarationMode {
    /// Claude default managed composition.
    ClaudeDefault,
    /// Claude inherit managed composition.
    ClaudeInherit,
    /// Claude bare managed composition.
    ClaudeBare,
    /// Claude owned Attached composition.
    ClaudeAttached,
    /// Codex legacy exec composition.
    CodexLegacy,
    /// Codex streaming composition.
    CodexStreaming,
    /// Pi managed composition.
    Pi,
    /// A provider without a supported declaration. Must carry a declaration gap.
    Unknown,
}

/// Meaning of a declared discovery root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionRootKind {
    /// Actual execution folder.
    Execution,
    /// Established project root.
    Project,
    /// Actual selected engine profile.
    Profile,
    /// Admitted ancestor root.
    Ancestor,
    /// Explicitly admitted extra root.
    Extra,
    /// Daemon-generated discovery configuration, sampled only through admitted fields.
    DaemonConfiguration,
    /// Managed policy input root.
    ManagedPolicy,
    /// Explicitly admitted file's resolution root.
    ExplicitFile,
    /// Home used only to resolve imports. It never admits enumeration of arbitrary home files.
    ImportHome,
}

/// A frozen root and its daemon-established context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionRoot {
    /// Exact root path.
    pub path: NativeInstructionPath,
    /// Its discovery meaning.
    pub kind: InstructionRootKind,
    /// Lease-backed or shared classification.
    pub context: InstructionPathContext,
}

/// Candidate families retain absent and shadowed members as well as selected files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionCandidate {
    /// Ordered fixed candidates relative to the rule's root.
    Fixed {
        /// Exact ordered relative names.
        paths: Vec<NativeInstructionPath>,
    },
    /// Ordered names at each ancestor, under the fixed ancestor work limit.
    Ancestor {
        /// Exact ordered relative names.
        paths: Vec<NativeInstructionPath>,
        /// The boundary that stops ascent.
        stop: InstructionAncestorStop,
    },
    /// Named files below a declared directory, under the fixed entry and depth limits.
    Descendant {
        /// Relative directory to expand.
        directory: NativeInstructionPath,
        /// Ordered candidate names.
        paths: Vec<NativeInstructionPath>,
    },
    /// Markdown rule files, including conditional rules.
    MarkdownRules {
        /// Relative rule directory.
        directory: NativeInstructionPath,
        /// Direct, ancestor, or descendant placement of that directory.
        scope: InstructionRuleDirectoryScope,
    },
    /// One explicitly admitted file, resolved against this rule's root.
    ExplicitFile {
        /// Exact file path.
        path: NativeInstructionPath,
    },
}

/// Placement of a recursively inventoried Markdown rules directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionRuleDirectoryScope {
    /// Exactly the declared root.
    Root,
    /// The execution folder and its bounded ancestors.
    Ancestors,
    /// Each bounded descendant directory of the execution folder.
    Descendants,
}

/// A frozen ancestor stopping rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionAncestorStop {
    /// Stop at the filesystem root.
    FileSystemRoot,
    /// Stop at a declared root, by its zero-based plan index.
    Root {
        /// Root index.
        root: u16,
    },
    /// Stop at the first directory containing any admitted root marker.
    Marker {
        /// Exact relative marker names.
        paths: Vec<NativeInstructionPath>,
    },
}

/// How a provider selects among ordered candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionSelection {
    /// Include every candidate.
    All,
    /// Select the first nonempty candidate.
    FirstNonempty,
    /// Select the first regular file.
    FirstRegular,
    /// Select the first readable file.
    FirstReadable,
}

/// Whether a declared rule or input is admitted to the provider's context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionActivation {
    /// Admitted by the actual composition.
    Active,
    /// Declared but excluded by the actual composition.
    Inactive,
    /// Depends on a condition whose family must remain observable.
    Conditional {
        /// The condition's typed source.
        condition: InstructionCondition,
    },
}

/// Discovery condition without raw configuration or prompt text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionCondition {
    /// Effective supported provider configuration.
    Configuration,
    /// Provider trust admission.
    Trust,
    /// Files the work touches.
    FileScope,
    /// Explicit launch option.
    ExplicitOption,
    /// Condition could not be resolved.
    Unknown,
}

/// A rule frozen before launch and re-expanded at the ending sample.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionDiscoveryRule {
    /// Zero-based index into the plan's roots.
    pub root: u16,
    /// Family to re-expand.
    pub candidate: InstructionCandidate,
    /// Ordered candidate selection.
    pub selection: InstructionSelection,
    /// Effective activation at the starting boundary.
    pub activation: InstructionActivation,
    /// Import semantics specific to this rule's files.
    pub imports: InstructionImportPolicy,
    /// Primary-source boundary and retained evidence digest for this rule.
    pub evidence: InstructionDiscoveryEvidence,
}

/// Primary-source boundary supporting a frozen discovery rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionDiscoveryEvidenceKind {
    /// Actual bounded provider request observation.
    RequestObservation,
    /// Exact-version provider source.
    VersionedSource,
    /// Vendor documentation, separate from exact-version observations.
    VendorDocumentation,
    /// Actual daemon-owned provider composition, separate from vendor documentation or source.
    DaemonComposition,
}

/// Content-free provenance of a discovery rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionDiscoveryEvidence {
    /// Which primary-source boundary established the declaration.
    pub kind: InstructionDiscoveryEvidenceKind,
    /// Digest of the retained evidence.
    pub digest: Hex32,
}

/// Supported import syntax and traversal semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionImportPolicy {
    /// This declaration admits no imports.
    None,
    /// Claude file imports, with its fixed four-hop limit.
    ClaudeFourHops,
}

/// Frozen discovery rules, never a JSON string or an opaque provider document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionDiscoveryPlan {
    /// Declared roots in stable order.
    pub roots: Vec<InstructionRoot>,
    /// Rules in provider precedence order.
    pub rules: Vec<InstructionDiscoveryRule>,
    /// Digest of supported discovery-affecting configuration, when known.
    pub configuration_digest: Option<Hex32>,
    /// Independently named declaration gaps.
    pub gaps: Vec<InstructionCoverageGap>,
}

/// One recorded user of the canonical execution folder, including idle or paused users.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionParticipant {
    /// Recorded agent identity.
    pub agent: AgentId,
    /// Actual provider.
    pub provider: Provider,
    /// Actual admitted composition, separate from a requested hire selector.
    pub mode: InstructionDeclarationMode,
    /// Digest identifying the declaration version.
    pub declaration_digest: Hex32,
    /// Rules to re-expand even if the participant later leaves.
    pub discovery: InstructionDiscoveryPlan,
}

/// Why a declared family or observation could not be covered completely.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionGapKind {
    /// Effective configuration is unknown.
    ConfigurationUnknown,
    /// Plugin inputs are unknown.
    PluginUnknown,
    /// Policy inputs are unknown.
    PolicyUnknown,
    /// Root marker selection is unknown.
    RootMarkerUnknown,
    /// Fallback filename selection is unknown.
    FallbackUnknown,
    /// Actual profile inputs are unknown.
    ProfileUnknown,
    /// No supported provider declaration exists.
    DeclarationMissing,
    /// Native path classification or resolution failed.
    PathUnresolved,
    /// A file could not be read.
    ReadFailed,
    /// A file changed during its read.
    ChangedDuringRead,
    /// An import could not be resolved.
    ImportUnresolved,
    /// Import traversal found a cycle.
    ImportCycle,
    /// Starting and ending participants differ.
    ParticipantChanged,
    /// Starting and ending declarations differ.
    DeclarationChanged,
    /// A newly found member has no proven absent baseline.
    UninventoriedMember,
    /// Input count limit reached.
    InputLimit,
    /// Recorded folder participation could not be fully determined.
    ParticipantCoverageUnknown,
    /// Participant count limit reached.
    ParticipantLimit,
    /// Ancestor work limit reached.
    AncestorLimit,
    /// Descendant entry or depth limit reached.
    DescendantLimit,
    /// Import edge or depth limit reached.
    ImportLimit,
    /// Symlink hop limit reached.
    SymlinkLimit,
    /// Aggregate source byte limit reached.
    SourceBytesLimit,
    /// Sampling deadline reached.
    Deadline,
    /// Encoded metadata did not fit; retained evidence is a bounded subset.
    MetadataLimit,
    /// Crash-era starting fingerprints are missing.
    MissingBaseline,
    /// Physical launch may have preceded a lost spawn receipt.
    UnrecordedLaunchBoundary,
    /// A host cleanup could not confirm absence.
    CleanupUnconfirmed,
    /// An actual Attached turn has no observed starting boundary.
    AttachedTurnStartMissing,
    /// No independently recorded ending boundary is available after recovery.
    EndingBoundaryMissing,
}

/// Independent uncertainty, scoped separately from any known path changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionCoverageGap {
    /// Named uncertainty without arbitrary error text.
    pub reason: InstructionGapKind,
    /// The affected path context; lease-local gaps cannot hide shared-input gaps.
    pub context: InstructionPathContext,
    /// The affected participant, when known.
    pub agent: Option<AgentId>,
    /// The affected native path, when representable.
    pub path: Option<NativeInstructionPath>,
}

impl InstructionCoverageGap {
    /// Compact overflow marker reserved before collecting metadata.
    #[must_use]
    pub const fn metadata_limit() -> Self {
        Self {
            reason: InstructionGapKind::MetadataLimit,
            context: InstructionPathContext::Unresolved,
            agent: None,
            path: None,
        }
    }
}

/// Discovery provenance for a sampled input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionInputSource {
    /// Participant that declares this input.
    pub agent: AgentId,
    /// Zero-based index of the declaring rule in that participant's frozen plan.
    pub rule: u16,
    /// Exact declared root, retained independently of the path.
    pub root: NativeInstructionPath,
    /// Discovery family or import edge.
    pub kind: InstructionSourceKind,
}

/// Discovery family that produced one input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionSourceKind {
    /// Fixed candidate.
    Fixed,
    /// Ancestor candidate.
    Ancestor,
    /// Descendant candidate.
    Descendant,
    /// Markdown rule.
    MarkdownRules,
    /// Explicit file.
    ExplicitFile,
    /// Import edge, without the source's contents.
    Import {
        /// Importing file.
        from: NativeInstructionPath,
        /// One-based import depth.
        depth: u8,
    },
}

/// An endpoint fact. Read errors and unresolved paths are never represented as absence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionFingerprint {
    /// Successful candidate and parent inventory proved absence.
    Absent,
    /// Successfully sampled regular file bytes.
    File {
        /// Digest of the complete sampled bytes.
        digest: Hex32,
        /// Complete sampled byte count.
        bytes: u64,
    },
}

/// A fingerprint with its frozen discovery provenance and selection status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionInput {
    /// Exact sampled candidate path.
    pub path: NativeInstructionPath,
    /// Canonical target observed while reading an existing candidate; absent candidates have none.
    pub resolved_path: Option<NativeInstructionPath>,
    /// Daemon-established path classification.
    pub context: InstructionPathContext,
    /// Declared root and discovery family.
    pub source: InstructionInputSource,
    /// Actual admitted activation.
    pub activation: InstructionActivation,
    /// Whether selection preferred an earlier candidate. Shadowed files remain inventoried.
    pub shadowed: bool,
    /// Successfully observed endpoint.
    pub fingerprint: InstructionFingerprint,
}

/// Work counters from the sampler, checked against fixed internal limits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionSampleUsage {
    /// Ancestor steps examined.
    pub ancestor_steps: u16,
    /// Descendant entries examined.
    pub descendant_entries: u32,
    /// Deepest descendant traversal.
    pub descendant_depth: u8,
    /// Import edges examined.
    pub import_edges: u16,
    /// Most symlink hops in one resolution.
    pub symlink_hops: u8,
    /// Aggregate source bytes read, including import parsing.
    pub source_bytes: u64,
}

/// Whether sampling establishes a fresh boundary or supplies recovery-time evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionSnapshotSource {
    /// Fresh sample before this physical attempt.
    Fresh,
    /// Actual sample performed during recovery.
    Recovery,
    /// Current evidence cannot reconstruct a crash-era baseline.
    MissingBaseline,
}

/// A complete bounded starting sample or independently named uncertainty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionSnapshot {
    /// Actual sample time, never substituted for a process boundary time.
    pub sampled_at: Timestamp,
    /// Provenance of the sample.
    pub source: InstructionSnapshotSource,
    /// Comparable operating-system boot identity actually sampled by the host, when supported.
    pub boot: Option<InstructionBootIdentity>,
    /// Actual canonical execution folder, when it was resolved and fits the lossless encoding.
    /// Absence requires a named unresolved-path, overflow or missing-baseline gap.
    pub execution_folder: Option<NativeInstructionPath>,
    /// Frozen recorded folder users and their discovery plans.
    pub participants: Vec<InstructionParticipant>,
    /// Sampled absent, selected and shadowed candidates.
    pub inputs: Vec<InstructionInput>,
    /// Families completely inventoried at the starting sample, independent of other gaps.
    /// A failed existing input remains named by a gap path and cannot become a proven creation.
    pub inventoried: Vec<InstructionRuleRef>,
    /// Independent sample coverage gaps.
    pub gaps: Vec<InstructionCoverageGap>,
    /// Actual bounded sampling work.
    pub usage: InstructionSampleUsage,
}

/// A discovery family in one participant's frozen plan.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub struct InstructionRuleRef {
    /// Participant that owns the rule.
    pub agent: AgentId,
    /// Zero-based rule index.
    pub rule: u16,
}

/// Whether a managed baseline preceded the issue or belongs to a retry of an immutable issue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionIssueAssociation {
    /// Baseline before a new issue. A crash here may settle as `NoIssuedTurn`.
    Prospective,
    /// Baseline for a physical retry of the existing immutable issue.
    Issued {
        /// Exact existing issue position.
        issue_lsn: Lsn,
    },
}

/// Owner of a physical attempt or whole owned Attached process interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionObservationOwner {
    /// Physical attempt under an immutable managed turn.
    Managed {
        /// Owning agent.
        agent: AgentId,
        /// Immutable issued or prospective turn.
        turn: TurnId,
        /// Starting baseline's relation to the issue.
        issue: InstructionIssueAssociation,
    },
    /// Whole owned Attached process, never an invented vendor turn.
    AttachedProcess {
        /// Owning agent.
        agent: AgentId,
        /// Real attachment record position.
        attachment_lsn: Lsn,
    },
    /// Actual Attached hook-ended turn whose consumption start is missing.
    /// This owner can only settle an explicit missing-boundary Unknown.
    AttachedTurn {
        /// Recorded agent identity.
        agent: AgentId,
        /// Actual hook-observed turn, never a synthetic process-interval identity.
        turn: TurnId,
        /// Unset: current observed hook ends carry no exact attachment receipt.
        attachment_lsn: Option<Lsn>,
    },
}

/// Ticket interval association frozen at the starting boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionTicketAssociation {
    /// Associated card.
    pub card: CardId,
    /// Associated goal.
    pub goal: GoalId,
    /// Assignment position whose interval began this work.
    pub assignment_lsn: Lsn,
    /// Ticket sequence captured at the starting boundary.
    pub seq_at_start: Seq,
}

/// Stable identity of an attempt, or the actual issued or ended turn whose start is missing.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionBaselineRef {
    /// Starting record's own LSN identifies this physical attempt.
    Started {
        /// Exact starting record position.
        lsn: Lsn,
    },
    /// Real observed turn without a comparable starting boundary.
    MissingTurnBoundary {
        /// Actual observed turn identity.
        turn: TurnId,
        /// Actual observed end position.
        end_lsn: Lsn,
    },
    /// An actual managed issue with neither a recorded start nor an observed end.
    MissingIssueBoundary {
        /// Immutable recorded turn identity.
        turn: TurnId,
        /// The actual issue position, reused as this uncertainty's identity on restart.
        issue_lsn: Lsn,
    },
}

/// An independently recorded end, separate from the time of the later filesystem sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionEndBoundary {
    /// First managed turn end.
    ManagedTurn {
        /// Actual managed turn.
        turn: TurnId,
        /// Observed end record position.
        end_lsn: Lsn,
    },
    /// Confirmed close or exit of a whole owned Attached process.
    AttachedProcess {
        /// Real attachment position.
        attachment_lsn: Lsn,
        /// Confirmed process end position.
        end_lsn: Lsn,
    },
    /// Actual Attached turn whose starting boundary is missing.
    AttachedTurn {
        /// Actual hook-observed turn.
        turn: TurnId,
        /// Actual hook-observed end position.
        end_lsn: Lsn,
    },
}

/// Known net change, retained independently of every coverage gap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionPathChange {
    /// Exact changed path.
    pub path: NativeInstructionPath,
    /// Classification backed by daemon evidence.
    pub context: InstructionPathContext,
    /// Discovery provenance.
    pub source: InstructionInputSource,
    /// Proven starting endpoint.
    pub before: InstructionFingerprint,
    /// Proven ending endpoint.
    pub after: InstructionFingerprint,
}

/// End comparison. Empty changes alone never establish complete coverage.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionComparison {
    /// Known endpoint changes, including changes found before another input failed.
    pub changes: Vec<InstructionPathChange>,
    /// Independent coverage gaps, including starting gaps and declaration changes.
    pub gaps: Vec<InstructionCoverageGap>,
    /// Actual ending sampling work.
    pub usage: InstructionSampleUsage,
}

/// Durable proof that this attempt started no provider process. No roster state is such proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionNoProcessProof {
    /// The live continuation asserts that this exact Managed attempt never called the host.
    /// An observed refusal hold or exact observed `NotLaunched` end corroborates the assertion;
    /// recovery cannot infer it from either.
    NoHostCall,
}

/// Strong comparable OS boot evidence. Other platforms remain unsupported until sourced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionBootIdentity {
    /// Exact opaque identity supplied by the Linux kernel for this boot.
    LinuxKernelBootId {
        /// Bounded content-free kernel value, never a clock or daemon-run identifier.
        value: String,
    },
}

/// Exact durable receipt associated with one physical attempt's native boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionNativeBoundaryReceipt {
    /// This attempt's recorded complete native spawn identity.
    Spawned {
        /// Exact `TurnSpawned` or owned `PtySpawned` position.
        lsn: Lsn,
    },
    /// This attempt's observed host launch failure with confirmed native cleanup.
    /// Brief execution may have occurred, so this receipt alone never certifies `NoProcess`.
    LaunchFailureEnded {
        /// Exact Observed `TurnEnded` with `NotLaunched`, never a generic failure or disappearance.
        lsn: Lsn,
    },
}

/// Scoped absence of an incumbent native process group or job, separate from comparison/review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionBoundaryAbsenceProof {
    /// The host confirmed fresh native group/job absence and successfully reaped its leader.
    /// This makes no claim about arbitrary descendants outside that native boundary.
    NativeOwnedBoundaryAbsent {
        /// Incumbent receipt attributed to this exact physical attempt.
        receipt: InstructionNativeBoundaryReceipt,
    },
    /// Comparable OS evidence proves that the starting boot has ended.
    PriorBootAbsent {
        /// Exact comparable boot stored by the starting sample.
        previous: InstructionBootIdentity,
        /// Independently sampled current boot from the same supported OS source.
        current: InstructionBootIdentity,
    },
}

/// A named reason a physical attempt cannot be certified by endpoint comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionUnknownReason {
    /// Missing durable crash-era baseline.
    MissingBaseline,
    /// Host launch may have happened before its durable receipt.
    UnrecordedLaunchBoundary,
    /// Host cleanup did not confirm absence.
    CleanupUnconfirmed,
    /// Actual Attached turn lacks a proven starting boundary.
    AttachedTurnStartMissing,
    /// Ending sampling could not establish complete coverage.
    IncompleteCoverage,
}

/// One terminal settlement. Later attempts cannot replace this attempt's uncertainty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionSettlement {
    /// Compared sampled endpoints of a recorded interval.
    Compared {
        /// Independently durable end.
        end: InstructionEndBoundary,
        /// Actual ending sample time.
        sampled_at: Timestamp,
        /// Ending sample was delayed until recovery.
        recovery: bool,
        /// Known changes together with independent uncertainty.
        comparison: InstructionComparison,
    },
    /// A prospective baseline was durable but its turn was never issued.
    NoIssuedTurn,
    /// Independent proof that this physical attempt started no provider process.
    NoProcess {
        /// Durable position supporting the proof.
        proof_lsn: Lsn,
        /// Nature of the actual proof.
        proof: InstructionNoProcessProof,
    },
    /// Honest uncertain settlement that still retains all bounded known changes.
    Unknown {
        /// Principal boundary uncertainty.
        reason: InstructionUnknownReason,
        /// Actual end, when one was recorded.
        end: Option<InstructionEndBoundary>,
        /// Actual sample time, if any sample was made.
        sampled_at: Option<Timestamp>,
        /// Evidence was settled during recovery.
        recovery: bool,
        /// Known changes and independent gaps are preserved together.
        comparison: InstructionComparison,
    },
}

/// Exact review set. The digest covers the canonical ordered outstanding interval identities
/// and their settlement positions through `through_lsn`, rather than only the newest result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstructionReviewWatermark {
    /// Durable cut of the named set.
    pub through_lsn: Lsn,
    /// Digest of that exact set.
    pub set_digest: Hex32,
}

/// Whether accepting this review also uses an existing observed goal completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionAcceptCompletion {
    /// Accept the completed ticket using existing qualified evidence.
    Completed {
        /// Existing observed completion record position.
        completion_lsn: Lsn,
    },
    /// Acknowledge instructions and continue the same work through ordinary admission.
    Continue,
}

/// Atomic operator review action, including the existing admitted rework payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionReviewAction {
    /// Acknowledge exactly the reviewed set.
    Accept {
        /// Existing completion evidence or continuing work.
        completion: InstructionAcceptCompletion,
    },
    /// Acknowledge and enqueue existing rework in this same required record.
    ChangesRequested {
        /// Existing admitted assignment target.
        agent: AgentId,
        /// Existing bounded, trimmed assignment message.
        message: Option<String>,
        /// Unfinished goal still held by the previous ticket holder, or explicit null.
        /// The same record clears this goal and queues the new assignment.
        #[serde(deserialize_with = "crate::chat::required_nullable")]
        #[schemars(with = "crate::chat::ExplicitNullable<InstructionGoalClear>")]
        clear_goal: Option<InstructionGoalClear>,
    },
}

/// Exact goal side effect of an atomic rework action, validated against the committed prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstructionGoalClear {
    /// The previous ticket holder who still holds this unfinished goal.
    pub agent: AgentId,
    /// Existing goal; completed or no-longer-held goals are never cleared by rework.
    pub goal: GoalId,
}

/// One observation interval in the operator's bounded evidence view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionReviewInterval {
    /// Observed owner, without an exclusive-writer claim.
    pub owner: InstructionObservationOwner,
    /// Exact physical attempt or real missing turn boundary.
    pub baseline: InstructionBaselineRef,
    /// Frozen ticket interval association.
    pub ticket: InstructionTicketAssociation,
    /// Settlement position; absent while comparison remains outstanding.
    pub finished_lsn: Option<Lsn>,
    /// Actual independently recorded end, when available.
    pub end: Option<InstructionEndBoundary>,
    /// Actual sample time, when available.
    pub sampled_at: Option<Timestamp>,
    /// Delayed recovery-time evidence.
    pub recovery: bool,
}

/// Path evidence shown by `ticket.read`, without instruction contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionChangedPath {
    /// Exact known changed path.
    pub path: NativeInstructionPath,
    /// Daemon-established context.
    pub context: InstructionPathContext,
}

/// The single pending/review contract shared by lifecycle, reconciliation and ticket details.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionPendingReview {
    /// Exact currently unconsumed set.
    pub watermark: InstructionReviewWatermark,
    /// A comparison is outstanding, so review actions must be refused.
    pub pending_comparison: bool,
    /// Bounded interval evidence.
    pub intervals: Vec<InstructionReviewInterval>,
    /// Bounded known changed paths.
    pub changed_paths: Vec<InstructionChangedPath>,
    /// Bounded independent coverage gaps.
    pub gaps: Vec<InstructionCoverageGap>,
}

/// Small action/evidence summary carried by the Floor card snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstructionReviewSummary {
    /// Exact currently unconsumed set.
    #[serde(with = "summary_watermark")]
    #[schemars(with = "SummaryWatermark")]
    pub watermark: InstructionReviewWatermark,
    /// Comparison remains outstanding.
    pub pending_comparison: bool,
    /// Number of retained intervals in the detail view.
    pub intervals: u32,
    /// Number of retained known changed paths.
    pub changed_paths: u32,
    /// Number of retained independent gaps.
    pub gaps: u32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SummaryWatermark {
    #[serde(with = "crate::rpc::floor::decimal_u64")]
    #[schemars(with = "String")]
    through_lsn: u64,
    set_digest: Hex32,
}

mod summary_watermark {
    use super::{InstructionReviewWatermark, SummaryWatermark};
    use dudes_types::Lsn;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &InstructionReviewWatermark,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        SummaryWatermark {
            through_lsn: value.through_lsn.0,
            set_digest: value.set_digest,
        }
        .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<InstructionReviewWatermark, D::Error> {
        let value = SummaryWatermark::deserialize(deserializer)?;
        Ok(InstructionReviewWatermark {
            through_lsn: Lsn(value.through_lsn),
            set_digest: value.set_digest,
        })
    }
}

/// A named contract validation failure, without filesystem error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstructionValidationError {
    /// Native path bytes are malformed.
    InvalidPath,
    /// Encoded path exceeds the fixed limit.
    PathTooLarge,
    /// Encoding cannot be reconstructed on this platform.
    UnsupportedPathEncoding,
    /// Input, rule, gap, change or interval collection exceeds its fixed limit.
    CollectionLimit,
    /// Participant collection exceeds its fixed limit.
    ParticipantLimit,
    /// A discovery root reference or rule is invalid.
    InvalidDiscovery,
    /// Sampling work exceeds a fixed internal limit.
    WorkLimit,
    /// Fingerprint is oversized or a known change has identical endpoints.
    InvalidFingerprint,
    /// Owner, attempt and end boundary contradict each other.
    InvalidBoundary,
    /// An uncertain sample lacks a named gap.
    MissingGap,
    /// Operator action or existing assignment text is invalid.
    InvalidReview,
    /// Encoded metadata exceeds its reserved budget.
    MetadataTooLarge,
}

impl fmt::Display for InstructionValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid instruction observation: {self:?}")
    }
}

impl std::error::Error for InstructionValidationError {}

struct EncodedCounter {
    bytes: usize,
    limit: usize,
}

impl Write for EncodedCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes) {
            return Err(io::Error::other("encoded instruction metadata limit"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encoded_fits(value: &impl Serialize, limit: usize) -> bool {
    serde_json::to_writer(EncodedCounter { bytes: 0, limit }, value).is_ok()
}

fn body_fits(value: &impl Serialize, gaps: &[InstructionCoverageGap]) -> bool {
    let limit = if gaps
        .iter()
        .any(|gap| gap.reason == InstructionGapKind::MetadataLimit)
    {
        BODY_LIMIT
    } else {
        COLLECTION_LIMIT
    };
    encoded_fits(value, limit)
}

fn collection(len: usize) -> Result<(), InstructionValidationError> {
    if len > MAX_INSTRUCTION_INPUTS {
        Err(InstructionValidationError::CollectionLimit)
    } else {
        Ok(())
    }
}

fn gaps_valid(gaps: &[InstructionCoverageGap]) -> Result<(), InstructionValidationError> {
    collection(gaps.len())?;
    for gap in gaps {
        if let Some(path) = &gap.path {
            path.validate()?;
        }
    }
    Ok(())
}

fn fingerprint_valid(
    fingerprint: InstructionFingerprint,
) -> Result<(), InstructionValidationError> {
    if matches!(fingerprint, InstructionFingerprint::File { bytes, .. } if bytes > MAX_INSTRUCTION_SOURCE_BYTES)
    {
        Err(InstructionValidationError::InvalidFingerprint)
    } else {
        Ok(())
    }
}

fn source_valid(source: &InstructionInputSource) -> Result<(), InstructionValidationError> {
    source.root.validate()?;
    if let InstructionSourceKind::Import { from, depth } = &source.kind {
        from.validate()?;
        if *depth == 0 || *depth > MAX_CLAUDE_IMPORT_DEPTH {
            return Err(InstructionValidationError::InvalidDiscovery);
        }
    }
    Ok(())
}

fn paths_valid(paths: &[NativeInstructionPath]) -> Result<(), InstructionValidationError> {
    collection(paths.len())?;
    if paths.is_empty() {
        return Err(InstructionValidationError::InvalidDiscovery);
    }
    for path in paths {
        path.validate()?;
    }
    Ok(())
}

impl InstructionDiscoveryPlan {
    /// Validate frozen root references, path encodings, families and encoded metadata.
    ///
    /// # Errors
    /// Refuses unbounded or malformed discovery plans.
    pub fn validate(&self) -> Result<(), InstructionValidationError> {
        collection(self.roots.len())?;
        collection(self.rules.len())?;
        gaps_valid(&self.gaps)?;
        for root in &self.roots {
            root.path.validate()?;
        }
        for rule in &self.rules {
            if usize::from(rule.root) >= self.roots.len() {
                return Err(InstructionValidationError::InvalidDiscovery);
            }
            if self.roots[usize::from(rule.root)].kind == InstructionRootKind::ImportHome {
                return Err(InstructionValidationError::InvalidDiscovery);
            }
            match &rule.candidate {
                InstructionCandidate::Fixed { paths } => paths_valid(paths)?,
                InstructionCandidate::Ancestor { paths, stop } => {
                    paths_valid(paths)?;
                    match stop {
                        InstructionAncestorStop::FileSystemRoot => {}
                        InstructionAncestorStop::Root { root } => {
                            if usize::from(*root) >= self.roots.len() {
                                return Err(InstructionValidationError::InvalidDiscovery);
                            }
                        }
                        InstructionAncestorStop::Marker { paths } => paths_valid(paths)?,
                    }
                }
                InstructionCandidate::Descendant { directory, paths } => {
                    directory.validate()?;
                    paths_valid(paths)?;
                }
                InstructionCandidate::MarkdownRules { directory, .. } => directory.validate()?,
                InstructionCandidate::ExplicitFile { path } => path.validate()?,
            }
        }
        if !encoded_fits(self, COLLECTION_LIMIT) {
            return Err(InstructionValidationError::MetadataTooLarge);
        }
        Ok(())
    }
}

impl InstructionSampleUsage {
    /// Validate fixed work counters.
    ///
    /// # Errors
    /// Refuses a sample that exceeded any fixed work limit.
    pub fn validate(self) -> Result<(), InstructionValidationError> {
        if self.ancestor_steps > MAX_INSTRUCTION_ANCESTORS
            || self.descendant_entries > MAX_INSTRUCTION_DESCENDANTS
            || self.descendant_depth > MAX_INSTRUCTION_DESCENDANT_DEPTH
            || self.import_edges > MAX_INSTRUCTION_IMPORT_EDGES
            || self.symlink_hops > MAX_INSTRUCTION_SYMLINK_HOPS
            || self.source_bytes > MAX_INSTRUCTION_SOURCE_BYTES
        {
            Err(InstructionValidationError::WorkLimit)
        } else {
            Ok(())
        }
    }
}

impl InstructionSnapshot {
    /// Validate starting metadata, retaining reserve for one compact overflow marker.
    ///
    /// # Errors
    /// Refuses invalid paths, over-limit work and malformed or oversized declarations.
    pub fn validate(&self) -> Result<(), InstructionValidationError> {
        if let Some(boot) = &self.boot {
            boot_valid(boot)?;
        }
        if let Some(folder) = &self.execution_folder {
            folder.validate()?;
        } else if !self.gaps.iter().any(|gap| {
            matches!(
                gap.reason,
                InstructionGapKind::PathUnresolved
                    | InstructionGapKind::MetadataLimit
                    | InstructionGapKind::MissingBaseline
            )
        }) {
            return Err(InstructionValidationError::MissingGap);
        }
        if self.participants.len() > MAX_INSTRUCTION_PARTICIPANTS {
            return Err(InstructionValidationError::ParticipantLimit);
        }
        collection(self.inputs.len())?;
        collection(self.inventoried.len())?;
        gaps_valid(&self.gaps)?;
        self.usage.validate()?;
        if self.participants.is_empty() && self.gaps.is_empty() {
            return Err(InstructionValidationError::MissingGap);
        }
        if self.source == InstructionSnapshotSource::MissingBaseline
            && !self
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::MissingBaseline)
        {
            return Err(InstructionValidationError::MissingGap);
        }
        self.validate_sources()?;
        if !body_fits(self, &self.gaps) {
            return Err(InstructionValidationError::MetadataTooLarge);
        }
        Ok(())
    }

    fn validate_sources(&self) -> Result<(), InstructionValidationError> {
        for participant in &self.participants {
            participant.discovery.validate()?;
            if participant.mode == InstructionDeclarationMode::Unknown
                && !participant
                    .discovery
                    .gaps
                    .iter()
                    .any(|gap| gap.reason == InstructionGapKind::DeclarationMissing)
            {
                return Err(InstructionValidationError::MissingGap);
            }
        }
        for rule in &self.inventoried {
            let participant = self
                .participants
                .iter()
                .find(|participant| participant.agent == rule.agent)
                .ok_or(InstructionValidationError::InvalidDiscovery)?;
            if usize::from(rule.rule) >= participant.discovery.rules.len() {
                return Err(InstructionValidationError::InvalidDiscovery);
            }
        }
        for input in &self.inputs {
            input.path.validate()?;
            if let Some(path) = &input.resolved_path {
                path.validate()?;
            }
            if input.fingerprint == InstructionFingerprint::Absent && input.resolved_path.is_some()
            {
                return Err(InstructionValidationError::InvalidFingerprint);
            }
            source_valid(&input.source)?;
            fingerprint_valid(input.fingerprint)?;
            let participant = self
                .participants
                .iter()
                .find(|participant| participant.agent == input.source.agent)
                .ok_or(InstructionValidationError::InvalidDiscovery)?;
            let rule = participant
                .discovery
                .rules
                .get(usize::from(input.source.rule))
                .ok_or(InstructionValidationError::InvalidDiscovery)?;
            if participant.discovery.roots[usize::from(rule.root)].path != input.source.root
                || (matches!(input.source.kind, InstructionSourceKind::Import { .. })
                    && rule.imports != InstructionImportPolicy::ClaudeFourHops)
            {
                return Err(InstructionValidationError::InvalidDiscovery);
            }
        }
        Ok(())
    }

    /// Append an input only if it fits after JSON escaping; preserve existing inputs on overflow.
    /// Returns false and adds a compact unknown marker when the next input does not fit.
    #[must_use]
    pub fn try_push_input(&mut self, input: InstructionInput) -> bool {
        if input.path.validate().is_err()
            || input
                .resolved_path
                .as_ref()
                .is_some_and(|path| path.validate().is_err())
            || (input.fingerprint == InstructionFingerprint::Absent
                && input.resolved_path.is_some())
            || source_valid(&input.source).is_err()
            || fingerprint_valid(input.fingerprint).is_err()
            || self.inputs.len() >= MAX_INSTRUCTION_INPUTS
        {
            mark_overflow(&mut self.gaps);
            return false;
        }
        self.inputs.push(input);
        if encoded_fits(self, COLLECTION_LIMIT) {
            true
        } else {
            let _ = self.inputs.pop();
            mark_overflow(&mut self.gaps);
            false
        }
    }

    /// Retain an independent gap if it fits; otherwise retain a compact overflow marker.
    #[must_use]
    pub fn try_push_gap(&mut self, gap: InstructionCoverageGap) -> bool {
        if gap
            .path
            .as_ref()
            .is_some_and(|path| path.validate().is_err())
            || self.gaps.len() >= MAX_INSTRUCTION_INPUTS - 1
        {
            mark_overflow(&mut self.gaps);
            return false;
        }
        self.gaps.push(gap);
        if encoded_fits(self, COLLECTION_LIMIT) {
            true
        } else {
            let _ = self.gaps.pop();
            mark_overflow(&mut self.gaps);
            false
        }
    }
}

fn mark_overflow(gaps: &mut Vec<InstructionCoverageGap>) {
    if !gaps
        .iter()
        .any(|gap| gap.reason == InstructionGapKind::MetadataLimit)
    {
        if gaps.len() == MAX_INSTRUCTION_INPUTS {
            let _ = gaps.pop();
        }
        gaps.push(InstructionCoverageGap::metadata_limit());
    }
}

impl InstructionComparison {
    /// Validate changes and gaps without conflating unknown coverage with absence.
    ///
    /// # Errors
    /// Refuses malformed or oversized metadata and invalid known changes.
    pub fn validate(&self) -> Result<(), InstructionValidationError> {
        collection(self.changes.len())?;
        gaps_valid(&self.gaps)?;
        self.usage.validate()?;
        for change in &self.changes {
            change.path.validate()?;
            source_valid(&change.source)?;
            fingerprint_valid(change.before)?;
            fingerprint_valid(change.after)?;
            if change.before == change.after {
                return Err(InstructionValidationError::InvalidFingerprint);
            }
        }
        if !body_fits(self, &self.gaps) {
            return Err(InstructionValidationError::MetadataTooLarge);
        }
        Ok(())
    }

    /// Derive unchanged only from empty changes and complete, valid coverage.
    #[must_use]
    pub fn is_unchanged(&self) -> bool {
        self.changes.is_empty() && self.gaps.is_empty() && self.validate().is_ok()
    }

    /// Whether known shared path changes require operator review.
    #[must_use]
    pub fn has_shared_changes(&self) -> bool {
        self.changes
            .iter()
            .any(|change| change.context.needs_shared_review())
    }

    /// Whether uncertain shared input coverage requires operator review.
    #[must_use]
    pub fn has_shared_gaps(&self) -> bool {
        self.gaps
            .iter()
            .any(|gap| gap.context.needs_shared_review())
    }

    /// Keep the existing known changes when another change cannot fit; add an unknown marker.
    #[must_use]
    pub fn try_push_change(&mut self, change: InstructionPathChange) -> bool {
        if change.path.validate().is_err()
            || source_valid(&change.source).is_err()
            || fingerprint_valid(change.before).is_err()
            || fingerprint_valid(change.after).is_err()
            || change.before == change.after
            || self.changes.len() >= MAX_INSTRUCTION_INPUTS
        {
            mark_overflow(&mut self.gaps);
            return false;
        }
        self.changes.push(change);
        if encoded_fits(self, COLLECTION_LIMIT) {
            true
        } else {
            let _ = self.changes.pop();
            mark_overflow(&mut self.gaps);
            false
        }
    }

    /// Add independent uncertainty without discarding already retained known changes.
    #[must_use]
    pub fn try_push_gap(&mut self, gap: InstructionCoverageGap) -> bool {
        if gap
            .path
            .as_ref()
            .is_some_and(|path| path.validate().is_err())
            || self.gaps.len() >= MAX_INSTRUCTION_INPUTS - 1
        {
            mark_overflow(&mut self.gaps);
            return false;
        }
        self.gaps.push(gap);
        if encoded_fits(self, COLLECTION_LIMIT) {
            true
        } else {
            let _ = self.gaps.pop();
            mark_overflow(&mut self.gaps);
            false
        }
    }
}

impl InstructionPendingReview {
    /// Validate the bounded detail view at its output boundary.
    ///
    /// # Errors
    /// Refuses malformed or oversized evidence.
    pub fn validate(&self) -> Result<(), InstructionValidationError> {
        collection(self.intervals.len())?;
        collection(self.changed_paths.len())?;
        gaps_valid(&self.gaps)?;
        for changed in &self.changed_paths {
            changed.path.validate()?;
        }
        if !body_fits(self, &self.gaps) {
            return Err(InstructionValidationError::MetadataTooLarge);
        }
        Ok(())
    }

    /// Derive the compact snapshot summary from this bounded detail view.
    #[must_use]
    pub fn summary(&self) -> InstructionReviewSummary {
        let count = |len| u32::try_from(len).unwrap_or(u32::MAX);
        InstructionReviewSummary {
            watermark: self.watermark,
            pending_comparison: self.pending_comparison,
            intervals: count(self.intervals.len()),
            changed_paths: count(self.changed_paths.len()),
            gaps: count(self.gaps.len()),
        }
    }
}

fn owner_valid(owner: InstructionObservationOwner) -> Result<(), InstructionValidationError> {
    if matches!(owner, InstructionObservationOwner::AttachedProcess { attachment_lsn, .. } if attachment_lsn == Lsn::ZERO)
        || matches!(owner, InstructionObservationOwner::Managed { issue: InstructionIssueAssociation::Issued {issue_lsn}, .. } if issue_lsn == Lsn::ZERO)
        || matches!(
            owner,
            InstructionObservationOwner::AttachedTurn {
                attachment_lsn: Some(_),
                ..
            }
        )
    {
        Err(InstructionValidationError::InvalidBoundary)
    } else {
        Ok(())
    }
}

fn boundary_matches(owner: InstructionObservationOwner, end: InstructionEndBoundary) -> bool {
    match (owner, end) {
        (
            InstructionObservationOwner::Managed { turn, .. },
            InstructionEndBoundary::ManagedTurn {
                turn: ended,
                end_lsn,
            },
        )
        | (
            InstructionObservationOwner::AttachedTurn { turn, .. },
            InstructionEndBoundary::AttachedTurn {
                turn: ended,
                end_lsn,
            },
        ) => turn == ended && end_lsn != Lsn::ZERO,
        (
            InstructionObservationOwner::AttachedProcess { attachment_lsn, .. },
            InstructionEndBoundary::AttachedProcess {
                attachment_lsn: ended,
                end_lsn,
            },
        ) => attachment_lsn == ended && end_lsn != Lsn::ZERO,
        _ => false,
    }
}

fn unknown_coverage_valid(
    reason: InstructionUnknownReason,
    comparison: &InstructionComparison,
) -> Result<(), InstructionValidationError> {
    comparison.validate()?;
    let required_gap = match reason {
        InstructionUnknownReason::MissingBaseline => Some(InstructionGapKind::MissingBaseline),
        InstructionUnknownReason::UnrecordedLaunchBoundary => {
            Some(InstructionGapKind::UnrecordedLaunchBoundary)
        }
        InstructionUnknownReason::CleanupUnconfirmed => {
            Some(InstructionGapKind::CleanupUnconfirmed)
        }
        InstructionUnknownReason::AttachedTurnStartMissing => {
            Some(InstructionGapKind::AttachedTurnStartMissing)
        }
        InstructionUnknownReason::IncompleteCoverage => None,
    };
    if comparison.gaps.is_empty()
        || required_gap
            .is_some_and(|required| !comparison.gaps.iter().any(|gap| gap.reason == required))
    {
        Err(InstructionValidationError::MissingGap)
    } else {
        Ok(())
    }
}

fn settlement_valid(
    owner: InstructionObservationOwner,
    baseline: InstructionBaselineRef,
    settlement: &InstructionSettlement,
) -> Result<(), InstructionValidationError> {
    owner_valid(owner)?;
    if let InstructionObservationOwner::AttachedTurn { turn, .. } = owner
        && (!matches!(baseline, InstructionBaselineRef::MissingTurnBoundary { turn: actual, .. } if turn == actual)
            || !matches!(
                settlement,
                InstructionSettlement::Unknown {
                    reason: InstructionUnknownReason::AttachedTurnStartMissing,
                    ..
                }
            ))
    {
        return Err(InstructionValidationError::InvalidBoundary);
    }
    let end = match settlement {
        InstructionSettlement::Compared {
            end, comparison, ..
        } => {
            comparison.validate()?;
            if matches!(end, InstructionEndBoundary::AttachedTurn { .. }) {
                return Err(InstructionValidationError::InvalidBoundary);
            }
            Some(*end)
        }
        InstructionSettlement::Unknown {
            reason,
            end,
            comparison,
            ..
        } => {
            unknown_coverage_valid(*reason, comparison)?;
            *end
        }
        InstructionSettlement::NoIssuedTurn => {
            if !matches!(
                owner,
                InstructionObservationOwner::Managed {
                    issue: InstructionIssueAssociation::Prospective,
                    ..
                }
            ) {
                return Err(InstructionValidationError::InvalidBoundary);
            }
            None
        }
        InstructionSettlement::NoProcess { proof_lsn, .. } => {
            if *proof_lsn == Lsn::ZERO {
                return Err(InstructionValidationError::InvalidBoundary);
            }
            None
        }
    };
    if end.is_some_and(|end| !boundary_matches(owner, end)) {
        return Err(InstructionValidationError::InvalidBoundary);
    }
    match baseline {
        InstructionBaselineRef::Started { lsn } if lsn == Lsn::ZERO => {
            Err(InstructionValidationError::InvalidBoundary)
        }
        InstructionBaselineRef::MissingTurnBoundary { turn, end_lsn } => {
            if end_lsn == Lsn::ZERO
                || !matches!(settlement, InstructionSettlement::Unknown { .. })
                || !matches!(end, Some(InstructionEndBoundary::ManagedTurn { turn: actual, end_lsn: actual_lsn } | InstructionEndBoundary::AttachedTurn { turn: actual, end_lsn: actual_lsn }) if turn == actual && end_lsn == actual_lsn)
            {
                Err(InstructionValidationError::InvalidBoundary)
            } else {
                Ok(())
            }
        }
        InstructionBaselineRef::MissingIssueBoundary { turn, issue_lsn } => {
            missing_issue_settlement(owner, turn, issue_lsn, settlement)
        }
        InstructionBaselineRef::Started { .. } => Ok(()),
    }
}

fn missing_issue_settlement(
    owner: InstructionObservationOwner,
    turn: TurnId,
    issue_lsn: Lsn,
    settlement: &InstructionSettlement,
) -> Result<(), InstructionValidationError> {
    if issue_lsn == Lsn::ZERO
        || !matches!(owner, InstructionObservationOwner::Managed {
            turn: actual,
            issue: InstructionIssueAssociation::Issued { issue_lsn: actual_lsn },
            ..
        } if actual == turn && actual_lsn == issue_lsn)
    {
        return Err(InstructionValidationError::InvalidBoundary);
    }
    let InstructionSettlement::Unknown {
        reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
        end: None,
        sampled_at: None,
        recovery: true,
        comparison,
    } = settlement
    else {
        return Err(InstructionValidationError::InvalidBoundary);
    };
    if !comparison.changes.is_empty() || comparison.usage != InstructionSampleUsage::default() {
        return Err(InstructionValidationError::InvalidBoundary);
    }
    if !comparison
        .gaps
        .iter()
        .any(|gap| gap.reason == InstructionGapKind::MissingBaseline)
    {
        return Err(InstructionValidationError::MissingGap);
    }
    Ok(())
}

/// Validate an instruction record before append and during replay. Other record kinds are
/// outside this validator. The journal's exact notification and newline check still applies.
///
/// # Errors
/// Refuses malformed boundaries, unauthorized review actors, unbounded metadata or invalid rework.
pub fn validate_instruction_record(record: &Record) -> Result<(), InstructionValidationError> {
    match record {
        Record::InstructionObservationStarted {
            owner, snapshot, ..
        } => {
            snapshot.validate()?;
            owner_valid(*owner)?;
            if matches!(owner, InstructionObservationOwner::AttachedTurn { .. }) {
                return Err(InstructionValidationError::InvalidBoundary);
            }
        }
        Record::InstructionObservationFinished {
            owner,
            baseline,
            settlement,
            ..
        } => settlement_valid(*owner, *baseline, settlement)?,
        Record::TicketInstructionReviewed {
            by,
            watermark,
            action,
            ..
        } => {
            if !matches!(by, Actor::Peer { .. } | Actor::BrowserSession { .. })
                || watermark.through_lsn == Lsn::ZERO
            {
                return Err(InstructionValidationError::InvalidReview);
            }
            match action {
                InstructionReviewAction::Accept {
                    completion: InstructionAcceptCompletion::Completed { completion_lsn },
                } if *completion_lsn == Lsn::ZERO => {
                    return Err(InstructionValidationError::InvalidReview);
                }
                InstructionReviewAction::ChangesRequested {
                    message: Some(message),
                    ..
                } if message != crate::ticket::trim(message)
                    || !crate::ticket::clean_text(message, crate::ticket::MAX_MESSAGE_BYTES) =>
                {
                    return Err(InstructionValidationError::InvalidReview);
                }
                _ => {}
            }
        }
        Record::InstructionAttemptBoundarySettled {
            owner,
            started_lsn,
            proof,
        } => {
            owner_valid(*owner)?;
            if *started_lsn == Lsn::ZERO
                || matches!(owner, InstructionObservationOwner::AttachedTurn { .. })
            {
                return Err(InstructionValidationError::InvalidBoundary);
            }
            match proof {
                InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent { receipt } => {
                    let lsn = match receipt {
                        InstructionNativeBoundaryReceipt::Spawned { lsn }
                        | InstructionNativeBoundaryReceipt::LaunchFailureEnded { lsn } => *lsn,
                    };
                    if lsn == Lsn::ZERO {
                        return Err(InstructionValidationError::InvalidBoundary);
                    }
                }
                InstructionBoundaryAbsenceProof::PriorBootAbsent { previous, current } => {
                    boot_valid(previous)?;
                    boot_valid(current)?;
                    if previous == current {
                        return Err(InstructionValidationError::InvalidBoundary);
                    }
                }
            }
        }
        _ => return Ok(()),
    }
    let entry = Entry {
        at: Timestamp::from_unix_millis(i64::MIN),
        record: record.clone(),
    };
    if !encoded_fits(&entry, MAX_INSTRUCTION_METADATA_BYTES) {
        return Err(InstructionValidationError::MetadataTooLarge);
    }
    Ok(())
}

fn boot_valid(boot: &InstructionBootIdentity) -> Result<(), InstructionValidationError> {
    match boot {
        InstructionBootIdentity::LinuxKernelBootId { value }
            if value.is_empty() || !crate::ticket::clean_title(value, 128) =>
        {
            Err(InstructionValidationError::InvalidBoundary)
        }
        InstructionBootIdentity::LinuxKernelBootId { .. } => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::samples;
    use crate::rpc::ticket::{
        TicketAcceptOutcome, TicketAcceptParams, TicketAcceptResult, TicketAssignParams,
    };

    fn path(value: &str) -> NativeInstructionPath {
        NativeInstructionPath::Utf8 {
            value: value.into(),
        }
    }

    fn source() -> InstructionInputSource {
        InstructionInputSource {
            agent: samples::agent(),
            rule: 0,
            root: path("/project"),
            kind: InstructionSourceKind::Fixed,
        }
    }

    fn gap(reason: InstructionGapKind) -> InstructionCoverageGap {
        InstructionCoverageGap {
            reason,
            context: InstructionPathContext::SharedProject,
            agent: Some(samples::agent()),
            path: None,
        }
    }

    fn change(value: &str) -> InstructionPathChange {
        InstructionPathChange {
            path: path(value),
            context: InstructionPathContext::SharedProject,
            source: source(),
            before: InstructionFingerprint::Absent,
            after: InstructionFingerprint::File {
                digest: Hex32::new([2; 32]),
                bytes: 1,
            },
        }
    }

    fn snapshot() -> InstructionSnapshot {
        InstructionSnapshot {
            sampled_at: Timestamp::UNIX_EPOCH,
            source: InstructionSnapshotSource::Fresh,
            boot: None,
            execution_folder: Some(path("/project")),
            participants: vec![InstructionParticipant {
                agent: samples::agent(),
                provider: Provider::Claude,
                mode: InstructionDeclarationMode::ClaudeDefault,
                declaration_digest: Hex32::new([1; 32]),
                discovery: InstructionDiscoveryPlan {
                    roots: vec![InstructionRoot {
                        path: path("/project"),
                        kind: InstructionRootKind::Execution,
                        context: InstructionPathContext::SharedProject,
                    }],
                    rules: vec![InstructionDiscoveryRule {
                        root: 0,
                        candidate: InstructionCandidate::Fixed {
                            paths: vec![path("CLAUDE.md")],
                        },
                        selection: InstructionSelection::All,
                        activation: InstructionActivation::Active,
                        imports: InstructionImportPolicy::ClaudeFourHops,
                        evidence: InstructionDiscoveryEvidence {
                            kind: InstructionDiscoveryEvidenceKind::VersionedSource,
                            digest: Hex32::new([3; 32]),
                        },
                    }],
                    configuration_digest: None,
                    gaps: Vec::new(),
                },
            }],
            inputs: vec![InstructionInput {
                path: path("/project/CLAUDE.md"),
                resolved_path: None,
                context: InstructionPathContext::SharedProject,
                source: source(),
                activation: InstructionActivation::Active,
                shadowed: false,
                fingerprint: InstructionFingerprint::Absent,
            }],
            inventoried: vec![InstructionRuleRef {
                agent: samples::agent(),
                rule: 0,
            }],
            gaps: Vec::new(),
            usage: InstructionSampleUsage::default(),
        }
    }

    fn owner() -> InstructionObservationOwner {
        InstructionObservationOwner::Managed {
            agent: samples::agent(),
            turn: samples::turn(),
            issue: InstructionIssueAssociation::Prospective,
        }
    }

    fn finished(settlement: InstructionSettlement) -> Record {
        Record::InstructionObservationFinished {
            owner: owner(),
            ticket: None,
            baseline: InstructionBaselineRef::Started { lsn: Lsn(11) },
            settlement,
        }
    }

    #[test]
    fn instruction_kinds_have_fixed_required_tags_and_leave_the_historical_array_unchanged() {
        assert_eq!(crate::RecordKind::ALL.len(), 46);
        for (kind, tag, name) in [
            (
                crate::RecordKind::InstructionObservationStarted,
                76,
                "instruction_observation_started",
            ),
            (
                crate::RecordKind::InstructionObservationFinished,
                77,
                "instruction_observation_finished",
            ),
            (
                crate::RecordKind::TicketInstructionReviewed,
                78,
                "ticket_instruction_reviewed",
            ),
        ] {
            assert_eq!(kind.tag(), tag);
            assert_eq!(crate::RecordKind::from_tag(tag), Some(kind));
            assert!(kind.requires_required_frame());
            let record = samples::record(kind);
            assert_eq!(record.kind(), kind);
            assert!(record.requires_required_frame());
            assert_eq!(validate_instruction_record(&record), Ok(()));
            let json = serde_json::to_value(&record).unwrap();
            assert_eq!(json["t"], name);
            assert_eq!(serde_json::from_value::<Record>(json).unwrap(), record);
        }
        assert_eq!(crate::RecordKind::from_tag(80), None);
    }

    #[test]
    fn literal_native_encodings_preserve_bytes_and_escape_display() {
        let utf8 = path("/p/é\n\u{202e}file");
        assert_eq!(utf8.display(), "/p/é\\n\\u{202e}file");
        let unix = NativeInstructionPath::UnixBytes {
            hex: "2f70ff".into(),
        };
        let windows = NativeInstructionPath::WindowsWide {
            units: vec![0x43, 0x3a, 0x5c, 0xd800],
        };
        for (native, expected) in [
            (
                unix,
                serde_json::json!({"encoding":"unix_bytes","hex":"2f70ff"}),
            ),
            (
                windows,
                serde_json::json!({"encoding":"windows_wide","units":[67,58,92,55296]}),
            ),
        ] {
            assert_eq!(native.validate(), Ok(()));
            assert_eq!(serde_json::to_value(&native).unwrap(), expected);
            assert_eq!(
                serde_json::from_value::<NativeInstructionPath>(expected).unwrap(),
                native
            );
        }
        for invalid in [
            NativeInstructionPath::UnixBytes { hex: "FF".into() },
            NativeInstructionPath::UnixBytes { hex: "2f0".into() },
            NativeInstructionPath::UnixBytes { hex: "2f00".into() },
            NativeInstructionPath::WindowsWide { units: vec![0] },
            path(""),
            path("a\0b"),
        ] {
            assert_eq!(
                invalid.validate(),
                Err(InstructionValidationError::InvalidPath)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_unix_path_round_trips_through_the_native_path_api() {
        use std::os::unix::ffi::OsStringExt as _;
        let native = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', b'p', 0xff]));
        let encoded = NativeInstructionPath::from_path(&native).unwrap();
        assert_eq!(
            encoded,
            NativeInstructionPath::UnixBytes {
                hex: "2f70ff".into()
            }
        );
        assert_eq!(encoded.to_path().unwrap(), native);
    }

    #[cfg(windows)]
    #[test]
    fn unpaired_windows_path_round_trips_through_the_native_path_api() {
        use std::os::windows::ffi::OsStringExt as _;
        let native = PathBuf::from(std::ffi::OsString::from_wide(&[0x43, 0x3a, 0x5c, 0xd800]));
        let encoded = NativeInstructionPath::from_path(&native).unwrap();
        assert_eq!(
            encoded,
            NativeInstructionPath::WindowsWide {
                units: vec![0x43, 0x3a, 0x5c, 0xd800]
            }
        );
        assert_eq!(encoded.to_path().unwrap(), native);
    }

    #[test]
    fn path_limit_counts_encoded_json_escaping_and_the_encoding_envelope() {
        assert_eq!(path(&"\u{1}".repeat(600)).validate(), Ok(()));
        assert_eq!(
            path(&"\u{1}".repeat(700)).validate(),
            Err(InstructionValidationError::PathTooLarge)
        );
        assert_eq!(
            path(&"x".repeat(MAX_INSTRUCTION_PATH_BYTES)).validate(),
            Err(InstructionValidationError::PathTooLarge)
        );
    }

    #[test]
    fn known_changes_survive_read_gaps_and_encoded_metadata_overflow() {
        let mut comparison = InstructionComparison::default();
        assert!(comparison.is_unchanged());
        comparison.gaps.push(gap(InstructionGapKind::ReadFailed));
        assert!(!comparison.is_unchanged());
        assert!(comparison.try_push_change(change("/project/CLAUDE.md")));
        let first = comparison.changes[0].clone();
        for index in 0..MAX_INSTRUCTION_INPUTS {
            let value = format!("/project/{index}/{}", "\u{1}".repeat(600));
            if !comparison.try_push_change(change(&value)) {
                break;
            }
        }
        assert_eq!(comparison.changes[0], first);
        assert!(comparison.has_shared_changes());
        assert!(comparison.has_shared_gaps());
        assert!(
            comparison
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::ReadFailed)
        );
        assert!(
            comparison
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::MetadataLimit)
        );
        assert_eq!(comparison.validate(), Ok(()));
        assert!(!comparison.is_unchanged());
        let record = finished(InstructionSettlement::Compared {
            end: InstructionEndBoundary::ManagedTurn {
                turn: samples::turn(),
                end_lsn: Lsn(12),
            },
            sampled_at: Timestamp::from_unix_millis(i64::MAX),
            recovery: true,
            comparison,
        });
        assert_eq!(validate_instruction_record(&record), Ok(()));
        let entry = Entry {
            at: Timestamp::from_unix_millis(i64::MIN),
            record,
        };
        let notification = crate::rpc::journal::journal_appended(Lsn(u64::MAX), &entry);
        assert!(serde_json::to_vec(&notification).unwrap().len() < crate::rpc::MAX_FRAME_BYTES);
        assert!(serde_json::to_vec(&entry).unwrap().len() <= MAX_INSTRUCTION_METADATA_BYTES);
    }

    #[test]
    fn gap_metadata_overflow_keeps_previously_observed_changes() {
        let mut comparison = InstructionComparison::default();
        assert!(comparison.try_push_change(change("/project/CLAUDE.md")));
        assert!(comparison.try_push_gap(gap(InstructionGapKind::ConfigurationUnknown)));
        for index in 0..MAX_INSTRUCTION_INPUTS {
            let mut failed = gap(InstructionGapKind::ReadFailed);
            failed.path = Some(path(&format!("/project/{index}/{}", "\u{1}".repeat(600))));
            if !comparison.try_push_gap(failed) {
                break;
            }
        }
        assert_eq!(comparison.validate(), Ok(()));
        assert_eq!(comparison.changes[0].path, path("/project/CLAUDE.md"));
        assert!(
            comparison
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::ConfigurationUnknown)
        );
        assert!(
            comparison
                .gaps
                .iter()
                .any(|gap| gap.reason == InstructionGapKind::MetadataLimit)
        );
        assert!(!comparison.is_unchanged());
    }

    #[test]
    fn fixed_sampling_limits_accept_the_boundary_and_refuse_each_excess() {
        let boundary = InstructionSampleUsage {
            ancestor_steps: 64,
            descendant_entries: 4096,
            descendant_depth: 16,
            import_edges: 512,
            symlink_hops: 8,
            source_bytes: 1_048_576,
        };
        assert_eq!(boundary.validate(), Ok(()));
        for excessive in [
            InstructionSampleUsage {
                ancestor_steps: 65,
                ..boundary
            },
            InstructionSampleUsage {
                descendant_entries: 4097,
                ..boundary
            },
            InstructionSampleUsage {
                descendant_depth: 17,
                ..boundary
            },
            InstructionSampleUsage {
                import_edges: 513,
                ..boundary
            },
            InstructionSampleUsage {
                symlink_hops: 9,
                ..boundary
            },
            InstructionSampleUsage {
                source_bytes: 1_048_577,
                ..boundary
            },
        ] {
            assert_eq!(
                excessive.validate(),
                Err(InstructionValidationError::WorkLimit)
            );
        }
    }

    #[test]
    fn inventory_is_independent_of_read_gaps_and_import_home_is_resolution_only() {
        let mut sampled = snapshot();
        let mut failed = gap(InstructionGapKind::ReadFailed);
        failed.path = Some(path("/project/other.md"));
        sampled.gaps.push(failed);
        assert_eq!(sampled.validate(), Ok(()));
        let json = serde_json::to_value(&sampled).unwrap();
        assert_eq!(
            json["inventoried"],
            serde_json::json!([{ "agent": samples::agent(), "rule": 0 }])
        );
        assert_eq!(json["gaps"][0]["path"]["value"], "/project/other.md");
        sampled.participants[0]
            .discovery
            .roots
            .push(InstructionRoot {
                path: path("/profile"),
                kind: InstructionRootKind::ImportHome,
                context: InstructionPathContext::SharedInput {
                    source: InstructionSharedInput::Import,
                },
            });
        assert_eq!(sampled.validate(), Ok(()));
        sampled.participants[0].discovery.rules[0].root = 1;
        assert_eq!(
            sampled.validate(),
            Err(InstructionValidationError::InvalidDiscovery)
        );
    }

    #[test]
    fn input_and_participant_limits_are_independent_of_the_byte_limit() {
        let mut sampled = snapshot();
        sampled.inputs = vec![sampled.inputs[0].clone(); MAX_INSTRUCTION_INPUTS + 1];
        assert_eq!(
            sampled.validate(),
            Err(InstructionValidationError::CollectionLimit)
        );
        sampled = snapshot();
        sampled.participants =
            vec![sampled.participants[0].clone(); MAX_INSTRUCTION_PARTICIPANTS + 1];
        assert_eq!(
            sampled.validate(),
            Err(InstructionValidationError::ParticipantLimit)
        );
    }

    #[test]
    fn missing_baseline_samples_and_unknown_settlements_cannot_certify_unchanged() {
        let mut sampled = snapshot();
        sampled.source = InstructionSnapshotSource::MissingBaseline;
        assert_eq!(
            sampled.validate(),
            Err(InstructionValidationError::MissingGap)
        );
        sampled.gaps.push(gap(InstructionGapKind::MissingBaseline));
        assert_eq!(sampled.validate(), Ok(()));
        let mut comparison = InstructionComparison::default();
        assert!(comparison.try_push_change(change("/project/CLAUDE.md")));
        comparison
            .gaps
            .push(gap(InstructionGapKind::CleanupUnconfirmed));
        let record = finished(InstructionSettlement::Unknown {
            reason: InstructionUnknownReason::CleanupUnconfirmed,
            end: None,
            sampled_at: None,
            recovery: true,
            comparison,
        });
        assert_eq!(validate_instruction_record(&record), Ok(()));
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["settlement"]["reason"], "cleanup_unconfirmed");
        assert_eq!(
            json["settlement"]["comparison"]["changes"][0]["path"]["value"],
            "/project/CLAUDE.md"
        );
        let invalid = finished(InstructionSettlement::Unknown {
            reason: InstructionUnknownReason::CleanupUnconfirmed,
            end: None,
            sampled_at: None,
            recovery: true,
            comparison: InstructionComparison::default(),
        });
        assert_eq!(
            validate_instruction_record(&invalid),
            Err(InstructionValidationError::MissingGap)
        );
    }

    #[test]
    fn unresolved_or_unencodable_execution_folders_are_durable_unknowns() {
        let mut sampled = snapshot();
        sampled.execution_folder = None;
        assert_eq!(
            sampled.validate(),
            Err(InstructionValidationError::MissingGap)
        );
        assert!(sampled.try_push_gap(gap(InstructionGapKind::PathUnresolved)));
        let record = Record::InstructionObservationStarted {
            owner: owner(),
            ticket: None,
            snapshot: sampled,
        };
        assert_eq!(validate_instruction_record(&record), Ok(()));
        let json = serde_json::to_value(record).unwrap();
        assert_eq!(
            json["snapshot"]["execution_folder"],
            serde_json::Value::Null
        );
        assert_eq!(json["snapshot"]["gaps"][0]["reason"], "path_unresolved");
        let mut sampled = snapshot();
        sampled.execution_folder = None;
        assert!(sampled.try_push_gap(InstructionCoverageGap::metadata_limit()));
        assert_eq!(sampled.validate(), Ok(()));
    }

    #[test]
    fn instruction_review_has_a_distinct_loop_hold_reason() {
        let decision = crate::LoopDecision::Hold {
            reason: crate::HoldReason::SharedInstructionsReview,
        };
        let json = serde_json::to_value(&decision).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind":"hold","reason":"shared_instructions_review"})
        );
        assert_eq!(
            serde_json::from_value::<crate::LoopDecision>(json).unwrap(),
            decision
        );
    }

    #[test]
    fn orphan_baselines_are_not_retry_absence_proof() {
        let mut record = finished(InstructionSettlement::NoIssuedTurn);
        assert_eq!(validate_instruction_record(&record), Ok(()));
        if let Record::InstructionObservationFinished { owner, .. } = &mut record {
            *owner = InstructionObservationOwner::Managed {
                agent: samples::agent(),
                turn: samples::turn(),
                issue: InstructionIssueAssociation::Issued { issue_lsn: Lsn(7) },
            };
        }
        assert_eq!(
            validate_instruction_record(&record),
            Err(InstructionValidationError::InvalidBoundary)
        );
        assert_eq!(
            validate_instruction_record(&finished(InstructionSettlement::NoProcess {
                proof_lsn: Lsn::ZERO,
                proof: InstructionNoProcessProof::NoHostCall
            })),
            Err(InstructionValidationError::InvalidBoundary)
        );
    }

    #[test]
    fn attached_missing_turn_boundary_uses_the_actual_turn_and_cannot_be_compared() {
        let owner = InstructionObservationOwner::AttachedTurn {
            agent: samples::agent(),
            turn: samples::turn(),
            attachment_lsn: None,
        };
        let baseline = InstructionBaselineRef::MissingTurnBoundary {
            turn: samples::turn(),
            end_lsn: Lsn(12),
        };
        let end = InstructionEndBoundary::AttachedTurn {
            turn: samples::turn(),
            end_lsn: Lsn(12),
        };
        let mut comparison = InstructionComparison::default();
        comparison
            .gaps
            .push(gap(InstructionGapKind::AttachedTurnStartMissing));
        let mut record = Record::InstructionObservationFinished {
            owner,
            ticket: None,
            baseline,
            settlement: InstructionSettlement::Unknown {
                reason: InstructionUnknownReason::AttachedTurnStartMissing,
                end: Some(end),
                sampled_at: None,
                recovery: false,
                comparison,
            },
        };
        assert_eq!(validate_instruction_record(&record), Ok(()));
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(
            json["owner"],
            serde_json::json!({ "kind": "attached_turn", "agent": samples::agent(), "turn": samples::turn(), "attachment_lsn": null })
        );
        assert_eq!(
            json["baseline"]["turn"],
            serde_json::to_value(samples::turn()).unwrap()
        );
        if let Record::InstructionObservationFinished { owner, .. } = &mut record {
            *owner = InstructionObservationOwner::AttachedTurn {
                agent: samples::agent(),
                turn: samples::turn(),
                attachment_lsn: Some(Lsn(7)),
            };
        }
        assert_eq!(
            validate_instruction_record(&record),
            Err(InstructionValidationError::InvalidBoundary)
        );
        if let Record::InstructionObservationFinished {
            owner: recorded, ..
        } = &mut record
        {
            *recorded = owner;
        }
        if let Record::InstructionObservationFinished { settlement, .. } = &mut record {
            *settlement = InstructionSettlement::Compared {
                end,
                sampled_at: Timestamp::UNIX_EPOCH,
                recovery: false,
                comparison: InstructionComparison::default(),
            };
        }
        assert_eq!(
            validate_instruction_record(&record),
            Err(InstructionValidationError::InvalidBoundary)
        );
        for settlement in [
            InstructionSettlement::NoIssuedTurn,
            InstructionSettlement::NoProcess {
                proof_lsn: Lsn(13),
                proof: InstructionNoProcessProof::NoHostCall,
            },
        ] {
            if let Record::InstructionObservationFinished {
                settlement: recorded,
                ..
            } = &mut record
            {
                *recorded = settlement;
            }
            assert_eq!(
                validate_instruction_record(&record),
                Err(InstructionValidationError::InvalidBoundary)
            );
        }
        let started = Record::InstructionObservationStarted {
            owner,
            ticket: None,
            snapshot: snapshot(),
        };
        assert_eq!(
            validate_instruction_record(&started),
            Err(InstructionValidationError::InvalidBoundary)
        );
    }

    #[test]
    fn a_worktree_does_not_make_profile_inputs_or_unknown_paths_private() {
        let owned = InstructionPathContext::OwnedWorktree {
            lease: WorktreeId::from_bytes([1; 16]),
        };
        let mut comparison = InstructionComparison::default();
        let mut local = change("/worktree/CLAUDE.md");
        local.context = owned;
        assert!(comparison.try_push_change(local));
        comparison.gaps.push(InstructionCoverageGap {
            reason: InstructionGapKind::ReadFailed,
            context: owned,
            agent: None,
            path: None,
        });
        assert!(!comparison.has_shared_changes());
        assert!(!comparison.has_shared_gaps());
        comparison.gaps.push(InstructionCoverageGap {
            reason: InstructionGapKind::ProfileUnknown,
            context: InstructionPathContext::SharedInput {
                source: InstructionSharedInput::Profile,
            },
            agent: None,
            path: None,
        });
        assert!(comparison.has_shared_gaps());
        assert!(InstructionPathContext::Unresolved.needs_shared_review());
    }

    #[test]
    fn review_actions_are_atomic_operator_records_with_existing_bounded_assignment_text() {
        let mut record = samples::record(crate::RecordKind::TicketInstructionReviewed);
        if let Record::TicketInstructionReviewed { action, .. } = &mut record {
            *action = InstructionReviewAction::ChangesRequested {
                agent: samples::agent(),
                message: Some("Review the shared file.".into()),
                clear_goal: None,
            };
        }
        assert_eq!(validate_instruction_record(&record), Ok(()));
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["t"], "ticket_instruction_reviewed");
        assert_eq!(
            json["action"],
            serde_json::json!({ "kind":"changes_requested", "agent":samples::agent(), "message":"Review the shared file.", "clear_goal":null })
        );
        let mut missing_clear = json.clone();
        missing_clear["action"]
            .as_object_mut()
            .unwrap()
            .remove("clear_goal");
        assert!(serde_json::from_value::<Record>(missing_clear).is_err());
        if let Record::TicketInstructionReviewed { action, .. } = &mut record {
            *action = InstructionReviewAction::ChangesRequested {
                agent: samples::agent(),
                message: Some("x".repeat(crate::ticket::MAX_MESSAGE_BYTES + 1)),
                clear_goal: None,
            };
        }
        assert_eq!(
            validate_instruction_record(&record),
            Err(InstructionValidationError::InvalidReview)
        );
        if let Record::TicketInstructionReviewed { by, action, .. } = &mut record {
            *by = Actor::Agent {
                agent: samples::agent(),
            };
            *action = InstructionReviewAction::Accept {
                completion: InstructionAcceptCompletion::Continue,
            };
        }
        assert_eq!(
            validate_instruction_record(&record),
            Err(InstructionValidationError::InvalidReview)
        );
    }

    #[test]
    fn floor_summary_watermarks_preserve_large_positions_as_decimal_strings_only() {
        let watermark = InstructionReviewWatermark {
            through_lsn: Lsn(9_007_199_254_740_993),
            set_digest: Hex32::new([7; 32]),
        };
        let summary = InstructionReviewSummary {
            watermark,
            pending_comparison: false,
            intervals: 1,
            changed_paths: 1,
            gaps: 0,
        };
        let value = serde_json::to_value(summary).unwrap();
        assert_eq!(value["watermark"]["through_lsn"], "9007199254740993");
        assert_eq!(
            serde_json::from_value::<InstructionReviewSummary>(value.clone()).unwrap(),
            summary
        );
        assert_eq!(
            serde_json::to_value(watermark).unwrap()["through_lsn"],
            9_007_199_254_740_993_u64
        );
        for invalid in [
            serde_json::json!(9_007_199_254_740_993_u64),
            serde_json::json!("09007199254740993"),
            serde_json::json!("18446744073709551616"),
        ] {
            let mut invalid_summary = value.clone();
            invalid_summary["watermark"]["through_lsn"] = invalid;
            assert!(serde_json::from_value::<InstructionReviewSummary>(invalid_summary).is_err());
        }
    }

    #[test]
    fn ordinary_rpc_requests_keep_their_json_and_accept_results_distinguish_continuing_work() {
        let accept = serde_json::json!({ "card": samples::card(), "expected_seq": 2 });
        let params: TicketAcceptParams = serde_json::from_value(accept.clone()).unwrap();
        assert_eq!(params.review_watermark, None);
        assert_eq!(serde_json::to_value(params).unwrap(), accept);
        let assign = serde_json::json!({ "card": samples::card(), "agent": samples::agent(), "expected_seq": 2 });
        let params: TicketAssignParams = serde_json::from_value(assign.clone()).unwrap();
        assert_eq!(params.review_watermark, None);
        assert_eq!(serde_json::to_value(params).unwrap(), assign);
        assert_eq!(
            serde_json::to_value(TicketAcceptResult::default()).unwrap(),
            serde_json::json!({})
        );
        assert_eq!(
            serde_json::from_str::<TicketAcceptResult>("{}").unwrap(),
            TicketAcceptResult::default()
        );
        assert_eq!(
            serde_json::to_value(TicketAcceptResult {
                outcome: Some(TicketAcceptOutcome::InstructionsAcknowledged)
            })
            .unwrap(),
            serde_json::json!({"outcome":"instructions_acknowledged"})
        );
    }

    #[test]
    fn optional_review_watermark_names_the_exact_set_with_strict_digest_encoding() {
        let watermark = InstructionReviewWatermark {
            through_lsn: Lsn(12),
            set_digest: Hex32::new([0xab; 32]),
        };
        let params = TicketAcceptParams {
            card: samples::card(),
            expected_seq: Seq(2),
            review_watermark: Some(watermark),
        };
        let json = serde_json::to_value(params).unwrap();
        assert_eq!(
            json["review_watermark"],
            serde_json::json!({"through_lsn":12,"set_digest":"abababababababababababababababababababababababababababababababab"})
        );
        assert_eq!(
            serde_json::from_value::<TicketAcceptParams>(json.clone())
                .unwrap()
                .review_watermark,
            Some(watermark)
        );
        let mut invalid = json;
        invalid["review_watermark"]["set_digest"] = serde_json::json!("AB".repeat(32));
        assert!(serde_json::from_value::<TicketAcceptParams>(invalid).is_err());
    }

    #[test]
    fn resolved_targets_are_bounded_and_sources_name_the_exact_rule_root() {
        let mut sample = snapshot();
        sample.inputs[0].resolved_path = Some(path("/target/CLAUDE.md"));
        assert_eq!(
            sample.validate(),
            Err(InstructionValidationError::InvalidFingerprint)
        );
        sample.inputs[0].fingerprint = InstructionFingerprint::File {
            digest: Hex32::new([1; 32]),
            bytes: 1,
        };
        assert!(sample.validate().is_ok());
        sample.inputs[0].resolved_path = Some(path(&"x".repeat(MAX_INSTRUCTION_PATH_BYTES)));
        assert_eq!(
            sample.validate(),
            Err(InstructionValidationError::PathTooLarge)
        );
        sample.inputs[0].resolved_path = None;
        sample.participants[0]
            .discovery
            .roots
            .push(InstructionRoot {
                path: path("/elsewhere"),
                kind: InstructionRootKind::Profile,
                context: InstructionPathContext::SharedInput {
                    source: InstructionSharedInput::Profile,
                },
            });
        sample.inputs[0].source.root = path("/elsewhere");
        assert_eq!(
            sample.validate(),
            Err(InstructionValidationError::InvalidDiscovery)
        );
    }

    #[test]
    fn review_reason_sentences_distinguish_observed_changes_from_uncertainty() {
        assert_eq!(
            crate::TicketReason::SharedInstructionsChanged.sentence(),
            "Shared project instructions changed during this work. Review the change, then Accept or Ask for changes."
        );
        assert_eq!(
            crate::TicketReason::SharedInstructionsUnknown.sentence(),
            "Dudes could not determine whether shared project instructions changed during this work. Review the work, then Accept or Ask for changes."
        );
    }
}
