//! Windows: the creation `FILETIME` as the start value, a job object as the group, the named-pipe
//! disconnect leaf, and **the only `unsafe` in this crate**.
//!
//! # The grant
//!
//! This module carries `#[allow(unsafe_code)]` and nothing else in the crate does. The crate sets
//! `unsafe_code = "deny"` rather than inheriting the workspace's `forbid`, because `forbid` cannot
//! be re-levelled from source and so admits no module-scoped exemption at all. The grant is
//! therefore *this file*, on *this platform*, and its size is a test: `tests/unsafe_grant.rs`
//! asserts the token appears here and in no other file of the crate, and proves it can fail by
//! planting one in a clean sample.
//!
//! Why the start value needs it: nothing exposes a process's creation `FILETIME` for an arbitrary
//! identifier through a safe API. `windows`, `windows-sys` and `winapi` all declare
//! `GetProcessTimes` as `pub unsafe fn`, and the one safe alternative in the ecosystem truncates
//! the value to whole seconds — which is precisely the resolution the start value has to keep,
//! since two processes created inside the same second must not compare equal. DACL and filesystem
//! operations live in `dudes-win`; keeping them out of this module is what lets the CLI use them
//! without reaching this process-ownership surface.
//!
//! # An open handle is a reservation — contested by a second vendor page — and is *not* evidence
//! of life
//!
//! Three things matter here and they pull in different directions.
//!
//! A process identifier stays valid *"from the time the process is created until all handles to
//! the process are closed and the process object is freed; at this point, the identifier may be
//! reused"* — `PROCESS_INFORMATION`, member `dwProcessId`,
//! <https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/ns-processthreadsapi-process_information>,
//! fetched 2026-08-26. So a handle held across the comparison-then-signal window keeps the
//! identifier from moving underneath it, which is why [`grip_adopted`] opens one **before** the
//! caller compares the key.
//!
//! **Two vendor pages disagree and this module does not get to pretend otherwise**.
//! *Process Handles and Identifiers* says instead that *"The identifier is valid from the time the
//! process is created until the process has been terminated"*, which would mean no reservation at
//! all —
//! <https://learn.microsoft.com/en-us/windows/win32/procthread/process-handles-and-identifiers>,
//! revised 2025-07-14, fetched 2026-08-26. It is cited with its URL because it is the page that
//! *contradicts* the one this module took, and so the one a later reader most needs to open. The
//! member-level page above is the one taken, because it matches Raymond Chen's account of the
//! kernel object — *"the process object remains … as long as somebody still has a handle to the
//! process object … When all handles are closed … the process object can be destroyed, which in
//! turn destroys the process ID"*,
//! <https://devblogs.microsoft.com/oldnewthing/20110107-00/?p=11803>, fetched 2026-08-26. If the
//! narrower page is right, this platform loses the reservation and falls back to the
//! re-probe-immediately-before-signalling mechanism that macOS uses.
//!
//! **Nothing here depends on which is right, and the reason is this module's own [`signal`]:** it
//! terminates a *job object* where there is one and an *owned handle* otherwise, and neither call
//! names a process identifier. There is nothing in either for a recycled identifier to be
//! substituted into. So the reservation narrows the window between reading a key and acting on it;
//! it is never what makes the signal itself safe. (Not "the key comparison runs before every
//! signal either way" — that is true of the adoption path and false of the reserved one, which
//! probes nothing; `src/kill.rs` is where the two are told apart.)
//!
//! But that same retained object keeps a *terminated* process resolvable, and the vendor says why:
//! *"When a process terminates, its kernel object is not destroyed until all processes that have
//! open handles to the process have released those handles"* — `TerminateProcess`,
//! <https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-terminateprocess>,
//! revised 2025-12-30, fetched 2026-08-26. So a successful open says nothing about whether the
//! child is running: the same as everywhere else in the host, an existence probe answers
//! for something which is not the child.
//!
//! **Comparing the start value does not close that on its own**, and believing it did was a defect
//! this module shipped. The lingering object is the *same* process, so `GetProcessTimes` returns
//! the *same* creation stamp, the comparison matches, and
//! [`liveness`](dudes_supervisor::host::liveness) answers `Alive` for a process that has already
//! exited. Downstream that reached a kill: the guard admitted the signal, `TerminateProcess` was
//! called on a terminated process, and the same page documents the result — *"After a process has
//! terminated, calls to **`TerminateProcess`** with open handles to the process fails with
//! **`ERROR_ACCESS_DENIED`** (5) error code."* A kill of an already-reaped child reported a
//! platform failure instead of a refusal.
//!
//! What separates a terminated process from a live one is the process object being **signalled**
//! on termination, so [`probe`] asks that first, with a zero-millisecond wait, and answers
//! [`ProbeAnswer::CouldNotAnswer`] for anything the kernel says has already terminated. Not
//! `NoSuchPid`: that would be positive absence, which this platform does not claim — see below.
//! [`quiet`] has always asked the same question; it is one helper now, so the two cannot drift.
//!
//! # `NoSuchPid` is unreachable on this platform, on purpose
//!
//! No Microsoft page documents which error `OpenProcess` returns for an ordinary nonexistent
//! identifier — the Return value section of
//! <https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openprocess>
//! (fetched 2026-08-26) documents no error codes at all, and the two named on the page are
//! special-cased to system processes. That claim is **not verified**, and an error
//! code that cannot be attributed is not positive evidence of absence, so every `OpenProcess`
//! failure here becomes [`ProbeAnswer::CouldNotAnswer`] and hence `Unknown`.
//!
//! For the kill path that is sufficient and fails closed: `Unknown` is not `Alive`, so the kill
//! refuses and no signal is sent, and a refusal cannot reach a stranger. The dangerous case — an
//! identifier that has been *reused* — is untouched, because a successful open plus
//! `GetProcessTimes` yields a start value and the comparison decides it.
//!
//! What it costs is positive absence: a child that simply died and left its identifier free
//! answers `Unknown` rather than `Gone`, so its turn stays open and `system.doctor` lists it.
//! Recovering that needs a successful, complete process enumeration, which is separate work with
//! its own failure modes. `tests/unsafe_grant.rs` asserts `NoSuchPid` does not appear in this file,
//! so the narrowing is machine-checked rather than remembered — it reads the code lines only,
//! which is why this paragraph may name the variant while the module may not return it.
#![allow(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::mem;
use std::os::windows::io::{AsRawHandle as _, BorrowedHandle, OwnedHandle};
use std::process::{Child, Command};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use dudes_supervisor::host::{AdoptionKey, ProbeAnswer};
use win32job::{ExtendedLimitInfo, Job};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_FILES, ERROR_PIPE_NOT_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{IsProcessInJob, TerminateJobObject};
use windows_sys::Win32::System::Pipes::DisconnectNamedPipe;
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    TerminateProcess, WaitForSingleObject,
};

