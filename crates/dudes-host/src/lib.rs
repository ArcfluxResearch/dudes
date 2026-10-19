//! The real [`ProcessHost`], plus the confined Windows halt capability injected into the local
//! transport.
//!
//! # Why this is a crate and not a module
//!
//! Two reasons, and the first one is not a preference. `[workspace.lints.rust]` sets
//! `unsafe_code = "forbid"`, and `forbid` — unlike `deny` — cannot be re-levelled from source, so
//! a crate that inherits the workspace lints table can hold no `unsafe` at all, not even in one
//! `#[allow]`-ed module. The Windows start value needs three functions no crate exposes safely, so
//! it needs a crate with its own `[lints]` table. The second reason is layering: `dudes-supervisor`
//! is sans-io and its layering rule exists to keep it that way, so the io-bound half of the host
//! seam cannot live beside the trait. This crate depends on `dudes-supervisor`; the edge never runs
//! the other way, and `cargo xtask layering` is what says so.
//!
//! # The process-ownership rule
//!
//! > An adoption key answers *who a process was*. A signal needs *who this identifier is now*.
//! > Only something that spans the two may authorise a signal, and a bare existence probe cannot:
//! > the stranger exists.
//!
//! The named-pipe disconnect leaf is separate from that process rule. It lives here because this
//! crate already owns the confined Windows `unsafe` module, while the daemon can inject the safe
//! function into `dudes-rpc` without reversing the dependency edge.
//!
//! **Two things span it, and the host uses whichever one it has.** They are not alternatives of
//! equal strength — the first is stronger — and `src/kill.rs`'s module docs are where the choice
//! is argued in full.
//!
//! - **The reservation.** A process identifier is freed at the *reap*, not at the exit, so a
//!   parent holding an unreaped child owns that number outright and nothing else can be given it.
//!   Where the host has that, a signal *cannot* reach a stranger, and no comparison can improve
//!   on it. So a child this host spawned and has not reaped is killed on the reservation, which
//!   [`RealHost::shutdown`] and [`RealHost::kill`] both do. The
//!   key is still compared once on that path, to answer a different question — *which* child the
//!   caller meant — because a host that reaped one child and spawned another at the same number
//!   must not let a stale kill for the first take the second down. That comparison is of the
//!   **whole** key, boot identity included: on Linux the start value is boot-relative, so a key
//!   replayed from the journal after a reboot can otherwise carry the same pid and start value as
//!   a live child of this run.
//!
//!   It also means the host must own every agent child directly: a child launched through an
//!   async runtime that reaps on its own schedule has no reservation at all.
//!
//! - **The key comparison.** Where the host holds no child — a key adopted from the journal after
//!   a restart, or one naming a child this run has already reaped — there is nothing to lean on,
//!   so [`RealHost::kill`] re-reads the identifier's start value, runs it through [`liveness`]
//!   immediately before **every** signal it sends, the grace escalation's second signal included,
//!   and refuses to signal anything that does not answer [`Liveness::Alive`].
//!
//!   Under that comparison sits a narrowing of its own: the host takes the strongest reference the
//!   platform offers *before* it compares — a pidfd on Linux, a process handle on Windows — so the
//!   comparison and the signal cannot be separated by a recycle. macOS offers no equivalent, so
//!   its window is narrowed to the two adjacent statements between the probe and the signal and is
//!   named rather than closed.
//!
//! The second case is the one the key comparison leaves uncovered and this crate was written for: the
//! reservation cannot cover a child that exited, was reaped, and had a kill decided for it
//! afterwards, and there the refusal is the whole guarantee.
//!
//! # Platform layers
//!
//! [`linux`], [`macos`] and [`windows`] each answer the same four questions — this host's boot
//! identity, one identifier's start value, how to hold an identifier still, and how to signal it —
//! and hold no classification of their own: they return [`ProbeAnswer`] and [`liveness`] decides.
//! Only `windows` contains `unsafe`, and `tests/unsafe_grant.rs` is what fails if that stops being
//! true — **of the files under `src/`**, which is the whole of what that test walks. DACL and
//! filesystem operations live in the separate `dudes-win` leaf, so this crate keeps only process
//! ownership. A test file
//! may re-level the crate's `deny` from its own source and compile; the grant scan would not see
//! it, and saying `src/` is what keeps the claim the size of its evidence.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use dudes_providers::adapter::{HookInstall, ManagedInstall};
use dudes_providers::launch::LaunchSpec;
use dudes_supervisor::host::{
    AdoptionKey, HostBoot, KillOutcome, Liveness, MAX_OWNED_TARGET_QUERY_ROWS,
    NativeBoundaryAcknowledgement, NativeBoundaryReceipt, NativeGroupPresence, OwnedTargetEvidence,
    OwnedTargetState, OwnedTargets, OwnedTargetsStatus, ProbeAnswer, ProcessHost, SpawnResult,
    liveness,
};

use crate::stream::ExitSlot;

#[cfg(any(target_os = "linux", test))]
mod procfs_view;

mod kill;
/// The platform-neutral half of the peer walk. Built under `test` on every platform so its own
/// scripted-source arms run everywhere; supported desktop platforms provide a
/// [`Source`](peer::Source) for it.
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
mod peer;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix_peer;

/// The test-only latch that holds one child's reap open across a `kill`, so the window in which a group sweep can answer `EPERM`
/// is about can be entered on purpose instead of waited for. Its own documentation says why the
/// release has to come from the production presence query rather than from the test.
///
/// Two uses, one latch. Holding the reap **open** until the production presence query walks into
/// that window is Darwin-only, because only there is that query's answer a measured fact. Holding
/// it **closed** until a named step of a cleanup has run is platform-neutral, because it waits for
/// a statement in this crate rather than for a kernel's answer. The module is built under every
/// test configuration and its Darwin-only half is gated inside it.
#[cfg(test)]
mod reap_hold;

#[cfg(unix)]
mod keeper;
#[cfg(unix)]
mod sigmask;
#[cfg(unix)]
mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as plat;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as plat;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as plat;

#[cfg(windows)]
use plat::native_boundary_state;
#[cfg(not(windows))]
use plat::target_state as native_boundary_state;

pub mod oneshot;
mod protocol;
pub mod stream;
pub use protocol::ProtocolWriter;

pub use kill::DEFAULT_GRACE;
pub use oneshot::{
    CAPTURED_ADDRESS_SPACE_BYTES, CapturedOutcome, OUTPUT_TAIL_BYTES, OneShot, OneShotOutcome,
};
pub use stream::{ChildExit, ChildStream, ChildStreams, MAX_LINE_BYTES};

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use unix_peer::{ExternalChild, PeerProcesses, PeerRelation};
#[cfg(windows)]
pub use windows::disconnect_named_pipe;
#[cfg(windows)]
pub use windows::{ExternalChild, PeerProcesses, PeerRelation};

/// The non-Windows half of the named-pipe halt injection.
///
/// Exported under the same name as the Windows capability so the daemon can pass it to the local
/// transport without carrying a transport `cfg`. Unix does not call it: its socket halt uses
/// `shutdown(2)` over a duplicate descriptor.
///
/// # Errors
///
/// Always [`io::ErrorKind::Unsupported`] outside Windows.
#[cfg(not(windows))]
pub fn disconnect_named_pipe(_handle: ()) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a Windows named pipe is unavailable on this platform",
    ))
}

/// This host's boot identity, and the *reason* when there is none.
///
/// [`AdoptionKey::boot`] is `Some` only where the platform's start value is boot-relative, and
/// `None` there asserts "wall-clock-anchored" — never "boot-relative but unreadable".
/// Those are two different facts and collapsing them into `Option<String>` at
/// construction is how a host silently answers [`Liveness::Unknown`] for every adopted child on a
/// machine with an unreadable `/proc`. This type keeps them apart so that the failure is visible
/// at construction and a policy for it can be chosen without rewriting the host.
#[derive(Debug)]
pub enum BootIdentity {
    /// This platform's start values are captured at process creation and mean the same thing
    /// across a reboot, so keys carry no boot identity. macOS and Windows.
    NotApplicable,
    /// Read once, this run. Linux.
    Read(String),
    /// A boot-relative platform whose read failed. Every key carrying a boot identity answers
    /// [`Liveness::Unknown`] against this host — no evidence, rather than a wrong `Gone`.
    Unreadable(io::Error),
}

impl BootIdentity {
    /// What [`liveness`] takes as `host_boot`: the identity if this host has one, `None`
    /// otherwise — and `None` for both of the other two variants, which is exactly why they are
    /// kept apart *here* rather than there.
    #[must_use]
    pub fn as_host_boot(&self) -> Option<&str> {
        match self {
            Self::Read(boot) => Some(boot.as_str()),
            Self::NotApplicable | Self::Unreadable(_) => None,
        }
    }
}

/// This host's boot identity as the seam reports it, which is this type with the `io::Error`
/// rendered.
///
/// **A twin rather than a move, and a twin owes a total mapping**. [`HostBoot`] is a
/// wire type: it crosses a process boundary in a `system.doctor` reply, so it cannot carry an
/// `io::Error`, and it is declared in `dudes-types` where a contract type may live. This one keeps
/// the error because [`ProcessHost::spawn`] refuses on it and the refusal reproduces its
/// `ErrorKind`. So the two exist for different reasons, and this is the one place either becomes
/// the other — three variants to three, with `tests::the_mapping_to_the_seam_is_total` covering
/// each arm, because the mapping that matters is the one nothing on a healthy machine executes.
impl From<&BootIdentity> for HostBoot {
    fn from(boot: &BootIdentity) -> Self {
        match boot {
            BootIdentity::NotApplicable => Self::NotApplicable,
            BootIdentity::Read(boot) => Self::Read(boot.clone()),
            // Rendered, not dropped. `Unreadable` is the state in which every adopted turn of this
            // run is stuck for good, and the platform's own words are the only clue to why.
            BootIdentity::Unreadable(error) => Self::Unreadable(error.to_string()),
        }
    }
}

/// The hold a platform layer could take on a child that has just been created, and the failure
/// that weakened it.
///
/// Not `io::Result<Grip>`, and the difference is invariant 10. A `Result` makes the caller choose
/// between a grip and an error at a point where the process **already exists**, so the error arm
/// has no grip to record, the `Child` is dropped, and a running process is left with nothing
/// holding it. This type says the two things that are actually true after a hold came out weak:
/// here is the strongest grip the platform could still give, and here is what went wrong.
pub(crate) struct Spawned {
    /// Degraded rather than absent when `weakened` is `Some`. On Windows a child that could not be
    /// assigned to its job is still reachable through its process handle, and a grip carrying that
    /// handle is what lets [`RealHost::shutdown`] reach it.
    pub(crate) grip: plat::Grip,
    /// The strongest target boundary this grip actually acquired.
    pub(crate) boundary: kill::Boundary,
    /// What stopped the hold being the full one, if anything.
    pub(crate) weakened: Option<io::Error>,
}

/// One child this host spawned and has not reaped.
///
/// Looked up by pid alone, and that is sound for exactly one reason: an unreaped child's
/// identifier cannot be reused, so a pid this map holds names the process this host spawned and
/// nothing else. The moment the child is reaped, that stops being true and the key comparison
/// becomes the only guarantee left — which is why the entry stays in the map afterwards and
/// [`ExitSlot::reaped`] is what the kill path consults, rather than the entry's presence.
struct Owned {
    /// The child's identifier, kept here because the [`Child`] that could answer for it
    /// belongs to the watcher thread.
    pid: u32,
    /// How this host learns the child has been reaped, and by whom it is reaped.
    exit: ExitSlot,
    /// What the platform gives the host for holding this identifier still and signalling it.
    grip: plat::Grip,
    /// The strongest boundary acquired for this child, kept with the capabilities it describes.
    boundary: kill::Boundary,
    /// The key read at spawn: what a caller-supplied key is compared against to work out *which*
    /// child a kill meant. `None` when the start-value read failed after the exec, and then no
    /// individual kill can select this child. Shutdown signals it while the watcher still holds
    /// the reservation; after reap, the per-signal authority check withholds any later numeric signal rather than guessing.
    key: Option<AdoptionKey>,
    cwd: PathBuf,
    reap_confirmed: Arc<AtomicBool>,
    native_receipt: NativeBoundaryReceipt,
    protocol: Option<ProtocolWriter>,
    /// The write end of this agent's keeper pipe, held exactly as long as this host owns the agent.
    /// Dropping it ends the agent's process group at once, so nothing that leaves the agent running
    /// may drop it.
    #[cfg(unix)]
    #[expect(dead_code, reason = "held for its Drop, which is what ends the group")]
    lifeline: Option<keeper::Lifeline>,
}

struct CurrentNativeTarget {
    key: Option<AdoptionKey>,
    cwd: PathBuf,
    receipt: NativeBoundaryReceipt,
    absence_observed: bool,
}

impl Drop for Owned {
    fn drop(&mut self) {
        if let Some(protocol) = &self.protocol {
            protocol.fail();
        }
    }
}

/// The real host: it owns every agent child, reads adoption keys, probes the process table, and
/// kills.
///
/// Not `Clone` and not `Sync`: it owns process handles, and a second copy would be a second
/// reaper. There is one of these per daemon.
///
/// **The `Sync` half of that sentence was an assertion until the watcher thread owned the child and is a fact now**, which is
/// worth recording because it is a breaking change to this crate's public API and because the
/// sentence read the same before and after. `Owned` holds each child's exit through an
/// `mpsc::Receiver`, and a `Receiver` is `Send` but not `Sync`, so the auto trait no longer
/// applies; before, nothing in the struct withheld it and the type *was* `Sync` in defiance of
/// this comment. `cargo semver-checks` reports it as `auto_trait_impl_removed`
/// ("type `RealHost` is no longer `Sync`") — and nothing in the workspace needed the impl: the daemon moves its host into one thread and shares it with
/// none.
pub struct RealHost {
    boot: BootIdentity,
    pid_namespace: Option<String>,
    children: BTreeMap<u32, Owned>,
    current_native_targets: Mutex<BTreeMap<u32, CurrentNativeTarget>>,
    /// The process relation this host maintains for its children, shared with the local door.
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    peers: PeerProcesses,
    grace: Duration,
    /// The executable that is started as a keeper beside each agent, when this host keeps agents
    /// alive only as long as the daemon. `None` starts no keeper.
    #[cfg(unix)]
    keeper_executable: Option<PathBuf>,
    /// Where every agent child's standard output goes, when this host was given somewhere to send
    /// it. `None` leaves stdout inherited and opens no pipe at all — which is what
    /// [`RealHost::new`] does and what every host did before there was a sink, so a host with no
    /// consumer for a stream never creates one nobody reads.
    streams: Option<Arc<dyn stream::ChildStreams>>,
    #[cfg(test)]
    setup_fault: Option<SetupFault>,
    /// The controlled producer matrix's cleanup evidence. Production has no such input; this field
    /// and its adapters do not exist outside crate tests.
    #[cfg(test)]
    cleanup_evidence_for_test: Option<kill::TestCleanupEvidence>,
    /// What the last failed-spawn cleanup on this host answered, as its own words.
    ///
    /// The spawn result cannot carry it: `reason` is reserved for the
    /// original setup error, deliberately, so a caller never parses cleanup detail out of it. That
    /// leaves a test with two different cleanup paths behind one `CleanupUnconfirmed`, and a red
    /// transcript that names neither. This field is the test-only way back to the answer, and
    /// nothing reads it in production because nothing outside `cfg(test)` can see it.
    #[cfg(test)]
    cleanup_answer_for_test: Option<String>,
    /// Holds this host's next child unreaped until the cleanup it drives has sent its first signal.
    ///
    /// The producer matrix's confirmed cells all depend on cleanup finding the child unreaped, and
    /// before this that was an assumption about a race rather than a fact: `stream::settle`'s reap
    /// publication runs against the `exit.reaped()` that `kill::cleanup_target` hands
    /// `kill::failed_spawn_guard`, and when the reap won, the cell's expectation was wrong through
    /// no fault of the code. Arming this makes the ordering the only one available.
    ///
    /// Released from `kill::TestCleanupTarget::signal` rather than held across the whole cleanup:
    /// the confirmed path ends in `exit.wait()`, which cannot return until the watcher reaps, so a
    /// hold that outlived the signal would deadlock the thing it is trying to make deterministic.
    #[cfg(test)]
    hold_reap_until_cleanup_signals: Option<reap_hold::Hold>,
}

