//! The host seam: everything non-deterministic enters the supervisor here, and only here.
//!
//! [`ProcessHost`], [`Clock`] and [`Rng`] are implemented by the daemon runtime and by
//! the fakes in `crate::testing`, which are behind the `testing` feature and are not in a default
//! build (unlinked for that reason). The machine reads `Clock` and `Rng` through these seams.
//! It drives no process through `ProcessHost`: it emits [`Effect`](crate::Effect)s and the runtime
//! executes them. Its one direct call is diagnostic and decides nothing —
//! [`key_health`](crate::Supervisor::key_health) asks the host for the boot identity
//! `system.doctor` reports. The traits live together here because [`AdoptionKey`], [`Liveness`]
//! and the fakes do, and because the runtime implements them against this crate.
//!
//! The one piece of *rule* in this module is [`liveness()`]: the liveness table, written once,
//! taking the host's boot identity as an argument so every cell is unit-testable on every
//! platform with no reboot, no foreign journal and no broken `/proc`.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use dudes_proto::Entry;
use dudes_proto::rpc::hook::HookToken;
use dudes_providers::adapter::{HookInstall, ManagedInstall};
use dudes_providers::launch::LaunchSpec;
use dudes_types::Timestamp;
use ulid::Ulid;

/// The error [`ProcessHost::shutdown`] returns when no concrete failure occurred and every row it
/// retains is a process group or job whose final target query found it absent and whose leader
/// reap is confirmed. Only complete boundary coverage is missing, which no native backend proves,
/// so a run whose managed children all exited and were reaped ends with this error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContainmentUnproven;

impl ContainmentUnproven {
    /// Whether `error` is this one.
    #[must_use]
    pub fn is(error: &io::Error) -> bool {
        error
            .get_ref()
            .is_some_and(<dyn std::error::Error + Send + Sync>::is::<Self>)
    }
}

impl std::fmt::Display for ContainmentUnproven {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("every retained child exited, but its containment is unproven")
    }
}

impl std::error::Error for ContainmentUnproven {}

/// What a recorded spawn is recognised by after a crash: the pid, the platform-native start
/// value, and the boot identity of the machine the child started on.
///
/// `start_time` is **opaque** — the value the host read, compared as read, never converted and
/// never rendered as a time: Linux `starttime` in clock ticks,
/// macOS `p_starttime`, Windows the creation `FILETIME`. `boot` is `Some` only where that value
/// is boot-relative; `None` asserts it is wall-clock-anchored, and never "boot-relative but
/// unreadable".
///
/// **Derives neither `PartialEq` nor `Eq`, deliberately**. Comparing a key against
/// *a probe* is a three-way answer that depends on host state which is not in the key, and a
/// derived equality collapses it to `Some(x) != None → Gone` — the false `Gone` the liveness table
/// forbids. [`liveness()`] is the only place that comparison happens.
///
/// A key is also compared against **another key**, which is a different question — are these two
/// recordings of the same process? — needing no host state, so it is answered field by field.
/// The host's selection sites pick which retained attempt a recorded key names. The missing derive
/// keeps even that comparison from being written `a == b`: selection must also compare boot
/// presence before value.
#[derive(Clone, Debug)]
pub struct AdoptionKey {
    /// The child's pid.
    pub pid: u32,
    /// The platform-native start value, opaque.
    pub start_time: u64,
    /// The boot identity of the machine the child started on, where the start value is
    /// boot-relative. Opaque: compared for equality, never parsed.
    pub boot: Option<String>,
}

/// What the daemon knows about a recorded child right now, declared in
/// `dudes-types` because it is on the wire — `system.doctor` answers with it per open turn — and a
/// contract type may not depend on this crate. Re-exported here because this is
/// where it is *produced*: [`liveness()`] below is the only place a key is compared against a
/// probe, and only the supervisor decides what an answer means.
pub use dudes_types::Liveness;

/// This run's boot identity and the reason when there is none, declared in `dudes-types` for the
/// same reason [`Liveness`] is: `system.doctor` answers with it, and a contract type may not depend
/// on this crate. Re-exported here because this is the crate that *consumes* it —
/// [`ProcessHost::boot_identity`] is where it enters, and [`liveness()`] is what it decides.
pub use dudes_types::HostBoot;

/// What the platform layer *read* about a pid, kept separate from what the rule *decides*.
///
/// Per-platform code holds no classification at all: it answers this, and [`liveness()`] decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProbeAnswer {
    /// The process table has no such pid.
    NoSuchPid,
    /// The process table has that pid, and this is its platform-native start value.
    StartValue(u64),
    /// Permission denied, an unreadable `/proc/<pid>/stat`, a transient API failure.
    CouldNotAnswer,
}

/// Result of a signal-zero query to one recorded process group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeGroupPresence {
    /// The kernel found at least one member the daemon may signal.
    Present,
    /// The kernel reported no such process group.
    Absent,
    /// The kernel found the group but permission or zombie state prevents signalling it.
    Unsignallable,
    /// The query failed or this platform cannot inspect the group.
    Unknown,
}

/// The only place an adoption key is compared **against a probe**. Returns [`Liveness`], never
/// `bool`. Key-to-key comparison is a different question and is not this: see [`AdoptionKey`].
///
/// Invariant 3: `Gone` is an Observed absence, so it is returned only on positive evidence — two
/// *present* boot identities that differ, or the process table's own answer. Anything else that
/// is not `Alive` is `Unknown`. Implements the table; **presence before value**.
///
/// `host_boot` is the boot identity *this host produces now* — read once at daemon start, `None`
/// on a platform whose start values are captured at creation, and `None` on a boot-relative
/// platform whose read failed this run. The diagonal of the table is the point: the key and the
/// host must first agree that a boot identity exists at all, and either disagreement is
/// `Unknown`.
#[must_use]
pub fn liveness(recorded: &AdoptionKey, host_boot: Option<&str>, probe: ProbeAnswer) -> Liveness {
    if !key_is_interpretable(recorded, host_boot) {
        // Presence disagreement, in either direction: no evidence about the child at all.
        return Liveness::Unknown;
    }
    match (recorded.boot.as_deref(), host_boot) {
        // Both present: a difference is a reboot, which is positive evidence of absence.
        (Some(key), Some(host)) if key != host => Liveness::Gone,
        _ => process_table(recorded.start_time, probe),
    }
}

