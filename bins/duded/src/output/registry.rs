use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use dudes_providers::steps::StepEvent;
use dudes_supervisor::host::AdoptionKey;
use dudes_supervisor::{Redactor, StreamScrubber};
use dudes_types::{AgentId, SessionId, TurnId};

use super::ring::{CapturedText, Pending, Ring, SessionGate, split_utf8};
use super::state::{Admission, Binding, ChildKey, State};
use super::step_ring::{PendingStep, StepOp, StepQueue};
use super::{
    Loss, MAX_SESSION_BYTES, OutputReply, OutputSnapshot, OutputState, OutputUsage, RING_BYTES,
    RING_CHUNKS, ReplyLoss, StepsDelta, StepsMark, Unavailable,
};
use crate::step_results::{self, Fetch};
use crate::steps::{self, Closing};

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

    pub(super) fn state(&self) -> MutexGuard<'_, State> {
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
    /// has one. A refusal is counted.
    fn admit(state: &mut State, key: &ChildKey, session: &str) -> Admission {
        if session.is_empty() {
            return Admission::Refused;
        }
        if session.len() > MAX_SESSION_BYTES || state.closed.contains(key) {
            state.refuse();
            return Admission::Refused;
        }
        let binding = state.bindings.get(key).cloned();
        if let Some(binding) = &binding {
            if binding.finished
                || state.rings.get(&binding.agent).is_none_or(|r| {
                    r.turn != binding.turn || r.key.as_ref() != Some(key) || r.ended || r.overflow
                })
            {
                state.refuse();
                return Admission::Refused;
            }
            match &state
                .rings
                .get(&binding.agent)
                .expect("checked binding")
                .gate
            {
                SessionGate::Admitted(accepted) if accepted.as_str() != session => {
                    state.refuse();
                    return Admission::Refused;
                }
                SessionGate::RefusedOvercap => {
                    state.refuse();
                    return Admission::Refused;
                }
                SessionGate::Unconfirmed | SessionGate::Admitted(_) => {}
            }
        }
        if state.orphan.get(key).is_some_and(|orphan| orphan.finished) {
            state.refuse();
            return Admission::Refused;
        }
        binding.map_or(Admission::Unbound, Admission::Bound)
    }

    fn capture_items(
        &self,
        key: &AdoptionKey,
        session: &str,
        items: impl Iterator<Item = CapturedText>,
    ) {
        let key = ChildKey::from(key);
        let mut state = self.state();
        let binding = match Self::admit(&mut state, &key, session) {
            Admission::Refused => return,
            Admission::Unbound => None,
            Admission::Bound(binding) => Some(binding),
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
            StepEvent::Open { step, .. } => {
                let (view, edit) = steps::open_with_text(&self.redactor, step, now);
                StepOp::Open {
                    view: Box::new(view),
                    edit,
                }
            }
            StepEvent::Close {
                id,
                failed,
                exit,
                result_bytes,
                result,
                ..
            } => StepOp::Close(Closing {
                id: steps::scrub_id(&self.redactor, &id),
                failed,
                exit,
                result_bytes,
                result: result.map(|text| steps::keep_result(&self.redactor, &text)),
                at: now,
            }),
        };
        let key = ChildKey::from(key);
        let mut state = self.state();
        let binding = match Self::admit(&mut state, &key, &session) {
            Admission::Refused => return,
            Admission::Unbound => None,
            Admission::Bound(binding) => Some(binding),
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

    /// The kind of the step each running agent is in now, newest running step first.
    pub(crate) fn step_labels(&self) -> BTreeMap<AgentId, &'static str> {
        self.state()
            .rings
            .iter()
            .filter(|(_, ring)| matches!(ring.gate, SessionGate::Admitted(_)) && !ring.ended)
            .filter_map(|(agent, ring)| {
                ring.steps
                    .items
                    .iter()
                    .rev()
                    .find(|step| step.view.status == steps::Status::Running)
                    .map(|step| (*agent, step.view.kind))
            })
            .collect()
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
                .map(|step| step.view.clone().rescrubbed(&self.redactor))
                .collect(),
        })
    }

    /// The text one step of `agent`'s latest turn kept, for the page that selected the agent.
    pub(crate) fn step_text(&self, agent: AgentId, turn: TurnId, id: &str) -> Fetch {
        let state = self.state();
        let Some(ring) = state
            .rings
            .get(&agent)
            .filter(|ring| ring.turn == turn && matches!(ring.gate, SessionGate::Admitted(_)))
        else {
            return Fetch::Gone;
        };
        let id = steps::scrub_id(&self.redactor, id);
        let Some(step) = ring.steps.items.iter().find(|step| step.view.id == id) else {
            return Fetch::Gone;
        };
        step_results::fetch(&self.redactor, &step.view, ring.steps.results.get(&id))
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
        ring.chunks.push_back(super::ring::Stored {
            chunk: super::OutputChunk {
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