#[cfg(test)]
#[derive(Clone)]
enum SetupFault {
    WriterCreation,
    WriterWrite,
    WriterPanic,
    KeyRead,
    /// [`SetupFault::KeyRead`], with the watcher's reap forced to land **before** cleanup runs.
    ///
    /// The ordering is the whole of it. A failed spawn whose key read failed has no key, so once
    /// its child is reaped `kill::failed_spawn_guard` has neither of the two authorities it can
    /// use and answers `None` — the case *reaped without a key sends
    /// no signal*. In the wild that ordering happens when the child exits before cleanup reaches
    /// it, which is a race nothing in a test controls; this fault makes it the only possible
    /// ordering, so the case can be asserted instead of waited for.
    KeyReadAfterReap,
    MissingBoundary(PathBuf),
    /// Hold this child's reap open until the presence query the kill path runs answers `EPERM`,
    /// so a `kill` can be driven against the host's own unreaped zombie on purpose.
    #[cfg(target_os = "macos")]
    HoldReap(reap_hold::Hold),
}

impl std::fmt::Debug for RealHost {
    /// Hand-written because the sink is a `dyn` seam with no `Debug` of its own, and because what
    /// a reader of a trace wants from a host is how many children it owns and whether it is
    /// watching their streams — not the address of a trait object.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut host = f.debug_struct("RealHost");
        host.field("boot", &self.boot)
            .field("children", &self.children);
        // A count, never the relation itself: it holds job and process handles whose numeric
        // values say nothing a reader of a trace wants.
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        host.field("peer_children", &self.peers.registered());
        host.field("grace", &self.grace)
            .field("watching", &self.streams.is_some())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Owned {
    /// Non-exhaustive on purpose: the platform grip holds process and job handles whose numeric
    /// values say nothing a reader of a trace wants, and printing them invites treating a handle
    /// as an identity — which is the mistake this whole crate exists to avoid.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Owned")
            .field("pid", &self.pid)
            .field("has_key", &self.key.is_some())
            .field("boundary", &self.boundary)
            .field("exit", &self.exit)
            .finish_non_exhaustive()
    }
}

// --- the modes on the agent's own directory ------------------------------------

/// The mode a directory the daemon creates for an agent is created with and kept at: reachable by
/// its owner and by nobody else.
///
/// **This is a boundary against other users on the box, and against nothing else.** The agent
/// runs as the same uid as the daemon, so no mode
/// keeps *it* out of this directory. What decides that boundary is the peer-classification table `classify` implements, not a
/// `chmod`.
///
/// On Windows [`create_owner_only_dir`] supplies a protected, inheritable DACL whose owner and sole
/// trustee are the creating process's `TokenUser`; [`restrict`] and [`write_owner_only`] apply the
/// non-inheritable counterpart to each settings file. The `dudes-win` leaf contains every Win32
/// call and reads each result back before this host may spawn the child.
#[cfg(not(windows))]
const AGENT_DIR_MODE: u32 = 0o700;

/// The mode an adapter's file inside that directory is written with (the `--settings` file is `0600`).
///
/// It is the file that earns it. `--settings` names the program and the arguments of the hook shim
/// that governs the turn, and the loopback hook door puts the agent's token in the URL the same
/// file carries.
///
/// **What made that reachable was not a mode; it was the absence of one.** Before this, the file
/// was written by `fs::write`, which creates `0666 & ~umask` — so its mode was not a decision at
/// all, it was whatever umask the daemon happened to inherit, and **no production code in Dudes
/// sets one**: the workspace's only calls to that syscall are the two in this file's own
/// `#[cfg(all(test, unix))]` module, where one test forces the mask so that a creation-time mode
/// is observable at all. The control is that the same kind of search for `set_permissions` does
/// return production call sites — in this file and in `dudes-rpc` — so that zero is a measurement
/// and not a search that cannot hit.
///
/// The command is described rather than quoted on purpose. Spelled out, the pattern matches this
/// very comment, and the citation then contradicts itself on its own tree — which is how the
/// sentence this one replaces came to be false.
///
/// Under the usual `0022` that leaves `0644`, and every other user on the box **reads** a file
/// that governs the turn — and reads the token too, once a token is written into a
/// settings file, which no code does yet. Under a daemon started at `0000` it is `0666`, and
/// every other user **writes** the argv of the hook shim. Giving the mode a value is what takes
/// it off the umask.
///
/// The Windows position is [`AGENT_DIR_MODE`]'s.
const INSTALL_FILE_MODE: u32 = 0o600;

#[cfg(windows)]
fn open_owner_only_file(path: &Path) -> io::Result<fs::File> {
    dudes_win::open_file_creator_only(path)
}

/// Narrow `path` to `mode`, whatever it was before.
///
/// Setting the mode *after* creation as well as at it is what makes the result independent of the
/// umask — which can only clear bits, so a creation-time mode is an upper bound and not a value —
/// and it is what narrows a directory or a file an earlier version left at `0755`.
///
/// **Unlike `dudes_rpc`'s runtime directory this re-modes rather than refuses**, and the
/// difference is whose directory it is. A runtime directory may have been established by an
/// operator, so changing its mode is not that call's decision and it refuses a wide one instead.
/// `agents/<AgentId>/` has exactly one writer — the daemon — so there is no third party's choice
/// being overridden, and refusing would only stop the daemon spawning.
///
/// On Windows the only call site is the content-unchanged file path: directory creation applies
/// its own inheritable descriptor in [`create_owner_only_dir`].
fn restrict(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        debug_assert_eq!(mode, INSTALL_FILE_MODE);
        drop(open_owner_only_file(path)?);
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Create every missing component of `dir`, and leave `dir` itself owner-only.
///
/// The mode is carried into `mkdir(2)` so a directory this creates is never, for an instant, wider
/// than the Unix agent-directory mode. [`restrict`] then pins it to exactly that mode.
///
/// **Both are observable, and the second is not what makes the first so.** [`restrict`] pins the
/// **leaf** and nothing above it, so every *intermediate* component `recursive` creates —
/// `<data_dir>`, `<data_dir>/agents` — keeps whatever `mkdir(2)` gave it and never sees a `chmod`.
/// Those directories are therefore a race-free witness of the creation-time mode: with the umask
/// forced to zero they read `0700` here and `0777` with `builder.mode(..)` deleted, at any ambient
/// umask. `the_intermediate_directories_carry_the_creation_time_mode` is that test.
///
/// The window on the leaf itself — between its `mkdir(2)` and its `chmod` — is still unobservable
/// without racing it. What is claimed above is that the *call* has a failing test, not that the
/// window does.
fn create_owner_only_dir(dir: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        if !dir.exists()
            && let Some(parent) = dir.parent().filter(|path| !path.as_os_str().is_empty())
            && !parent.exists()
        {
            create_owner_only_dir(parent)?;
        }
        dudes_win::create_agent_dir_creator_only(dir)
    }
    #[cfg(not(windows))]
    {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;

            builder.mode(AGENT_DIR_MODE);
        }
        builder.create(dir)?;
        restrict(dir, AGENT_DIR_MODE)
    }
}

/// Write `content` to `path`, leaving the file readable and writable by its owner alone.
///
/// On Unix the content goes into a new file beside `path`, created with `O_EXCL` and the owner-only
/// mode as an argument to `open(2)`, and is then renamed over `path`. Neither step follows a
/// symlink: `O_EXCL` refuses one at the temporary name, and `rename(2)` replaces a link at `path`
/// instead of opening through it. So a link planted at an install target never lets the daemon
/// truncate or write the file it points at, and a file that stood there `0644` is replaced by one
/// that was never wider than [`INSTALL_FILE_MODE`].
fn write_owner_only(path: &Path, content: &str) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
        let mut temporary_name = std::ffi::OsString::from(".");
        temporary_name.push(name);
        temporary_name.push(format!(".new-{}", std::process::id()));
        let temporary = path.with_file_name(temporary_name);
        // A leftover from a crashed write of this same pid; `remove_file` does not follow a link.
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let written = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(INSTALL_FILE_MODE)
                .open(&temporary)?;
            file.write_all(content.as_bytes())?;
            drop(file);
            fs::rename(&temporary, path)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        written
    }
    #[cfg(windows)]
    {
        use std::io::Write as _;

        let mut file = open_owner_only_file(path)?;
        file.set_len(0)?;
        file.write_all(content.as_bytes())
    }
    #[cfg(not(any(unix, windows)))]
    {
        fs::write(path, content)
    }
}

impl RealHost {
    /// Read this host's boot identity once and take ownership of nothing yet.
    ///
    /// The read happens here rather than per-probe because the liveness table compares the key's
    /// boot identity against *this host's*, and a value re-read per probe could change underneath
    /// a decision.
    #[must_use]
    pub fn new() -> Self {
        Self::with_grace(DEFAULT_GRACE)
    }

    /// [`RealHost::new`] with a chosen grace between the polite signal and the hard one. Tests use
    /// a short one; the daemon uses [`DEFAULT_GRACE`].
    #[must_use]
    pub fn with_grace(grace: Duration) -> Self {
        let boot = match plat::boot_identity() {
            Ok(Some(boot)) => BootIdentity::Read(boot),
            Ok(None) => BootIdentity::NotApplicable,
            Err(error) => BootIdentity::Unreadable(error),
        };
        #[cfg(windows)]
        let peers = PeerProcesses::default();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let peers = PeerProcesses::new(boot.as_host_boot());
        #[cfg(target_os = "linux")]
        let pid_namespace = procfs_view::read_current(|| {
            fs::read_link("/proc/self/ns/pid")
                .ok()
                .and_then(|path| path.to_str().map(str::to_owned))
        })
        .flatten();
        #[cfg(not(target_os = "linux"))]
        let pid_namespace = None;
        Self {
            boot,
            pid_namespace,
            children: BTreeMap::new(),
            current_native_targets: Mutex::new(BTreeMap::new()),
            #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
            peers,
            grace,
            #[cfg(unix)]
            keeper_executable: None,
            streams: None,
            #[cfg(test)]
            setup_fault: None,
            #[cfg(test)]
            cleanup_evidence_for_test: None,
            #[cfg(test)]
            cleanup_answer_for_test: None,
            #[cfg(test)]
            hold_reap_until_cleanup_signals: None,
        }
    }

    /// A host that pipes every agent child's standard output and hands it to `streams`.
    ///
    /// **This is the constructor a daemon uses, and the difference it makes is a managed turn that
    /// can end normally.** A managed turn's result is parsed out of the child's stdout, so a host
    /// with no sink launches turns whose ends nothing can observe except the reaper's absence
    /// probe — which writes `Vanished` with no usage, because nothing read a `result` line.
    ///
    /// The pipe is **per child**, which is per turn: see `src/stream.rs` for why that is the only
    /// lifetime available and why nothing about it belongs in a [`LaunchSpec`].
    #[must_use]
    pub fn watching(streams: Arc<dyn stream::ChildStreams>) -> Self {
        Self::watching_with_grace(streams, DEFAULT_GRACE)
    }

    /// [`RealHost::watching`] with a chosen grace.
    #[must_use]
    pub fn watching_with_grace(streams: Arc<dyn stream::ChildStreams>, grace: Duration) -> Self {
        Self {
            streams: Some(streams),
            ..Self::with_grace(grace)
        }
    }

    /// Start a keeper beside every agent this host launches, from `executable`.
    ///
    /// `executable` is the daemon's own binary, which answers `keeper` as a hidden mode. A launch
    /// whose keeper cannot start fails, and no task reaches that agent.
    #[cfg(unix)]
    #[must_use]
    pub fn with_keeper(mut self, executable: PathBuf) -> Self {
        self.keeper_executable = Some(executable);
        self
    }

    /// How many children this host owns and has not reaped. The count invariant 10 is about.
    #[must_use]
    pub fn owned(&self) -> usize {
        self.children.len()
    }

    /// A read-only share of the process relation this host maintains for its children.
    ///
    /// The share exposes one resolved relation and no job or process handle. It is cloned before
    /// this host moves to the decide-loop thread and used by the connection threads to implement
    /// the peer-classification table.
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[must_use]
    pub fn peer_processes(&self) -> PeerProcesses {
        self.peers.clone()
    }

