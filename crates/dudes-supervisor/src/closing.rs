//! Journal-derived closing participants, acknowledgements and Managed finish observations.

use std::collections::{BTreeMap, BTreeSet};

use dudes_proto::rpc::closing::ClosingSettledReason;
use dudes_proto::{ClosingPhase, Entry, Record};
use dudes_types::{AgentId, GoalId, Lsn, Mode, Provenance, Timestamp, TurnId};

use crate::World;

/// A closing operation that cannot cross the journal boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosingError {
    /// A prior durable launch or persistence continuation is unresolved.
    LaunchPending,
    /// An open journal resource has no complete structural child identity.
    OwnershipUnproven,
    /// The immutable snapshot cannot fit the protocol's accepted bound.
    TooManyParticipants,
    /// A current writer-thread retirement check refused the caller.
    Retired,
    /// The caller did not identify the current immutable closure.
    WrongAnnouncement,
    /// The caller owns no participation in this closure.
    NotParticipant,
    /// Completion is not eligible, or the closure is already terminal.
    NotReady,
}

impl std::fmt::Display for ClosingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::LaunchPending => {
                "a prior launch is unresolved; reconcile it or use hard shutdown"
            }
            Self::OwnershipUnproven => {
                "an open child identity is unproven; reconcile it or use hard shutdown"
            }
            Self::TooManyParticipants => "the closing participant limit is exceeded",
            Self::Retired => "the caller's agent is retired or absent",
            Self::WrongAnnouncement => "the closing announcement does not match",
            Self::NotParticipant => "the caller is not a closing participant",
            Self::NotReady => "closing is not ready to complete",
        })
    }
}

impl std::error::Error for ClosingError {}

/// Existing Floor disclosure excludes participant identity from acknowledgement progress.
#[must_use]
pub const fn floor_phase(phase: ClosingPhase) -> ClosingPhase {
    match phase {
        ClosingPhase::Ack { .. } => ClosingPhase::Announced,
        phase => phase,
    }
}

/// Whether closing time can still be answered by a participant that has not acknowledged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// An acknowledgement, or a saving turn that leads to one, can still arrive.
    Open,
    /// No acknowledgement can arrive, for a reason the journal never reverses.
    Settled(ClosingSettledReason),
}

impl Standing {
    /// The reason, when no acknowledgement can still arrive.
    #[must_use]
    pub const fn reason(self) -> Option<ClosingSettledReason> {
        match self {
            Self::Open => None,
            Self::Settled(reason) => Some(reason),
        }
    }
}

impl World {
    /// Standing of `participant` now, read only from folded state.
    ///
    /// One function serves the saving decision and closing readiness, so a settled participant is
    /// by construction one the saving decision declines for a goal-terminal reason. Nothing is
    /// latched: every call recomputes from the journal.
    #[must_use]
    pub fn closing_standing(&self, agent: AgentId, participant: &ClosingParticipant) -> Standing {
        if participant.mode != Mode::Managed
            || participant.acknowledged.is_some()
            || !participant.unfinished.is_empty()
            || self
                .agents
                .get(&agent)
                .is_some_and(|row| row.open_turn.is_some())
        {
            return Standing::Open;
        }
        let Some(row) = self.agents.get(&agent).filter(|row| !row.retired) else {
            return Standing::Settled(ClosingSettledReason::Retired);
        };
        let reason = match participant.goal {
            None if row.goal.is_none() => ClosingSettledReason::NoGoal,
            None => ClosingSettledReason::GoalReplaced,
            Some(goal) if self.goals.get(&goal).is_none_or(|goal| goal.cleared) => {
                ClosingSettledReason::GoalCleared
            }
            Some(goal) if self.completed_goal_lsn(goal).is_some() => ClosingSettledReason::GoalDone,
            Some(goal) if row.goal != Some(goal) => ClosingSettledReason::GoalReplaced,
            Some(_) => return Standing::Open,
        };
        Standing::Settled(reason)
    }
}

/// Immutable participation with its journal-derived progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosingParticipant {
    /// Display name captured at announcement.
    pub name: String,
    /// Actual goal captured at announcement, when one exists.
    pub goal: Option<GoalId>,
    /// Mode captured at announcement.
    pub mode: Mode,
    /// Explicit own acknowledgement position.
    pub acknowledged: Option<Lsn>,
    /// The participant's one issued saving turn.
    pub saving_turn: Option<TurnId>,
    /// Managed turns whose separate Observed end has not reached the journal.
    pub unfinished: BTreeSet<TurnId>,
}

/// One immutable announcement and its folded progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosingSession {
    /// Stable journal identity.
    pub announcement: Lsn,
    /// Original journal time label.
    pub announced_at: Timestamp,
    /// Latest durable phase.
    pub phase: ClosingPhase,
    /// Immutable participant membership.
    pub participants: BTreeMap<AgentId, ClosingParticipant>,
    /// Completion eligibility position, absent until durable.
    pub complete: Option<Lsn>,
    pub(crate) managed_cleanup: BTreeSet<TurnId>,
    pub(crate) attached_cleanup: BTreeSet<AgentId>,
}

