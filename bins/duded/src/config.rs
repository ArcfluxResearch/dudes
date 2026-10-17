//! Operator configuration loaded before the daemon opens persistent state or owns children.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use dudes_journal::MemoryCondenseConfig;
use dudes_proto::rpc::goal::ENROLLED_CHECK_ID;
use dudes_proto::{AgentBudget, GoalBudget, Predicate, Spend};
use dudes_providers::codex::config::CodexConfig;
use dudes_providers::pi::config::PiConfig;
use dudes_types::{IsolationMode, Provider, Tokens, Usd};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::floor_actions::{CatalogError, EnrolledCheck, Enrollment, Profile, valid_workspace_id};
use crate::settings::{FloorControl, RunOptions};

const RELOAD_FILE_LIMIT: u64 = 1024 * 1024;

/// The budget settings this loader owns.
///
/// **There is no shipped floor ceiling.** An absent `[budget] floor_usd_per_day` means the
/// operator set no floor-wide ceiling, which is what absence means everywhere else
/// — not a number this crate chose on their behalf. A reserve is not a
/// ceiling: each unit the operator leaves unset is [`Spend::DEFAULT_PER_TURN_FLOOR`]'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetConfig {
    /// The budget gate's floor-wide daily dollar ceiling, absent when the operator configured none.
    pub floor_usd_per_day: Option<Usd>,
    /// Price reserved for each journaled unobserved end.
    pub per_turn_floor: Spend,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            floor_usd_per_day: None,
            per_turn_floor: Spend::DEFAULT_PER_TURN_FLOOR,
        }
    }
}

/// Operator budget, browser, and provider settings from the startup file.
#[derive(Clone, Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent operator switches, each its own `[floor.*]` or `[memory]` key"
)]
pub struct OperatorConfig {
    /// Existing floor budget settings.
    pub budget: BudgetConfig,
    /// Optional finite browser enrollment.
    pub enrollment: Enrollment,
    /// `[floor.answer] enabled`: whether the Floor may answer a Waiting agent's open question.
    /// Absent or `false` registers no route and sends no `answer` sibling.
    pub answer_enabled: bool,
    /// `[floor.hire] enabled`: whether the Floor may hire. Absent or `false` sends no `onboarding`
    /// sibling and refuses every hire before any append.
    pub hire_enabled: bool,
    /// `[floor.control]`, `[floor.goal]`, `[floor.retire]` and `[floor.renew]` `enabled`: whether
    /// the Floor may pause, resume and rename one agent, and whether it may also
    /// replace one agent's goal text, retire one agent, or renew a goal that has nothing left.
    /// Absent or `false` registers no route and sends no sibling or member. `[floor.goal]` and
    /// `[floor.retire]` are refused at load without `[floor.control]`,
    /// and `[floor.renew]` without both `[floor.goal]` and `[floor.hire]`.
    pub control: FloorControl,
    /// `[floor.recovery] enabled`: recovery also requires the running pause/resume grant.
    pub recovery_enabled: bool,
    /// `[floor.board] enabled`: whether the Floor may create, give, accept, stop and remove
    /// tickets. Absent or `false` registers no route and sends no `ticket` sibling. It is refused
    /// at load without both `[floor.control]` and `[floor.goal]`.
    pub board_enabled: bool,
    /// `[floor.terminal] enabled`.
    pub terminal_enabled: bool,
    /// `[floor.git] enabled`: whether the Floor may read an agent's checkout for its Git tab.
    /// Absent or `false` registers no route and sends no `git` sibling.
    pub git_enabled: bool,
    /// `[floor.diff] enabled`: whether the Floor may read a ticket's bounded checkout diff. Absent or
    /// `false` registers no route. The route also needs `[floor.git]` on; with either off it
    /// does not exist.
    pub diff_enabled: bool,
    /// The `[providers.<name>]` tables the operator wrote.
    pub providers: ProviderTables,
    /// `[memory] condense_model_enabled`: whether a condense may spend a one-shot model call for
    /// its summary before falling back to deterministic exact excerpts. Defaults to `true`.
    pub condense_model_enabled: bool,
    /// `[memory] condense_model`: an explicit model name to ask for instead of the cheapest model
    /// this build knows for the agent's own provider. Absent means that built-in default.
    pub condense_model: Option<String>,
    /// `[mcp.servers]`: the MCP servers an agent may be opted in to at hire. Read at start only;
    /// a reload names a difference as needing a restart.
    pub mcp_servers: dudes_providers::mcp::DeclaredServers,
}

/// One operator file read with its additive startup options.
#[derive(Clone, Debug, Default)]
pub struct OperatorStartup {
    /// Settings used by existing daemon callers.
    pub operator: OperatorConfig,
    /// Options whose addition preserves existing configuration literals.
    pub options: RunOptions,
}

/// The `[providers.<name>]` tables the operator wrote. An absent table means no adapter for that
/// provider, so a hire on it is refused.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderTables {
    /// `[providers.codex]`.
    pub codex: Option<CodexConfig>,
    /// `[providers.pi]`.
    pub pi: Option<PiConfig>,
}

impl ProviderTables {
    /// Whether this daemon reads a `[providers.<name>]` table for `provider`, so writing one turns
    /// its adapter on.
    #[must_use]
    pub const fn reads(provider: Provider) -> bool {
        matches!(provider, Provider::Codex | Provider::Pi)
    }
}