    /// Write the files in `install` whose content differs from what is on disk, into a directory
    /// nobody else on the machine can reach.
    ///
    /// A rewrite is not a state change: the adapter's files are a projection, so a re-issued spawn rebuilds them and a `--settings` path in argv always
    /// resolves. Writing only what differs keeps an unchanged file's mtime stable, which is what
    /// a vendor CLI watching its own settings file reacts to.
    ///
    /// **The mode is part of the projection, so it is re-established on every spawn — an
    /// *unchanged* file included.** That is what narrows a directory or a file left behind at
    /// `0755` by an earlier version, and it costs the mtime guarantee above nothing: `chmod(2)`
    /// moves `ctime`, not `mtime`.
    fn write_install(files: &[(PathBuf, String)]) -> io::Result<()> {
        for (path, content) in files {
            // `Some("")` is what `parent()` answers for a bare relative file name, and
            // `set_permissions("")` is `ENOENT`. `ManagedInstall` documents its paths as absolute,
            // so this is a guard rather than a case: a caller that ignores that gets the file
            // written where it asked, not an error from a directory that does not exist.
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                create_owner_only_dir(parent)?;
            }
            // `symlink_metadata`, so a link is never "equal content": it is rewritten as a regular
            // file, and `restrict` below never chmods through one.
            let regular = fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file());
            if regular && fs::read_to_string(path).ok().as_deref() == Some(content.as_str()) {
                restrict(path, INSTALL_FILE_MODE)?;
                continue;
            }
            write_owner_only(path, content)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod spawn_failure_tests {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use dudes_supervisor::host::{AdoptionKey, NativeGroupPresence, ProbeAnswer};
    use std::ffi::OsString;
    use std::fs;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use dudes_providers::adapter::ManagedInstall;
    use dudes_providers::launch::{EnvVar, LaunchSpec};
    use dudes_supervisor::host::{
        Liveness, NativeBoundaryAcknowledgement, NativeBoundaryReceipt, OwnedTargetState,
        OwnedTargetsStatus, ProcessHost, SpawnResult,
    };

    use super::kill::TestCleanupEvidence;
    use super::{RealHost, SetupFault};

    const ROLE: &str = "DUDES_HOST_UNIT_SPAWN_ROLE";
    const MARK: &str = "DUDES_HOST_UNIT_SPAWN_MARK";

    #[test]
    fn failed_spawn_helper() {
        let mark = std::env::var_os(MARK).map(PathBuf::from);
        match std::env::var(ROLE).ok().as_deref() {
            None | Some("exit") => {}
            Some("park") => heartbeat(mark.as_deref().expect("park helper mark")),
            Some("interrupted-heartbeat") => {
                let mark = mark.as_deref().expect("interrupted helper mark");
                fs::write(mark, std::process::id().to_string()).expect("write helper pid");
                let beat = mark.with_extension("beat");
                publish_heartbeat(&beat, "1", |_| {});
                publish_heartbeat(&beat, "2", |staged| {
                    assert_eq!(fs::read_to_string(staged).expect("staged counter"), "2");
                    fs::write(mark.with_extension("ready"), "2").expect("announce staged counter");
                    loop {
                        sleep(Duration::from_millis(20));
                    }
                });
            }
            Some("descendant") => {
                let mark = mark.as_deref().expect("descendant helper mark");
                let descendant = mark.with_file_name("escaped-descendant.pid");
                let mut command = helper_command("park", &descendant);
                #[allow(clippy::zombie_processes)]
                let _child = command.spawn().expect("spawn escaped descendant");
                heartbeat(mark);
            }
            #[cfg(unix)]
            Some(role @ ("exit-with-escaped-descendant" | "exit-with-retained-descendant")) => {
                let mark = mark.as_deref().expect("escaped descendant mark");
                let mut command = helper_command("park", mark);
                if role == "exit-with-escaped-descendant" {
                    super::unix::prepare(&mut command);
                }
                #[allow(clippy::zombie_processes)]
                let _child = command.spawn().expect("spawn query test descendant");
                await_beating(mark);
            }
            Some(other) => panic!("unknown helper role {other}"),
        }
    }

    fn spec(role: &str, stdin: Option<Vec<u8>>) -> LaunchSpec {
        spec_with_mark(role, stdin, None)
    }

    fn helper_env(role: &str, mark: Option<&Path>) -> Vec<EnvVar> {
        let mut env: Vec<EnvVar> = [
            "PATH",
            "HOME",
            "TMPDIR",
            "SystemRoot",
            "windir",
            "TEMP",
            "TMP",
            "USERPROFILE",
            "SystemDrive",
        ]
        .into_iter()
        .filter_map(|name| std::env::var_os(name).map(|value| EnvVar::new(name, value)))
        .collect();
        env.push(EnvVar::new(ROLE, role));
        if let Some(mark) = mark {
            env.push(EnvVar::new(MARK, mark.as_os_str()));
        }
        env
    }

    fn spec_with_mark(role: &str, stdin: Option<Vec<u8>>, mark: Option<&Path>) -> LaunchSpec {
        LaunchSpec {
            argv: vec![
                OsString::from(std::env::current_exe().expect("current test executable")),
                "--exact".into(),
                "spawn_failure_tests::failed_spawn_helper".into(),
                "--quiet".into(),
            ],
            env: helper_env(role, mark),
            cwd: std::env::current_dir().expect("current directory"),
            stdin: stdin.map_or(
                dudes_providers::launch::LaunchInput::Null,
                dudes_providers::launch::LaunchInput::WriteThenClose,
            ),
        }
    }

    fn helper_command(role: &str, mark: &Path) -> Command {
        let spec = spec_with_mark(role, None, Some(mark));
        let (program, args) = spec.argv.split_first().expect("helper argv");
        let mut command = Command::new(program);
        command.args(args);
        command.env_clear();
        for var in &spec.env {
            command.env(var.name(), var.expose_value());
        }
        command.current_dir(spec.cwd);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn heartbeat(mark: &Path) -> ! {
        fs::write(mark, std::process::id().to_string()).expect("write helper pid");
        let beat = mark.with_extension("beat");
        let mut count = 0_u64;
        loop {
            count += 1;
            publish_heartbeat(&beat, &count.to_string(), |_| {});
            sleep(Duration::from_millis(20));
        }
    }

    fn publish_heartbeat(beat: &Path, counter: &str, before_replace: impl FnOnce(&Path)) {
        let name = beat
            .file_name()
            .expect("heartbeat basename")
            .to_string_lossy();
        let staged = beat.with_file_name(format!(".{name}.{}.writing", std::process::id()));
        fs::write(&staged, counter).expect("stage heartbeat");
        // The interruption control pauses at the same boundary normal publication crosses.
        // Replacing the file preserves the previous complete counter if the writer is killed.
        before_replace(&staged);
        fs::rename(&staged, beat).expect("publish heartbeat");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn prior_run_queries_observe_an_owned_fixture_without_adopting_it() {
        let root = tempfile::TempDir::new().unwrap();
        let mark = root.path().join("leader");
        let mut command = helper_command("park", &mark);
        super::unix::prepare(&mut command);
        let mut child = HelperChild(command.spawn().unwrap());
        await_beating(&mark);
        let host = RealHost::new();
        let pid = child.0.id();
        let key = host.read_key(pid, &super::plat::grip_adopted(pid)).unwrap();
        assert_eq!(
            host.probe_native_leader(&key),
            ProbeAnswer::StartValue(key.start_time)
        );
        assert_eq!(
            host.native_group_presence(pid),
            NativeGroupPresence::Present
        );
        assert!(host.children.is_empty());
        assert!(host.owned_targets(1).targets.is_empty());
        child.kill_and_reap();
        assert_eq!(host.probe_native_leader(&key), ProbeAnswer::NoSuchPid);
        assert_eq!(host.native_group_presence(pid), NativeGroupPresence::Absent);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn prior_run_group_queries_keep_permission_and_errors_unresolved() {
        assert_eq!(
            super::native_group_presence(&Ok(super::unix::Presence::Absent)),
            NativeGroupPresence::Absent
        );
        assert_eq!(
            super::native_group_presence(&Ok(super::unix::Presence::Present)),
            NativeGroupPresence::Present
        );
        assert_eq!(
            super::native_group_presence(&Ok(super::unix::Presence::Unsignallable)),
            NativeGroupPresence::Unsignallable
        );
        assert_eq!(
            super::native_group_presence(&Err(io::Error::from(io::ErrorKind::PermissionDenied))),
            NativeGroupPresence::Unknown
        );
        let host = RealHost::new();
        for pid in [0, 1, i32::MAX as u32 + 1, u32::MAX] {
            assert_eq!(
                host.native_group_presence(pid),
                NativeGroupPresence::Unknown
            );
            assert_eq!(
                host.probe_native_leader(&AdoptionKey {
                    pid,
                    start_time: 1,
                    boot: None
                }),
                ProbeAnswer::CouldNotAnswer
            );
        }
        assert_eq!(
            host.probe_native_leader(&AdoptionKey {
                pid: 42,
                start_time: 0,
                boot: None
            }),
            ProbeAnswer::CouldNotAnswer
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn prior_run_missing_process_table_entry_requires_direct_pid_absence() {
        for presence in [
            super::unix::Presence::Present,
            super::unix::Presence::Unsignallable,
        ] {
            assert_eq!(
                super::native_leader_observation(ProbeAnswer::NoSuchPid, || Ok(presence)),
                ProbeAnswer::CouldNotAnswer
            );
        }
        assert_eq!(
            super::native_leader_observation(ProbeAnswer::NoSuchPid, || Err(
                io::ErrorKind::PermissionDenied.into()
            )),
            ProbeAnswer::CouldNotAnswer
        );
        assert_eq!(
            super::native_leader_observation(ProbeAnswer::NoSuchPid, || Err(io::Error::other(
                "query failed"
            ))),
            ProbeAnswer::CouldNotAnswer
        );
        assert_eq!(
            super::native_leader_observation(ProbeAnswer::NoSuchPid, || Ok(
                super::unix::Presence::Absent
            )),
            ProbeAnswer::NoSuchPid
        );
        for answer in [ProbeAnswer::StartValue(99), ProbeAnswer::CouldNotAnswer] {
            assert_eq!(
                super::native_leader_observation(answer, || panic!(
                    "only missing table entries require corroboration"
                )),
                answer
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prior_run_namespace_and_start_values_use_this_process_aligned_view() {
        let mut host = RealHost::new();
        let namespace = fs::read_link("/proc/self/ns/pid").unwrap();
        assert_eq!(host.pid_namespace().as_deref(), namespace.to_str());
        assert!(host.pid_namespace().is_some());
        let pid = std::process::id();
        let key = host.read_key(pid, &super::plat::grip_adopted(pid)).unwrap();
        let expected = if pid == 1 {
            ProbeAnswer::CouldNotAnswer
        } else {
            ProbeAnswer::StartValue(key.start_time)
        };
        assert_eq!(host.probe_native_leader(&key), expected);
        host.pid_namespace = None;
        assert_eq!(host.pid_namespace(), None);
    }

    struct HelperChild(Child);

    impl HelperChild {
        fn kill_and_reap(&mut self) {
            self.0.kill().expect("kill test-owned helper");
            self.0.wait().expect("reap test-owned helper");
        }
    }

    impl Drop for HelperChild {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }

    #[cfg(target_os = "macos")]
    fn exited_unreaped_child() -> HelperChild {
        let mut command = helper_command("exit", Path::new("."));
        super::unix::prepare(&mut command);
        let child = HelperChild(command.spawn().expect("spawn short-lived helper"));
        let pid = rustix::process::Pid::from_raw(i32::try_from(child.0.id()).expect("pid fits"))
            .expect("nonzero pid");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !matches!(
            rustix::process::test_kill_process_group(pid),
            Err(rustix::io::Errno::PERM)
        ) {
            assert!(
                Instant::now() < deadline,
                "the helper did not become a zombie"
            );
            sleep(Duration::from_millis(5));
        }
        assert!(
            rustix::process::test_kill_process(pid).is_ok(),
            "the unreaped child must still reserve its pid"
        );
        child
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_managed_child_key_can_be_read_after_exit_before_reap() {
        let child = exited_unreaped_child();
        let pid = child.0.id();
        let key = RealHost::new()
            .read_key(pid, &super::plat::grip_adopted(pid))
            .expect("the unreaped child's start value");
        assert_eq!(key.pid, pid);
        assert_ne!(key.start_time, 0);
        assert_eq!(key.boot, None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_external_child_can_register_after_exit_before_reap() {
        let child = exited_unreaped_child();
        let pid = child.0.id();
        let processes = RealHost::new().peer_processes();
        let slot = processes.begin_spawn();
        let mut external = processes.track_external(slot);
        let key = external.register(pid).expect("the unreaped child's key");
        assert_eq!(key.pid, pid);
        assert_ne!(key.start_time, 0);
        assert!(processes.finish_spawn(slot, Some(&key)));
    }

    #[test]
    fn an_interrupted_heartbeat_publication_keeps_the_last_complete_counter() {
        let root = tempfile::tempdir().expect("heartbeat controls");
        let live_mark = root.path().join("live.pid");
        let mut live = HelperChild(
            helper_command("park", &live_mark)
                .spawn()
                .expect("live helper"),
        );
        await_beating(&live_mark);
        live.kill_and_reap();

        let mark = root.path().join("interrupted.pid");
        let mut child = HelperChild(
            helper_command("interrupted-heartbeat", &mark)
                .spawn()
                .expect("interrupted helper"),
        );
        assert_eq!(await_number(&mark.with_extension("ready")), 2);
        child.kill_and_reap();
        assert_eq!(
            fs::read_to_string(mark.with_extension("beat")).expect("read published heartbeat"),
            "1",
            "an interrupted publication must preserve the last complete counter"
        );
        assert!(
            !beating(&mark),
            "the killed and reaped helper must be stopped"
        );
    }

    #[test]
    fn missing_empty_or_malformed_initial_heartbeats_never_prove_death() {
        let root = tempfile::tempdir().expect("invalid heartbeat controls");
        std::thread::scope(|scope| {
            for (name, content) in [
                ("missing", None),
                ("empty", Some("")),
                ("malformed", Some("x")),
            ] {
                let mark = root.path().join(format!("{name}.pid"));
                if let Some(content) = content {
                    fs::write(mark.with_extension("beat"), content)
                        .expect("plant invalid heartbeat");
                }
                scope.spawn(move || {
                    let error = std::panic::catch_unwind(|| beating(&mark))
                        .expect_err("an invalid initial heartbeat must not prove death");
                    let message = error
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| error.downcast_ref::<&str>().copied())
                        .expect("heartbeat panic message");
                    assert!(message.starts_with("no heartbeat at "), "{message}");
                });
            }
        });
    }

    fn await_number(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(text) = fs::read_to_string(path)
                && let Ok(pid) = text.parse()
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "no pid at {}", path.display());
            sleep(Duration::from_millis(10));
        }
    }

    fn beating(mark: &Path) -> bool {
        let beat = mark.with_extension("beat");
        let read = || fs::read_to_string(&beat).ok()?.parse::<u64>().ok();
        let deadline = Instant::now() + Duration::from_secs(20);
        let before = loop {
            if let Some(value) = read() {
                break value;
            }
            assert!(
                Instant::now() < deadline,
                "no heartbeat at {}",
                beat.display()
            );
            sleep(Duration::from_millis(10));
        };
        sleep(Duration::from_millis(200));
        let after = read();
        after.is_some_and(|after| after > before)
    }

    fn await_beating(mark: &Path) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !beating(mark) {
            assert!(
                Instant::now() < deadline,
                "heartbeat did not advance at {}",
                mark.with_extension("beat").display()
            );
        }
    }

    #[cfg(unix)]
    fn stop_test_descendant(pid: u32) {
        if let Some(pid) = rustix::process::Pid::from_raw(i32::try_from(pid).expect("pid fits")) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }

    #[cfg(windows)]
    fn stop_test_descendant(pid: u32) {
        let pid = pid.to_string();
        let _ = Command::new("taskkill")
            .args(["/pid", &pid, "/T", "/F"])
            .status();
    }

    /// Wait until the descendant at `pid` can no longer run, rather than sampling once.
    ///
    /// [`stop_test_descendant`] and `RealHost::shutdown` both *issue* a termination; neither waits
    /// for it to land. This descendant is not this process's child — its leader was the parent and
    /// is already gone — so there is no `Child::wait` to make death observable the way
    /// [`HelperChild::kill_and_reap`] makes it observable for the helper above, and
    /// `assert!(!beating(..))` then takes its single 200 ms sample against a process that may not
    /// have stopped yet. One publication landing after that sample's first read reads as life, and
    /// under a loaded full-workspace run one did.
    ///
    /// Widening or narrowing the sample cannot fix that; ordering can. Note which way the asymmetry
    /// runs: [`await_beating`] above already *retries* for proof of life, because a live helper can
    /// be delayed by the scheduler. Proof of death needs the opposite — wait for the event, then
    /// observe — and that is what this is.
    #[cfg(unix)]
    fn await_stopped(pid: u32, _mark: &Path, what: &str) {
        let Some(pid) = rustix::process::Pid::from_raw(i32::try_from(pid).expect("pid fits"))
        else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            // The two errnos are spelled out here rather than taken from `unix::quiet_checked`:
            // `kill::shutdown` consults that function to decide when to stop waiting, so a test
            // that asked it would agree with the code under test by construction. What they mean
            // is easy to get backwards. `ESRCH` is the one that means reaped, and
            // this loop waits for it. `EPERM` is not a zombie having released its credentials:
            // this is the direct form, and against an own unreaped zombie the direct form answers
            // rc=0, because the kernel finds it in the zombie list and POSIX requires success. A
            // direct `EPERM` means a live process this test may not signal, and accepting it as
            // stop evidence is a separate defect that this loop does not repair.
            if matches!(
                rustix::process::test_kill_process(pid),
                Err(rustix::io::Errno::SRCH | rustix::io::Errno::PERM)
            ) {
                return;
            }
            assert!(Instant::now() < deadline, "{what}");
            sleep(Duration::from_millis(10));
        }
    }

    /// The same wait where there is no pid to ask.
    ///
    /// Reading a process's liveness here would need a Win32 call, and `unsafe` outside
    /// `src/windows.rs` is what `tests/unsafe_grant.rs` fails on — it matches on basename, so this
    /// module is not exempt. Retrying [`beating`] to a bound is what removes the defect anyway: the
    /// single sample is the bug, not the heartbeat.
    #[cfg(windows)]
    fn await_stopped(_pid: u32, mark: &Path, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while beating(mark) {
            assert!(Instant::now() < deadline, "{what}");
        }
    }

    /// The setup reason, once the spawn result is the expected one.
    ///
    /// `cleanup` is [`RealHost::cleanup_answer_for_test`], and passing it is the difference between
    /// a transcript that can be read and one that can only be re-run. `reason` is the *setup* error
    /// is the *setup* error, so it is the same string whichever cleanup path produced the
    /// variant; a failure that prints only that names the fault that was injected and nothing about
    /// what cleanup did with it. The cleanup answer distinguishes, for instance, a refused guard
    /// (`Unconfirmed(None)`, no signal sent) from a cleanup step that failed with a reason of its
    /// own (`Unconfirmed(Some(..))`).
    fn expect_setup_result(
        result: SpawnResult,
        cleanup: Option<&str>,
        confirmed: bool,
        label: &str,
    ) -> String {
        let cleanup = cleanup.unwrap_or("<no cleanup ran>");
        match (confirmed, result) {
            (true, SpawnResult::NotLaunched { reason })
            | (false, SpawnResult::CleanupUnconfirmed { reason }) => reason.to_string(),
            (true, SpawnResult::CleanupUnconfirmed { reason }) => {
                panic!(
                    "{label} had complete cleanup evidence but remained uncertain: setup reason \
                     {reason}, cleanup answered {cleanup}"
                )
            }
            (false, SpawnResult::NotLaunched { reason }) => {
                panic!(
                    "{label} withheld boundary evidence but claimed absence: setup reason \
                     {reason}, cleanup answered {cleanup}"
                )
            }
            (_, SpawnResult::Spawned(_)) => panic!("{label} unexpectedly spawned"),
        }
    }

    fn expect_unresolved_shutdown(host: &mut RealHost, label: &str) {
        let Err(error) = host.shutdown() else {
            panic!("{label} discarded incomplete ownership");
        };
        assert!(
            !error.to_string().is_empty(),
            "{label} returned an empty error"
        );
        assert_ne!(
            host.owned(),
            0,
            "{label} returned an error without retention"
        );
    }

    #[test]
    fn owned_target_queries_preserve_full_keys_and_bound_the_inventory() {
        let root = tempfile::tempdir().expect("owned target inventory");
        let mut host = RealHost::with_grace(Duration::from_millis(100));
        let first_mark = root.path().join("first.pid");
        let mut launch = spec_with_mark("park", None, Some(&first_mark));
        launch.cwd = root.path().to_path_buf();
        let first = match host.spawn(&launch, &ManagedInstall::default()) {
            SpawnResult::Spawned(key) => key,
            other => panic!("first target did not spawn: {other:?}"),
        };
        let second_mark = root.path().join("second.pid");
        launch = spec_with_mark("park", None, Some(&second_mark));
        launch.cwd = root.path().to_path_buf();
        let second = match host.spawn(&launch, &ManagedInstall::default()) {
            SpawnResult::Spawned(key) => key,
            other => panic!("second target did not spawn: {other:?}"),
        };
        await_beating(&first_mark);
        await_beating(&second_mark);
        let omitted = host.owned_targets(0);
        assert_eq!(omitted.status, OwnedTargetsStatus::Truncated { omitted: 2 });
        assert!(omitted.targets.is_empty());
        let bounded = host.owned_targets(1);
        assert_eq!(bounded.status, OwnedTargetsStatus::Truncated { omitted: 1 });
        assert_eq!(bounded.targets.len(), 1);
        let all = host.owned_targets(usize::MAX);
        assert_eq!(all.status, OwnedTargetsStatus::Complete);
        assert_eq!(all.targets.len(), 2);
        for key in [&first, &second] {
            let target = all
                .targets
                .iter()
                .find(|target| target.key.as_ref().is_some_and(|seen| seen.pid == key.pid))
                .expect("each owned key is present");
            let seen = target.key.as_ref().expect("the target retains its key");
            assert_eq!(seen.start_time, key.start_time);
            assert_eq!(seen.boot, key.boot);
            assert_eq!(target.cwd, root.path());
            assert_eq!(target.state, OwnedTargetState::Present);
        }
        let recycled = dudes_supervisor::host::AdoptionKey {
            start_time: first.start_time.wrapping_add(1),
            ..first.clone()
        };
        assert_eq!(host.alive(&first), Liveness::Alive);
        assert_eq!(host.alive(&recycled), Liveness::Gone);
        host.children
            .get_mut(&first.pid)
            .expect("the retained first row")
            .key = Some(recycled);
        let recycled_row = host
            .owned_targets(2)
            .targets
            .into_iter()
            .find(|target| {
                target
                    .key
                    .as_ref()
                    .is_some_and(|seen| seen.pid == first.pid)
            })
            .expect("the recycled row is still retained");
        assert_eq!(recycled_row.state, OwnedTargetState::UnknownIdentity);
        host.children
            .get_mut(&first.pid)
            .expect("the retained first row")
            .key = Some(first.clone());
        assert_eq!(host.owned(), 2, "read-only queries retain both rows");
        let first_receipt = host.children[&first.pid].native_receipt.clone();
        assert!(matches!(
            host.kill(&first),
            dudes_supervisor::host::KillOutcome::Signalled
        ));
        let missing = host
            .owned_targets(2)
            .targets
            .into_iter()
            .find(|target| super::same_recorded_keys(target.key.as_ref(), Some(&first)))
            .unwrap();
        assert_eq!(missing.state, OwnedTargetState::UnknownMissingCapability);
        assert!(missing.native_absence_receipt.is_none());
        assert_eq!(
            host.acknowledge_native_boundary_absence(&first, &first_receipt),
            NativeBoundaryAcknowledgement::UnknownMissingCapability
        );
        expect_unresolved_shutdown(&mut host, "the native targets have incomplete coverage");
    }

    #[cfg(unix)]
    #[test]
    fn native_boundary_absence_does_not_cover_an_escaped_descendant() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        struct EndedStream(Arc<AtomicBool>);
        impl crate::ChildStreams for EndedStream {
            fn watch(
                &self,
                _key: Option<&dudes_supervisor::host::AdoptionKey>,
            ) -> Box<dyn crate::ChildStream> {
                Box::new(Self(Arc::clone(&self.0)))
            }
        }
        impl crate::ChildStream for EndedStream {
            fn line(&mut self, _text: &str) {}

            fn ended(self: Box<Self>, _exit: crate::ChildExit) {
                self.0.store(true, Ordering::Release);
            }
        }

        let root = tempfile::tempdir().expect("retained descendant query");
        let mark = root.path().join("descendant.pid");
        let ended = Arc::new(AtomicBool::new(false));
        let mut host = RealHost::watching_with_grace(
            Arc::new(EndedStream(Arc::clone(&ended))),
            Duration::from_millis(100),
        );
        let mut launch = spec_with_mark("exit-with-escaped-descendant", None, Some(&mark));
        launch.cwd = root.path().to_path_buf();
        let key = match host.spawn(&launch, &ManagedInstall::default()) {
            SpawnResult::Spawned(key) => key,
            other => panic!("retained descendant leader did not spawn: {other:?}"),
        };
        let descendant_pid = await_number(&mark);
        let mut descendant = QueryDescendant::track(descendant_pid);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ended.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "the leader stream did not end");
            sleep(Duration::from_millis(5));
        }
        await_beating(&mark);
        assert_eq!(host.alive(&key), Liveness::Gone);
        let answer = host.owned_targets(1);
        assert_eq!(answer.status, OwnedTargetsStatus::Complete);
        assert_eq!(answer.targets.len(), 1);
        assert_eq!(answer.targets[0].cwd, root.path());
        assert_eq!(
            answer.targets[0].state,
            OwnedTargetState::NativeBoundaryAbsent
        );
        assert_eq!(host.owned(), 1);
        let stopped = descendant.host.kill(&descendant.key);
        assert!(matches!(
            stopped,
            dudes_supervisor::host::KillOutcome::Signalled
        ));
        await_stopped(descendant_pid, &mark, "the test descendant did not stop");
        assert_eq!(
            host.owned_targets(1).targets[0].state,
            OwnedTargetState::NativeBoundaryAbsent,
            "scoped absence says nothing about the escaped descendant"
        );
        expect_unresolved_shutdown(&mut host, "shutdown containment remains unproven");
    }

    #[cfg(unix)]
    struct QueryDescendant {
        host: RealHost,
        key: dudes_supervisor::host::AdoptionKey,
    }

    #[cfg(unix)]
    impl QueryDescendant {
        fn track(pid: u32) -> Self {
            let host = RealHost::with_grace(Duration::from_millis(100));
            let key = host
                .read_key(pid, &super::plat::grip_adopted(pid))
                .expect("the test descendant has a full adoption key");
            Self { host, key }
        }
    }

    #[cfg(unix)]
    impl Drop for QueryDescendant {
        fn drop(&mut self) {
            let _ = self.host.kill(&self.key);
        }
    }

    fn completed_query_target(host: &mut RealHost) -> dudes_supervisor::host::AdoptionKey {
        let key = match host.spawn(&spec("exit", None), &ManagedInstall::default()) {
            SpawnResult::Spawned(key) => key,
            other => panic!("the completed query target did not spawn: {other:?}"),
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while !host.children[&key.pid]
            .reap_confirmed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            assert!(
                Instant::now() < deadline,
                "the completed target's successful reap was not published"
            );
            sleep(Duration::from_millis(5));
        }
        key
    }

    fn observed_absence(
        host: &RealHost,
        key: &dudes_supervisor::host::AdoptionKey,
    ) -> dudes_supervisor::host::OwnedTargetEvidence {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let target = host
                .owned_targets(512)
                .targets
                .into_iter()
                .find(|target| super::same_recorded_keys(target.key.as_ref(), Some(key)))
                .expect("the current query contains the exact keyed attempt");
            if target.state == OwnedTargetState::NativeBoundaryAbsent {
                return target;
            }
            assert!(
                Instant::now() < deadline,
                "the native target did not become absent"
            );
            sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn withheld_acknowledgement_and_rebound_identity_preserve_exact_absence_receipts() {
        let mut host = RealHost::with_grace(Duration::from_millis(100));
        let key = completed_query_target(&mut host);
        let first = observed_absence(&host, &key);
        let first_receipt = first.native_absence_receipt.unwrap();
        let retry = observed_absence(&host, &key);
        assert_eq!(retry.native_absence_receipt, Some(first_receipt.clone()));
        assert_eq!(host.owned(), 1, "withheld settlement retains ownership");

        host.children.get_mut(&key.pid).unwrap().native_receipt = NativeBoundaryReceipt::fresh();
        host.index_native_target(key.pid);
        let rebound_receipt = host.children[&key.pid].native_receipt.clone();
        assert_eq!(
            host.acknowledge_native_boundary_absence(&key, &rebound_receipt),
            NativeBoundaryAcknowledgement::RefusedUnobserved
        );
        let rebound = observed_absence(&host, &key);
        assert_eq!(
            rebound.native_absence_receipt,
            Some(rebound_receipt.clone())
        );
        assert_ne!(first_receipt, rebound_receipt);
        assert_eq!(
            host.acknowledge_native_boundary_absence(&key, &first_receipt),
            NativeBoundaryAcknowledgement::UnknownIdentity,
            "an equal pid, start value and boot cannot reuse an earlier attempt's receipt"
        );
        let stale_key = dudes_supervisor::host::AdoptionKey {
            start_time: key.start_time.wrapping_add(1),
            ..key.clone()
        };
        assert_eq!(
            host.acknowledge_native_boundary_absence(&stale_key, &rebound_receipt),
            NativeBoundaryAcknowledgement::UnknownIdentity
        );
        let foreign_boot = dudes_supervisor::host::AdoptionKey {
            boot: if key.boot.is_some() {
                None
            } else {
                Some("another-boot".into())
            },
            ..key.clone()
        };
        assert_eq!(
            host.acknowledge_native_boundary_absence(&foreign_boot, &rebound_receipt),
            NativeBoundaryAcknowledgement::UnknownIdentity
        );
        assert_eq!(
            observed_absence(&host, &key).native_absence_receipt,
            Some(rebound_receipt.clone())
        );
        assert_eq!(
            host.acknowledge_native_boundary_absence(&key, &rebound_receipt),
            NativeBoundaryAcknowledgement::Acknowledged
        );
        let acknowledged = host.owned_targets(1);
        assert_eq!(acknowledged.status, OwnedTargetsStatus::Complete);
        assert!(acknowledged.targets.is_empty());
        assert_eq!(
            host.owned(),
            1,
            "acknowledgement keeps shutdown capabilities"
        );
        expect_unresolved_shutdown(&mut host, "acknowledgement did not prove full containment");
    }

    #[test]
    fn acknowledged_history_does_not_fill_the_current_native_target_query_bound() {
        use super::{
            CurrentNativeTarget, acknowledge_current_native_target, query_current_native_targets,
        };
        use std::collections::BTreeMap;

        let owned: BTreeMap<_, _> = (1..=514)
            .map(|pid| {
                let key = dudes_supervisor::host::AdoptionKey {
                    pid,
                    start_time: u64::from(pid),
                    boot: None,
                };
                (pid, (key, NativeBoundaryReceipt::fresh()))
            })
            .collect();
        let mut current: BTreeMap<_, _> = owned
            .iter()
            .map(|(pid, (key, receipt))| {
                (
                    *pid,
                    CurrentNativeTarget {
                        key: Some(key.clone()),
                        cwd: "/fixture".into(),
                        receipt: receipt.clone(),
                        absence_observed: false,
                    },
                )
            })
            .collect();
        let inspect = |pid: u32, _: &CurrentNativeTarget| {
            if pid == 514 {
                OwnedTargetState::Present
            } else {
                OwnedTargetState::NativeBoundaryAbsent
            }
        };
        let first = query_current_native_targets(&mut current, usize::MAX, inspect);
        assert_eq!(first.status, OwnedTargetsStatus::Truncated { omitted: 2 });
        assert_eq!(first.targets.len(), 512);
        assert!(
            first
                .targets
                .iter()
                .all(|target| target.key.as_ref().unwrap().pid != 514)
        );
        let withheld = query_current_native_targets(&mut current, 512, inspect);
        assert_eq!(
            withheld.status,
            OwnedTargetsStatus::Truncated { omitted: 2 }
        );
        for (observed, retry) in first.targets.iter().zip(&withheld.targets) {
            assert_eq!(observed.state, OwnedTargetState::NativeBoundaryAbsent);
            assert_eq!(retry.state, OwnedTargetState::NativeBoundaryAbsent);
            assert_eq!(
                observed.native_absence_receipt,
                retry.native_absence_receipt
            );
            let key = observed.key.as_ref().unwrap();
            let (owned_key, owned_receipt) = &owned[&key.pid];
            assert_eq!(
                acknowledge_current_native_target(
                    &mut current,
                    key,
                    observed.native_absence_receipt.as_ref().unwrap(),
                    Some((Some(owned_key), owned_receipt)),
                ),
                NativeBoundaryAcknowledgement::Acknowledged
            );
        }
        let remaining = query_current_native_targets(&mut current, 512, inspect);
        assert_eq!(remaining.status, OwnedTargetsStatus::Complete);
        assert_eq!(remaining.targets.len(), 2);
        let active = remaining
            .targets
            .iter()
            .find(|target| target.key.as_ref().unwrap().pid == 514)
            .unwrap();
        assert_eq!(active.state, OwnedTargetState::Present);
        assert!(active.native_absence_receipt.is_none());
        assert_eq!(
            owned.len(),
            514,
            "query acknowledgement preserves immutable ownership evidence"
        );
        assert_eq!(current.len(), 2);
    }

    #[test]
    fn an_ordinary_completed_target_can_be_absent_with_shutdown_ownership_retained() {
        let mut host = RealHost::with_grace(Duration::from_millis(100));
        let key = match host.spawn(&spec("exit", None), &ManagedInstall::default()) {
            SpawnResult::Spawned(key) => key,
            other => panic!("the completed target did not spawn: {other:?}"),
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let answer = host.owned_targets(1);
            assert_eq!(answer.status, OwnedTargetsStatus::Complete);
            assert_eq!(answer.targets.len(), 1);
            let seen = answer.targets[0]
                .key
                .as_ref()
                .expect("the retained exact key");
            assert_eq!(seen.pid, key.pid);
            assert_eq!(seen.start_time, key.start_time);
            assert_eq!(seen.boot, key.boot);
            if answer.targets[0].state == OwnedTargetState::NativeBoundaryAbsent {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the native target did not become absent"
            );
            sleep(Duration::from_millis(5));
        }
        assert_eq!(host.owned(), 1);
        expect_unresolved_shutdown(
            &mut host,
            "scoped absence does not discharge shutdown ownership",
        );
        assert_eq!(
            host.owned_targets(1).targets[0].state,
            OwnedTargetState::NativeBoundaryAbsent
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_same_group_descendant_prevents_native_boundary_absence_after_leader_reap() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let root = tempfile::tempdir().expect("native group descendant query");
        let mark = root.path().join("descendant.pid");
        let mut host = RealHost::with_grace(Duration::from_millis(100));
        let mut command = helper_command("exit-with-retained-descendant", &mark);
        command.current_dir(root.path());
        let prep = super::plat::prepare(&mut command).expect("prepare native process group");
        let child = command.spawn().expect("spawn native group leader");
        let pid = child.id();
        let spawned = super::plat::grip_spawned(prep, &child);
        let key = host
            .read_key(pid, &spawned.grip)
            .expect("the exact owned leader key");
        let reap_confirmed = Arc::new(AtomicBool::new(false));
        let confirmed = Arc::clone(&reap_confirmed);
        let leader_only_exit = super::stream::watch(
            child,
            None,
            None,
            move || confirmed.store(true, Ordering::Release),
            Instant::now(),
            None,
            None,
        );
        host.children.insert(
            pid,
            super::Owned {
                pid,
                exit: leader_only_exit,
                grip: spawned.grip,
                boundary: spawned.boundary,
                key: Some(key.clone()),
                cwd: root.path().to_path_buf(),
                reap_confirmed: Arc::clone(&reap_confirmed),
                native_receipt: NativeBoundaryReceipt::fresh(),
                protocol: None,
                #[cfg(unix)]
                lifeline: None,
            },
        );
        host.index_native_target(pid);
        let descendant_pid = await_number(&mark);
        let mut descendant = QueryDescendant::track(descendant_pid);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !reap_confirmed.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "the leader's successful reap was not published"
            );
            sleep(Duration::from_millis(5));
        }
        assert_eq!(host.alive(&key), Liveness::Gone);
        await_beating(&mark);
        let retained = host
            .children
            .get(&pid)
            .expect("the native group remains retained");
        assert_eq!(
            super::native_boundary_state(&retained.grip, pid).unwrap(),
            super::kill::TargetState::Present
        );
        assert_eq!(
            host.owned_targets(1).targets[0].state,
            OwnedTargetState::UnknownIdentity
        );
        let stopped = descendant.host.kill(&descendant.key);
        assert!(matches!(
            stopped,
            dudes_supervisor::host::KillOutcome::Signalled
        ));
        await_stopped(
            descendant_pid,
            &mark,
            "the same-group test descendant did not stop",
        );
        assert_eq!(
            host.owned_targets(1).targets[0].state,
            OwnedTargetState::NativeBoundaryAbsent
        );
        expect_unresolved_shutdown(&mut host, "shutdown still requires complete containment");
    }

    #[test]
    fn native_boundary_absence_requires_scope_reap_and_fresh_query_evidence() {
        use super::kill::{Boundary, TargetState};
        use super::owned_target_state;
        #[cfg(unix)]
        let native = Boundary::ProcessGroup;
        #[cfg(windows)]
        let native = Boundary::Job;

        assert_eq!(
            owned_target_state(
                native,
                Some(Liveness::Alive),
                false,
                &Ok(TargetState::Present)
            ),
            OwnedTargetState::Present
        );
        assert_eq!(
            owned_target_state(
                native,
                Some(Liveness::Gone),
                true,
                &Ok(TargetState::WholeAbsent)
            ),
            OwnedTargetState::NativeBoundaryAbsent
        );
        assert_eq!(
            owned_target_state(
                Boundary::LeaderOnly,
                Some(Liveness::Gone),
                true,
                &Ok(TargetState::WholeAbsent)
            ),
            OwnedTargetState::UnknownIncompleteCoverage
        );
        assert_eq!(
            owned_target_state(
                native,
                Some(Liveness::Gone),
                true,
                &Ok(TargetState::LeaderAbsent)
            ),
            OwnedTargetState::UnknownIncompleteCoverage
        );
        assert_eq!(
            owned_target_state(
                native,
                Some(Liveness::Gone),
                true,
                &Err(std::io::Error::other("target query failed"))
            ),
            OwnedTargetState::UnknownQueryFailed
        );
        assert_eq!(
            owned_target_state(
                native,
                Some(Liveness::Unknown),
                true,
                &Ok(TargetState::WholeAbsent)
            ),
            OwnedTargetState::NativeBoundaryAbsent
        );
        assert_eq!(
            owned_target_state(
                native,
                Some(Liveness::Gone),
                false,
                &Ok(TargetState::WholeAbsent)
            ),
            OwnedTargetState::UnknownReapUnconfirmed
        );
    }

    #[test]
    fn a_protocol_key_read_failure_never_admits_its_input_consumer() {
        use dudes_providers::adapter::{Adapter as _, TurnSession};
        use dudes_providers::codex::{managed::CodexAdapter, startup::CodexStartup};
        use dudes_providers::launch::LaunchInput;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Count(Arc<AtomicUsize>);
        impl crate::ChildStreams for Count {
            fn watch(
                &self,
                _key: Option<&dudes_supervisor::host::AdoptionKey>,
            ) -> Box<dyn crate::ChildStream> {
                panic!("protocol launch used legacy consumer");
            }
            fn watch_protocol(
                &self,
                _key: &dudes_supervisor::host::AdoptionKey,
                _writer: crate::ProtocolWriter,
            ) -> Option<Box<dyn crate::ChildStream>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                None
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let mut host = RealHost::watching(Arc::new(Count(Arc::clone(&calls))));
        host.setup_fault = Some(SetupFault::KeyRead);
        let mut launch = spec("park", None);
        launch.stdin = LaunchInput::Protocol(CodexStartup::new(
            launch.cwd.clone(),
            launch.cwd.clone(),
            launch.cwd.clone(),
            "must not reach child".into(),
            TurnSession::Assigned,
            CodexAdapter::default().model_selection(),
        ));
        let result = host.spawn(&launch, &ManagedInstall::default());
        assert!(matches!(result, SpawnResult::CleanupUnconfirmed { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(host.owned(), 1, "failed child ownership is retained");
        expect_unresolved_shutdown(&mut host, "protocol key was unavailable");
    }

    /// The other ordering of the key-read row, which made the producer matrix fail intermittently
    /// when its expectation assumed only one.
    ///
    /// **The expectation, not the guard, has to allow both orderings.** Every cell of
    /// [`every_post_exec_setup_error_uses_the_same_cleanup_evidence`] assumes the child is still
    /// unreaped when cleanup reaches it — which a parked helper normally is, but that is an
    /// assumption about a race and not a fact the test controls. When the child exits first the
    /// answer changes, and the matrix called that a failure. `KeyReadAfterReap` makes the reaped
    /// ordering the only one available, so the case can be asserted instead of waited for.
    ///
    /// **The answer asserted here is the intended one, not the observed one.** Cleanup chooses its guard from
    /// the current `ExitSlot` and the stored key: unreaped uses the reservation, reaped with a key uses the key comparison, and reaped
    /// without a key sends no signal. Any weaker coverage, failed or withheld signal, … retains `Owned`
    /// and maps to `CleanupUnconfirmed`. A key read that failed leaves no key, so a reaped child
    /// leaves cleanup no authority to signal under; with no signal there is no post-escalation
    /// quiet, and `NotLaunched` needs the whole conjunction. Complete cleanup *evidence* does not
    /// make an unauthorised cleanup confirmed. The row stays owned, which is what `shutdown` needs
    /// in order to still reach it.
    ///
    /// The racing pair, by symbol: `stream::settle`'s `reaped.store(true, Ordering::Release)`
    /// against the `exit.reaped()` that `kill::cleanup_target` passes to `kill::failed_spawn_guard`.
    #[test]
    fn a_failed_spawn_reaped_before_cleanup_without_a_key_is_unconfirmed() {
        let mut host = RealHost::with_grace(Duration::from_millis(300));
        host.setup_fault = Some(SetupFault::KeyReadAfterReap);
        host.cleanup_evidence_for_test = Some(TestCleanupEvidence::Complete);
        let result = host.spawn(&spec("exit", None), &ManagedInstall::default());
        let reason = expect_setup_result(
            result,
            host.cleanup_answer_for_test.as_deref(),
            false,
            "key read after reap",
        );
        assert!(!reason.is_empty(), "the setup reason was not preserved");
        assert_eq!(
            host.cleanup_answer_for_test.as_deref(),
            Some("Unconfirmed(None)"),
            "a refused guard is uncertain with no cleanup error of its own: nothing failed, there \
             was simply no authority to signal under"
        );
        assert_eq!(
            host.owned(),
            1,
            "an unconfirmed cleanup released the owned row"
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while !host
            .children
            .values()
            .next()
            .unwrap()
            .reap_confirmed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            assert!(
                Instant::now() < deadline,
                "the keyless child's successful reap was not published"
            );
            sleep(Duration::from_millis(5));
        }
        let retained = host.owned_targets(1);
        assert_eq!(retained.status, OwnedTargetsStatus::Complete);
        assert_eq!(retained.targets.len(), 1);
        assert!(retained.targets[0].key.is_none());
        assert!(retained.targets[0].native_absence_receipt.is_none());
        assert_eq!(retained.targets[0].cwd, std::env::current_dir().unwrap());
        assert_eq!(
            retained.targets[0].state,
            OwnedTargetState::NativeBoundaryAbsent
        );
        expect_unresolved_shutdown(&mut host, "reaped-first key read");
    }

    /// One cell of the producer matrix: one setup fault against one cleanup evidence answer.
    ///
    /// **The confirmed cells force their ordering rather than assume it.** All of them depend on
    /// cleanup finding the child unreaped, and before this that was an assumption about a race:
    /// when `stream::settle` published its reap first, `kill::cleanup_target` read `exit.reaped()`
    /// as true and took the other branch — no reservation, and then for the key-read fault no key
    /// either, or for the writer faults a key comparison whose probe finds the pid already gone —
    /// answering `Unconfirmed(None)`, correctly, against a cell that expected confirmation. That is
    /// the flake: the answer was right and the expectation wrong.
    /// The latch makes the unreaped ordering the only one available.
    ///
    /// Returns the preserved setup reason, which the caller compares across a fault's cells.
    fn run_matrix_cell(
        fault: SetupFault,
        label: &str,
        stdin: Option<Vec<u8>>,
        evidence: TestCleanupEvidence,
        confirmed: bool,
        mark: &Path,
    ) -> String {
        let mut host = RealHost::with_grace(Duration::from_millis(300));
        host.setup_fault = Some(fault);
        host.cleanup_evidence_for_test = Some(evidence);
        let ordering =
            (evidence == TestCleanupEvidence::Complete).then(crate::reap_hold::Hold::default);
        host.hold_reap_until_cleanup_signals = ordering.clone();

        let result = host.spawn(
            &spec_with_mark("park", stdin, Some(mark)),
            &ManagedInstall::default(),
        );
        let reason = expect_setup_result(
            result,
            host.cleanup_answer_for_test.as_deref(),
            confirmed,
            label,
        );
        assert_eq!(
            host.owned(),
            usize::from(!confirmed),
            "{label} classified ownership incorrectly"
        );
        // The latch is evidence only if it actually held something. `Failsafe` would mean cleanup
        // never signalled and the ordering this cell claims to assert was never forced — a green
        // run there would be green for the old reason.
        if let Some(ordering) = &ordering {
            assert_eq!(
                ordering.release_reason(),
                Some(crate::reap_hold::Release::Explicit),
                "{label}: the reap was not held until cleanup signalled, so this cell asserted an \
                 ordering it did not force"
            );
        }
        if confirmed {
            host.shutdown()
                .expect("confirmed cleanup left an idle host");
        } else {
            expect_unresolved_shutdown(&mut host, "test cleanup");
        }
        reason
    }

    /// Every post-exec setup producer reaches the same cleanup classifier.
    /// The real broken-pipe arm needs no injection; the otherwise unreachable thread failures and
    /// deterministic key-read failure use the crate-private test field.
    #[test]
    fn every_post_exec_setup_error_uses_the_same_cleanup_evidence() {
        let marks = tempfile::tempdir().expect("producer-matrix marks");
        let cases = [
            (
                SetupFault::WriterCreation,
                "writer creation",
                Some(b"task".to_vec()),
            ),
            (
                SetupFault::WriterWrite,
                "stdin write",
                Some(b"task".to_vec()),
            ),
            (
                SetupFault::WriterPanic,
                "writer panic",
                Some(b"task".to_vec()),
            ),
            (SetupFault::KeyRead, "key read", None),
        ];
        for (case_index, (fault, label, stdin)) in cases.into_iter().enumerate() {
            let mut reasons = Vec::new();
            for (evidence, confirmed) in [
                (TestCleanupEvidence::Complete, true),
                (TestCleanupEvidence::MissingBoundary, false),
                (TestCleanupEvidence::SignalWithheld, false),
                (TestCleanupEvidence::QuietWithheld, false),
                (TestCleanupEvidence::ReapWithheld, false),
            ] {
                let mark = marks
                    .path()
                    .join(format!("case-{case_index}-{evidence:?}.pid"));
                reasons.push(run_matrix_cell(
                    fault.clone(),
                    label,
                    stdin.clone(),
                    evidence,
                    confirmed,
                    &mark,
                ));
            }
            assert!(
                reasons.windows(2).all(|pair| pair[0] == pair[1]),
                "{label} did not preserve one original setup reason: {reasons:?}"
            );
        }

        // The real broken-pipe producer remains the positive control for a process-created write
        // failure; the injected parked arm above is what makes both evidence outcomes deterministic.
        let mut host = RealHost::with_grace(Duration::from_millis(300));
        let result = host.spawn(
            &spec("exit", Some(vec![b'x'; 1 << 20])),
            &ManagedInstall::default(),
        );
        let reason = expect_setup_result(
            result,
            host.cleanup_answer_for_test.as_deref(),
            false,
            "real stdin EPIPE",
        );
        assert!(!reason.is_empty());
        assert_eq!(host.owned(), 1);
        expect_unresolved_shutdown(&mut host, "real EPIPE cleanup");

        let mut host = RealHost::with_grace(Duration::ZERO);
        let successful_mark = marks.path().join("successful-spawn.pid");
        assert!(matches!(
            host.spawn(
                &spec_with_mark("park", None, Some(&successful_mark)),
                &ManagedInstall::default(),
            ),
            SpawnResult::Spawned(_)
        ));
        assert_eq!(
            host.owned(),
            1,
            "the successful control stays normally owned"
        );
        expect_unresolved_shutdown(&mut host, "successful control cleanup");
    }

    fn missing_boundary_with_escaped_descendant() {
        let dir = tempfile::tempdir().expect("tempdir");
        let leader = dir.path().join("boundary-leader.pid");
        let descendant = dir.path().join("escaped-descendant.pid");
        let mut host = RealHost::with_grace(Duration::from_millis(100));
        host.setup_fault = Some(SetupFault::MissingBoundary(descendant.clone()));

        assert!(matches!(
            host.spawn(
                &spec_with_mark("descendant", None, Some(&leader)),
                &ManagedInstall::default()
            ),
            SpawnResult::CleanupUnconfirmed { .. }
        ));
        assert_eq!(host.owned(), 1, "every acquired capability remains owned");
        let descendant_pid = await_number(&descendant);
        await_beating(&descendant);

        expect_unresolved_shutdown(&mut host, "ordinary shutdown retries acquired capabilities");
        await_beating(&descendant);
        stop_test_descendant(descendant_pid);
        await_stopped(
            descendant_pid,
            &descendant,
            "test-owned cleanup did not stop descendant",
        );

        let present_dir = tempfile::tempdir().expect("present-boundary tempdir");
        let present_leader = present_dir.path().join("present-leader.pid");
        let present_descendant = present_dir.path().join("escaped-descendant.pid");
        let mut present = RealHost::with_grace(Duration::from_millis(100));
        assert!(matches!(
            present.spawn(
                &spec_with_mark("descendant", None, Some(&present_leader)),
                &ManagedInstall::default()
            ),
            SpawnResult::Spawned(_)
        ));
        let present_descendant_pid = await_number(&present_descendant);
        await_beating(&present_descendant);
        expect_unresolved_shutdown(&mut present, "addressable group/job remains incomplete");
        await_stopped(
            present_descendant_pid,
            &present_descendant,
            "the present group/job control did not reach its descendant",
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_unix_group_with_a_live_descendant_is_unconfirmed() {
        missing_boundary_with_escaped_descendant();
    }

    #[cfg(windows)]
    #[test]
    fn failed_windows_job_assignment_with_an_escaped_descendant_is_unconfirmed() {
        missing_boundary_with_escaped_descendant();
    }

    /// Group `EPERM`, against the real platform: a child killed while its reap has not happened yet is
    /// reported as killed, and the host stops owning it.
    ///
    /// **Why this is not the scripted arm.** `kill::tests::permission_denied_group_is_retained_or_
    /// reported` hands the escalation an `EPERM` and checks what the policy does with it. That is a
    /// different question from the one that was wrong: what the *platform* returns for a group
    /// holding nothing but this host's own unreaped zombie, and what the host then does. Only a
    /// real `spawn` → `kill` can answer it, and only with the reap window held open, because
    /// otherwise the watcher reaps first and the window is never entered.
    ///
    /// **Why it is deterministic.** Nothing here sleeps. The hold parks the watcher before its
    /// `wait`; `await_number` proves the child is running before the kill, so a `Signalled` here is
    /// not a kill of nothing; and the hold is lifted by the kill path's own observation of the
    /// platform's `EPERM`, never by this thread. A test that released the hold from here would lose
    /// the race it is trying to create — the reap would land inside the kill's poll interval and
    /// the query would never see the zombie — so the release reason is the evidence that the kill
    /// met the window rather than missing it.
    ///
    /// **Which reason, and why that is not the same question as which window.** The final assertion
    /// used to name `PresenceEperm` alone, and that was over-specified. `plat::signal`
    /// signals the leader *before* it sweeps the group, so the child this test kills may already be
    /// a zombie by the time the sweep runs, and the sweep's own `EPERM` then lifts the hold first
    /// with `GroupSignalEperm`. Both orders are correct behaviour; the reason records which window
    /// fired **first**, not which windows the kill entered.
    ///
    /// So both errno-keyed reasons are admitted, and they are **named** rather than widened to
    /// `is_some()`: `Failsafe` means the window was never entered, `Explicit` means a
    /// test-side step released it by name, and either would make this test pass while proving
    /// nothing. Only the two below are produced by the platform answering `EPERM` to the kill.
    ///
    /// **The property this test exists for is unchanged by that**, which is the part worth being
    /// sure of before loosening anything. It is this: a group holding nothing but this host's
    /// own unreaped zombie answers `EPERM` to the presence query, and `classify_presence` must
    /// map it to `Unsignallable` so `wait_until_quiet` keeps polling. If it returned `Err` instead,
    /// `wait_until_quiet` would fail and the outcome below would be `Failed(EPERM)` — in **either**
    /// interleaving, because the escalation reaches that query either way. The failing signal is the
    /// outcome assertion; the release reason is only the anti-vacuity guard beside it.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_kill_while_the_reap_is_held_open_is_signalled_and_leaves_nothing_owned() {
        use super::KillOutcome;
        use super::reap_hold::{Hold, Release};

        let dir = tempfile::tempdir().expect("held-reap tempdir");
        let mark = dir.path().join("held-reap.pid");
        let hold = Hold::default();
        let mut host = RealHost::new();
        host.setup_fault = Some(SetupFault::HoldReap(hold.clone()));

        let key = match host.spawn(
            &spec_with_mark("park", None, Some(&mark)),
            &ManagedInstall::default(),
        ) {
            SpawnResult::Spawned(key) => key,
            other => panic!("the held-reap child did not spawn: {other:?}"),
        };
        assert_eq!(host.owned(), 1, "the spawned child is owned");
        // Anti-vacuity: the helper has announced its own pid, so it is running when the kill lands.
        assert_eq!(
            await_number(&mark),
            key.pid,
            "the helper announced a pid that is not the spawned child's"
        );

        let outcome = host.kill(&key);
        assert!(
            matches!(outcome, KillOutcome::Signalled),
            "a child this host killed was not reported as killed: {outcome:?}"
        );
        assert_eq!(
            host.owned(),
            0,
            "a successfully killed child is still owned (invariant 10's count)"
        );
        let reason = hold.release_reason();
        assert!(
            matches!(
                reason,
                Some(Release::PresenceEperm | Release::GroupSignalEperm)
            ),
            "the kill never met the held-open reap window, so this run tested nothing: {reason:?}"
        );
    }

    /// The sibling of the test above: the same symptom arriving by the
    /// **delivered** path rather than the query path.
    ///
    /// The child exits on its own with its reap held, so by the time `kill` runs the group holds
    /// nothing but this host's own zombie. `escalate`'s first act is a signal, not a query, so the
    /// group sweep is what meets that state: `killpg` answers `EPERM`, which is `nfound == 0` — the
    /// group is there and the call signalled nothing in it — and before this record that was a
    /// failed kill: `Failed(EPERM)`, with the ownership row re-inserted for a child already dead.
    /// Invariant 10's count told untruthfully, by a different route than the one an earlier fix closed.
    ///
    /// **The anti-vacuity assertion names `GroupSignalEperm` and nothing weaker.** A `!= Failsafe`
    /// would be satisfied by any release at all, and a release *by name* would prove only that some
    /// test-side code ran. Only the platform's own `EPERM`, observed at the delivered site,
    /// produces this variant, so the assertion is that the window was entered **there** — which is
    /// the whole claim. The query path's `PresenceEperm` is a different window and a different
    /// test, which is why the two reasons are separate variants.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_delivered_group_sweep_against_our_own_zombie_is_signalled_and_leaves_nothing_owned() {
        use super::KillOutcome;
        use super::reap_hold::{Hold, Release};

        let hold = Hold::default();
        let mut host = RealHost::new();
        host.setup_fault = Some(SetupFault::HoldReap(hold.clone()));

        let key = match host.spawn(&spec("exit", None), &ManagedInstall::default()) {
            SpawnResult::Spawned(key) => key,
            other => panic!("the held-reap child did not spawn: {other:?}"),
        };
        assert_eq!(host.owned(), 1, "the spawned child is owned");

        // Anti-vacuity, and the state under test: wait until the group answers `EPERM`, which on
        // Darwin is exactly "this group holds nothing but an unreaped zombie". Asked through
        // `rustix` directly rather than through `presence_checked`, so this poll neither fires the
        // query path's release hook nor borrows the classification it is about to test.
        let group = rustix::process::Pid::from_raw(i32::try_from(key.pid).expect("pid fits"))
            .expect("a non-zero pid");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !matches!(
            rustix::process::test_kill_process_group(group),
            Err(rustix::io::Errno::PERM)
        ) {
            assert!(
                Instant::now() < deadline,
                "the child never became an unreaped zombie in its own group"
            );
            sleep(Duration::from_millis(5));
        }

        let outcome = host.kill(&key);
        assert!(
            matches!(outcome, KillOutcome::Signalled),
            "a group sweep that signalled nothing was reported as a failed kill: {outcome:?}"
        );
        assert_eq!(
            host.owned(),
            0,
            "a successfully killed child is still owned (invariant 10's count)"
        );
        assert_eq!(
            hold.release_reason(),
            Some(Release::GroupSignalEperm),
            "the delivered group sweep never met the zombie window, so this run tested nothing"
        );
    }
}

/// Run `start` while no other child of this process is being created.
///
/// A descriptor is only close-on-exec once it has been marked, and on macOS the standard library
/// creates a pipe and marks it in two steps. A child forked by another thread between them would
/// inherit the pipe's write end, and a stray copy of a keeper's write end or an agent's stdin write
/// end would keep that pipe open after its owner is gone. Every child this crate starts goes
/// through here so that no fork can fall inside another spawn's two steps.
///
/// Code in this process that forks a child without this crate, such as the attached terminal host,
/// takes the same lock around its fork. `start` must not start a child through this crate.
pub fn serialize_spawn<T>(start: impl FnOnce() -> T) -> T {
    static SPAWNING: Mutex<()> = Mutex::new(());
    let _one_at_a_time = SPAWNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    start()
}

pub(crate) fn spawn_serialized(command: &mut Command) -> io::Result<Child> {
    serialize_spawn(|| command.spawn())
}

/// End an agent that has no keeper and was never reported as launched: its whole group, then the
/// leader itself in case the group could not be signalled, and the reap.
#[cfg(unix)]
fn end_unkept_agent(mut child: Child) {
    let _ = unix::signal_group(unix::as_pid(child.id()), kill::Sig::Kill);
    let _ = child.kill();
    let _ = child.wait();
}

/// `spec` as a [`Command`]: `argv[0]` is the program and every other element is one argument.
///
/// Invariant 7 in one function — there is no string anywhere on this path, no shell, and no
/// interpolation. `env_clear` first, because [`LaunchSpec::env`] is the child's *complete*
/// environment and inheriting the daemon's would hand a child the operator's vendor
/// credentials that the scrub took out.
///
/// **Crate-level rather than an associated function of [`RealHost`], so that predicate children and agent children share one implementation.**
/// [`OneShot`](oneshot::OneShot) creates the other class of child this crate owns, and invariant 7
/// is the rule for both: one implementation, so a predicate child cannot acquire a shell or
/// an inherited environment by being built somewhere else. What differs between the two callers is
/// what each does with the child's output, and each applies that on top of what this returns: a
/// predicate's stdout and stderr are pipes because its exit code and output tail are the evidence,
/// and an agent child's stdout is a pipe when the host has somewhere to send it, because a managed
/// turn's result is parsed out of it.
pub(crate) fn command(spec: &LaunchSpec) -> io::Result<Command> {
    let (program, args) = spec
        .argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "LaunchSpec.argv is empty"))?;
    let mut command = Command::new(program);
    command.args(args);
    command.env_clear();
    for var in &spec.env {
        command.env(var.name(), var.expose_value());
    }
    command.current_dir(&spec.cwd);
    // `stdout` and `stderr` are left at the standard library's default here, which is to inherit
    // the daemon's, and the caller narrows `stdout` when it has a reader for it.
    //
    // `LaunchSpec` has no field for that, and the reason is not that the field would be
    // unnecessary — it is that the field would be *harmful*. `RealHost::spawn` derives the pipe
    // and its reader from one `Option`, so no caller can ask for a pipe that nothing drains; a
    // field here would hand that back, letting a caller ask for one on a host with no sink. And a
    // pipe with nobody reading blocks the child the first time it fills the buffer, which is worse
    // than inheriting.
    //
    // The lifetime question the field was once thought to need — per turn or per agent — is
    // settled and settles nothing here: a managed turn *is* one process, so the stream is per
    // child and always wanted, and what varies is only whether the host was given a sink at all
    // (`RealHost::watching`). See `src/stream.rs`.
    command.stdin(
        if matches!(spec.stdin, dudes_providers::launch::LaunchInput::Null) {
            Stdio::null()
        } else {
            Stdio::piped()
        },
    );
    Ok(command)
}

/// Deliver `spec.stdin` and close the pipe. A child whose stdin stays open waits on it.
pub(crate) fn feed_stdin(child: &mut Child, spec: &LaunchSpec) -> io::Result<()> {
    let bytes = match &spec.stdin {
        dudes_providers::launch::LaunchInput::Null => return Ok(()),
        dudes_providers::launch::LaunchInput::WriteThenClose(bytes) => bytes,
        dudes_providers::launch::LaunchInput::Protocol(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "protocol input requires a managed protocol consumer",
            ));
        }
    };
    if let Some(pipe) = child.stdin.take() {
        write_stdin(pipe, bytes)?;
    }
    Ok(())
}

/// Write every byte to a child's standard input, then close it.
fn write_stdin(mut pipe: ChildStdin, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    pipe.write_all(bytes)
}

impl Default for RealHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessHost for RealHost {
    /// Write the install, exec, and read the child's adoption key while the identifier is still
    /// reserved by this host being the child's unreaped parent.
    ///
    /// Once the exec has happened, every result goes through `self.children`. Post-exec setup
    /// errors are cleaned only against that entry and remain [`SpawnResult::CleanupUnconfirmed`]
    /// unless all four absence facts are positively established.
    #[expect(
        clippy::too_many_lines,
        reason = "the post-exec ownership, stream handoff and cleanup order is one invariant-bearing chronology"
    )]
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        if matches!(
            spec.stdin,
            dudes_providers::launch::LaunchInput::Protocol(_)
        ) && self.streams.is_none()
        {
            return SpawnResult::NotLaunched {
                reason: io::Error::new(
                    io::ErrorKind::Unsupported,
                    "protocol input requires a managed protocol consumer",
                ),
            };
        }
        for directory in &install.directories {
            if let Err(reason) = create_owner_only_dir(directory) {
                return SpawnResult::NotLaunched { reason };
            }
        }
        if let Err(reason) = Self::write_install(&install.files) {
            return SpawnResult::NotLaunched { reason };
        }
        let mut command = match command(spec) {
            Ok(command) => command,
            Err(reason) => return SpawnResult::NotLaunched { reason },
        };
        // The pipe and its reader come from one `Option`, read twice in this function, so no
        // *caller* can ask for one without the other — which is the reason `LaunchSpec` has no
        // field for it. It is a convention held here and not a property of a type: making this
        // unconditional compiles, and what saves the child then is `stream::read_in_background`
        // dropping the pipe so the child takes `EPIPE`. `stderr` stays inherited: a vendor CLI's
        // diagnostics belong in the daemon's own log, and nothing parses them.
        if self.streams.is_some() {
            command.stdout(Stdio::piped());
        }
        let prep = match plat::prepare(&mut command) {
            Ok(prep) => prep,
            Err(reason) => return SpawnResult::NotLaunched { reason },
        };

        // The last `?` before a process exists, and the reason there is not another one until the
        // `insert` below. `?` drops the `Child`, and dropping a `Child` neither signals it nor
        // reaps it — so an early return past this line leaves a running process, in a process
        // group or job of its own, that this host holds no record of and `shutdown` can never
        // reach. That is invariant 10 failing inside the one function that creates the thing the
        // invariant is about, and it is the same judgement the doc comment above already makes
        // about the key read, applied to the two steps that precede it.
        // The slot the daemon armed with `PeerProcesses::begin_spawn` becomes *active* here, one
        // statement before the exec: a peer that connects from this instant on may be this child,
        // and the door must wait for it rather than answer the column that admits its operator
        // credential.
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        let peer_slot = self.peers.activate();
        let launched_at = std::time::Instant::now();
        let mut child = match spawn_serialized(&mut command) {
            Ok(child) => child,
            Err(reason) => {
                #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
                self.peers.pre_exec_failed(peer_slot);
                return SpawnResult::NotLaunched { reason };
            }
        };
        let pid = child.id();

        // Before stdin rather than after: on Windows this is the call that puts the child in its
        // job, and the window between creation and assignment is one the platform layer names and
        // cannot close. Delivering stdin first would widen it by however long the write takes,
        // which for a spec carrying the task text is not a bounded amount of time.
        #[cfg(test)]
        let spawned = match self.setup_fault.as_ref() {
            Some(SetupFault::MissingBoundary(ready)) => {
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                while !ready.exists() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the escaped descendant never announced itself at {}",
                        ready.display()
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                plat::grip_spawned_without_boundary_for_test(prep, &child)
            }
            _ => plat::grip_spawned(prep, &child),
        };
        #[cfg(not(test))]
        let spawned = plat::grip_spawned(prep, &child);
        let Spawned {
            grip,
            boundary,
            weakened,
        } = spawned;
        // The active slot becomes a tracked pid-and-job the instant the job assignment is known.
        #[cfg(windows)]
        self.peers.register(pid, grip.job(), peer_slot);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.peers.register(pid, peer_slot);

        // After the peer registration and before the stdin writer, so a task written to stdin only
        // ever reaches an agent that already has a keeper. Before the child moves to its watcher, so
        // the agent is not yet reaped and its group still exists to join.
        #[cfg(unix)]
        let lifeline = match self
            .keeper_executable
            .as_deref()
            .map(|exe| keeper::start(exe, pid))
        {
            None => None,
            Some(Ok(lifeline)) => Some(lifeline),
            Some(Err(reason)) => {
                end_unkept_agent(child);
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                self.peers.exited(pid, peer_slot);
                return SpawnResult::NotLaunched { reason };
            }
        };

        // The scoped writer borrows the launch bytes instead of copying a potentially large task,
        // and it cannot outlive this call. The child moves to its stdout watcher before the join,
        // so its two pipes can make progress independently: a child may fill stdout before it
        // starts reading stdin without meeting this host in a two-full-pipes cycle.
        //
        // No failure past the exec returns from the scope. A writer that could not start, failed
        // its write, or panicked still has its pipe closed and its child handed to `watch`; a key
        // read failure does the same. Only after `Owned` is inserted below does the stored result
        // reach the caller.
        #[cfg(test)]
        let (fail_writer_creation, fail_writer, panic_writer, fail_key_read) = (
            matches!(self.setup_fault.as_ref(), Some(SetupFault::WriterCreation)),
            matches!(self.setup_fault.as_ref(), Some(SetupFault::WriterWrite)),
            matches!(self.setup_fault.as_ref(), Some(SetupFault::WriterPanic)),
            matches!(
                self.setup_fault.as_ref(),
                Some(SetupFault::KeyRead | SetupFault::KeyReadAfterReap)
            ),
        );
        #[cfg(not(test))]
        let (fail_writer_creation, fail_writer, panic_writer, fail_key_read) =
            (false, false, false, false);
        // Read before the map is borrowed for cleanup, because the fault and the row live on the
        // same `self`.
        #[cfg(test)]
        let reap_before_cleanup = matches!(
            self.setup_fault.as_ref(),
            Some(SetupFault::KeyReadAfterReap)
        );
        // Armed here, with the child alive and its identifier known, and before anything can reach
        // `kill`: the hold is keyed by pid, and `unix::prepare` has already made this child its own
        // group leader, so that pid is the name the presence query will carry.
        #[cfg(all(test, target_os = "macos"))]
        let held_open = match self.setup_fault.as_ref() {
            Some(SetupFault::HoldReap(hold)) => {
                hold.arm(pid);
                Some(hold.clone())
            }
            _ => None,
        };
        // The other use of the same latch, and the only one that exists on every platform: the
        // watcher is held *before* its reap until cleanup says so, which is what turns "the child
        // is probably still unreaped" into a fact the assertions downstream can rest on.
        #[cfg(test)]
        let held_closed = self.hold_reap_until_cleanup_signals.clone();
        // The cleanup adapter's copy, taken here because the map is borrowed by then.
        #[cfg(test)]
        let cleanup_hold = held_closed.clone();
        #[cfg(all(test, target_os = "macos"))]
        let reap_hold = held_open.or(held_closed);
        #[cfg(all(test, not(target_os = "macos")))]
        let reap_hold = held_closed;
        let (delivered, read_key) = thread::scope(|scope| {
            let mut writer_error = None;
            let mut protocol_pipe = None;
            let writer = match (child.stdin.take(), &spec.stdin) {
                (Some(pipe), dudes_providers::launch::LaunchInput::Protocol(_)) => {
                    protocol_pipe = Some(pipe);
                    None
                }
                (Some(pipe), dudes_providers::launch::LaunchInput::WriteThenClose(bytes)) => {
                    if fail_writer_creation {
                        writer_error = Some(io::Error::other(
                            "the child's stdin writer thread could not start",
                        ));
                        drop(pipe);
                        None
                    } else {
                        match thread::Builder::new()
                            .name(format!("dudes-child-{pid}-stdin"))
                            .spawn_scoped(scope, move || {
                                assert!(!panic_writer, "injected stdin writer panic");
                                if fail_writer {
                                    Err(io::Error::other("injected stdin write failure"))
                                } else {
                                    write_stdin(pipe, bytes)
                                }
                            }) {
                            Ok(writer) => Some(writer),
                            Err(error) => {
                                writer_error = Some(error);
                                None
                            }
                        }
                    }
                }
                _ => None,
            };

            // The key is read while the identifier is still reserved. It has to precede the
            // watcher, which may reap and release that identifier as soon as it starts.
            let mut read_key = self.read_key(pid, &grip);
            if fail_key_read {
                read_key = Err(io::Error::other("injected adoption-key read failure"));
            }
            // The full adoption key is what turns a tracked pid into something `resolve_pending`
            // can compare a peer against, so it is published the moment it is read.
            #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
            if let Ok(key) = read_key.as_ref() {
                self.peers.keyed(pid, key);
            }
            let mut protocol = None;
            let consumer = if matches!(
                spec.stdin,
                dudes_providers::launch::LaunchInput::Protocol(_)
            ) {
                if let (Some(streams), Ok(key), None) = (&self.streams, &read_key, &weakened) {
                    let input = ProtocolWriter::prepare(key.clone(), streams, launched_at);
                    let consumer = streams.watch_protocol(key, input.clone());
                    if consumer.is_none() {
                        writer_error = Some(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "protocol consumer refused this child",
                        ));
                    }
                    protocol = Some(input);
                    consumer
                } else {
                    writer_error = Some(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "protocol input requires a fully owned child key",
                    ));
                    None
                }
            } else {
                self.streams.as_ref().map(|streams| {
                    streams.watch(if weakened.is_none() {
                        read_key.as_ref().ok()
                    } else {
                        None
                    })
                })
            };
            let reap_confirmed = Arc::new(AtomicBool::new(false));
            let confirm_successful_reap = {
                let confirmed = Arc::clone(&reap_confirmed);
                move || confirmed.store(true, Ordering::Release)
            };
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let confirm_reap_and_release_unix_peer = {
                let peers = self.peers.clone();
                move || {
                    confirm_successful_reap();
                    peers.exited(pid, peer_slot);
                }
            };
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let confirm_reap_and_retain_peer_job = confirm_successful_reap;
            let exit = stream::watch(
                child,
                consumer,
                protocol.clone(),
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                confirm_reap_and_release_unix_peer,
                #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                confirm_reap_and_retain_peer_job,
                launched_at,
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                plat::exit_group(&grip),
                #[cfg(test)]
                reap_hold,
            );

            if protocol.is_some() && matches!(&exit, ExitSlot::Held { .. }) {
                writer_error = Some(io::Error::other("protocol stdout watcher could not start"));
            }

            // The ownership entry exists before stdin delivery completes. No post-exec error can
            // escape without the exact child and every acquired capability being reachable here.
            self.children.insert(
                pid,
                Owned {
                    pid,
                    exit,
                    grip,
                    boundary,
                    key: read_key.as_ref().ok().cloned(),
                    cwd: spec.cwd.clone(),
                    reap_confirmed,
                    native_receipt: NativeBoundaryReceipt::fresh(),
                    protocol: protocol.clone(),
                    #[cfg(unix)]
                    lifeline,
                },
            );
            self.index_native_target(pid);

            if let Some(protocol) = protocol {
                if writer_error.is_none() {
                    writer_error = match protocol_pipe.take() {
                        Some(pipe) => protocol.start(pipe).err(),
                        None => Some(io::Error::other("protocol child has no stdin pipe")),
                    };
                }
                if writer_error.is_some() {
                    protocol.fail();
                }
            }
            drop(protocol_pipe);

            // Join only after the stdout watcher is independently runnable. A scoped join also
            // turns a panic into data here; no detached writer can retain the pipe after spawn
            // returns.
            let delivered = match (writer_error, writer) {
                (Some(error), None) => Err(error),
                (None, Some(writer)) => match writer.join() {
                    Ok(result) => result,
                    Err(_) => Err(io::Error::other("the child's stdin writer thread panicked")),
                },
                (None, None) => Ok(()),
                (Some(_), Some(_)) => unreachable!("a failed thread start returns no handle"),
            };
            (delivered, read_key)
        });

