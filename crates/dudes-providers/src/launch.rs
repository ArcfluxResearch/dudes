//! Typed arguments and explicit standard-input modes for one owned child.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::PathBuf;

/// One environment variable of a child process: a name that prints and a value that does not.
///
/// The type exists for its `Debug`. An environment value may be a credential — the operator's
/// `ANTHROPIC_API_KEY`, or the per-agent `DUDES_TOKEN` the supervisor sets on every child it
/// launches — and no formatter can tell which, so no value is printed at all:
/// `Debug` writes `"NAME"=<redacted>`. Names stay visible, because *which* variables reached a
/// child is what a trace line is usually asked. `argv` is untouched because it is not secret:
/// a spec whose argv were redacted would be undebuggable, and the task text on it is what the
/// operator typed.
///
/// Carrying the pair in a type, rather than redacting at each place one is printed, preferred
/// because it survives someone adding a second env-carrying struct.
/// [`LaunchSpec`], [`AgentCtx`](crate::adapter::AgentCtx),
/// [`UsageSource::OtelEnv`](crate::adapter::UsageSource::OtelEnv) and the supervisor's `Config`
/// are the four such types under a `crates/*/src` — `AgentCtx` and `Config` hold the daemon's
/// environment *before* the scrub, so an operator's vendor key is in them — and each is safe to
/// `Debug`-print because its values are `EnvVar`s. The daemon's `Settings` and the CLI's
/// `Environment` are the two binary types, and use `EnvVar` for the same reason.
///
/// The next type inherits that by using this one. `cargo xtask env-pairs` enforces the choice for
/// every workspace library under `crates/` and both binaries that hold an environment: it parses
/// type declarations and rejects a raw environment name/value representation within the limits
/// its module states. `OtelEnv` was the first representation that guard found.
///
/// Not a credential type: equality is derived, so it is not constant-time, and ordering is by
/// name then value, which keeps a sorted environment byte-stable. A secret compared against one
/// a caller presented belongs in `dudes_proto::HookToken`, which compares in constant time.
///
/// `Clone` is derived, and it is the one `HookToken` protection this type deliberately drops:
/// an environment value exists in order to be copied into a child. The scrub builds the child's
/// environment by cloning the daemon's, and [`LaunchSpec`], the supervisor's `Config` and the
/// `Command` that carries a spec to the machine each derive `Clone` over one. Withholding it
/// would not withhold the value — `expose_value` already hands that out — it would only make the
/// copy that has to happen anyway awkward.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EnvVar {
    name: OsString, // purity-ok: canonical redacting EnvVar
    value: OsString,
}

impl EnvVar {
    /// A variable from its name and its value.
    pub fn new(name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }

    /// The variable's name. Not a secret: it is what `Debug` prints, and what the nesting
    /// scrub matches on.
    #[must_use]
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// The variable's value. Named so that a call site handing it on — into a process launch, a
    /// message, or a log line — is visible in review, the way `HookToken::expose` is.
    #[must_use]
    pub fn expose_value(&self) -> &OsStr {
        &self.value
    }
}

impl fmt::Debug for EnvVar {
    /// `"NAME"=<redacted>`. The name goes through `OsStr`'s own `Debug`, so it arrives quoted
    /// and escaped and a name containing a newline cannot forge a second line of output.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}=<redacted>", self.name)
    }
}

