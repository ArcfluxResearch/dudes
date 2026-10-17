//! The fixture every daemon test boots against, and the seams it substitutes.
//!
//! # What is fake and what is real, and why each is which
//!
//! - **The process host is chosen by the test.** Most use `FakeHost`, which models the sweep rather
//!   than accepting it:
//!   it counts `shutdown` and marks every pid it handed out `NoSuchPid`, so a
//!   daemon that never called it cannot pass a test that says it did. Behind a `Mutex` here, so the
//!   test can read the counter after `run` has consumed the daemon. The joined-uncertainty
//!   control uses [`RetainedFailureHost`], a wrapper around the real host that substitutes only the
//!   deterministic failing launch spec and keeps ownership observable.
//! - **The predicate runner is whichever the test needs.** [`ScriptedRunner`] answers from a script
//!   and counts sweeps, for the tests about verdicts; the *real* `dudes_host::OneShot` is used by
//!   `predicate_exit.rs`, because the exit sweep is a claim about a real child.
//! - **The clock and the randomness are fake**, so entry stamps and ids are values a test can name.
//!
//! # Why the clock is not `dudes_supervisor::testing::FakeClock`
//!
//! Originally because that one held a `Cell`, making it `Send` and not `Sync`, while the daemon
//! shares its clock between the machine and the runtime through an `Arc` — the day a spend is
//! bucketed into and the day the `TurnEnded` beside it is stamped with have to be one value.
//! **That reason is spent:** `FakeClock` is an `Arc<AtomicI64>` and is `Sync`, so it would satisfy
//! this seam today. [`TestClock`] stays to keep an unrelated fixture refactor out of the change
//! that made `FakeClock` shareable. It is a fixture, not a second rule: it reads a value it was
//! told and nothing else. `bins/dudes/tests/common/mod.rs` carries the same type and says the
//! same thing, so a correction here belongs there too.
//!
//! # Reading the daemon while it runs
//!
//! `Daemon::run` consumes the daemon, so a test that wants to see inside a *running* one reads the
//! journal — the same files the daemon is writing, through the same reader the daemon's own
//! `JournalRead` port uses. That is stronger than an accessor would be: it asserts on what reached
//! the disk.
#![allow(dead_code)]

pub(crate) mod hook_shim;
mod rng;
pub(crate) use rng::FixtureRng;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use duded::predicate::{PredicateOutcome, PredicateRunner};
use duded::pty_output::{OutputSink, PtyOutput};
use duded::settings::{SOCKET_NAME, Seams, Settings};
use duded::startup::Witness;
use dudes_host::RealHost;
use dudes_journal::{Fsync, JournalOptions, Reader};
use dudes_policy::BreakerConfig;
use dudes_proto::goal::Predicate;
use dudes_proto::rpc::agent::HireParams;
use dudes_proto::rpc::goal::GoalSetParams;
use dudes_proto::rpc::pty::PtyId;
use dudes_proto::{AgentBudget, Entry, GoalBudget, Record, Spend};
use dudes_providers::adapter::{Adapter, HookInstall, ManagedInstall};
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::claude::config::ClaudeConfig;
use dudes_providers::launch::{EnvVar, LaunchSpec};
use dudes_providers::registry::Adapters;
use dudes_providers::stream::{FinalReply, FinalReplyCompleteness, LineRead, TurnEnd, TurnReader};
use dudes_supervisor::host::{
    AdoptionKey, Clock, HostBoot, KillOutcome, Liveness, MAX_OWNED_TARGET_QUERY_ROWS,
    NativeBoundaryAcknowledgement, NativeBoundaryReceipt, OwnedTargetEvidence, OwnedTargetState,
    OwnedTargets, OwnedTargetsStatus, ProcessHost, SpawnResult,
};
use dudes_supervisor::testing::{FakeHost, FakeRng};
use dudes_types::{AgentId, GoalId, Lsn, Provider, Timestamp, Tokens, TurnId, Usd};

/// A clock that moves only when the test says so, and is `Sync` so the daemon can share it.
#[derive(Debug)]
pub(crate) struct TestClock {
    at: AtomicI64,
    step: i64,
}

impl TestClock {
    /// A clock that advances `step` milliseconds on each read, so consecutive entries carry
    /// distinct, predictable stamps.
    pub(crate) const fn ticking(start_millis: i64, step: i64) -> Self {
        Self {
            at: AtomicI64::new(start_millis),
            step,
        }
    }

    /// A clock at a fixed moment in 2026, ticking one millisecond a read.
    pub(crate) const fn new() -> Self {
        Self::ticking(1_787_394_600_000, 1)
    }

    /// Move it forward without reading it.
    pub(crate) fn advance(&self, millis: i64) {
        self.at.fetch_add(millis, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(self.at.fetch_add(self.step, Ordering::SeqCst))
    }
}

/// `FakeHost` behind a lock, so the test keeps a handle on it after `run` has taken the daemon.
#[derive(Clone, Debug, Default)]
pub(crate) struct SharedHost(Arc<Mutex<FakeHost>>, Arc<Mutex<Option<ManagedNativeState>>>);

#[derive(Debug)]
struct ManagedNativeState {
    journal: PathBuf,
    next: u64,
    targets: BTreeMap<u64, ManagedNativeTarget>,
    acknowledged: Vec<(AdoptionKey, NativeAttemptBirth)>,
    children: Option<Arc<duded::ChildEnds>>,
    watches: BTreeMap<u64, ManagedChildWatch>,
}

struct ManagedChildWatch {
    key: AdoptionKey,
    watcher: duded::ChildTurn,
    result: Arc<Mutex<Option<TurnEnd>>>,
}

impl std::fmt::Debug for ManagedChildWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedChildWatch")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

struct MockTurnReader(Arc<Mutex<Option<TurnEnd>>>);

impl std::fmt::Debug for MockTurnReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockTurnReader").finish_non_exhaustive()
    }
}

impl TurnReader for MockTurnReader {
    fn line(&mut self, _text: &str) -> LineRead {
        LineRead::default()
    }

    fn end(self: Box<Self>, _exit: dudes_proto::ExitStatus) -> TurnEnd {
        let mut result = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        result
            .take()
            .expect("the exact mock completion was scripted before EOF")
    }
}

#[derive(Debug)]
struct ManagedNativeTarget {
    key: AdoptionKey,
    cwd: PathBuf,
    birth: Option<NativeAttemptBirth>,
    receipt: NativeBoundaryReceipt,
    reaped: bool,
    observed: bool,
}

fn same_native_key(left: &AdoptionKey, right: &AdoptionKey) -> bool {
    left.pid == right.pid && left.start_time == right.start_time && left.boot == right.boot
}

#[derive(Clone, Copy, Debug)]
struct NativeAttemptBirth {
    tail: Lsn,
    agent: AgentId,
}

fn native_journal_entries(journal: &Path) -> Option<Vec<(Lsn, dudes_proto::Entry)>> {
    Reader::new(journal)
        .iter_from(Lsn::ZERO)
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()
}

fn native_attempt_birth(journal: &Path, spec: &LaunchSpec) -> Option<NativeAttemptBirth> {
    let entries = native_journal_entries(journal)?;
    let tail = entries.last()?.0;
    let mut mailboxes = spec.env.iter().filter(|var| var.name() == "DUDES_MAILBOX");
    let mailbox = Path::new(mailboxes.next()?.expose_value());
    if mailboxes.next().is_some() || mailbox.parent()?.file_name()? != "agents" {
        return None;
    }
    let agent = AgentId::parse(mailbox.file_name()?.to_str()?).ok()?;
    Some(NativeAttemptBirth { tail, agent })
}