        let read = delivered.and(read_key);
        // A weakened hold outranks a failed read. "The host does not fully own this child" and
        // "the host cannot name this child" are different decisions for the caller to make, and
        // the first is the one that has to be made first.
        let key = match weakened {
            Some(error) => Err(error),
            None => read,
        };

        match key {
            Ok(key) => SpawnResult::Spawned(key),
            Err(reason) => {
                let cleanup = {
                    let owned = self
                        .children
                        .get_mut(&pid)
                        .expect("the post-exec child is owned before setup completes");
                    // Forces the reaped-first ordering, and forces it by waiting for the fact
                    // rather than by sleeping until it is likely: `ExitSlot::wait` blocks until the
                    // watcher has published the reap and caches the result, so the later wait
                    // inside cleanup replays it instead of blocking a second time.
                    #[cfg(test)]
                    if reap_before_cleanup {
                        let _ = owned.exit.wait();
                    }
                    #[cfg(test)]
                    if let Some(evidence) = self.cleanup_evidence_for_test {
                        kill::cleanup_failed_spawn_with_evidence_for_test(
                            owned,
                            self.boot.as_host_boot(),
                            self.grace,
                            evidence,
                            cleanup_hold,
                        )
                    } else {
                        kill::cleanup_failed_spawn(owned, self.boot.as_host_boot(), self.grace)
                    }
                    #[cfg(not(test))]
                    kill::cleanup_failed_spawn(owned, self.boot.as_host_boot(), self.grace)
                };
                #[cfg(test)]
                {
                    self.cleanup_answer_for_test = Some(format!("{cleanup:?}"));
                }
                match cleanup {
                    kill::Cleanup::Confirmed => {
                        self.children.remove(&pid);
                        // The tracked pid goes with the child. Its slot becomes released rather
                        // than forgotten, so a `finish_spawn` that arrives after the cleanup
                        // still finds it.
                        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
                        self.peers.release(pid);
                        SpawnResult::NotLaunched { reason }
                    }
                    kill::Cleanup::Unconfirmed(error) => {
                        if let Some(error) = error {
                            tracing::error!(
                                %error,
                                pid,
                                "failed-spawn cleanup did not confirm absence"
                            );
                        }
                        SpawnResult::CleanupUnconfirmed { reason }
                    }
                }
            }
        }
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        if let Some(owned) = self.children.get(&key.pid)
            && owned.key.as_ref().is_some_and(|stored| {
                stored.pid == key.pid
                    && stored.start_time == key.start_time
                    && stored.boot == key.boot
            })
            && let Some(protocol) = &owned.protocol
        {
            protocol.fail();
        }
        let outcome = kill::kill(
            &mut self.children,
            key,
            self.boot.as_host_boot(),
            self.grace,
        );
        // Whatever the outcome, the entry is gone from `children` iff the kill reaped it, and the
        // peer relation follows `children` rather than the outcome: a pid this host no longer owns
        // must not keep answering for a peer.
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        if !self.children.contains_key(&key.pid) {
            self.peers.release(key.pid);
        }
        outcome
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        liveness(key, self.boot.as_host_boot(), self.probe(key.pid))
    }

    fn probe_native_leader(&self, key: &AdoptionKey) -> ProbeAnswer {
        if !(2..=i32::MAX as u32).contains(&key.pid) || key.start_time == 0 {
            return ProbeAnswer::CouldNotAnswer;
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let observe = || {
            native_leader_observation(self.probe(key.pid), || {
                unix::presence_checked(None, key.pid)
            })
        };
        #[cfg(target_os = "linux")]
        {
            procfs_view::read_current(observe).unwrap_or(ProbeAnswer::CouldNotAnswer)
        }
        #[cfg(target_os = "macos")]
        {
            observe()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            ProbeAnswer::CouldNotAnswer
        }
    }

    fn native_group_presence(&self, pid: u32) -> NativeGroupPresence {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let Some(group) = unix::as_pid(pid).filter(|_| (2..=i32::MAX as u32).contains(&pid))
            else {
                return NativeGroupPresence::Unknown;
            };
            native_group_presence(&unix::presence_checked(Some(group), pid))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = pid;
            NativeGroupPresence::Unknown
        }
    }

    fn pid_namespace(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            procfs_view::read_current(|| self.pid_namespace.clone()).flatten()
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.pid_namespace.clone()
        }
    }

    fn owned_targets(&self, limit: usize) -> OwnedTargets {
        let mut current = self
            .current_native_targets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        query_current_native_targets(&mut current, limit, |pid, entry| {
            match self.children.get(&pid) {
                None => OwnedTargetState::UnknownMissingCapability,
                Some(owned)
                    if owned.native_receipt != entry.receipt
                        || !same_recorded_keys(owned.key.as_ref(), entry.key.as_ref()) =>
                {
                    OwnedTargetState::UnknownIdentity
                }
                Some(owned) => owned_target_state(
                    owned.boundary,
                    owned.key.as_ref().map(|key| self.alive(key)),
                    owned.reap_confirmed.load(Ordering::Acquire),
                    &native_boundary_state(&owned.grip, owned.pid),
                ),
            }
        })
    }

    fn acknowledge_native_boundary_absence(
        &mut self,
        key: &AdoptionKey,
        receipt: &NativeBoundaryReceipt,
    ) -> NativeBoundaryAcknowledgement {
        let current = self
            .current_native_targets
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        let owned = self
            .children
            .get(&key.pid)
            .map(|owned| (owned.key.as_ref(), &owned.native_receipt));
        acknowledge_current_native_target(current, key, receipt, owned)
    }

    /// The read taken once at construction, reported rather than repeated.
    ///
    /// The mapping to the seam's type is [`HostBoot::from`], and it is where the `io::Error` this
    /// host keeps for the spawn refusal becomes a sentence an operator reads.
    fn boot_identity(&self) -> HostBoot {
        HostBoot::from(&self.boot)
    }

    /// Apply the per-signal authority check before each shutdown signal in fixed phase and stable pid order, spending **one**
    /// grace for all rows together.
    ///
    /// Group/job delivery, direct-leader delivery, whole-target absence, boundary coverage and
    /// direct reap remain separate facts. An unproved direct wait is skipped; a row is removed only
    /// when complete coverage, fresh whole-target absence after its final signal decision and reap
    /// all hold. Current production grips do not prove complete arbitrary-tree coverage, so a
    /// nonempty host normally stops its addressable processes, retains their rows and returns an
    /// unresolved-ownership error.
    ///
    /// *When* the daemon calls this, and its ordering against the journal flush and the socket
    /// teardown, is the daemon's. This is the half that makes the guarantee available.
    ///
    /// # Errors
    /// The first concrete platform, query or reap failure after every row was attempted, or the
    /// synthetic unresolved-ownership error when retained rows produced no concrete failure.
    fn shutdown(&mut self) -> io::Result<()> {
        // Shutdown is terminal for peer resolution even when native ownership rows remain
        // retained for a later retry: a late `resolve` must see neither a tracked job nor an
        // armed slot it would wait `PENDING_ABANDON_BOUND` for.
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        self.peers.clear();
        for owned in self.children.values() {
            if let Some(protocol) = &owned.protocol {
                protocol.fail();
            }
        }
        kill::shutdown(&mut self.children, self.boot.as_host_boot(), self.grace)
    }

    /// The same owner-only projection [`RealHost::write_install`] gives a managed install.
    fn write_hooks(&mut self, install: &HookInstall) -> io::Result<()> {
        Self::write_install(&install.files)
    }
}

