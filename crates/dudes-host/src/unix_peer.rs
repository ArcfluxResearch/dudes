//! Linux and macOS ownership publication around the platform-neutral ancestry walk.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dudes_supervisor::host::{AdoptionKey, ProbeAnswer};

use crate::peer;
use crate::plat;

/// The result of comparing a peer with the recorded child keys supplied by the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerRelation {
    /// The key at this index is the first full ancestry match.
    Matched(usize),
    /// The peer belongs to a just-spawned child whose agent mapping is not in `World` yet.
    Pending(u64),
    /// The walk completed and no recorded child matched.
    NoMatch,
    /// A required platform read could not produce evidence.
    Unresolvable,
    /// A pending ownership relation did not complete within the bounded wait.
    Abandoned,
}

/// The Unix process relation owned by a [`crate::RealHost`].
#[derive(Clone, Debug)]
pub struct PeerProcesses {
    shared: Arc<PeerShared>,
    #[cfg(target_os = "linux")]
    boot: Option<String>,
}

#[derive(Debug, Default)]
struct PeerShared {
    state: Mutex<PeerState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct PeerState {
    next: u64,
    queued: VecDeque<u64>,
    active: BTreeSet<u64>,
    children: BTreeMap<u32, TrackedChild>,
    released: BTreeSet<u64>,
}

#[derive(Debug)]
struct TrackedChild {
    slot: Option<u64>,
    key: Option<AdoptionKey>,
}

/// A child another owner creates, tracked in this relation the way [`crate::RealHost`] tracks its
/// own. Dropping it releases the child's registration, or the slot when no child was
/// registered.
#[derive(Debug)]
pub struct ExternalChild {
    processes: PeerProcesses,
    slot: u64,
    pid: Option<u32>,
}

impl ExternalChild {
    /// Register the child the owner created, and read its full key while the owner holds it
    /// unreaped.
    ///
    /// # Errors
    ///
    /// The key could not be read. The child stays registered without a key, so a peer of it keeps
    /// waiting rather than resolving to no match, until this value is dropped.
    pub fn register(&mut self, pid: u32) -> io::Result<AdoptionKey> {
        self.processes.register(pid, Some(self.slot));
        self.pid = Some(pid);
        let key = self.processes.read_key(pid)?;
        self.processes.keyed(pid, &key);
        Ok(key)
    }
}

impl Drop for ExternalChild {
    fn drop(&mut self) {
        match self.pid {
            Some(pid) => self.processes.exited(pid, Some(self.slot)),
            None => self.processes.pre_exec_failed(Some(self.slot)),
        }
    }
}

enum Pending {
    None,
    Waiting,
    Keyed(Vec<(u64, AdoptionKey)>),
}

#[derive(Clone, Copy)]
struct Peer {
    pid: u32,
    #[cfg(target_os = "macos")]
    pidversion: u32,
}

const PENDING_KEY_BOUND: Duration = Duration::from_secs(1);
const PENDING_ABANDON_BOUND: Duration = Duration::from_secs(3);

impl PeerProcesses {
    pub(crate) fn new(boot: Option<&str>) -> Self {
        #[cfg(target_os = "macos")]
        let _ = boot;
        Self {
            shared: Arc::new(PeerShared::default()),
            #[cfg(target_os = "linux")]
            boot: boot.map(str::to_owned),
        }
    }

    /// Every child this relation tracks now, by pid. A child is here from the moment it is
    /// registered, before any record names its agent.
    #[must_use]
    pub fn live_pids(&self) -> Vec<u32> {
        self.state().children.keys().copied().collect()
    }

    pub(crate) fn registered(&self) -> usize {
        self.state().children.len()
    }

    /// Allocate the opaque slot the daemon arms before calling `spawn`.
    pub fn begin_spawn(&self) -> u64 {
        let mut state = self.state();
        state.next = state.next.wrapping_add(1).max(1);
        let slot = state.next;
        state.queued.push_back(slot);
        slot
    }

    /// Activate `slot`, from [`Self::begin_spawn`], for a child another owner is about to create.
    ///
    /// From this call until the returned value registers the child, a peer that connects waits for
    /// it instead of resolving to no match, as it does for a child [`crate::RealHost`] spawns.
    pub fn track_external(&self, slot: u64) -> ExternalChild {
        let mut state = self.state();
        state.queued.retain(|queued| *queued != slot);
        state.active.insert(slot);
        self.shared.changed.notify_all();
        drop(state);
        ExternalChild {
            processes: self.clone(),
            slot,
            pid: None,
        }
    }