fn native_boundary_written(
    journal: &Path,
    key: &AdoptionKey,
    pty: Option<PtyId>,
    birth: Option<NativeAttemptBirth>,
) -> bool {
    let Some(birth) = birth else {
        return false;
    };
    let Some(entries) = native_journal_entries(journal) else {
        return false;
    };
    entries.iter().any(|(settled_lsn, settled)| {
        let Record::InstructionAttemptBoundarySettled { owner, started_lsn, proof:
            dudes_proto::InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                receipt: dudes_proto::InstructionNativeBoundaryReceipt::Spawned { lsn: spawn_lsn },
            },
        } = &settled.record else { return false; };
        let agent = match owner {
            dudes_proto::InstructionObservationOwner::Managed { agent, .. }
                | dudes_proto::InstructionObservationOwner::AttachedProcess { agent, .. } => *agent,
            dudes_proto::InstructionObservationOwner::AttachedTurn { .. } => return false,
        };
        if agent != birth.agent || *started_lsn > birth.tail || *spawn_lsn <= birth.tail
            || *settled_lsn <= *spawn_lsn {
            return false;
        }
        let start_matches = entries.iter().any(|(lsn, entry)| *lsn == *started_lsn
            && matches!(entry.record, Record::InstructionObservationStarted { owner: seen, .. } if seen == *owner));
        start_matches && entries.iter().any(|(lsn, entry)| *lsn == *spawn_lsn && match (&entry.record, owner, pty) {
            (Record::TurnSpawned { agent, turn, pid, start_time, boot, .. },
                dudes_proto::InstructionObservationOwner::Managed { agent: owned, turn: owned_turn, .. }, None) => {
                agent == owned && turn == owned_turn && *pid == key.pid
                    && *start_time == key.start_time && *boot == key.boot
            }
            (Record::PtySpawned { agent, pty: spawned, pid, start_time, boot, .. },
                dudes_proto::InstructionObservationOwner::AttachedProcess { agent: owned, .. }, Some(pty)) => {
                agent == owned && *spawned == pty && *pid == key.pid
                    && *start_time == key.start_time && *boot == key.boot
            }
            _ => false,
        })
    })
}

impl SharedHost {
    /// Wrap one deliberately shaped fake host.
    pub(crate) fn with(host: FakeHost) -> Self {
        Self(Arc::new(Mutex::new(host)), Arc::new(Mutex::new(None)))
    }

    /// A wall-clock-anchored host, pids from 1000.
    pub(crate) fn new() -> Self {
        Self::with(FakeHost::wall_clock_anchored())
    }

    pub(crate) fn with_native_evidence(journal: PathBuf) -> Self {
        Self(
            Arc::new(Mutex::new(FakeHost::wall_clock_anchored())),
            Arc::new(Mutex::new(Some(ManagedNativeState {
                journal,
                next: 0,
                targets: BTreeMap::new(),
                acknowledged: Vec::new(),
                children: None,
                watches: BTreeMap::new(),
            }))),
        )
    }

    pub(crate) fn bind_child_ends(&self, children: Arc<duded::ChildEnds>) {
        let mut native = self.1.lock().unwrap_or_else(PoisonError::into_inner);
        let native = native
            .as_mut()
            .expect("owned child binding requires native opt-in");
        assert!(
            native.targets.is_empty() && native.children.is_none(),
            "bind one child owner before any spawn"
        );
        native.children = Some(children);
    }

    pub(crate) fn finish_owned_child(&self, key: &AdoptionKey, input: dudes_supervisor::Input) {
        let dudes_supervisor::Input::ChildExited {
            exit,
            elapsed_ms,
            usage,
            model_usage,
            outcome,
            last_message,
            final_reply,
            ..
        } = input
        else {
            panic!("expected an exact mock child completion payload");
        };
        let watch = {
            let mut native = self.1.lock().unwrap_or_else(PoisonError::into_inner);
            let native = native
                .as_mut()
                .expect("owned completion requires native opt-in");
            let mut matches = native
                .watches
                .iter()
                .filter(|(_, watch)| same_native_key(&watch.key, key));
            let (&ordinal, _) = matches.next().expect("exact retained owned ChildTurn");
            assert!(
                matches.next().is_none(),
                "owned completion key must be unambiguous"
            );
            native.watches.remove(&ordinal).unwrap()
        };
        *watch.result.lock().unwrap_or_else(PoisonError::into_inner) = Some(TurnEnd {
            final_observation: None,
            outcome,
            usage,
            model_usage,
            last_message,
            final_reply: final_reply.map(|reply| FinalReply {
                text: reply.text,
                completeness: match reply.completeness {
                    dudes_proto::ChatReplyCompleteness::Complete => {
                        FinalReplyCompleteness::Complete
                    }
                    dudes_proto::ChatReplyCompleteness::Truncated => {
                        FinalReplyCompleteness::Truncated
                    }
                },
            }),
            usage_refused: None,
            session: None,
            attribution_refused: 0,
            unreadable: 0,
        });
        watch.watcher.ended_with_elapsed(exit, elapsed_ms);
    }

    pub(crate) fn mark_native_reaped(&self, key: &AdoptionKey) {
        let mut host = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut native = self.1.lock().unwrap_or_else(PoisonError::into_inner);
        let state = native.as_mut().expect("native evidence must be opted in");
        let mut matching = state
            .targets
            .values_mut()
            .filter(|target| same_native_key(&target.key, key));
        let target = matching.next().expect("exact retained mock spawn");
        assert!(
            matching.next().is_none(),
            "a full key cannot select two retained attempts"
        );
        target.reaped = true;
        host.answer(key.pid, dudes_supervisor::host::ProbeAnswer::NoSuchPid);
    }

