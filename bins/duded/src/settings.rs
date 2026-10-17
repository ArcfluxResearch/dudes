//! What a daemon run is configured with, and the seams it is given.
//!
//! # Two directories, not one
//!
//! The **state** directory holds the journal and the redb projection; the **runtime** directory
//! holds the control socket and the operator credential. They are separate because the credential
//! lives in the runtime directory — `bind_in` already creates and verifies that
//! directory `0700`, and `O_CREAT|O_EXCL` inside it adds no new path-resolution site — and because
//! a crash test needs them separable: a daemon that dies leaves the runtime directory dirty and the
//! state directory exactly as durable as `append` promised.
//!
//! # Nothing here reads an environment or a configuration file
//!
//! Every value arrives as a field. `main.rs` is the one place that reads the ambient environment and
//! loads operator configuration, once. That is what makes the whole daemon drivable from a test
//! with no process-wide mutation, and it is the same discipline `dudes-policy` keeps for a
//! different reason.

use std::ffi::{OsStr, OsString};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dudes_journal::JournalOptions;
use dudes_policy::BreakerConfig;
use dudes_proto::Spend;
use dudes_providers::launch::EnvVar;
use dudes_providers::registry::Adapters;
use dudes_supervisor::{Clock, ProcessHost, Rng};
use dudes_types::Usd;

use crate::predicate::PredicateRunner;
use crate::startup::Witness;

/// Startup options added without changing existing [`Settings`] literals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOptions {
    chat_key_ignored: bool,
    providers: crate::config::ProviderTables,
    mcp_servers: dudes_providers::mcp::DeclaredServers,
    #[cfg(feature = "fault-injection")]
    recovery_stop: Option<crate::fault::RecoveryStop>,
}

impl RunOptions {
    /// Retain a native recovery stop in an explicitly instrumented build.
    #[cfg(feature = "fault-injection")]
    #[must_use]
    pub fn with_recovery_stop(mut self, stop: Option<crate::fault::RecoveryStop>) -> Self {
        self.recovery_stop = stop;
        self
    }

    /// Native recovery stop selected before boot, absent in ordinary builds.
    #[cfg(feature = "fault-injection")]
    #[must_use]
    pub const fn recovery_stop(&self) -> Option<&crate::fault::RecoveryStop> {
        self.recovery_stop.as_ref()
    }

    /// Record that the operator file still carries a `[floor.chat]` table, which the daemon
    /// ignores because Chat is always on.
    #[must_use]
    pub const fn with_ignored_chat_key(mut self) -> Self {
        self.chat_key_ignored = true;
        self
    }

    /// Retain the provider tables used to construct this daemon's adapters.
    #[must_use]
    pub fn with_providers(mut self, providers: crate::config::ProviderTables) -> Self {
        self.providers = providers;
        self
    }

    pub(crate) fn into_providers(self) -> crate::config::ProviderTables {
        self.providers
    }

    /// Retain the `[mcp.servers]` table this daemon reads at start.
    #[must_use]
    pub fn with_mcp_servers(mut self, servers: dudes_providers::mcp::DeclaredServers) -> Self {
        self.mcp_servers = servers;
        self
    }

    pub(crate) fn mcp_servers(&self) -> &dudes_providers::mcp::DeclaredServers {
        &self.mcp_servers
    }

    /// Whether the operator file carried a `[floor.chat]` table this run ignores.
    #[must_use]
    pub const fn chat_key_ignored(&self) -> bool {
        self.chat_key_ignored
    }
}

