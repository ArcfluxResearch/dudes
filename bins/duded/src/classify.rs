//! The two ports the peer-classification table is decided over, implemented where the daemon is.
//!
//! `dudes-rpc` declares both and implements neither, and that is a layering rule rather than a
//! convention: `cargo xtask layering` forbids `dudes-rpc → dudes-policy`, and `dudes-supervisor`
//! depends on `dudes-policy`, so a `dudes-rpc` that folded the journal or walked a process table
//! would be a transitive violation. This package depends on both already, which is what makes it
//! the place the adapters live.
//!
//! # What ships here, and what does not
//!
//! [`WorldTokens`] is the token projection, and it is a *published snapshot* rather than a fold
//! of its own: `World` folds `HookTokenMinted` and `AgentRetired` on the decide-loop thread, the
//! loop publishes that answer before a folded launch can spawn and again at the end of every
//! `pump`, and this reads the immutable snapshot.
//! A fold produces a fresh owned copy; a pump between folds republishes the same `Arc`. One fold,
//! in the one place `dudes replay` rebuilds (invariant 1) — a second fold here would be a second
//! thing that can be wrong, and the one that is wrong is the one nobody replays.
//!
//! [`NoWalk`] is the explicit fallback for unsupported targets and a fixed test column: it answers
//! [`PeerProvenance::Unresolvable`] for every peer and walks nothing. Linux, macOS and Windows use
//! the host-owned ancestry relation.
//!
//! **`Unresolvable`, never `NoRecordedAgent`, and the difference is journaled rather than
//! decided.** The two reach the same peer-classification column and decide identically, so nothing in the
//! suite separates them — but the journal must tell *"unresolvable because the
//! named process was gone"* from *"walked cleanly to no match"*, and only the first is a
//! laundering signal. Recording the second for a walk that never ran would be a false `Observed`.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
use std::sync::{Condvar, Mutex, PoisonError};
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
use std::time::{Duration, Instant};

use dudes_proto::rpc::hook::HookToken;
use dudes_proto::rpc::pty::PtyId;
use dudes_rpc::auth::{
    MintedTokens, PeerIdentity, PeerProvenance, PeerResolution, PeerResolver, TokenOwner,
};
use std::io;

use dudes_providers::adapter::{HookInstall, ManagedInstall};
use dudes_providers::launch::LaunchSpec;
use dudes_supervisor::host::{
    AdoptionKey, HostBoot, KillOutcome, Liveness, NativeBoundaryAcknowledgement,
    NativeBoundaryReceipt, NativeGroupPresence, OwnedTargets, ProbeAnswer, ProcessHost,
    SpawnResult,
};
use dudes_supervisor::{MintedFor, World, blake3_hex};
use dudes_types::AgentId;

use crate::ports::{
    ExitCause, Handle, OpenedPty, PtyChild, PtyCloseOutcome, PtyHostPort, PtyOpenFailure,
};
use crate::pty_output::OutputSubscription;

/// The resolver a daemon runs before it can walk an ancestry chain: every peer is
/// [`PeerProvenance::Unresolvable`].
///
/// # What this costs, stated where it is taken
///
/// It is not neutral. An operator credential presented from inside an
/// agent's tree is refused — the (operator credential × `Agent(_)`) cell of `classify` — and a
/// resolver that never produces that column never produces that refusal. So that corroboration does not apply to any peer of this daemon,
/// and the (operator credential × `Unresolvable`) cell → `Operator` for every peer that can read the operator credential
/// file beside the socket. **Same uid is not a boundary** (invariant 8),
/// so that is a real widening over a daemon that refuses every method but `system.ping`, and it is
/// the price of shipping a door at all.
///
/// It is fail-*open* in exactly one direction and fail-closed in the other, which is why it is the
/// correct interim answer rather than merely the cheap one: row 2 — an agent's token — is
/// **refused** in this column (`Unresolvable` is not `Agent(A)`), so no peer can be honoured as an
/// agent while this ships.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoWalk;

impl PeerResolver for NoWalk {
    /// The walk that was not performed.
    ///
    /// It reads its argument for nothing, and that is the honest answer rather than an oversight:
    /// A walk that could not run resolves here, as neither an error nor a
    /// match, and a walk that was never attempted is a walk that could not run.
    ///
    /// **`Resolved`, and never [`PeerResolution::Refuse`].** Not walking is a *completed*
    /// resolution: it reaches a classification every time, on every peer, immediately. The
    /// refusal is for a resolution that never answered, and this one always answers.
    fn resolve(&self, _peer: &PeerIdentity) -> PeerResolution {
        PeerResolution::Resolved(PeerProvenance::Unresolvable)
    }
}

/// The daemon's own resolver, wrapped so that a panic inside it becomes a named daemon exit.
///
/// # Why a panic here is the daemon's business and not one connection's
///
/// `dudes_rpc::serve` asks the resolver on the connection's own thread, so an unguarded panic
/// unwinds that one thread and nothing else. Scoped threads are joined at the end of the scope
/// rather than when they die, so nothing observes it until the accept loop stops for some other
/// reason — meanwhile the door stays open and **every** subsequent connection fails identically.
/// A daemon in that state answers no peer honestly while reporting that it is healthy, which is
/// exactly what invariant 8's *"unproven identity is refused"* forbids being silent about.
///
/// So the panic is caught where it happens and turned into [`ExitCause::ResolverPanicked`]. The
/// daemon's ordinary exit sequence then runs in full — invariant 10's daemon-exit half included —
/// instead of being skipped by an unwind that reaches nobody.
///
/// # Why this lives here rather than in `dudes-rpc`
///
/// `serve` is a library entry point with callers it cannot know, and "a broken resolver means the
/// program is over" is a policy, not a transport rule. The crate contains the failure; the daemon
/// decides what it costs.
///
/// # It answers no column at all, and the connection is not served
///
/// A caught panic is not a resolution, so this type never turns one into a column. The connection
/// whose resolve panicked is left **unserved** — the panic is re-raised on its own thread once the
/// stop is sent, before any conditioned context exists and before a byte of a request is read —
/// and every connection that arrives before the door closes is refused the same way, by a panic
/// raised before the inner resolver is touched at all.
///
/// The alternative was to answer [`PeerProvenance::Unresolvable`] and let the connection run. That
/// is a **fabricated** column rather than a missing one, and it fails open: in `classify`'s table the
/// (operator credential × `Unresolvable`) cell *admits* the credential, so it would switch off the
/// refusal of an operator credential presented from inside an agent's tree, by a
/// panic, for that connection and for every connection after it. A *resolver* may
/// answer `Unresolvable` when its own read refused; it licenses nothing to answer on behalf of
/// a resolver caught mid-walk. That [`NoWalk`] supplies that fallback column does not make the answer
/// free: the guard's first live moment is a resolver that walks, which is the moment the two stop
/// being the same claim.
///
/// # The latch, and what `Relaxed` buys
///
/// A resolver that has panicked once is not asked again: the flag is stored before the unwind is
/// resumed, and a resolution that observes it panics without reaching the inner resolver.
///
/// That is the whole of this type's `AssertUnwindSafe` argument, and it is narrower than
/// exclusion. Resolution is concurrent — one thread per connection — and the flag is stored
/// *after* the catch, so a caller already past the load when the panic lands still enters the
/// resolver in the state it broke in. What makes that sound is not the latch but where such a call
/// goes: on a panicking path this function returns nothing, so whatever a broken resolver answers
/// a caller inside that window is discarded by the unwind instead of reaching `classify`. No caller
/// that observes the latch enters the inner resolver at all.
///
/// `Relaxed` is what a flag read for itself needs: it publishes no other data and guards no other
/// read, and the only cost of a stale read is one more call into a resolver whose panic is caught
/// again.
#[derive(Debug)]
pub(crate) struct GuardedResolver {
    inner: Arc<dyn PeerResolver>,
    handle: Handle,
    /// Set once, by the first panic, and read for itself. `Relaxed` because it publishes no other
    /// data — the only consequence of a stale read is one more call into a resolver that will be
    /// caught again.
    broken: AtomicBool,
}