/// **Presence before value**: whether this host can interpret `recorded`'s boot identity at all.
///
/// The first half of the liveness table, read before any value is compared and before the process
/// table is asked. `false` is the table's diagonal — the key and the host disagree about whether a
/// boot identity *exists*, either because a journal was written on another platform or because a
/// boot-relative host's own read failed this run — and it is the one `Unknown` that no action on
/// this host resolves: re-probing a key nothing here can interpret returns the same answer for the
/// rest of the run, so the turn stays open and the loop makes no progress on that agent.
///
/// Split out of [`liveness()`], which calls it, rather than written twice. `system.doctor` marks
/// an open turn with this and a second copy of the presence test is exactly
/// the drift that putting one comparison in one function prevents — the marked set and the
/// unresolvable set would stop being the same set, silently, at the first edit to either.
///
/// **Not the same question as "is the answer `Unknown`".** A probe the host could not answer is
/// also `Unknown` and is *not* this: that one resolves the moment the host recovers.
#[must_use]
pub fn key_is_interpretable(recorded: &AdoptionKey, host_boot: Option<&str>) -> bool {
    matches!(
        (recorded.boot.as_deref(), host_boot),
        (Some(_), Some(_)) | (None, None)
    )
}

/// "The process table decides", in the two cells of the liveness table that reach it. They are unchanged:
/// no such pid or a different start value is `Gone`, an equal start value is `Alive`.
const fn process_table(recorded_start: u64, probe: ProbeAnswer) -> Liveness {
    match probe {
        ProbeAnswer::StartValue(seen) if seen == recorded_start => Liveness::Alive,
        // No such pid, or that pid with a different start value: the original is gone either way.
        ProbeAnswer::NoSuchPid | ProbeAnswer::StartValue(_) => Liveness::Gone,
        ProbeAnswer::CouldNotAnswer => Liveness::Unknown,
    }
}

/// What a kill did, as the host observed it — a decision, not a `Result`.
///
/// `io::Result<()>` collapses "refused because the key no longer matches" and "the signal
/// failed" into one `Err`, and those are opposite facts: the first is the guard working, the
/// second is the platform saying no. The adoption key has to be re-validated
/// immediately before **every** signal and the signal withheld when it does not match, so the
/// seam has to be able to say "nothing was signalled, and here is why" without erroring.
///
/// **A refusal reaches the machine as [`Input::Liveness`](crate::Input::Liveness), not as a new
/// record**: `Refused { liveness: Gone }` is the reaper's finding by a second
/// route — the daemon has Observed, by probing and comparing, that the recorded child is gone —
/// and `Input::Liveness`'s own handling already says what `Gone` and `Unknown` write. That handling is
/// implemented; what is still owed is the runtime that feeds this value back in. The host returns
/// it and routes nothing.
///
/// Carries no `AdoptionKey`, so it needs no equality: the caller has the key it passed.
#[derive(Debug)]
pub enum KillOutcome {
    /// At least one signal was sent, each one authorised — by the key re-validating as
    /// [`Liveness::Alive`], or by the reservation. See [`ProcessHost::kill`] for which applies.
    ///
    /// Not a claim that the process is dead on the adoption path, where the daemon is not the
    /// parent and cannot reap; for a child the host owns, it is, because the host reaps before
    /// returning.
    Signalled,
    /// No signal was sent at all: nothing authorised one. On the adoption path that is the key
    /// failing to re-validate; on the reserved path it is the key naming a *different* process at
    /// that identifier, or naming a child the host could read no key for. Either way `liveness`
    /// is [`Liveness::Gone`] or [`Liveness::Unknown`], never [`Liveness::Alive`].
    Refused {
        /// What the host Observed about the recorded child instead.
        liveness: Liveness,
    },
    /// Selection and signal authority held, but a platform signal, checked quiet query or eligible
    /// direct reap failed. An owned implementation retains the child for a later retry.
    Failed(io::Error),
}

/// What one host spawn attempt established.
///
/// This is deliberately not serialized and has no default. A caller must distinguish a child it
/// can register, a positively established absence it may end, and retained cleanup uncertainty it
/// must leave claimed for this daemon run.
#[derive(Debug)]
pub enum SpawnResult {
    /// The child is running and can be named after a crash.
    Spawned(AdoptionKey),
    /// No process was created, or post-exec cleanup proved the complete target quiet and the
    /// leader reaped.
    NotLaunched {
        /// The original error that prevented launch.
        reason: io::Error,
    },
    /// A process existed and cleanup did not establish every fact required for Observed absence.
    CleanupUnconfirmed {
        /// The original error that prevented launch.
        reason: io::Error,
    },
}

/// The maximum number of retained ownership rows a single host query inspects.
pub const MAX_OWNED_TARGET_QUERY_ROWS: usize = 512;

/// Current process evidence for one retained ownership row.
///
/// Leader absence alone never establishes native-boundary absence. These answers describe the
/// current query, not whether a process ran earlier in the turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OwnedTargetState {
    /// The native target is present and the retained leader identity still validates.
    Present,
    /// The leader was successfully reaped and a fresh query finds the recorded group or job empty.
    ///
    /// This covers only the acquired native boundary. Processes that escaped or were created
    /// outside it remain outside this evidence.
    NativeBoundaryAbsent,
    /// The acquired boundary or its query cannot establish group or job absence.
    UnknownIncompleteCoverage,
    /// The leader's successful reap has not been established.
    UnknownReapUnconfirmed,
    /// A checked target query failed.
    UnknownQueryFailed,
    /// A present native target cannot be associated with the retained leader identity.
    UnknownIdentity,
    /// The indexed attempt no longer has its original retained host capability.
    UnknownMissingCapability,
}

/// An opaque receipt bound to one host-local spawn attempt.
///
/// Cloning preserves identity. A fresh receipt has a distinct identity even for an equal adoption
/// key. It is never serialized or recovered across daemon runs.
#[derive(Clone)]
pub struct NativeBoundaryReceipt(Arc<()>);

impl NativeBoundaryReceipt {
    /// Create a distinct attempt identity for a host implementation.
    #[must_use]
    pub fn fresh() -> Self {
        Self(Arc::new(()))
    }
}