/// The Floor bundle installed with the `duded` at `exe`: a `floor` directory beside it, where
/// `cargo xtask dist` puts one, or else, when `exe` sits in a directory named `bin`,
/// `share/dudes/floor` under that directory's parent, where `install.sh` puts one. `None` when
/// neither directory exists.
///
/// # Errors
///
/// On Unix, when any directory from the prefix through `share/dudes/floor` is owned by neither
/// `user`, the uid this process runs as, nor root, or when its group or others can write it:
/// another local user could replace the Floor between lookup and admission. Elsewhere `user` is
/// unused.
#[doc(hidden)]
#[cfg_attr(
    not(unix),
    expect(
        clippy::unnecessary_wraps,
        reason = "only Unix has an owner and a mode to refuse"
    )
)]
pub fn installed_floor(exe: &Path, user: u32) -> anyhow::Result<Option<PathBuf>> {
    let beside = exe.with_file_name("floor");
    if beside.is_dir() {
        return Ok(Some(beside));
    }
    let prefix = exe
        .parent()
        .filter(|dir| dir.file_name() == Some(OsStr::new("bin")))
        .and_then(Path::parent);
    let Some(prefix) = prefix else {
        return Ok(None);
    };
    let shared = prefix.join("share").join("dudes").join("floor");
    if !shared.is_dir() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        for path in [
            shared.clone(),
            prefix.join("share/dudes"),
            prefix.join("share"),
            prefix.to_path_buf(),
        ] {
            let metadata = std::fs::metadata(&path)?;
            let what = if path == shared {
                "Floor bundle"
            } else {
                "Floor ancestor"
            };
            anyhow::ensure!(
                metadata.uid() == user || metadata.uid() == 0,
                "the {what} {} is owned by uid {}, not by this user or root, so another user \
                 could replace the Floor; remove it and reinstall, or set DUDES_FLOOR_ASSETS",
                path.display(),
                metadata.uid()
            );
            anyhow::ensure!(
                metadata.mode() & 0o022 == 0,
                "the {what} {} is writable by its group or others (mode {:04o}); run \
                 `chmod go-w` on it",
                path.display(),
                metadata.mode() & 0o7777
            );
        }
    }
    #[cfg(not(unix))]
    let _ = user;
    Ok(Some(shared))
}

/// Parse the two local Floor environment values into typed settings.
///
/// `bundled` is the directory [`installed_floor`] found. It serves only when
/// `DUDES_FLOOR_ASSETS` is unset and the directory exists, and it passes the same release-manifest
/// admission as an explicit directory.
///
/// `main.rs` is the ambient reader; this function accepts values so tests need no process-wide
/// environment mutation.
#[doc(hidden)]
pub fn floor_from_values(
    assets: Option<OsString>,
    bundled: Option<PathBuf>,
    port: Option<OsString>,
) -> anyhow::Result<Option<FloorSettings>> {
    let assets_dir = if let Some(assets) = assets {
        let assets_dir = PathBuf::from(assets);
        anyhow::ensure!(
            assets_dir.is_absolute(),
            "DUDES_FLOOR_ASSETS must be a non-empty absolute directory"
        );
        anyhow::ensure!(
            assets_dir.is_dir(),
            "DUDES_FLOOR_ASSETS must name an existing directory"
        );
        assets_dir
    } else if let Some(bundled) = bundled.filter(|dir| dir.is_dir()) {
        bundled
    } else {
        anyhow::ensure!(
            port.is_none(),
            "DUDES_FLOOR_PORT requires a Floor: install it as a `floor` directory beside duded \
             or as `share/dudes/floor` beside duded's `bin` directory, or set DUDES_FLOOR_ASSETS"
        );
        return Ok(None);
    };
    let port = port
        .map(|value| {
            let value = value
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("DUDES_FLOOR_PORT must be ASCII decimal"))?;
            anyhow::ensure!(
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
                "DUDES_FLOOR_PORT must be ASCII decimal"
            );
            value
                .parse::<u16>()
                .map_err(|_| anyhow::anyhow!("DUDES_FLOOR_PORT must fit u16"))
        })
        .transpose()?;
    Ok(Some(FloorSettings { assets_dir, port }))
}

/// The name the control socket takes inside the runtime directory.
pub const SOCKET_NAME: &str = "duded.sock";

/// How long a beat is: one `Input::Tick` per interval, and one socket-identity re-comparison.
pub const DEFAULT_BEAT: Duration = Duration::from_secs(30);

/// How many connections the accept loop serves at once.
///
/// A cap rather than none, because `serve` spawns a thread per connection and an unbounded accept
/// loop is a thread bomb reachable by anything that can connect.
pub const DEFAULT_MAX_CONNECTIONS: usize = 64;

/// The local static Floor listener's typed configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FloorSettings {
    /// Absolute directory containing `release-manifest.json` and its admitted files.
    pub assets_dir: PathBuf,
    /// `DUDES_FLOOR_PORT`: the TCP port on `127.0.0.1`, where zero asks the operating system to
    /// choose one. `None` asks first for the port the last run bound, kept in the state directory.
    pub port: Option<u16>,
}

