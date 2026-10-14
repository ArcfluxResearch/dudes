//! Everything ambient, read once — the daemon paths, socket, runtime directory, and agent token.
//!
//! # One function reads the environment, and the rest of the binary takes values
//!
//! `bins/duded/src/main.rs` keeps the same discipline for the same reason: it makes the whole thing
//! drivable from a test with no process-wide mutation. [`Environment::resolve`] takes the lookup as
//! an argument, so a test names the variables it is testing instead of setting them on the process
//! — which matters more here than in the daemon, because a test that set `DUDES_TOKEN` globally
//! would leak it into every other test's subprocess.
//!
//! # Each command chooses its presentation
//!
//! Ask and agent memory commands use the inherited token. Memory with an explicit `--agent`
//! uses the operator credential, like the other operator commands. The daemon resolves either
//! presentation.
//!
//! # The token is read and presented only by the agent's command
//!
//! Puts `DUDES_TOKEN` and `DUDES_SOCK` in a managed child's environment, and this reads
//! both. `dudes ask` presents the token on `agent.ask`. Agent memory commands present it on
//! `memory.append` and `memory.read`; the
//! command carries no agent id, so the daemon derives the asker from the token and peer provenance.
//! `agent.hire`, `agent.why`, `goal.set` and `journal.tail` remain Operator rows, and `system.ping`
//! admits every role without needing anything presented.
//!
//! `tests/token.rs` asserts both the admitted `agent.ask` presentation and that the token reaches no
//! argv or output, with the socket path as the control that proves the capture would have seen it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use dudes_proto::rpc::hook::HookToken;
use dudes_providers::launch::{DAEMON_ENV_ALLOWLIST, EnvVar, billing_env_vars};
use dudes_types::{McpServerName, Provider};

/// The local endpoint's logical name inside a runtime directory.
///
/// **A second spelling of `duded::settings::SOCKET_NAME`, and it has to be.** This package depends
/// on the daemon only as a `[dev-dependencies]` entry — the CLI is a client and its layering row
/// says so — so the constant cannot be imported into `src/`. `tests/env.rs` asserts the two are
/// equal through that dev-dependency, which is the one place the drift is visible.
pub const SOCKET_NAME: &str = "duded.sock";

/// The daemon's single-writer lock file inside its state directory: `duded::lock::LOCK_FILE_NAME`,
/// spelled a second time for [`SOCKET_NAME`]'s reason, and compared with it by `tests/stop.rs`.
pub const LOCK_NAME: &str = "LOCK";

/// Where a per-user daemon keeps its runtime directory when nothing says otherwise.
///
/// Mirrors `bins/duded/src/main.rs`'s `settings_from_environment`, which is the only thing that
/// makes the default correct: the CLI does not choose where the daemon lives, it works out where
/// the daemon already put itself.
const DEFAULT_RUNTIME_SUBDIR: &str = ".local/state/dudes";

/// Where the daemon keeps its journal and projections when nothing says otherwise.
///
/// This mirrors `bins/duded/src/main.rs`'s `settings_from_environment`; quickstart sets the
/// resolved value explicitly so its detached child cannot resolve against a different home.
const DEFAULT_STATE_SUBDIR: &str = ".local/share/dudes";