    pub(crate) fn activate(&self) -> Option<u64> {
        let mut state = self.state();
        let slot = state.queued.pop_front();
        if let Some(slot) = slot {
            state.active.insert(slot);
            self.shared.changed.notify_all();
        }
        slot
    }

    pub(crate) fn pre_exec_failed(&self, slot: Option<u64>) {
        let Some(slot) = slot else {
            return;
        };
        let mut state = self.state();
        state.active.remove(&slot);
        state.released.insert(slot);
        self.shared.changed.notify_all();
    }

    pub(crate) fn register(&self, pid: u32, slot: Option<u64>) {
        let Some(slot) = slot else {
            return;
        };
        let mut state = self.state();
        state.active.remove(&slot);
        state.children.insert(
            pid,
            TrackedChild {
                slot: Some(slot),
                key: None,
            },
        );
        self.shared.changed.notify_all();
    }

    pub(crate) fn keyed(&self, pid: u32, key: &AdoptionKey) {
        let mut state = self.state();
        if let Some(child) = state.children.get_mut(&pid) {
            child.key = Some(key.clone());
            self.shared.changed.notify_all();
        }
    }

    pub(crate) fn release(&self, pid: u32) {
        let mut state = self.state();
        if let Some(child) = state.children.remove(&pid)
            && let Some(slot) = child.slot
        {
            state.released.insert(slot);
        }
        self.shared.changed.notify_all();
    }

    /// Release a child its watcher reaped, as [`Self::release`] does after a kill.
    ///
    /// Matched on `slot` as well as `pid`: the reap freed the identifier, so a later spawn may
    /// already be tracked under it.
    pub(crate) fn exited(&self, pid: u32, slot: Option<u64>) {
        let Some(slot) = slot else {
            return;
        };
        let mut state = self.state();
        if state
            .children
            .get(&pid)
            .is_some_and(|child| child.slot == Some(slot))
        {
            state.children.remove(&pid);
            state.released.insert(slot);
            self.shared.changed.notify_all();
        }
    }