impl GuardedResolver {
    /// Wrap `inner`, stopping the loop `handle` reaches if it ever panics.
    pub(crate) fn wrap(inner: Arc<dyn PeerResolver>, handle: Handle) -> Arc<Self> {
        Arc::new(Self {
            inner,
            handle,
            broken: AtomicBool::new(false),
        })
    }
}

impl PeerResolver for GuardedResolver {
    /// The inner resolver's answer — its refusal included — or no answer at all.
    ///
    /// **There is a failure value, and this function still does not use it.**
    /// [`PeerResolution::Refuse`] exists, so the panic is not for want of a way to say "nothing was
    /// resolved". Two other reasons hold, and neither is the absence of a value:
    ///
    /// 1. **The producer list is closed.** The only producers of `Refuse` are the
    ///    daemon barrier's non-answers and the host loop's exhausted-ceiling answer. A caught
    ///    panic is not on that list, and widening a closed list needs a documented decision
    ///    rather than an implementation choice.
    /// 2. **A refusal and a panic mean different things to whoever reads the log.** `Refuse` is a
    ///    bounded, per-connection event expected to recur: a peer whose resolution
    ///    collided with a spawn in flight loses that connection and reconnects. A resolver that
    ///    panicked is a daemon that can place no peer at all, and it stops for
    ///    [`ExitCause::ResolverPanicked`]. One arm for both would cost exactly the discrimination
    ///    this path needs.
    ///
    /// What it *does* pass through untouched is the inner resolver's own `Refuse`. A guard that
    /// re-coloured one would swallow that `Refuse` entirely.
    fn resolve(&self, peer: &PeerIdentity) -> PeerResolution {
        // The refusal, not a debug check: it is read before the inner resolver is touched, so a
        // latched guard asks nothing and answers nothing, and the panic it raises is what leaves
        // this connection unserved. The daemon is already stopping for the panic that set the flag.
        assert!(
            !self.broken.load(Ordering::Relaxed),
            "the peer resolver is broken; this connection is not resolved"
        );
        // `AssertUnwindSafe` for the reason the type doc gives: nothing this function returns is
        // derived from a resolver that panicked, so the state it broke in reaches no caller.
        match catch_unwind(AssertUnwindSafe(|| self.inner.resolve(peer))) {
            // Pass-through, `Refuse` included: the guard adds an exit, it never edits an answer.
            Ok(resolution) => resolution,
            Err(payload) => {
                self.broken.store(true, Ordering::Relaxed);
                // A constant. The payload is the resolver's own message about the peer it was
                // reading, and this line goes to the daemon's log.
                tracing::error!(
                    "the peer resolver panicked; no connection can be given a provenance, so the \
                     daemon is stopping and this one is not served"
                );
                self.handle.stop(ExitCause::ResolverPanicked);
                resume_unwind(payload);
            }
        }
    }
}

/// The resolver passed through the daemon's seam, with an optional live projection publisher.
///
/// Tests keep supplying fixed peer-classification columns. Production Linux, macOS and Windows construct a
/// [`ProcessRelation`] beside their concrete host and pass it to [`Self::processes`]. A
/// just-created child has an opaque pending
/// slot: its connection waits until the ordinary quiescent `World` publication supplies the full
/// key-to-agent mapping, so a stale snapshot can never turn that child into `NoRecordedAgent` and
/// admit its operator credential.
#[derive(Clone)]
pub struct Resolver {
    inner: Arc<dyn PeerResolver>,
    owners: Option<WorldChildren>,
    #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
    processes: Option<Arc<dyn ProcessRelation>>,
}

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("Resolver");
        debug
            .field("inner", &self.inner)
            .field("publishes_children", &self.owners.is_some());
        #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
        debug.field("spawn_barrier", &self.processes.is_some());
        debug.finish()
    }
}

impl Resolver {
    /// A fixed resolver, used on unsupported targets and by tests selecting a peer-classification column.
    #[must_use]
    pub fn fixed(inner: Arc<dyn PeerResolver>) -> Self {
        Self {
            inner,
            owners: None,
            #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
            processes: None,
        }
    }

    /// The live process-ancestry resolver over a host's process-table view.
    #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
    #[must_use]
    pub fn processes(processes: Arc<dyn ProcessRelation>) -> Self {
        let owners = WorldChildren::default();
        Self {
            inner: Arc::new(ProcessResolver {
                processes: Arc::clone(&processes),
                owners: owners.clone(),
            }),
            owners: Some(owners),
            processes: Some(processes),
        }
    }

    /// The agent processes alive now, read afresh by each caller.
    #[must_use]
    pub(crate) fn agent_roots(&self) -> Arc<dyn crate::floor_peer::AgentRoots> {
        #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
        if let Some(owners) = &self.owners {
            return Arc::new(ResolverRoots {
                owners: owners.clone(),
                processes: self.processes.clone(),
            });
        }
        Arc::new(crate::floor_peer::NoAgents)
    }

    /// Arm the publication barrier before the host starts creating the child.
    #[doc(hidden)]
    #[must_use]
    pub fn begin_spawn(&self) -> Option<PendingSpawn> {
        let owners = self.owners.as_ref()?;
        #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
        if let Some(processes) = &self.processes {
            let slot = processes.begin_spawn();
            owners.arm(slot);
            return Some(PendingSpawn { slot });
        }
        let _ = owners;
        None
    }

    /// Confirm that the host recorded this full key in the armed slot.
    ///
    /// This deliberately does not publish an agent. The single-writer `World` supplies that half
    /// at pump quiescence; a matching connection remains behind the barrier until then.
    #[doc(hidden)]
    pub fn map_spawn(&self, pending: PendingSpawn, key: &AdoptionKey) -> bool {
        let Some(owners) = &self.owners else {
            let _ = (pending, key);
            return true;
        };
        #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
        if let Some(processes) = &self.processes {
            if processes.finish_spawn(pending.slot, Some(key)) {
                return true;
            }
            if processes.finish_spawn(pending.slot, None) {
                owners.abort(pending.slot);
            }
            return false;
        }
        let _ = (owners, pending, key);
        true
    }

    /// Arm the barrier for a child the PTY host is about to create, and activate its slot in the
    /// relation so a peer of that child waits from before the exec.
    ///
    /// `None` wherever the resolver publishes nothing, as for [`Self::begin_spawn`].
    fn begin_external(&self) -> Option<ExternalSpawn> {
        let owners = self.owners.as_ref()?;
        #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
        if let Some(processes) = &self.processes {
            let slot = processes.begin_spawn();
            owners.arm(slot);
            return Some(ExternalSpawn {
                pending: PendingSpawn { slot },
                child: processes.track_external(slot),
            });
        }
        let _ = owners;
        None
    }

    /// Release the relation's hold on a child, then the barrier.
    fn abort_external(&self, spawn: ExternalSpawn) {
        let ExternalSpawn { pending, child } = spawn;
        drop(child);
        self.abort_spawn(pending);
    }

    /// Release a barrier only after a failed spawn has left no process behind.
    #[doc(hidden)]
    pub fn abort_spawn(&self, pending: PendingSpawn) {
        let Some(owners) = &self.owners else {
            let _ = pending;
            return;
        };
        #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
        if let Some(processes) = &self.processes {
            if processes.finish_spawn(pending.slot, None) {
                owners.abort(pending.slot);
            }
            return;
        }
        let _ = (owners, pending);
    }

    /// Publish every recorded child without a recorded end after a pump reaches quiescence, then
    /// wake any pending connection whose complete key now maps to an agent.
    #[doc(hidden)]
    pub fn publish(&self, world: &World) {
        let Some(owners) = &self.owners else {
            return;
        };
        let Some(snapshot) = owners.publish(world) else {
            return;
        };
        #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
        if let Some(processes) = &self.processes {
            let completed = processes.published(&snapshot.keys);
            for (slot, index) in completed {
                if let (Some(agent), Some(key)) = (
                    snapshot.agents.get(index).copied(),
                    snapshot.keys.get(index),
                ) {
                    owners.map(slot, agent, key);
                }
            }
            owners.retain(&snapshot.keys);
        }
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos", test)))]
        let _ = snapshot;
    }
}