impl PartialEq for NativeBoundaryReceipt {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for NativeBoundaryReceipt {}

impl std::fmt::Debug for NativeBoundaryReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeBoundaryReceipt { .. }")
    }
}

/// Whether exact acknowledged absence could retire current query bookkeeping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NativeBoundaryAcknowledgement {
    /// The observed receipt retired only its matching current query entry.
    Acknowledged,
    /// No scoped absence was observed for this entry.
    RefusedUnobserved,
    /// The key or receipt does not select the current retained attempt.
    UnknownIdentity,
    /// The matching retained process capability is unavailable.
    UnknownMissingCapability,
    /// This host does not support absence acknowledgement.
    UnknownUnsupported,
}

/// One retained ownership row and the launch metadata needed to associate it with runtime work.
#[derive(Clone, Debug)]
pub struct OwnedTargetEvidence {
    /// The exact key acquired at spawn, including boot presence, or none after a failed key read.
    pub key: Option<AdoptionKey>,
    /// The exact execution folder passed to the host at launch.
    ///
    /// A folder shared by multiple attempts cannot identify an exclusive writer or one attempt.
    pub cwd: PathBuf,
    /// What this query establishes about the retained target now.
    pub state: OwnedTargetState,
    /// An authentic acknowledgement receipt for a keyed scoped absence observed by this query.
    ///
    /// None for an unkeyed attempt or when this query did not establish native-boundary absence.
    pub native_absence_receipt: Option<NativeBoundaryReceipt>,
}

/// Whether a query enumerated every current, unacknowledged native target entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OwnedTargetsStatus {
    /// Every current entry was inspected. Individual rows may still have unknown target state.
    Complete,
    /// The query stopped at its row bound.
    Truncated {
        /// The number of current entries that were not inspected.
        omitted: usize,
    },
    /// This host does not implement retained native-boundary queries.
    UnknownUnsupported,
}

/// A bounded snapshot of this host's current native target query entries.
///
/// An empty complete snapshot says nothing about children owned by a previous daemon run.
#[derive(Clone, Debug)]
pub struct OwnedTargets {
    /// Inspected entries in the host's stable query order.
    pub targets: Vec<OwnedTargetEvidence>,
    /// Whether every current query entry was inspected.
    pub status: OwnedTargetsStatus,
}

/// The process side of the runtime, as the supervisor's effects describe it.
///
/// Three of the four methods are executed by the runtime in response to an
/// [`Effect`](crate::Effect); the machine calls none of them.
/// **[`shutdown`](ProcessHost::shutdown) is the exception, and the exception is the point:** it
/// is called by the daemon's own exit path rather than by an `Effect`, because invariant 10's
/// daemon-exit half must hold on exits the machine never sees — `SIGTERM`, a panic unwind,
/// `main` returning. An `Effect` would make that guarantee conditional on the loop still
/// running, which is the state it is least likely to be in.
///
/// **This trait is not the whole of invariant 10.** Declaring `shutdown` makes the daemon-exit
/// half *reachable* and *assertable*; what makes it *hold* is three things together — the
/// platform hold taken at `spawn` (a process group on Unix, a job with `KILL_ON_JOB_CLOSE` on
/// Windows), the daemon calling `shutdown` on every exit path, and the real host's own tests. On
/// Unix the platform hold **selects** and does not kill, so a daemon killed with `SIGKILL`
/// orphans its children on that platform and no implementation of this trait can prevent it.
///
/// `spawn` takes the [`ManagedInstall`] alongside the spec because the adapter's files are a
/// projection: the host writes any that differ *before* exec, so a
/// re-issued spawn rebuilds them and a `--settings` path in argv always exists.
pub trait ProcessHost {
    /// Write `install`'s files if they differ, then exec `spec` and read the child's adoption
    /// key while the pid is still reserved.
    ///
    /// Every pre-exec failure is [`SpawnResult::NotLaunched`]. After exec, that result requires
    /// complete target coverage, per-signal-authorised signals, a fresh whole-target quiet answer after
    /// the last signal, and confirmed leader reap. Any missing fact is
    /// [`SpawnResult::CleanupUnconfirmed`] and retains every capability the host acquired.
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult;

    /// Signal the child `key` names and its process group or job, and refuse if nothing
    /// authorises the signal.
    ///
    /// A key answers *who a process was*, a signal needs *who this identifier is now*, and
    /// something has to span the two — an existence probe cannot, because the stranger exists.
    /// **Two things do, and an implementation uses whichever it has:**
    ///
    /// - **The key comparison**, where the implementation holds only a key. It re-validates
    ///   before every signal the escalation sends, not once at the top, and signals nothing that
    ///   does not answer [`Liveness::Alive`].
    /// - **The reservation**, where the implementation spawned the child and has not reaped it.
    ///   A process identifier is freed at the reap, so the number cannot have been handed to
    ///   anything else and the signal cannot reach a stranger — which is stronger than the
    ///   comparison, not weaker. A host's own shutdown path uses it only while the child remains
    ///   unreaped. A retained post-exec-failure row whose leader was reaped must compare its key
    ///   before each later signal, or send no signal if it acquired no key.
    ///
    ///   A `kill` does need one, because it selects. The key is compared once there, **whole** —
    ///   identifier, start value and boot identity, presence before value — so that neither a
    ///   stale kill for a reaped child takes its successor at the same identifier down, nor a key
    ///   replayed from the journal after a reboot selects a live child of the current run. Only
    ///   the *authorisation* comes from the reservation; the *selection* is still the key.
    ///   `dudes-host` is the worked example, and its `src/kill.rs` argues the split.
    ///
    /// Errors are a variant of [`KillOutcome`], not a `Result`: see that type.
    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome;

    /// Read the process table and apply [`liveness()`] against the host's own boot identity.
    fn alive(&self, key: &AdoptionKey) -> Liveness;

    /// Observe the recorded leader without adopting it or delivering a signal.
    /// [`ProbeAnswer::NoSuchPid`] means confirmed direct pid absence, never only a missing
    /// process-table entry. Tables may hide live processes, so an independent signal-zero
    /// query must confirm absence. It delivers no signal. Permission refusals and query errors
    /// remain uncertainty. [`ProbeAnswer::StartValue`] is raw observed native start evidence.
    /// On Linux, the numeric process-table view must first be confirmed aligned with this
    /// daemon's active pid namespace.
    fn probe_native_leader(&self, _key: &AdoptionKey) -> ProbeAnswer {
        ProbeAnswer::CouldNotAnswer
    }