fn query_current_native_targets(
    current: &mut BTreeMap<u32, CurrentNativeTarget>,
    limit: usize,
    mut inspect: impl FnMut(u32, &CurrentNativeTarget) -> OwnedTargetState,
) -> OwnedTargets {
    let limit = limit.min(MAX_OWNED_TARGET_QUERY_ROWS);
    let targets = current
        .iter_mut()
        .take(limit)
        .map(|(pid, entry)| {
            let state = inspect(*pid, entry);
            let receipt = if state == OwnedTargetState::NativeBoundaryAbsent && entry.key.is_some()
            {
                entry.absence_observed = true;
                Some(entry.receipt.clone())
            } else {
                None
            };
            OwnedTargetEvidence {
                key: entry.key.clone(),
                cwd: entry.cwd.clone(),
                state,
                native_absence_receipt: receipt,
            }
        })
        .collect();
    let omitted = current.len().saturating_sub(limit);
    OwnedTargets {
        targets,
        status: if omitted == 0 {
            OwnedTargetsStatus::Complete
        } else {
            OwnedTargetsStatus::Truncated { omitted }
        },
    }
}

fn acknowledge_current_native_target(
    current: &mut BTreeMap<u32, CurrentNativeTarget>,
    key: &AdoptionKey,
    receipt: &NativeBoundaryReceipt,
    owned: Option<(Option<&AdoptionKey>, &NativeBoundaryReceipt)>,
) -> NativeBoundaryAcknowledgement {
    let Some(entry) = current.get(&key.pid) else {
        return NativeBoundaryAcknowledgement::RefusedUnobserved;
    };
    if entry.receipt != *receipt || !same_recorded_keys(entry.key.as_ref(), Some(key)) {
        return NativeBoundaryAcknowledgement::UnknownIdentity;
    }
    let Some((owned_key, owned_receipt)) = owned else {
        return NativeBoundaryAcknowledgement::UnknownMissingCapability;
    };
    if owned_receipt != receipt || !same_recorded_keys(owned_key, Some(key)) {
        return NativeBoundaryAcknowledgement::UnknownIdentity;
    }
    if !entry.absence_observed {
        return NativeBoundaryAcknowledgement::RefusedUnobserved;
    }
    current.remove(&key.pid);
    NativeBoundaryAcknowledgement::Acknowledged
}

