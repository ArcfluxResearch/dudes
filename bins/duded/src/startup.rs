//! The seven core startup steps, optional Floor steps, and the recorder for both orders.
//!
//! # Two mechanisms, and neither covers the whole order
//!
//! **Half the order is structural, and [`crate::boot`]'s header is where which half is enumerated.**
//! Three of the six links are carried by the types and a reorder across them does not compile; the
//! other three are ordering by statement. This module deliberately does **not** restate the
//! enumeration — a rule stated twice is how drift starts.
//!
//! **The recorder is what the untyped half rests on, and it is weaker than a type.** Each step
//! pushes its [`StartupStep`] the moment it succeeds, and a test reads the sequence back. Where the
//! link *is* typed, the runtime control for the assertion is achieved by moving a `record` call
//! rather than by moving the logic — so the recorder is not what enforces those, and is not
//! presented as if it were. What it buys everywhere is a second guarantee: each step's
//! on-disk side effect can be asserted beside the step that claims to have produced it, and a
//! second daemon's recorder holding **zero** steps is what proves the refusal came from step 1 and
//! not from a socket path that happened to be occupied.

use std::sync::{Arc, Mutex, PoisonError};

/// One step of the normative startup order.
///
/// The order these are declared in is the order they must happen in, and
/// [`StartupStep::ORDER`] is that sequence written once so a test does not keep a second copy.
///
/// **Deliberately not `#[non_exhaustive]`**: a step added without every reader noticing is a step
/// whose place in the order nobody chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StartupStep {
    /// The single-writer lock on the state directory is held. Invariant 1: a second daemon on the
    /// same state directory is refused here, before anything is opened.
    Lock,
    /// The journal is open for writing, its torn tail repaired.
    JournalOpen,
    /// Every entry has been folded into the projection and into the supervisor's world. Binding
    /// before this would serve a caller a projection that is behind the log.
    Replay,
    /// The optional Floor release manifest and every admitted file are retained and verified.
    FloorPrepared,
    /// The control socket is bound inside the runtime directory.
    Bind,
    /// The operator credential is minted, once per daemon run. Binding without
    /// it serves an operator surface nothing can authenticate against.
    MintCredential,
    /// The reaper is running. Deciding before this would issue a turn beside a child nobody has
    /// probed.
    ReaperStart,
    /// The first `decide` has happened: `Input::Started` has been handled.
    FirstDecide,
    /// The optional Floor listener acknowledged its serving thread and is ready.
    FloorReady,
}

impl StartupStep {
    /// The normative order, written once.
    pub const ORDER: [Self; 7] = [
        Self::Lock,
        Self::JournalOpen,
        Self::Replay,
        Self::Bind,
        Self::MintCredential,
        Self::ReaperStart,
        Self::FirstDecide,
    ];

    /// The startup sequence when the static Floor is enabled.
    pub const FLOOR_ORDER: [Self; 9] = [
        Self::Lock,
        Self::JournalOpen,
        Self::Replay,
        Self::FloorPrepared,
        Self::Bind,
        Self::MintCredential,
        Self::ReaperStart,
        Self::FirstDecide,
        Self::FloorReady,
    ];
}

/// One step of the exit sequence.
///
/// The order matters as much as the startup order's does. Attached PTYs are closed first so each
/// confirmed absence can become a durable system detach before the flushed witness. Predicates and
/// managed children are then swept in one sequence so no child outlives the daemon on any exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TeardownStep {
    /// Every attached PTY has been closed or retained as unresolved after the full host sweep.
    PtyHostShutdown,
    /// Every queued effect and confirmed PTY detach has been journaled and projected.
    JournalFlushed,
    /// `PredicateRunner::sweep` has run: no goal predicate is still in flight.
    PredicatesSwept,
    /// `ProcessHost::shutdown` has returned after attempting every agent child. Addressable
    /// processes have been stopped where authority allowed; unresolved ownership may remain after
    /// a reported error (invariant 10).
    HostShutdown,
    /// The optional Floor listener has stopped admission and its worker completed or hit its bound.
    FloorStopped,
    /// The accept loop has stopped and its connections are halted.
    TransportStopped,
}

impl TeardownStep {
    /// The normative order, written once.
    pub const ORDER: [Self; 5] = [
        Self::PtyHostShutdown,
        Self::JournalFlushed,
        Self::PredicatesSwept,
        Self::HostShutdown,
        Self::TransportStopped,
    ];

    /// The teardown sequence when the static Floor was enabled.
    pub const FLOOR_ORDER: [Self; 6] = [
        Self::PtyHostShutdown,
        Self::JournalFlushed,
        Self::PredicatesSwept,
        Self::HostShutdown,
        Self::FloorStopped,
        Self::TransportStopped,
    ];
}

