//! Run-local Managed assistant text. The journal remains the only persistent truth.
//! All child-key joins, sequence changes, snapshots and notifications share one lock.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use dudes_providers::steps::StepEvent;
use dudes_supervisor::host::AdoptionKey;
use dudes_supervisor::{Redactor, StreamScrubber};
use dudes_types::{AgentId, SessionId, TurnId};

use crate::steps::{self, Closing, StepView};

const CHUNK_BYTES: usize = 4_096;
const RING_CHUNKS: usize = 128;
const RING_BYTES: usize = 65_536;
const REPLY_BYTES: usize = 65_536;
const MAX_RINGS: usize = 128;
const STEP_RING_ENTRIES: usize = 256;
const STEP_RING_BYTES: usize = 256 * 1_024;
const MAX_STEP_BYTES: usize = 8 * 1_024 * 1_024;
const MAX_BYTES: usize = 8 * 1_024 * 1_024;
// A session is an untrusted exact identity, not an output chunk. Capping it before
// retention also bounds 256 pending key/ring slots × 128 pieces × 256 bytes to 8 MiB.
const MAX_SESSION_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ChildKey {
    pid: u32,
    start_time: u64,
    boot: Option<String>,
}

impl From<&AdoptionKey> for ChildKey {
    fn from(key: &AdoptionKey) -> Self {
        Self {
            pid: key.pid,
            start_time: key.start_time,
            boot: key.boot.clone(),
        }
    }
}

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

#[derive(Clone, Debug)]
struct Stored {
    chunk: OutputChunk,
    arrival: u128,
}

#[derive(Clone)]
struct Pending {
    session: Arc<str>,
    body: CapturedText,
    arrival: u128,
}

#[derive(Clone)]
enum CapturedText {
    Text(String),
    Omitted,
}

impl CapturedText {
    fn bytes(&self) -> usize {
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

/// A step change that arrived before its turn's session was confirmed.
#[derive(Clone)]
struct PendingStep {
    session: Arc<str>,
    op: StepOp,
    arrival: u128,
}

#[derive(Clone)]
enum StepOp {
    Open(StepView),
    Close(Closing),
}

impl StepOp {
    fn bytes(&self) -> usize {
        match self {
            Self::Open(view) => view.bytes(),
            Self::Close(_) => 64,
        }
    }
}

/// Step changes waiting for a session, bounded like the ring they will join.
#[derive(Default)]
struct StepQueue {
    items: VecDeque<PendingStep>,
    bytes: usize,
    lost: bool,
}

impl StepQueue {
    fn push(&mut self, item: PendingStep) {
        self.bytes += item.op.bytes();
        self.items.push_back(item);
        while self.items.len() > STEP_RING_ENTRIES || self.bytes > STEP_RING_BYTES {
            if let Some(old) = self.items.pop_front() {
                self.bytes -= old.op.bytes();
                self.lost = true;
            }
        }
    }
}

impl std::fmt::Debug for StepQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StepQueue")
            .field("steps", &self.items.len())
            .field("bytes", &self.bytes)
            .finish()
    }
}

#[derive(Debug)]
struct StoredStep {
    view: StepView,
    seq: u64,
    arrival: u128,
}

/// The latest state of each step of a turn, oldest first, under its own bounds so steps never
/// push text out of the output ring.
#[derive(Debug, Default)]
struct StepRing {
    items: VecDeque<StoredStep>,
    bytes: usize,
    seq: u64,
    lost: bool,
}

impl StepRing {
    fn apply(&mut self, op: StepOp, arrival: u128) {
        self.seq = self.seq.saturating_add(1);
        match op {
            StepOp::Open(view) => {
                let bytes = view.bytes();
                if let Some(old) = self.items.iter_mut().find(|old| old.view.id == view.id) {
                    self.bytes = self.bytes - old.view.bytes() + bytes;
                    old.view = view;
                    old.seq = self.seq;
                } else {
                    self.bytes += bytes;
                    self.items.push_back(StoredStep {
                        view,
                        seq: self.seq,
                        arrival,
                    });
                }
                self.trim();
            }
            StepOp::Close(closing) => {
                if let Some(step) = self
                    .items
                    .iter_mut()
                    .find(|step| step.view.id == closing.id)
                {
                    step.view.close(&closing);
                    step.seq = self.seq;
                }
            }
        }
    }

    fn trim(&mut self) {
        while self.items.len() > STEP_RING_ENTRIES || self.bytes > STEP_RING_BYTES {
            self.drop_oldest();
        }
    }

    fn drop_oldest(&mut self) {
        if let Some(old) = self.items.pop_front() {
            self.bytes -= old.view.bytes();
            self.lost = true;
        }
    }

    fn abandon(&mut self, at: u64) {
        for step in &mut self.items {
            if step.view.status == steps::Status::Running {
                step.view.abandon(at);
                self.seq = self.seq.saturating_add(1);
                step.seq = self.seq;
            }
        }
    }
}

#[derive(Default, Debug)]
struct Orphan {
    chunks: VecDeque<Pending>,
    steps: StepQueue,
    bytes: usize,
    lost: bool,
    finished: bool,
}