    /// The full key of a child another owner holds unreaped, as [`crate::RealHost`] reads its own.
    #[cfg_attr(
        target_os = "macos",
        allow(
            clippy::unused_self,
            reason = "Linux reads the boot snapshot from self; macOS has no boot identity"
        )
    )]
    fn read_key(&self, pid: u32) -> io::Result<AdoptionKey> {
        let grip = plat::grip_adopted(pid);
        #[cfg(target_os = "macos")]
        let answer = plat::unreaped_start_value(&grip, pid);
        #[cfg(target_os = "linux")]
        let answer = plat::probe(&grip, pid);
        let ProbeAnswer::StartValue(start_time) = answer else {
            return Err(io::Error::other(format!(
                "could not read the start value of the external child at pid {pid}"
            )));
        };
        #[cfg(target_os = "linux")]
        let boot = Some(self.boot.clone().ok_or_else(|| {
            io::Error::other("this host's boot identity is unreadable, so no key can be formed")
        })?);
        #[cfg(target_os = "macos")]
        let boot = None;
        Ok(AdoptionKey {
            pid,
            start_time,
            boot,
        })
    }

    /// Confirm one spawn's full key, or release a slot after confirmed cleanup.
    pub fn finish_spawn(&self, slot: u64, key: Option<&AdoptionKey>) -> bool {
        if let Some(key) = key {
            return self.state().children.values().any(|child| {
                child.slot == Some(slot)
                    && child
                        .key
                        .as_ref()
                        .is_some_and(|recorded| peer::same_key(recorded, key))
            });
        }

        let mut state = self.state();
        if let Some(index) = state.queued.iter().position(|queued| *queued == slot) {
            state.queued.remove(index);
            self.shared.changed.notify_all();
            return true;
        }
        if state.active.remove(&slot) || state.released.remove(&slot) {
            self.shared.changed.notify_all();
            return true;
        }
        false
    }

    /// Return pending slots whose full keys are now present in the published `World`.
    ///
    /// The slot remains attached until [`Self::release`] or [`Self::exited`] removes the tracked
    /// child. Repeated publication is deliberately idempotent so a connection holding an older
    /// owner snapshot still reaches the pending mapping instead of `NoMatch`.
    pub fn published(&self, children: &[AdoptionKey]) -> Vec<(u64, usize)> {
        let mut state = self.state();
        let mut completed = Vec::new();
        for child in state.children.values_mut() {
            let index = child.key.as_ref().and_then(|key| {
                children
                    .iter()
                    .position(|published| peer::same_key(key, published))
            });
            if let Some(index) = index
                && let Some(slot) = child.slot
            {
                completed.push((slot, index));
            }
        }
        if !completed.is_empty() {
            self.shared.changed.notify_all();
        }
        completed
    }

    pub(crate) fn clear(&self) {
        let mut state = self.state();
        let slots: Vec<_> = state
            .queued
            .iter()
            .copied()
            .chain(state.active.iter().copied())
            .chain(state.children.values().filter_map(|child| child.slot))
            .collect();
        state.released.extend(slots);
        state.queued.clear();
        state.active.clear();
        state.children.clear();
        self.shared.changed.notify_all();
    }

    /// Resolve a Linux peer against the recorded child keys.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn resolve(&self, peer: u32, children: &[AdoptionKey]) -> PeerRelation {
        self.resolve_peer(Peer { pid: peer }, children)
    }

    /// Resolve one listener-captured macOS execution against the recorded child keys.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn resolve_execution(
        &self,
        peer: u32,
        pidversion: u32,
        children: &[AdoptionKey],
    ) -> PeerRelation {
        self.resolve_peer(
            Peer {
                pid: peer,
                pidversion,
            },
            children,
        )
    }

    fn resolve_peer(&self, peer: Peer, children: &[AdoptionKey]) -> PeerRelation {
        match self.walk(peer, children) {
            peer::Resolution::Matched(index) => PeerRelation::Matched(index),
            peer::Resolution::NoMatch => self.resolve_pending(peer),
            peer::Resolution::Unresolvable => match self.pending() {
                Pending::None => PeerRelation::Unresolvable,
                Pending::Waiting | Pending::Keyed(_) => self.resolve_pending(peer),
            },
        }
    }

    fn resolve_pending(&self, peer: Peer) -> PeerRelation {
        let abandon_at = Instant::now() + PENDING_ABANDON_BOUND;
        let mut observed_pending = false;
        loop {
            match self.pending() {
                Pending::None => {
                    return if observed_pending {
                        PeerRelation::Abandoned
                    } else {
                        PeerRelation::NoMatch
                    };
                }
                Pending::Keyed(entries) => {
                    let keys: Vec<_> = entries.iter().map(|(_, key)| key.clone()).collect();
                    return match self.walk(peer, &keys) {
                        peer::Resolution::Matched(index) => PeerRelation::Pending(entries[index].0),
                        peer::Resolution::NoMatch => PeerRelation::NoMatch,
                        peer::Resolution::Unresolvable => PeerRelation::Unresolvable,
                    };
                }
                Pending::Waiting => {
                    observed_pending = true;
                    let now = Instant::now();
                    if now >= abandon_at {
                        return PeerRelation::Abandoned;
                    }
                    let guard = self.state();
                    let _ = self
                        .shared
                        .changed
                        .wait_timeout(
                            guard,
                            PENDING_KEY_BOUND.min(abandon_at.saturating_duration_since(now)),
                        )
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }

    fn pending(&self) -> Pending {
        let state = self.state();
        if !state.active.is_empty() {
            return Pending::Waiting;
        }
        let mut keyed = Vec::new();
        for child in state.children.values() {
            let Some(slot) = child.slot else {
                continue;
            };
            let Some(key) = child.key.clone() else {
                return Pending::Waiting;
            };
            keyed.push((slot, key));
        }
        if keyed.is_empty() {
            Pending::None
        } else {
            Pending::Keyed(keyed)
        }
    }

    #[cfg(target_os = "linux")]
    fn walk(&self, peer: Peer, children: &[AdoptionKey]) -> peer::Resolution {
        let mut source = plat::PeerSource::new(self.boot.clone());
        peer::resolve(&mut source, peer.pid, children)
    }

    #[cfg(target_os = "macos")]
    #[allow(
        clippy::unused_self,
        reason = "Linux reads the boot snapshot from self; macOS has no boot identity"
    )]
    fn walk(&self, peer: Peer, children: &[AdoptionKey]) -> peer::Resolution {
        let mut source = plat::PeerSource::new(peer.pid, peer.pidversion);
        peer::resolve(&mut source, peer.pid, children)
    }

    fn state(&self) -> MutexGuard<'_, PeerState> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(pid: u32) -> AdoptionKey {
        AdoptionKey {
            pid,
            start_time: 42,
            boot: cfg!(target_os = "linux").then(|| "boot".to_owned()),
        }
    }

    #[test]
    fn a_pre_exec_failure_releases_its_slot() {
        let processes = PeerProcesses::new(None);
        let slot = processes.begin_spawn();
        let active = processes.activate();
        assert_eq!(active, Some(slot));
        processes.pre_exec_failed(active);

        assert!(processes.finish_spawn(slot, None));
        assert!(matches!(processes.pending(), Pending::None));
    }

    #[test]
    fn a_keyed_slot_completes_only_when_the_world_publishes_that_key() {
        let processes = PeerProcesses::new(None);
        let slot = processes.begin_spawn();
        let active = processes.activate();
        processes.register(7, active);
        processes.keyed(7, &key(7));

        assert!(processes.finish_spawn(slot, Some(&key(7))));
        assert!(processes.published(&[key(8)]).is_empty());
        assert_eq!(processes.published(&[key(7)]), vec![(slot, 0)]);
        assert_eq!(
            processes.published(&[key(7)]),
            vec![(slot, 0)],
            "publication is idempotent while the tracked child remains live"
        );
        processes.release(7);
        assert!(processes.published(&[key(7)]).is_empty());
    }

    /// A relation that forms full keys on this host, so an external child can be keyed.
    fn keying() -> PeerProcesses {
        PeerProcesses::new(plat::boot_identity().expect("boot identity").as_deref())
    }

    #[test]
    fn an_external_child_is_pending_from_before_its_exec_until_it_is_released() {
        let processes = keying();
        let slot = processes.begin_spawn();
        let mut child = processes.track_external(slot);
        assert!(
            matches!(processes.pending(), Pending::Waiting),
            "a peer waits from before the exec"
        );

        let key = child
            .register(std::process::id())
            .expect("a live child's key");
        assert!(processes.finish_spawn(slot, Some(&key)));
        assert_eq!(
            processes.published(std::slice::from_ref(&key)),
            vec![(slot, 0)],
            "the barrier completes when the daemon publishes the key"
        );

        drop(child);
        assert!(processes.published(&[key]).is_empty());
        assert!(matches!(processes.pending(), Pending::None));
    }

    #[test]
    fn an_unreadable_external_key_keeps_its_peers_waiting_until_released() {
        let processes = keying();
        let slot = processes.begin_spawn();
        let mut child = processes.track_external(slot);

        assert!(child.register(u32::MAX - 1).is_err(), "no such process");
        assert!(matches!(processes.pending(), Pending::Waiting));
        drop(child);
        assert!(matches!(processes.pending(), Pending::None));
        assert!(
            processes.finish_spawn(slot, None),
            "the barrier can then be released"
        );
    }

    #[test]
    fn releasing_an_external_child_leaves_a_later_registration_of_its_pid() {
        let processes = keying();
        let slot = processes.begin_spawn();
        let mut child = processes.track_external(slot);
        let pid = std::process::id();
        child.register(pid).expect("a live child's key");

        let later = processes.begin_spawn();
        assert_eq!(processes.activate(), Some(later));
        processes.register(pid, Some(later));
        drop(child);
        assert_eq!(
            processes.registered(),
            1,
            "the pid now names the later spawn"
        );
    }

    #[test]
    fn a_reap_releases_only_the_spawn_that_held_its_pid() {
        let processes = PeerProcesses::new(None);
        let spawn_at_7 = || {
            let slot = processes.begin_spawn();
            processes.register(7, processes.activate());
            processes.keyed(7, &key(7));
            slot
        };
        let earlier = spawn_at_7();
        let later = spawn_at_7();

        processes.exited(7, Some(earlier));
        assert!(
            processes.finish_spawn(later, Some(&key(7))),
            "a late reap of the earlier child leaves the later spawn at that pid tracked"
        );

        processes.exited(7, Some(later));
        assert!(
            processes.finish_spawn(later, None),
            "the reaped spawn's slot is released, as after a kill"
        );
        assert!(matches!(processes.pending(), Pending::None));
    }

    #[test]
    fn attached_live_child_cannot_be_aborted_as_if_cleanup_succeeded() {
        let processes = PeerProcesses::new(None);
        let slot = processes.begin_spawn();
        let active = processes.activate();
        processes.register(7, active);

        assert!(!processes.finish_spawn(slot, None));
        assert!(matches!(processes.pending(), Pending::Waiting));
    }
}
