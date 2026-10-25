use std::collections::VecDeque;
use std::sync::Arc;

use dudes_types::{SessionId, TurnId};

use super::state::ChildKey;
use super::step_ring::{StepQueue, StepRing};
use super::{CHUNK_BYTES, Loss, OutputChunk, REPLY_BYTES, RING_BYTES, RING_CHUNKS, ReplyLoss};

#[derive(Clone, Debug)]
pub(super) struct Stored {
    pub(super) chunk: OutputChunk,
    pub(super) arrival: u128,
}

#[derive(Clone)]
pub(super) struct Pending {
    pub(super) session: Arc<str>,
    pub(super) body: CapturedText,
    pub(super) arrival: u128,
}

#[derive(Clone)]
pub(super) enum CapturedText {
    Text(String),
    Omitted,
}

impl CapturedText {
    pub(super) fn bytes(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Omitted => 0,
        }
    }
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("bytes", &self.body.bytes())
            .field("arrival", &self.arrival)
            .finish_non_exhaustive()
    }
}

/// A durable session acknowledgement is distinct from retaining its identity.
/// The refused state carries no vendor bytes but still closes the Unbound wait.
#[derive(Default)]
pub(super) enum SessionGate {
    #[default]
    Unconfirmed,
    Admitted(SessionId),
    RefusedOvercap,
}

impl std::fmt::Debug for SessionGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unconfirmed => f.write_str("Unconfirmed"),
            Self::Admitted(session) => f
                .debug_tuple("Admitted")
                .field(&session.as_str().len())
                .finish(),
            Self::RefusedOvercap => f.write_str("RefusedOvercap"),
        }
    }
}

#[derive(Debug)]
pub(super) struct Ring {
    pub(super) turn: TurnId,
    pub(super) key: Option<ChildKey>,
    pub(super) gate: SessionGate,
    pub(super) chunks: VecDeque<Stored>,
    pub(super) pending: VecDeque<Pending>,
    pub(super) pending_bytes: usize,
    pub(super) pending_steps: StepQueue,
    pub(super) steps: StepRing,
    pub(super) bytes: usize,
    pub(super) first_seq: u64,
    pub(super) next_seq: u64,
    pub(super) truncated: bool,
    pub(super) loss: Option<Loss>,
    pub(super) ended: bool,
    pub(super) overflow: bool,
    pub(super) arrival: u128,
    pub(super) reply: String,
    pub(super) reply_arrival: u128,
    pub(super) reply_loss: Option<ReplyLoss>,
    pub(super) reply_revision: u128,
}

impl Ring {
    pub(super) fn capture(&mut self, body: CapturedText, arrival: u128) {
        match body {
            CapturedText::Text(text) => self.add_visible(text, arrival),
            CapturedText::Omitted => self.lose_reply(ReplyLoss::ProviderLimit),
        }
    }
    pub(super) fn new(turn: TurnId, arrival: u128) -> Self {
        Self {
            turn,
            key: None,
            gate: SessionGate::Unconfirmed,
            chunks: VecDeque::new(),
            pending: VecDeque::new(),
            pending_bytes: 0,
            pending_steps: StepQueue::default(),
            steps: StepRing::default(),
            bytes: 0,
            first_seq: 0,
            next_seq: 0,
            truncated: false,
            loss: None,
            ended: false,
            overflow: false,
            arrival,
            reply: String::new(),
            reply_arrival: arrival,
            reply_loss: None,
            reply_revision: 0,
        }
    }

    pub(super) fn revise_reply(&mut self) {
        self.reply_revision = self
            .reply_revision
            .checked_add(1)
            .expect("a reply cannot have u128::MAX changes");
    }

    pub(super) fn lose_reply(&mut self, reason: ReplyLoss) {
        if self.reply_loss != Some(reason) {
            self.reply_loss = Some(reason);
            self.revise_reply();
        }
    }

    pub(super) fn trim_pending(&mut self) {
        while self.pending.len() > RING_CHUNKS || self.pending_bytes > RING_BYTES {
            if let Some(old) = self.pending.pop_front() {
                self.pending_bytes -= old.body.bytes();
                self.truncated = true;
                self.loss = Some(Loss::PendingOverflow);
                self.lose_reply(ReplyLoss::PendingOverflow);
            }
        }
    }

    pub(super) fn add_visible(&mut self, text: String, arrival: u128) {
        if self.ended || self.overflow {
            return;
        }
        let Some(next_seq) = self.next_seq.checked_add(1) else {
            self.overflow = true;
            self.loss = Some(Loss::SequenceOverflow);
            self.lose_reply(ReplyLoss::SequenceOverflow);
            return;
        };
        if self.reply.is_empty() {
            self.reply_arrival = arrival;
        }
        if self.reply.len() + text.len() > REPLY_BYTES {
            let mut discarded = self.reply.len() + text.len() - REPLY_BYTES;
            while !self.reply.is_char_boundary(discarded) {
                discarded += 1;
            }
            self.reply.drain(..discarded);
            self.reply_loss = Some(ReplyLoss::ReplyTruncation);
        }
        self.reply.reserve_exact(text.len());
        self.reply.push_str(&text);
        self.revise_reply();
        self.bytes += text.len();
        self.chunks.push_back(Stored {
            chunk: OutputChunk {
                seq: self.next_seq,
                text,
            },
            arrival,
        });
        self.next_seq = next_seq;
        while self.chunks.len() > RING_CHUNKS || self.bytes > RING_BYTES {
            self.drop_oldest_visible(Loss::RingTruncation);
        }
    }

    pub(super) fn drop_oldest_visible(&mut self, reason: Loss) {
        if let Some(old) = self.chunks.pop_front() {
            self.bytes -= old.chunk.text.len();
            self.first_seq = old.chunk.seq + 1;
            self.truncated = true;
            self.loss = Some(reason);
        }
    }
}

pub(super) fn split_utf8(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut end = rest.len().min(CHUNK_BYTES);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (piece, remaining) = rest.split_at(end);
        rest = remaining;
        Some(piece)
    })
}