/// A durable session acknowledgement is distinct from retaining its identity.
/// The refused state carries no vendor bytes but still closes the Unbound wait.
#[derive(Default)]
enum SessionGate {
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
struct Ring {
    turn: TurnId,
    key: Option<ChildKey>,
    gate: SessionGate,
    chunks: VecDeque<Stored>,
    pending: VecDeque<Pending>,
    pending_bytes: usize,
    pending_steps: StepQueue,
    steps: StepRing,
    bytes: usize,
    first_seq: u64,
    next_seq: u64,
    truncated: bool,
    loss: Option<Loss>,
    ended: bool,
    overflow: bool,
    arrival: u128,
    reply: String,
    reply_arrival: u128,
    reply_loss: Option<ReplyLoss>,
    reply_revision: u128,
}

impl Ring {
    fn capture(&mut self, body: CapturedText, arrival: u128) {
        match body {
            CapturedText::Text(text) => self.add_visible(text, arrival),
            CapturedText::Omitted => self.lose_reply(ReplyLoss::ProviderLimit),
        }
    }
    fn new(turn: TurnId, arrival: u128) -> Self {
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

    fn revise_reply(&mut self) {
        self.reply_revision = self
            .reply_revision
            .checked_add(1)
            .expect("a reply cannot have u128::MAX changes");
    }

    fn lose_reply(&mut self, reason: ReplyLoss) {
        if self.reply_loss != Some(reason) {
            self.reply_loss = Some(reason);
            self.revise_reply();
        }
    }

    fn trim_pending(&mut self) {
        while self.pending.len() > RING_CHUNKS || self.pending_bytes > RING_BYTES {
            if let Some(old) = self.pending.pop_front() {
                self.pending_bytes -= old.body.bytes();
                self.truncated = true;
                self.loss = Some(Loss::PendingOverflow);
                self.lose_reply(ReplyLoss::PendingOverflow);
            }
        }
    }

    fn add_visible(&mut self, text: String, arrival: u128) {
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

    fn drop_oldest_visible(&mut self, reason: Loss) {
        if let Some(old) = self.chunks.pop_front() {
            self.bytes -= old.chunk.text.len();
            self.first_seq = old.chunk.seq + 1;
            self.truncated = true;
            self.loss = Some(reason);
        }
    }
}

#[derive(Clone, Debug)]
struct Binding {
    agent: AgentId,
    turn: TurnId,
    finished: bool,
}

#[derive(Default, Debug)]
struct State {
    rings: BTreeMap<AgentId, Ring>,
    bindings: BTreeMap<ChildKey, Binding>,
    orphan: BTreeMap<ChildKey, Orphan>,
    closed: VecDeque<ChildKey>,
    unavailable: BTreeMap<AgentId, Unavailable>,
    unavailable_order: VecDeque<AgentId>,
    revision: u128,
    arrival: u128,
    refused: u64,
    finished: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RetainedText {
    Chunk,
    Pending,
    Reply,
}

impl State {
    fn pending_omission(&self, binding: Option<&Binding>, key: &ChildKey, session: &str) -> bool {
        let pending = if let Some(binding) = binding {
            self.rings.get(&binding.agent).map(|ring| &ring.pending)
        } else {
            self.orphan.get(key).map(|orphan| &orphan.chunks)
        };
        pending.is_some_and(|pending| {
            pending.iter().any(|item| {
                item.session.as_ref() == session && matches!(&item.body, CapturedText::Omitted)
            })
        })
    }

    fn next_arrival(&mut self) -> u128 {
        self.arrival = self
            .arrival
            .checked_add(1)
            .expect("a run cannot have u128::MAX chunks");
        self.arrival
    }

    fn refuse(&mut self) {
        self.refused = self.refused.saturating_add(1);
    }

    fn remember(&mut self, agent: AgentId, reason: Unavailable) {
        self.unavailable_order.retain(|old| *old != agent);
        self.unavailable_order.push_back(agent);
        self.unavailable.insert(agent, reason);
        while self.unavailable.len() > MAX_RINGS {
            if let Some(old) = self.unavailable_order.pop_front() {
                self.unavailable.remove(&old);
            }
        }
    }

    fn close(&mut self, key: ChildKey) {
        self.orphan.remove(&key);
        if !self.closed.contains(&key) {
            self.closed.push_back(key);
        }
        while self.closed.len() > MAX_RINGS {
            self.closed.pop_front();
        }
    }

    fn revoke(&mut self, agent: AgentId) {
        let keys: Vec<_> = self
            .bindings
            .iter()
            .filter(|(_, binding)| binding.agent == agent)
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.bindings.remove(&key);
            self.close(key);
        }
    }

    fn make_room(&mut self) {
        while self.rings.len() > MAX_RINGS {
            let oldest = self
                .rings
                .iter()
                .min_by_key(|(_, ring)| ring.arrival)
                .map(|(id, _)| *id);
            if let Some(agent) = oldest {
                self.rings.remove(&agent);
                self.revoke(agent);
                self.remember(agent, Unavailable::Evicted);
            }
        }
        while self.orphan.len() > MAX_RINGS {
            let oldest = self
                .orphan
                .iter()
                .filter_map(|(key, value)| {
                    value
                        .chunks
                        .front()
                        .map(|front| (key.clone(), front.arrival))
                })
                .min_by_key(|(_, at)| *at)
                .map(|(key, _)| key)
                .or_else(|| self.orphan.keys().next().cloned());
            if let Some(key) = oldest {
                self.orphan.remove(&key);
                self.close(key);
            }
        }
        while self.text_bytes() > MAX_BYTES {
            let ring_oldest = self
                .rings
                .iter()
                .filter_map(|(agent, ring)| {
                    let visible = ring
                        .chunks
                        .front()
                        .map(|c| (c.arrival, RetainedText::Chunk));
                    let pending = ring
                        .pending
                        .front()
                        .map(|c| (c.arrival, RetainedText::Pending));
                    let reply = (!ring.reply.is_empty())
                        .then_some((ring.reply_arrival, RetainedText::Reply));
                    visible
                        .into_iter()
                        .chain(pending)
                        .chain(reply)
                        .min()
                        .map(|(at, pending)| (*agent, at, pending))
                })
                .min_by_key(|(_, at, _)| *at);
            let orphan_oldest = self
                .orphan
                .iter()
                .filter_map(|(key, o)| o.chunks.front().map(|c| (key.clone(), c.arrival)))
                .min_by_key(|(_, at)| *at);
            match (ring_oldest, orphan_oldest) {
                (Some((_, at, _)), Some((key, other))) if other < at => {
                    self.drop_orphan(&key);
                }
                (Some((agent, _, retained)), _) => {
                    let ring = self.rings.get_mut(&agent).expect("selected ring");
                    match retained {
                        RetainedText::Pending => {
                            if let Some(old) = ring.pending.pop_front() {
                                ring.pending_bytes -= old.body.bytes();
                                ring.truncated = true;
                                ring.loss = Some(Loss::GlobalEviction);
                                ring.lose_reply(ReplyLoss::GlobalEviction);
                            }
                        }
                        RetainedText::Chunk => ring.drop_oldest_visible(Loss::GlobalEviction),
                        RetainedText::Reply => {
                            ring.reply = String::new();
                            ring.reply_loss = Some(ReplyLoss::GlobalEviction);
                            ring.revise_reply();
                        }
                    }
                }
                (None, Some((key, _))) => self.drop_orphan(&key),
                (None, None) => break,
            }
        }
        while self.step_bytes() > MAX_STEP_BYTES {
            let oldest = self
                .rings
                .iter()
                .filter_map(|(agent, ring)| {
                    ring.steps.items.front().map(|step| (*agent, step.arrival))
                })
                .min_by_key(|(_, at)| *at);
            let Some((agent, _)) = oldest else { break };
            if let Some(ring) = self.rings.get_mut(&agent) {
                ring.steps.drop_oldest();
            }
        }
    }

    fn step_bytes(&self) -> usize {
        self.rings.values().map(|ring| ring.steps.bytes).sum()
    }

    fn drop_orphan(&mut self, key: &ChildKey) {
        if let Some(orphan) = self.orphan.get_mut(key)
            && let Some(old) = orphan.chunks.pop_front()
        {
            orphan.bytes -= old.body.bytes();
            orphan.lost = true;
        }
    }

    fn text_bytes(&self) -> usize {
        self.rings
            .values()
            .map(|r| r.bytes + r.pending_bytes + r.reply.len())
            .sum::<usize>()
            + self.orphan.values().map(|p| p.bytes).sum::<usize>()
    }

    fn identity_bytes(&self) -> usize {
        self.rings
            .values()
            .map(|ring| {
                (match &ring.gate {
                    SessionGate::Admitted(session) => session.as_str().len(),
                    SessionGate::Unconfirmed | SessionGate::RefusedOvercap => 0,
                }) + ring
                    .pending
                    .iter()
                    .map(|chunk| chunk.session.len())
                    .sum::<usize>()
            })
            .sum::<usize>()
            + self
                .orphan
                .values()
                .flat_map(|orphan| &orphan.chunks)
                .map(|chunk| chunk.session.len())
                .sum::<usize>()
    }
}

/// A daemon-run-only registry; all mutation and snapshots are ordered by one lock.
pub struct OutputRegistry {
    state: Mutex<State>,
    changed: Condvar,
    redactor: Redactor,
    /// One scrubber per child, holding back the tail of its text while that tail could still
    /// become a known secret. Taken out when the child's output ends.
    streams: Mutex<BTreeMap<ChildKey, (StreamScrubber, String)>>,
}

impl std::fmt::Debug for OutputRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OutputRegistry { text: <redacted> }")
    }
}

impl OutputRegistry {
    /// An empty run-local registry that scrubs nothing. Only tests build one: a daemon builds its
    /// registry with the secret set its children share.
    #[cfg(any(test, feature = "testing"))]
    #[expect(
        clippy::new_without_default,
        reason = "a Default would let a missed wiring compile"
    )]
    #[must_use]
    pub fn new() -> Self {
        Self::with_redactor(Redactor::for_run())
    }

    /// An empty registry whose text is scrubbed of `redactor`'s secrets before anything keeps it.
    #[must_use]
    pub fn with_redactor(redactor: Redactor) -> Self {
        Self {
            state: Mutex::default(),
            changed: Condvar::new(),
            redactor,
            streams: Mutex::default(),
        }
    }

    /// The child's scrubber is taken out of the map while it matches, so one large message
    /// blocks no other child. A child has one reader thread, so nothing else touches its entry.
    fn scrub(&self, key: &AdoptionKey, session: &str, text: &str) -> String {
        let child = ChildKey::from(key);
        let taken = self
            .streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&child);
        let mut scrubber = taken.map_or_else(|| self.redactor.stream(), |(scrubber, _)| scrubber);
        let released = scrubber.push(text);
        self.streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(child, (scrubber, session.to_owned()));
        released
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn notify(&self, state: &mut State) {
        state.revision = state
            .revision
            .checked_add(1)
            .expect("a run cannot have u128::MAX notices");
        self.changed.notify_all();
    }

    /// A durable `TurnIssued` replaces any old output even before the new child starts.
    pub fn replace(&self, agent: AgentId, turn: TurnId) {
        let mut state = self.state();
        state.revoke(agent);
        let at = state.next_arrival();
        state.rings.insert(agent, Ring::new(turn, at));
        state.unavailable.remove(&agent);
        state.unavailable_order.retain(|old| *old != agent);
        state.make_room();
        self.notify(&mut state);
    }

    /// Prepare a replayed durable turn before registering its new child.
    pub fn prepare_reissued(&self, agent: AgentId, turn: TurnId) {
        let state = self.state();
        if state
            .rings
            .get(&agent)
            .is_some_and(|ring| ring.turn == turn)
        {
            return;
        }
        drop(state);
        self.replace(agent, turn);
    }

    /// Call only after the daemon independently resolves a durable Managed turn/agent row.
    pub fn register(&self, agent: AgentId, turn: TurnId, key: &AdoptionKey) -> bool {
        let key = ChildKey::from(key);
        let mut state = self.state();
        if state.closed.contains(&key) || state.bindings.contains_key(&key) {
            state.refuse();
            return false;
        }
        if state
            .rings
            .get(&agent)
            .is_none_or(|ring| ring.turn != turn || ring.ended || ring.key.is_some())
        {
            state.refuse();
            return false;
        }
        let orphan = state.orphan.remove(&key);
        let ring = state.rings.get_mut(&agent).expect("new ring");
        ring.key = Some(key.clone());
        let finished = orphan.as_ref().is_some_and(|orphan| orphan.finished);
        if let Some(orphan) = orphan {
            ring.pending = orphan.chunks;
            ring.pending_bytes = orphan.bytes;
            ring.pending_steps = orphan.steps;
            if orphan.lost {
                ring.truncated = true;
                ring.loss = Some(Loss::PendingOverflow);
                ring.lose_reply(ReplyLoss::PendingOverflow);
            }
            ring.trim_pending();
        }
        state.bindings.insert(
            key,
            Binding {
                agent,
                turn,
                finished,
            },
        );
        state.unavailable.remove(&agent);
        state.unavailable_order.retain(|old| *old != agent);
        state.make_room();
        self.notify(&mut state);
        true
    }

    /// The watcher may reach this before `register`; an unregistered key cannot publish.
    pub fn capture(&self, key: &AdoptionKey, session: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        let released = self.scrub(key, session, text);
        self.capture_scrubbed(key, session, &released);
    }

    fn capture_scrubbed(&self, key: &AdoptionKey, session: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        self.capture_items(
            key,
            session,
            split_utf8(text).map(|text| CapturedText::Text(text.to_owned())),
        );
    }

    /// Record missing assistant text under the same identity gates as captured text.
    pub fn omit(&self, key: &AdoptionKey, session: &str) {
        self.capture_items(key, session, std::iter::once(CapturedText::Omitted));
    }

    /// Whether a child's output may be kept for `session`, and the turn binding it joins when it
    /// has one. `None` refuses it and counts the refusal.
    fn admit(state: &mut State, key: &ChildKey, session: &str) -> Option<Option<Binding>> {
        if session.is_empty() {
            return None;
        }
        if session.len() > MAX_SESSION_BYTES || state.closed.contains(key) {
            state.refuse();
            return None;
        }
        let binding = state.bindings.get(key).cloned();
        if let Some(binding) = &binding {
            if binding.finished
                || state.rings.get(&binding.agent).is_none_or(|r| {
                    r.turn != binding.turn || r.key.as_ref() != Some(key) || r.ended || r.overflow
                })
            {
                state.refuse();
                return None;
            }
            match &state
                .rings
                .get(&binding.agent)
                .expect("checked binding")
                .gate
            {
                SessionGate::Admitted(accepted) if accepted.as_str() != session => {
                    state.refuse();
                    return None;
                }
                SessionGate::RefusedOvercap => {
                    state.refuse();
                    return None;
                }
                SessionGate::Unconfirmed | SessionGate::Admitted(_) => {}
            }
        }
        if state.orphan.get(key).is_some_and(|orphan| orphan.finished) {
            state.refuse();
            return None;
        }
        Some(binding)
    }

    fn capture_items(
        &self,
        key: &AdoptionKey,
        session: &str,
        items: impl Iterator<Item = CapturedText>,
    ) {
        let key = ChildKey::from(key);
        let mut state = self.state();
        let Some(binding) = Self::admit(&mut state, &key, session) else {
            return;
        };
        let session: Arc<str> = Arc::from(session);
        for piece in items {
            if matches!(&piece, CapturedText::Omitted)
                && state.pending_omission(binding.as_ref(), &key, &session)
            {
                continue;
            }
            let arrival = state.next_arrival();
            if let Some(binding) = &binding {
                let ring = state
                    .rings
                    .get_mut(&binding.agent)
                    .expect("checked binding");
                if matches!(ring.gate, SessionGate::Admitted(_)) {
                    ring.capture(piece, arrival);
                } else {
                    ring.pending_bytes += piece.bytes();
                    ring.pending.push_back(Pending {
                        session: Arc::clone(&session),
                        body: piece,
                        arrival,
                    });
                    ring.trim_pending();
                }
            } else {
                let orphan = state.orphan.entry(key.clone()).or_default();
                orphan.bytes += piece.bytes();
                orphan.chunks.push_back(Pending {
                    session: Arc::clone(&session),
                    body: piece,
                    arrival,
                });
                while orphan.chunks.len() > RING_CHUNKS || orphan.bytes > RING_BYTES {
                    if let Some(old) = orphan.chunks.pop_front() {
                        orphan.bytes -= old.body.bytes();
                        orphan.lost = true;
                    }
                }
            }
            state.make_room();
        }
        self.notify(&mut state);
    }

    /// A tool call the child's reader opened or closed. The text is scrubbed here, before anything
    /// keeps it, and it joins the turn's step ring only once the session is confirmed.
    pub fn capture_step(&self, key: &AdoptionKey, event: StepEvent) {
        let now = steps::now_ms();
        let session = event.session().as_str().to_owned();
        let op = match event {
            StepEvent::Open { step, .. } => StepOp::Open(steps::open(&self.redactor, step, now)),
            StepEvent::Close {
                id,
                failed,
                exit,
                result_bytes,
                note,
                ..
            } => StepOp::Close(Closing {
                id,
                failed,
                exit,
                result_bytes,
                note,
                at: now,
            }),
        };
        let key = ChildKey::from(key);
        let mut state = self.state();
        let Some(binding) = Self::admit(&mut state, &key, &session) else {
            return;
        };
        let arrival = state.next_arrival();
        let item = PendingStep {
            session: Arc::from(session),
            op,
            arrival,
        };
        if let Some(binding) = binding {
            let ring = state
                .rings
                .get_mut(&binding.agent)
                .expect("checked binding");
            if matches!(ring.gate, SessionGate::Admitted(_)) {
                ring.steps.apply(item.op, arrival);
            } else {
                ring.pending_steps.push(item);
            }
        } else {
            state.orphan.entry(key).or_default().steps.push(item);
        }
        state.make_room();
        self.notify(&mut state);
    }

    /// Called only after the accepted `SessionLearned` append and projection fold.
    pub fn confirm(&self, agent: AgentId, turn: TurnId, session: &SessionId) {
        let mut state = self.state();
        let Some(ring) = state
            .rings
            .get_mut(&agent)
            .filter(|r| r.turn == turn && !r.ended && r.key.is_some())
        else {
            state.refuse();
            return;
        };
        if !matches!(ring.gate, SessionGate::Unconfirmed) {
            state.refuse();
            return;
        }
        if session.as_str().len() > MAX_SESSION_BYTES {
            let pending_refused = ring.pending.len() + ring.pending_steps.items.len();
            ring.pending.clear();
            ring.pending_bytes = 0;
            ring.pending_steps = StepQueue::default();
            ring.gate = SessionGate::RefusedOvercap;
            state.refused = state.refused.saturating_add(pending_refused as u64 + 1);
            self.notify(&mut state);
            return;
        }
        ring.gate = SessionGate::Admitted(session.clone());
        ring.revise_reply();
        let pending = std::mem::take(&mut ring.pending);
        ring.pending_bytes = 0;
        let mut refused = 0;
        for chunk in pending {
            if chunk.session.as_ref() == session.as_str() {
                ring.capture(chunk.body, chunk.arrival);
            } else {
                refused += 1;
            }
        }
        let waiting = std::mem::take(&mut ring.pending_steps);
        ring.steps.lost |= waiting.lost;
        for step in waiting.items {
            if step.session.as_ref() == session.as_str() {
                ring.steps.apply(step.op, step.arrival);
            } else {
                refused += 1;
            }
        }
        state.refused = state.refused.saturating_add(refused);
        state.make_room();
        self.notify(&mut state);
    }

    /// The accepted durable end, never the vendor's result self-report.
    pub fn end(&self, agent: AgentId, turn: TurnId) {
        let mut state = self.state();
        if let Some(ring) = state.rings.get_mut(&agent).filter(|r| r.turn == turn) {
            if !ring.ended {
                ring.revise_reply();
            }
            ring.ended = true;
            let refused = if matches!(ring.gate, SessionGate::Unconfirmed) {
                ring.pending.len() + ring.pending_steps.items.len()
            } else {
                0
            };
            ring.pending.clear();
            ring.pending_bytes = 0;
            ring.pending_steps = StepQueue::default();
            ring.steps.abandon(steps::now_ms());
            state.refused = state.refused.saturating_add(refused as u64);
            state.revoke(agent);
            self.notify(&mut state);
        }
    }

    /// Discard text immediately after the durable retirement.
    pub fn retire(&self, agent: AgentId) {
        let mut state = self.state();
        state.rings.remove(&agent);
        state.revoke(agent);
        state.remember(agent, Unavailable::Retired);
        self.notify(&mut state);
    }

    /// EOF refuses later callbacks but keeps bounded, full-key pending data claimable
    /// until the daemon's successful Spawn joins it to its durable turn.
    pub fn watch_finished(&self, key: &AdoptionKey) {
        let stream = self
            .streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&ChildKey::from(key));
        if let Some((mut scrubber, session)) = stream {
            let tail = scrubber.finish();
            self.capture_scrubbed(key, &session, &tail);
        }
        let mut state = self.state();
        state.finished = state.finished.saturating_add(1);
        let key = ChildKey::from(key);
        if let Some(binding) = state.bindings.get_mut(&key) {
            binding.finished = true;
        } else if !state.closed.contains(&key) {
            state.orphan.entry(key).or_default().finished = true;
            state.make_room();
        }
    }

    #[must_use]
    /// Read one agent's bounded latest-turn view and change cursor atomically.
    pub fn snapshot(&self, agent: AgentId) -> OutputSnapshot {
        let state = self.state();
        Self::snapshot_locked(&state, agent)
    }

    /// Wait without a per-listener text queue. Snapshot and revision are read under one lock,
    /// so a notification cannot race between registration and the first read.
    #[must_use]
    pub fn wait_changed(&self, agent: AgentId, seen: u128, timeout: Duration) -> OutputSnapshot {
        let state = self.state();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |s| s.revision == seen)
            .unwrap_or_else(PoisonError::into_inner);
        Self::snapshot_locked(&state, agent)
    }

    fn snapshot_locked(state: &State, agent: AgentId) -> OutputSnapshot {
        let view = match state.rings.get(&agent) {
            None => OutputState::Unavailable(
                state
                    .unavailable
                    .get(&agent)
                    .copied()
                    .unwrap_or(Unavailable::NoRunOutput),
            ),
            Some(ring) if matches!(ring.gate, SessionGate::Unconfirmed) && !ring.ended => {
                OutputState::Unavailable(Unavailable::Unbound)
            }
            Some(ring) if ring.overflow => OutputState::Gap {
                agent,
                turn: ring.turn,
                first_seq: ring.first_seq,
                next_seq: ring.next_seq,
                chunks: ring
                    .chunks
                    .iter()
                    .map(|stored| stored.chunk.clone())
                    .collect(),
                truncated: ring.truncated,
                ended: ring.ended,
                reason: Loss::SequenceOverflow,
            },
            Some(ring) => OutputState::Ready {
                agent,
                turn: ring.turn,
                first_seq: ring.first_seq,
                next_seq: ring.next_seq,
                chunks: ring
                    .chunks
                    .iter()
                    .map(|stored| stored.chunk.clone())
                    .collect(),
                truncated: ring.truncated,
                ended: ring.ended,
                loss: ring.loss,
            },
        };
        OutputSnapshot {
            revision: state.revision,
            state: view,
            reply: state
                .rings
                .get(&agent)
                .filter(|ring| matches!(ring.gate, SessionGate::Admitted(_)))
                .map(|ring| OutputReply {
                    agent,
                    turn: ring.turn,
                    revision: ring.reply_revision,
                    text: ring.reply.clone(),
                    loss: ring.reply_loss,
                    ended: ring.ended,
                }),
            steps: state
                .rings
                .get(&agent)
                .filter(|ring| matches!(ring.gate, SessionGate::Admitted(_)))
                .map(|ring| StepsMark {
                    turn: ring.turn,
                    seq: ring.steps.seq,
                }),
        }
    }

    /// The steps of `agent`'s latest turn that changed after `since`, once the turn's session is
    /// confirmed. `since` is zero for everything the ring holds.
    pub(crate) fn steps_since(&self, agent: AgentId, since: u64) -> Option<StepsDelta> {
        let state = self.state();
        let ring = state
            .rings
            .get(&agent)
            .filter(|ring| matches!(ring.gate, SessionGate::Admitted(_)))?;
        Some(StepsDelta {
            turn: ring.turn,
            seq: ring.steps.seq,
            truncated: ring.steps.lost,
            items: ring
                .steps
                .items
                .iter()
                .filter(|step| step.seq > since)
                .map(|step| step.view.clone())
                .collect(),
        })
    }

    #[must_use]
    /// Metadata counters without any content.
    pub fn usage(&self) -> OutputUsage {
        let state = self.state();
        OutputUsage {
            rings: state.rings.len(),
            pending_keys: state.orphan.len(),
            chunks: state
                .rings
                .values()
                .map(|r| r.chunks.len() + r.pending.len())
                .sum::<usize>()
                + state.orphan.values().map(|p| p.chunks.len()).sum::<usize>(),
            text_bytes: state.text_bytes(),
            identity_bytes: state.identity_bytes(),
            refused: state.refused,
            finished: state.finished,
        }
    }

    #[cfg(test)]
    pub(crate) fn seal_sequence_for_test(&self, agent: AgentId) {
        let mut state = self.state();
        let at = state.next_arrival();
        let ring = state.rings.get_mut(&agent).expect("test ring");
        ring.chunks.clear();
        ring.chunks.push_back(Stored {
            chunk: OutputChunk {
                seq: u64::MAX - 1,
                text: "retained-suffix".into(),
            },
            arrival: at,
        });
        ring.bytes = "retained-suffix".len();
        ring.first_seq = u64::MAX - 1;
        ring.next_seq = u64::MAX;
        ring.truncated = true;
        ring.add_visible("overflow".into(), at + 1);
        self.notify(&mut state);
    }
}