/// A directory with an empty override or no admitted absolute default root.
///
/// The words are the daemon's own (`directory_from_values` in
/// `bins/duded/src/main.rs`), so an operator reads one refusal whichever binary met it first. The
/// current directory never stands in: it would put the journal, the socket and the credential
/// wherever the command happened to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NoDirectory {
    /// The override is unset and no absolute default root is available.
    #[cfg_attr(
        not(windows),
        error("HOME is unavailable; set an absolute HOME or {0}")
    )]
    #[cfg_attr(
        windows,
        error(
            "HOME is unavailable; set an absolute HOME or, with HOME unset, an absolute USERPROFILE or {0}"
        )
    )]
    NoHome(&'static str),
    /// The override is set to the empty string.
    #[error("{0} is empty")]
    Empty(&'static str),
}

/// The ambient values, resolved.
///
/// No `Clone`, and not by omission: [`HookToken`] has none, for the reason that type records — a
/// secret that can be duplicated by inference is a secret whose copies nobody counted. A value that
/// holds one inherits the rule.
#[derive(Debug)]
pub struct Environment {
    socket: Result<PathBuf, NoDirectory>,
    runtime_dir: Result<PathBuf, NoDirectory>,
    state_dir: Result<PathBuf, NoDirectory>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    mailbox: Result<PathBuf, NoMailbox>,
    daemon: Option<PathBuf>,
    daemon_env: Vec<EnvVar>,
    agent_token: Option<HookToken>,
    config: Result<PathBuf, NoConfig>,
    config_overridden: bool,
    current_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    declared: Declared,
    billed: Vec<Provider>,
    upgrade_env: Vec<EnvVar>,
}

/// The variables `dudes upgrade` hands to the programs it runs: where to find the tools the
/// installer uses, where to write, and how to reach GitHub through a proxy or a private CA.
/// `DUDES_INSTALL_BASE_URL` is deliberately absent, so the environment cannot redirect an upgrade.
const UPGRADE_ENV: [&str; 14] = [
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CURL_CA_BUNDLE",
];

/// The variables the operator file's `[mcp.servers]` declares, as this process holds them.
///
/// Quickstart forwards [`Declared::held`] to a daemon it starts, besides the allowlist, because a
/// quickstarted daemon would otherwise never see a secret the operator exported in this shell.
/// A file the daemon would refuse forwards nothing: the daemon's own refusal then says why.
#[derive(Debug, Default)]
pub(crate) struct Declared {
    held: Vec<EnvVar>,
    missing: Vec<(McpServerName, String)>,
}

impl Declared {
    /// The declared variables of `operator_file` that `lookup` answers with a non-empty value,
    /// and the server and name of each it does not.
    fn read(operator_file: &str, lookup: impl Fn(&str) -> Option<OsString>) -> Self {
        #[derive(Default, serde::Deserialize)]
        #[serde(default)]
        struct File {
            mcp: Mcp,
        }
        #[derive(Default, serde::Deserialize)]
        #[serde(default)]
        struct Mcp {
            servers: std::collections::BTreeMap<String, toml::Value>,
        }
        let Some(servers) = toml::from_str::<File>(operator_file)
            .ok()
            .and_then(|file| dudes_providers::mcp::parse_servers(file.mcp.servers).ok())
        else {
            return Self::default();
        };
        let mut declared = Self::default();
        let mut seen = std::collections::BTreeSet::new();
        for (server, definition) in &servers {
            for variable in definition.variables() {
                let name = variable.as_str();
                match lookup(name).filter(|value| !value.is_empty()) {
                    Some(value) if seen.insert(name) => {
                        declared.held.push(EnvVar::new(name, value));
                    }
                    Some(_) => {}
                    None => declared.missing.push((server.clone(), name.to_owned())),
                }
            }
        }
        declared
    }

    /// The pairs quickstart forwards, in server then variable order.
    pub(crate) fn held(&self) -> &[EnvVar] {
        &self.held
    }

    /// Each declared variable this process lacks, with the server that names it.
    pub(crate) fn missing(&self) -> &[(McpServerName, String)] {
        &self.missing
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub(crate) enum NoMailbox {
    #[error("DUDES_MAILBOX is not set")]
    Missing,
    #[error("DUDES_MAILBOX is empty")]
    Empty,
}

impl Environment {
    /// Read the real process environment and working directory.
    #[must_use]
    pub fn from_env() -> Self {
        let resolved = Self {
            current_dir: std::env::current_dir().ok(),
            ..Self::resolve(|name| std::env::var_os(name))
        };
        let file = resolved
            .config
            .as_deref()
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok());
        resolved.with_declared(file.as_deref(), |name| std::env::var_os(name))
    }

    /// The same values, with the MCP variables `operator_file` declares looked up in `lookup`.
    #[must_use]
    pub(crate) fn with_declared(
        self,
        operator_file: Option<&str>,
        lookup: impl Fn(&str) -> Option<OsString>,
    ) -> Self {
        Self {
            declared: operator_file
                .map(|text| Declared::read(text, lookup))
                .unwrap_or_default(),
            ..self
        }
    }

    /// Resolve from an arbitrary lookup, so a test names its own variables.
    ///
    /// # The resolution order, and why the socket and the directory are resolved separately
    ///
    /// - **The endpoint** is `DUDES_SOCK` when set, else the platform's
    ///   [`dudes_rpc::local_endpoint`] for `<runtime dir>` and `duded.sock`.
    /// - **The runtime directory** is `DUDES_RUNTIME_DIR` when set, else `<root>/.local/state/dudes`
    ///   for an absolute `HOME` or, on Windows with `HOME` absent, `USERPROFILE`; an empty override is
    ///   refused too. The state directory resolves
    ///   the same way from `DUDES_STATE_DIR` and `<root>/.local/share/dudes`.
    ///
    /// They are two resolutions rather than one because a managed child is handed
    /// `DUDES_SOCK` and a token and **not** a runtime directory, so deriving the directory from the
    /// socket would put the credential somewhere the daemon never said it was.
    ///
    /// # This buys no defence against a tokened child
    ///
    /// The split does not deny a tokened child the credential's path: the `$HOME` fallback below
    /// hands it over, deliberately, because it mirrors the daemon's own default location. A
    /// process carrying only the child environment plus an inherited `HOME` reaches the full
    /// operator surface:
    ///
    /// ```text
    /// DUDES_SOCK + DUDES_TOKEN + HOME (credential under it)  -> exit 0, agent hired
    /// DUDES_SOCK + DUDES_TOKEN + HOME (no credential)        -> exit 3, -32001
    /// ```
    ///
    /// **That is inside the accepted exposure, not outside it.** The gain is that reaching the
    /// operator set takes an *act* rather than an omission — running `dudes hire` is the act — and
    /// against a peer which reads the operator credential none of the method surface is closed.
    ///
    /// # The daemon's agent-tree refusal stands in front of that when the process relation places the caller inside an agent
    ///
    /// `classify` refuses an operator credential presented from inside an
    /// agent's tree. Linux, macOS and Windows now supply that process-qualified column; unsupported
    /// targets retain the explicit `NoWalk` fallback. The resolver owns that boundary, not this CLI.
    ///
    /// # The `$HOME` fallback is not the exposure, so refusing it is not a mitigation
    ///
    /// Refusing the fallback would not help, and the reason is
    /// structural: the credential is `dirname($DUDES_SOCK)/operator.cred` in every deployment, and
    /// a child composes the environment it hands `dudes` — so `DUDES_RUNTIME_DIR` reaches the same
    /// file with the fallback refused, and a child that forwards no `DUDES_TOKEN` is not a peer the
    /// refusal could key on at all.
    ///
    /// Both rows above are pinned, in the two places they belong: the resolution by
    /// `tests::a_managed_childs_environment_resolves_the_socket_and_the_credential_directory`
    /// below, and the exit codes by `bins/dudes/tests/doors.rs`, which runs the real binary against
    /// an explicit `duded::classify::NoWalk` fixture rather than the fixture's default column.
    ///
    /// A `DUDES_TOKEN` that is not a well-formed [`HookToken`] resolves to `None` rather than to an
    /// error. The daemon is the thing that decides what a token means — malformed text presents no
    /// token, resolves to `Unproven`, and is refused there — and a CLI that failed
    /// locally would be answering a question the daemon already answers. `dudes ask` therefore
    /// still sends the call without a recognised presentation and lets the daemon refuse it.
    #[must_use]
    pub fn resolve(lookup: impl Fn(&str) -> Option<OsString>) -> Self {
        let home_value = lookup("HOME");
        let home = home_value
            .as_ref()
            .map(PathBuf::from)
            .filter(|home| home.is_absolute());
        let storage_root = home_value
            .or_else(|| {
                if cfg!(windows) {
                    lookup("USERPROFILE")
                } else {
                    None
                }
            })
            .map(PathBuf::from)
            .filter(|root| root.is_absolute());
        let directory = |name: &'static str, subdir: &str| match lookup(name) {
            Some(path) if path.is_empty() => Err(NoDirectory::Empty(name)),
            Some(path) => Ok(PathBuf::from(path)),
            None => storage_root
                .as_ref()
                .map(|root| root.join(subdir))
                .ok_or(NoDirectory::NoHome(name)),
        };
        let runtime_dir = directory("DUDES_RUNTIME_DIR", DEFAULT_RUNTIME_SUBDIR);
        let state_dir = directory("DUDES_STATE_DIR", DEFAULT_STATE_SUBDIR);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let mailbox = match lookup("DUDES_MAILBOX") {
            Some(path) if path.is_empty() => Err(NoMailbox::Empty),
            Some(path) => Ok(PathBuf::from(path)),
            None => Err(NoMailbox::Missing),
        };
        let socket = lookup("DUDES_SOCK").map_or_else(
            || {
                runtime_dir
                    .as_deref()
                    .map(|dir| dudes_rpc::local_endpoint(dir, SOCKET_NAME))
                    .map_err(|missing| *missing)
            },
            |socket| Ok(PathBuf::from(socket)),
        );
        let daemon = lookup("DUDES_DAEMON").map(PathBuf::from);
        // One loop over [`DAEMON_ENV_ALLOWLIST`] and no second spelling of any name: the list
        // and the construction cannot drift apart, and a name is added by editing the constant
        // that carries the reason for it. Absent names are dropped rather than forwarded empty,
        // because an empty `PATH` is not the same instruction as no `PATH`.
        let mut daemon_env = Vec::with_capacity(DAEMON_ENV_ALLOWLIST.len());
        for name in DAEMON_ENV_ALLOWLIST {
            if let Some(value) = lookup(name) {
                daemon_env.push(EnvVar::new(*name, value));
            }
        }
        let agent_token = lookup("DUDES_TOKEN")
            .and_then(|value| value.into_string().ok())
            .and_then(|text| HookToken::new(text).ok());
        let config = operator_config_path(&lookup);
        let set = |name: &str| lookup(name).is_some_and(|value| !value.is_empty());
        let billed = [Provider::Claude, Provider::Codex, Provider::Pi]
            .into_iter()
            .filter(|provider| billing_env_vars(*provider).iter().any(|name| set(name)))
            .collect();
        Self {
            socket,
            runtime_dir,
            state_dir,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            mailbox,
            daemon,
            daemon_env,
            agent_token,
            config,
            config_overridden: lookup("DUDES_CONFIG").is_some(),
            current_dir: None,
            home,
            declared: Declared::default(),
            billed,
            upgrade_env: UPGRADE_ENV
                .iter()
                .filter_map(|name| Some(EnvVar::new(*name, lookup(name)?)))
                .collect(),
        }
    }

    /// The absolute `HOME`, where a vendor login runs.
    #[must_use]
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// The directory the command runs in, which `quickstart` enrolls and hires into; `None` when
    /// it cannot be read, and always from [`Self::resolve`].
    #[must_use]
    pub fn current_dir(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }

    /// The daemon's operator file, which `dudes workspace` edits.
    ///
    /// # Errors
    ///
    /// Returns [`NoConfig`] when `DUDES_CONFIG` is refused or the environment names no absolute
    /// root for the platform default.
    pub fn config(&self) -> Result<&Path, NoConfig> {
        self.config.as_deref().map_err(|error| *error)
    }

    /// Whether `DUDES_CONFIG` names the operator file, rather than the platform default.
    pub(crate) fn config_overridden(&self) -> bool {
        self.config_overridden
    }

    /// The local control endpoint this CLI connects to.
    ///
    /// # Errors
    ///
    /// [`NoDirectory`] when neither `DUDES_SOCK` nor a runtime directory names it.
    pub fn socket(&self) -> Result<&Path, NoDirectory> {
        self.socket.as_deref().map_err(|missing| *missing)
    }

    /// The directory the operator credential is read from.
    ///
    /// # Errors
    ///
    /// [`NoDirectory`] when neither an override nor an absolute default root names it.
    pub fn runtime_dir(&self) -> Result<&Path, NoDirectory> {
        self.runtime_dir.as_deref().map_err(|missing| *missing)
    }

    /// The daemon's persistent state directory.
    pub(crate) fn state_dir(&self) -> Result<&Path, NoDirectory> {
        self.state_dir.as_deref().map_err(|missing| *missing)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn mailbox(&self) -> Result<&Path, NoMailbox> {
        self.mailbox.as_deref().map_err(|missing| *missing)
    }

    /// The daemon executable, resolved absolutely without a `PATH` search.
    ///
    /// # Errors
    ///
    /// When the operating system cannot report the running `dudes` executable and no explicit
    /// `DUDES_DAEMON` was supplied.
    pub(crate) fn daemon(&self) -> std::io::Result<PathBuf> {
        if let Some(path) = &self.daemon {
            return Ok(path.clone());
        }
        let mut sibling = std::env::current_exe()?;
        sibling.set_file_name(format!("duded{}", std::env::consts::EXE_SUFFIX));
        Ok(sibling)
    }

    /// The [`DAEMON_ENV_ALLOWLIST`] names this process actually holds, in that constant's order.
    ///
    /// Not the daemon's whole environment: quickstart adds `DUDES_RUNTIME_DIR` and
    /// `DUDES_STATE_DIR` on top of these, and adds nothing else. The rationale for which names are
    /// here and which are deliberately absent lives on that constant.
    pub(crate) fn daemon_env(&self) -> &[EnvVar] {
        &self.daemon_env
    }

    /// The daemon `DUDES_DAEMON` names, when it is set.
    pub(crate) fn daemon_override(&self) -> Option<&Path> {
        self.daemon.as_deref()
    }

    /// The MCP variables the operator file declares, as this process holds them.
    pub(crate) fn declared(&self) -> &Declared {
        &self.declared
    }

    /// The variables `dudes upgrade` passes to `curl` and the installer.
    pub(crate) fn upgrade_env(&self) -> &[EnvVar] {
        &self.upgrade_env
    }

    /// The engines for which this process holds a credential that bills each token, by the lists
    /// the daemon's adapters use.
    pub(crate) fn billed(&self) -> &[Provider] {
        &self.billed
    }

    /// The agent token this process was given, if it was given a well-formed one.
    ///
    /// Presented by `dudes ask` and the agent memory commands. See this module's header. Returned by reference and never
    /// by value because [`HookToken`] is a secret with no `Clone`, so a caller that hands it on is
    /// visible at the call site.
    #[must_use]
    pub fn agent_token(&self) -> Option<&HookToken> {
        self.agent_token.as_ref()
    }
}

/// Why the operator file path is unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NoConfig {
    /// `DUDES_CONFIG` is set to the empty string.
    #[error("DUDES_CONFIG is empty; set an absolute path to the operator file or unset it")]
    Empty,
    /// `DUDES_CONFIG` is set to a path that is not absolute.
    #[error("DUDES_CONFIG must be an absolute path to the operator file")]
    Relative,
    /// There is no override and the platform default has no absolute root.
    #[error("no operator configuration path: the environment names no absolute home")]
    NoRoot,
}

/// Where the daemon reads its operator file: `DUDES_CONFIG` when set, else the platform default.
///
/// **A second spelling of `duded::config::operator_config_path`, for [`SOCKET_NAME`]'s reason**:
/// the CLI links no daemon code outside its dev-dependencies. `tests/workspace.rs` asserts the two
/// agree on the host platform.
fn operator_config_path(lookup: &impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, NoConfig> {
    operator_config_path_for(Platform::current(), lookup)
}

#[derive(Clone, Copy)]
enum Platform {
    MacOs,
    Windows,
    Unix,
}

impl Platform {
    const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

fn operator_config_path_for(
    platform: Platform,
    lookup: &impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, NoConfig> {
    if let Some(configured) = lookup("DUDES_CONFIG") {
        if configured.is_empty() {
            return Err(NoConfig::Empty);
        }
        let path = PathBuf::from(configured);
        return if path.is_absolute() {
            Ok(path)
        } else {
            Err(NoConfig::Relative)
        };
    }
    let absolute = |name: &str| {
        lookup(name)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    match platform {
        Platform::MacOs => {
            absolute("HOME").map(|home| home.join("Library/Application Support/dudes/config.toml"))
        }
        Platform::Windows => absolute("APPDATA").map(|root| root.join("dudes/config.toml")),
        Platform::Unix => absolute("XDG_CONFIG_HOME")
            .or_else(|| absolute("HOME").map(|home| home.join(".config")))
            .map(|root| root.join("dudes/config.toml")),
    }
    .ok_or(NoConfig::NoRoot)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dudes_proto::rpc::hook::HOOK_TOKEN_LEN;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Environment {
        let map: BTreeMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(*v)))
            .collect();
        Environment::resolve(move |name| map.get(name).cloned())
    }

    /// An absolute `HOME` on the host platform, which is the only kind the resolver falls back on.
    const HOME: &str = if cfg!(windows) {
        r"C:\Users\operator"
    } else {
        "/home/operator"
    };

    fn token_text() -> String {
        "t".repeat(HOOK_TOKEN_LEN)
    }

    /// The programs `dudes upgrade` runs get a short allowlist, and not `DUDES_INSTALL_BASE_URL`,
    /// so the environment cannot point an upgrade at another source.
    #[test]
    fn upgrade_children_get_the_allowlist_and_never_the_install_source() {
        let resolved = env(&[
            ("PATH", "/usr/bin:/bin"),
            ("DUDES_INSTALL_BASE_URL", "http://127.0.0.1:1"),
            ("HTTPS_PROXY", "http://proxy.example:3128"),
            ("DUDES_TOKEN", "secret"),
            ("OPENAI_API_KEY", "secret"),
        ]);
        assert_eq!(
            resolved.upgrade_env(),
            [
                EnvVar::new("PATH", "/usr/bin:/bin"),
                EnvVar::new("HTTPS_PROXY", "http://proxy.example:3128"),
            ]
        );
    }

    /// `DUDES_SOCK` wins over the runtime directory, and the runtime directory is still resolved —
    /// which is the pair that matters, because the credential is read from the second while the
    /// connection is made to the first. A resolver that derived one from the other would fail this.
    #[test]
    fn the_socket_and_the_runtime_directory_resolve_independently() {
        let resolved = env(&[
            ("DUDES_SOCK", "/tmp/elsewhere.sock"),
            ("DUDES_RUNTIME_DIR", "/run/dudes"),
        ]);
        assert_eq!(resolved.socket(), Ok(Path::new("/tmp/elsewhere.sock")));
        assert_eq!(resolved.runtime_dir(), Ok(Path::new("/run/dudes")));
    }

    /// Without `DUDES_SOCK` the endpoint is derived from the runtime directory, which is the
    /// arrangement an operator gets. The control is the arm above: the same field takes a different
    /// value when the variable is set, so this is not a constant.
    #[test]
    fn the_endpoint_defaults_from_the_runtime_directory() {
        let resolved = env(&[("DUDES_RUNTIME_DIR", "/run/dudes")]);
        assert_eq!(
            resolved.socket(),
            Ok(dudes_rpc::local_endpoint("/run/dudes", SOCKET_NAME).as_path())
        );
    }

    /// With neither variable it is `$HOME`'s, which is where a per-user daemon puts itself.
    #[test]
    fn the_runtime_directory_defaults_under_home() {
        let resolved = env(&[("HOME", HOME)]);
        assert_eq!(
            resolved.runtime_dir(),
            Ok(Path::new(HOME).join(DEFAULT_RUNTIME_SUBDIR).as_path())
        );
        assert_eq!(
            resolved.socket(),
            Ok(dudes_rpc::local_endpoint(
                Path::new(HOME).join(DEFAULT_RUNTIME_SUBDIR),
                SOCKET_NAME
            )
            .as_path())
        );
    }

    /// Quickstart hands the daemon the same state directory the daemon would resolve itself and
    /// resolves its executable without a `PATH` search. The absent-override arm is the control
    /// against a resolver that returned the configured path as a constant.
    #[test]
    fn quickstart_resolves_the_daemon_and_state_directory() {
        let configured = env(&[
            ("DUDES_DAEMON", "/opt/dudes/bin/duded"),
            ("DUDES_STATE_DIR", "/var/lib/dudes"),
        ]);
        assert_eq!(
            configured.daemon().expect("the configured daemon"),
            Path::new("/opt/dudes/bin/duded")
        );
        assert_eq!(configured.state_dir(), Ok(Path::new("/var/lib/dudes")));

        let defaults = env(&[]);
        let mut sibling = std::env::current_exe().expect("this test executable has a path");
        sibling.set_file_name(format!("duded{}", std::env::consts::EXE_SUFFIX));
        assert_eq!(defaults.daemon().expect("the sibling daemon"), sibling);
    }

    /// Without an absolute `HOME`, a directory no variable names is refused with the daemon's own
    /// words and never resolved under the current directory. `HOME` absent, empty and relative
    /// are the three ways to have none; the override arm is the control that keeps each refusal
    /// about the home rather than about the directory.
    #[test]
    fn a_default_directory_needs_an_absolute_home() {
        for home in [None, Some(""), Some("relative/home")] {
            let mut pairs = Vec::new();
            if let Some(home) = home {
                pairs.push(("HOME", home));
            }
            let resolved = env(&pairs);
            let state_refusal = if cfg!(windows) {
                "HOME is unavailable; set an absolute HOME or, with HOME unset, an absolute USERPROFILE or DUDES_STATE_DIR"
            } else {
                "HOME is unavailable; set an absolute HOME or DUDES_STATE_DIR"
            };
            let runtime_refusal = if cfg!(windows) {
                "HOME is unavailable; set an absolute HOME or, with HOME unset, an absolute USERPROFILE or DUDES_RUNTIME_DIR"
            } else {
                "HOME is unavailable; set an absolute HOME or DUDES_RUNTIME_DIR"
            };
            assert_eq!(
                resolved.state_dir().map_err(|missing| missing.to_string()),
                Err(state_refusal.to_owned()),
                "HOME {home:?}"
            );
            assert_eq!(
                resolved
                    .runtime_dir()
                    .map_err(|missing| missing.to_string()),
                Err(runtime_refusal.to_owned()),
                "HOME {home:?}"
            );
            assert_eq!(
                resolved.socket(),
                Err(NoDirectory::NoHome("DUDES_RUNTIME_DIR")),
                "HOME {home:?}"
            );

            pairs.extend([
                ("DUDES_STATE_DIR", "/var/lib/dudes"),
                ("DUDES_RUNTIME_DIR", "/run/dudes"),
            ]);
            let overridden = env(&pairs);
            assert_eq!(overridden.state_dir(), Ok(Path::new("/var/lib/dudes")));
            assert_eq!(overridden.runtime_dir(), Ok(Path::new("/run/dudes")));
        }
    }

    /// An empty override names no directory, even with an absolute `HOME` to fall back on: it is
    /// refused rather than resolved as the current directory or replaced by the default.
    #[test]
    fn an_empty_override_is_refused() {
        let resolved = env(&[
            ("HOME", HOME),
            ("DUDES_STATE_DIR", ""),
            ("DUDES_RUNTIME_DIR", ""),
        ]);
        assert_eq!(
            resolved.state_dir().map_err(|missing| missing.to_string()),
            Err("DUDES_STATE_DIR is empty".to_owned())
        );
        assert_eq!(
            resolved
                .runtime_dir()
                .map_err(|missing| missing.to_string()),
            Err("DUDES_RUNTIME_DIR is empty".to_owned())
        );
        assert_eq!(
            resolved.socket(),
            Err(NoDirectory::Empty("DUDES_RUNTIME_DIR"))
        );
    }

    #[test]
    fn storage_root_does_not_redefine_the_vendor_login_home() {
        let profile = tempfile::tempdir().expect("absolute profile");
        let profile = profile.path().to_str().expect("UTF-8 test profile");
        let resolved = env(&[("USERPROFILE", profile)]);
        assert_eq!(resolved.home(), None);
        let resolved = env(&[("HOME", HOME), ("USERPROFILE", profile)]);
        assert_eq!(resolved.home(), Some(Path::new(HOME)));
        assert_eq!(
            resolved.runtime_dir(),
            Ok(Path::new(HOME).join(".local/state/dudes").as_path())
        );
        assert_eq!(
            resolved.state_dir(),
            Ok(Path::new(HOME).join(".local/share/dudes").as_path())
        );
    }

    #[test]
    fn relative_overrides_do_not_require_a_default_root() {
        let resolved = env(&[
            ("HOME", ""),
            ("DUDES_RUNTIME_DIR", "relative-runtime"),
            ("DUDES_STATE_DIR", "relative-state"),
        ]);
        assert_eq!(resolved.runtime_dir(), Ok(Path::new("relative-runtime")));
        assert_eq!(resolved.state_dir(), Ok(Path::new("relative-state")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_storage_defaults_use_profile_only_when_home_is_absent() {
        let profile = r"C:\Users\A Name";
        let resolved = env(&[("USERPROFILE", profile)]);
        assert_eq!(
            resolved.runtime_dir(),
            Ok(Path::new(r"C:\Users\A Name\.local\state\dudes"))
        );
        assert_eq!(
            resolved.state_dir(),
            Ok(Path::new(r"C:\Users\A Name\.local\share\dudes"))
        );
        for home in ["", "relative", r"C:relative", r"\relative"] {
            let resolved = env(&[("HOME", home), ("USERPROFILE", profile)]);
            assert!(resolved.runtime_dir().is_err(), "HOME {home:?}");
            assert!(resolved.state_dir().is_err(), "HOME {home:?}");
            let overridden = env(&[
                ("HOME", home),
                ("USERPROFILE", profile),
                ("DUDES_RUNTIME_DIR", r"C:\explicit\run"),
                ("DUDES_STATE_DIR", "relative-state"),
            ]);
            assert_eq!(overridden.runtime_dir(), Ok(Path::new(r"C:\explicit\run")));
            assert_eq!(overridden.state_dir(), Ok(Path::new("relative-state")));
        }
        for profile in ["", "relative", r"C:relative", r"\relative"] {
            let resolved = env(&[("USERPROFILE", profile)]);
            assert!(resolved.runtime_dir().is_err(), "USERPROFILE {profile:?}");
            assert!(resolved.state_dir().is_err(), "USERPROFILE {profile:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_storage_defaults_ignore_windows_roots() {
        let profile = tempfile::tempdir().expect("absolute profile");
        let profile = profile.path().to_str().expect("UTF-8 test profile");
        let resolved = env(&[("USERPROFILE", profile), ("LOCALAPPDATA", profile)]);
        assert!(resolved.runtime_dir().is_err());
        assert!(resolved.state_dir().is_err());
        let overridden = env(&[
            ("USERPROFILE", profile),
            ("DUDES_RUNTIME_DIR", "/run/dudes"),
        ]);
        assert_eq!(overridden.runtime_dir(), Ok(Path::new("/run/dudes")));
        assert!(overridden.state_dir().is_err());
    }

    /// The daemon's ambient share is [`DAEMON_ENV_ALLOWLIST`], in that order, carrying the
    /// caller's values — and nothing else the caller held, whatever it was called.
    ///
    /// **The expected list is written out literally and is never derived from the constant**, which
    /// is the whole of what makes it a guard: a fixture built from the list it guards tracks that
    /// list and cannot go red when the list loses an entry.
    ///
    /// **Two fail-side controls, and the order of the two assertions is what keeps them two.** The
    /// value scan runs first, so adding `ANTHROPIC_API_KEY` to `DAEMON_ENV_ALLOWLIST` fails on the
    /// scan while removing `LANG` reaches it intact and fails on the pair list — two mutations,
    /// two lines, two messages. Ordered the other way the equality answers both and the scan can
    /// never fire: it is then not a second control but a line that has only ever been green.
    ///
    /// **`TMPDIR` is on the allowlist and is deliberately not planted.** That is the arm which
    /// pins *dropped, not forwarded empty*: composing with `lookup(name).unwrap_or_default()`
    /// yields a fourth pair `("TMPDIR", "")` and fails the pair list, and with every allowlisted
    /// name planted that mutation survives untouched. The distinction is not cosmetic — the daemon
    /// snapshots `PATH` into `Settings::predicate_path` (`bins/duded/src/main.rs:365`), where an
    /// empty value is a different instruction from an absent one.
    ///
    /// Values are asserted rather than only names, so a construction that forwarded the right
    /// names with wrong or defaulted values cannot pass.
    ///
    /// This is the construction half. The process half — what the spawned daemon's environment
    /// actually contains — is `tests/quickstart.rs`, and it has to be separate: nothing in this
    /// module creates a process, so a correct vector wired to an inheriting spawn passes here.
    #[test]
    fn quickstart_composes_the_daemon_environment() {
        const PLANTED: &str = "must-not-travel";

        let resolved = env(&[
            ("PATH", "/planted/bin"),
            ("HOME", "/planted/home"),
            // `TMPDIR` is allowlisted and deliberately absent from what the caller holds.
            ("LANG", "planted.UTF-8"),
            // Only the names the daemon needs to start on this platform cross.
            ("APPDATA", "/planted/appdata"),
            ("SystemRoot", "/planted/windows"),
            ("XDG_CONFIG_HOME", "/planted/config"),
            ("USERPROFILE", PLANTED),
            ("LOCALAPPDATA", PLANTED),
            ("ANTHROPIC_API_KEY", PLANTED),
            ("PLANTED_API_KEY", PLANTED),
            ("RUST_LOG", PLANTED),
            ("QUICKSTART_UNLISTED_SENTINEL", PLANTED),
        ]);

        let composed: Vec<(&str, &str)> = resolved
            .daemon_env()
            .iter()
            .map(|var| {
                (
                    var.name().to_str().expect("an allowlisted name is UTF-8"),
                    var.expose_value()
                        .to_str()
                        .expect("this test's values are UTF-8"),
                )
            })
            .collect();
        assert!(
            composed.iter().all(|(_, value)| *value != PLANTED),
            "a planted value reached the daemon's environment: {composed:?}"
        );
        #[cfg(target_os = "macos")]
        let expected = [
            ("PATH", "/planted/bin"),
            ("HOME", "/planted/home"),
            ("LANG", "planted.UTF-8"),
        ];
        #[cfg(windows)]
        let expected = [
            ("PATH", "/planted/bin"),
            ("HOME", "/planted/home"),
            ("LANG", "planted.UTF-8"),
            ("APPDATA", "/planted/appdata"),
            ("SystemRoot", "/planted/windows"),
        ];
        #[cfg(not(any(target_os = "macos", windows)))]
        let expected = [
            ("PATH", "/planted/bin"),
            ("HOME", "/planted/home"),
            ("LANG", "planted.UTF-8"),
            ("XDG_CONFIG_HOME", "/planted/config"),
        ];
        assert_eq!(
            composed, expected,
            "the ambient share is the allowlist in order, carrying the caller's values, and an \
             allowlisted name the caller does not hold is dropped rather than forwarded empty"
        );
    }

    fn config_for(platform: Platform, pairs: &[(&str, &str)]) -> Result<PathBuf, NoConfig> {
        operator_config_path_for(platform, &|name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        })
    }

    fn host_absolute(tail: &str) -> String {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        format!("{root}{tail}")
    }

    #[test]
    fn each_platform_selects_its_own_default_file() {
        let (home, appdata, xdg) = (
            host_absolute("home"),
            host_absolute("appdata"),
            host_absolute("xdg"),
        );
        let all = [
            ("HOME", &*home),
            ("APPDATA", &*appdata),
            ("XDG_CONFIG_HOME", &*xdg),
        ];
        assert_eq!(
            config_for(Platform::MacOs, &all),
            Ok(PathBuf::from(&home).join("Library/Application Support/dudes/config.toml"))
        );
        assert_eq!(
            config_for(Platform::Windows, &all),
            Ok(PathBuf::from(&appdata).join("dudes/config.toml"))
        );
        assert_eq!(
            config_for(Platform::Unix, &all),
            Ok(PathBuf::from(&xdg).join("dudes/config.toml"))
        );
        assert_eq!(
            config_for(Platform::Unix, &all[..1]),
            Ok(PathBuf::from(&home).join(".config/dudes/config.toml"))
        );
        assert_eq!(config_for(Platform::Windows, &[]), Err(NoConfig::NoRoot));
    }

    #[test]
    fn dudes_config_wins_on_every_platform_and_a_bad_one_is_refused() {
        let chosen = host_absolute("chosen/ops.toml");
        let home = host_absolute("home");
        for platform in [Platform::MacOs, Platform::Windows, Platform::Unix] {
            assert_eq!(
                config_for(platform, &[("DUDES_CONFIG", &chosen), ("HOME", &home)]),
                Ok(PathBuf::from(&chosen))
            );
            assert_eq!(
                config_for(platform, &[("DUDES_CONFIG", &chosen)]),
                Ok(PathBuf::from(&chosen)),
                "no home needed"
            );
            assert_eq!(
                config_for(platform, &[("DUDES_CONFIG", "ops.toml"), ("HOME", &home)]),
                Err(NoConfig::Relative)
            );
            assert_eq!(
                config_for(platform, &[("DUDES_CONFIG", ""), ("HOME", &home)]),
                Err(NoConfig::Empty)
            );
        }
    }

    /// Every name the daemon's rule for its operator file reads crosses to a quickstarted
    /// daemon, so the daemon resolves the file the CLI writes. The rule is asked twice, with
    /// nothing set and with every name set, so a name it reads only after another lookup succeeds
    /// is caught as well as one it reads only on the way to giving up.
    #[test]
    fn every_name_the_daemons_config_rule_reads_crosses() {
        let absolute = if cfg!(windows) {
            r"C:\planted-root"
        } else {
            "/planted-root"
        };
        let mut read = Vec::new();
        for answer in [None, Some(std::ffi::OsString::from(absolute))] {
            let asked = std::cell::RefCell::new(Vec::new());
            let found = duded::config::operator_config_path(|name| {
                asked.borrow_mut().push(name.to_owned());
                if name == "DUDES_CONFIG" {
                    None
                } else {
                    answer.clone()
                }
            });
            assert_eq!(
                found.is_ok(),
                answer.is_some(),
                "control: the rule finds a file exactly when its names are set"
            );
            read.extend(asked.into_inner());
        }
        let withheld: Vec<&String> = read
            .iter()
            .filter(|name| !DAEMON_ENV_ALLOWLIST.contains(&name.as_str()))
            .collect();
        assert!(
            withheld.is_empty(),
            "the daemon's config rule reads {withheld:?}, which quickstart withholds"
        );
    }

    /// The accepted exposure, in the one configuration nothing else here pins:
    /// `DUDES_SOCK` and `HOME` set, `DUDES_RUNTIME_DIR` **absent**.
    ///
    /// That is what a managed child holds — the `DUDES_SOCK` and token pair, plus the `HOME` the daemon's own
    /// environment passes down — and both halves of the answer matter, because they are read for
    /// different purposes: the socket is what the CLI connects to, the runtime directory is where
    /// it reads the operator credential from. The independence arm above covers the same socket
    /// with `DUDES_RUNTIME_DIR` set, which is the operator's arrangement and not this one.
    ///
    /// **This is a resolution, not an outcome.** `bins/dudes/tests/doors.rs` carries the exit
    /// codes for each credential presentation, over a real socket and against the resolver that ships; nothing
    /// in this module can produce one.
    ///
    /// The control is `the_socket_and_the_runtime_directory_resolve_independently` above, where
    /// the runtime directory takes a different value under the same `DUDES_SOCK` — so neither
    /// assertion here can be passing as a constant.
    #[test]
    fn a_managed_childs_environment_resolves_the_socket_and_the_credential_directory() {
        let resolved = env(&[
            ("DUDES_SOCK", "/run/dudes/duded.sock"),
            ("DUDES_TOKEN", &token_text()),
            ("HOME", HOME),
        ]);

        assert_eq!(
            resolved.socket(),
            Ok(Path::new("/run/dudes/duded.sock")),
            "the socket is the one the daemon named"
        );
        assert_eq!(
            resolved.runtime_dir(),
            Ok(Path::new(HOME).join(DEFAULT_RUNTIME_SUBDIR).as_path()),
            "and with no DUDES_RUNTIME_DIR the credential is read from under $HOME"
        );
        assert!(
            resolved.agent_token().is_some(),
            "the peer this is about is a tokened one"
        );
    }

    /// The token is read when it is well formed and is dropped when it is not — both arms, because
    /// a resolver that returned `None` unconditionally would satisfy the second alone and would
    /// make the whole agent door invisible.
    #[test]
    fn a_well_formed_token_is_read_and_a_malformed_one_is_not() {
        let with = env(&[("DUDES_TOKEN", &token_text())]);
        assert_eq!(
            with.agent_token().map(HookToken::expose),
            Some(token_text().as_str()),
            "positive control: a well-formed token is discovered"
        );

        for bad in ["", "short", &"t".repeat(HOOK_TOKEN_LEN + 1), "!!!"] {
            assert!(
                env(&[("DUDES_TOKEN", bad)]).agent_token().is_none(),
                "`{bad}` is not a token"
            );
        }
        assert!(env(&[]).agent_token().is_none(), "and absent is absent");
    }

    /// Neither the agent token nor a daemon environment value reaches the `Debug` of the
    /// environment that holds them. [`HookToken`] and [`EnvVar`] each carry their own redaction,
    /// and the assertion is here because *this* struct is the one a diagnostic would print.
    ///
    /// `ExposesValue` is the firing control: it is the ordinary hand-written formatter mistake
    /// that calls [`EnvVar::expose_value`]. The same predicate that accepts `Environment` must
    /// reject that rendering, so an empty or unrelated `Debug` cannot satisfy the absence claim.
    #[test]
    fn the_environments_debug_redacts_the_token() {
        const ENV_VALUE: &str = "sk-ant-planted-canary-value-do-not-print";

        struct ExposesValue<'a>(&'a EnvVar);

        impl std::fmt::Debug for ExposesValue<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("EnvVar")
                    .field("name", &self.0.name())
                    .field("value", &self.0.expose_value())
                    .finish()
            }
        }

        let token = format!("{}{}", "zq7", "t".repeat(HOOK_TOKEN_LEN - 3));
        let resolved = env(&[("PATH", ENV_VALUE), ("DUDES_TOKEN", &token)]);
        let value_is_redacted =
            |printed: &str| printed.contains("PATH") && !printed.contains(ENV_VALUE);

        let shown = format!("{resolved:?}");
        assert!(
            value_is_redacted(&shown),
            "the PATH name must print and its value must not: {shown}"
        );
        assert!(!shown.contains("zq7"), "the token reached a Debug: {shown}");
        assert!(shown.contains("redacted"), "{shown}");

        let leaking = format!("{:?}", ExposesValue(&resolved.daemon_env()[0]));
        assert!(
            !value_is_redacted(&leaking),
            "positive control: the predicate accepted a formatter that exposed the value: {leaking}"
        );
    }
}