fn owned_target_state(
    boundary: kill::Boundary,
    leader: Option<Liveness>,
    reap_confirmed: bool,
    target: &io::Result<kill::TargetState>,
) -> OwnedTargetState {
    match target {
        Err(_) => OwnedTargetState::UnknownQueryFailed,
        Ok(kill::TargetState::LeaderAbsent) => OwnedTargetState::UnknownIncompleteCoverage,
        Ok(kill::TargetState::WholeAbsent) if boundary == kill::Boundary::LeaderOnly => {
            OwnedTargetState::UnknownIncompleteCoverage
        }
        Ok(kill::TargetState::WholeAbsent) if !reap_confirmed => {
            OwnedTargetState::UnknownReapUnconfirmed
        }
        Ok(kill::TargetState::WholeAbsent) => OwnedTargetState::NativeBoundaryAbsent,
        Ok(kill::TargetState::Present) if leader == Some(Liveness::Alive) => {
            OwnedTargetState::Present
        }
        Ok(kill::TargetState::Present) => OwnedTargetState::UnknownIdentity,
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn native_leader_observation(
    answer: ProbeAnswer,
    direct: impl FnOnce() -> io::Result<unix::Presence>,
) -> ProbeAnswer {
    match answer {
        ProbeAnswer::NoSuchPid => match direct() {
            Ok(unix::Presence::Absent) => ProbeAnswer::NoSuchPid,
            Ok(unix::Presence::Present | unix::Presence::Unsignallable) | Err(_) => {
                ProbeAnswer::CouldNotAnswer
            }
        },
        ProbeAnswer::StartValue(_) | ProbeAnswer::CouldNotAnswer => answer,
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn native_group_presence(answer: &io::Result<unix::Presence>) -> NativeGroupPresence {
    match answer {
        Ok(unix::Presence::Present) => NativeGroupPresence::Present,
        Ok(unix::Presence::Absent) => NativeGroupPresence::Absent,
        Ok(unix::Presence::Unsignallable) => NativeGroupPresence::Unsignallable,
        Err(_) => NativeGroupPresence::Unknown,
    }
}

fn same_recorded_keys(left: Option<&AdoptionKey>, right: Option<&AdoptionKey>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            left.pid == right.pid && left.start_time == right.start_time && left.boot == right.boot
        }
        (None, None) => true,
        _ => false,
    }
}

impl RealHost {
    fn index_native_target(&mut self, pid: u32) {
        let owned = self
            .children
            .get(&pid)
            .expect("the exact child is owned before indexing");
        self.current_native_targets
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                pid,
                CurrentNativeTarget {
                    key: owned.key.clone(),
                    cwd: owned.cwd.clone(),
                    receipt: owned.native_receipt.clone(),
                    absence_observed: false,
                },
            );
    }
    /// The process table's answer for `pid`, through the strongest reference this host holds for
    /// it. Holds no classification: [`liveness`] decides.
    fn probe(&self, pid: u32) -> ProbeAnswer {
        match self.children.get(&pid) {
            Some(owned) => plat::probe(&owned.grip, pid),
            None => plat::probe(&plat::grip_adopted(pid), pid),
        }
    }

    /// The key for a child this host has just spawned and holds unreaped.
    fn read_key(&self, pid: u32, grip: &plat::Grip) -> io::Result<AdoptionKey> {
        #[cfg(target_os = "macos")]
        let answer = plat::unreaped_start_value(grip, pid);
        #[cfg(not(target_os = "macos"))]
        let answer = plat::probe(grip, pid);
        let start_time = match answer {
            ProbeAnswer::StartValue(value) => value,
            // Both are errors here and neither is a `Liveness`: a child that has just execed and
            // is held unreaped by this host *is* in the process table, so either answer means the
            // read failed, not that the child is gone.
            ProbeAnswer::NoSuchPid => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no process table entry for the child just spawned at pid {pid}"),
                ));
            }
            ProbeAnswer::CouldNotAnswer => {
                return Err(io::Error::other(format!(
                    "could not read the start value of the child just spawned at pid {pid}"
                )));
            }
        };
        let boot = match &self.boot {
            BootIdentity::Read(boot) => Some(boot.clone()),
            BootIdentity::NotApplicable => None,
            // Not `None`: `None` would assert this platform's start values are wall-clock-anchored,
            // which on a boot-relative platform is a lie that survives into the journal.
            BootIdentity::Unreadable(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("this host's boot identity is unreadable: {error}"),
                ));
            }
        };
        Ok(AdoptionKey {
            pid,
            start_time,
            boot,
        })
    }
}