/// What a daemon run recorded about one of its phases.
///
/// Cheap to clone and shared with the boot chain, because the chain hands its product on by value
/// and a recorder threaded through by value would be one more thing every step's signature had to
/// carry. A test holds one, boots a daemon with it, and reads [`Recorder::steps`].
#[derive(Debug)]
pub struct Recorder<S>(Arc<Mutex<Vec<S>>>);

// Derived `Clone` would demand `S: Clone`, which is not what is being cloned: the handle is.
impl<S> Clone for Recorder<S> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<S> Default for Recorder<S> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
}

impl<S: Clone> Recorder<S> {
    /// A recorder that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Note that `step` has just succeeded.
    pub fn record(&self, step: S) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(step);
    }

    /// The steps recorded so far, in order.
    #[must_use]
    pub fn steps(&self) -> Vec<S> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The three recorders, carried together because every construction wants them.
#[derive(Clone, Debug, Default)]
pub struct Witness {
    /// The seven startup steps.
    pub startup: Recorder<StartupStep>,
    /// The exit sequence.
    pub teardown: Recorder<TeardownStep>,
    /// What the reaper found at the control socket's path, one entry per beat.
    ///
    /// **This is the only observable the socket-identity check has**, and it exists because without
    /// one the check could not fail. The socket-identity detection produces no `Input`, no `Record` and no
    /// return value — it logs, deliberately, because a rung is per-agent and a squatted socket has
    /// no agent (see [`crate::socket`]). A log with no subscriber is not a consequence a test can
    /// read, so deleting the reaper's call, or making the capture at bind answer `None`, left the
    /// whole suite green. Both are red against this.
    ///
    /// It records the daemon's **own** captured identity re-compared against the path — not a value
    /// a test built for itself, which would exercise `SocketIdentity` and nothing else.
    pub sockets: Recorder<crate::socket::Standing>,
    #[cfg(any(test, feature = "testing"))]
    replay_fold_refusal: Arc<Mutex<Option<ReplayFoldRefusal>>>,
}

#[cfg(any(test, feature = "testing"))]
#[derive(Debug)]
struct ReplayFoldRefusal {
    lsn: dudes_types::Lsn,
    reason: dudes_proto::recovery_state::RecoveryStateError,
}

#[cfg(any(test, feature = "testing"))]
impl Witness {
    /// Inject one labelled replay-fold transport failure after strict Reader acceptance.
    #[doc(hidden)]
    pub fn refuse_replay_fold_once(
        &self,
        lsn: dudes_types::Lsn,
        reason: dudes_proto::recovery_state::RecoveryStateError,
    ) {
        *self
            .replay_fold_refusal
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(ReplayFoldRefusal { lsn, reason });
    }

    pub(crate) fn take_replay_fold_refusal(
        &self,
        lsn: dudes_types::Lsn,
    ) -> Option<dudes_proto::recovery_state::RecoveryStateError> {
        let mut refusal = self
            .replay_fold_refusal
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if refusal.as_ref().is_some_and(|failure| failure.lsn == lsn) {
            refusal.take().map(|failure| failure.reason)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recorder keeps what it was given, in the order it was given it — the property the
    /// startup test's assertion rests on. Two arms: an empty recorder reads empty (which is what a
    /// refused second daemon looks like), and a recorded sequence reads back whole.
    #[test]
    fn the_recorder_keeps_the_order_it_was_given() {
        let recorder: Recorder<StartupStep> = Recorder::new();
        assert!(
            recorder.steps().is_empty(),
            "control: a daemon that got nowhere records nothing"
        );
        recorder.record(StartupStep::Lock);
        recorder.record(StartupStep::JournalOpen);
        assert_eq!(
            recorder.steps(),
            vec![StartupStep::Lock, StartupStep::JournalOpen]
        );
    }

    /// `FLOOR_ORDER` is the complete declaration order; `ORDER` is its disabled-Floor subsequence.
    #[test]
    fn the_declared_order_has_every_step_once() {
        let mut sorted = StartupStep::FLOOR_ORDER;
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            StartupStep::FLOOR_ORDER,
            "FLOOR_ORDER must be the enum's own declaration order"
        );
        let unique: std::collections::BTreeSet<_> = StartupStep::FLOOR_ORDER.iter().collect();
        assert_eq!(
            unique.len(),
            StartupStep::FLOOR_ORDER.len(),
            "a step appears twice"
        );
        let core: Vec<_> = StartupStep::FLOOR_ORDER
            .into_iter()
            .filter(|step| !matches!(step, StartupStep::FloorPrepared | StartupStep::FloorReady))
            .collect();
        assert_eq!(core, StartupStep::ORDER);
    }
}