/// The names a quickstarted daemon receives from the caller's ambient environment, in the order
/// they are handed over.
///
/// **A positive set, not a filtered inheritance.** `quickstart::spawn_daemon` clears the child's
/// environment, applies this list, adds each variable the operator file declares for an MCP server
/// when the caller holds it ([`crate::mcp`]), then sets `DUDES_RUNTIME_DIR` and `DUDES_STATE_DIR`
/// explicitly, and adds nothing else. That shape is fixed,
/// and the names the daemon needs to start on each platform are added to it; none of that is
/// restated. What follows is why each name is on the list, why the rest are not, and what the
/// answer costs.
///
/// Both binaries read this one list: the CLI forwards it, and the daemon refuses an MCP
/// declaration that names one of its variables, because the daemon removes every declared name
/// from what its other children inherit.
///
/// # Why each of these crosses
///
/// - **`PATH`** — the daemon snapshots it into `Settings::predicate_path`
///   (`bins/duded/src/main.rs:365`) and resolves a **bare** predicate name against it: the
///   `search_path` branch of `predicate::resolve` (`bins/duded/src/predicate.rs:140-146`), walking
///   the handed-in value (`:165-171`). An absolute or relative name resolves without it
///   (`:136-139`), so the bare shape is the one that has no route left when `PATH` is withheld.
///   Note the size of what forwarding buys, because it is easy to overstate: the
///   *value* is still the caller's, so this is not a daemon-chosen `PATH`. Invariant 7 is
///   satisfied by the lookup happening in the daemon, which it already did; composition adds only
///   that the value arrives as one named forward with nothing riding alongside it.
/// - **`HOME`** — the daemon's fallback for both of its directories
///   (`bins/duded/src/main.rs:346-350`), and, by way of `Settings::daemon_env`, the home a vendor
///   CLI reads its own configuration and credential store from. Withholding it would move where a
///   managed child looks for its vendor credential, which is not this module's call to make.
/// - **`TMPDIR`** — the platform temporary root. A daemon that writes a temporary file writes
///   where the operator's platform puts them.
/// - **`LANG`** — locale and encoding, for the daemon and for everything it launches.
/// - **`DUDES_CONFIG`** — the operator file's path itself, which wins over the platform default
///   on every platform. Withheld, a daemon would read the platform default while the CLI wrote
///   the file the operator named. It is a path, not a credential.
/// - **`APPDATA` on Windows, `XDG_CONFIG_HOME` on other Unix systems** — the root of the operator
///   file, by `duded::config::operator_config_path`'s rule, which `dudes`' own
///   `operator_config_path` repeats for the file the CLI writes. Withheld, a Windows daemon finds no root and exits, and
///   a Linux daemon reads `~/.config` while the CLI wrote under `XDG_CONFIG_HOME`. macOS keeps the
///   file under `HOME`, which already crosses. If the daemon's config loader ever reads a name this list does not
///   carry, `tests::every_name_the_daemons_config_rule_reads_crosses` fails.
/// - **`SystemRoot` on Windows** — the directory Winsock loads its service providers from.
///   Withheld, the daemon cannot open a TCP socket, so a quickstarted daemon that serves an
///   installed Floor fails to bind its listener with os error 10106. It names a system directory
///   and every Windows process expects it.
///
/// `DUDES_RUNTIME_DIR` and `DUDES_STATE_DIR` are absent from this list because they are not
/// forwarded at all: quickstart sets them to the paths the CLI resolved, so the child cannot
/// re-derive a different pair from `HOME` than the one this process validated.
///
/// # The names that do not cross, each a decision rather than an omission
///
/// - **`ANTHROPIC_API_KEY`, and every other vendor credential** — excluded, see below.
/// - **`DUDES_TOKEN`** — an agent credential, and the daemon is not an agent (invariant 8). Were
///   it to cross it would sit in `Settings::daemon_env`, which the Claude adapter's scrub hands to
///   a managed child deliberately (`crates/dudes-providers/src/claude/managed.rs:207-210`).
///   Excluded as defence in depth, and stated at that strength and no higher: the supervisor
///   pushes the live token last when it composes a child's environment
///   (`crates/dudes-supervisor/src/machine.rs:682`) and the host applies those pairs in order onto
///   a cleared child (`crates/dudes-host/src/lib.rs:514-517`), so on the managed path an ambient
///   token is overwritten rather than used.
/// - **`DUDES_SOCK`** — consumed by `quickstart::validate_configuration`, which refuses a divergent
///   endpoint before anything is created, and forwarded nowhere. The daemon reads the name at no
///   site.
/// - **`DUDES_DAEMON`** — a CLI-only executable seam (`dudes`' `Environment::daemon`). The daemon has no
///   use for the path it was started from.
/// - **`DUDES_FAULT_STALL_BEFORE_SPAWN` and `DUDES_FAULT_STALL_MARKER`** — read only under
///   `duded`'s `fault-injection` feature (`bins/duded/src/main.rs:317-318`), which no manifest in
///   the workspace enables. Excluding them means quickstart cannot arm a stall even in a build
///   that contains the code, which is deliberate: a fault is armed by whoever built the daemon for
///   one, not by whatever an interactive session exported.
/// - **`USER`** — the Claude CLI needs it to find its keychain login, and the daemon supplies it
///   from the user database for its own uid when its environment has none (`with_user_name` in
///   `bins/duded/src/main.rs`). A value from the uid is not one from the caller.
/// - **Everything else, by construction** — `RUST_LOG` among them, and the interpreter-injection
///   names `NODE_OPTIONS`, `RUSTC_WRAPPER` and `PYTHONSTARTUP` that `PREDICATE_ENV_ALLOWLIST`
///   calls out (`bins/duded/src/predicate.rs:84-87`). That a positive list needs no growing
///   denylist is its main security property, and is why this is a list of names rather than a
///   filter.
///
/// # No vendor API key crosses by this route
///
/// The only name that can matter is `ANTHROPIC_API_KEY`
/// (`crates/dudes-providers/src/claude/config.rs:110`), and only under `isolation = "bare"`, which
/// `ClaudeConfig::check` refuses without a non-empty one (`config.rs:230-231`,
/// `ConfigError::BareWithoutApiKey`). **That refusal cannot fire in any daemon anyone can
/// currently configure** — `crates/dudes-providers/src/registry.rs:104` says so in terms, because
/// `providers_table()` hands the check a defaulted config — so excluding the key breaks no
/// reachable configuration. Against that gain of nothing, forwarding costs a live credential in
/// every agent process: the key would land in `Settings::daemon_env`, and the scrub passes it
/// **verbatim**, it being in neither nesting list (`managed.rs:58-70`). One `export` in one
/// interactive session would become a vendor credential in every managed child for the life of the
/// daemon.
///
/// This rules the ambient route and only that. Whether a *configured* key path may reach a daemon
/// is the daemon's configuration-file loader's question, and
/// that loader is not written. Nothing here closes it, and the residual below says plainly who
/// holds it, which is nobody.
///
/// # What that makes of `Settings::daemon_env` being the pre-scrub root
///
/// `Settings::daemon_env` is documented as the environment a managed child starts from, before any
/// scrub — *"so an operator's `ANTHROPIC_API_KEY` sits in it from boot"*
/// (`crates/dudes-supervisor/src/machine.rs:44-45`). Three sentences on what that now means, and
/// the second is the one a reader will otherwise get wrong.
///
/// 1. For a **quickstarted** daemon the pre-scrub root is the names above plus the `USER` the
///    daemon derives for its own uid, so the scrub can only narrow: no adapter can hand a child a
///    vendor key value, because there is none upstream to scrub.
/// 2. **That is a property of this start path, not of the daemon.** `duded` started any other way
///    — by hand, by a service manager, from an operator's session — still snapshots its whole
///    ambient environment (`bins/duded/src/main.rs:360-362`), unchanged by this list. A reader who
///    takes point 1 for a daemon-wide guarantee builds on something false.
/// 3. **"No credential value" is not "no credential reachable."** `HOME` crosses, and a vendor
///    CLI's on-disk credential store lives under it — which is why bare mode is the mode that
///    *"reads no OAuth credential and no keychain"* (`config.rs:61`). The boundary drawn here is
///    about what sits in the daemon's environment, not about what a child can open.
///
/// # Residuals, with owners rather than silence
///
/// - **`ANTHROPIC_API_KEY` by a configured path — the daemon's configuration-file loader, and
///   nothing holds it yet.** The question is answered when a daemon can be given a `[providers]`
///   table at all; until then no configuration file is named and the residual is recorded as
///   unheld, because a residual with a nominal owner reads as covered and is not.
/// - **`RUST_LOG` — nothing reads it, so there is nothing yet to hold.** The daemon installs no
///   subscriber: *"`tracing` events are emitted; where they go is the operator's build and
///   packaging"*. No logging name is read anywhere in the
///   daemon, so excluding it costs nothing today; whoever installs a subscriber inherits the
///   question of whether an operator sets the daemon's level through this route.
///
/// Adding a name here changes the fixed shape of the quickstart environment, as the platform names did, and needs a
/// design discussion first.
pub const DAEMON_ENV_ALLOWLIST: &[&str] = if cfg!(target_os = "macos") {
    &["PATH", "HOME", "TMPDIR", "LANG", "DUDES_CONFIG"]
} else if cfg!(windows) {
    &[
        "PATH",
        "HOME",
        "TMPDIR",
        "LANG",
        "DUDES_CONFIG",
        "APPDATA",
        "SystemRoot",
    ]
} else {
    &[
        "PATH",
        "HOME",
        "TMPDIR",
        "LANG",
        "DUDES_CONFIG",
        "XDG_CONFIG_HOME",
    ]
};