/// A refused operator configuration.
///
/// The error deliberately retains no input text or parser diagnostic: startup prints its error
/// chain, and a TOML diagnostic can contain values from unrelated operator configuration. The only
/// names it may carry are a workspace id and a `[providers]` table's name, and only in the
/// restricted ASCII.
#[derive(Debug)]
pub struct ConfigError {
    path: PathBuf,
    kind: ConfigErrorKind,
}

#[derive(Clone, Debug)]
enum ConfigErrorKind {
    Read(io::ErrorKind),
    NonRegularFile,
    ReloadFileTooLarge,
    InvalidDocument,
    InvalidMemory,
    InvalidFloorUsdPerDay,
    InvalidPerTurnFloor(&'static str),
    InvalidOnboarding,
    InvalidWorkspace(String),
    NonCanonicalWorkspace(String),
    OnboardingTooLarge,
    ChecksTooLarge,
    InvalidGoal,
    InvalidRetire,
    InvalidRenew,
    InvalidBoard,
    InvalidProvider(&'static str),
    UnreadProvider(Option<String>),
    InvalidMcp(dudes_providers::mcp::DeclarationError),
}

impl ConfigError {
    fn new(path: &Path, kind: ConfigErrorKind) -> Self {
        Self {
            path: path.to_path_buf(),
            kind,
        }
    }

    /// A refused `[floor.onboarding]` catalog, naming the workspace when one caused it.
    pub(crate) fn catalog(path: &Path, error: CatalogError) -> Self {
        Self::new(
            path,
            match error {
                CatalogError::TooMany | CatalogError::InvalidId => {
                    ConfigErrorKind::InvalidOnboarding
                }
                CatalogError::Workspace(id) => ConfigErrorKind::InvalidWorkspace(id),
                CatalogError::TooLarge => ConfigErrorKind::OnboardingTooLarge,
                CatalogError::ChecksTooLarge => ConfigErrorKind::ChecksTooLarge,
            },
        )
    }

    /// A reload would add or change `id` with a `cwd` whose canonical form differs from the file.
    pub(crate) fn non_canonical(path: &Path, id: &str) -> Self {
        Self::new(path, ConfigErrorKind::NonCanonicalWorkspace(id.to_owned()))
    }

    fn workspace(path: &Path, id: &str) -> Self {
        Self::new(
            path,
            if valid_workspace_id(id) {
                ConfigErrorKind::InvalidWorkspace(id.to_owned())
            } else {
                ConfigErrorKind::InvalidOnboarding
            },
        )
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "configuration at {} ", self.path.display())?;
        match &self.kind {
            ConfigErrorKind::Read(kind) => write!(formatter, "could not be read ({kind:?})"),
            ConfigErrorKind::NonRegularFile => write!(formatter, "is not a regular file"),
            ConfigErrorKind::ReloadFileTooLarge => {
                write!(formatter, "exceeds the 1 MiB reload cap")
            }
            ConfigErrorKind::InvalidDocument => write!(formatter, "is not a valid TOML document"),
            ConfigErrorKind::InvalidMemory => write!(formatter, "has invalid [memory] settings"),
            ConfigErrorKind::InvalidFloorUsdPerDay => {
                write!(formatter, "has an invalid [budget] floor_usd_per_day")
            }
            ConfigErrorKind::InvalidPerTurnFloor(key) => {
                write!(formatter, "has an invalid [budget.per_turn_floor] {key}")
            }
            ConfigErrorKind::InvalidOnboarding => {
                write!(formatter, "has an invalid [floor.onboarding] enrollment")
            }
            ConfigErrorKind::InvalidWorkspace(id) => {
                write!(
                    formatter,
                    "has an invalid [floor.onboarding] workspace `{id}`"
                )
            }
            ConfigErrorKind::NonCanonicalWorkspace(id) => write!(
                formatter,
                "has a [floor.onboarding] workspace `{id}` whose cwd is not its canonical form"
            ),
            ConfigErrorKind::OnboardingTooLarge => write!(
                formatter,
                "has a [floor.onboarding] catalog over the 16 KiB cap once marked"
            ),
            ConfigErrorKind::ChecksTooLarge => write!(
                formatter,
                "has [floor.onboarding] completion checks whose list for the Floor is over its \
                 {} byte cap",
                dudes_proto::rpc::floor::MAX_CHECKS_BYTES
            ),
            ConfigErrorKind::InvalidGoal => write!(
                formatter,
                "enables [floor.goal] without [floor.control] enabled"
            ),
            ConfigErrorKind::InvalidRetire => write!(
                formatter,
                "enables [floor.retire] without [floor.control] enabled"
            ),
            ConfigErrorKind::InvalidRenew => write!(
                formatter,
                "enables [floor.renew] without both [floor.goal] and [floor.hire] enabled"
            ),
            ConfigErrorKind::InvalidBoard => write!(
                formatter,
                "enables [floor.board] without both [floor.control] and [floor.goal] enabled"
            ),
            ConfigErrorKind::InvalidProvider(name) => write!(
                formatter,
                "has an invalid [providers.{name}] table: an unknown key, or a command that is not a bare name or absolute path"
            ),
            ConfigErrorKind::UnreadProvider(Some(name)) => write!(
                formatter,
                "has a [providers.{name}] table, and this daemon reads only [providers.codex] and \
                 [providers.pi]"
            ),
            ConfigErrorKind::InvalidMcp(error) => write!(formatter, "is refused: {error}"),
            ConfigErrorKind::UnreadProvider(None) => write!(
                formatter,
                "has a [providers] table other than [providers.codex] and [providers.pi], the only \
                 ones this daemon reads"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Document {
    budget: Option<BudgetTable>,
    floor: Option<FloorTable>,
    providers: Option<toml::Table>,
    memory: Option<MemoryTable>,
    mcp: Option<McpTable>,
}

/// `[mcp]`: only `[mcp.servers.<name>]` tables. Each server is judged on its own, so a refusal
/// names it.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct McpTable {
    servers: std::collections::BTreeMap<String, toml::Value>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_field_names)] // the exact `[memory]` TOML key names, verbatim
struct MemoryTable {
    condense_after_kib: usize,
    condense_keep_sections: usize,
    /// Whether a condense may spend a one-shot model call for its summary before falling back to
    /// deterministic exact excerpts. On by default: the call is reserved and
    /// admitted exactly like an unobserved turn end (invariant 4), an unset ceiling is no cap,
    /// and any failure, timeout or invalid output falls back for free, so there is no
    /// user who is better served by the weaker deterministic-only recall as a silent default.
    condense_model_enabled: bool,
    /// Override the model a condense call asks for, instead of the cheapest model this build
    /// knows for the agent's own provider. Ignored for a provider this build has no model call
    /// for, which always falls back to deterministic exact excerpts.
    condense_model: Option<String>,
}

impl Default for MemoryTable {
    fn default() -> Self {
        let defaults = MemoryCondenseConfig::default();
        Self {
            condense_after_kib: defaults.threshold_bytes() / 1_024,
            condense_keep_sections: defaults.keep_sections(),
            condense_model_enabled: true,
            condense_model: None,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FloorTable {
    onboarding: Option<OnboardingTable>,
    answer: Option<AnswerTable>,
    hire: Option<HireTable>,
    control: Option<ControlTable>,
    recovery: Option<RecoveryTable>,
    goal: Option<GoalTable>,
    retire: Option<RetireTable>,
    renew: Option<RenewTable>,
    board: Option<BoardTable>,
    terminal: Option<TerminalTable>,
    git: Option<GitTable>,
    diff: Option<DiffTable>,
    chat: Option<ChatTable>,
}

/// `[floor.git]`: the operator's opt-in to the Floor's Git tab, read once at start.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct GitTable {
    enabled: bool,
}

/// `[floor.diff]`: the operator's opt-in to the Floor's ticket diff route, read once at start.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DiffTable {
    enabled: bool,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ChatTable {
    enabled: bool,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TerminalTable {
    enabled: bool,
}

/// `[floor.hire]`: the operator's opt-in to the Floor's hire, read once at start.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HireTable {
    enabled: bool,
}

/// `[floor.control]`: the operator's opt-in to the Floor's pause, resume and rename, read once at
/// start.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ControlTable {
    enabled: bool,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RecoveryTable {
    enabled: bool,
}

/// `[floor.goal]`: the operator's opt-in to the Floor's goal change, read once at start.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct GoalTable {
    enabled: bool,
}

/// `[floor.renew]`: the operator's opt-in to the Floor's renewal of a goal that has nothing left,
/// read once at start.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RenewTable {
    enabled: bool,
}

/// `[floor.board]`: the operator's opt-in to the Floor's tickets, read once at start.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BoardTable {
    enabled: bool,
}

/// `[floor.retire]`: the operator's opt-in to the Floor's retirement of one agent, read once at
/// start.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RetireTable {
    enabled: bool,
}

/// `[floor.answer]`: the operator's opt-in to the Floor's answer action, read once at start.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AnswerTable {
    enabled: bool,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OnboardingTable {
    workspaces: Vec<WorkspaceTable>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceTable {
    workspace_id: String,
    label: String,
    completion_check_label: String,
    #[serde(default)]
    completion_check_id: Option<String>,
    cwd: PathBuf,
    department: String,
    isolation: IsolationMode,
    agent_usd_per_day: Option<toml::Spanned<toml::Value>>,
    goal_usd: Option<toml::Spanned<toml::Value>>,
    hop_cap: u32,
    predicate_argv: Vec<String>,
    predicate_timeout_s: u64,
    #[serde(default)]
    may_already_pass: bool,
    tokens_per_day: Option<u64>,
    goal_tokens: Option<u64>,
    #[serde(default)]
    checks: Vec<CheckTable>,
}

/// One more enrolled check: a row of `[[floor.onboarding.workspaces.checks]]`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckTable {
    id: String,
    label: String,
    argv: Vec<String>,
    timeout_s: u64,
    #[serde(default)]
    may_already_pass: bool,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BudgetTable {
    floor_usd_per_day: Option<toml::Spanned<toml::Value>>,
    per_turn_floor: Option<PerTurnFloorTable>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PerTurnFloorTable {
    usd: Option<toml::Spanned<toml::Value>>,
    tokens: Option<toml::Spanned<toml::Value>>,
}

/// Why no operator file path could be chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatorConfigPathError {
    /// `DUDES_CONFIG` is set to the empty string.
    EmptyOverride,
    /// `DUDES_CONFIG` is set to a path that is not absolute.
    RelativeOverride,
    /// There is no override and the platform default has no absolute root.
    NoRoot,
}

impl fmt::Display for OperatorConfigPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyOverride => {
                "DUDES_CONFIG is empty; set an absolute path to the operator file or unset it"
            }
            Self::RelativeOverride => "DUDES_CONFIG must be an absolute path to the operator file",
            Self::NoRoot => "operator configuration root is unavailable",
        })
    }
}

