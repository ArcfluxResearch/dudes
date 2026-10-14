//! The `Adapter` trait: what the supervisor asks of a provider in managed mode.
//!
//! An adapter is pure. It turns what the daemon knows about an agent ([`AgentCtx`]) and a task
//! into a [`LaunchSpec`], reads the child's stream through a [`TurnReader`], and says where usage
//! comes from. It reads no environment, no clock and no file: two calls with equal inputs give
//! equal bytes, which is what makes a launch replayable and a golden argv fixture possible.
//!
//! Managed mode is required. Attached mode is a capability: the default methods return `None`,
//! keeping a managed-only adapter explicit and preventing the host from inventing vendor argv.
//! Hook translation and reply encoding remain provider-specific interfaces because their errors
//! and payloads differ; the Claude adapter exports its implementations from `claude`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use dudes_types::{AgentId, IsolationMode, Mode, ModelSelection, Provider, SessionId, Tier, Usd};

use crate::instructions::InstructionDeclaration;
use crate::launch::{EnvVar, LaunchError, LaunchSpec};
use crate::mcp::{McpLaunch, McpSupport};
use crate::stream::TurnReader;

/// What the daemon knows about an agent when it builds a launch: the `AgentHired` record plus
/// the daemon's own environment. Nothing here is read by the adapter from the process.
///
/// `daemon_env` is the environment **before** the scrub, so an operator's `ANTHROPIC_API_KEY`
/// is in it whenever the daemon has one. That is why the pairs are [`EnvVar`]s: this struct
/// derives `Debug`, and the derive is only safe because every value redacts itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCtx<'a> {
    /// The agent the turn belongs to.
    pub agent: AgentId,
    /// The repository the agent works in (`AgentHired.cwd`); the child's working directory.
    pub cwd: &'a Path,
    /// The daemon-owned directory of this agent (`<data>/agents/<AgentId>`), where the
    /// adapter's files for this agent live.
    pub agent_dir: &'a Path,
    /// The daemon's environment, which the adapter scrubs into the child's.
    pub daemon_env: &'a [EnvVar],
    /// Whether the provider's sandbox on-switch is wanted (`AgentHired.sandbox`); `None` means
    /// the provider's configured default.
    pub sandbox: Option<bool>,
    /// Whether the operator vouched for this repository at hire. Recorded once and replayed;
    /// adapter configuration never reinterprets it on a later turn.
    pub trusted: bool,
    /// Cap on the vendor's agentic turns within one Dudes turn, when the hire set one.
    pub max_turns: Option<u32>,
    /// The per-turn figure the vendor checks its own locally computed cost estimate against, when
    /// the hire set one.
    ///
    /// **Not a spend ceiling**, and the adapter that emits it says why at the argv site — see the
    /// comment above the `--max-budget-usd` emission in `claude::managed`. Dudes' spend ceilings
    /// are gate 5's, and they are what invariant 4 governs.
    pub max_budget: Option<Usd>,
    /// The declared MCP servers this launch loads: the agent's recorded set, admitted again by
    /// [`LoadedServers::admit`](crate::mcp::LoadedServers::admit) for this launch. Empty for an agent
    /// with none and for every attached launch.
    pub mcp_servers: &'a [McpLaunch],
}

/// Which vendor session a turn announces.
///
/// Not an `Option`: where Dudes mints the id itself it must be able to say "announce this id"
/// apart from "resume it", a pure adapter cannot mint one, and an `Option` collapses the two.
/// Which variant a turn gets is the supervisor's, following the session-resume logic — it is not the
/// adapter's, and not a count of how many turns have gone before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnSession {
    /// An id the daemon minted and announces to the vendor, because the agent has no
    /// vendor-confirmed session — whether this is its first turn or none was ever confirmed.
    /// Only a [`SessionAcquisition::Pinned`] provider can render one.
    First(SessionId),
    /// No id at all, because the agent has no vendor-confirmed session and its provider names
    /// its own id in the turn's output. Only a [`SessionAcquisition::Assigned`]
    /// provider can render one.
    Assigned,
    /// The id the vendor confirmed, resumed.
    Resume(SessionId),
}