    pub(crate) fn native_acknowledged(&self, key: &AdoptionKey) -> bool {
        self.1
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|native| {
                !native
                    .targets
                    .values()
                    .any(|target| same_native_key(&target.key, key))
                    && native
                        .acknowledged
                        .iter()
                        .any(|(ack, _)| same_native_key(ack, key))
            })
    }

    fn native_settlement_written(&self, key: &AdoptionKey) -> bool {
        let native = self.1.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(native) = native.as_ref() else {
            return false;
        };
        native
            .acknowledged
            .iter()
            .rev()
            .find_map(|(ack, birth)| same_native_key(ack, key).then_some(*birth))
            .is_some_and(|birth| native_boundary_written(&native.journal, key, None, Some(birth)))
    }

    pub(crate) fn await_native_acknowledged(&self, key: &AdoptionKey) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.native_acknowledged(key) {
            assert!(
                Instant::now() < deadline,
                "exact native absence was not acknowledged"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Read the fake.
    pub(crate) fn peek<T>(&self, f: impl FnOnce(&FakeHost) -> T) -> T {
        f(&self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// How many times `shutdown` was called. The number an exit path owes is one.
    pub(crate) fn shutdowns(&self) -> usize {
        self.peek(|host| host.shutdowns)
    }

    /// The specs handed to `spawn`, in order.
    pub(crate) fn spawned(&self) -> Vec<LaunchSpec> {
        self.peek(|host| host.spawned.iter().map(|(spec, _)| spec.clone()).collect())
    }

    /// The pids the fake handed out.
    pub(crate) fn pids(&self) -> Vec<u32> {
        self.peek(|host| host.spawned_pids.clone())
    }

    pub(crate) fn answer(&self, pid: u32, answer: dudes_supervisor::host::ProbeAnswer) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .answer(pid, answer);
    }

    /// What the fake says about one of its own keys now.
    pub(crate) fn alive_of(&self, pid: u32) -> Liveness {
        self.peek(|host| host.alive(&host.key_for(pid)))
    }
}

impl ProcessHost for SharedHost {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        let birth = self
            .1
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(|native| native_attempt_birth(&native.journal, spec));
        let result = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .spawn(spec, install);
        if let SpawnResult::Spawned(key) = &result
            && let Some(native) = self
                .1
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_mut()
        {
            native.next += 1;
            let ordinal = native.next;
            if let Some(children) = &native.children {
                let result = Arc::new(Mutex::new(None));
                children.read_next_with(Some(Box::new(MockTurnReader(Arc::clone(&result)))));
                let watcher = children.watch(Some(key));
                native.watches.insert(
                    ordinal,
                    ManagedChildWatch {
                        key: key.clone(),
                        watcher,
                        result,
                    },
                );
            }
            native.targets.insert(
                ordinal,
                ManagedNativeTarget {
                    key: key.clone(),
                    cwd: spec.cwd.clone(),
                    birth,
                    receipt: NativeBoundaryReceipt::fresh(),
                    reaped: false,
                    observed: false,
                },
            );
        }
        result
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .alive(key)
    }

    fn owned_targets(&self, limit: usize) -> OwnedTargets {
        let host = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut native = self.1.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(native) = native.as_mut() else {
            return OwnedTargets {
                targets: Vec::new(),
                status: OwnedTargetsStatus::UnknownUnsupported,
            };
        };
        let targets: Vec<_> = native
            .targets
            .values_mut()
            .take(limit.min(MAX_OWNED_TARGET_QUERY_ROWS))
            .map(|target| {
                target.observed |= target.reaped;
                OwnedTargetEvidence {
                    key: Some(target.key.clone()),
                    cwd: target.cwd.clone(),
                    state: if target.reaped {
                        OwnedTargetState::NativeBoundaryAbsent
                    } else if host.alive(&target.key) == Liveness::Alive {
                        OwnedTargetState::Present
                    } else {
                        OwnedTargetState::UnknownReapUnconfirmed
                    },
                    native_absence_receipt: target.reaped.then(|| target.receipt.clone()),
                }
            })
            .collect();
        let status = if targets.len() == native.targets.len() {
            OwnedTargetsStatus::Complete
        } else {
            OwnedTargetsStatus::Truncated {
                omitted: native.targets.len() - targets.len(),
            }
        };
        OwnedTargets { targets, status }
    }

    fn acknowledge_native_boundary_absence(
        &mut self,
        key: &AdoptionKey,
        receipt: &NativeBoundaryReceipt,
    ) -> NativeBoundaryAcknowledgement {
        let mut native = self.1.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(native) = native.as_mut() else {
            return NativeBoundaryAcknowledgement::UnknownUnsupported;
        };
        let mut matches = native
            .targets
            .iter()
            .filter(|(_, target)| same_native_key(&target.key, key));
        let Some((&ordinal, target)) = matches.next() else {
            return NativeBoundaryAcknowledgement::UnknownIdentity;
        };
        if matches.next().is_some() || target.receipt != *receipt {
            return NativeBoundaryAcknowledgement::UnknownIdentity;
        }
        if !target.reaped
            || !target.observed
            || !native_boundary_written(&native.journal, key, None, target.birth)
        {
            return NativeBoundaryAcknowledgement::RefusedUnobserved;
        }
        let birth = target.birth.expect("validated current mock attempt birth");
        native.targets.remove(&ordinal);
        native.acknowledged.push((key.clone(), birth));
        NativeBoundaryAcknowledgement::Acknowledged
    }

    fn boot_identity(&self) -> HostBoot {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .boot_identity()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .shutdown()
    }

    fn write_hooks(&mut self, _install: &HookInstall) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct FakePtyHost {
    journal: PathBuf,
    next_id: u64,
    opens: Vec<(LaunchSpec, u16, u16)>,
    closes: Vec<PtyId>,
    live: BTreeSet<PtyId>,
    attached_before_open: bool,
    shutdowns: usize,
    fail_shutdown_once: bool,
    next_open: Option<FakePtyOpen>,
    fail_close_count: usize,
    writes: Vec<(PtyId, Vec<u8>)>,
    /// One entry per host write: was a `PtyWriteRequested` for those exact bytes already on the
    /// disk when the host was called? Read from the journal inside `write`, for
    /// `attached_before_open`'s reason — the write order is only observable from
    /// inside the call it brackets.
    requested_before_write: Vec<bool>,
    /// One entry per host close: its matching request was already fsynced.
    requested_before_close: Vec<bool>,
    /// One entry per still-live handle at host shutdown: its batch request was durable.
    requested_before_shutdown: Vec<bool>,
    next_write: Option<FakePtyWrite>,
    resizes: Vec<(PtyId, u16, u16)>,
    fail_next_resize: bool,
    outputs: BTreeMap<PtyId, (Arc<OutputSink>, PtyOutput)>,
    native: Option<BTreeMap<PtyId, PtyNativeTarget>>,
    native_acknowledged: Vec<PtyId>,
}

#[derive(Debug)]
struct PtyNativeTarget {
    key: Option<AdoptionKey>,
    birth: Option<NativeAttemptBirth>,
    receipt: NativeBoundaryReceipt,
    reaped: bool,
    observed: bool,
}

#[derive(Debug)]
enum FakePtyOpen {
    Failed,
    CleanupUnconfirmed,
}

/// A scripted host answer for the next write, so `Partial` and `Failed` are reachable without a
/// real terminal that has to be made to misbehave.
#[derive(Debug)]
enum FakePtyWrite {
    /// Take this many bytes and stop.
    Short(usize),
    /// Take none.
    Failed,
}

/// A shared, scripted PTY port for attached-route integration tests.
#[derive(Clone, Debug)]
pub(crate) struct SharedPtyHost(Arc<Mutex<FakePtyHost>>);

impl SharedPtyHost {
    pub(crate) fn new(journal: PathBuf) -> Self {
        Self(Arc::new(Mutex::new(FakePtyHost {
            journal,
            next_id: 41,
            opens: Vec::new(),
            closes: Vec::new(),
            live: BTreeSet::new(),
            attached_before_open: false,
            shutdowns: 0,
            fail_shutdown_once: false,
            next_open: None,
            fail_close_count: 0,
            writes: Vec::new(),
            requested_before_write: Vec::new(),
            requested_before_close: Vec::new(),
            requested_before_shutdown: Vec::new(),
            next_write: None,
            resizes: Vec::new(),
            fail_next_resize: false,
            outputs: BTreeMap::new(),
            native: None,
            native_acknowledged: Vec::new(),
        })))
    }

    pub(crate) fn with_native_evidence(journal: PathBuf) -> Self {
        let host = Self::new(journal);
        host.0.lock().unwrap_or_else(PoisonError::into_inner).native = Some(BTreeMap::new());
        host
    }

    pub(crate) fn native_key(&self, pty: PtyId) -> Option<AdoptionKey> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .native
            .as_ref()?
            .get(&pty)?
            .key
            .clone()
    }

    pub(crate) fn native_key_for_child(&self, pid: u32) -> Option<AdoptionKey> {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut keys = state
            .native
            .as_ref()?
            .values()
            .filter_map(|target| target.key.as_ref())
            .filter(|key| key.pid == pid);
        let key = keys.next()?;
        if keys.next().is_some() {
            return None;
        }
        Some(key.clone())
    }

    pub(crate) fn native_acknowledged(&self, pty: PtyId) -> bool {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        !state
            .native
            .as_ref()
            .is_some_and(|targets| targets.contains_key(&pty))
            && state.native_acknowledged.contains(&pty)
    }

    pub(crate) fn await_native_acknowledged(&self, pty: PtyId) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.native_acknowledged(pty) {
            assert!(
                Instant::now() < deadline,
                "exact PTY native absence was not acknowledged"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn block_host_calls(&self) -> std::sync::MutexGuard<'_, impl std::fmt::Debug> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn live(&self) -> BTreeSet<PtyId> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .live
            .clone()
    }

    pub(crate) fn end_output(&self, pty: PtyId) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .outputs
            .remove(&pty);
    }

    pub(crate) fn fail_next_resize(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fail_next_resize = true;
    }

    pub(crate) fn resizes(&self) -> Vec<(PtyId, u16, u16)> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resizes
            .clone()
    }

    pub(crate) fn output_sink(&self, pty: PtyId) -> Arc<OutputSink> {
        Arc::clone(
            &self
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .outputs
                .get(&pty)
                .expect("an opened fake PTY")
                .0,
        )
    }

    pub(crate) fn produce_output(&self, pty: PtyId, bytes: &[u8]) {
        self.output_sink(pty).push(bytes);
    }

    /// Every host write this run performed, in order, with the exact bytes each carried.
    pub(crate) fn writes(&self) -> Vec<(PtyId, Vec<u8>)> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .writes
            .clone()
    }

    /// For each host write, whether its `PtyWriteRequested` was durable before the call.
    pub(crate) fn requested_before_write(&self) -> Vec<bool> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .requested_before_write
            .clone()
    }

    pub(crate) fn requested_before_close(&self) -> Vec<bool> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .requested_before_close
            .clone()
    }

    pub(crate) fn requested_before_shutdown(&self) -> Vec<bool> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .requested_before_shutdown
            .clone()
    }

    pub(crate) fn mark_already_closed(&self, pty: PtyId) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.live.remove(&pty);
        if state.native.is_some() {
            state.outputs.remove(&pty);
        }
        if let Some(target) = state
            .native
            .as_mut()
            .and_then(|targets| targets.get_mut(&pty))
        {
            target.reaped = true;
        }
    }

    /// Make the next host write take only `bytes` of what it is given.
    pub(crate) fn short_next_write(&self, bytes: usize) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next_write = Some(FakePtyWrite::Short(bytes));
    }

    /// How many times the daemon swept this host on its way out.
    ///
    /// A wiring control: a run whose host records a shutdown is a run that really held *this*
    /// object, which is what makes "and it recorded no writes" evidence rather than a tautology
    /// about an object nobody consulted.
    pub(crate) fn shutdowns(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .shutdowns
    }

    /// Make the next host write fail outright.
    pub(crate) fn fail_next_write(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next_write = Some(FakePtyWrite::Failed);
    }

    pub(crate) fn opens(&self) -> Vec<(LaunchSpec, u16, u16)> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .opens
            .clone()
    }

    pub(crate) fn closes(&self) -> Vec<PtyId> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closes
            .clone()
    }

    pub(crate) fn attached_before_open(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .attached_before_open
    }

    pub(crate) fn fail_next_open(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next_open = Some(FakePtyOpen::Failed);
    }

    pub(crate) fn retain_next_failed_open(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next_open = Some(FakePtyOpen::CleanupUnconfirmed);
    }

    pub(crate) fn fail_next_close(&self) {
        self.fail_next_closes(1);
    }

    pub(crate) fn fail_next_closes(&self, count: usize) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fail_close_count = count;
    }

    pub(crate) fn fail_next_shutdown(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fail_shutdown_once = true;
    }
}