use crate::Spawned;
use crate::kill::{Boundary, Delivery, Sig, TargetState};
use crate::peer::{self, Containment, Process, Read, Source};

/// Disconnect one server-side named-pipe instance.
///
/// The transport lends the exact handle returned by `CreateNamedPipeW`; no duplicate is used.
/// The state change makes a later read fail immediately even when teardown lands before the first
/// read begins.
///
/// # Errors
///
/// Whatever `DisconnectNamedPipe` reports. An already disconnected instance is already in the
/// requested state and succeeds.
pub fn disconnect_named_pipe(handle: BorrowedHandle<'_>) -> io::Result<()> {
    // SAFETY: `handle` stays live for the call and the transport supplies a server-side named-pipe
    // handle created by `CreateNamedPipeW`; the API borrows it and retains no pointer.
    if unsafe { DisconnectNamedPipe(handle.as_raw_handle()) } != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_PIPE_NOT_CONNECTED.cast_signed()) {
        Ok(())
    } else {
        Err(error)
    }
}

/// The exit code a terminated child is given. Nonzero, because it did not choose to stop.
const TERMINATED: u32 = 1;

/// `SYNCHRONIZE`, the standard access right that lets an object be waited on.
///
/// Spelled out rather than imported: `windows-sys` publishes this constant only as a *file* access
/// right, under a feature this crate has no other use for, whereas the right itself is one of the
/// four standard rights shared by every kernel object type — including the process objects
/// [`ProcessHandle`] opens. Taking the file-system feature to borrow it would pull a surface far
/// larger than the constant.
const SYNCHRONIZE: u32 = 0x0010_0000;

/// A process handle this module owns and closes.
///
/// The type exists so that no code path can forget the close, and so that "the handle is what
/// reserves the identifier" is expressed by a value's lifetime rather than by a comment.
struct ProcessHandle(HANDLE);

impl ProcessHandle {
    /// Open `pid` for the three things this module does with a process: read its times, wait on
    /// it, and terminate it. `None` on any failure — see the module docs on why a failure is never
    /// read as absence.
    fn open(pid: u32) -> Option<Self> {
        Self::open_with(pid, ACCESS)
    }

    /// Open a peer or ancestor without taking the termination right the resolver never uses.
    fn open_query(pid: u32) -> Option<Self> {
        Self::open_with(pid, QUERY_ACCESS)
    }

    fn open_with(pid: u32, access: u32) -> Option<Self> {
        // SAFETY: `OpenProcess` takes an access mask, a `BOOL` and a `u32` by value and returns
        // either a handle or null; it dereferences nothing and there is no pointer to be valid.
        // The returned handle is owned by this value and closed exactly once, in `Drop`.
        let handle = unsafe { OpenProcess(access, 0, pid) };
        if handle.is_null() {
            None
        } else {
            Some(Self(handle))
        }
    }

    /// The raw handle, for the calls below. Borrowed for no longer than `self`.
    const fn raw(&self) -> HANDLE {
        self.0
    }
}

/// Read the times, wait on the object, terminate it. Nothing here needs `PROCESS_ALL_ACCESS`, and
/// asking for less is what lets the probe work against a process the daemon did not spawn.
const ACCESS: u32 = PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE | PROCESS_TERMINATE;
/// The subset the peer walk takes: it reads times and waits, and terminates nothing.
const QUERY_ACCESS: u32 = PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE;

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful `OpenProcess` in `open`, is non-null, has not
        // been closed (nothing else closes it and the type is not `Copy` or `Clone`), and this
        // runs once because `Drop::drop` runs once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// The job object, created before the child so that it can be assigned the moment the child
/// exists.
pub(crate) struct Prep(Job);

/// What the host holds for one identifier on Windows.
pub(crate) struct Grip {
    /// The job the child was assigned to, and every descendant it goes on to create. `None` for an
    /// adopted identifier: a job cannot be joined after the fact from outside.
    job: Option<Arc<Job>>,
    /// The handle that keeps the identifier from being reused while it is held. `None` only when
    /// the open failed, in which case the probe answers `CouldNotAnswer` and the kill refuses.
    process: Option<ProcessHandle>,
}

// SAFETY: a `Grip` holds a job object handle and a process handle. A Windows kernel handle is a
// process-wide value with no thread affinity — every call in this module (`TerminateJobObject`,
// `TerminateProcess`, `GetProcessTimes`, `IsProcessInJob`, `WaitForSingleObject`, `CloseHandle`) is
// documented to be
// callable from any thread — and the `HANDLE` inside `ProcessHandle` is only a raw pointer type by
// declaration, never dereferenced here. So moving a `Grip` between threads moves an integer the
// kernel owns, not a pointer into this process. `Sync` is deliberately **not** asserted: this type
// crosses a thread boundary only inside `oneshot::OneShot`'s `Mutex`, which needs `Send` alone, and
// asserting the wider bound would be asserting something no call site needs.
//
// The unix layers need no equivalent: their `Grip` is an `Option<rustix::process::Pid>`, which is
// `Send` on its own.
unsafe impl Send for Grip {}