impl PeerResolver for Resolver {
    fn resolve(&self, peer: &PeerIdentity) -> PeerResolution {
        self.inner.resolve(peer)
    }
}

/// The published open-children snapshot, plus the barrier a just-spawned child waits behind.
#[derive(Clone, Debug, Default)]
struct WorldChildren {
    published: Arc<RwLock<Arc<ChildOwners>>>,
    #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
    pending: Arc<(Mutex<BTreeMap<u64, PendingOwner>>, Condvar)>,
}

/// Two aligned vectors rather than a map: the host answers with an *index* into the key slice it
/// was handed, so the agent at that index is the answer and no second lookup can disagree.
#[derive(Debug, Default)]
struct ChildOwners {
    agents: Vec<AgentId>,
    keys: Vec<AdoptionKey>,
}

#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
fn same_key(left: &AdoptionKey, right: &AdoptionKey) -> bool {
    left.pid == right.pid
        && left.start_time == right.start_time
        && left.boot.as_deref() == right.boot.as_deref()
}

/// One host spawn between arming and the full `World` projection catching up.
#[derive(Clone, Copy, Debug)]
pub struct PendingSpawn {
    #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
    slot: u64,
}

/// One PTY child between arming and its publication, and the relation's hold on it.
struct ExternalSpawn {
    pending: PendingSpawn,
    child: Box<dyn ExternalChild>,
}

#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
#[derive(Clone, Debug)]
enum PendingOwner {
    Waiting,
    Mapped { agent: AgentId, key: AdoptionKey },
}

#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
enum PendingAnswer {
    Mapped(AgentId),
    Aborted,
    TimedOut,
}