impl duded::ports::PtyHostPort for SharedPtyHost {
    fn open(
        &mut self,
        spec: LaunchSpec,
        cols: u16,
        rows: u16,
    ) -> Result<duded::ports::OpenedPty, duded::ports::PtyOpenFailure> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let birth = state
            .native
            .as_ref()
            .and_then(|_| native_attempt_birth(&state.journal, &spec));
        state.attached_before_open = Reader::new(&state.journal)
            .iter_from(Lsn::ZERO)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|(_, entry)| matches!(entry.record, Record::Attached { .. }));
        state.opens.push((spec, cols, rows));
        let id = PtyId(state.next_id);
        state.next_id += 1;
        match state.next_open.take() {
            Some(FakePtyOpen::Failed) => {
                return Err(duded::ports::PtyOpenFailure::Failed(io::Error::other(
                    "injected PTY open failure",
                )));
            }
            Some(FakePtyOpen::CleanupUnconfirmed) => {
                state.live.insert(id);
                if let Some(native) = &mut state.native {
                    native.insert(
                        id,
                        PtyNativeTarget {
                            key: None,
                            birth,
                            receipt: NativeBoundaryReceipt::fresh(),
                            reaped: false,
                            observed: false,
                        },
                    );
                }
                return Err(duded::ports::PtyOpenFailure::CleanupUnconfirmed {
                    pty: id,
                    cause: io::Error::other("injected PTY open failure"),
                    cleanup: io::Error::other("injected PTY cleanup uncertainty"),
                });
            }
            None => {}
        }
        state.live.insert(id);
        let pid = if let Some(native) = &mut state.native {
            let key = AdoptionKey {
                pid: 70_000 + u32::try_from(id.0).expect("bounded mock PTY id"),
                start_time: 900 + id.0,
                boot: None,
            };
            let pid = key.pid;
            native.insert(
                id,
                PtyNativeTarget {
                    key: Some(key),
                    birth,
                    receipt: NativeBoundaryReceipt::fresh(),
                    reaped: false,
                    observed: false,
                },
            );
            pid
        } else {
            0
        };
        let (sink, output) = duded::pty_output::channel();
        state.outputs.insert(id, (Arc::new(sink), output));
        Ok(duded::ports::OpenedPty {
            pty: id,
            child: duded::ports::PtyChild {
                pid,
                #[cfg(windows)]
                job: None,
            },
        })
    }

    fn resize(&mut self, pty: PtyId, cols: u16, rows: u16) -> io::Result<()> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.resizes.push((pty, cols, rows));
        if std::mem::take(&mut state.fail_next_resize) {
            return Err(io::Error::other("injected PTY resize failure"));
        }
        if state.live.contains(&pty) {
            Ok(())
        } else {
            Err(io::Error::other("the fake PTY is not open"))
        }
    }

    fn subscribe(&mut self, pty: PtyId) -> Option<duded::pty_output::OutputSubscription> {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.outputs.get(&pty)?.1.subscribe()
    }

    fn write(&mut self, pty: PtyId, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        // Read the disk from inside the call, the way `open` reads it for `Attached`: whether the
        // request was durable *before* the host was touched is only observable here, and a check
        // made after the call returns would be satisfied by an append in either order.
        let digest = blake3::hash(bytes).to_hex().to_string();
        let requested = Reader::new(&state.journal)
            .iter_from(Lsn::ZERO)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|(_, entry)| match entry.record {
                Record::PtyWriteRequested {
                    pty: seen, payload, ..
                } => {
                    seen == pty
                        && payload.blake3 == digest
                        && payload.decoded_len == bytes.len() as u64
                }
                _ => false,
            });
        state.requested_before_write.push(requested);
        state.writes.push((pty, bytes.to_vec()));
        match state.next_write.take() {
            Some(FakePtyWrite::Short(taken)) => Ok(taken.min(bytes.len())),
            Some(FakePtyWrite::Failed) => Err(io::Error::other("injected PTY write failure")),
            None => Ok(bytes.len()),
        }
    }

    fn close(&mut self, pty: PtyId) -> duded::ports::PtyCloseOutcome {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let requests = Reader::new(&state.journal)
            .iter_from(Lsn::ZERO)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|(_, entry)| {
                matches!(entry.record, Record::PtyCloseRequested { pty: seen, .. } if seen == pty)
            })
            .count();
        let prior_closes = state.closes.iter().filter(|seen| **seen == pty).count();
        state.requested_before_close.push(requests > prior_closes);
        state.closes.push(pty);
        if state.fail_close_count > 0 {
            state.fail_close_count -= 1;
            return duded::ports::PtyCloseOutcome::Failed(io::Error::other(
                "injected PTY close failure",
            ));
        }
        state.outputs.remove(&pty);
        if state.live.remove(&pty) {
            if let Some(target) = state
                .native
                .as_mut()
                .and_then(|targets| targets.get_mut(&pty))
            {
                target.reaped = true;
            }
            duded::ports::PtyCloseOutcome::Closed
        } else {
            duded::ports::PtyCloseOutcome::AlreadyClosed
        }
    }

    fn track_native_close(
        &mut self,
        pty: PtyId,
    ) -> Result<NativeBoundaryReceipt, duded::ports::PtyNativeBoundaryUnknown> {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let targets = state
            .native
            .as_ref()
            .ok_or(duded::ports::PtyNativeBoundaryUnknown::Unsupported)?;
        let target = targets
            .get(&pty)
            .filter(|target| target.key.is_some())
            .ok_or(duded::ports::PtyNativeBoundaryUnknown::Identity)?;
        Ok(target.receipt.clone())
    }

    fn native_close_absence(
        &mut self,
        pty: PtyId,
        identity: &NativeBoundaryReceipt,
    ) -> Result<(), duded::ports::PtyNativeBoundaryUnknown> {
        let mut state = match self.0.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => {
                return Err(duded::ports::PtyNativeBoundaryUnknown::Unconfirmed);
            }
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        let targets = state
            .native
            .as_mut()
            .ok_or(duded::ports::PtyNativeBoundaryUnknown::Unsupported)?;
        let target = targets
            .get_mut(&pty)
            .filter(|target| target.key.is_some() && target.receipt == *identity)
            .ok_or(duded::ports::PtyNativeBoundaryUnknown::Identity)?;
        if !target.reaped {
            return Err(duded::ports::PtyNativeBoundaryUnknown::Unconfirmed);
        }
        target.observed = true;
        Ok(())
    }

    fn acknowledge_native_close(&mut self, pty: PtyId, identity: &NativeBoundaryReceipt) -> bool {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(target) = state.native.as_ref().and_then(|targets| targets.get(&pty)) else {
            return false;
        };
        let Some(key) = &target.key else {
            return false;
        };
        if target.receipt != *identity
            || !target.reaped
            || !target.observed
            || !native_boundary_written(&state.journal, key, Some(pty), target.birth)
        {
            return false;
        }
        state.native.as_mut().unwrap().remove(&pty);
        state.native_acknowledged.push(pty);
        true
    }

    fn shutdown(&mut self) -> io::Result<()> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let entries: Vec<_> = Reader::new(&state.journal)
            .iter_from(Lsn::ZERO)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .collect();
        let live: Vec<_> = state.live.iter().copied().collect();
        for pty in live {
            let requests = entries
                .iter()
                .filter(|(_, entry)| matches!(&entry.record, Record::PtyCloseRequested { pty: seen, .. } if *seen == pty))
                .count();
            let prior_closes = state.closes.iter().filter(|seen| **seen == pty).count();
            state
                .requested_before_shutdown
                .push(requests > prior_closes);
        }
        state.shutdowns += 1;
        if std::mem::take(&mut state.fail_shutdown_once) {
            return Err(io::Error::other("injected PTY host shutdown failure"));
        }
        state.live.clear();
        state.outputs.clear();
        if let Some(native) = &mut state.native {
            for target in native.values_mut() {
                target.reaped = true;
            }
        }
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[derive(Debug)]
struct NativeRelation {
    ptys: SharedPtyHost,
    slots: Mutex<NativeRelationState>,
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[derive(Debug, Default)]
struct NativeRelationState {
    next: u64,
    pending: BTreeMap<u64, Option<AdoptionKey>>,
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
impl duded::classify::ProcessRelation for NativeRelation {
    fn begin_spawn(&self) -> u64 {
        let mut state = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        state.next += 1;
        let slot = state.next;
        state.pending.insert(slot, None);
        slot
    }

