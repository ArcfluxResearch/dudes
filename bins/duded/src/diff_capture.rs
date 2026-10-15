//! Turns the bounded checkout reader's observations into the baseline and review-stamp payloads
//! a ticket round records.
//!
//! Every reader refusal becomes a finite unavailable outcome. Nothing here guesses a baseline,
//! reuses one across tickets, or writes a record: the caller appends the payload and decides
//! what a failed append means.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};

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

/// Samples a recorded checkout and describes the result as bounded payloads.
pub struct DiffCapture {
    reader: Arc<DiffReader>,
    serial: Mutex<()>,
    clock: Box<dyn Fn() -> Timestamp + Send + Sync>,
}

impl DiffCapture {
    /// Use `reader` for observations and `clock` for the capture interval.
    #[must_use]
    pub fn new(reader: Arc<DiffReader>, clock: Box<dyn Fn() -> Timestamp + Send + Sync>) -> Self {
        Self {
            reader,
            serial: Mutex::new(()),
            clock,
        }
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
        let _one_at_a_time = self.serial.lock().unwrap_or_else(PoisonError::into_inner);
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
        let _one_at_a_time = self.serial.lock().unwrap_or_else(PoisonError::into_inner);
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