impl WorldChildren {
    fn publish(&self, world: &World) -> Option<Arc<ChildOwners>> {
        let (agents, keys) = world.recorded_children().unzip();
        let snapshot = Arc::new(ChildOwners { agents, keys });
        let mut slot = self.published.write().ok()?;
        *slot = Arc::clone(&snapshot);
        drop(slot);
        Some(snapshot)
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
    fn snapshot(&self) -> Option<Arc<ChildOwners>> {
        self.published.read().ok().map(|slot| Arc::clone(&slot))
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
    fn arm(&self, slot: u64) {
        let (pending, _) = &*self.pending;
        pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(slot, PendingOwner::Waiting);
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
    fn map(&self, slot: u64, agent: AgentId, key: &AdoptionKey) {
        let (pending, changed) = &*self.pending;
        let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(owner) = pending.get_mut(&slot) {
            *owner = PendingOwner::Mapped {
                agent,
                key: key.clone(),
            };
            changed.notify_all();
        }
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
    fn abort(&self, slot: u64) {
        let (pending, changed) = &*self.pending;
        pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&slot);
        changed.notify_all();
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
    fn retain(&self, keys: &[AdoptionKey]) {
        let (pending, changed) = &*self.pending;
        let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
        let before = pending.len();
        pending.retain(|_, owner| match owner {
            PendingOwner::Waiting => true,
            PendingOwner::Mapped { key, .. } => keys.iter().any(|live| same_key(key, live)),
        });
        if pending.len() != before {
            changed.notify_all();
        }
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
    fn wait(&self, slot: u64, bound: Duration) -> PendingAnswer {
        let (pending, changed) = &*self.pending;
        let pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
        let (pending, _) = changed
            .wait_timeout_while(pending, bound, |pending| {
                matches!(pending.get(&slot), Some(PendingOwner::Waiting))
            })
            .unwrap_or_else(PoisonError::into_inner);
        match pending.get(&slot) {
            Some(PendingOwner::Mapped { agent, .. }) => PendingAnswer::Mapped(*agent),
            Some(PendingOwner::Waiting) => PendingAnswer::TimedOut,
            None => PendingAnswer::Aborted,
        }
    }
}

#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
#[derive(Clone, Debug)]
struct ProcessResolver {
    processes: Arc<dyn ProcessRelation>,
    owners: WorldChildren,
}

/// The agent processes the daemon knows: the recorded children, plus any the host already tracks.
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
#[derive(Debug)]
struct ResolverRoots {
    owners: WorldChildren,
    processes: Option<Arc<dyn ProcessRelation>>,
}

#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
impl crate::floor_peer::AgentRoots for ResolverRoots {
    fn live(&self) -> Vec<crate::floor_peer::AgentRoot> {
        let mut roots: Vec<_> = self
            .owners
            .snapshot()
            .map(|recorded| {
                recorded
                    .agents
                    .iter()
                    .zip(&recorded.keys)
                    .map(|(agent, key)| crate::floor_peer::AgentRoot {
                        pid: key.pid,
                        agent: Some(*agent),
                    })
                    .collect()
            })
            .unwrap_or_default();
        for pid in self.processes.iter().flat_map(|host| host.live_pids()) {
            if roots.iter().all(|root| root.pid != pid) {
                roots.push(crate::floor_peer::AgentRoot { pid, agent: None });
            }
        }
        roots
    }
}

/// The process-host result the daemon maps to a peer-classification column.
///
/// The daemon's mirror of `dudes_host::PeerRelation`, one hop across the crate boundary. Two enums
/// rather than a re-export because the mapping is the thing worth pinning: it is the hop where an
/// answer about *processes* becomes an answer about *agents*, and a mis-pointed arm compiles.
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    /// The key at this index is the first full platform relation match.
    Matched(usize),
    /// The peer belongs to a child whose full key exists but whose `World` row is not published.
    Pending(u64),
    /// The walk completed without a matching recorded child.
    NoMatch,
    /// A required platform read produced no evidence.
    Unresolvable,
    /// The host's own pending loop exhausted its bound and gave up.
    ///
    /// **The only host-side answer that is not about the peer**, and the only one this adapter
    /// turns into [`PeerResolution::Refuse`]. The four above each name a peer-classification column; this one
    /// says a loop ran out of time, and the nearest column — `NoRecordedAgent` — is the cell that
    /// *admits* an operator credential.
    Abandoned,
}

/// The process relation supplied where the concrete child owner is constructed.
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
pub trait ProcessRelation: std::fmt::Debug + Send + Sync {
    /// Allocate the slot the host associates with its next process creation.
    fn begin_spawn(&self) -> u64;
    /// Confirm `slot`'s key, or abort it with `None` after cleanup.
    fn finish_spawn(&self, slot: u64, key: Option<&AdoptionKey>) -> bool;
    /// Return each slot and key index now present in the complete `World` snapshot.
    fn published(&self, children: &[AdoptionKey]) -> Vec<(u64, usize)>;
    /// Resolve `peer` against the aligned recorded-child key slice.
    fn resolve(&self, peer: u32, pidversion: Option<u32>, children: &[AdoptionKey]) -> Relation;
    /// Every child process the host tracks now, including one spawned a moment ago whose agent
    /// the `World` has not yet recorded. The Floor refuses requests from inside any of them.
    fn live_pids(&self) -> Vec<u32> {
        Vec::new()
    }
    /// Activate `slot` for a child another owner is about to create.
    ///
    /// The default registers nothing, so a PTY opened through a relation without it is refused.
    fn track_external(&self, slot: u64) -> Box<dyn ExternalChild> {
        let _ = slot;
        Box::new(Untracked)
    }
}

/// A process relation's hold on one child another owner created. Dropping it
/// releases the child's registration, or the slot when no child was registered.
pub trait ExternalChild: std::fmt::Debug + Send {
    /// Register the created child and answer its full adoption key.
    ///
    /// # Errors
    ///
    /// The child could not be registered or its key could not be read.
    fn register(&mut self, child: PtyChild) -> io::Result<AdoptionKey>;
}

/// The hold a relation that cannot register an external child answers with.
#[derive(Debug)]
struct Untracked;

impl ExternalChild for Untracked {
    fn register(&mut self, _child: PtyChild) -> io::Result<AdoptionKey> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this process relation cannot register an attached child",
        ))
    }
}

/// How long **one** wait on the publication barrier blocks before the loop looks again.
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
const PENDING_MAP_BOUND: Duration = Duration::from_secs(1);

/// How long the barrier loop may run **from entry**, across every wait it takes, before it
/// refuses.
///
/// # Why this number, and why there are two of them
///
/// It bounds waiting for one thing: a `World` pump to publish the agent that owns a key the host
/// has *already* matched. That is decide-loop work on a queue this connection is not in, so it is
/// not proportional to anything the peer does, and three whole [`PENDING_MAP_BOUND`] waits is
/// ample. It is deliberately the same three seconds as `dudes_host`'s `PENDING_ABANDON_BOUND`, so
/// the `10 ×` liveness guard the tests use is one number — 30 s — on both sides instead of
/// two a reader has to keep apart.
///
/// Two constants rather than one because `bins/duded` declares `dudes-host` and the reverse edge
/// would be a **Cargo dependency cycle**, which Cargo rejects outright. That is a build-graph
/// fact; **it is not invariant 9**, and nothing here may say it is.
///
/// The loops are sequential, not nested: the host's `resolve` runs to completion under its own
/// bound, and only a `Relation::Pending(slot)` answer then enters this one. Nothing re-enters the
/// host's loop afterwards, so `PeerResolver::resolve` returns within
/// `PENDING_ABANDON_BOUND + PENDING_RESOLVE_CEILING` of entry on every path, and the only outcome
/// of exhausting either is [`PeerResolution::Refuse`].
///
/// **A lower bound on the refusal, not an upper bound on the call.** Neither constant bounds the
/// syscalls between two waits, so no test asserts a maximum elapsed; one would be measuring the
/// platform rather than this code.
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
const PENDING_RESOLVE_CEILING: Duration = Duration::from_secs(3);

#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
impl PeerResolver for ProcessResolver {
    fn resolve(&self, peer: &PeerIdentity) -> PeerResolution {
        use dudes_rpc::auth::PeerPid;

        // Every arm that reaches a column answers `Resolved`. The producer
        // list for `Refuse` is closed to the two barrier non-answers and the host's exhausted ceiling, and a
        // read that refused is a completed resolution that reached `Unresolvable`.
        let (pid, pidversion) = match peer.pid {
            PeerPid::Reported { pid, pidversion } => match u32::try_from(pid) {
                Ok(pid) => (pid, pidversion),
                Err(_) => return PeerResolution::Resolved(PeerProvenance::Unresolvable),
            },
            PeerPid::Unreadable => return PeerResolution::Resolved(PeerProvenance::Unresolvable),
        };
        let Some(owners) = self.owners.snapshot() else {
            return PeerResolution::Resolved(PeerProvenance::Unresolvable);
        };
        match self.processes.resolve(pid, pidversion, &owners.keys) {
            Relation::Matched(index) => PeerResolution::Resolved(
                owners
                    .agents
                    .get(index)
                    .copied()
                    .map_or(PeerProvenance::Unresolvable, PeerProvenance::Agent),
            ),
            Relation::Pending(slot) => self.wait_for_mapping(slot),
            Relation::NoMatch => PeerResolution::Resolved(PeerProvenance::NoRecordedAgent),
            Relation::Unresolvable => PeerResolution::Resolved(PeerProvenance::Unresolvable),
            // The only place a host-side non-answer becomes a refusal.
            Relation::Abandoned => PeerResolution::Refuse,
        }
    }
}

#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
impl ProcessResolver {
    /// Wait for the pump to name the agent that owns `slot`, for at most
    /// [`PENDING_RESOLVE_CEILING`] from **entry to this function**, not per wait.
    ///
    /// The `TimedOut` arm leaves the loop only when `finish_spawn(slot, None)` succeeds, and it can
    /// fail indefinitely — a job whose termination the platform refuses answers `false` every
    /// time. Without an entry-relative bound this loop has no exit, and a resolution that never
    /// returns is not a refusal, it is a hang.
    ///
    /// On Windows, `finish_spawn(slot, None)` may terminate a slot still attached to a live job;
    /// the Unix host refuses that cleanup claim while the child remains attached. The adapter
    /// preserves either platform answer. `PENDING_RESOLVE_CEILING` bounds how long a refused
    /// cleanup can keep the connection waiting.
    fn wait_for_mapping(&self, slot: u64) -> PeerResolution {
        let ceiling_at = Instant::now() + PENDING_RESOLVE_CEILING;
        loop {
            let now = Instant::now();
            // Entry-relative, and checked before each wait rather than after it, so no arm below
            // can extend the call past the ceiling.
            if now >= ceiling_at {
                return PeerResolution::Refuse;
            }
            let bound = PENDING_MAP_BOUND.min(ceiling_at.saturating_duration_since(now));
            match self.owners.wait(slot, bound) {
                PendingAnswer::Mapped(agent) => {
                    return PeerResolution::Resolved(PeerProvenance::Agent(agent));
                }
                PendingAnswer::Aborted => return PeerResolution::Refuse,
                PendingAnswer::TimedOut => {
                    if self.processes.finish_spawn(slot, None) {
                        self.owners.abort(slot);
                        return PeerResolution::Refuse;
                    }
                }
            }
        }
    }
}

/// A [`ProcessHost`] that arms and releases the resolver's publication barrier around each spawn.
///
/// # Why a wrapper and not a call inside the executor
///
/// The spawn the barrier has to bracket is `dudes_supervisor`'s effect executor calling
/// `ProcessHost::spawn`, and that executor is in another crate. Wrapping the host is how this
/// crate gets between the two without reaching into it — the same shape `crate::fault`'s
/// injectors already use, and the same reason: a `ProcessHost` is the seam the daemon owns.
///
/// # What the bracket is for
///
/// A child exists, and is connectable, before the `World` row naming its agent is published. In
/// that window a walk that found the child would have nothing to map it to and would answer
/// `NoRecordedAgent` — the cell that *admits* an operator credential. Arming before the
/// exec and releasing after it is what turns that window into a wait instead of an admission.
///
/// **A no-op wherever the resolver publishes nothing.** [`Resolver::begin_spawn`] answers `None`
/// for a fixed resolver, which is every unsupported target and every fixed-column test, so this
/// wrapper delegates there without arming a slot. Linux, macOS and Windows production use the
/// process relation and therefore take the active barrier path.
#[derive(Debug)]
pub struct PeerBarrier<H> {
    inner: H,
    resolver: Resolver,
}

impl<H: ProcessHost> PeerBarrier<H> {
    /// Wrap `inner`, arming `resolver`'s barrier around every launch it performs.
    pub const fn new(inner: H, resolver: Resolver) -> Self {
        Self { inner, resolver }
    }
}

impl<H: ProcessHost> ProcessHost for PeerBarrier<H> {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        // Armed before the exec, because the window this closes opens at the exec.
        let pending = self.resolver.begin_spawn();
        let result = self.inner.spawn(spec, install);
        let Some(pending) = pending else {
            return result;
        };
        match &result {
            // The host recorded a full key. The barrier stays armed — the agent half is the
            // pump's to publish — but the slot is confirmed as this key's.
            SpawnResult::Spawned(key) => {
                if !self.resolver.map_spawn(pending, key) {
                    tracing::warn!(
                        "the host's spawn slot did not carry the key it reported, or the child \
                         exited before the slot was confirmed"
                    );
                }
            }
            // No child, or a child whose cleanup the host could not confirm. Either way nothing
            // will ever publish an agent for this slot, so releasing it is what keeps a later
            // peer from waiting out the whole ceiling for an answer that is not coming.
            SpawnResult::NotLaunched { .. } | SpawnResult::CleanupUnconfirmed { .. } => {
                self.resolver.abort_spawn(pending);
            }
        }
        result
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.inner.kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.inner.alive(key)
    }

    fn probe_native_leader(&self, key: &AdoptionKey) -> ProbeAnswer {
        self.inner.probe_native_leader(key)
    }

    fn native_group_presence(&self, pid: u32) -> NativeGroupPresence {
        self.inner.native_group_presence(pid)
    }

    fn pid_namespace(&self) -> Option<String> {
        self.inner.pid_namespace()
    }