impl std::error::Error for OperatorConfigPathError {}

/// The platform families whose default operator file locations differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigPlatform {
    /// `~/Library/Application Support/dudes/config.toml`.
    MacOs,
    /// `%APPDATA%\dudes\config.toml`.
    Windows,
    /// `$XDG_CONFIG_HOME/dudes/config.toml`, else `~/.config/dudes/config.toml`.
    Unix,
}

impl ConfigPlatform {
    /// The platform this binary was built for.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// The operator file's path: `DUDES_CONFIG` when set, else the platform default,
/// `~/Library/Application Support/dudes/config.toml` on macOS,
/// `$XDG_CONFIG_HOME/dudes/config.toml` (else `~/.config/dudes/config.toml`) on other Unix
/// systems, `%APPDATA%\dudes\config.toml` on Windows.
///
/// `DUDES_CONFIG` names the file itself and wins on every platform. A set but empty or relative
/// value is refused rather than ignored, because falling back would read a file the operator
/// meant to avoid. The XDG base-directory convention ignores an empty or relative
/// `XDG_CONFIG_HOME`, and the fallback still requires an absolute `HOME`: cwd is never an operator
/// configuration authority.
///
/// # Errors
///
/// Returns [`OperatorConfigPathError`] for a refused override, or when the platform default has no
/// absolute root.
pub fn operator_config_path(
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, OperatorConfigPathError> {
    operator_config_path_for(ConfigPlatform::current(), lookup)
}

/// [`operator_config_path`] for a named platform, so each rule is testable on any host.
///
/// # Errors
///
/// As [`operator_config_path`].
pub fn operator_config_path_for(
    platform: ConfigPlatform,
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, OperatorConfigPathError> {
    if let Some(configured) = lookup("DUDES_CONFIG") {
        if configured.is_empty() {
            return Err(OperatorConfigPathError::EmptyOverride);
        }
        let path = PathBuf::from(configured);
        return if path.is_absolute() {
            Ok(path)
        } else {
            Err(OperatorConfigPathError::RelativeOverride)
        };
    }
    let absolute = |name: &str| {
        lookup(name)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    match platform {
        ConfigPlatform::MacOs => {
            absolute("HOME").map(|home| home.join("Library/Application Support/dudes/config.toml"))
        }
        ConfigPlatform::Windows => absolute("APPDATA").map(|root| root.join("dudes/config.toml")),
        ConfigPlatform::Unix => absolute("XDG_CONFIG_HOME")
            .or_else(|| absolute("HOME").map(|home| home.join(".config")))
            .map(|root| root.join("dudes/config.toml")),
    }
    .ok_or(OperatorConfigPathError::NoRoot)
}

/// Load the floor budget from one complete TOML document.
///
/// A missing file, table or key sets **no floor ceiling**, and that is an answer rather than a
/// fallback: the operator did not configure one. Any unreadable or malformed present file, unknown
/// key inside `[budget]`, or unsupported present value is refused, exactly as before — what a
/// *present* value means has not changed, including `0`, which is a ceiling of nothing and refuses
/// the first turn.
///
/// # Errors
///
/// Returns a path-bearing, value-free error for every refused present configuration.
pub fn load_budget(path: &Path) -> Result<BudgetConfig, ConfigError> {
    load_operator(path).map(|config| config.budget)
}

/// Parse operator budget and optional browser enrollment from one file read.
///
/// # Errors
///
/// Returns a path-bearing, value-free error for every refused present configuration.
pub fn load_operator(path: &Path) -> Result<OperatorConfig, ConfigError> {
    load_operator_and_memory(path).map(|(operator, _)| operator)
}

/// Load operator settings and memory maintenance from one coherent file snapshot.
///
/// # Errors
/// The same value-free configuration refusal as [`load_operator`].
pub fn load_operator_and_memory(
    path: &Path,
) -> Result<(OperatorConfig, MemoryCondenseConfig), ConfigError> {
    read_operator(path).and_then(|bytes| parse_operator_and_memory(path, bytes.as_deref()))
}

/// Parse existing settings and startup options from one file read.
///
/// # Errors
///
/// Returns a path-bearing, value-free error for refused operator configuration.
pub fn load_operator_with_options(path: &Path) -> Result<OperatorStartup, ConfigError> {
    read_operator(path).and_then(|bytes| parse_operator_with_options(path, bytes.as_deref()))
}

/// Load existing settings, memory maintenance and startup options from one file read.
///
/// # Errors
///
/// Returns the same value-free configuration refusal as [`load_operator`].
pub fn load_operator_and_memory_with_options(
    path: &Path,
) -> Result<(OperatorStartup, MemoryCondenseConfig), ConfigError> {
    read_operator(path)
        .and_then(|bytes| parse_operator_and_memory_with_options(path, bytes.as_deref()))
}

/// The operator file's bytes, or `None` when it does not exist.
pub(crate) fn read_operator(path: &Path) -> Result<Option<Vec<u8>>, ConfigError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ConfigError::new(path, ConfigErrorKind::Read(error.kind()))),
    }
}

pub(crate) fn read_operator_for_reload(path: &Path) -> Result<Option<Vec<u8>>, ConfigError> {
    let mut file = match open_reload_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ConfigError::new(path, ConfigErrorKind::Read(error.kind()))),
    };
    let metadata = file
        .metadata()
        .map_err(|error| ConfigError::new(path, ConfigErrorKind::Read(error.kind())))?;
    if !metadata.is_file() {
        return Err(ConfigError::new(path, ConfigErrorKind::NonRegularFile));
    }
    if metadata.len() > RELOAD_FILE_LIMIT {
        return Err(ConfigError::new(path, ConfigErrorKind::ReloadFileTooLarge));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(RELOAD_FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| ConfigError::new(path, ConfigErrorKind::Read(error.kind())))?;
    if bytes.len() as u64 > RELOAD_FILE_LIMIT {
        return Err(ConfigError::new(path, ConfigErrorKind::ReloadFileTooLarge));
    }
    Ok(Some(bytes))
}