    fn finish_spawn(&self, slot: u64, key: Option<&AdoptionKey>) -> bool {
        let mut state = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(current) = state.pending.get_mut(&slot) else {
            return false;
        };
        if let Some(key) = key {
            *current = Some(key.clone());
        } else {
            state.pending.remove(&slot);
        }
        true
    }

    fn published(&self, children: &[AdoptionKey]) -> Vec<(u64, usize)> {
        let mut state = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let published: Vec<_> = state
            .pending
            .iter()
            .filter_map(|(slot, key)| {
                let key = key.as_ref()?;
                children
                    .iter()
                    .position(|child| same_native_key(child, key))
                    .map(|index| (*slot, index))
            })
            .collect();
        for (slot, _) in &published {
            state.pending.remove(slot);
        }
        published
    }

    fn resolve(
        &self,
        _peer: u32,
        _pidversion: Option<u32>,
        _children: &[AdoptionKey],
    ) -> duded::classify::Relation {
        duded::classify::Relation::NoMatch
    }

    fn track_external(&self, _slot: u64) -> Box<dyn duded::classify::ExternalChild> {
        Box::new(NativeExternal(self.ptys.clone()))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[derive(Debug)]
struct NativeExternal(SharedPtyHost);

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
impl duded::classify::ExternalChild for NativeExternal {
    fn register(&mut self, child: duded::ports::PtyChild) -> io::Result<AdoptionKey> {
        self.0
            .native_key_for_child(child.pid)
            .ok_or_else(|| io::Error::other("mock PTY child has no exact native key"))
    }
}

pub(crate) fn native_resolver(ptys: &SharedPtyHost) -> duded::classify::Resolver {
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    {
        duded::classify::Resolver::processes(Arc::new(NativeRelation {
            ptys: ptys.clone(),
            slots: Mutex::new(NativeRelationState::default()),
        }))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = ptys;
        duded::classify::Resolver::fixed(Arc::new(duded::classify::NoWalk))
    }
}

/// A recording host whose shutdown performs the fake sweep and then reports unresolved ownership.
/// The error is deterministic so the daemon test can assert both the diagnostic and later teardown.
#[derive(Clone, Debug)]
pub(crate) struct ShutdownErrorHost {
    inner: SharedHost,
    errors: Arc<AtomicUsize>,
}

impl ShutdownErrorHost {
    pub(crate) fn new() -> Self {
        Self {
            inner: SharedHost::new(),
            errors: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn shutdowns(&self) -> usize {
        self.inner.shutdowns()
    }

    pub(crate) fn errors(&self) -> usize {
        self.errors.load(Ordering::SeqCst)
    }
}

impl ProcessHost for ShutdownErrorHost {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        self.inner.spawn(spec, install)
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.inner.kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.inner.alive(key)
    }

    fn boot_identity(&self) -> HostBoot {
        self.inner.boot_identity()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown()?;
        self.errors.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::other("injected unresolved ownership"))
    }
}

/// A shared real host that substitutes one deterministic post-exec EPIPE launch shape.
///
/// The production daemon still calls the real `ProcessHost` interface and maps its real
/// `CleanupUnconfirmed` result. The wrapper only makes the provider's launch spec reproducible for
/// the integration test and exposes ownership/call counts after the daemon takes the host by value.
#[derive(Clone)]
pub(crate) struct RetainedFailureHost {
    inner: Arc<Mutex<RealHost>>,
    spec: Arc<LaunchSpec>,
    spawns: Arc<AtomicUsize>,
}

impl RetainedFailureHost {
    pub(crate) fn new(spec: LaunchSpec) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RealHost::with_grace(Duration::from_millis(300)))),
            spec: Arc::new(spec),
            spawns: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn spawns(&self) -> usize {
        self.spawns.load(Ordering::SeqCst)
    }

    pub(crate) fn owned(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .owned()
    }
}

impl ProcessHost for RetainedFailureHost {
    fn spawn(&mut self, _spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        self.spawns.fetch_add(1, Ordering::SeqCst);
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .spawn(&self.spec, install)
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .alive(key)
    }

    fn boot_identity(&self) -> HostBoot {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .boot_identity()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .shutdown()
    }
}

#[derive(Clone)]
pub(crate) struct UnconfirmedHost {
    inner: SharedHost,
    refuse_next: Arc<std::sync::atomic::AtomicBool>,
}

impl UnconfirmedHost {
    /// A host that leaves its first launch unanswered when `unconfirmed_first`.
    pub(crate) fn new(unconfirmed_first: bool) -> Self {
        Self {
            inner: SharedHost::new(),
            refuse_next: Arc::new(std::sync::atomic::AtomicBool::new(unconfirmed_first)),
        }
    }

    pub(crate) fn with_native_evidence(unconfirmed_first: bool, journal: PathBuf) -> Self {
        Self {
            inner: SharedHost::with_native_evidence(journal),
            refuse_next: Arc::new(std::sync::atomic::AtomicBool::new(unconfirmed_first)),
        }
    }

    /// The fake underneath, for the counters.
    pub(crate) fn shared(&self) -> SharedHost {
        self.inner.clone()
    }

    /// How many children this host actually created.
    pub(crate) fn children(&self) -> usize {
        self.inner.pids().len()
    }
}

impl ProcessHost for UnconfirmedHost {
    fn spawn(&mut self, spec: &LaunchSpec, install: &ManagedInstall) -> SpawnResult {
        if self.refuse_next.swap(false, Ordering::SeqCst) {
            return SpawnResult::CleanupUnconfirmed {
                reason: io::Error::other("cleanup could not be confirmed for this launch"),
            };
        }
        self.inner.spawn(spec, install)
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.inner.kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.inner.alive(key)
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
}

/// A confirmed pre-exec failure followed by uncertainty, so a daemon test observes exactly one
/// `SpawnFailed` lifecycle and then parks without racing through the hop cap.
#[derive(Clone)]
pub(crate) struct ConfirmedFailureHost {
    inner: SharedHost,
    confirmed_next: Arc<std::sync::atomic::AtomicBool>,
}

impl ConfirmedFailureHost {
    pub(crate) fn new() -> Self {
        Self {
            inner: SharedHost::new(),
            confirmed_next: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }
}

impl ProcessHost for ConfirmedFailureHost {
    fn spawn(&mut self, _spec: &LaunchSpec, _install: &ManagedInstall) -> SpawnResult {
        if self.confirmed_next.swap(false, Ordering::SeqCst) {
            SpawnResult::NotLaunched {
                reason: io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "confirmed pre-exec failure",
                ),
            }
        } else {
            SpawnResult::CleanupUnconfirmed {
                reason: io::Error::other("stop after the confirmed control"),
            }
        }
    }

    fn kill(&mut self, key: &AdoptionKey) -> KillOutcome {
        self.inner.kill(key)
    }

    fn alive(&self, key: &AdoptionKey) -> Liveness {
        self.inner.alive(key)
    }

    fn boot_identity(&self) -> HostBoot {
        self.inner.boot_identity()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown()
    }
}

/// A predicate runner that answers from a script and counts sweeps.
///
/// It creates no process, which is the point for the tests about *verdicts*: what those assert is
/// what the loop does with an answer, and a real child would make them slow and platform-shaped.
/// The tests about a real child's lifetime use the real runner instead.
#[derive(Debug, Default)]
pub(crate) struct ScriptedRunner {
    answers: Mutex<VecDeque<PredicateOutcome>>,
    runs: AtomicUsize,
    sweeps: AtomicUsize,
    seen: Mutex<Vec<LaunchSpec>>,
}

impl ScriptedRunner {
    /// A runner that answers these outcomes in order, and then repeats the last one for ever.
    pub(crate) fn scripted(answers: impl IntoIterator<Item = PredicateOutcome>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().collect()),
            ..Self::default()
        }
    }

    /// How many predicates were run.
    pub(crate) fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }

    /// How many times the exit path swept.
    pub(crate) fn sweeps(&self) -> usize {
        self.sweeps.load(Ordering::SeqCst)
    }

