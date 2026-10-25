use std::collections::{BTreeMap, VecDeque};

use dudes_supervisor::host::AdoptionKey;
use dudes_types::{AgentId, TurnId};

use super::ring::{CapturedText, Pending, Ring, SessionGate};
use super::step_ring::StepQueue;
use super::{Loss, MAX_BYTES, MAX_RINGS, ReplyLoss, Unavailable};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ChildKey {
    pub(super) pid: u32,
    pub(super) start_time: u64,
    pub(super) boot: Option<String>,
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

#[derive(Default, Debug)]
pub(super) struct Orphan {
    pub(super) chunks: VecDeque<Pending>,
    pub(super) steps: StepQueue,
    pub(super) bytes: usize,
    pub(super) lost: bool,
    pub(super) finished: bool,
}

pub(super) enum Admission {
    Refused,
    Unbound,
    Bound(Binding),
}

#[derive(Clone, Debug)]
pub(super) struct Binding {
    pub(super) agent: AgentId,
    pub(super) turn: TurnId,
    pub(super) finished: bool,
}

#[derive(Default, Debug)]
pub(super) struct State {
    pub(super) rings: BTreeMap<AgentId, Ring>,
    pub(super) bindings: BTreeMap<ChildKey, Binding>,
    pub(super) orphan: BTreeMap<ChildKey, Orphan>,
    pub(super) closed: VecDeque<ChildKey>,
    pub(super) unavailable: BTreeMap<AgentId, Unavailable>,
    pub(super) unavailable_order: VecDeque<AgentId>,
    pub(super) revision: u128,
    pub(super) arrival: u128,
    pub(super) refused: u64,
    pub(super) finished: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum RetainedText {
    Chunk,
    Pending,
    Reply,
}

impl State {
    pub(super) fn pending_omission(
        &self,
        binding: Option<&Binding>,
        key: &ChildKey,
        session: &str,
    ) -> bool {
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

    pub(super) fn next_arrival(&mut self) -> u128 {
        self.arrival = self
            .arrival
            .checked_add(1)
            .expect("a run cannot have u128::MAX chunks");
        self.arrival
    }

    pub(super) fn refuse(&mut self) {
        self.refused = self.refused.saturating_add(1);
    }

    pub(super) fn remember(&mut self, agent: AgentId, reason: Unavailable) {
        self.unavailable_order.retain(|old| *old != agent);
        self.unavailable_order.push_back(agent);
        self.unavailable.insert(agent, reason);
        while self.unavailable.len() > MAX_RINGS {
            if let Some(old) = self.unavailable_order.pop_front() {
                self.unavailable.remove(&old);
            }
        }
    }

    pub(super) fn close(&mut self, key: ChildKey) {
        self.orphan.remove(&key);
        if !self.closed.contains(&key) {
            self.closed.push_back(key);
        }
        while self.closed.len() > MAX_RINGS {
            self.closed.pop_front();
        }
    }

    pub(super) fn revoke(&mut self, agent: AgentId) {
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

    pub(super) fn make_room(&mut self) {
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
                    let text = value.chunks.front().map(|front| front.arrival);
                    let step = value.steps.items.front().map(|front| front.arrival);
                    text.into_iter()
                        .chain(step)
                        .min()
                        .map(|at| (key.clone(), at))
                })
                .min_by_key(|(_, at)| *at)
                .map(|(key, _)| key)
                .or_else(|| self.orphan.keys().next().cloned());
            if let Some(key) = oldest {
                self.orphan.remove(&key);
                self.close(key);
            }
        }
        while self.text_bytes() + self.step_bytes() > MAX_BYTES {
            if self.drop_oldest_step() {
                continue;
            }
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
    }

    /// Every byte the steps hold, kept, waiting for a session or waiting for a turn.
    pub(super) fn step_bytes(&self) -> usize {
        self.rings
            .values()
            .map(|ring| ring.steps.bytes + ring.steps.results.bytes() + ring.pending_steps.bytes)
            .sum::<usize>()
            + self
                .orphan
                .values()
                .map(|orphan| orphan.steps.bytes)
                .sum::<usize>()
    }

    /// Drops the oldest step change anywhere, so steps give way before any text does. False when
    /// no step is held.
    pub(super) fn drop_oldest_step(&mut self) -> bool {
        enum Held {
            Kept(AgentId),
            Result(AgentId),
            Waiting(AgentId),
            Unbound(ChildKey),
        }
        let kept = self.rings.iter().filter_map(|(agent, ring)| {
            let front = ring.steps.items.front()?;
            Some((front.arrival, Held::Kept(*agent)))
        });
        let results = self.rings.iter().filter_map(|(agent, ring)| {
            Some((ring.steps.results.oldest_arrival()?, Held::Result(*agent)))
        });
        let waiting = self.rings.iter().filter_map(|(agent, ring)| {
            let front = ring.pending_steps.items.front()?;
            Some((front.arrival, Held::Waiting(*agent)))
        });
        let unbound = self.orphan.iter().filter_map(|(key, orphan)| {
            let front = orphan.steps.items.front()?;
            Some((front.arrival, Held::Unbound(key.clone())))
        });
        let Some((_, held)) = kept
            .chain(results)
            .chain(waiting)
            .chain(unbound)
            .min_by_key(|(arrival, _)| *arrival)
        else {
            return false;
        };
        match held {
            Held::Kept(agent) => {
                if let Some(ring) = self.rings.get_mut(&agent) {
                    ring.steps.drop_oldest();
                }
            }
            Held::Result(agent) => {
                if let Some(ring) = self.rings.get_mut(&agent) {
                    ring.steps.results.drop_oldest();
                }
            }
            Held::Waiting(agent) => {
                if let Some(ring) = self.rings.get_mut(&agent) {
                    ring.pending_steps.drop_oldest();
                }
            }
            Held::Unbound(key) => {
                if let Some(orphan) = self.orphan.get_mut(&key) {
                    orphan.steps.drop_oldest();
                }
            }
        }
        true
    }

    pub(super) fn drop_orphan(&mut self, key: &ChildKey) {
        if let Some(orphan) = self.orphan.get_mut(key)
            && let Some(old) = orphan.chunks.pop_front()
        {
            orphan.bytes -= old.body.bytes();
            orphan.lost = true;
        }
    }

    pub(super) fn text_bytes(&self) -> usize {
        self.rings
            .values()
            .map(|r| r.bytes + r.pending_bytes + r.reply.len())
            .sum::<usize>()
            + self.orphan.values().map(|p| p.bytes).sum::<usize>()
    }

    pub(super) fn identity_bytes(&self) -> usize {
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