#[cfg(unix)]
// A FIFO can block in `open` before handle metadata is available.
fn open_reload_file(path: &Path) -> io::Result<fs::File> {
    use rustix::fs::{CWD, Mode, OFlags, openat};

    openat(
        CWD,
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(fs::File::from)
    .map_err(io::Error::from)
}

#[cfg(not(unix))]
fn open_reload_file(path: &Path) -> io::Result<fs::File> {
    fs::File::open(path)
}

fn parse_operator_and_memory(
    path: &Path,
    bytes: Option<&[u8]>,
) -> Result<(OperatorConfig, MemoryCondenseConfig), ConfigError> {
    parse_operator_and_memory_with_options(path, bytes)
        .map(|(startup, memory)| (startup.operator, memory))
}

/// The refusal for a switch whose companions are not on, if any. A renewal funds turns from
/// enrollment, which only an operator who let the Floor hire has already granted the browser. A
/// ticket given to an agent starts paid work, which a goal change alone never does, so the operator
/// who allows it has already let the Floor control agents and set goals.
const fn unmet_companion(
    (control, goal, hire): (bool, bool, bool),
    (renew, board): (bool, bool),
) -> Option<ConfigErrorKind> {
    if renew && !(goal && hire) {
        Some(ConfigErrorKind::InvalidRenew)
    } else if board && !(control && goal) {
        Some(ConfigErrorKind::InvalidBoard)
    } else {
        None
    }
}

pub(crate) fn parse_operator_with_options(
    path: &Path,
    bytes: Option<&[u8]>,
) -> Result<OperatorStartup, ConfigError> {
    parse_operator_and_memory_with_options(path, bytes).map(|(startup, _)| startup)
}

#[expect(
    clippy::too_many_lines,
    reason = "one pass reads every operator table and applies the start rules in the order they are reported"
)]
pub(crate) fn parse_operator_and_memory_with_options(
    path: &Path,
    bytes: Option<&[u8]>,
) -> Result<(OperatorStartup, MemoryCondenseConfig), ConfigError> {
    let Some(bytes) = bytes else {
        return Ok((OperatorStartup::default(), MemoryCondenseConfig::default()));
    };
    let source = std::str::from_utf8(bytes)
        .map_err(|_| ConfigError::new(path, ConfigErrorKind::Read(io::ErrorKind::InvalidData)))?;
    let document: Document = toml::from_str(source)
        .map_err(|_| ConfigError::new(path, ConfigErrorKind::InvalidDocument))?;
    let memory_table = document.memory.unwrap_or_default();
    let condense_model_enabled = memory_table.condense_model_enabled;
    let condense_model = memory_table.condense_model.clone();
    let memory = MemoryCondenseConfig::new(
        memory_table.condense_after_kib,
        memory_table.condense_keep_sections,
    )
    .map_err(|_| ConfigError::new(path, ConfigErrorKind::InvalidMemory))?;
    let providers = parse_providers(path, document.providers.unwrap_or_default())?;
    let mcp_servers = parse_mcp(path, document.mcp.unwrap_or_default())?;
    let budget = document.budget.unwrap_or_default();
    let floor = document.floor.unwrap_or_default();
    let mut options = RunOptions::default()
        .with_providers(providers.clone())
        .with_mcp_servers(mcp_servers.clone());
    if floor.chat.is_some() {
        options = options.with_ignored_chat_key();
    }
    let answer_enabled = floor.answer.unwrap_or_default().enabled;
    let hire_enabled = floor.hire.unwrap_or_default().enabled;
    let control_enabled = floor.control.unwrap_or_default().enabled;
    let recovery_enabled = floor.recovery.unwrap_or_default().enabled;
    let goal_enabled = floor.goal.unwrap_or_default().enabled;
    let retire_enabled = floor.retire.unwrap_or_default().enabled;
    let renew_enabled = floor.renew.unwrap_or_default().enabled;
    let board_enabled = floor.board.unwrap_or_default().enabled;
    let terminal_enabled = floor.terminal.unwrap_or_default().enabled;
    let git_enabled = floor.git.unwrap_or_default().enabled;
    let diff_enabled = floor.diff.unwrap_or_default().enabled;
    let floor_usd_per_day = budget
        .floor_usd_per_day
        .map(|value| {
            let raw = source
                .get(value.span())
                .ok_or_else(|| ConfigError::new(path, ConfigErrorKind::InvalidFloorUsdPerDay))?;
            decimal_micro_dollars(raw, value.get_ref())
                .ok_or_else(|| ConfigError::new(path, ConfigErrorKind::InvalidFloorUsdPerDay))
        })
        .transpose()?;
    let per_turn_floor = budget
        .per_turn_floor
        .map(|floor| parse_per_turn_floor(path, source, floor))
        .transpose()?
        .unwrap_or(Spend::DEFAULT_PER_TURN_FLOOR);
    // A change waits for a window between turns, and Pause is what opens one on a busy agent.
    let control = match (control_enabled, goal_enabled, retire_enabled) {
        (false, false, false) => FloorControl::Off,
        (false, true, _) => return Err(ConfigError::new(path, ConfigErrorKind::InvalidGoal)),
        (false, false, true) => {
            return Err(ConfigError::new(path, ConfigErrorKind::InvalidRetire));
        }
        (true, goal, retire) => FloorControl::On {
            goal,
            retire,
            renew: renew_enabled,
        },
    };
    if let Some(kind) = unmet_companion(
        (control_enabled, goal_enabled, hire_enabled),
        (renew_enabled, board_enabled),
    ) {
        return Err(ConfigError::new(path, kind));
    }
    let mut enrollment = Enrollment {
        profiles: floor
            .onboarding
            .unwrap_or_default()
            .workspaces
            .into_iter()
            .map(|item| parse_workspace(path, source, item))
            .collect::<Result<Vec<_>, _>>()?,
    };
    enrollment
        .validate(floor_usd_per_day)
        .map_err(|error| ConfigError::catalog(path, error))?;
    Ok((
        OperatorStartup {
            options,
            operator: OperatorConfig {
                budget: BudgetConfig {
                    floor_usd_per_day,
                    per_turn_floor,
                },
                enrollment,
                answer_enabled,
                hire_enabled,
                control,
                recovery_enabled,
                board_enabled,
                terminal_enabled,
                git_enabled,
                diff_enabled,
                providers,
                condense_model_enabled,
                condense_model,
                mcp_servers,
            },
        },
        memory,
    ))
}