    /// Query the process group whose recorded leader pid is `pid`, without delivering a signal.
    /// Only an explicit no-such-group answer establishes absence.
    fn native_group_presence(&self, _pid: u32) -> NativeGroupPresence {
        NativeGroupPresence::Unknown
    }

    /// The cached Linux pid namespace only when its initial and current numeric process-table
    /// views are confirmed aligned with the daemon's active pid namespace. Otherwise absent.
    fn pid_namespace(&self) -> Option<String> {
        None
    }

    /// Inspect at most `limit` current native target entries, capped at
    /// [`MAX_OWNED_TARGET_QUERY_ROWS`], without signalling, waiting, or releasing ownership.
    ///
    /// This includes rows whose leader or stream ended and failed spawns without an adoption key.
    /// The default explicitly reports unsupported evidence. An implementation must not infer
    /// native-boundary absence from [`Self::alive`] returning [`Liveness::Gone`].
    fn owned_targets(&self, limit: usize) -> OwnedTargets {
        let _ = limit;
        OwnedTargets {
            targets: Vec::new(),
            status: OwnedTargetsStatus::UnknownUnsupported,
        }
    }

    /// Acknowledge exact scoped absence only after its boundary settlement was durably folded.
    ///
    /// This retires current query bookkeeping only. Retained shutdown ownership and capabilities
    /// remain untouched. The receipt must still name the exact retained attempt and its observed
    /// native-boundary absence. Unkeyed attempts cannot be cleared by a shared execution folder.
    fn acknowledge_native_boundary_absence(
        &mut self,
        _key: &AdoptionKey,
        _receipt: &NativeBoundaryReceipt,
    ) -> NativeBoundaryAcknowledgement {
        NativeBoundaryAcknowledgement::UnknownUnsupported
    }

    /// The boot identity this run stamps spawns with, or the reason it has none.
    ///
    /// Read once at daemon start and cached, so this is a report of that read and
    /// not a second one: a value re-read per call could change underneath a decision, and every
    /// answer [`alive`](ProcessHost::alive) has already given was decided against the cached one.
    ///
    /// **On the seam rather than on a concrete host, and that is the whole point of declaring it.**
    /// Invariant 8 makes identity structural: this value comes from the platform, never from a
    /// caller and never from a configuration field, so `system.doctor` has exactly one honest route
    /// to it. The daemon names no concrete host. A method taken off the trait and kept inherent on `RealHost`
    /// is exactly the shape to avoid, because the seam would then not be the route the caller takes,
    /// and a daemon holding `RealHost` concretely rather than by injection would remove that
    /// reachability argument outright. So a method here is what lets the diagnostic be asked of a *fake* host,
    /// which is what makes the failure it reports testable at all.
    ///
    /// Returns an owned value: an implementation that keeps a richer local type maps into this one
    /// (`dudes-host` does), and one that borrows out of itself would force that type to be this one.
    fn boot_identity(&self) -> HostBoot;

    /// Apply the per-signal authority check in shutdown to every retained child in stable row order and one shared grace.
    ///
    /// This is invariant 10's daemon-exit half — *"die with their … daemon"* — and it is the one
    /// method here that no [`Effect`](crate::Effect) asks for. See the trait's own doc for why.
    ///
    /// An unreaped direct leader is waited only after fresh whole-target absence or independently
    /// accepted direct hard-signal delivery. Aggregate group/job success alone does not bound that
    /// wait. Ownership is removed only after complete boundary coverage, fresh whole-target
    /// absence after the final signal decision and confirmed direct reap are all established
    /// independently.
    ///
    /// # Errors
    /// The first concrete platform, query or reap failure in fixed phase and stable row order,
    /// after every child has still been attempted. Any retained row makes the call fail; when no
    /// concrete failure exists, the implementation returns an unresolved-ownership error, which is
    /// [`ContainmentUnproven`] when every retained row lacks only complete boundary coverage. A
    /// failed call keeps unresolved rows reachable for a later retry.
    fn shutdown(&mut self) -> io::Result<()>;

    /// Write an attached launch's hook settings if they differ, with the modes
    /// [`spawn`](ProcessHost::spawn) gives its install, before another owner starts that child.
    ///
    /// The default refuses, so a host that cannot write them refuses the attach rather than
    /// starting a CLI whose hooks are missing.
    ///
    /// # Errors
    ///
    /// Whatever stopped a file from being written.
    fn write_hooks(&mut self, install: &HookInstall) -> io::Result<()> {
        let _ = install;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this process host cannot write hook settings",
        ))
    }
}

/// The daemon's wall clock. The single source of time in this crate: the machine stamps
/// [`Entry::at`] from it at the moment it emits a `Persist` and resolves the
/// run-local compaction deadline from it.
pub trait Clock {
    /// UTC milliseconds.
    fn now(&self) -> Timestamp;
}

/// Randomness, as a seam, so the crash tests are reproducible.
pub trait Rng {
    /// The next id. Every `AgentId`, `TurnId`, `GoalId` and vendor session id the supervisor
    /// mints comes from here.
    fn ulid(&mut self) -> Ulid;

    /// Thirty-two fresh random bytes as a [`HookToken`]: one agent's credential for one daemon
    /// run.
    fn agent_token(&mut self) -> HookToken;
}