/// Everything a daemon run needs that is not a seam.
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each mirrors one independent `[floor.*]` or `[memory]` operator switch"
)]
pub struct Settings {
    /// Journal and projection. The single-writer lock is taken here.
    pub state_dir: PathBuf,
    /// Control socket and operator credential.
    pub runtime_dir: PathBuf,
    /// The socket's file name inside [`Settings::runtime_dir`].
    pub socket_name: String,
    /// The journal's sync policy and rotation size.
    pub journal: JournalOptions,
    /// `[budget] floor_usd_per_day` (budget gate), absent when the operator configured no floor
    /// ceiling.
    pub floor_usd_per_day: Option<Usd>,
    /// `[budget].per_turn_floor`, each unset unit defaulting to
    /// [`Spend::DEFAULT_PER_TURN_FLOOR`]'s.
    pub per_turn_floor: Spend,
    /// Startup-loaded profiles available to the finite browser onboarding action.
    pub enrollment: crate::floor_actions::Enrollment,
    /// The operator file `enrollment` was loaded from, and the only file
    /// `floor.enrollment.reload` reads. `None` refuses every reload.
    pub operator_config: Option<PathBuf>,
    /// `[floor.answer] enabled`: the Floor's answer route and its `answer` snapshot sibling
    /// exist only when this is true.
    pub floor_answer_enabled: bool,
    /// [`crate::config::OperatorConfig::hire_enabled`].
    pub floor_hire_enabled: bool,
    /// `[floor.control]`, `[floor.goal]`, `[floor.retire]` and `[floor.renew]`: which of the
    /// Floor's loop-control routes and snapshot siblings exist.
    pub floor_control: FloorControl,
    /// [`crate::config::OperatorConfig::recovery_enabled`].
    pub floor_recovery_enabled: bool,
    /// [`crate::config::OperatorConfig::board_enabled`].
    pub floor_board_enabled: bool,
    /// [`crate::config::OperatorConfig::terminal_enabled`].
    pub floor_terminal_enabled: bool,
    /// [`crate::config::OperatorConfig::git_enabled`].
    pub floor_git_enabled: bool,
    /// [`crate::config::OperatorConfig::diff_enabled`].
    pub floor_diff_enabled: bool,
    /// [`crate::config::OperatorConfig::condense_model_enabled`].
    pub condense_model_enabled: bool,
    /// [`crate::config::OperatorConfig::condense_model`].
    pub condense_model: Option<String>,
    /// `[breaker]`: whether the ladder may reach `Stop`.
    pub breaker: BreakerConfig,
    /// The environment a **managed child** starts from, before the adapter scrubs it. Carries the
    /// operator's vendor credentials, which is why every value is an [`EnvVar`].
    pub daemon_env: Vec<EnvVar>,
    /// The environment a **predicate child** is given, whole. Built by
    /// [`crate::predicate::allowlisted`] and never inherited.
    pub predicate_env: Vec<EnvVar>,
    /// The `PATH` a predicate's `argv[0]` is resolved against. Separate from
    /// [`Settings::predicate_env`] so that resolution cannot silently follow a `PATH` the child was
    /// never given.
    pub predicate_path: Option<OsString>,
    /// How long `git worktree add` may run before its child is killed.
    pub git_checkout_deadline: Duration,
    /// The accept loop's cap.
    pub max_connections: NonZeroUsize,
    /// The beat.
    pub beat: Duration,
    /// The optional static Floor shell. Absence means no TCP listener.
    pub floor: Option<FloorSettings>,
    /// The absolute path of `dudes-hook`, which an attached CLI's hook settings run. `None`, or a
    /// path that is not UTF-8 or names no regular file, refuses the attach.
    pub hook_command: Option<PathBuf>,
    /// The daemon's own executable. The `dudes` file beside it is what the one-entry `agent-bin`
    /// directory links to; `None` leaves agents without that directory.
    pub daemon_exe: Option<PathBuf>,
}

impl Settings {
    /// The control endpoint the daemon binds: the socket's path on Unix, the pipe name on Windows.
    #[must_use]
    pub fn socket(&self) -> PathBuf {
        dudes_rpc::local_endpoint(&self.runtime_dir, &self.socket_name)
    }
}