/// `[mcp.servers.<name>]`, each judged by the rules in [`dudes_providers::mcp::declare`]. The
/// parse reads no environment: a variable the daemon lacks refuses only the hires and launches
/// that need it.
fn parse_mcp(
    path: &Path,
    table: McpTable,
) -> Result<dudes_providers::mcp::DeclaredServers, ConfigError> {
    dudes_providers::mcp::parse_servers(table.servers)
        .map_err(|error| ConfigError::new(path, ConfigErrorKind::InvalidMcp(error)))
}

/// `[providers.codex]` and `[providers.pi]`, and a refusal for any other provider table. A
/// misspelled or unread table would otherwise leave the operator believing a provider was
/// configured when no adapter reads it.
fn parse_providers(path: &Path, mut table: toml::Table) -> Result<ProviderTables, ConfigError> {
    let providers = ProviderTables {
        codex: command_table(path, &mut table, "codex", |config: &CodexConfig| {
            &config.command
        })?,
        pi: command_table(path, &mut table, "pi", |config: &PiConfig| &config.command)?,
    };
    if let Some(name) = table.keys().next() {
        let named = valid_workspace_id(name).then(|| name.clone());
        return Err(ConfigError::new(
            path,
            ConfigErrorKind::UnreadProvider(named),
        ));
    }
    Ok(providers)
}