    fn owned_targets(&self, limit: usize) -> OwnedTargets {
        self.inner.owned_targets(limit)
    }

    fn acknowledge_native_boundary_absence(
        &mut self,
        key: &AdoptionKey,
        receipt: &NativeBoundaryReceipt,
    ) -> NativeBoundaryAcknowledgement {
        self.inner.acknowledge_native_boundary_absence(key, receipt)
    }

    fn boot_identity(&self) -> HostBoot {
        self.inner.boot_identity()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown()
    }

    fn write_hooks(&mut self, install: &HookInstall) -> io::Result<()> {
        self.inner.write_hooks(install)
    }
}

/// The attached PTY port under the same barrier as [`PeerBarrier`].
///
/// The slot is armed and active before the PTY host execs the child, the child is registered with
/// the process relation and its key read after, and the barrier releases when the pump publishes
/// the `PtySpawned` fold. A child whose registration fails is closed and never reported open. The
/// relation's hold on a registered child lasts until the host confirms its PTY closed, so a PTY
/// whose close failed stays governed.
///
/// On Linux, macOS and Windows a fixed resolver, which only fixed-column tests use there, publishes
/// nothing, and the PTY opens with no registration and no key, as [`PeerBarrier`] does. Other
/// targets walk nothing, so they refuse the attach.
pub(crate) struct PtyBarrier {
    inner: Box<dyn PtyHostPort>,
    resolver: Resolver,
    tracked: BTreeMap<PtyId, TrackedPty>,
}

struct TrackedPty {
    _child: Box<dyn ExternalChild>,
    key: AdoptionKey,
    cwd: std::path::PathBuf,
}

impl PtyBarrier {
    pub(crate) fn new(inner: Box<dyn PtyHostPort>, resolver: Resolver) -> Self {
        Self {
            inner,
            resolver,
            tracked: BTreeMap::new(),
        }
    }

    /// Open one attached launch, and answer its handle with the key of its registered child.
    pub(crate) fn open(
        &mut self,
        spec: LaunchSpec,
        cols: u16,
        rows: u16,
    ) -> Result<(PtyId, Option<AdoptionKey>), PtyOpenFailure> {
        let cwd = spec.cwd.clone();
        let Some(mut spawn) = self.resolver.begin_external() else {
            #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
            {
                let _ = (spec, cols, rows);
                return Err(PtyOpenFailure::Failed(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "this platform cannot register an attached child, so the attach is refused",
                )));
            }
            #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
            return self
                .inner
                .open(spec, cols, rows)
                .map(|opened| (opened.pty, None));
        };
        let OpenedPty { pty, child } = match self.inner.open(spec, cols, rows) {
            Ok(opened) => opened,
            Err(failure) => {
                self.resolver.abort_external(spawn);
                return Err(failure);
            }
        };
        let registered = spawn.child.register(child).and_then(|key| {
            if self.resolver.map_spawn(spawn.pending, &key) {
                Ok(key)
            } else {
                Err(io::Error::other(
                    "the process relation did not record the key it read",
                ))
            }
        });
        match registered {
            Ok(key) => {
                self.tracked.insert(
                    pty,
                    TrackedPty {
                        _child: spawn.child,
                        key: key.clone(),
                        cwd,
                    },
                );
                Ok((pty, Some(key)))
            }
            // Closed before the barrier aborts, so a peer of the child waits out the close.
            Err(cause) => {
                let failure = match self.inner.close(pty) {
                    PtyCloseOutcome::Closed | PtyCloseOutcome::AlreadyClosed => {
                        PtyOpenFailure::Failed(cause)
                    }
                    PtyCloseOutcome::Failed(cleanup) => PtyOpenFailure::CleanupUnconfirmed {
                        pty,
                        cause,
                        cleanup,
                    },
                };
                self.resolver.abort_external(spawn);
                Err(failure)
            }
        }
    }

    pub(crate) fn receipt(&self, pty: PtyId) -> Option<(&AdoptionKey, &std::path::Path)> {
        self.tracked
            .get(&pty)
            .map(|child| (&child.key, child.cwd.as_path()))
    }

    pub(crate) fn write(&mut self, pty: PtyId, bytes: &[u8]) -> io::Result<usize> {
        self.inner.write(pty, bytes)
    }

    pub(crate) fn resize(&mut self, pty: PtyId, cols: u16, rows: u16) -> io::Result<()> {
        self.inner.resize(pty, cols, rows)
    }

    pub(crate) fn subscribe(&mut self, pty: PtyId) -> Option<OutputSubscription> {
        self.inner.subscribe(pty)
    }

    /// Close one handle; only a confirmed close releases its child from the relation.
    pub(crate) fn close(&mut self, pty: PtyId) -> PtyCloseOutcome {
        let outcome = self.inner.close(pty);
        if matches!(
            outcome,
            PtyCloseOutcome::Closed | PtyCloseOutcome::AlreadyClosed
        ) {
            self.tracked.remove(&pty);
        }
        outcome
    }

    pub(crate) fn track_native_close(
        &mut self,
        pty: PtyId,
    ) -> Result<dudes_supervisor::host::NativeBoundaryReceipt, crate::ports::PtyNativeBoundaryUnknown>
    {
        self.inner.track_native_close(pty)
    }

    pub(crate) fn native_close_absence(
        &mut self,
        pty: PtyId,
        identity: &dudes_supervisor::host::NativeBoundaryReceipt,
    ) -> Result<(), crate::ports::PtyNativeBoundaryUnknown> {
        self.inner.native_close_absence(pty, identity)
    }

    pub(crate) fn acknowledge_native_close(
        &mut self,
        pty: PtyId,
        identity: &dudes_supervisor::host::NativeBoundaryReceipt,
    ) -> bool {
        self.inner.acknowledge_native_close(pty, identity)
    }

    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        let result = self.inner.shutdown();
        if result.is_ok() {
            self.tracked.clear();
        }
        result
    }
}

/// Which agent each journaled agent-token hash was minted for, and which token row it reaches —
/// `World::token_hash_owners`'s answer, as an owned value that can cross a thread boundary.
pub type TokenOwners = BTreeMap<String, MintedFor>;

/// A read-only count of entries in freshly built token-owner snapshots that were published.
///
/// "Published" means the write lock was acquired and the fresh snapshot replaced the prior one. A
/// fresh snapshot successfully published before a failed pump returns is counted; a snapshot that
/// cannot be published through a poisoned lock is not. A quiet `Arc` reuse records no copied
/// entries. This is a direct work unit, not a timer and not a count of every hash the `World`
/// visited: the hashes of retired agents with no recorded child are filtered before the owned map
/// reaches this adapter. Kept as its own handle so a daemon integration test can retain the
/// observation after [`crate::Daemon::run`] consumes the daemon without gaining any way to read
/// or replace the authority snapshot itself.
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct TokenPublicationWork {
    copied: Arc<AtomicUsize>,
}

impl TokenPublicationWork {
    /// Entries in fresh snapshots successfully published since this handle was created.
    ///
    /// A publication at a failed pump exit contributes; a poisoned publication and a quiet `Arc`
    /// reuse do not.
    #[must_use]
    pub fn copied_entries(&self) -> usize {
        self.copied.load(Ordering::Relaxed)
    }

    fn record(&self, copied: usize) {
        self.copied.fetch_add(copied, Ordering::Relaxed);
    }
}