impl TurnSession {
    /// The session id the turn names, or `None` when it names none.
    #[must_use]
    pub fn id(&self) -> Option<&SessionId> {
        match self {
            Self::First(id) | Self::Resume(id) => Some(id),
            Self::Assigned => None,
        }
    }
}

/// How a provider's session id comes into existence: declared once by its adapter,
/// never inferred from the argv the adapter built.
///
/// The other two modes, an id reachable only out of band and no route to an id at all,
/// arrive with the adapters that declare them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionAcquisition {
    /// The vendor accepts a caller-minted id when it creates the session.
    Pinned {
        /// The argv word a minted id follows, such as `--session-id`.
        pin: &'static str,
        /// The argv word a vendor-confirmed id follows, such as `--resume`.
        resume: &'static str,
    },
    /// The vendor names its own id in the turn's output, so a turn with nothing confirmed names
    /// no session.
    Assigned {
        /// The argv word a vendor-confirmed id follows.
        resume: &'static str,
        /// Which end of the turn the vendor names its id at.
        arrival: Arrival,
    },
}

/// Which end of a turn an assigning vendor names its session id at. Until it does, a crash leaves
/// a child the daemon cannot resume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    /// The first line of the turn names it.
    Start,
    /// Only the turn's last line names it.
    End,
}

impl SessionAcquisition {
    /// Whether the vendor accepts a caller-minted id, so a turn with nothing to resume is handed
    /// one. Every other mode announces nothing.
    #[must_use]
    pub const fn pins(self) -> bool {
        match self {
            Self::Pinned { .. } => true,
            Self::Assigned { .. } => false,
        }
    }

    /// The argv parts that name `session` to a provider of this mode: a word and the id, or
    /// nothing for [`TurnSession::Assigned`].
    ///
    /// # Errors
    ///
    /// [`LaunchError::SessionModeMismatch`] when this mode cannot render `session`.
    pub fn argv(self, session: &TurnSession) -> Result<Vec<OsString>, LaunchError> {
        let (word, id) = match (self, session) {
            (Self::Pinned { pin, .. }, TurnSession::First(id)) => (pin, id),
            (
                Self::Pinned { resume, .. } | Self::Assigned { resume, .. },
                TurnSession::Resume(id),
            ) => (resume, id),
            (Self::Assigned { .. }, TurnSession::Assigned) => return Ok(Vec::new()),
            (Self::Pinned { .. }, TurnSession::Assigned)
            | (Self::Assigned { .. }, TurnSession::First(_)) => {
                return Err(LaunchError::SessionModeMismatch);
            }
        };
        Ok(vec![word.into(), id.as_str().into()])
    }
}

/// How a managed turn is kept from inheriting what it was not given: a
/// declared capability with a verified degree, not a tier property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isolation {
    /// Typed argv parts the adapter composes into every managed launch.
    Flags(&'static [&'static str]),
    /// A configuration directory owned by the daemon.
    ConfigDir,
    /// Nothing: the turn runs whatever the operator's own CLI would run. Logged at hire and
    /// shown on the floor.
    None,
}

/// The provider context composition used by one launch mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextMode {
    /// A composition without a Claude isolation selector.
    Other,
    /// The configured selector used by Claude's managed composer.
    ClaudeManaged(IsolationMode),
    /// Claude's resume composer, which adds hook settings without managed exclusions.
    ClaudeAttached,
}

impl ContextMode {
    /// Whether this composition inherits Claude context.
    #[must_use]
    pub const fn claude_inherit(self) -> bool {
        matches!(
            self,
            Self::ClaudeManaged(IsolationMode::Inherit) | Self::ClaudeAttached
        )
    }
}