fn split_utf8(text: &str) -> impl Iterator<Item = &str> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_omissions_follow_pending_confirmation_and_session_gates() {
        for before_registration in [false, true] {
            for session in ["wrong", "session"] {
                let output = OutputRegistry::new();
                let agent = AgentId::from_parts(1, 1);
                let turn = TurnId::from_parts(2, 1);
                let key = AdoptionKey {
                    pid: 10,
                    start_time: 20,
                    boot: None,
                };
                if before_registration {
                    for _ in 0..1000 {
                        output.omit(&key, session);
                    }
                }
                output.replace(agent, turn);
                assert!(output.register(agent, turn, &key));
                if !before_registration {
                    for _ in 0..1000 {
                        output.omit(&key, session);
                    }
                }
                output.capture(&key, "session", "prefix");
                assert!(output.snapshot(agent).reply.is_none());
                output.confirm(agent, turn, &SessionId::new("session"));
                let reply = output.snapshot(agent).reply.unwrap();
                assert_eq!(reply.text, "prefix");
                assert_eq!(
                    reply.loss,
                    if session == "session" {
                        Some(ReplyLoss::ProviderLimit)
                    } else {
                        None
                    }
                );
                assert_eq!(output.usage().chunks, 1);
                assert_eq!(output.usage().text_bytes, 12);
            }
        }
    }

    #[test]
    fn provider_omissions_preserve_text_and_refuse_stale_children_and_ended_turns() {
        let output = OutputRegistry::new();
        let agent = AgentId::from_parts(3, 1);
        let turn = TurnId::from_parts(4, 1);
        let key = AdoptionKey {
            pid: 30,
            start_time: 40,
            boot: None,
        };
        output.replace(agent, turn);
        assert!(output.register(agent, turn, &key));
        output.confirm(agent, turn, &SessionId::new("session"));
        output.capture(&key, "session", "first");
        let before = output.snapshot(agent).reply.unwrap();
        let recycled = AdoptionKey {
            start_time: 41,
            ..key.clone()
        };
        output.omit(&recycled, "session");
        output.omit(&key, "wrong");
        assert_eq!(output.snapshot(agent).reply.unwrap(), before);
        output.omit(&key, "session");
        let lost = output.snapshot(agent).reply.unwrap();
        assert_eq!(lost.text, "first");
        assert_eq!(lost.loss, Some(ReplyLoss::ProviderLimit));
        assert!(lost.revision > before.revision);
        output.capture(&key, "session", " reply");
        assert_eq!(output.snapshot(agent).reply.unwrap().text, "first reply");
        output.end(agent, turn);
        let ended = output.snapshot(agent).reply.unwrap();
        let refused = output.usage().refused;
        output.omit(&key, "session");
        assert_eq!(output.snapshot(agent).reply.unwrap(), ended);
        assert_eq!(output.usage().refused, refused + 1);
        output.retire(agent);
        output.omit(&key, "session");
        assert!(output.snapshot(agent).reply.is_none());
        assert_eq!(output.usage().refused, refused + 2);
    }

    #[test]
    fn repeated_pending_omissions_preserve_text_with_bounded_identity_storage() {
        let output = OutputRegistry::new();
        let key = AdoptionKey {
            pid: 50,
            start_time: 60,
            boot: None,
        };
        output.capture(&key, "session", "prefix");
        for _ in 0..1000 {
            output.omit(&key, "session");
        }
        assert_eq!(output.usage().pending_keys, 1);
        assert_eq!(output.usage().chunks, 2);
        assert_eq!(output.usage().text_bytes, 6);
        assert_eq!(output.usage().identity_bytes, 2 * 7);
        let agent = AgentId::from_parts(5, 1);
        let turn = TurnId::from_parts(6, 1);
        output.replace(agent, turn);
        assert!(output.register(agent, turn, &key));
        output.confirm(agent, turn, &SessionId::new("session"));
        let reply = output.snapshot(agent).reply.unwrap();
        assert_eq!(reply.text, "prefix");
        assert_eq!(reply.loss, Some(ReplyLoss::ProviderLimit));
        assert_eq!(output.usage().chunks, 1);
    }

    #[test]
    fn utf8_split_and_checked_sequence_overflow_are_explicit() {
        let text = "🙂".repeat(1_025);
        let pieces: Vec<_> = split_utf8(&text).collect();
        assert_eq!(
            pieces.iter().map(|s| s.len()).collect::<Vec<_>>(),
            [4_096, 4]
        );
        let mut ring = Ring::new(TurnId::NIL, 0);
        ring.next_seq = u64::MAX;
        ring.add_visible("x".into(), 1);
        assert!(ring.overflow);
        assert_eq!(ring.loss, Some(Loss::SequenceOverflow));
        assert!(ring.chunks.is_empty());
    }

    use dudes_providers::steps::{Step, StepKind};

    const SESSION: &str = "session";

    fn open_event(id: &str, session: &str, command: &str) -> StepEvent {
        StepEvent::Open {
            session: SessionId::new(session),
            step: Step {
                id: id.to_owned(),
                parent: None,
                kind: StepKind::Command,
                tool: "Bash".to_owned(),
                args: vec![("command".to_owned(), command.to_owned())],
                edit: None,
                plan: Vec::new(),
            },
        }
    }

    fn close_event(id: &str) -> StepEvent {
        StepEvent::Close {
            session: SessionId::new(SESSION),
            id: id.to_owned(),
            failed: false,
            exit: None,
            result_bytes: 7,
            note: None,
        }
    }

    fn bound() -> (OutputRegistry, AgentId, TurnId, AdoptionKey) {
        let output = OutputRegistry::new();
        let agent = AgentId::from_parts(1, 1);
        let turn = TurnId::from_parts(2, 1);
        let key = AdoptionKey {
            pid: 10,
            start_time: 20,
            boot: None,
        };
        output.replace(agent, turn);
        assert!(output.register(agent, turn, &key));
        (output, agent, turn, key)
    }

    #[test]
    fn steps_wait_for_the_confirmed_session_and_then_appear_once() {
        let (output, agent, turn, key) = bound();
        output.capture_step(&key, open_event("toolu_1", SESSION, "ls"));
        output.capture_step(&key, close_event("toolu_1"));
        output.capture_step(&key, open_event("toolu_2", "another", "pwd"));
        assert!(output.snapshot(agent).steps.is_none());
        assert!(output.steps_since(agent, 0).is_none());
        output.confirm(agent, turn, &SessionId::new(SESSION));
        let delta = output.steps_since(agent, 0).expect("steps once confirmed");
        let ids: Vec<_> = delta.items.iter().map(|step| step.id.as_str()).collect();
        assert_eq!(ids, ["toolu_1"], "a line of another session is refused");
        assert_eq!(delta.items[0].result_bytes, Some(7));
        assert_eq!(output.snapshot(agent).steps.unwrap().seq, delta.seq);
        assert!(
            output
                .steps_since(agent, delta.seq)
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[test]
    fn steps_that_arrive_before_the_turn_is_registered_join_it_later() {
        let output = OutputRegistry::new();
        let agent = AgentId::from_parts(1, 1);
        let turn = TurnId::from_parts(2, 1);
        let key = AdoptionKey {
            pid: 10,
            start_time: 20,
            boot: None,
        };
        output.capture_step(&key, open_event("toolu_1", SESSION, "ls"));
        output.replace(agent, turn);
        assert!(output.register(agent, turn, &key));
        output.confirm(agent, turn, &SessionId::new(SESSION));
        assert_eq!(output.steps_since(agent, 0).unwrap().items.len(), 1);
    }

    #[test]
    fn a_flood_of_steps_never_evicts_the_reply_text() {
        let (output, agent, turn, key) = bound();
        output.capture(&key, SESSION, "the answer");
        output.confirm(agent, turn, &SessionId::new(SESSION));
        for index in 0..600 {
            output.capture_step(&key, open_event(&format!("toolu_{index}"), SESSION, "ls"));
        }
        let delta = output.steps_since(agent, 0).unwrap();
        assert_eq!(delta.items.len(), STEP_RING_ENTRIES);
        assert!(delta.truncated);
        assert_eq!(delta.items[0].id, "toolu_344", "the oldest steps go first");
        assert_eq!(output.snapshot(agent).reply.unwrap().text, "the answer");
    }

    #[test]
    fn the_end_of_a_turn_settles_the_steps_still_running() {
        let (output, agent, turn, key) = bound();
        output.confirm(agent, turn, &SessionId::new(SESSION));
        output.capture_step(&key, open_event("toolu_1", SESSION, "sleep 60"));
        output.end(agent, turn);
        let step = &output.steps_since(agent, 0).unwrap().items[0];
        assert_eq!(step.status, steps::Status::Failed);
        assert_eq!(step.note, Some("the turn ended first"));
    }

    #[test]
    fn a_secret_the_daemon_knows_never_enters_the_step_ring() {
        let redactor = Redactor::for_run();
        redactor.set_source("test", ["synthetic-secret-value-0123456789".to_owned()]);
        let output = OutputRegistry::with_redactor(redactor);
        let agent = AgentId::from_parts(1, 1);
        let turn = TurnId::from_parts(2, 1);
        let key = AdoptionKey {
            pid: 10,
            start_time: 20,
            boot: None,
        };
        output.replace(agent, turn);
        assert!(output.register(agent, turn, &key));
        output.capture_step(
            &key,
            open_event("toolu_1", SESSION, "echo synthetic-secret-value-0123456789"),
        );
        output.confirm(agent, turn, &SessionId::new(SESSION));
        let step = &output.steps_since(agent, 0).unwrap().items[0];
        assert_eq!(step.args[0].value, "echo (redacted)");
    }
}