/// Aggregate progress of the immutable closing participants.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClosingCounts {
    /// Participants with a durable acknowledgement.
    pub acknowledged: u32,
    /// Unacknowledged participants with nothing left to save.
    pub settled: u32,
    /// Participants that still owe an acknowledgement.
    pub owed: u32,
    /// Participants with at least one turn awaiting an Observed end.
    pub running: u32,
}

impl ClosingSession {
    pub(crate) fn participant_ids(world: &World) -> BTreeSet<AgentId> {
        world
            .open_children()
            .map(|(_, row)| row.agent)
            .chain(world.attached_agents())
            .filter(|agent| world.agents.get(agent).is_some_and(|row| !row.retired))
            .collect()
    }

    pub(crate) fn capture(world: &World, lsn: Lsn, at: Timestamp) -> Self {
        let participants = Self::participant_ids(world)
            .into_iter()
            .filter_map(|agent| {
                let row = world.agents.get(&agent).filter(|row| !row.retired)?;
                let unfinished = world
                    .open_children()
                    .filter_map(|(turn, row)| (row.agent == agent).then_some(turn))
                    .collect();
                Some((
                    agent,
                    ClosingParticipant {
                        name: bounded_name(&row.name),
                        goal: row.goal,
                        mode: if row.attached {
                            Mode::Attached
                        } else {
                            Mode::Managed
                        },
                        acknowledged: None,
                        saving_turn: None,
                        unfinished,
                    },
                ))
            })
            .collect();
        Self {
            announcement: lsn,
            announced_at: at,
            phase: ClosingPhase::Announced,
            participants,
            complete: None,
            managed_cleanup: BTreeSet::new(),
            attached_cleanup: BTreeSet::new(),
        }
    }

    /// Participants without an explicit durable acknowledgement, in identity order.
    pub fn missing(&self) -> impl Iterator<Item = AgentId> + '_ {
        self.participants
            .iter()
            .filter_map(|(agent, row)| row.acknowledged.is_none().then_some(*agent))
    }

    /// Participants without an acknowledgement that can no longer be asked to save, in identity order.
    pub fn settled<'a>(
        &'a self,
        world: &'a World,
    ) -> impl Iterator<Item = (AgentId, ClosingSettledReason)> + 'a {
        self.participants.iter().filter_map(|(agent, row)| {
            world
                .closing_standing(*agent, row)
                .reason()
                .map(|reason| (*agent, reason))
        })
    }

    /// Whether every participant acknowledged or is settled, and every Managed turn ended Observed.
    #[must_use]
    pub fn ready(&self, world: &World) -> bool {
        let counts = self.counts(world);
        counts.owed == 0 && counts.running == 0
    }

    /// Count folded acknowledgements, settled standing and outstanding turns without identities.
    #[must_use]
    pub fn counts(&self, world: &World) -> ClosingCounts {
        let mut counts = ClosingCounts::default();
        for (agent, row) in &self.participants {
            if row.acknowledged.is_some() {
                counts.acknowledged += 1;
            } else if world.closing_standing(*agent, row) != Standing::Open {
                counts.settled += 1;
            } else {
                counts.owed += 1;
            }
            if !row.unfinished.is_empty() {
                counts.running += 1;
            }
        }
        counts
    }

    /// Whether completion eligibility is durable.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete.is_some()
    }

    /// Whether resources captured at Complete still need durable reconciliation.
    #[must_use]
    pub fn needs_recovery(&self) -> bool {
        !self.managed_cleanup.is_empty() || !self.attached_cleanup.is_empty()
    }

    pub(crate) fn complete_resources(
        &mut self,
        managed: BTreeSet<TurnId>,
        attached: BTreeSet<AgentId>,
    ) {
        self.managed_cleanup = managed;
        self.managed_cleanup.extend(
            self.participants
                .values()
                .flat_map(|row| row.unfinished.iter().copied()),
        );
        self.attached_cleanup = attached;
    }

    pub(crate) fn fold(&mut self, lsn: Lsn, entry: &Entry) {
        match &entry.record {
            Record::ClosingTime {
                phase: ClosingPhase::Ack { agent },
            } if self.complete.is_none() => {
                if let Some(participant) = self.participants.get_mut(agent)
                    && participant.acknowledged.is_none()
                {
                    participant.acknowledged = Some(lsn);
                    self.phase = ClosingPhase::Ack { agent: *agent };
                }
            }
            Record::ClosingTime {
                phase: ClosingPhase::Complete,
            } if self.complete.is_none() => {
                self.complete = Some(lsn);
                self.phase = ClosingPhase::Complete;
            }
            Record::TurnIssued { agent, turn, .. } if self.complete.is_none() => {
                if let Some(participant) = self.participants.get_mut(agent)
                    && participant.saving_turn.is_none()
                {
                    participant.saving_turn = Some(*turn);
                    participant.unfinished.insert(*turn);
                }
            }
            Record::TurnEnded {
                agent,
                turn,
                mode: Mode::Managed,
                provenance: Provenance::Observed,
                ..
            } => {
                if let Some(participant) = self.participants.get_mut(agent) {
                    participant.unfinished.remove(turn);
                }
                self.managed_cleanup.remove(turn);
            }
            Record::Detached { agent, .. } => {
                self.attached_cleanup.remove(agent);
            }
            _ => {}
        }
    }
}

fn bounded_name(name: &str) -> String {
    let mut end = name.len().min(256);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_owned()
}