/// Where a turn's usage comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsageSource {
    /// The vendor exports OpenTelemetry when these variables are set on the child. Held as
    /// [`EnvVar`]s rather than as raw pairs, so this enum's derived `Debug` prints the names and
    /// no value: an OTLP collector is addressed with `OTEL_EXPORTER_OTLP_HEADERS`,
    /// whose value carries its bearer token.
    ///
    /// What holds that is `a_formatted_usage_source_prints_env_names_but_no_env_value` in
    /// `tests/redaction.rs`, not the derive above: replacing the derive with a hand-written
    /// `Debug` is an ordinary readability edit, and neither source guard can see one: the local
    /// purity rule keys on a `Debug` format spec, while `cargo xtask env-pairs` keys on a raw
    /// representation, and this is neither.
    OtelEnv(Vec<EnvVar>),
    /// The `result` line of the headless stream.
    HeadlessResult,
    /// Counted by the daemon's proxy.
    Proxy,
    /// Not available for this provider.
    None,
}

/// Directories and files the host prepares before a managed turn. The supervisor and adapter
/// produce the projection without writing. Replay rebuilds it before exec.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManagedInstall {
    /// Absolute daemon-owned directories to create and restrict before exec.
    pub directories: Vec<PathBuf>,
    /// Absolute path and complete content, one entry per file.
    pub files: Vec<(PathBuf, String)>,
}

/// Files the host writes into the agent's directory before attached-mode hooks can run. The
/// adapter produces them and performs no writes. Equal inputs produce equal paths and bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookInstall {
    /// Absolute path and complete content, one entry per file.
    pub files: Vec<(PathBuf, String)>,
}