/// The non-deterministic parts, injected.
///
/// **Every one of these is a trait or a `dyn`, and that is the point of the type.** The code alone cannot
/// establish that the daemon reaches the host *through the trait* — a
/// daemon holding a concrete `RealHost` and calling the inherent method would satisfy invariant 10
/// while making the trait change pointless — so the daemon is generic over `H: ProcessHost` and
/// `main.rs` is the only file that names a concrete one. The same argument
/// applies to [`PredicateRunner`].
pub struct Seams<H: ProcessHost, P: PredicateRunner, C: Clock, R: Rng> {
    /// The process side of the runtime: agent children.
    pub host: H,
    /// The daemon-owned attached PTY host behind a neutral port.
    pub pty_host: Box<dyn crate::ports::PtyHostPort>,
    /// The one-shot side: goal-predicate children. Behind an `Arc` because the exit path and the
    /// predicate thread hold it at once — the mechanism that keeps every child owned.
    pub predicates: Arc<P>,
    /// The daemon's wall clock, the single source of time. Behind an `Arc` because
    /// the machine and the runtime need the *same* reading: the day a spend is bucketed into and
    /// the day the `TurnEnded` beside it is stamped with must be one value.
    pub clock: Arc<C>,
    /// Ids and agent tokens.
    pub rng: R,
    /// Which agent's tree a connecting peer is in — the peer-classification table's **column**.
    ///
    /// Not the authorizer: `boot` builds that itself, from the credential it mints and the token
    /// projection the loop publishes, because both come into existence inside `boot` and neither
    /// can be handed in. What a caller chooses here is one input to it.
    ///
    /// # Why this remains a seam
    ///
    /// Production constructs a host-owned process relation on Linux, macOS and Windows and the
    /// explicit [`crate::classify::NoWalk`] fallback elsewhere. The field also lets a **test**
    /// select a peer-classification column without replacing the authorizer.
    ///
    /// **What an injected resolver can and cannot do.** It chooses a column, never a row.
    /// `PeerClassifier::row` reads only what the call presents, this daemon's credential and the
    /// token projection, so a resolver moves a call *along* a row: rows 3 and 4 are column
    /// wildcards, and in the three rows where the column decides, the peer has already presented a
    /// credential the daemon itself recognised. An `Authorizer` was under none of those
    /// constraints, which is why that seam is gone and this one is not a hole.
    ///
    /// **Two conditions under which the paragraph above stops holding, and both must be watched.**
    ///
    /// - **The day `classify` gives row 3 or row 4 a column-dependent verdict.** Those two rows are
    ///   what "never a row" rests on: while each answers the same thing in all three columns, an
    ///   injected resolver cannot move a peer that presented nothing, or presented an unrecognised
    ///   token, anywhere at all. A verdict there that reads the column ends that, and this seam
    ///   becomes a way to promote a peer that proved nothing. **Not fired**: `classify`'s row 3 arm
    ///   is `(Row::UnmintedToken, _)` and its row 4 arm is `(Row::Nothing, _)`, both column
    ///   wildcards. **Re-read for retired agents' recorded children, and it holds.** The row split out of
    ///   row 3 for retired agents reads the column, but like row 2 it applies only to a token this daemon minted, so
    ///   an injected resolver still chooses a column and never a row.
    /// - **The day a production registration admits `Requirement::Agent` or
    ///   `Requirement::OperatorOrAgent`.** An injected column then becomes real capability for any
    ///   peer holding a live token.
    ///
    ///   **Already the case for `agent.ask`, registered `Requirement::Agent` in
    ///   `register_all`.** The seam is safe for a *narrower* reason than the column alone.
    ///
    ///   `Agent` also reaches `memory.read`, `board.claim` and `board.transition`, while
    ///   `Unproven` reaches only `system.ping`. So an injected column has a real consequence.
    ///
    ///   What holds is that this seam chooses a column and never a row. It cannot promote a peer
    ///   that presented nothing or an unrecognised token: reaching an Agent row requires both a
    ///   token this daemon minted and the host relation naming that same agent.
    ///
    ///   **Likewise for `memory.append`:** its production registration is
    ///   `Requirement::OperatorOrAgent`. The safety property still holds: the
    ///   conditioned decoder derives an agent caller's target from the authenticated role and
    ///   rejects any caller-supplied target, while an operator still has to present the daemon's
    ///   independent credential. An injected column can therefore expose only the capability of a
    ///   token the daemon minted, and that capability reaches only that same agent's memory.
    ///
    ///   **Likewise for `hook.event`:** its production registration is `Requirement::Agent`.
    ///   The handler takes the agent only from the resolved `Call::role`; payload `agent_id` cannot
    ///   choose a target, so an injected column exposes only the minted token's own supervisor hook
    ///   input and reply.
    ///
    ///   **What would end that property:** an `Agent` or
    ///   `OperatorOrAgent` effect whose target can reach beyond the authenticated calling agent.
    ///   `tests/routed_methods.rs`'s `the_agent_reachable_methods_are_exactly_seven` fails when a
    ///   method joins the set an agent can call, which the retirement handling also rests on.
    ///
    /// Either one means this field's safety argument must be re-examined rather than assumed.
    /// Concrete rather than `Arc<dyn PeerResolver>`, because the daemon needs three things from
    /// it that the trait does not carry: the publication barrier armed around each spawn, the
    /// child-owner snapshot republished at pump quiescence, and the column itself. A trait object
    /// would give only the third, and the walk would then resolve against a snapshot nothing ever
    /// filled — answering `NoRecordedAgent` for every peer, which is a false "walked cleanly to no
    /// match" and the cell that admits an operator credential.
    /// [`crate::classify::Resolver::fixed`] is how a fixed-column test or unsupported target
    /// supplies a column without a process relation.
    pub resolver: crate::classify::Resolver,
    /// This run's adapters, built once from the operator's `[providers]` table.
    ///
    /// A field rather than something `boot` builds, for the reason every other seam here is one:
    /// `main.rs` is the only file that reads the ambient environment, and provider configuration
    /// validation needs it — the daemon environment a managed child starts from is what decides
    /// whether an `isolation = "bare"` table is honourable at all.
    ///
    /// **A provider absent here cannot be hired**, not merely never launched: `Ask::Hire` refuses
    /// on it before the record is built. Otherwise the agent would be
    /// journaled and then sit on the floor unable to take a turn for ever.
    pub adapters: Adapters,
    /// Where every managed child's standard output ends up, and the only production constructor
    /// of `Input::ChildExited`.
    ///
    /// A field rather than something `boot` makes for itself, because the host is built before the
    /// daemon is and has to be handed the same value: the host writes lines into it and the loop
    /// registers turns against it. `boot` gives it the way back into the loop as soon as there is
    /// one.
    pub children: std::sync::Arc<crate::children::ChildEnds>,
    /// Where the startup and teardown orders are recorded.
    pub witness: Witness,
}

