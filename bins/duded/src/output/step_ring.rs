use std::collections::VecDeque;
use std::sync::Arc;

use super::{STEP_RING_BYTES, STEP_RING_ENTRIES};
use crate::step_results::ResultStore;
use crate::steps::{self, Closing, KeptEdit, StepView};

/// A step change that arrived before its turn's session was confirmed.
#[derive(Clone)]
pub(super) struct PendingStep {
    pub(super) session: Arc<str>,
    pub(super) op: StepOp,
    pub(super) arrival: u128,
}

#[derive(Clone)]
pub(super) enum StepOp {
    Open {
        view: Box<StepView>,
        edit: Option<KeptEdit>,
    },
    Close(Closing),
}

impl StepOp {
    pub(super) fn bytes(&self) -> usize {
        match self {
            Self::Open { view, edit } => view.bytes() + edit.as_ref().map_or(0, KeptEdit::bytes),
            Self::Close(closing) => {
                64 + closing.result.as_ref().map_or(0, steps::KeptResult::bytes)
            }
        }
    }
}

/// Step changes waiting for a session, bounded like the ring they will join.
#[derive(Default)]
pub(super) struct StepQueue {
    pub(super) items: VecDeque<PendingStep>,
    pub(super) bytes: usize,
    pub(super) lost: bool,
}

impl StepQueue {
    pub(super) fn push(&mut self, item: PendingStep) {
        self.bytes += item.op.bytes();
        self.items.push_back(item);
        while self.items.len() > STEP_RING_ENTRIES || self.bytes > STEP_RING_BYTES {
            self.drop_oldest();
        }
    }

    pub(super) fn drop_oldest(&mut self) {
        if let Some(old) = self.items.pop_front() {
            self.bytes -= old.op.bytes();
            self.lost = true;
        }
    }
}

impl std::fmt::Debug for StepQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StepQueue")
            .field("steps", &self.items.len())
            .field("bytes", &self.bytes)
            .field("lost", &self.lost)
            .finish()
    }
}

#[derive(Debug)]
pub(super) struct StoredStep {
    pub(super) view: StepView,
    pub(super) seq: u64,
    pub(super) arrival: u128,
}

/// The latest state of each step of a turn, oldest first, under its own bounds so steps never
/// push text out of the output ring.
#[derive(Debug, Default)]
pub(super) struct StepRing {
    pub(super) items: VecDeque<StoredStep>,
    pub(super) bytes: usize,
    pub(super) seq: u64,
    pub(super) lost: bool,
    pub(super) results: ResultStore,
}

impl StepRing {
    pub(super) fn apply(&mut self, op: StepOp, arrival: u128) {
        self.seq = self.seq.saturating_add(1);
        match op {
            StepOp::Open { view, edit } => {
                self.results.forget(&view.id);
                self.results.keep(&view.id, arrival, None, edit);
                let bytes = view.bytes();
                if let Some(old) = self.items.iter_mut().find(|old| old.view.id == view.id) {
                    self.bytes = self.bytes - old.view.bytes() + bytes;
                    old.view = *view;
                    old.seq = self.seq;
                } else {
                    self.bytes += bytes;
                    self.items.push_back(StoredStep {
                        view: *view,
                        seq: self.seq,
                        arrival,
                    });
                }
                self.trim();
            }
            StepOp::Close(mut closing) => {
                if let Some(step) = self
                    .items
                    .iter_mut()
                    .find(|step| step.view.id == closing.id)
                {
                    step.view.close(&closing);
                    step.seq = self.seq;
                    if steps::keeps_result(&step.view) {
                        self.results
                            .keep(&closing.id, arrival, closing.result.take(), None);
                    }
                }
            }
        }
    }

    pub(super) fn trim(&mut self) {
        while self.items.len() > STEP_RING_ENTRIES || self.bytes > STEP_RING_BYTES {
            self.drop_oldest();
        }
    }

    pub(super) fn drop_oldest(&mut self) {
        if let Some(old) = self.items.pop_front() {
            self.bytes -= old.view.bytes();
            self.lost = true;
        }
    }

    pub(super) fn abandon(&mut self, at: u64) {
        for step in &mut self.items {
            if step.view.status == steps::Status::Running {
                step.view.abandon(at);
                self.seq = self.seq.saturating_add(1);
                step.seq = self.seq;
            }
        }
    }
}