/// The two remaining modes, asserted against the directory and the file
/// [`RealHost::write_install`] actually created.
///
/// # Two techniques, one purpose: no assertion here may be satisfied by the runner's umask
///
/// **A mode assertion that holds because nothing widened the artefact is not evidence that
/// anything narrowed it.** `mkdir(2)` and `open(2)` both take the mode through the umask, so on a
/// runner with umask `0077` a directory created with *no* mode call reads back `0700` and a file
/// reads `0600` — and a test that only created and read would pass against a build with every
/// mode call deleted. Two different constructions rule that out, and which one a test uses
/// depends on whether the call it pins runs before or after the artefact exists:
///
/// - **Widen, read the widening back, then call the code** — for the three `chmod`s, which act on
///   an artefact that is already there. The narrowing is observed rather than inferred.
/// - **Force the umask to zero, then read a component nothing chmods afterwards** — for the
///   creation-time mode on `mkdir(2)`, which leaves no artefact to widen. `restrict` pins only the
///   leaf, so the intermediates are that component
///   ([`tests::the_intermediate_directories_carry_the_creation_time_mode`]).
///
/// Between them, four of the five mode calls have a test that goes red when the call is deleted,
/// and it goes red at every ambient umask. The fifth — [`write_owner_only`]'s
/// `OpenOptions::mode` — is reachable by neither construction: it is masked microseconds later by
/// an `fchmod` on the same inode, so there is nothing to widen and nothing the umask can reveal.
/// It stays green when deleted, and is disclaimed at its call site rather than implied to be
/// covered.
#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use super::{RealHost, fs};

    /// A realistic agent directory: `<data_dir>/agents/<AgentId>/`, none of whose components
    /// exists yet, so the first call has to create the chain.
    fn agent_dir(root: &Path) -> PathBuf {
        root.join("data")
            .join("agents")
            .join("01ARZ3NDEKTSV4RRFFQ69G5FAV")
    }

    fn install(path: &Path, content: &str) -> Vec<(PathBuf, String)> {
        vec![(path.to_path_buf(), content.to_owned())]
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path)
            .expect("the path exists")
            .permissions()
            .mode()
            & 0o777
    }

    fn widen(path: &Path, to: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(to)).expect("the chmod lands");
        assert_eq!(
            mode(path),
            to,
            "the widening itself is read back, so what follows measures a narrowing"
        );
    }

    /// `agents/<AgentId>/` is created `0700`, and is `0700` again after being found
    /// wider.
    ///
    /// The second half is the control. It is red against a build with
    /// `restrict` deleted from [`create_owner_only_dir`] under **every** umask, because the
    /// directory is observed at `0755` on the line before the call.
    #[test]
    fn the_agent_directory_is_created_owner_only_and_narrowed_when_found_wider() {
        let root = tempfile::TempDir::new().unwrap();
        let dir = agent_dir(root.path());
        let file = dir.join("managed-settings.json");
        let install = install(&file, "{}");

        RealHost::write_install(&install).expect("the chain is created");
        assert_eq!(
            mode(&dir),
            0o700,
            "the agent's directory is reachable by its owner and by nobody else"
        );

        widen(&dir, 0o755);
        RealHost::write_install(&install).expect("a second spawn re-establishes the mode");
        assert_eq!(
            mode(&dir),
            0o700,
            "a directory found world-readable is narrowed, whatever this runner's umask is"
        );
    }

    /// The creation-time mode on `mkdir(2)`, which [`create_owner_only_dir`] once claimed no test
    /// could reach.
    ///
    /// It can. [`restrict`] pins the **leaf**, so the intermediate components `recursive` creates
    /// — `<data_dir>` and `<data_dir>/agents` — are never chmodded and hold exactly what
    /// `mkdir(2)` gave them. Forcing the umask to zero makes that value readable with no race:
    /// `0700` with `builder.mode(..)`, `0777` without it. The `umask(2)` is restored before the
    /// assertions so a failure here cannot leak a zero umask into the rest of the process, and
    /// `nextest` gives every test its own process in any case.
    #[test]
    fn the_intermediate_directories_carry_the_creation_time_mode() {
        let root = tempfile::TempDir::new().unwrap();
        let data = root.path().join("data");
        let file = agent_dir(root.path()).join("managed-settings.json");

        let previous = rustix::process::umask(rustix::fs::Mode::empty());
        let created = RealHost::write_install(&install(&file, "{}"));
        rustix::process::umask(previous);
        created.expect("the chain is created");

        assert_eq!(
            mode(&data),
            0o700,
            "<data_dir> keeps the mode mkdir(2) was given: nothing chmods it afterwards"
        );
        assert_eq!(
            mode(&data.join("agents")),
            0o700,
            "and so does <data_dir>/agents, at any ambient umask"
        );
    }

    /// The `--settings` file is written `0600`, on both branches of the
    /// content comparison, and is `0600` again after being found wider.
    ///
    /// Three arms, because the mode is set in three different places and each is separately
    /// deletable: creation, the rewrite path (`fchmod` before the content), and the
    /// content-unchanged path (which writes nothing and so would otherwise never re-mode).
    #[test]
    fn the_settings_file_is_written_owner_only_on_every_branch() {
        let root = tempfile::TempDir::new().unwrap();
        let dir = agent_dir(root.path());
        let file = dir.join("managed-settings.json");
        let first = install(&file, r#"{"hooks":{}}"#);
        let second = install(&file, r#"{"hooks":{"Stop":[]}}"#);

        RealHost::write_install(&first).expect("the file is created");
        assert_eq!(mode(&file), 0o600, "created owner-only");

        // The rewrite path: found `0644`, content differs, so it is opened and written.
        widen(&file, 0o644);
        RealHost::write_install(&second).expect("the file is rewritten");
        assert_eq!(
            mode(&file),
            0o600,
            "a file found world-readable is narrowed by the rewrite, whatever the umask is"
        );
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            r#"{"hooks":{"Stop":[]}}"#,
            "control: the rewrite arm is the arm that ran, so the mode above was not the \
             unchanged path's doing"
        );

        // The content-unchanged path: found `0644`, nothing to write, and still narrowed.
        widen(&file, 0o644);
        RealHost::write_install(&second).expect("nothing to write");
        assert_eq!(
            mode(&file),
            0o600,
            "the mode is re-established even when the content is identical and no write happens"
        );
    }

    /// A link planted at an install target must never be followed: the file it points at keeps its
    /// bytes and mode, and the target becomes a regular file holding the projection. Both content
    /// branches, because the unchanged branch could chmod through the link and the rewrite branch
    /// could truncate through it.
    #[test]
    fn a_link_planted_at_the_target_is_replaced_and_never_followed() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        for (planted, label) in [("old", "different content"), ("same", "equal content")] {
            let root = tempfile::TempDir::new().unwrap();
            let dir = agent_dir(root.path());
            let outside = root.path().join("outside");
            fs::write(&outside, planted).unwrap();
            fs::set_permissions(&outside, fs::Permissions::from_mode(0o644)).unwrap();
            let file = dir.join("claude-env.sh");
            fs::create_dir_all(&dir).unwrap();
            symlink(&outside, &file).unwrap();

            RealHost::write_install(&install(&file, "same")).expect("the install is written");

            assert_eq!(
                fs::read_to_string(&outside).unwrap(),
                planted,
                "{label}: bytes"
            );
            assert_eq!(
                mode(&outside),
                0o644,
                "{label}: the outside file keeps its mode"
            );
            let meta = fs::symlink_metadata(&file).unwrap();
            assert!(
                meta.is_file(),
                "{label}: the target is a regular file, not a link"
            );
            assert_eq!(
                fs::read_to_string(&file).unwrap(),
                "same",
                "{label}: content"
            );
            assert_eq!(mode(&file), 0o600, "{label}: owner-only");
        }
    }
}