/// A provider in managed mode. Object-safe: the daemon holds adapters as `&dyn Adapter`.
pub trait Adapter: Send + Sync {
    /// Which CLI this is.
    fn provider(&self) -> Provider;
    /// The program a managed launch runs, as configured: an absolute path, or a bare name the
    /// daemon looks up on the `PATH` it gives the child. The daemon reports whether it finds it
    /// and never runs it to ask.
    fn program(&self) -> &OsStr;
    /// The attached-mode governance tier; it never changes managed behaviour.
    fn tier(&self) -> Tier;
    /// The isolation every managed launch from this adapter carries.
    fn isolation(&self) -> Isolation;
    /// The actual context composition, independent of the recorded hire selector.
    fn context_mode(&self, _mode: Mode) -> ContextMode {
        ContextMode::Other
    }
    /// Pure candidate discovery for the actual composition. Filesystem expansion and sampling
    /// belong to the daemon. An undeclared adapter supplies explicit unknown coverage.
    fn instruction_manifest(
        &self,
        _ctx: &AgentCtx<'_>,
        mode: Mode,
        _selection: &ModelSelection,
    ) -> InstructionDeclaration {
        InstructionDeclaration::missing(self.provider(), mode)
    }
    /// How this provider's session id comes into existence. Pure and constant.
    fn session_acquisition(&self) -> SessionAcquisition;
    /// The model this adapter would select for a managed turn right now, as its own raw spelling.
    /// Pure, and asked **once per turn** by the supervisor, before the `TurnIssued`
    /// that records it is fsynced.
    fn model_selection(&self) -> ModelSelection;
    /// The launch of one managed turn. Invariant 7: typed parts, the task text is one argv
    /// element or stdin, nothing is interpolated into a shell.
    ///
    /// `selection` is the model the **supervisor** holds for this turn — the value on its
    /// `TurnIssued`, which for a re-issue is the recorded one and not today's configuration. An
    /// implementation encodes what it is given and must not re-read its own configured model;
    /// doing so is how a replayed turn silently changes model.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchError`] when the task or recorded agent context cannot form a managed
    /// launch. The adapter refuses before returning any [`LaunchSpec`], so the supervisor can
    /// stop before `TurnIssued` is persisted (invariant 2).
    fn managed(
        &self,
        ctx: &AgentCtx<'_>,
        task: &str,
        session: &TurnSession,
        selection: &ModelSelection,
    ) -> Result<LaunchSpec, LaunchError>;
    /// A fresh reader for one managed turn's standard output.
    fn turn_reader(&self) -> Box<dyn TurnReader>;
    /// A reader for the exact prepared launch, including its immutable protocol task.
    fn turn_reader_for(&self, _spec: &LaunchSpec) -> Box<dyn TurnReader> {
        self.turn_reader()
    }
    /// The files a managed launch refers to, for the host to write under `ctx.agent_dir`. Takes
    /// the same `selection` as [`Adapter::managed`], and for the same reason: the file and the
    /// argv have to describe one model, and it is the turn's, not the configuration's.
    fn managed_install(&self, ctx: &AgentCtx<'_>, selection: &ModelSelection) -> ManagedInstall;
    /// The interactive launch for an attached session, or `None` when this provider cannot attach
    /// that session.
    ///
    /// Only [`TurnSession::Resume`] is vendor-confirmed and therefore attachable. The default
    /// keeps a provider managed-only. No task or message body is an input to this method
    /// (invariant 6), and the returned argv remains typed parts (invariant 7).
    fn attached(&self, _ctx: &AgentCtx<'_>, _session: &TurnSession) -> Option<LaunchSpec> {
        None
    }
    /// The provider-owned hook settings for the host to write under `ctx.agent_dir`.
    ///
    /// `hook_command` is an absolute UTF-8 path supplied by the host. The adapter serialises the
    /// path as data and does not look it up or check that an executable exists there. Providers
    /// without attached-mode hook governance, and a path that is not absolute UTF-8, return an
    /// empty projection.
    fn install_hooks(&self, _ctx: &AgentCtx<'_>, _hook_command: &Path) -> HookInstall {
        HookInstall::default()
    }
    /// One compile-time, content-free line the host may submit to an idle attached session, or
    /// `None` when attached mail waits for the next managed turn (invariant 6).
    fn wake_sentinel(&self) -> Option<&'static str> {
        None
    }
    /// Where usage comes from.
    fn usage_source(&self) -> UsageSource;
    /// The vendor's sandbox on-switch as argv parts, when it is an argv flag.
    fn sandbox_flag(&self) -> Option<&'static [&'static str]>;
    /// The environment variable names that, when one of them is present and **non-empty** in the
    /// daemon's environment, mean a turn on this provider can be billed.
    ///
    /// Pure: it reads no environment, no clock and no configuration, and answers with a
    /// compile-time list. The daemon asks it before it journals a hire or a goal, to decide whether
    /// a call that sets no ceiling in any scope may be admitted.
    ///
    /// **An empty slice is a legal answer** and means *no environment variable selects a billing
    /// credential for this provider*. Its consequence is that a hire on such a provider with no
    /// ceiling anywhere is admitted — so the empty answer is a claim made for that provider on
    /// purpose, never a value taken because nobody looked.
    ///
    /// **Required and default-free on purpose.** A provider that answers nothing does not compile.
    /// A default body would make silence the answer, and silence is the one answer this method
    /// exists to refuse.
    fn billing_env_vars(&self) -> &'static [&'static str];
    /// The arguments after the program that ask this engine's own CLI whether it is signed in, or
    /// `None` when no such command is safe to run. The daemon reads only the exit status.
    fn sign_in_check(&self) -> Option<&'static [&'static str]> {
        None
    }
    /// Which declared MCP servers a managed launch of this adapter, as configured, can load. The
    /// default loads none, so an adapter that composes no server says so by not answering.
    fn mcp_support(&self) -> McpSupport {
        McpSupport::None
    }
    /// The environment a managed turn of this provider starts with, from `daemon_env`, before the
    /// supervisor adds `DUDES_SOCK` and `DUDES_TOKEN`. The default passes every variable through,
    /// sorted by name.
    fn child_env(&self, daemon_env: &[EnvVar]) -> Vec<EnvVar> {
        let mut env = daemon_env.to_vec();
        env.sort();
        env
    }
}

/// Compile-time proof that the trait is object-safe.
const _: fn(&dyn Adapter) = |_| {};