/// The [`MintedTokens`] port, over a snapshot the decide loop publishes.
///
/// One value, cloned: the loop holds a clone to publish through and the classifier holds one to
/// read through, and both share the cell. A reader takes the lock only long enough to clone an
/// `Arc`, so it cannot block the writer behind a map lookup and the writer cannot block it behind
/// a fold.
///
/// # What a reader may be behind by, and which direction that falls
///
/// The loop publishes completed dirty folds before a folded launch crosses `Effect::Spawn`, and
/// publishes again at the **end of every `pump`**. A successful exit follows queue quiescence and
/// publishes the complete folded answer. A failed exit follows the first effect-execution error
/// and publishes only folds completed before it; it runs no later effect and drains no queued
/// input. A completed fold publishes a fresh owned snapshot; a pump that crossed no
/// fold retains the same immutable `Arc`. The two staleness windows are not symmetric:
///
/// - **Behind a `HookTokenMinted`** — a valid token presents before its minting is published.
///   `owner_of` answers `None`, the call is an unknown token, and the peer is refused `-32004`. An
///   availability cost.
/// - **Behind an `AgentRetired`, or behind the end of a retired agent's last recorded child** — a
///   retired agent's token is still honoured from inside its own tree. This is the direction
///   with a security consequence, and that is why it admits nothing: every effect that
///   identity reaches is decided on the loop thread from its current fold, not from this
///   snapshot. The
///   publication point is still tied to an *answered call*:
///   `Handle::ask` blocks on a reply the loop sends only after `pump` has returned. A successful
///   pump publishes its complete fold; a failed pump publishes every fold completed before its
///   error. Therefore a completed retirement fold is visible before either answer carries it back,
///   and `tests/retire_demotes.rs` plus `tests/token_publication.rs` hold the two exits.
///
/// **A poisoned lock reads as absent.** The writer is the loop thread and the guard is held for
/// one assignment, so poisoning means that thread panicked — a dying daemon. `None` is then an unknown
/// token and `-32004`: fail-closed, and the direction invariant 8 requires, since an identity
/// the daemon cannot prove is refused.
#[derive(Clone, Debug, Default)]
pub struct WorldTokens {
    published: Arc<RwLock<Arc<TokenOwners>>>,
    initialized: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
    work: TokenPublicationWork,
}

impl WorldTokens {
    /// An empty projection: every presented token is an unknown token until the loop publishes.
    ///
    /// Empty rather than optional, and that is the fail-closed start: a daemon that has not yet
    /// folded its journal honours no token, where an "unknown" state would need a rule of its own.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the snapshot unconditionally.
    ///
    /// A poisoned lock is dropped rather than recovered: the only production thread that writes
    /// here is the loop, and a snapshot nobody can read refuses every token, which is the safe
    /// direction. The loop's once-per-pump conditional rebuild is
    /// [`Self::publish_completed_folds`].
    pub fn publish(&self, owners: TokenOwners) {
        if let Ok(mut slot) = self.published.write() {
            let copied = owners.len();
            *slot = Arc::new(owners);
            self.initialized.store(true, Ordering::Release);
            self.dirty.store(false, Ordering::Release);
            self.work.record(copied);
        }
    }

    /// Remember that the authoritative fold moved until a fresh snapshot is published.
    ///
    /// Called immediately after the machine folds an input and before any effect from that input
    /// can fail. The bit belongs to the shared publication state rather than one `pump`, so an
    /// answered pump error cannot lose the obligation.
    pub(crate) fn mark_folded(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    /// Publish completed folds, rebuilding only after the fold moved.
    ///
    /// A caller may reach this immediately before a launch or at pump exit. In both places it
    /// supplies exactly the folds already completed; it claims neither whole-call completeness nor
    /// that every durable record has folded.
    ///
    /// The write lock is still taken exactly once for every call. When no fold remains unpublished
    /// and a snapshot has already been initialized, assigning a clone of the same immutable `Arc`
    /// keeps the once-per-pump publication and lock cadence without copying its entries.
    /// The closure runs before the lock is taken, preserving the rule that a reader never waits
    /// behind the fold's owned-map construction.
    pub(crate) fn publish_completed_folds(&self, owners: impl FnOnce() -> TokenOwners) {
        let fresh = self.dirty.load(Ordering::Acquire) || !self.initialized.load(Ordering::Acquire);
        let next = fresh.then(|| {
            let owners = owners();
            let copied = owners.len();
            (Arc::new(owners), copied)
        });

        if let Ok(mut slot) = self.published.write() {
            if let Some((next, copied)) = next {
                *slot = next;
                self.initialized.store(true, Ordering::Release);
                self.dirty.store(false, Ordering::Release);
                self.work.record(copied);
            } else {
                let same = Arc::clone(&*slot);
                *slot = same;
            }
        }
    }

    pub(crate) fn work(&self) -> TokenPublicationWork {
        self.work.clone()
    }
}

impl MintedTokens for WorldTokens {
    /// The agent this token was minted for, and the row it reaches, per the published snapshot.
    ///
    /// **The hashing is this side's**, because the plaintext is never journaled and
    /// `dudes-supervisor` answers about a hash. It is `dudes_supervisor::blake3_hex` — the very
    /// function the mint hashes with — rather than a second call to the same algorithm, so the two
    /// cannot drift into disagreeing about an encoding.
    ///
    /// Hashed before the lock is taken, so the critical section is one `Arc` clone.
    fn owner_of(&self, token: &HookToken) -> Option<TokenOwner> {
        let hash = blake3_hex(token.expose().as_bytes());
        let snapshot = Arc::clone(&*self.published.read().ok()?);
        snapshot.get(&hash).map(|owner| match *owner {
            MintedFor::Live(agent) => TokenOwner::Live(agent),
            MintedFor::RetiredWithChild(agent) => TokenOwner::RetiredWithChild(agent),
        })
    }
}

#[cfg(test)]
pub(crate) mod native_observation_test_support {
    use std::cell::RefCell;
    use std::rc::Rc;

    use dudes_supervisor::host::{NativeGroupPresence, ProbeAnswer};

    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Leader {
            pid: u32,
            start_time: u64,
            boot: Option<String>,
        },
        Group(u32),
        Namespace,
    }

    #[derive(Debug)]
    struct State {
        calls: Vec<Call>,
        leader: ProbeAnswer,
        group: NativeGroupPresence,
        namespace: Option<String>,
    }

    #[derive(Clone, Debug)]
    pub(crate) struct Sentinel(Rc<RefCell<State>>);

    impl Sentinel {
        pub(crate) fn new() -> Self {
            Self(Rc::new(RefCell::new(State {
                calls: Vec::new(),
                leader: ProbeAnswer::CouldNotAnswer,
                group: NativeGroupPresence::Unknown,
                namespace: None,
            })))
        }
    }

    impl ProcessHost for Sentinel {
        fn spawn(&mut self, _spec: &LaunchSpec, _install: &ManagedInstall) -> SpawnResult {
            panic!("read-only observation test must not spawn")
        }

        fn kill(&mut self, _key: &AdoptionKey) -> KillOutcome {
            panic!("read-only observation test must not kill")
        }

        fn alive(&self, _key: &AdoptionKey) -> Liveness {
            panic!("native observation must not substitute ordinary liveness")
        }

        fn boot_identity(&self) -> HostBoot {
            panic!("observation wrapper must forward the requested method")
        }

        fn shutdown(&mut self) -> io::Result<()> {
            panic!("read-only observation test must not shut down")
        }

        fn probe_native_leader(&self, key: &AdoptionKey) -> ProbeAnswer {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::Leader {
                pid: key.pid,
                start_time: key.start_time,
                boot: key.boot.clone(),
            });
            state.leader
        }