    /// The specs it was handed, in order.
    pub(crate) fn seen(&self) -> Vec<LaunchSpec> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl PredicateRunner for ScriptedRunner {
    fn run(&self, spec: &LaunchSpec, _timeout: Duration) -> io::Result<PredicateOutcome> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(spec.clone());
        let mut answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(if answers.len() > 1 {
            answers.pop_front().expect("checked")
        } else {
            answers
                .front()
                .cloned()
                .unwrap_or(PredicateOutcome::Finished {
                    exit: Some(1),
                    output_tail: String::new(),
                    timed_out: false,
                })
        })
    }

    fn sweep(&self) {
        self.sweeps.fetch_add(1, Ordering::SeqCst);
    }
}

/// A predicate outcome that holds.
pub(crate) fn passes() -> PredicateOutcome {
    PredicateOutcome::Finished {
        exit: Some(0),
        output_tail: "ok".to_owned(),
        timed_out: false,
    }
}

/// A predicate outcome that does not.
pub(crate) fn fails() -> PredicateOutcome {
    PredicateOutcome::Finished {
        exit: Some(1),
        output_tail: "not ok".to_owned(),
        timed_out: false,
    }
}

/// The two directories a daemon run needs, and the settings over them.
///
/// Both are `TempDir`s held by this value: dropping a fixture removes both trees, and the
/// single-writer lock inside the state directory goes with it.
pub(crate) struct Fixture {
    pub(crate) state: tempfile::TempDir,
    pub(crate) runtime: tempfile::TempDir,
}

impl Fixture {
    /// Two fresh directories.
    pub(crate) fn new() -> Self {
        Self {
            state: tempfile::tempdir().expect("a state directory"),
            runtime: tempfile::tempdir().expect("a runtime directory"),
        }
    }

    /// Settings over this fixture's directories.
    ///
    /// `Fsync::Full` and not `None`: invariant 2 is what most of these tests are about, and a
    /// journal configured not to sync would make "persist before spawn" untestable by definition.
    pub(crate) fn settings(&self) -> Settings {
        settings_at(self.state.path(), self.runtime.path())
    }

    /// The journal directory the daemon writes into.
    pub(crate) fn journal_dir(&self) -> PathBuf {
        self.state.path().join(duded::boot::JOURNAL_DIR)
    }

    /// Every entry on the disk right now, in lsn order.
    ///
    /// **Panics rather than returning an empty vector on any read failure**, and that is not
    /// fastidiousness: three assertions whose entire content is an *absence* sit on this — no
    /// `PredicateResult` after a swept run, no second `TurnIssued` past the ceiling, no `Done` after
    /// a squat. A journal that could not be read would satisfy every one of them. The only tolerated
    /// empty is a journal directory that does not exist yet, which is a daemon that has not booted.
    /// **Every `BudgetDecision` read here is asserted to be a refusal**, which is the
    /// rule held over the whole suite rather than at one call site: `allowed` is permanently
    /// `false`, and the frame contract will not let the field be removed within frame
    /// `ver = 1`. This funnel is what makes that assertable — `records()` and `await_record()`
    /// both come through here, so a daemon that journaled an admitted turn would turn red on
    /// whichever test caught it rather than only on the one that went looking. The machine's own
    /// half of the same rule is in `dudes_supervisor::testing::Harness::step`.
    pub(crate) fn entries(&self) -> Vec<(Lsn, Entry)> {
        let dir = self.journal_dir();
        if !dir.exists() {
            return Vec::new();
        }
        let entries: Vec<(Lsn, Entry)> = Reader::new(&dir)
            .iter_from(Lsn::ZERO)
            .unwrap_or_else(|error| panic!("reading {}: {error}", dir.display()))
            .map(|item| item.unwrap_or_else(|error| panic!("reading {}: {error}", dir.display())))
            .collect();
        for (lsn, entry) in &entries {
            if let Record::BudgetDecision { allowed, .. } = &entry.record {
                assert!(
                    !allowed,
                    "a BudgetDecision is journaled on a refusal and never on an \
                     admission, so `allowed` is permanently false; {lsn:?} carries {:?}",
                    entry.record
                );
            }
        }
        entries
    }

    /// Every record on the disk right now.
    pub(crate) fn records(&self) -> Vec<Record> {
        self.entries()
            .into_iter()
            .map(|(_, entry)| entry.record)
            .collect()
    }

    /// Block until `f` finds what it is looking for in the journal, or fail.
    pub(crate) fn await_record<T>(&self, what: &str, f: impl Fn(&Record) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(found) = self.records().iter().find_map(&f) {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "no {what} reached the journal within 10s; records: {:?}",
                self.records().iter().map(Record::kind).collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The turn a `TurnIssued` named, once one is on the disk.
    pub(crate) fn await_turn(&self) -> TurnId {
        self.await_record("TurnIssued", |record| match record {
            Record::TurnIssued { turn, .. } => Some(*turn),
            _ => None,
        })
    }
}

/// The same test settings over caller-owned paths, for a helper daemon process that must leave the
/// directories behind when the process is killed abruptly.
pub(crate) fn settings_at(state: &Path, runtime: &Path) -> Settings {
    Settings {
        state_dir: state.to_path_buf(),
        runtime_dir: runtime.join("run"),
        socket_name: SOCKET_NAME.to_owned(),
        journal: JournalOptions {
            fsync: Fsync::Full,
            ..JournalOptions::default()
        },
        floor_usd_per_day: Some(Usd::from_dollars(100)),
        per_turn_floor: Spend {
            usd: Usd::from_micros(1),
            tokens: Tokens::new(1),
            work_tokens: Tokens::new(1),
        },
        enrollment: duded::floor_actions::Enrollment::default(),
        floor_answer_enabled: false,
        floor_hire_enabled: true,
        floor_control: duded::settings::FloorControl::Off,
        floor_recovery_enabled: false,
        floor_terminal_enabled: false,
        floor_git_enabled: false,
        floor_diff_enabled: false,
        floor_board_enabled: false,
        // Off by default in the shared test harness, so the many existing tests that never
        // exercise memory condense keep their exact deterministic-only behavior unchanged; tests
        // of the model path turn this on explicitly over a scripted runner and a resolvable
        // stand-in "claude" program, never a real one.
        condense_model_enabled: false,
        condense_model: None,
        operator_config: None,
        breaker: BreakerConfig::default(),
        daemon_env: vec![EnvVar::new("PATH", stand_in_cli_path(runtime))],
        predicate_env: vec![EnvVar::new("PATH", "/usr/bin:/bin")],
        predicate_path: Some(std::ffi::OsString::from("/usr/bin:/bin")),
        git_checkout_deadline: duded::worktree::git::CHECKOUT_DEADLINE,
        max_connections: NonZeroUsize::new(8).expect("not zero"),
        // Far longer than any test: a beat that fired mid-assertion would make a test about
        // ordering a test about timing.
        beat: Duration::from_hours(1),
        floor: None,
        hook_command: Some(stand_in_hook(runtime)),
        daemon_exe: None,
    }
}

/// A managed child's `PATH` whose first entry holds a runnable stand-in `claude`, so the daemon
/// resolves the provider's program and launches it. The fake hosts these settings serve never run it.
fn stand_in_cli_path(dir: &Path) -> std::ffi::OsString {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("a stand-in bin directory");
    let cli = bin.join(if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    });
    std::fs::write(&cli, b"").expect("a stand-in managed CLI");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755))
            .expect("a runnable stand-in");
    }
    std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
        .expect("a joinable PATH")
}