/// The environment a predicate child may inherit, in its own fixed list.
///
/// The fixed list for now and nothing more. Deliberately excluded, and worth naming because their absence
/// is a decision rather than an oversight: `DUDES_*`, any vendor credential the operator's
/// environment happens to hold, and the interpreter-injection variables `NODE_OPTIONS`,
/// `RUSTC_WRAPPER` and `PYTHONSTARTUP`.
pub const PREDICATE_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    // The Windows spellings of `HOME` and the two temporary-directory names above.
    "USERPROFILE",
    "TEMP",
    "TMP",
];

/// Why an adapter refused to compose a managed launch.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    /// The agent's owned path cannot be represented in the Pi extension's JSON input.
    #[error("Pi MCP servers require UTF-8 agent paths")]
    PiMcpPathEncoding,
    /// A declared program server names the agent's folder, which is not UTF-8 and so cannot be
    /// written into the engine's JSON configuration.
    #[error("a declared MCP server's `{{agent_dir}}` argument requires a UTF-8 agent folder")]
    McpAgentDirEncoding,
    /// The opt-in transport has no daemon-owned persistent profile.
    #[error("Codex streaming requires a managed profile from the running daemon")]
    ManagedProfileUnavailable,
    /// The task contains a NUL byte, which cannot be represented in an argv element.
    #[error("task contains a NUL byte at byte offset {offset}")]
    TaskContainsNul {
        /// Byte offset of the first NUL in the task.
        offset: usize,
    },
    /// The task carries no work.
    #[error("task is empty")]
    EmptyTask,
    /// Recorded repository trust and the selected Claude settings cannot safely coexist.
    /// Fixed text: replay may journal this display and must never copy configuration values.
    #[error(
        "an untrusted repository cannot contribute project settings; record repository trust at \
         hire or select settings that suppress the project source"
    )]
    UntrustedRepositorySettings,
    /// A declared MCP server the agent was opted in to cannot load in this launch. The text names
    /// servers and variables, never a value, so it may be journaled as the reason for a hold.
    #[error("declared MCP servers refused: {0}")]
    McpServer(crate::mcp::McpRefusal),
    /// The turn's session is one the adapter's
    /// [`SessionAcquisition`](crate::adapter::SessionAcquisition) cannot render: a minted id for
    /// a provider that assigns its own, or no id for a provider that pins.
    #[error("the turn's session does not match how this provider acquires its session id")]
    SessionModeMismatch,
}