impl Grip {
    pub(crate) fn job(&self) -> Option<&Arc<Job>> {
        self.job.as_ref()
    }
}

/// The result of comparing a peer with the recorded child keys supplied by the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerRelation {
    /// The key at this index is the first ancestry and job-object match.
    Matched(usize),
    /// The peer belongs to a just-spawned child whose agent mapping is not in `World` yet.
    Pending(u64),
    /// The walk completed and no recorded child satisfied both relations.
    NoMatch,
    /// A required platform read could not produce evidence.
    Unresolvable,
    /// The walk waited [`PENDING_ABANDON_BOUND`] from entry and never reached any of the four
    /// above.
    ///
    /// **Not a statement about the peer, and that is why it is a fifth variant rather than a
    /// fourth answer.** Every other variant says something the daemon can map onto a peer-classification
    /// column; this one says the loop gave up. The daemon turns it into a refusal, because the
    /// nearest column — `NoRecordedAgent` — is one that *admits* an operator credential, and
    /// borrowing it for a non-answer would open the exact door that refusing an unresolved connection exists to shut.
    Abandoned,
}

/// The Windows process relation owned by a [`crate::RealHost`].
///
/// The public interface exposes no raw handle and no process-table intermediate. The lifecycle
/// methods carry one opaque slot from pre-spawn arming to quiescent publication; `resolve` answers
/// the platform half of peer classification, and the daemon alone maps its index or pending slot to an
/// agent.
#[derive(Clone, Debug, Default)]
pub struct PeerProcesses {
    shared: Arc<PeerShared>,
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
    jobs: BTreeMap<u32, TrackedJob>,
    released: BTreeSet<u64>,
}

#[derive(Debug)]
struct TrackedJob {
    slot: Option<u64>,
    job: Option<JobHandle>,
    key: Option<AdoptionKey>,
}

/// The job a tracked child is in: one a [`crate::RealHost`] grip owns, which this relation does
/// not keep alive, or a duplicate of another owner's handle, which it holds until the child is
/// released.
#[derive(Debug)]
enum JobHandle {
    Owned(Weak<Job>),
    External(Arc<OwnedHandle>),
}

impl JobHandle {
    fn live(&self) -> Option<LiveJob> {
        match self {
            Self::Owned(job) => job.upgrade().map(LiveJob::Owned),
            Self::External(handle) => Some(LiveJob::External(Arc::clone(handle))),
        }
    }
}

/// A tracked job held open for the length of one query or termination.
enum LiveJob {
    Owned(Arc<Job>),
    External(Arc<OwnedHandle>),
}

impl LiveJob {
    /// The job handle, open for as long as `self` is borrowed.
    fn raw(&self) -> HANDLE {
        match self {
            Self::Owned(job) => job.handle() as HANDLE,
            Self::External(handle) => handle.as_raw_handle(),
        }
    }
}

/// A child another owner creates, tracked in this relation the way [`crate::RealHost`] tracks its
/// own. Dropping it releases the child's registration and its job, or the slot
/// when no child was registered.
#[derive(Debug)]
pub struct ExternalChild {
    processes: PeerProcesses,
    slot: u64,
    pid: Option<u32>,
}

impl ExternalChild {
    /// Register the child the owner created and the job that contains its tree, and read its
    /// full key while the owner holds it unreaped. `job` is this relation's own duplicate.
    ///
    /// # Errors
    ///
    /// The key could not be read. The child and its job stay registered without a key, so a peer
    /// of it keeps waiting rather than resolving to no match, until this value is dropped.
    pub fn register(&mut self, pid: u32, job: OwnedHandle) -> io::Result<AdoptionKey> {
        {
            let mut state = self.processes.state();
            state.active.remove(&self.slot);
            state.jobs.insert(
                pid,
                TrackedJob {
                    slot: Some(self.slot),
                    job: Some(JobHandle::External(Arc::new(job))),
                    key: None,
                },
            );
            self.processes.shared.changed.notify_all();
        }
        self.pid = Some(pid);
        let ProbeAnswer::StartValue(start_time) = probe(&grip_adopted(pid), pid) else {
            return Err(io::Error::other(format!(
                "could not read the start value of the external child at pid {pid}"
            )));
        };
        let key = AdoptionKey {
            pid,
            start_time,
            boot: None,
        };
        self.processes.keyed(pid, &key);
        Ok(key)
    }
}

impl Drop for ExternalChild {
    fn drop(&mut self) {
        match self.pid {
            Some(pid) => {
                let registered = self
                    .processes
                    .state()
                    .jobs
                    .get(&pid)
                    .is_some_and(|job| job.slot == Some(self.slot));
                if registered {
                    self.processes.release(pid);
                }
            }
            None => self.processes.pre_exec_failed(Some(self.slot)),
        }
    }
}

enum Pending {
    None,
    Active,
    Job {
        pid: u32,
        slot: u64,
        job: Option<LiveJob>,
        key: Option<AdoptionKey>,
    },
    Refused,
}

/// How long **one** wait inside [`PeerProcesses::resolve_pending`] blocks before the loop looks at
/// the state again.
const PENDING_KEY_BOUND: Duration = Duration::from_secs(1);