/// Which of the Floor's control actions the operator enabled, fixed for the run. `[floor.control]`
/// enables pause and resume, and also the display rename, which is not a
/// loop-control action and needs nothing from pause. Goal change and retirement exist only beside
/// pause and resume: Pause opens the window a change needs on a busy agent, and it
/// is the reversible stop a retirement's confirmation offers. So no value enables
/// either alone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FloorControl {
    /// No route and no sibling for any of them.
    #[default]
    Off,
    /// `[floor.control] enabled`: pause and resume, rename, and the actions
    /// that require pause.
    On {
        /// `[floor.goal] enabled`: goal change.
        goal: bool,
        /// `[floor.retire] enabled`: retirement.
        retire: bool,
        /// `[floor.renew] enabled`: renewal of a goal that has nothing left. It counts only beside
        /// goal change, and the operator file is refused at load without goal change and Hire.
        renew: bool,
    },
}

impl FloorControl {
    /// Whether pause, resume and rename are routed and snapshots carry the `control` sibling.
    #[must_use]
    pub const fn pause_resume(self) -> bool {
        matches!(self, Self::On { .. })
    }

    /// Whether goal change is routed and snapshots carry the `goal` sibling.
    #[must_use]
    pub const fn goal(self) -> bool {
        matches!(self, Self::On { goal: true, .. })
    }

    /// Whether retirement is routed and snapshots carry the `retire` sibling.
    #[must_use]
    pub const fn retire(self) -> bool {
        matches!(self, Self::On { retire: true, .. })
    }

    /// Whether renewal is routed and the `goal` sibling carries its `renew` member.
    #[must_use]
    pub const fn renew(self) -> bool {
        matches!(
            self,
            Self::On {
                goal: true,
                renew: true,
                ..
            }
        )
    }
}
