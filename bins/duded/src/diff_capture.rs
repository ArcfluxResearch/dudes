//! Turns the bounded checkout reader's observations into the baseline and review-stamp payloads
//! a ticket round records.
//!
//! Every reader refusal becomes a finite unavailable outcome. Nothing here guesses a baseline,
//! reuses one across tickets, or writes a record: the caller appends the payload and decides
//! what a failed append means.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

use dudes_proto::DiffRefusal;
use dudes_proto::diff_stamp::{
    BaselineOutcome, BaselinePayload, CaptureInterval, DigestHex, STAMP_PAYLOAD_VERSION,
    SamplerVersions, StampOutcome, StampPayload, StampPayloadError, TicketBinding,
};
use dudes_types::{Lsn, Timestamp};

use crate::git_read::Tree;
use crate::git_read::diff::{
    BaselineCapture, DiffReader, ReviewCapture, RootIdentity, SAMPLER_VERSION, TEXT_POLICY_VERSION,
};

/// The one slot every capture and every Floor diff read takes in turn.
///
/// A capture waits for it; a Floor read never does. While any capture waits, a Floor read is
/// refused as busy, so a capture waits for at most the one Floor read already in flight.
#[derive(Default)]
pub struct ReadSlot {
    lock: Mutex<()>,
    waiting: AtomicUsize,
}

/// A capture's place in line, held while it waits for the slot.
pub struct Waiting<'a>(&'a ReadSlot);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ReadSlot {
    /// Announce a capture, so Floor reads give way until the returned value is dropped.
    #[must_use]
    pub fn announce(&self) -> Waiting<'_> {
        self.waiting.fetch_add(1, Ordering::AcqRel);
        Waiting(self)
    }

    /// Wait for the slot on behalf of a capture.
    pub fn capture(&self) -> MutexGuard<'_, ()> {
        let waiting = self.announce();
        let guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        drop(waiting);
        guard
    }

    /// The slot for a Floor read, or `busy` while a read or capture holds it or a capture waits.
    ///
    /// # Errors
    /// `busy` when the slot is not free at once.
    pub fn floor(&self) -> Result<MutexGuard<'_, ()>, DiffRefusal> {
        if self.waiting.load(Ordering::Acquire) > 0 {
            return Err(DiffRefusal::Busy);
        }
        match self.lock.try_lock() {
            Ok(guard) => Ok(guard),
            Err(TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => Err(DiffRefusal::Busy),
        }
    }

    /// How many captures wait for the slot.
    #[must_use]
    #[doc(hidden)]
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Acquire)
    }
}

/// Samples a recorded checkout and describes the result as bounded payloads.
pub struct DiffCapture {
    reader: Arc<DiffReader>,
    slot: Arc<ReadSlot>,
    clock: Box<dyn Fn() -> Timestamp + Send + Sync>,
}

impl DiffCapture {
    /// Use `reader` for observations and `clock` for the capture interval.
    #[must_use]
    pub fn new(reader: Arc<DiffReader>, clock: Box<dyn Fn() -> Timestamp + Send + Sync>) -> Self {
        Self {
            reader,
            slot: Arc::default(),
            clock,
        }
    }

    /// The reader this service samples with, for the Floor's diff route to use.
    #[must_use]
    pub fn reader(&self) -> Arc<DiffReader> {
        Arc::clone(&self.reader)
    }

    /// The slot every capture and every Floor diff read takes in turn: at most one of them reads
    /// a checkout at a time, daemon-wide.
    #[must_use]
    pub fn slot(&self) -> Arc<ReadSlot> {
        Arc::clone(&self.slot)
    }

    /// Observe the round-start state of `tree` for `binding`.
    ///
    /// # Errors
    /// Fails only if the payload cannot be encoded, which fixed-width fields cannot cause.
    pub fn baseline(
        &self,
        tree: &Tree,
        binding: TicketBinding,
        cancelled: Arc<AtomicBool>,
    ) -> Result<BaselinePayload, StampPayloadError> {
        let _one_at_a_time = self.slot.capture();
        let started_at = (self.clock)();
        let outcome = match self.reader.capture_baseline(tree, cancelled) {
            Ok(capture) => BaselineOutcome::Captured {
                device: capture.root.device,
                inode: capture.root.inode,
                head: capture.head,
                preexisting_tracked: capture.preexisting_tracked,
            },
            Err(reason) => BaselineOutcome::Unavailable { reason },
        };
        BaselinePayload::new(binding, self.interval(started_at), versions(), outcome)
    }