        fn native_group_presence(&self, pid: u32) -> NativeGroupPresence {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::Group(pid));
            state.group
        }

        fn pid_namespace(&self) -> Option<String> {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::Namespace);
            state.namespace.clone()
        }
    }

    pub(crate) fn assert_forwarded(host: &impl ProcessHost, sentinel: &Sentinel) {
        for (index, (leader, group, namespace)) in [
            (
                ProbeAnswer::StartValue(991),
                NativeGroupPresence::Present,
                Some("pid:[7]"),
            ),
            (
                ProbeAnswer::NoSuchPid,
                NativeGroupPresence::Absent,
                Some("pid:[11]"),
            ),
            (
                ProbeAnswer::CouldNotAnswer,
                NativeGroupPresence::Unsignallable,
                None,
            ),
            (
                ProbeAnswer::StartValue(7),
                NativeGroupPresence::Unknown,
                Some("pid:[13]"),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let offset = u32::try_from(index).unwrap();
            let key = AdoptionKey {
                pid: 42 + offset,
                start_time: 1_000 + u64::from(offset),
                boot: (index % 2 == 0).then(|| format!("recorded-boot-{index}")),
            };
            let group_pid = 142 + offset;
            {
                let mut state = sentinel.0.borrow_mut();
                state.calls.clear();
                state.leader = leader;
                state.group = group;
                state.namespace = namespace.map(str::to_owned);
            }
            let observed = (
                host.probe_native_leader(&key),
                host.native_group_presence(group_pid),
                host.pid_namespace(),
            );
            assert_eq!(observed, (leader, group, namespace.map(str::to_owned)));
            assert_eq!(
                sentinel.0.borrow().calls,
                [
                    Call::Leader {
                        pid: key.pid,
                        start_time: key.start_time,
                        boot: key.boot
                    },
                    Call::Group(group_pid),
                    Call::Namespace
                ]
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use dudes_rpc::auth::{PeerIdentity, PeerPid, PeerProvenance, PeerResolution, PeerResolver};

    use super::{GuardedResolver, NoWalk};
    use crate::ports::Handle;

    #[test]
    fn peer_barrier_delegates_native_observations() {
        use super::native_observation_test_support::{Sentinel, assert_forwarded};
        let sentinel = Sentinel::new();
        let host =
            super::PeerBarrier::new(sentinel.clone(), super::Resolver::fixed(Arc::new(NoWalk)));
        assert_forwarded(&host, &sentinel);
    }

    /// A resolver that answers a constant, including a refusal.
    #[derive(Debug)]
    struct Constant(PeerResolution);

    impl PeerResolver for Constant {
        fn resolve(&self, _peer: &PeerIdentity) -> PeerResolution {
            self.0
        }
    }

    /// A resolver that cannot answer, in the way a process-table read fails: by panicking.
    #[derive(Debug)]
    struct Panicking;

    impl PeerResolver for Panicking {
        fn resolve(&self, _peer: &PeerIdentity) -> PeerResolution {
            panic!("this resolver cannot read what it was asked to read");
        }
    }

    /// A handle onto a channel nothing reads. `Handle::stop` drops silently on a closed channel,
    /// which is the state a guard that stops a daemon nobody started would find anyway.
    fn detached_handle() -> Handle {
        let (jobs, receiver) = std::sync::mpsc::channel();
        // Kept alive so a send inside the guard succeeds rather than being dropped for the wrong
        // reason: the arm below asserts how the guard *leaves*, not what its stop did.
        std::mem::forget(receiver);
        Handle::new(jobs)
    }

    /// The guard's caught-panic path diverges rather than answering
    /// [`PeerResolution::Refuse`], while a refusal the inner resolver itself produced comes back
    /// out of the guard unchanged.
    ///
    /// The two arms are one test because the second is the first's positive control *and* the
    /// precondition's own check. A fixture whose guard answered nothing at all would pass
    /// the divergence arm and fail the pass-through arm; a guard that turned a caught panic into
    /// `Refuse` — a closed producer list says it must not — fails the first while
    /// the second stays green.
    #[test]
    fn a_caught_panic_diverges_rather_than_answering_a_refusal() {
        let peer = PeerIdentity::unknown();

        // Control first: an inner `Refuse` is passed through, so the guard is on the path and
        // returning normally for something.
        let passed_through = GuardedResolver::wrap(
            Arc::new(Constant(PeerResolution::Refuse)),
            detached_handle(),
        );
        let control = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            passed_through.resolve(&peer)
        }));
        assert_eq!(
            control.expect("control: a non-panicking inner resolver returns normally"),
            PeerResolution::Refuse,
            "control: the guard passes an inner refusal through unchanged"
        );

        // Subject: a panicking inner resolver leaves by diverging, not by answering.
        let broken = GuardedResolver::wrap(Arc::new(Panicking), detached_handle());
        let hushed = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let answer =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| broken.resolve(&peer)));
        std::panic::set_hook(hushed);
        assert!(
            answer.is_err(),
            "the guard answered {:?} for a caught panic; the producer list is closed \
             and a caught panic is not on it",
            answer.ok()
        );
    }

    /// Every peer resolves to `Unresolvable`, and specifically not to `NoRecordedAgent`, which
    /// would falsely claim that an ancestry walk completed.
    ///
    /// Asserted over peers that differ in every field, so "it answers a constant" is the claim and
    /// not an accident of one input.
    #[test]
    fn every_peer_is_unresolvable() {
        let peers = [
            dudes_rpc::auth::PeerIdentity::unknown(),
            dudes_rpc::auth::PeerIdentity {
                uid: Some(501),
                gid: Some(20),
                pid: PeerPid::Reported {
                    pid: 4242,
                    pidversion: Some(7),
                },
            },
            dudes_rpc::auth::PeerIdentity {
                uid: Some(0),
                gid: Some(0),
                pid: PeerPid::Unreadable,
            },
        ];
        for peer in peers {
            let answer = NoWalk.resolve(&peer);
            assert_eq!(
                answer,
                PeerResolution::Resolved(PeerProvenance::Unresolvable),
                "for {peer:?}"
            );
            assert_ne!(
                answer,
                PeerResolution::Resolved(PeerProvenance::NoRecordedAgent),
                "a walk that never ran must not be journaled as one that found nothing"
            );
            assert_ne!(
                answer,
                PeerResolution::Refuse,
                "not walking is a completed resolution, and the producer list does \
                 not include it"
            );
        }
    }

    // ----: the daemon's half of the refusal -----

    use std::sync::{Barrier, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use dudes_supervisor::host::AdoptionKey;
    use dudes_types::AgentId;

    use super::{
        ChildOwners, PENDING_RESOLVE_CEILING, ProcessRelation, ProcessResolver, Relation,
        WorldChildren,
    };

    /// How long a test waits for the resolver's answer before calling the loop dead.
    ///
    /// **A liveness guard, not a measurement.** The property is the *lower* bound on `elapsed`,
    /// asserted inside the worker. This exists only so a barrier loop that never returns fails on
    /// a nameable assertion instead of hanging the harness. `10 ×` the ceiling, floored at 30 s —
    /// which for a 3 s ceiling is the floor.
    const LIVENESS: Duration = {
        let ten = Duration::from_secs(PENDING_RESOLVE_CEILING.as_secs() * 10);
        if ten.as_secs() < 30 {
            Duration::from_secs(30)
        } else {
            ten
        }
    };

    const AGENT: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn agent() -> AgentId {
        AgentId::parse(AGENT).expect("a parseable agent id")
    }

    fn key(pid: u32) -> AdoptionKey {
        AdoptionKey {
            pid,
            start_time: 4242,
            boot: None,
        }
    }

    fn reported(pid: i32) -> PeerIdentity {
        PeerIdentity {
            uid: Some(501),
            gid: Some(20),
            pid: PeerPid::Reported {
                pid,
                pidversion: None,
            },
        }
    }

    /// A relation that answers one constant, with `finish_spawn`'s answer chosen by the fixture.
    ///
    /// **`finish_spawn` answering `false` is the point of the timing-out fixture.** The `TimedOut`
    /// arm leaves the barrier loop only `if self.processes.finish_spawn(slot, None)`; a fake
    /// answering `true` breaks out on the *first* timeout and never reaches the ceiling, so the
    /// arm would pass against a resolver with no ceiling at all. A fake answering `false` re-enters
    /// the loop, and `PENDING_RESOLVE_CEILING` is then the only thing that can end the call.
    #[derive(Debug)]
    struct PendingRelation {
        answer: Relation,
        finish: bool,
    }

    impl ProcessRelation for PendingRelation {
        fn begin_spawn(&self) -> u64 {
            1
        }

        fn finish_spawn(&self, _slot: u64, _key: Option<&AdoptionKey>) -> bool {
            self.finish
        }

        fn published(&self, _children: &[AdoptionKey]) -> Vec<(u64, usize)> {
            Vec::new()
        }

        fn resolve(
            &self,
            _peer: u32,
            _pidversion: Option<u32>,
            _children: &[AdoptionKey],
        ) -> Relation {
            self.answer
        }
    }

    #[derive(Debug)]
    struct InterleavingRelation {
        snapshot_captured: Arc<Barrier>,
        publication_complete: Arc<Barrier>,
        retained: bool,
    }

    impl ProcessRelation for InterleavingRelation {
        fn begin_spawn(&self) -> u64 {
            7
        }

        fn finish_spawn(&self, _slot: u64, _key: Option<&AdoptionKey>) -> bool {
            false
        }

        fn published(&self, _children: &[AdoptionKey]) -> Vec<(u64, usize)> {
            Vec::new()
        }

        fn resolve(
            &self,
            _peer: u32,
            _pidversion: Option<u32>,
            children: &[AdoptionKey],
        ) -> Relation {
            assert!(
                children.is_empty(),
                "the connection must hold the owner snapshot from before publication"
            );
            self.snapshot_captured.wait();
            self.publication_complete.wait();
            if self.retained {
                Relation::Pending(7)
            } else {
                Relation::NoMatch
            }
        }
    }

    /// Build the resolver directly rather than through `Resolver::processes`, so the test can put a
    /// published snapshot and an armed slot in place before anyone resolves.
    fn resolver_over(relation: PendingRelation, owners: &WorldChildren) -> ProcessResolver {
        ProcessResolver {
            processes: Arc::new(relation),
            owners: owners.clone(),
        }
    }

    fn publish(owners: &WorldChildren, rows: &[(AgentId, AdoptionKey)]) {
        let snapshot = ChildOwners {
            agents: rows.iter().map(|(agent, _)| *agent).collect(),
            keys: rows.iter().map(|(_, key)| key.clone()).collect(),
        };
        *owners.published.write().expect("the snapshot cell") = Arc::new(snapshot);
    }

    fn resolve_across_publication(retained: bool) -> PeerResolution {
        let owners = WorldChildren::default();
        owners.arm(7);
        let snapshot_captured = Arc::new(Barrier::new(2));
        let publication_complete = Arc::new(Barrier::new(2));
        let resolver = ProcessResolver {
            processes: Arc::new(InterleavingRelation {
                snapshot_captured: Arc::clone(&snapshot_captured),
                publication_complete: Arc::clone(&publication_complete),
                retained,
            }),
            owners: owners.clone(),
        };
        let worker = thread::spawn(move || resolver.resolve(&reported(42)));

        snapshot_captured.wait();
        publish(&owners, &[(agent(), key(42))]);
        owners.map(7, agent(), &key(42));
        owners.map(7, agent(), &key(42));
        publication_complete.wait();

        worker.join().expect("the interleaving resolver")
    }

    #[test]
    fn a_stale_owner_snapshot_uses_the_retained_pending_slot() {
        assert_eq!(
            resolve_across_publication(true),
            PeerResolution::Resolved(PeerProvenance::Agent(agent())),
            "a connection holding the old owner Arc waits for the exact mapped slot"
        );
        assert_eq!(
            resolve_across_publication(false),
            PeerResolution::Resolved(PeerProvenance::NoRecordedAgent),
            "control: consuming the slot at publication recreates the admitting stale-snapshot result"
        );
    }

    /// Resolve on a detached thread and return the answer with how long the call took.
    ///
    /// **Detached, and never joined.** `JoinHandle::join` carries no deadline and a scope's
    /// implicit join blocks *after* an assertion panics, so either spelling turns a mutant that
    /// loops forever into a harness timeout — the failure mode these arms exist to replace with a
    /// named assertion.
    fn answer_within(
        resolver: ProcessResolver,
        peer: PeerIdentity,
        bound: Duration,
    ) -> Option<(PeerResolution, Duration)> {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let started = Instant::now();
            let answer = resolver.resolve(&peer);
            let _ = sender.send((answer, started.elapsed()));
        });
        receiver.recv_timeout(bound).ok()
    }

    /// A pending slot whose agent mapping never arrives refuses the connection, and
    /// does not refuse before the ceiling.
    ///
    /// **This test is what pins the `Refuse` cases**, and the value assertion alone cannot carry it: a
    /// ceiling that fired and a barrier that answered early both produce `Refuse`. The control
    /// runs first — the sibling fixture differing only in `finish_spawn`'s answer, against a
    /// published mapping, which must answer `Agent(_)` promptly. Without it a resolver that
    /// refused everything would pass the arm below perfectly.
    #[test]
    fn a_pending_resolution_that_times_out_refuses_the_connection() {
        // Control first: same shape, mapping published, and it answers an agent rather than a
        // refusal — so the barrier is reachable and a `Refuse` below is the ceiling, not the shape.
        let mapped = WorldChildren::default();
        publish(&mapped, &[(agent(), key(11))]);
        mapped.arm(7);
        mapped.map(7, agent(), &key(11));
        let control = resolver_over(
            PendingRelation {
                answer: Relation::Pending(7),
                finish: true,
            },
            &mapped,
        );
        let (control_answer, control_elapsed) =
            answer_within(control, reported(11), LIVENESS).expect("control: the barrier answers");
        assert_eq!(
            control_answer,
            PeerResolution::Resolved(PeerProvenance::Agent(agent())),
            "control: a published mapping resolves the pending slot to its agent"
        );
        assert!(
            control_elapsed < PENDING_RESOLVE_CEILING,
            "control: the mapped barrier took {control_elapsed:?}, which is the ceiling it must \
             never reach"
        );

        // Subject: armed, never mapped, and `finish_spawn` refuses to release the slot.
        let waiting = WorldChildren::default();
        publish(&waiting, &[(agent(), key(11))]);
        waiting.arm(7);
        let subject = resolver_over(
            PendingRelation {
                answer: Relation::Pending(7),
                finish: false,
            },
            &waiting,
        );
        let (answer, elapsed) = answer_within(subject, reported(11), LIVENESS).expect(
            "the barrier never answered inside the liveness guard: PENDING_RESOLVE_CEILING does \
             not bound the loop from entry",
        );
        assert_eq!(
            answer,
            PeerResolution::Refuse,
            "a pending slot whose mapping never arrives must refuse, never borrow a column"
        );
        assert!(
            elapsed >= PENDING_RESOLVE_CEILING,
            "refused after {elapsed:?}, short of {PENDING_RESOLVE_CEILING:?}: the ceiling is not \
             what ended the call"
        );
    }

    /// `Relation::Abandoned` refuses, and the same fixture resolving a match does not.
    ///
    /// The mutant this exists for **compiles**: `Relation::Abandoned => Resolved(Unresolvable)`.
    /// The (operator credential × `Unresolvable`) cell then admits that connection as `Operator`, which is the
    /// exact admission the `Refuse` variant exists to remove. *Deleting* the arm is compiler-caught;
    /// mis-pointing it is caught by nothing else in either tree.
    ///
    /// **The published snapshot is part of the control.** With no owner published, `Matched(0)`
    /// answers `Resolved(Unresolvable)` — still not `Refuse`, but proving only that the arm is not
    /// the abandoned one. With the owner published it answers `Agent(_)`, which proves the whole
    /// mapping ran.
    #[test]
    fn an_abandoned_relation_refuses_and_never_resolves() {
        let owners = WorldChildren::default();
        publish(&owners, &[(agent(), key(11))]);

        // Control first.
        let matched = resolver_over(
            PendingRelation {
                answer: Relation::Matched(0),
                finish: true,
            },
            &owners,
        );
        assert_eq!(
            matched.resolve(&reported(11)),
            PeerResolution::Resolved(PeerProvenance::Agent(agent())),
            "control: a match against a published owner resolves to that agent"
        );

        // Subject: one answer changed.
        let abandoned = resolver_over(
            PendingRelation {
                answer: Relation::Abandoned,
                finish: true,
            },
            &owners,
        );
        let answer = abandoned.resolve(&reported(11));
        assert_eq!(
            answer,
            PeerResolution::Refuse,
            "an abandoned walk must refuse the connection"
        );
        assert_ne!(
            answer,
            PeerResolution::Resolved(PeerProvenance::Unresolvable),
            "and specifically not the column row 1 admits an operator credential in"
        );
        assert_ne!(
            answer,
            PeerResolution::Resolved(PeerProvenance::NoRecordedAgent),
            "nor the other column row 1 admits"
        );
    }
}