fn command_table<T: DeserializeOwned>(
    path: &Path,
    table: &mut toml::Table,
    name: &'static str,
    command: fn(&T) -> &str,
) -> Result<Option<T>, ConfigError> {
    table
        .remove(name)
        .map(|value| {
            value
                .try_into::<T>()
                .ok()
                .filter(|config| {
                    let command = command(config);
                    let written = Path::new(command);
                    written.is_absolute() || written.file_name() == Some(command.as_ref())
                })
                .ok_or_else(|| ConfigError::new(path, ConfigErrorKind::InvalidProvider(name)))
        })
        .transpose()
}

fn parse_workspace(
    path: &Path,
    source: &str,
    item: WorkspaceTable,
) -> Result<Profile, ConfigError> {
    let invalid = || ConfigError::workspace(path, &item.workspace_id);
    let money = |value: toml::Spanned<toml::Value>| {
        source
            .get(value.span())
            .and_then(|raw| decimal_micro_dollars(raw, value.get_ref()))
            .filter(|usd| *usd > Usd::ZERO)
            .ok_or_else(invalid)
    };
    if !item.cwd.is_absolute() || !item.cwd.is_dir() {
        return Err(invalid());
    }
    let cwd = item.cwd.canonicalize().map_err(|_| invalid())?;
    if !cwd.join(".git").exists() {
        return Err(invalid());
    }
    let agent_usd = item.agent_usd_per_day.map(&money).transpose()?;
    let goal_usd = item.goal_usd.map(&money).transpose()?;
    let own = EnrolledCheck {
        id: item
            .completion_check_id
            .unwrap_or_else(|| ENROLLED_CHECK_ID.to_owned()),
        label: item.completion_check_label,
        predicate: Predicate::Command {
            argv: item.predicate_argv,
            timeout_s: item.predicate_timeout_s,
        },
        may_already_pass: item.may_already_pass,
    };
    let checks = std::iter::once(own)
        .chain(item.checks.into_iter().map(|row| EnrolledCheck {
            id: row.id,
            label: row.label,
            predicate: Predicate::Command {
                argv: row.argv,
                timeout_s: row.timeout_s,
            },
            may_already_pass: row.may_already_pass,
        }))
        .collect();
    Ok(Profile {
        id: item.workspace_id,
        label: item.label,
        checks,
        cwd,
        written_cwd: item.cwd,
        department: item.department,
        isolation: item.isolation,
        agent_budget: AgentBudget {
            usd_per_day: agent_usd,
            tokens_per_day: item.tokens_per_day.map(Tokens::new),
        },
        goal_budget: GoalBudget {
            usd: goal_usd,
            tokens: item.goal_tokens.map(Tokens::new),
        },
        hop_cap: item.hop_cap,
        mark: None,
    })
}