/// A file where `dudes-hook` would be, so the daemon names it and an attach is admitted. The fake
/// PTY hosts these settings serve never run it.
fn stand_in_hook(dir: &Path) -> PathBuf {
    let path = dir.join("dudes-hook");
    std::fs::write(&path, b"").expect("a stand-in hook command");
    path
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

/// A [`PeerResolver`] that answers one scripted column for every peer.
///
/// # Why this exists at all, when the daemon needs no resolver seam
///
/// Most existing fixtures deliberately select `classify::NoWalk`, pinning their connections to
/// the third peer-classification column. A test that needs another column selects it here through
/// `Seams.resolver` — a choice of **column** and never of **row**: `PeerClassifier::row` still
/// reads only what the call presents, the daemon's own credential and token projection.
///
/// **Written here rather than borrowed, and that is a convention rather than a compiler outcome.**
/// `dudes-rpc`'s `FakeResolver` is behind that crate's `testing` feature, which `bins/duded` does
/// not enable — but a dev-dependency's features unify across a workspace build, and
/// `bins/dudes/Cargo.toml` dev-depends on `dudes-rpc` with `testing` *and* on `duded`. Measured,
/// by planting `pub use dudes_rpc::testing::AdmitAll as _Probe;`: `cargo build -p duded` fails
/// with `error[E0432]: unresolved import`, while `cargo nextest run --workspace` compiles clean.
///
/// So nothing stops a future maintainer swapping this type for `FakeResolver` under the workspace
/// test run. What holds here is `tests/host_seam.rs`'s source scan, not the compiler — see that
/// file for which graph each gate covers. Keeping the fake local is what keeps the scan able to
/// see it.
#[derive(Debug)]
pub(crate) struct ScriptedColumn {
    /// The answer, settable because a column naming an agent cannot be written before that agent
    /// is hired — and the seam is filled in before `boot` runs. Read once per connection, so a
    /// change takes effect for the next connection and never mid-connection.
    provenance: std::sync::Mutex<dudes_rpc::auth::PeerProvenance>,
    asked: std::sync::Mutex<usize>,
}

impl ScriptedColumn {
    /// A resolver answering `provenance` for every peer.
    pub(crate) fn new(provenance: dudes_rpc::auth::PeerProvenance) -> Arc<Self> {
        Arc::new(Self {
            provenance: std::sync::Mutex::new(provenance),
            asked: std::sync::Mutex::new(0),
        })
    }

    /// Answer `provenance` from the next connection on.
    pub(crate) fn set(&self, provenance: dudes_rpc::auth::PeerProvenance) {
        *self.provenance.lock().expect("the fixture's lock") = provenance;
    }

    /// How many connections it has been asked about. One per connection, never one per call.
    pub(crate) fn asked(&self) -> usize {
        *self.asked.lock().expect("the fixture's lock")
    }
}

impl dudes_rpc::auth::PeerResolver for ScriptedColumn {
    fn resolve(&self, _peer: &dudes_rpc::auth::PeerIdentity) -> dudes_rpc::auth::PeerResolution {
        *self.asked.lock().expect("the fixture's lock") += 1;
        dudes_rpc::auth::PeerResolution::Resolved(
            *self.provenance.lock().expect("the fixture's lock"),
        )
    }
}

/// The seams a test daemon runs on: a chosen host and predicate runner, a test clock and a counting
/// rng.
pub(crate) fn seams<H: ProcessHost, P: PredicateRunner>(
    settings: &Settings,
    host: H,
    predicates: Arc<P>,
    clock: Arc<TestClock>,
    witness: Witness,
) -> Seams<H, P, TestClock, FixtureRng> {
    seams_with(
        settings,
        host,
        predicates,
        clock,
        witness,
        Arc::new(duded::classify::NoWalk),
    )
}

/// [`seams`] with a chosen peer-classification column, for the tests that need one the daemon cannot produce.
pub(crate) fn seams_with<H: ProcessHost, P: PredicateRunner>(
    settings: &Settings,
    host: H,
    predicates: Arc<P>,
    clock: Arc<TestClock>,
    witness: Witness,
    resolver: Arc<dyn dudes_rpc::auth::PeerResolver>,
) -> Seams<H, P, TestClock, FixtureRng> {
    Seams {
        host,
        pty_host: Box::new(duded::ports::ClosedPtyHost),
        predicates,
        clock,
        rng: FixtureRng::from_rng(settings, FakeRng::new()),
        // `seams` deliberately passes the fixed `NoWalk` column used by these fixtures. Tests of
        // production process resolution pass a concrete test-local adapter to
        // `Resolver::processes`; a test that needs another fixed column passes
        // [`ScriptedColumn`]. No seam accepts a permissive authorizer.
        resolver: duded::classify::Resolver::fixed(resolver),
        // The registry the shipped daemon builds: the default Claude adapter. A test that needs
        // a provider *absent* asks for it here.
        adapters: Adapters::new([
            Box::new(ClaudeAdapter::new(ClaudeConfig::default())) as Box<dyn Adapter>
        ]),
        // A fake host opens no pipe, so nothing ever reaches this sink here; it is supplied
        // because the daemon's executor registers every spawned child against one. The test that
        // drives a real child through it is `bins/duded/tests/managed_turn.rs`.
        children: duded::ChildEnds::new(),
        witness,
    }
}

/// `agent.hire` params for a Claude-driven agent in `cwd`.
pub(crate) fn hire_params(cwd: &Path) -> HireParams {
    HireParams {
        character: None,
        accent: None,
        name: "Hector".to_owned(),
        role: "infra".to_owned(),
        department: "infra".to_owned(),
        provider: Provider::Claude,
        cwd: cwd.to_path_buf(),
        budget: AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        trusted: false,
        checkout: None,
        from: None,
        mcp_servers: Vec::new(),
    }
}

/// `System32\<program>` under `system_root`. Windows has no `/usr/bin`, so the checks these
/// tests set come from there.
pub(crate) fn system32_program(system_root: &Path, program: &str) -> PathBuf {
    system_root.join("System32").join(program)
}

fn check_program(unix: &str, windows: &str) -> String {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").expect("Windows SystemRoot");
        system32_program(Path::new(&root), windows)
            .into_os_string()
            .into_string()
            .expect("a UTF-8 system path")
    } else {
        unix.to_owned()
    }
}

/// A check program that exits 0 and that `goal.set` resolves on every platform the suite runs on.
pub(crate) fn true_program() -> &'static str {
    static PROGRAM: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| check_program("/usr/bin/true", "whoami.exe"));
    &PROGRAM
}

/// A check program that exits non-zero, `findstr.exe` with no pattern on Windows, and that
/// `goal.set` resolves on every platform the suite runs on.
pub(crate) fn false_program() -> &'static str {
    static PROGRAM: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| check_program("/usr/bin/false", "findstr.exe"));
    &PROGRAM
}

/// `goal.set` params whose predicate is `argv`, declared as a check that may already pass, so goal
/// set does not run it first. A test about that run builds its own params.
pub(crate) fn goal_params(agent: AgentId, argv: &[&str]) -> GoalSetParams {
    GoalSetParams {
        agent,
        text: "make the tests pass".to_owned(),
        predicate: Predicate::Command {
            argv: argv.iter().map(|part| (*part).to_owned()).collect(),
            timeout_s: 600,
        },
        budget: GoalBudget {
            usd: Some(Usd::from_dollars(5)),
            tokens: None,
        },
        hop_cap: 8,
        check: None,
        may_already_pass: true,
    }
}

/// A named command check's revision, recomputed here from its id, label and command rather than
/// taken from the daemon.
pub(crate) fn check_revision(id: &str, label: &str, argv: &[&str], timeout_s: u64) -> String {
    check_revision_as(id, label, argv, timeout_s, false)
}