/// How long [`PeerProcesses::resolve_pending`] may run **from entry**, across every wait it takes,
/// before it answers [`PeerRelation::Abandoned`].
///
/// # Why this number
///
/// No external source gives a value, so here is the reasoning it is chosen by. The loop waits for exactly
/// two things, and both are the daemon's own spawn path rather than anything the peer controls:
/// a slot that has been armed but whose pid has not been registered yet (`CreateProcess` plus one
/// job assignment), and a registered pid whose adoption key has not been read yet (one
/// `GetProcessTimes` on an already-open handle). Both are sub-millisecond on an idle box and
/// bounded by scheduler latency on a loaded one — nothing in either is proportional to the size of
/// the process table or to how many agents exist.
///
/// Three whole [`PENDING_KEY_BOUND`] waits is therefore three orders of magnitude of headroom over
/// the work being waited for, and the value is picked at the low end deliberately: every second
/// spent here is a second a legitimate operator's connection is stalled, and the outcome of
/// waiting too long is not a better answer but a slower refusal. It matches
/// `duded`'s `PENDING_RESOLVE_CEILING` so that the `10 ×` liveness guard the tests use is the
/// same 30 s on both sides rather than two numbers a reader has to keep apart.
///
/// **It is a lower bound on the abandonment, not an upper bound on the call.** Neither this nor
/// the daemon's ceiling bounds the syscalls between two waits, so no test asserts a maximum
/// elapsed here; one would be measuring the platform.
pub(crate) const PENDING_ABANDON_BOUND: Duration = Duration::from_secs(3);