    /// Sample the review scope of `tree` after the predicate result at `check_lsn`, against the
    /// baseline recorded at `baseline_lsn`.
    ///
    /// # Errors
    /// Fails only if the payload cannot be encoded, which fixed-width fields cannot cause.
    pub fn stamp(
        &self,
        tree: &Tree,
        baseline: &BaselinePayload,
        baseline_lsn: Lsn,
        binding: TicketBinding,
        check_lsn: Lsn,
        cancelled: Arc<AtomicBool>,
    ) -> Result<StampPayload, StampPayloadError> {
        let _one_at_a_time = self.slot.capture();
        let started_at = (self.clock)();
        let outcome = match self.sample(tree, baseline, binding, cancelled) {
            Ok(capture) => sampled(&capture),
            Err(reason) => StampOutcome::Unavailable { reason },
        };
        StampPayload::new(
            binding,
            baseline_lsn,
            check_lsn,
            self.interval(started_at),
            versions(),
            outcome,
        )
    }

    /// A baseline for a round whose checkout cannot be read at all. Built directly: its fields
    /// are fixed-width, so no size check can fail.
    #[must_use]
    pub fn baseline_unavailable(
        &self,
        binding: TicketBinding,
        reason: DiffRefusal,
    ) -> BaselinePayload {
        let at = (self.clock)();
        BaselinePayload {
            v: STAMP_PAYLOAD_VERSION,
            binding,
            interval: CaptureInterval {
                started_at: at,
                ended_at: at,
            },
            versions: versions(),
            outcome: BaselineOutcome::Unavailable { reason },
        }
    }

    /// A stamp for a round whose checkout cannot be read at all, built directly like
    /// [`Self::baseline_unavailable`].
    #[must_use]
    pub fn stamp_unavailable(
        &self,
        binding: TicketBinding,
        baseline_lsn: Lsn,
        check_lsn: Lsn,
        reason: DiffRefusal,
    ) -> StampPayload {
        let at = (self.clock)();
        StampPayload {
            v: STAMP_PAYLOAD_VERSION,
            binding,
            baseline_lsn,
            check_lsn,
            interval: CaptureInterval {
                started_at: at,
                ended_at: at,
            },
            versions: versions(),
            outcome: StampOutcome::Unavailable { reason },
        }
    }

    fn sample(
        &self,
        tree: &Tree,
        baseline: &BaselinePayload,
        binding: TicketBinding,
        cancelled: Arc<AtomicBool>,
    ) -> Result<ReviewCapture, DiffRefusal> {
        if baseline.binding != binding {
            return Err(DiffRefusal::StaleTicket);
        }
        let BaselineOutcome::Captured {
            device,
            inode,
            head,
            preexisting_tracked,
        } = baseline.outcome
        else {
            return Err(DiffRefusal::BaselineUnavailable);
        };
        let capture = BaselineCapture {
            root: RootIdentity { device, inode },
            head,
            preexisting_tracked,
        };
        self.reader.capture_review(tree, &capture, cancelled)
    }

    fn interval(&self, started_at: Timestamp) -> CaptureInterval {
        CaptureInterval {
            started_at,
            ended_at: (self.clock)().max(started_at),
        }
    }
}

fn versions() -> SamplerVersions {
    SamplerVersions {
        sampler: SAMPLER_VERSION,
        policy: TEXT_POLICY_VERSION,
    }
}

fn sampled(capture: &ReviewCapture) -> StampOutcome {
    match DigestHex::parse(&capture.digest) {
        Ok(digest) => StampOutcome::Sampled {
            complete: capture.complete,
            digest,
            omitted: capture.omitted,
            omitted_lower_bound: capture.omitted_lower_bound,
            untracked: capture.untracked,
            untracked_lower_bound: capture.untracked_lower_bound,
        },
        Err(_) => StampOutcome::Unavailable {
            reason: DiffRefusal::Unreadable,
        },
    }
}