/// How the host supplies one managed child's standard input.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum LaunchInput {
    /// Attach the null device.
    #[default]
    Null,
    /// Deliver the complete body and close stdin.
    WriteThenClose(Vec<u8>),
    /// Keep a bounded protocol writer open for one issued task.
    Protocol(crate::codex::startup::CodexStartup),
}

impl fmt::Debug for LaunchInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("Null"),
            Self::WriteThenClose(bytes) => {
                f.debug_tuple("WriteThenClose").field(&bytes.len()).finish()
            }
            Self::Protocol(startup) => f.debug_tuple("Protocol").field(startup).finish(),
        }
    }
}

/// A fully resolved process launch. The host execs `argv[0]` (PATH lookup happens in the
/// daemon, not in a shell), with exactly this environment and working directory.
///
/// Standard input is null, a write-then-close body, or a bounded startup protocol.
#[derive(Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Program and arguments, one element each; never a shell string.
    pub argv: Vec<OsString>,
    /// The child's complete environment, one [`EnvVar`] per variable.
    pub env: Vec<EnvVar>,
    /// The child's working directory.
    pub cwd: PathBuf,
    /// How the host delivers this child's standard input.
    pub stdin: LaunchInput,
}

impl fmt::Debug for LaunchSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchSpec")
            .field("argv", &self.argv)
            .field("env", &self.env)
            .field("cwd", &self.cwd)
            .field("stdin", &self.stdin)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_input_is_null_or_delivered_then_closed() {
        let spec = LaunchSpec {
            argv: vec!["claude".into(), "-p".into(), "--verbose".into()],
            env: vec![],
            cwd: PathBuf::from("/tmp/dudes-fixture"),
            stdin: crate::launch::LaunchInput::WriteThenClose(b"the task".to_vec()),
        };
        let how = match &spec.stdin {
            LaunchInput::Null => "null device",
            LaunchInput::WriteThenClose(bytes) if !bytes.is_empty() => "write then close",
            LaunchInput::WriteThenClose(_) => "close",
            LaunchInput::Protocol(_) => "bounded protocol",
        };
        assert_eq!(how, "write then close");
        assert!(
            !spec.argv.iter().any(|part| part == "--"),
            "a task delivered on stdin leaves no separator behind"
        );

        let null = LaunchSpec {
            stdin: crate::launch::LaunchInput::Null,
            ..spec.clone()
        };
        assert_eq!(
            match &null.stdin {
                LaunchInput::Null => "null device",
                LaunchInput::WriteThenClose(_) => "delivered",
                LaunchInput::Protocol(_) => "bounded protocol",
            },
            "null device"
        );
    }
}