impl PeerProcesses {
    pub(crate) fn registered(&self) -> usize {
        self.state().jobs.len()
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

    pub(crate) fn register(&self, pid: u32, job: Option<&Arc<Job>>, slot: Option<u64>) {
        if slot.is_none() && job.is_none() {
            return;
        }
        let mut state = self.state();
        if let Some(slot) = slot {
            state.active.remove(&slot);
        }
        state.jobs.insert(
            pid,
            TrackedJob {
                slot,
                job: job.map(|job| JobHandle::Owned(Arc::downgrade(job))),
                key: None,
            },
        );
        self.shared.changed.notify_all();
    }

    pub(crate) fn keyed(&self, pid: u32, key: &AdoptionKey) {
        let mut state = self.state();
        if let Some(job) = state.jobs.get_mut(&pid) {
            job.key = Some(key.clone());
            self.shared.changed.notify_all();
        }
    }

    pub(crate) fn release(&self, pid: u32) {
        let mut state = self.state();
        if let Some(job) = state.jobs.remove(&pid)
            && let Some(slot) = job.slot
        {
            state.released.insert(slot);
        }
        self.shared.changed.notify_all();
    }

    /// Finish one daemon-side spawn step: confirm its full key, or abort it after cleanup.
    pub fn finish_spawn(&self, slot: u64, key: Option<&AdoptionKey>) -> bool {
        if let Some(key) = key {
            return self.state().jobs.values().any(|job| {
                job.slot == Some(slot)
                    && job
                        .key
                        .as_ref()
                        .is_some_and(|recorded| peer::same_key(recorded, key))
            });
        }

        let mut state = self.state();
        let queued = state.queued.iter().position(|queued| *queued == slot);
        if let Some(index) = queued {
            state.queued.remove(index);
            self.shared.changed.notify_all();
            return true;
        }
        if state.active.remove(&slot) || state.released.remove(&slot) {
            self.shared.changed.notify_all();
            return true;
        }
        let attached = state.jobs.iter().find_map(|(pid, job)| {
            (job.slot == Some(slot)).then(|| (*pid, job.job.as_ref().and_then(JobHandle::live)))
        });
        drop(state);
        let Some((pid, Some(job))) = attached else {
            return false;
        };
        if terminate_job(&job).is_err() {
            return false;
        }
        self.release(pid);
        true
    }

    /// Return pending slots whose full keys are now present in the daemon's published `World`.
    ///
    /// The slot remains attached until [`Self::release`] removes the tracked child. Repeated
    /// publication is deliberately idempotent so a connection holding an older owner snapshot
    /// still reaches the pending mapping instead of `NoMatch`.
    pub fn published(&self, children: &[AdoptionKey]) -> Vec<(u64, usize)> {
        let mut state = self.state();
        let mut completed = Vec::new();
        for job in state.jobs.values_mut() {
            let index = job.key.as_ref().and_then(|key| {
                children
                    .iter()
                    .position(|published| peer::same_key(key, published))
            });
            if let Some(index) = index
                && let Some(slot) = job.slot
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
            .chain(state.jobs.values().filter_map(|job| job.slot))
            .collect();
        state.released.extend(slots);
        state.queued.clear();
        state.active.clear();
        state.jobs.clear();
        self.shared.changed.notify_all();
    }

    /// Resolve `peer` against `children`, returning the first full-key and job-object match.
    ///
    /// Takes one system-wide process snapshot per call. The daemon calls this once per accepted
    /// connection, on that connection's own thread. A peer that walks cleanly to no
    /// match may then wait on a spawn in flight, bounded from entry by [`PENDING_ABANDON_BOUND`];
    /// exhausting that bound answers [`PeerRelation::Abandoned`] and nothing else. An assignment
    /// or termination the platform cannot confirm answers [`PeerRelation::Unresolvable`] rather
    /// than buying the `NoRecordedAgent` column.
    #[must_use]
    pub fn resolve(&self, peer: u32, children: &[AdoptionKey]) -> PeerRelation {
        let Ok(mut source) = WindowsSource::new(peer, Arc::clone(&self.shared)) else {
            return PeerRelation::Unresolvable;
        };
        match peer::resolve(&mut source, peer, children) {
            peer::Resolution::Matched(index) => PeerRelation::Matched(index),
            peer::Resolution::NoMatch => self.resolve_pending(&mut source, peer),
            peer::Resolution::Unresolvable => PeerRelation::Unresolvable,
        }
    }

    /// Wait for a spawn in flight to name itself, for at most [`PENDING_ABANDON_BOUND`] from
    /// **entry to this function**, not per wait.
    ///
    /// The distinction is the whole point: each individual wait is still
    /// [`PENDING_KEY_BOUND`], because a `Condvar` needs a timeout to re-read the state under; what
    /// changed is what happens when one of those waits expires against a slot this loop cannot act
    /// on — an unassigned process, a weakened hold, or a job whose termination the platform
    /// refused. That arm must not re-arm the per-wait deadline *unconditionally*, which is a loop
    /// with no exit: the connection's resolution never returned, and a value that never returns is
    /// not a refusal, it is a hang.
    fn resolve_pending(&self, source: &mut WindowsSource, peer_pid: u32) -> PeerRelation {
        let abandon_at = Instant::now() + PENDING_ABANDON_BOUND;
        let mut deadline = Instant::now() + PENDING_KEY_BOUND;
        loop {
            match self.pending(source) {
                Pending::None => return PeerRelation::NoMatch,
                Pending::Refused => return PeerRelation::Unresolvable,
                Pending::Job {
                    slot,
                    key: Some(key),
                    ..
                } => {
                    return match peer::resolve(source, peer_pid, std::slice::from_ref(&key)) {
                        peer::Resolution::Matched(0) => PeerRelation::Pending(slot),
                        peer::Resolution::Matched(_) => {
                            unreachable!("one pending key was supplied")
                        }
                        peer::Resolution::NoMatch => PeerRelation::NoMatch,
                        peer::Resolution::Unresolvable => PeerRelation::Unresolvable,
                    };
                }
                pending @ (Pending::Active | Pending::Job { key: None, .. }) => {
                    let now = Instant::now();
                    // Entry-relative, and checked before anything else in this arm so that no
                    // path below can extend the call past the bound.
                    if now >= abandon_at {
                        return PeerRelation::Abandoned;
                    }
                    if now < deadline {
                        let guard = self.state();
                        let _ = self
                            .shared
                            .changed
                            .wait_timeout(
                                guard,
                                deadline.min(abandon_at).saturating_duration_since(now),
                            )
                            .unwrap_or_else(PoisonError::into_inner);
                        continue;
                    }
                    if let Pending::Job {
                        pid,
                        job: Some(job),
                        ..
                    } = pending
                        && terminate_job(&job).is_ok()
                    {
                        self.release(pid);
                        return PeerRelation::Unresolvable;
                    }
                    // A process not yet assigned, a weakened hold, or a refused termination is
                    // still potentially the connecting child. Keep the door waiting rather than
                    // return the `NoRecordedAgent` column that admits its operator credential —
                    // but only until `abandon_at`, which the check above enforces.
                    deadline = Instant::now() + PENDING_KEY_BOUND;
                }
            }
        }
    }

    fn pending(&self, source: &WindowsSource) -> Pending {
        let state = self.state();
        if !state.active.is_empty() {
            return Pending::Active;
        }
        let jobs: Vec<_> = state
            .jobs
            .iter()
            .filter_map(|(pid, job)| {
                job.slot.map(|slot| {
                    (
                        *pid,
                        slot,
                        job.job.as_ref().and_then(JobHandle::live),
                        job.key.clone(),
                    )
                })
            })
            .collect();
        drop(state);
        for (pid, slot, job, key) in jobs {
            let Some(job) = job else {
                return Pending::Job {
                    pid,
                    slot,
                    job: None,
                    key,
                };
            };
            match source.in_specific_job(&job) {
                Read::Found(true) => {
                    return Pending::Job {
                        pid,
                        slot,
                        job: Some(job),
                        key,
                    };
                }
                Read::Found(false) | Read::Missing => {}
                Read::Refused => return Pending::Refused,
            }
        }
        Pending::None
    }

    fn state(&self) -> MutexGuard<'_, PeerState> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// An owned Toolhelp snapshot and a process handle that reserves the reported peer pid while the
/// walk and job query run.
struct WindowsSource {
    peer_pid: u32,
    peer: ProcessHandle,
    parents: BTreeMap<u32, u32>,
    shared: Arc<PeerShared>,
}

impl WindowsSource {
    fn new(peer_pid: u32, shared: Arc<PeerShared>) -> io::Result<Self> {
        let peer = ProcessHandle::open_query(peer_pid).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "could not open the peer process",
            )
        })?;
        if !matches!(answer_for(peer.raw()), ProbeAnswer::StartValue(_)) {
            return Err(io::Error::other(
                "the peer process is not live and readable",
            ));
        }
        Ok(Self {
            peer_pid,
            peer,
            parents: process_parents()?,
            shared,
        })
    }

    fn start_value(&self, pid: u32) -> Read<u64> {
        let answer = if pid == self.peer_pid {
            answer_for(self.peer.raw())
        } else {
            let Some(handle) = ProcessHandle::open_query(pid) else {
                return Read::Refused;
            };
            answer_for(handle.raw())
        };
        peer::from_probe(answer)
    }

    fn in_specific_job(&self, job: &LiveJob) -> Read<bool> {
        let mut result = 0;
        // SAFETY: both handles are live for this call: `self.peer` owns the process handle and the
        // borrowed `LiveJob` holds the job handle open. `result` is an aligned, initialised
        // out-parameter in this frame, and `IsProcessInJob` retains none of the three values.
        let ok = unsafe { IsProcessInJob(self.peer.raw(), job.raw(), &raw mut result) };
        if ok == 0 {
            Read::Refused
        } else {
            Read::Found(result != 0)
        }
    }
}