/// The agent directory and settings file, measured on native Windows after each object
/// is deliberately widened. The control prevents inherited permissions from making a deleted
/// production call look effective.
#[cfg(all(test, windows))]
mod windows_dacl_tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::{RealHost, fs};

    fn agent_dir(root: &Path) -> PathBuf {
        root.join("data")
            .join("agents")
            .join("01ARZ3NDEKTSV4RRFFQ69G5FAV")
    }

    fn install(path: &Path, content: &str) -> Vec<(PathBuf, String)> {
        vec![(path.to_path_buf(), content.to_owned())]
    }

    fn widen(path: &Path) {
        let trustee = if path.is_dir() {
            "*S-1-1-0:(OI)(CI)(R)"
        } else {
            "*S-1-1-0:R"
        };
        let result = Command::new("icacls")
            .arg(path)
            .args(["/grant", trustee])
            .output()
            .expect("run the Windows ACL control");
        assert!(
            result.status.success(),
            "icacls failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            !dudes_win::is_creator_only(path).expect("read wider control"),
            "the widening is read back before the production call"
        );
    }

    #[test]
    fn the_agent_directory_has_a_creator_only_owner_and_dacl() {
        let root = tempfile::TempDir::new().expect("temporary root");
        let dir = agent_dir(root.path());
        let file = dir.join("managed-settings.json");
        let projected = install(&file, "{}");

        RealHost::write_install(&projected).expect("create protected agent directory");
        assert!(
            dir.is_dir(),
            "the directory assertion measures an existing object"
        );
        dudes_win::create_runtime_dir_creator_only(&dir)
            .expect("the agent directory has the exact protected, inheritable descriptor");

        widen(&dir);
        RealHost::write_install(&projected).expect("narrow existing agent directory");
        dudes_win::create_runtime_dir_creator_only(&dir)
            .expect("the existing directory is narrowed to the exact descriptor");
    }

    #[test]
    fn the_settings_file_has_a_creator_only_owner_and_dacl_on_every_branch() {
        let root = tempfile::TempDir::new().expect("temporary root");
        let dir = agent_dir(root.path());
        let file = dir.join("managed-settings.json");
        RealHost::write_install(&install(&file, "same contents"))
            .expect("create protected settings file");
        assert!(dudes_win::is_creator_only(&file).expect("read created settings file"));
        widen(&file);
        let unchanged_mtime = fs::metadata(&file)
            .expect("unchanged file metadata")
            .modified()
            .expect("unchanged file modified time");

        RealHost::write_install(&install(&file, "same contents"))
            .expect("narrow unchanged settings file");
        assert!(dudes_win::is_creator_only(&file).expect("read unchanged result"));
        assert_eq!(
            fs::metadata(&file)
                .expect("unchanged result metadata")
                .modified()
                .expect("unchanged result modified time"),
            unchanged_mtime,
            "DACL repair must not make an unchanged settings file look rewritten"
        );

        widen(&file);
        RealHost::write_install(&install(&file, "new contents"))
            .expect("narrow and rewrite settings file");
        assert_eq!(
            fs::read_to_string(&file).expect("read rewritten settings file"),
            "new contents"
        );
        assert!(dudes_win::is_creator_only(&file).expect("read rewrite result"));
    }
}

/// The seam mapping, on every platform.
///
/// A module of its own because the one above is `cfg(all(test, unix))` — its subject is file modes
/// — and what is asserted here is platform-independent by construction: the [`BootIdentity`] under
/// test is hand-made, never read off this machine.
#[cfg(test)]
mod boot_tests {
    use super::{BootIdentity, HostBoot, io};

    /// Every arm of the twin mapping, including the one no healthy machine reaches.
    ///
    /// **`Unreadable` is why this test exists.** It is produced only when `plat::boot_identity()`
    /// errors, which on a working host never happens, so the arm that renders the reason is
    /// unreachable from any integration test on any CI leg — and it is the arm carrying the whole
    /// point of the field: a run in that state answers `Unknown` for every adopted turn until it
    /// is restarted. Mapped to `NotApplicable`, the reply would say a macOS-shaped thing about a
    /// Linux host that has stopped working, and the operator would read "no boot identity here,
    /// nothing to see".
    #[test]
    fn the_mapping_to_the_seam_is_total() {
        assert_eq!(
            HostBoot::from(&BootIdentity::NotApplicable),
            HostBoot::NotApplicable
        );
        assert_eq!(
            HostBoot::from(&BootIdentity::Read("f0e1d2c3".to_owned())),
            HostBoot::Read("f0e1d2c3".to_owned())
        );

        let unreadable = BootIdentity::Unreadable(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "permission denied",
        ));
        let mapped = HostBoot::from(&unreadable);
        assert_eq!(mapped, HostBoot::Unreadable("permission denied".to_owned()));
        // The two states that produce no identity stay apart across the mapping, which is the
        // property the three-valued shape exists for. The line above is the control: the reason
        // survives, so this is not two values that both failed to be built.
        assert_ne!(mapped, HostBoot::from(&BootIdentity::NotApplicable));
        // And neither of them answers the comparison `liveness` makes.
        assert_eq!(mapped.as_host_boot(), None);
    }
}