/// blake3 of some bytes, lowercase hex — the form `HookTokenMinted.token_hash` and
/// `LoopDecided.input_digest` both take.
#[must_use]
pub fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// The canonical digest of a `dudes_policy::LoopInput`, as `LoopDecided.input_digest` carries it —
/// or `None` when the value cannot be serialised.
///
/// Canonical because `serde_json` writes struct fields in declaration order and `LoopInput` holds
/// no map: two runs over an equal input give equal bytes, which is what makes `dudes why`
/// reproducible from the journal.
///
/// **Fallible on purpose.** Swallowing a serialisation failure into the hash of `b""` would give a
/// well-formed 64-hex digest that *every* failing input shares. The beat compares the digest to
/// decide whether an `Exhausted` agent is re-examined, and two different inputs hashing equal
/// would make every stuck agent look unchanged for ever. So the failure is returned, and the
/// caller decides what it means.
///
/// **Concrete on purpose.** The seam's whole contract is *the digest of the loop input*, and the
/// two call sites that matter — `Supervisor::decide` and `Supervisor::re_examine_exhausted` — are
/// safe only because they digest **the same value**. A `T: Serialize` parameter would let one of
/// them be handed something else and still compile, which is a property this signature can hold and
/// a bound cannot. It also keeps the fallibility break the only breaking change here: widening to a
/// generic and narrowing back later is a second major version for nothing.
#[must_use]
pub fn input_digest(input: &dudes_policy::LoopInput) -> Option<String> {
    canonical_digest(input)
}

/// Digest the complete closing-mode input, including the actual task and carried answers.
#[must_use]
pub fn closing_input_digest(input: &dudes_policy::ClosingLoopInput) -> Option<String> {
    #[derive(serde::Serialize)]
    struct ClosingCanonical<'a> {
        kind: &'static str,
        input: &'a dudes_policy::ClosingLoopInput,
    }
    canonical_digest(&ClosingCanonical {
        kind: "closing_save_v1",
        input,
    })
}

/// Digest the separately typed admission charged to an already completed goal.
#[must_use]
pub fn completed_note_input_digest(input: &dudes_policy::CompletedNoteInput) -> Option<String> {
    #[derive(serde::Serialize)]
    struct NoteCanonical<'a> {
        kind: &'static str,
        input: &'a dudes_policy::CompletedNoteInput,
    }
    canonical_digest(&NoteCanonical {
        kind: "completed_browser_note_v1",
        input,
    })
}

/// Canonical encoding of a taught completed-note input failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaughtNoteDigestError;
impl std::fmt::Display for TaughtNoteDigestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("taught completed-note input does not encode")
    }
}
impl std::error::Error for TaughtNoteDigestError {}

/// Digest the complete input under distinct taught authority without reusing a raw hash.
///
/// # Errors
/// Returns a named encoding refusal instead of a fabricated empty digest.
pub fn taught_note_input_digest(
    input: &dudes_policy::CompletedNoteInput,
) -> Result<String, TaughtNoteDigestError> {
    #[derive(serde::Serialize)]
    struct TaughtCanonical<'a> {
        kind: &'static str,
        contract: dudes_proto::NoteContract,
        input: &'a dudes_policy::CompletedNoteInput,
    }
    canonical_digest(&TaughtCanonical {
        kind: "taught_completed_browser_note_v1",
        contract: dudes_proto::NoteContract::Taught,
        input,
    })
    .ok_or(TaughtNoteDigestError)
}

/// Canonical encoding of a complete mail admission input failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailWakeDigestError;

impl std::fmt::Display for MailWakeDigestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("mail-wake admission input does not encode")
    }
}

impl std::error::Error for MailWakeDigestError {}

/// Digest every supplied admission fact under its typed fresh or resume authority.
///
/// The supervisor envelope carries the supplied day separately from the pure policy input.
/// It excludes ticking time and the prior refusal's own digest.
///
/// # Errors
/// Returns a named canonical encoding refusal, never a digest of an empty fallback.
pub fn mail_wake_input_digest(
    input: &dudes_policy::MailWakeInput,
    stamped_day: dudes_types::UtcDay,
) -> Result<dudes_proto::MailWakeDigest, MailWakeDigestError> {
    canonical_mail_wake_digest(input.domain(), stamped_day, input)
}

fn canonical_mail_wake_digest<T: serde::Serialize + ?Sized>(
    domain: dudes_proto::MailWakeDigestDomain,
    day: dudes_types::UtcDay,
    input: &T,
) -> Result<dudes_proto::MailWakeDigest, MailWakeDigestError> {
    #[derive(serde::Serialize)]
    struct MailCanonical<'a, T: ?Sized> {
        kind: &'static str,
        domain: dudes_proto::MailWakeDigestDomain,
        day: dudes_types::UtcDay,
        input: &'a T,
    }
    let bytes = serde_json::to_vec(&MailCanonical {
        kind: "mail_wake_admission_v1",
        domain,
        day,
        input,
    })
    .map_err(|_| MailWakeDigestError)?;
    Ok(dudes_proto::MailWakeDigest {
        domain,
        value: dudes_proto::record::Hex32::new(*blake3::hash(&bytes).as_bytes()),
    })
}

/// [`input_digest`] with the type erased, so the `None` arm has a reachable test.
///
/// Private, and generic only for that: `LoopInput`'s field graph holds no fallible serialiser, so
/// against the public signature alone the failure arm is unreachable and the rule that *an
/// unproducible digest reads as changed* would be a shape with nothing behind it. This module's own
/// `#[cfg(test)] mod tests` is a child module and reaches this directly, so the coverage is the same
/// as a public bound bought — without putting the bound on the seam.
fn canonical_digest<T>(input: &T) -> Option<String>
where
    T: serde::Serialize + ?Sized,
{
    serde_json::to_vec(input)
        .ok()
        .map(|canonical| blake3_hex(&canonical))
}

/// A vendor session id in UUID form, derived from one [`Ulid`] out of the [`Rng`] seam.
///
/// Claude Code's `--session-id` requires a valid UUID, and a `Ulid` is exactly 128 bits, so the
/// daemon mints one id and renders it in the shape the vendor accepts — version 4, RFC 4122
/// variant — rather than adding a second method to the seam. Deterministic under `FakeRng`, so a
/// test can name the id a given draw produces — which is a property of
/// the derivation, not a promise about re-issue: minting an id does not confirm it, so a re-issue
/// draws again unless the vendor confirmed a session.
#[must_use]
pub fn session_id_from_ulid(ulid: Ulid) -> dudes_types::SessionId {
    use std::fmt::Write as _;

    let mut bytes = ulid.to_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut text = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        let _ = write!(text, "{byte:02x}");
    }
    dudes_types::SessionId::new(text)
}

