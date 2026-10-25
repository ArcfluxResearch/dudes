//! Run-local Managed assistant text. The journal remains the only persistent truth.
//! All child-key joins, sequence changes, snapshots and notifications share one lock.

mod registry;
mod ring;
mod state;
mod step_ring;
#[cfg(test)]
mod tests;

pub use registry::OutputRegistry;

use dudes_types::{AgentId, TurnId};

use crate::steps::StepView;

const CHUNK_BYTES: usize = 4_096;
const RING_CHUNKS: usize = 128;
const RING_BYTES: usize = 65_536;
const REPLY_BYTES: usize = 65_536;
const MAX_RINGS: usize = 128;
const STEP_RING_ENTRIES: usize = 256;
const STEP_RING_BYTES: usize = 256 * 1_024;
const MAX_BYTES: usize = 8 * 1_024 * 1_024;
// A session is an untrusted exact identity, not an output chunk. Capping it before
// retention also bounds 256 pending key/ring slots × 128 pieces × 256 bytes to 8 MiB.
const MAX_SESSION_BYTES: usize = 256;

/// One retained UTF-8 piece from this run's latest turn.
#[derive(Clone, PartialEq, Eq)]
pub struct OutputChunk {
    /// Checked sequence within `(run, agent, turn)`.
    pub seq: u64,
    /// Typed assistant text only, at most 4,096 UTF-8 bytes.
    pub text: String,
}

impl std::fmt::Debug for OutputChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputChunk")
            .field("seq", &self.seq)
            .field("bytes", &self.text.len())
            .finish()
    }
}

/// Finite reasons why this registry has no readable ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// No child has produced run-local output after startup/restart.
    NoRunOutput,
    /// A turn exists but no durable vendor session has bound it.
    Unbound,
    /// Finite retention evicted the latest ring.
    Evicted,
    /// The agent was durably retired.
    Retired,
}

/// A missing interval that a live reader must label or use to end selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loss {
    /// Text arrived before confirmation faster than pending retention.
    PendingOverflow,
    /// The latest ring exceeded its chunk or byte bound.
    RingTruncation,
    /// Text was the oldest retained data across this daemon run.
    GlobalEviction,
    /// Checked chunk sequence could not advance.
    SequenceOverflow,
}

/// Missing assistant text in an accumulated provisional reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyLoss {
    /// The provider parser omitted assistant text at its bounded bookkeeping limit.
    ProviderLimit,
    /// Text was discarded before its durable session was confirmed.
    PendingOverflow,
    /// The accumulated reply exceeded its UTF-8 byte limit.
    ReplyTruncation,
    /// The run-wide text limit discarded the accumulated prefix.
    GlobalEviction,
    /// The raw output sequence sealed before accepting more text.
    SequenceOverflow,
}

/// A bounded, provisional reply assembled only from attributable assistant text.
#[derive(Clone, PartialEq, Eq)]
pub struct OutputReply {
    /// Durable agent identity.
    pub agent: AgentId,
    /// Durable turn identity.
    pub turn: TurnId,
    /// Checked change counter within `(run, agent, turn)`, independent of raw chunks.
    pub revision: u128,
    /// Accumulated UTF-8 suffix, at most 65,536 bytes.
    pub text: String,
    /// Why some assistant text is missing, if any.
    pub loss: Option<ReplyLoss>,
    /// Whether the durable turn ended; this text is still provisional.
    pub ended: bool,
}

impl std::fmt::Debug for OutputReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputReply")
            .field("agent", &self.agent)
            .field("turn", &self.turn)
            .field("revision", &self.revision)
            .field("bytes", &self.text.len())
            .field("loss", &self.loss)
            .field("ended", &self.ended)
            .finish()
    }
}

/// The complete daemon-local read view, without raw JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputState {
    /// No durably bound latest-turn text is readable.
    Unavailable(Unavailable),
    /// The bounded latest-turn snapshot.
    Ready {
        /// Durable agent identity.
        agent: AgentId,
        /// Durable turn identity.
        turn: TurnId,
        /// Earliest retained sequence, including a missing prefix.
        first_seq: u64,
        /// Checked sequence for the next chunk.
        next_seq: u64,
        /// Retained typed text pieces.
        chunks: Vec<OutputChunk>,
        /// Whether any earlier text was lost.
        truncated: bool,
        /// Whether an accepted durable end sealed the turn.
        ended: bool,
        /// A live reader must treat this as a gap. A fresh selection may snapshot
        /// the retained suffix and label its missing prefix.
        loss: Option<Loss>,
    },
    /// Checked sequence overflow sealed output for this turn.
    Gap {
        /// Durable agent identity.
        agent: AgentId,
        /// Durable turn identity.
        turn: TurnId,
        /// Earliest retained sequence in the sealed ring.
        first_seq: u64,
        /// Sequence that could no longer advance.
        next_seq: u64,
        /// Retained bounded suffix for a truthful initial snapshot.
        chunks: Vec<OutputChunk>,
        /// Whether an earlier prefix was discarded.
        truncated: bool,
        /// Whether an accepted durable end sealed the turn.
        ended: bool,
        /// Why output sealed.
        reason: Loss,
    },
}

/// Where a turn's steps stand, for a reader that wants to know whether to fetch them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StepsMark {
    pub(crate) turn: TurnId,
    pub(crate) seq: u64,
}

/// The steps that changed after a cursor.
#[derive(Debug)]
pub(crate) struct StepsDelta {
    pub(crate) turn: TurnId,
    pub(crate) seq: u64,
    pub(crate) truncated: bool,
    pub(crate) items: Vec<StepView>,
}

/// Atomic read plus notification cursor; revisions are not journal positions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSnapshot {
    /// Change counter for `wait_changed`.
    pub revision: u128,
    /// Current bounded view.
    pub state: OutputState,
    /// Independently bounded reply, available after durable session attribution.
    pub reply: Option<OutputReply>,
    /// Where the turn's steps stand, available after durable session attribution.
    pub(crate) steps: Option<StepsMark>,
}

/// Safe counters for asserting the run-local bound without exposing text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputUsage {
    /// Latest-turn ring count.
    pub rings: usize,
    /// Unregistered full child keys with pending text.
    pub pending_keys: usize,
    /// All retained chunks, including pending.
    pub chunks: usize,
    /// All retained UTF-8 text bytes, including accumulated replies.
    pub text_bytes: usize,
    /// Conservative retained session-string bytes (shared pieces may count twice).
    pub identity_bytes: usize,
    /// Refused callbacks; no text is retained in diagnostics.
    pub refused: u64,
    /// Watcher EOF callbacks observed in this run.
    pub finished: u64,
}