fn parse_per_turn_floor(
    path: &Path,
    source: &str,
    floor: PerTurnFloorTable,
) -> Result<Spend, ConfigError> {
    let usd = floor
        .usd
        .map(|value| {
            source
                .get(value.span())
                .and_then(|raw| decimal_micro_dollars(raw, value.get_ref()))
                .ok_or_else(|| ConfigError::new(path, ConfigErrorKind::InvalidPerTurnFloor("usd")))
        })
        .transpose()?
        .unwrap_or(Spend::DEFAULT_PER_TURN_FLOOR.usd);
    let invalid_tokens = || ConfigError::new(path, ConfigErrorKind::InvalidPerTurnFloor("tokens"));
    let tokens = floor
        .tokens
        .map(|value| match value.get_ref() {
            toml::Value::Integer(value) if *value >= 0 => u64::try_from(*value)
                .map(Tokens::new)
                .map_err(|_| invalid_tokens()),
            _ => Err(invalid_tokens()),
        })
        .transpose()?
        .unwrap_or(Spend::DEFAULT_PER_TURN_FLOOR.tokens);
    Ok(Spend {
        usd,
        tokens,
        work_tokens: tokens,
    })
}

fn decimal_micro_dollars(raw: &str, value: &toml::Value) -> Option<Usd> {
    matches!(value, toml::Value::Integer(_) | toml::Value::Float(_))
        .then_some(raw)
        .and_then(Usd::from_decimal)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::{
        ConfigPlatform, OperatorConfigPathError, load_operator_and_memory,
        load_operator_and_memory_with_options, load_operator_with_options,
        operator_config_path_for,
    };

    fn absolute(tail: &str) -> PathBuf {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        Path::new(root).join(tail)
    }

    #[test]
    fn recovery_is_default_off_and_loads_independently_of_control() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for (text, recovery, control) in [
            ("", false, false),
            ("[floor.recovery]\nenabled = false\n", false, false),
            ("[floor.recovery]\nenabled = true\n", true, false),
            (
                "[floor.recovery]\nenabled = true\n[floor.control]\nenabled = true\n",
                true,
                true,
            ),
        ] {
            std::fs::write(&path, text).unwrap();
            let loaded = super::load_operator(&path).unwrap();
            assert_eq!(loaded.recovery_enabled, recovery);
            assert_eq!(loaded.control.pause_resume(), control);
        }
        for text in [
            "[floor.recovery]\nenabled = 'yes'\n",
            "[floor.recovery]\nother = true\n",
        ] {
            std::fs::write(&path, text).unwrap();
            assert!(super::load_operator(&path).is_err());
        }
    }

    fn resolve(
        platform: ConfigPlatform,
        pairs: &[(&str, PathBuf)],
    ) -> Result<PathBuf, OperatorConfigPathError> {
        operator_config_path_for(platform, |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        })
    }

    const PLATFORMS: [ConfigPlatform; 3] = [
        ConfigPlatform::MacOs,
        ConfigPlatform::Windows,
        ConfigPlatform::Unix,
    ];

    /// The platform default is chosen by platform alone, and each one reads only its own root.
    #[test]
    fn each_platform_selects_its_own_default_file() {
        let env = [
            ("HOME", absolute("home")),
            ("APPDATA", absolute("appdata")),
            ("XDG_CONFIG_HOME", absolute("xdg")),
        ];
        assert_eq!(
            resolve(ConfigPlatform::MacOs, &env),
            Ok(absolute("home").join("Library/Application Support/dudes/config.toml"))
        );
        assert_eq!(
            resolve(ConfigPlatform::Windows, &env),
            Ok(absolute("appdata").join("dudes/config.toml"))
        );
        assert_eq!(
            resolve(ConfigPlatform::Unix, &env),
            Ok(absolute("xdg").join("dudes/config.toml"))
        );
        assert_eq!(
            resolve(ConfigPlatform::Unix, &env[..1]),
            Ok(absolute("home").join(".config/dudes/config.toml"))
        );
        for platform in [ConfigPlatform::Windows, ConfigPlatform::MacOs] {
            assert_eq!(
                resolve(platform, &[]),
                Err(OperatorConfigPathError::NoRoot),
                "{platform:?}"
            );
        }
    }

    /// `DUDES_CONFIG` names the file itself and beats every platform default, whatever else is set.
    #[test]
    fn dudes_config_wins_on_every_platform() {
        let chosen = absolute("chosen/ops.toml");
        for platform in PLATFORMS {
            let env = [
                ("DUDES_CONFIG", chosen.clone()),
                ("HOME", absolute("home")),
                ("APPDATA", absolute("appdata")),
                ("XDG_CONFIG_HOME", absolute("xdg")),
            ];
            assert_eq!(resolve(platform, &env), Ok(chosen.clone()), "{platform:?}");
            assert_eq!(
                resolve(platform, &env[..1]),
                Ok(chosen.clone()),
                "{platform:?} needs no home"
            );
        }
    }

    /// A relative or empty override is refused, never replaced by the default it was meant to avoid.
    #[test]
    fn a_relative_or_empty_override_is_refused_on_every_platform() {
        for platform in PLATFORMS {
            let env = |value: &str| {
                [
                    ("DUDES_CONFIG", PathBuf::from(value)),
                    ("HOME", absolute("home")),
                    ("APPDATA", absolute("appdata")),
                ]
            };
            assert_eq!(
                resolve(platform, &env("ops.toml")),
                Err(OperatorConfigPathError::RelativeOverride),
                "{platform:?}"
            );
            assert_eq!(
                resolve(platform, &env("./ops.toml")),
                Err(OperatorConfigPathError::RelativeOverride),
                "{platform:?}"
            );
            assert_eq!(
                resolve(platform, &env("")),
                Err(OperatorConfigPathError::EmptyOverride),
                "{platform:?}"
            );
        }
    }

    /// `[mcp.servers]` reaches both the options the daemon boots from and the settings a reload
    /// compares, without reading any variable it names; a refusal names the file and the server and
    /// repeats nothing else from it. Mutants: check the variable's presence at load, or echo the
    /// offending text.
    #[test]
    fn mcp_servers_load_without_their_variables_and_refusals_name_only_the_server() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(
            &path,
            "[mcp.servers.memory]\nurl = 'https://memory.example.com/mcp'\nbearer_env = 'MEMORY_BEARER_NEVER_SET'\n",
        )
        .unwrap();
        let startup = load_operator_with_options(&path).unwrap();
        assert_eq!(startup.options.mcp_servers(), &startup.operator.mcp_servers);
        assert_eq!(
            startup
                .operator
                .mcp_servers
                .keys()
                .map(dudes_types::McpServerName::as_str)
                .collect::<Vec<_>>(),
            ["memory"]
        );

        std::fs::write(
            &path,
            "[mcp.servers.memory]\ncommand = 'memory-server'\nsecret_env = ['sk-planted-value']\n",
        )
        .unwrap();
        assert_eq!(
            load_operator_with_options(&path).unwrap_err().to_string(),
            format!(
                "configuration at {} is refused: [mcp.servers.memory] names a variable that is not \
                 uppercase letters, digits and underscores, not starting with a digit",
                path.display()
            )
        );
        std::fs::write(&path, "[mcp]\nother = 1\n").unwrap();
        assert_eq!(
            load_operator_with_options(&path).unwrap_err().to_string(),
            format!(
                "configuration at {} is not a valid TOML document",
                path.display()
            )
        );
    }

    #[test]
    fn startup_retains_the_provider_configuration_for_reload_comparison() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(
            &path,
            "[providers.codex]\ncommand = 'codex-test'\nstreaming = true\n",
        )
        .unwrap();
        let startup = load_operator_with_options(&path).unwrap();
        assert_eq!(
            startup.options.clone().into_providers(),
            startup.operator.providers
        );
        assert!(
            startup
                .options
                .clone()
                .into_providers()
                .codex
                .as_ref()
                .unwrap()
                .streaming
        );
    }

    #[test]
    fn one_startup_snapshot_keeps_the_ignored_chat_key_and_explicit_memory_configuration() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(&path, "[floor.chat]\nenabled = true\n[memory]\ncondense_after_kib = 2\ncondense_keep_sections = 3\ncondense_model_enabled = false\ncondense_model = 'configured-model'\n").unwrap();
        let (startup, memory) = load_operator_and_memory_with_options(&path).unwrap();
        assert!(startup.options.chat_key_ignored());
        assert!(!startup.operator.condense_model_enabled);
        assert_eq!(
            startup.operator.condense_model.as_deref(),
            Some("configured-model")
        );
        assert_eq!(
            (memory.threshold_bytes(), memory.keep_sections()),
            (2_048, 3)
        );
        let (legacy, legacy_memory) = load_operator_and_memory(&path).unwrap();
        assert!(!legacy.condense_model_enabled);
        assert_eq!(legacy.condense_model.as_deref(), Some("configured-model"));
        assert_eq!(legacy_memory, memory);
        let options_only = load_operator_with_options(&path).unwrap();
        assert!(options_only.options.chat_key_ignored());
        assert_eq!(
            options_only.operator.condense_model,
            startup.operator.condense_model
        );
        std::fs::write(
            &path,
            "[floor.chat]\nenabled = false\n[memory]\ncondense_after_kib = 0\n",
        )
        .unwrap();
        assert!(load_operator_and_memory_with_options(&path).is_err());
        assert!(startup.options.chat_key_ignored());
        assert_eq!(
            (memory.threshold_bytes(), memory.keep_sections()),
            (2_048, 3)
        );
    }

    #[test]
    fn the_options_wrapper_preserves_memory_defaults_and_value_free_refusals() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        let (startup, memory) = load_operator_and_memory_with_options(&path).unwrap();
        assert!(!startup.options.chat_key_ignored());
        assert!(!startup.operator.condense_model_enabled);
        assert_eq!(
            (memory.threshold_bytes(), memory.keep_sections()),
            (65_536, 8)
        );
        std::fs::write(&path, "[floor.chat]\nenabled = true\n").unwrap();
        let (startup, memory) = load_operator_and_memory_with_options(&path).unwrap();
        assert!(startup.options.chat_key_ignored());
        assert!(startup.operator.condense_model_enabled);
        assert_eq!(startup.operator.condense_model, None);
        assert_eq!(
            (memory.threshold_bytes(), memory.keep_sections()),
            (65_536, 8)
        );
        for body in [
            "[floor.chat]\nenabled = true\n[memory]\ncondense_after_kib = 0\n",
            "[floor.chat]\nenabled = true\n[memory]\ncondense_keep_sections = 129\n",
            "[floor.chat]\nenabled = true\n[memory]\nunknown = 'private-sentinel'\n",
        ] {
            std::fs::write(&path, body).unwrap();
            let error = load_operator_and_memory_with_options(&path)
                .unwrap_err()
                .to_string();
            assert!(!error.contains("private-sentinel"));
            assert!(load_operator_and_memory(&path).is_err());
            assert!(load_operator_with_options(&path).is_err());
        }
    }
}