/// The journal side of an effect the runtime executed: what `Input::Persisted` acknowledges.
///
/// Not a trait — the writer lives in `dudes-journal`, which this crate does not depend on
/// (invariant 9 and "sans-io": reading redb inside `handle()` would be I/O). The runtime appends
/// the [`Entry`] and feeds the `lsn` back.
pub type Persisted = (Entry, dudes_types::Lsn);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_without_owned_target_support_reports_unknown() {
        struct LeaderOnlyHost;

        impl ProcessHost for LeaderOnlyHost {
            fn spawn(&mut self, _spec: &LaunchSpec, _install: &ManagedInstall) -> SpawnResult {
                panic!("this check performs no spawn");
            }

            fn kill(&mut self, _key: &AdoptionKey) -> KillOutcome {
                panic!("this check performs no kill");
            }

            fn alive(&self, _key: &AdoptionKey) -> Liveness {
                Liveness::Gone
            }

            fn boot_identity(&self) -> HostBoot {
                HostBoot::NotApplicable
            }

            fn shutdown(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut host = LeaderOnlyHost;
        let key = AdoptionKey {
            pid: 100,
            start_time: 200,
            boot: None,
        };
        assert_eq!(host.alive(&key), Liveness::Gone);
        let answer = host.owned_targets(1);
        assert_eq!(answer.status, OwnedTargetsStatus::UnknownUnsupported);
        assert!(answer.targets.is_empty());
        assert_eq!(
            host.acknowledge_native_boundary_absence(&key, &NativeBoundaryReceipt::fresh()),
            NativeBoundaryAcknowledgement::UnknownUnsupported
        );
    }

    #[test]
    fn native_receipts_preserve_clone_identity_and_distinguish_fresh_attempts() {
        let receipt = NativeBoundaryReceipt::fresh();
        assert_eq!(receipt, receipt.clone());
        assert_ne!(receipt, NativeBoundaryReceipt::fresh());
    }

    fn mail_digest(
        input: &dudes_policy::MailWakeInput,
    ) -> Result<dudes_proto::MailWakeDigest, MailWakeDigestError> {
        mail_wake_input_digest(input, dudes_types::UtcDay::from_days_since_epoch(1))
    }

    fn mail_parts() -> (
        dudes_policy::MailWakeFacts,
        dudes_policy::MailWakeWork,
        dudes_policy::CompletedNoteState,
    ) {
        use dudes_proto::mail_wake::MailWakeCandidate;
        use dudes_proto::{MailRef, MailWakePlan, MailWakeTarget, MailWakeTask};
        use dudes_types::{AgentId, GoalId, Lsn, MailId, Provenance};
        let goal = GoalId::from_parts(1, 1);
        let receipt = MailRef {
            id: MailId::from_parts(1, 1),
            lsn: Lsn(20),
        };
        (
            dudes_policy::MailWakeFacts {
                agent: AgentId::from_parts(1, 1),
                completion: Some((goal, Lsn(10))),
                retired: false,
                open_turn: None,
                browser_note_priority: false,
                question_lsn: Some(Lsn(25)),
                wakes_today: 0,
                fifth_crossing: Lsn(15),
                fifth_consumed: Lsn(16),
            },
            dudes_policy::MailWakeWork::Fresh {
                candidate: MailWakeCandidate {
                    goal,
                    completion_lsn: Lsn(10),
                    trigger: receipt,
                    frontier: receipt.lsn,
                },
                plan: MailWakePlan {
                    trigger: receipt,
                    frontier: receipt.lsn,
                    unread_count: 1,
                    task: MailWakeTask::new(1).unwrap(),
                    targets: vec![MailWakeTarget {
                        mail: receipt,
                        original_count: 0,
                        target_count: 1,
                    }],
                },
            },
            dudes_policy::CompletedNoteState {
                control: dudes_policy::Control::default(),
                last_turn_end: Some(Provenance::Observed),
                open_question: Some("real question".into()),
                budget_state: dudes_policy::BudgetState::default(),
                ceilings: dudes_policy::Ceilings {
                    goal: dudes_proto::GoalBudget {
                        usd: None,
                        tokens: None,
                    },
                    agent: dudes_proto::AgentBudget {
                        usd_per_day: None,
                        tokens_per_day: None,
                    },
                    floor_usd_per_day: None,
                },
                per_turn_floor: dudes_proto::Spend::default(),
                rung: dudes_types::BreakerRung::Healthy,
                turns_on_goal: 1,
                hop_cap: 1,
            },
        )
    }

    fn mail_input(
        parts: (
            dudes_policy::MailWakeFacts,
            dudes_policy::MailWakeWork,
            dudes_policy::CompletedNoteState,
        ),
    ) -> dudes_policy::MailWakeInput {
        dudes_policy::MailWakeInput::new(parts.0, parts.1, parts.2).unwrap()
    }

    #[test]
    fn mail_digest_tracks_each_common_admission_value_day_quota_and_debt() {
        use dudes_types::{AgentId, BreakerRung, Lsn, Provenance, Tokens, Usd, UtcDay};
        let baseline = mail_digest(&mail_input(mail_parts())).unwrap();
        for index in 0..35 {
            let (mut facts, work, mut state) = mail_parts();
            let mut day = UtcDay::from_days_since_epoch(1);
            match index {
                0 => facts.agent = AgentId::from_parts(1, 2),
                1 => facts.question_lsn = Some(Lsn(26)),
                2 => day = UtcDay::from_days_since_epoch(2),
                3 => facts.wakes_today = 19,
                4 => facts.fifth_crossing = Lsn(14),
                5 => facts.fifth_consumed = Lsn(17),
                6 => state.control.halted = true,
                7 => state.control.paused = true,
                8 => state.control.attached = true,
                9 => {
                    state.control.automatic_hold =
                        Some(dudes_proto::HoldReason::RepeatedFastFailure);
                }
                10 => state.last_turn_end = Some(Provenance::Inferred),
                11 => state.open_question = Some("changed real question".into()),
                12 => state.budget_state.goal.usd = Usd::from_micros(1),
                13 => state.budget_state.goal.tokens = Tokens::new(1),
                14 => state.budget_state.goal.work_tokens = Tokens::new(1),
                15 => state.budget_state.goal_unknown_ends = 1,
                16 => state.budget_state.agent_today.usd = Usd::from_micros(1),
                17 => state.budget_state.agent_unknown_ends_today = 1,
                18 => state.budget_state.floor_today.usd = Usd::from_micros(1),
                19 => state.budget_state.floor_unknown_ends_today = 1,
                20 => state.ceilings.goal.usd = Some(Usd::ZERO),
                21 => state.ceilings.goal.tokens = Some(Tokens::ZERO),
                22 => state.ceilings.agent.usd_per_day = Some(Usd::ZERO),
                23 => state.ceilings.agent.tokens_per_day = Some(Tokens::ZERO),
                24 => state.ceilings.floor_usd_per_day = Some(Usd::ZERO),
                25 => state.per_turn_floor.usd = Usd::from_micros(1),
                26 => state.per_turn_floor.tokens = Tokens::new(1),
                27 => state.rung = BreakerRung::Steer,
                28 => state.turns_on_goal = 2,
                29 => state.hop_cap = 2,
                30 => state.budget_state.agent_today.tokens = Tokens::new(1),
                31 => state.budget_state.agent_today.work_tokens = Tokens::new(1),
                32 => state.budget_state.floor_today.tokens = Tokens::new(1),
                33 => state.budget_state.floor_today.work_tokens = Tokens::new(1),
                34 => state.per_turn_floor.work_tokens = Tokens::new(1),
                _ => unreachable!(),
            }
            assert_ne!(
                mail_wake_input_digest(&mail_input((facts, work, state)), day).unwrap(),
                baseline,
                "read fact {index}"
            );
        }
    }

    #[test]
    fn mail_digest_tracks_fresh_scope_target_frontier_and_task_snapshot() {
        use dudes_types::{GoalId, Lsn, MailId};
        let baseline = mail_digest(&mail_input(mail_parts())).unwrap();
        for index in 0..5 {
            let (mut facts, mut work, state) = mail_parts();
            let dudes_policy::MailWakeWork::Fresh { candidate, plan } = &mut work else {
                unreachable!()
            };
            match index {
                0 => {
                    let goal = GoalId::from_parts(1, 2);
                    facts.completion = Some((goal, Lsn(10)));
                    candidate.goal = goal;
                }
                1 => {
                    facts.completion = Some((candidate.goal, Lsn(11)));
                    candidate.completion_lsn = Lsn(11);
                }
                2 => {
                    let id = MailId::from_parts(1, 2);
                    candidate.trigger.id = id;
                    plan.trigger.id = id;
                    plan.targets[0].mail.id = id;
                }
                3 => {
                    candidate.frontier = Lsn(21);
                    plan.frontier = Lsn(21);
                }
                4 => {
                    plan.unread_count = 2;
                    plan.task = dudes_proto::MailWakeTask::new(2).unwrap();
                }
                _ => unreachable!(),
            }
            assert_ne!(
                mail_digest(&mail_input((facts, work, state))).unwrap(),
                baseline
            );
        }
    }

    #[test]
    fn mail_digest_resume_domain_progress_root_and_crossing_watermarks_are_distinct() {
        use dudes_proto::mail_wake::{MailWakePreparation, MailWakeProgress};
        use dudes_types::Lsn;
        let (facts, work, state) = mail_parts();
        let fresh = mail_digest(&mail_input((facts.clone(), work.clone(), state.clone()))).unwrap();
        let dudes_policy::MailWakeWork::Fresh { candidate, plan } = work else {
            unreachable!()
        };
        let preparation = MailWakePreparation {
            root_lsn: Lsn(30),
            goal: candidate.goal,
            completion_lsn: candidate.completion_lsn,
            plan,
            progress: vec![MailWakeProgress::Notified { lsn: Lsn(31) }],
        };
        let resume = mail_digest(&mail_input((
            facts.clone(),
            dudes_policy::MailWakeWork::Resume {
                preparation: preparation.clone(),
            },
            state.clone(),
        )))
        .unwrap();
        assert_eq!(fresh.domain, dudes_proto::MailWakeDigestDomain::Fresh);
        assert_eq!(resume.domain, dudes_proto::MailWakeDigestDomain::Resume);
        assert_ne!(fresh.value, resume.value);
        for index in 0..3 {
            let mut preparation = preparation.clone();
            match index {
                0 => preparation.root_lsn = Lsn(29),
                1 => preparation.progress[0] = MailWakeProgress::Notified { lsn: Lsn(32) },
                2 => preparation.progress[0] = MailWakeProgress::Pending,
                _ => unreachable!(),
            }
            assert_ne!(
                mail_digest(&mail_input((
                    facts.clone(),
                    dudes_policy::MailWakeWork::Resume { preparation },
                    state.clone()
                )))
                .unwrap(),
                resume
            );
        }
    }

    #[test]
    fn mail_digest_equal_input_and_raw_ticking_time_preserve_refusal_identity() {
        let (facts, work, mut state) = mail_parts();
        state.control.halted = true;
        let input = mail_input((facts, work, state));
        let early = Timestamp::from_unix_millis(1).utc_day();
        let late = Timestamp::from_unix_millis(86_399_999).utc_day();
        assert_eq!(
            dudes_policy::admit_mail_wake(&input),
            dudes_policy::admit_mail_wake(&input.clone())
        );
        assert_eq!(
            mail_wake_input_digest(&input, early),
            mail_wake_input_digest(&input, late)
        );
        assert_eq!(
            mail_wake_input_digest(&input, early),
            mail_wake_input_digest(&input.clone(), early)
        );
        assert_ne!(
            mail_wake_input_digest(&input, early),
            mail_wake_input_digest(&input, Timestamp::from_unix_millis(86_400_000).utc_day())
        );
    }

    #[test]
    fn mail_digest_encoding_failure_is_named_and_never_an_empty_fallback() {
        for domain in [
            dudes_proto::MailWakeDigestDomain::Fresh,
            dudes_proto::MailWakeDigestDomain::Resume,
        ] {
            assert_eq!(
                canonical_mail_wake_digest(
                    domain,
                    dudes_types::UtcDay::from_days_since_epoch(1),
                    &Unserialisable
                ),
                Err(MailWakeDigestError)
            );
            assert!(
                canonical_mail_wake_digest(
                    domain,
                    dudes_types::UtcDay::from_days_since_epoch(1),
                    "serializable control"
                )
                .is_ok()
            );
        }
    }

    fn key(boot: Option<&str>) -> AdoptionKey {
        AdoptionKey {
            pid: 4242,
            start_time: 900,
            boot: boot.map(ToOwned::to_owned),
        }
    }

    /// The two cells that reach the process table are unchanged: no such pid or a
    /// different start value is `Gone`, an equal start value is `Alive`, and a host that could
    /// not answer is `Unknown`.
    #[test]
    fn the_process_table_decides_in_the_agreeing_cells() {
        for (key, host) in [(key(None), None), (key(Some("b")), Some("b"))] {
            assert_eq!(
                liveness(&key, host, ProbeAnswer::StartValue(900)),
                Liveness::Alive
            );
            assert_eq!(
                liveness(&key, host, ProbeAnswer::StartValue(901)),
                Liveness::Gone
            );
            assert_eq!(liveness(&key, host, ProbeAnswer::NoSuchPid), Liveness::Gone);
            assert_eq!(
                liveness(&key, host, ProbeAnswer::CouldNotAnswer),
                Liveness::Unknown
            );
        }
    }

    /// [`key_is_interpretable`] is the presence half of the same table [`liveness()`] applies, and
    /// the two cannot drift apart: an uninterpretable key is `Unknown` **whatever the probe said**,
    /// and an interpretable one is whatever the rest of the table decides.
    ///
    /// The quantifier over `probe` is the assertion. `system.doctor` marks a turn from the left
    /// side and the loop makes progress from the right, so a predicate that agreed on the two
    /// probes a test happened to name and disagreed on the third would mark a set that is not the
    /// stuck set — and the marking would still look right.
    #[test]
    fn an_uninterpretable_key_is_unknown_whatever_the_process_table_says() {
        let probes = [
            ProbeAnswer::StartValue(900),
            ProbeAnswer::StartValue(901),
            ProbeAnswer::NoSuchPid,
            ProbeAnswer::CouldNotAnswer,
        ];
        for (recorded, host) in [(key(Some("b")), None), (key(None), Some("b"))] {
            assert!(!key_is_interpretable(&recorded, host));
            for probe in probes {
                assert_eq!(
                    liveness(&recorded, host, probe),
                    Liveness::Unknown,
                    "{probe:?} against a key this host cannot interpret"
                );
            }
        }

        // The control: on the agreeing cells the predicate says so, and the probe is what decides
        // — including a cell that answers `Gone`. Without this the loop above would pass against a
        // predicate that answered `false` to everything and a `liveness` that answered `Unknown`.
        for (recorded, host) in [(key(None), None), (key(Some("b")), Some("b"))] {
            assert!(key_is_interpretable(&recorded, host));
            assert_eq!(
                liveness(&recorded, host, ProbeAnswer::StartValue(900)),
                Liveness::Alive
            );
            assert_eq!(
                liveness(&recorded, host, ProbeAnswer::NoSuchPid),
                Liveness::Gone
            );
        }

        // And the cell that compares *values*: two present identities that differ is `Gone`, not
        // an interpretation failure. Presence and value are different questions and this is the
        // one that would collapse them.
        assert!(key_is_interpretable(&key(Some("b")), Some("c")));
        assert_eq!(
            liveness(&key(Some("b")), Some("c"), ProbeAnswer::StartValue(900)),
            Liveness::Gone
        );
    }

    /// A session id derived from a ULID is UUID-shaped: 8-4-4-4-12 lowercase hex, version 4 and
    /// the RFC 4122 variant, and the same ULID always gives the same id. That is all it says: a
    /// re-issued turn draws a fresh ULID unless the vendor confirmed a session.
    #[test]
    fn a_minted_session_id_is_a_stable_uuid() {
        let ulid = Ulid::from_parts(1, 2);
        let id = session_id_from_ulid(ulid);
        let text = id.as_str().to_owned();
        assert_eq!(text.len(), 36);
        let groups: Vec<usize> = text.split('-').map(str::len).collect();
        assert_eq!(groups, [8, 4, 4, 4, 12]);
        assert!(
            text.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "{text}"
        );
        assert_eq!(text.as_bytes()[14], b'4', "version 4: {text}");
        assert!(
            matches!(text.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "RFC 4122 variant: {text}"
        );
        assert_eq!(session_id_from_ulid(ulid).as_str(), text);
    }

    /// The digest is blake3 of the canonical bytes, so an equal input digests equally and a
    /// changed input does not.
    #[test]
    fn the_input_digest_is_blake3_of_the_canonical_bytes() {
        assert_eq!(blake3_hex(b"").len(), 64);
        assert_ne!(blake3_hex(b"a"), blake3_hex(b"b"));
        assert_eq!(blake3_hex(b"a"), blake3_hex(b"a"));

        assert_eq!(canonical_digest("a").map(|d| d.len()), Some(64));
        assert_eq!(canonical_digest("a"), canonical_digest("a"));
        assert_ne!(canonical_digest("a"), canonical_digest("b"));
    }

    /// A value that cannot be serialised. `LoopInput`'s graph holds no fallible serialiser, so
    /// without [`canonical_digest`]'s bound there would be no way to reach the arm below and
    /// the digest-failure rule would be a shape with no test behind it. Reached from here and
    /// nowhere else: this is a child module of `host`, so the helper stays private while the public
    /// [`input_digest`] keeps the concrete `LoopInput` signature its contract asks for.
    struct Unserialisable;

    impl serde::Serialize for Unserialisable {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom(
                "this value has no canonical form",
            ))
        }
    }

    /// a serialisation failure is **not** swallowed into the hash of empty input.
    ///
    /// Digesting `unwrap_or_default()` bytes would produce `blake3_hex(b"")` — a perfectly
    /// well-formed 64-hex digest — so every unserialisable input would digest *equal to every
    /// other one*, and equal to a real input that happened to serialise to nothing. The second
    /// assertion fails for that behaviour. `None` is the only answer that a caller can tell apart
    /// from a digest.
    #[test]
    fn an_unserialisable_input_has_no_digest_rather_than_the_empty_ones() {
        assert_eq!(canonical_digest(&Unserialisable), None);
        assert_ne!(canonical_digest(&Unserialisable), Some(blake3_hex(b"")));
        // Positive control: the same call on a serialisable value does produce one, so the `None`
        // above is the failure and not a function that answers `None` to everything.
        assert!(canonical_digest("a").is_some());
    }
}