impl Source for WindowsSource {
    fn process(&mut self, pid: u32) -> Read<Process> {
        let Some(parent) = self.parents.get(&pid).copied() else {
            return Read::Missing;
        };
        match self.start_value(pid) {
            Read::Found(start_time) => Read::Found(Process {
                parent,
                start_time,
                boot: None,
            }),
            Read::Missing => Read::Missing,
            Read::Refused => Read::Refused,
        }
    }

    fn containment(&mut self, _peer: u32, root: u32) -> Read<Containment> {
        let job = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .jobs
            .get(&root)
            .and_then(|job| job.job.as_ref())
            .and_then(JobHandle::live);
        let Some(job) = job else {
            return Read::Refused;
        };
        match self.in_specific_job(&job) {
            Read::Found(true) => Read::Found(Containment::Matched),
            Read::Found(false) => Read::Found(Containment::Mismatched),
            Read::Missing => Read::Missing,
            Read::Refused => Read::Refused,
        }
    }
}

struct Snapshot(HANDLE);

impl Drop for Snapshot {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful `CreateToolhelp32Snapshot`, is owned by this
        // value, and is closed exactly once here. `CloseHandle` retains no pointer.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn process_parents() -> io::Result<BTreeMap<u32, u32>> {
    // SAFETY: both arguments are values. On success the returned handle is owned by `Snapshot`;
    // on failure it equals `INVALID_HANDLE_VALUE` and is never closed as a live handle.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = Snapshot(raw);
    let mut row = PROCESSENTRY32W {
        dwSize: u32::try_from(mem::size_of::<PROCESSENTRY32W>()).expect("structure size fits u32"),
        ..PROCESSENTRY32W::default()
    };
    // SAFETY: `snapshot` owns a valid read-only Toolhelp handle and `row` is initialised with the
    // required size. The call writes only to `row` and retains neither argument.
    if unsafe { Process32FirstW(snapshot.0, &raw mut row) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut parents = BTreeMap::new();
    loop {
        parents.insert(row.th32ProcessID, row.th32ParentProcessID);
        // SAFETY: the same valid snapshot and initialised out-parameter remain live. Each success
        // overwrites `row`; a false return is examined as end-of-enumeration or a real error.
        if unsafe { Process32NextW(snapshot.0, &raw mut row) } != 0 {
            continue;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NO_MORE_FILES.cast_signed()) {
            return Ok(parents);
        }
        return Err(error);
    }
}

fn terminate_job(job: &LiveJob) -> io::Result<()> {
    let handle = job.raw();
    // SAFETY: `handle` is the job object handle `job` holds open, and `job` is borrowed for this
    // call and therefore outlives it. `TerminateJobObject` takes it and a `u32` by value and
    // dereferences nothing.
    let ok = unsafe { TerminateJobObject(handle, TERMINATED) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Windows start values are captured at creation and mean the same thing across a reboot, so keys
/// from this host carry no boot identity.
// Uniform across the three platform layers on purpose: this shim cannot fail here and can on
// another, and one call site in `lib.rs` reads all three. Collapsing the signature per platform
// would move the `cfg` from the layer into the caller.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn boot_identity() -> io::Result<Option<String>> {
    Ok(None)
}

/// Create the job the child will be assigned to, with kill-on-close set.
///
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is the whole point and it is set explicitly: when the last
/// handle to the job closes — including because the daemon died without unwinding — every process
/// still in it is terminated. That is invariant 10 on this platform, and a job created without the
/// flag satisfies it on paper while leaking every descendant in practice.
pub(crate) fn prepare(_command: &mut Command) -> io::Result<Prep> {
    let mut limits = ExtendedLimitInfo::new();
    limits.limit_kill_on_job_close();
    let job = Job::create_with_limit_info(&limits).map_err(io::Error::other)?;
    Ok(Prep(job))
}

/// Assign the child to the job, and take the handle that reserves its identifier.
///
/// There is a window between `CreateProcess` returning and this assignment in which the child
/// could itself create a process outside the job. Closing it needs the child created suspended and
/// resumed after assignment, which needs the thread handle `std::process::Command` does not hand
/// out. Named rather than hidden; the child here is a vendor CLI that does not spawn in its first
/// microseconds.
pub(crate) fn grip_spawned(prep: Prep, child: &Child) -> Spawned {
    let handle = child.as_raw_handle() as isize;
    let Prep(job) = prep;
    // A failed assignment is reported, never propagated with `?`: the child exists by now, and an
    // early return would drop it. The grip that comes back is the weaker one — no job, but the
    // process handle, which `signal` falls back to and `shutdown` can therefore still reach.
    let (job, weakened) = match job.assign_process(handle) {
        Ok(()) => (Some(Arc::new(job)), None),
        // Dropped rather than kept: a job the child was never assigned to holds nothing, and
        // keeping it would make `signal` terminate an empty job and report success.
        Err(error) => (None, Some(io::Error::other(error))),
    };
    Spawned {
        boundary: if job.is_some() {
            Boundary::Job
        } else {
            Boundary::LeaderOnly
        },
        grip: Grip {
            job,
            process: ProcessHandle::open(child.id()),
        },
        weakened,
    }
}

#[cfg(test)]
pub(crate) fn grip_spawned_without_boundary_for_test(prep: Prep, child: &Child) -> Spawned {
    drop(prep);
    Spawned {
        grip: Grip {
            job: None,
            process: ProcessHandle::open(child.id()),
        },
        boundary: Boundary::LeaderOnly,
        weakened: Some(io::Error::other(
            "the spawned child could not be assigned to its job",
        )),
    }
}

/// The grip on an identifier this host did not spawn.
///
/// The handle is opened here, before the caller compares the key, and holding it is what keeps the
/// identifier from moving between the comparison and the signal.
pub(crate) fn grip_adopted(pid: u32) -> Grip {
    Grip {
        job: None,
        process: ProcessHandle::open(pid),
    }
}

/// The creation `FILETIME` of `pid`, as one opaque integer.
///
/// The two 32-bit halves are recombined into the 64-bit value the kernel keeps and compared as
/// read: never converted to a wall-clock time, never truncated to seconds. Truncation is what
/// makes two processes created in the same second indistinguishable, which is the whole failure
/// this key exists to prevent.
pub(crate) fn probe(grip: &Grip, pid: u32) -> ProbeAnswer {
    match &grip.process {
        Some(handle) => answer_for(handle.raw()),
        None => ProcessHandle::open(pid).map_or(ProbeAnswer::CouldNotAnswer, |handle| {
            answer_for(handle.raw())
        }),
    }
}

/// The probe's answer for one open handle.
///
/// The termination check comes **first**, and it is not an optimisation: a handle that opens is
/// not evidence of life on this platform, and neither is a creation stamp that matches, because a
/// terminated process keeps both for as long as anyone holds a handle to it. See the module docs.
fn answer_for(handle: HANDLE) -> ProbeAnswer {
    if has_terminated(handle) {
        return ProbeAnswer::CouldNotAnswer;
    }
    creation_time(handle)
}

/// Whether the kernel says the process behind `handle` has already terminated.
///
/// A process object is signalled once the process exits, so a zero-millisecond wait asks the
/// kernel rather than inferring from a probe — and it is the only thing on this platform that
/// tells a terminated process from a live one, since a retained handle keeps both resolvable.
fn has_terminated(handle: HANDLE) -> bool {
    has_terminated_checked(handle).unwrap_or(false)
}

/// `GetProcessTimes`' creation stamp for an open handle.
fn creation_time(handle: HANDLE) -> ProbeAnswer {
    let mut creation = windows_sys::Win32::Foundation::FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    // SAFETY: `handle` is a live process handle opened with `PROCESS_QUERY_LIMITED_INFORMATION`,
    // which is the access `GetProcessTimes` documents as sufficient, and it outlives this call
    // because the `ProcessHandle` that owns it is borrowed by the caller. The four out-parameters
    // are distinct, initialised, correctly aligned `FILETIME`s owned by this frame; the callee
    // writes each of them and retains none.
    let ok = unsafe {
        GetProcessTimes(
            handle,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if ok == 0 {
        return ProbeAnswer::CouldNotAnswer;
    }
    ProbeAnswer::StartValue(
        (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime),
    )
}

/// Terminate the job where there is one, the process otherwise.
///
/// The two signals collapse into one act on this platform: Windows has no polite equivalent of
/// `SIGTERM` that reaches a console-less child, so the host does not pretend to one. Terminating
/// the job is preferred because it names no identifier at all — it reaches the child and every
/// descendant in one call, and there is nothing in it for a recycled identifier to be substituted
/// into.
pub(crate) fn signal(grip: &Grip, pid: u32, sig: Sig) -> io::Result<()> {
    // `NothingSignalled` is a Unix group sweep's answer and this platform has no process group: a
    // job is terminated or it is not, so nothing here produces it. It is stated rather than
    // wildcarded, and it sits beside `Unavailable` because the two mean the same thing to the next
    // statement — nothing in the boundary took the signal, so try the leader.
    match signal_boundary(grip, sig)? {
        Delivery::Unavailable | Delivery::NothingSignalled => {
            match signal_leader(grip, pid, sig)? {
                Delivery::Accepted | Delivery::Absent => Ok(()),
                Delivery::NothingSignalled | Delivery::Unavailable => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no handle for that process identifier",
                )),
            }
        }
        Delivery::Accepted | Delivery::Absent => Ok(()),
    }
}

/// Terminate the job independently of the direct process handle.
pub(crate) fn signal_boundary(grip: &Grip, _sig: Sig) -> io::Result<Delivery> {
    if let Some(job) = &grip.job {
        return terminate_job(&LiveJob::Owned(Arc::clone(job))).map(|()| Delivery::Accepted);
    }
    Ok(Delivery::Unavailable)
}

/// Terminate the direct leader independently of its job.
pub(crate) fn signal_leader(grip: &Grip, _pid: u32, _sig: Sig) -> io::Result<Delivery> {
    let Some(process) = &grip.process else {
        return Ok(Delivery::Unavailable);
    };
    // SAFETY: `process.raw()` is a live handle opened with `PROCESS_TERMINATE` and owned by
    // `process`, which is borrowed for this call. `TerminateProcess` takes it and a `u32` by value
    // and dereferences nothing.
    let ok = unsafe { TerminateProcess(process.raw(), TERMINATED) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(Delivery::Accepted)
    }
}

pub(crate) use leader_target_state as target_state;

pub(crate) fn leader_target_state(grip: &Grip, _pid: u32) -> io::Result<TargetState> {
    grip.process
        .as_ref()
        .map_or(Ok(TargetState::LeaderAbsent), |process| {
            has_terminated_checked(process.raw()).map(|terminated| {
                if terminated {
                    TargetState::LeaderAbsent
                } else {
                    TargetState::Present
                }
            })
        })
}

/// Query only the retained native job with the dependency's fixed process-list buffer.
pub(crate) fn native_boundary_state(grip: &Grip, pid: u32) -> io::Result<TargetState> {
    let Some(job) = grip.job() else {
        return leader_target_state(grip, pid);
    };
    let members = job.query_process_id_list().map_err(io::Error::other)?;
    Ok(if members.is_empty() {
        TargetState::WholeAbsent
    } else {
        TargetState::Present
    })
}

/// Whether the process has stopped running.
///
/// The same question [`probe`] asks before it reads a creation stamp, through the same helper:
/// asking it separately would let `probe` skip it, which is the defect the module docs describe. No handle is nothing left to signal.
pub(crate) fn quiet(grip: &Grip, _pid: u32) -> bool {
    grip.process
        .as_ref()
        .is_none_or(|process| has_terminated(process.raw()))
}

pub(crate) fn quiet_checked(grip: &Grip, _pid: u32) -> io::Result<bool> {
    grip.process
        .as_ref()
        .map_or(Ok(true), |process| has_terminated_checked(process.raw()))
}

fn has_terminated_checked(handle: HANDLE) -> io::Result<bool> {
    // SAFETY: `handle` is a live process handle opened with `SYNCHRONIZE`, which is the access
    // `WaitForSingleObject` requires, and it is owned by a `ProcessHandle` borrowed by the caller.
    // The zero timeout means the call returns immediately and blocks nothing.
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        status => Err(io::Error::other(format!(
            "WaitForSingleObject returned unexpected status {status}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    //! The abandonment bound, measured where the state that produces it can be armed.
    //!
    //! This module is inside `windows.rs` and not in `bins/duded` because nothing outside this
    //! crate can reach the state under test: `pending()` gates on `state.active`, `activate()` is
    //! `pub(crate)`, and the `pub` `begin_spawn()` only pushes onto `queued`. A slot that is armed
    //! *and* unregistered is therefore constructible here and nowhere else.

    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use dudes_supervisor::host::AdoptionKey;

    use super::{PENDING_ABANDON_BOUND, PeerProcesses, PeerRelation};

    /// How long the test waits for the worker's answer before calling the loop dead.
    ///
    /// **A liveness guard, not a measurement.** The property being measured is the *lower* bound
    /// on `elapsed`, asserted inside the worker; this only exists so that a `resolve_pending` that
    /// never returns fails on a nameable assertion instead of hanging the harness. `10 ×` the
    /// bound, floored at 30 s — which for a 3 s bound is the floor.
    const LIVENESS: Duration = {
        let ten = Duration::from_secs(PENDING_ABANDON_BOUND.as_secs() * 10);
        if ten.as_secs() < 30 {
            Duration::from_secs(30)
        } else {
            ten
        }
    };

    /// Resolve this process's own pid against no recorded children, on a detached thread, and
    /// return the answer with how long the call took.
    ///
    /// **Detached, and never joined.** `JoinHandle::join` carries no deadline and a scope's
    /// implicit join blocks *after* the assertion panics, so either spelling turns a mutant that
    /// loops forever into a harness timeout — which is the failure mode this arm exists to replace
    /// with a named assertion.
    fn answer_within(
        processes: &PeerProcesses,
        bound: Duration,
    ) -> Option<(PeerRelation, Duration)> {
        let (sender, receiver) = mpsc::channel();
        let worker = processes.clone();
        thread::spawn(move || {
            let started = Instant::now();
            let answer = worker.resolve(std::process::id(), &[]);
            let _ = sender.send((answer, started.elapsed()));
        });
        receiver.recv_timeout(bound).ok()
    }

    /// A slot armed by `begin_spawn` + `activate` and never registered is abandoned,
    /// and the abandonment is not reached before the bound.
    ///
    /// The control runs first and its failure voids the arm below it. It asks for `NoMatch`
    /// specifically, not merely "not `Abandoned`": `NoMatch` is produced only inside
    /// `resolve_pending`, so a control that reads it has proved the loop was entered — which it
    /// must, because on a runner whose ancestry walk meets a process it cannot open, `peer::resolve`
    /// answers `Unresolvable`, `resolve` returns early, and this loop never runs at all. A control
    /// asking only for "promptly, and not `Abandoned`" passes in exactly that case while nothing
    /// ran.
    #[test]
    fn an_armed_and_unregistered_slot_abandons_the_walk_within_the_bound() {
        let processes = PeerProcesses::default();

        let (control, control_elapsed) =
            answer_within(&processes, LIVENESS).expect("control: the unarmed walk answers");
        assert_eq!(
            control,
            PeerRelation::NoMatch,
            "control: with no slot armed the pending loop is entered and walks out of it"
        );
        assert!(
            control_elapsed < PENDING_ABANDON_BOUND,
            "control: the unarmed walk waited {control_elapsed:?}, which is the bound it should \
             never reach"
        );

        let slot = processes.begin_spawn();
        assert_eq!(
            processes.activate(),
            Some(slot),
            "the slot this test arms is the one it allocated"
        );

        let (answer, elapsed) = answer_within(&processes, LIVENESS).expect(
            "the armed walk never answered inside the liveness guard: resolve_pending did not \
             bound itself from entry",
        );
        assert_eq!(
            answer,
            PeerRelation::Abandoned,
            "an armed, unregistered slot must abandon rather than answer a column"
        );
        assert!(
            elapsed >= PENDING_ABANDON_BOUND,
            "abandoned after {elapsed:?}, which is short of {PENDING_ABANDON_BOUND:?}: the bound \
             is not entry-relative"
        );
    }

    #[test]
    fn a_published_slot_remains_until_the_tracked_child_is_released() {
        let processes = PeerProcesses::default();
        let slot = processes.begin_spawn();
        assert_eq!(processes.activate(), Some(slot));
        processes.register(7, None, Some(slot));
        let key = AdoptionKey {
            pid: 7,
            start_time: 42,
            boot: None,
        };
        processes.keyed(7, &key);

        assert_eq!(
            processes.published(std::slice::from_ref(&key)),
            vec![(slot, 0)]
        );
        assert_eq!(
            processes.published(std::slice::from_ref(&key)),
            vec![(slot, 0)],
            "publication is idempotent while the tracked child remains live"
        );
        processes.release(7);
        assert!(processes.published(&[key]).is_empty());
    }
}