/// [`check_revision`] for a check that may already pass when `may_already_pass`, which frames that
/// declaration under its own domain.
pub(crate) fn check_revision_as(
    id: &str,
    label: &str,
    argv: &[&str],
    timeout_s: u64,
    may_already_pass: bool,
) -> String {
    use sha2::{Digest as _, Sha256};
    let mut fields = vec![
        ("check_id".to_owned(), id.to_owned()),
        ("label".to_owned(), label.to_owned()),
        ("kind".to_owned(), "command".to_owned()),
        ("argc".to_owned(), argv.len().to_string()),
    ];
    for (i, arg) in argv.iter().enumerate() {
        fields.push((format!("argv_{i}"), (*arg).to_owned()));
    }
    fields.push(("timeout_s".to_owned(), timeout_s.to_string()));
    let mut hasher = Sha256::new();
    if may_already_pass {
        fields.push(("may_already_pass".to_owned(), "true".to_owned()));
        hasher.update(b"dudes.goal-check.v2\0");
    } else {
        hasher.update(b"dudes.goal-check.v1\0");
    }
    for (name, value) in fields {
        hasher.update(u16::try_from(name.len()).unwrap().to_be_bytes());
        hasher.update(name.as_bytes());
        hasher.update(u32::try_from(value.len()).unwrap().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .fold(String::new(), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// A goal with a ceiling of `usd` dollars, for the budget tests.
pub(crate) fn goal_params_with_budget(agent: AgentId, argv: &[&str], usd: Usd) -> GoalSetParams {
    GoalSetParams {
        budget: GoalBudget {
            usd: Some(usd),
            tokens: None,
        },
        ..goal_params(agent, argv)
    }
}

/// The goal a `GoalSet` named, from the journal.
pub(crate) fn goal_of(records: &[Record]) -> Option<GoalId> {
    records.iter().rev().find_map(|record| match record {
        Record::GoalSet { goal, .. } => Some(goal.id),
        _ => None,
    })
}

/// Whether the process at `pid` has finished — exited, or exited and not yet reaped.
///
/// `ESRCH` is the only absence result. `EPERM` and every other query failure leave this control
/// unable to say the process finished.
#[cfg(unix)]
pub(crate) fn finished(pid: u32) -> bool {
    use rustix::io::Errno;
    use rustix::process::Pid;

    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return true;
    };
    matches!(rustix::process::test_kill_process(pid), Err(Errno::SRCH))
}

/// Block until the process at `pid` is finished, or fail.
#[cfg(unix)]
pub(crate) fn await_finished(pid: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !finished(pid) {
        assert!(
            Instant::now() < deadline,
            "{what}: pid {pid} was still running after 10s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A daemon serving on a thread of its own, and the handle that drives it.
///
/// `Daemon::run` blocks, and every command the transport's `Control` port makes goes *through* the
/// loop — so a test that called `Handle::ask` before starting the loop would wait for an answer
/// nobody is there to give. This is the shape every driving test takes: start, drive, stop, join.
pub(crate) struct Running {
    /// How the test reaches the loop, exactly as the accept thread and the reaper do.
    pub(crate) handle: duded::Handle,
    thread: Option<std::thread::JoinHandle<anyhow::Result<duded::ExitCause>>>,
}

impl Running {
    /// Ask the daemon to stop for `cause`, and wait for its exit sequence to finish.
    pub(crate) fn stop(&mut self, cause: duded::ExitCause) -> duded::ExitCause {
        self.handle.stop(cause);
        self.join()
    }

    /// Wait for the loop to end on its own.
    pub(crate) fn join(&mut self) -> duded::ExitCause {
        self.thread
            .take()
            .expect("the daemon thread is joined once")
            .join()
            .expect("the daemon thread did not panic")
            .expect("the daemon ran")
    }
}

impl Drop for Running {
    /// A test that failed an assertion still takes its daemon down, so no accept loop outlives it.
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.handle.stop(duded::ExitCause::Ended);
            let _ = self.thread.take().expect("checked").join();
        }
    }
}

/// Start a booted daemon on a thread and hand back its handle.
pub(crate) fn start<H, P, C, R>(daemon: duded::Daemon<H, P, C, R>) -> Running
where
    H: ProcessHost + Send + 'static,
    P: PredicateRunner,
    C: Clock + Send + Sync + 'static,
    R: dudes_supervisor::Rng + Send + 'static,
{
    let handle = daemon.handle();
    let thread = std::thread::Builder::new()
        .name("duded-under-test".to_owned())
        .spawn(move || daemon.run())
        .expect("the daemon thread");
    Running {
        handle,
        thread: Some(thread),
    }
}

/// Hire an agent through the loop, exactly as `agent.hire` does.
pub(crate) fn hire(handle: &duded::Handle, cwd: &Path) -> AgentId {
    match handle
        .ask(duded::ports::Ask::Hire(Box::new(hire_params(cwd))))
        .expect("the hire")
    {
        duded::ports::Answered::Hired(result) => result.agent,
        other => panic!("agent.hire answered {other:?}"),
    }
}

/// Give an agent a goal through the loop, exactly as `goal.set` does.
pub(crate) fn set_goal(handle: &duded::Handle, params: GoalSetParams) -> GoalId {
    match handle
        .ask(duded::ports::Ask::SetGoal(Box::new(params)))
        .expect("the goal")
    {
        duded::ports::Answered::GoalSet(result) => result.goal,
        other => panic!("goal.set answered {other:?}"),
    }
}

pub(crate) fn end_turn(handle: &duded::Handle, turn: TurnId) {
    handle.input(dudes_supervisor::Input::ChildExited {
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: dudes_proto::ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: dudes_proto::TurnOutcome::Completed,
        last_message: None,
    });
}

pub(crate) fn end_turn_with_native(
    fixture: &Fixture,
    _handle: &duded::Handle,
    host: &SharedHost,
    turn: TurnId,
) {
    end_turn_with_native_payload(
        fixture,
        host,
        dudes_supervisor::Input::ChildExited {
            final_reply: None,
            elapsed_ms: None,
            turn,
            exit: dudes_proto::ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: None,
            model_usage: BTreeMap::new(),
            outcome: dudes_proto::TurnOutcome::Completed,
            last_message: None,
        },
    );
}

pub(crate) fn end_turn_with_native_payload(
    fixture: &Fixture,
    host: &SharedHost,
    input: dudes_supervisor::Input,
) {
    let dudes_supervisor::Input::ChildExited { turn, .. } = &input else {
        panic!("expected an exact mock child completion payload")
    };
    let turn = *turn;
    let key = fixture.await_record(
        "exact managed spawn before mock reap",
        |record| match record {
            Record::TurnSpawned {
                turn: owned,
                pid,
                start_time,
                boot,
                ..
            } if *owned == turn => Some(AdoptionKey {
                pid: *pid,
                start_time: *start_time,
                boot: boot.clone(),
            }),
            _ => None,
        },
    );
    host.mark_native_reaped(&key);
    host.finish_owned_child(&key, input);
    fixture.await_record("exact managed end after mock reap", |record| {
        matches!(record, Record::TurnEnded { turn: owned, .. } if *owned == turn).then_some(())
    });
    host.await_native_acknowledged(&key);
    assert!(host.native_settlement_written(&key));
}

/// Block until `path` holds a parsable number, and return it.
pub(crate) fn await_number(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "nothing readable appeared at {} within 10s",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Canonical RFC 4648 §4 base64, spelled out here rather than taken from the crate the daemon
/// decodes with: an encoder that shared the decoder's bug would hide it from every test below.
pub(crate) fn base64_standard(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let trio = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let packed = (u32::from(trio[0]) << 16) | (u32::from(trio[1]) << 8) | u32::from(trio[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(char::from(
                    ALPHABET[((packed >> (18 - 6 * index)) & 0x3f) as usize],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn read_http_head(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read as _;
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("response head");
        head.push(byte[0]);
    }
    String::from_utf8(head).expect("ASCII head")
}

fn read_http_body(stream: &mut std::net::TcpStream, head: &str) -> Vec<u8> {
    use std::io::Read as _;
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .expect("response length")
        .parse()
        .expect("decimal length");
    let mut body = vec![0; length];
    stream.read_exact(&mut body).expect("response body");
    body
}

/// A signed-in Floor page's cookie and CSRF nonce: a bootstrap issued over the operator socket,
/// then exchanged on the Floor listener.
pub(crate) fn floor_session(settings: &Settings, addr: std::net::SocketAddr) -> (String, String) {
    use std::io::Write as _;
    let credential = std::fs::read_to_string(
        settings
            .runtime_dir
            .join(dudes_rpc::OPERATOR_CREDENTIAL_NAME),
    )
    .expect("operator credential");
    let reply = dudes_rpc::Client::connect(settings.socket())
        .expect("daemon socket")
        .call_as_operator(
            dudes_proto::rpc::Method::FloorSessionIssue,
            None,
            dudes_proto::rpc::cred::OperatorCredential::new(credential).expect("credential"),
        )
        .expect("issue response");
    let issued: dudes_proto::rpc::floor::SessionIssueResult =
        serde_json::from_value(reply.result.expect("issue succeeded")).expect("typed issue");
    let host = addr.to_string();
    let mut stream = std::net::TcpStream::connect(addr).expect("Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let request = format!(
        "POST /floor/session HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nContent-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n\r\n{}",
        issued.bootstrap.len(),
        issued.bootstrap
    );
    stream.write_all(request.as_bytes()).expect("exchange");
    let head = read_http_head(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    let cookie = head
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: dudes_floor_session="))
        .and_then(|value| value.split(';').next())
        .expect("cookie")
        .to_owned();
    let body = read_http_body(&mut stream, &head);
    let csrf = serde_json::from_slice::<serde_json::Value>(&body).expect("JSON")["csrf"]
        .as_str()
        .expect("csrf")
        .to_owned();
    (cookie, csrf)
}

/// One authenticated Floor action POST: the response head and its JSON body, `Null` when empty.
pub(crate) fn floor_post(
    addr: std::net::SocketAddr,
    path: &str,
    (cookie, csrf): (&str, &str),
    body: &serde_json::Value,
) -> (String, serde_json::Value) {
    use std::io::Write as _;
    let body = serde_json::to_vec(body).expect("request JSON");
    let host = addr.to_string();
    let mut stream = std::net::TcpStream::connect(addr).expect("Floor listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: dudes_floor_session={cookie}\r\nX-Dudes-CSRF: {csrf}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("request head");
    stream.write_all(&body).expect("request body");
    let head = read_http_head(&mut stream);
    let body = read_http_body(&mut stream, &head);
    let value = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).expect("response JSON")
    };
    (head, value)
}
