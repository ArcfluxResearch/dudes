//! Which ticket starts next, and which moves the daemon owes: pure functions over the board and
//! the supervisor's world. They read no clock and do no I/O, so the same answer comes out live and
//! after a restart, and a crash between two records repairs itself on the next evaluation.

use std::collections::{BTreeMap, BTreeSet};

use dudes_proto::{
    CardView, ExhaustedReason, HoldReason, QueueKey, TicketReason, TicketRow, TicketWait,
};
use dudes_supervisor::world::{AgentRow, World};
use dudes_types::{AgentId, CardId, CardStatus, GoalId, Seq};

/// What the pure functions read.
pub(crate) struct Facts<'a> {
    pub(crate) world: &'a World,
    /// Every ticket on the board, with its card.
    pub(crate) tickets: &'a [(CardView, TicketRow)],
    /// The project each agent was hired into, for the agents whose hire recorded one.
    pub(crate) hires: &'a BTreeMap<AgentId, String>,
    /// The agents with a turn or a check run open, whoever started it.
    pub(crate) working: &'a BTreeSet<AgentId>,
    /// The daemon is shutting down.
    pub(crate) closing: bool,
}

/// A move the daemon owes a ticket, written as System and conditional on the card's sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Owed {
    pub(crate) card: CardId,
    pub(crate) from: CardStatus,
    pub(crate) to: CardStatus,
    pub(crate) seq: Seq,
    pub(crate) reason: Option<TicketReason>,
}

impl Facts<'_> {
    fn ticket(&self, card: CardId) -> Option<&(CardView, TicketRow)> {
        self.tickets.iter().find(|(view, _)| view.id == card)
    }

    /// The agent's goal is unfinished: it has one that is neither cleared nor latched Done. A goal
    /// that ran out of allowance is not finished, because the loop decides it again when its
    /// limit resets.
    fn has_unfinished_goal(&self, row: &AgentRow) -> bool {
        row.goal.is_some_and(|goal| {
            self.world
                .goals
                .get(&goal)
                .is_some_and(|held| !held.cleared)
                && self.world.completed_goal_lsn(goal).is_none()
        })
    }

    /// The ticket's goal is still its holder's unfinished goal.
    pub(crate) fn holds_unfinished_goal(&self, view: &CardView, ticket: &TicketRow) -> bool {
        let (Some(agent), Some(goal)) = (view.claimed_by, ticket.goal) else {
            return false;
        };
        self.world
            .agents
            .get(&agent)
            .is_some_and(|row| row.goal == Some(goal) && self.has_unfinished_goal(row))
    }

    /// The agent and goal to clear before the operator moves a ticket whose goal is still its
    /// holder's unfinished goal, so the old goal does not hold the agent for ever.
    pub(crate) fn goal_to_settle(
        &self,
        view: &CardView,
        ticket: &TicketRow,
    ) -> Option<(AgentId, GoalId)> {
        if !self.holds_unfinished_goal(view, ticket) {
            return None;
        }
        Some((view.claimed_by?, ticket.goal?))
    }

    /// Why `agent` cannot start a ticket now, or `None` when it can.
    pub(crate) fn agent_wait(&self, agent: AgentId, project: &str) -> Option<TicketWait> {
        if self.closing {
            return Some(TicketWait::Closing);
        }
        let Some(row) = self.world.agents.get(&agent).filter(|row| !row.retired) else {
            return Some(TicketWait::AgentBusy);
        };
        if self.hires.get(&agent).map(String::as_str) != Some(project) {
            return Some(TicketWait::AgentBusy);
        }
        if row.halted
            || row.paused
            || row.automatic_hold.is_some()
            || row.last_turn_end == Some(dudes_types::Provenance::Inferred)
        {
            return Some(TicketWait::AgentPaused);
        }
        if row.attached {
            return Some(TicketWait::AgentAttached);
        }
        (self.working.contains(&agent)
            || row.open_turn.is_some()
            || row.open_question.is_some()
            || self.has_unfinished_goal(row))
        .then_some(TicketWait::AgentBusy)
    }

    /// Why the project cannot start another ticket now, or `None` when it is free.
    pub(crate) fn project_wait(&self, project: &str) -> Option<TicketWait> {
        let holder = self
            .tickets
            .iter()
            .filter(|(_, ticket)| ticket.project == project)
            .find(|(view, ticket)| match view.status {
                CardStatus::Doing | CardStatus::Review => true,
                CardStatus::Blocked => self.holds_unfinished_goal(view, ticket),
                _ => false,
            });
        if let Some((view, _)) = holder {
            return Some(TicketWait::Ticket { card: view.id });
        }
        self.hires
            .iter()
            .filter(|(_, hired)| hired.as_str() == project)
            .any(|(agent, _)| {
                self.working.contains(agent)
                    || self
                        .world
                        .agents
                        .get(agent)
                        .is_some_and(|row| row.open_turn.is_some())
            })
            .then_some(TicketWait::ProjectBusy)
    }

    /// The tickets waiting in `project`'s queues, in serving order: given-again work first, then
    /// by the position of the assignment, across every agent hired into the project.
    pub(crate) fn queue(&self, project: &str) -> Vec<(CardId, AgentId, QueueKey)> {
        let mut queue: Vec<_> = self
            .tickets
            .iter()
            .filter(|(view, ticket)| {
                view.status == CardStatus::Claimed && ticket.project == project
            })
            .filter_map(|(view, ticket)| Some((view.id, view.claimed_by?, ticket.queue?)))
            .collect();
        queue.sort_by_key(|(_, _, key)| *key);
        queue
    }

    /// The projects that have a ticket waiting.
    pub(crate) fn queued_projects(&self) -> BTreeSet<&str> {
        self.tickets
            .iter()
            .filter(|(view, _)| view.status == CardStatus::Claimed)
            .map(|(_, ticket)| ticket.project.as_str())
            .collect()
    }

    /// The ticket `project` starts next: none while the project is busy, otherwise the first in
    /// its queue whose agent can take it. An agent that cannot is skipped, so one stuck agent does
    /// not hold the project.
    pub(crate) fn next_ticket(&self, project: &str) -> Option<CardId> {
        if self.project_wait(project).is_some() {
            return None;
        }
        self.queue(project)
            .into_iter()
            .find(|(_, agent, _)| self.agent_wait(*agent, project).is_none())
            .map(|(card, _, _)| card)
    }

    /// Why a ticket is not working yet, or `None` when it is working, finished, or about to start.
    pub(crate) fn wait_of(&self, card: CardId) -> Option<TicketWait> {
        let (view, ticket) = self.ticket(card)?;
        match view.status {
            CardStatus::Todo => Some(self.stopped_or_not_given(ticket)),
            CardStatus::Doing => {
                match self
                    .world
                    .agents
                    .get(&view.claimed_by?)
                    .and_then(|row| row.exhausted)
                {
                    Some(ExhaustedReason::AgentBudget) => Some(TicketWait::AgentLimit),
                    Some(ExhaustedReason::FloorBudget) => Some(TicketWait::FloorLimit),
                    _ => None,
                }
            }
            CardStatus::Claimed => {
                let agent = view.claimed_by?;
                self.agent_wait(agent, &ticket.project)
                    .or_else(|| self.project_wait(&ticket.project))
                    .or_else(|| {
                        self.next_ticket(&ticket.project)
                            .filter(|next| *next != card)
                            .map(|next| TicketWait::Ticket { card: next })
                    })
            }
            _ => None,
        }
    }

    /// A ticket that was stopped while its agent was still in a turn shows that the agent is
    /// finishing; any other unassigned ticket has simply not been given yet.
    fn stopped_or_not_given(&self, ticket: &TicketRow) -> TicketWait {
        let finishing = ticket
            .goal
            .and_then(|goal| self.world.goals.get(&goal))
            .filter(|held| held.cleared)
            .and_then(|held| self.world.agents.get(&held.agent))
            .is_some_and(|row| row.open_turn.is_some());
        if finishing {
            TicketWait::Stopping
        } else {
            TicketWait::NotGiven
        }
    }

    /// The place of `card` in `agent`'s own queue, counting from 1.
    pub(crate) fn position(&self, agent: AgentId, card: CardId) -> Option<u32> {
        let mut own: Vec<_> = self
            .tickets
            .iter()
            .filter(|(view, _)| {
                view.status == CardStatus::Claimed && view.claimed_by == Some(agent)
            })
            .filter_map(|(view, ticket)| Some((ticket.queue?, view.id)))
            .collect();
        own.sort_unstable();
        own.iter()
            .position(|(_, queued)| *queued == card)
            .map(|index| u32::try_from(index + 1).unwrap_or(u32::MAX))
    }

    /// The moves the current state owes, oldest card first. A ticket whose goal the loop has
    /// latched Done owes Ready for you, whether it is Working or was in Needs you. A Working
    /// ticket whose goal cannot go on without the operator owes Needs you.
    pub(crate) fn owed_ticket_moves(&self) -> Vec<Owed> {
        self.tickets
            .iter()
            .filter_map(|(view, ticket)| {
                let goal = ticket.goal?;
                let (to, reason) = if matches!(view.status, CardStatus::Doing | CardStatus::Blocked)
                    && self.world.completed_goal_lsn(goal).is_some()
                {
                    (CardStatus::Review, None)
                } else if view.status == CardStatus::Doing {
                    (CardStatus::Blocked, Some(self.stall(view, goal)?))
                } else {
                    return None;
                };
                Some(Owed {
                    card: view.id,
                    from: view.status,
                    to,
                    seq: view.seq,
                    reason,
                })
            })
            .collect()
    }

    /// Why a Working ticket's goal has stopped without the loop finishing it, when only the
    /// operator can restart it. A limit that resets by itself, and a hold the operator put on the
    /// agent, are not stalls: the operator caused or sees them, and the goal carries on.
    fn stall(&self, view: &CardView, goal: GoalId) -> Option<TicketReason> {
        let row = self.world.agents.get(&view.claimed_by?)?;
        if row.goal != Some(goal) || self.world.goals.get(&goal).is_none_or(|held| held.cleared) {
            return Some(TicketReason::GoalChanged);
        }
        if let Some(exhausted) = row.exhausted {
            return match exhausted {
                ExhaustedReason::GoalBudget => Some(TicketReason::LimitGoal),
                ExhaustedReason::Breaker => Some(TicketReason::Slowed),
                ExhaustedReason::Hops => Some(TicketReason::NoTurns),
                ExhaustedReason::CeilingRequired => Some(TicketReason::CeilingRequired),
                ExhaustedReason::AgentBudget
                | ExhaustedReason::FloorBudget
                | ExhaustedReason::MailWakeLimit => None,
            };
        }
        let hold = row
            .automatic_hold
            .as_ref()
            .map(dudes_proto::AutomaticHold::hold_reason)
            .or_else(|| {
                (row.last_turn_end == Some(dudes_types::Provenance::Inferred))
                    .then_some(HoldReason::InferredTurnEnd)
            })?;
        match hold {
            HoldReason::LaunchRefused | HoldReason::LaunchFailed => Some(TicketReason::HeldLaunch),
            HoldReason::RepeatedFastFailure => Some(TicketReason::HeldFailing),
            HoldReason::InferredTurnEnd | HoldReason::UnknownCause => {
                Some(TicketReason::HeldUnsure)
            }
            HoldReason::WorktreeMissing => Some(TicketReason::WorktreeMissing),
            HoldReason::Halted | HoldReason::Paused | HoldReason::Attached => None,
        }
    }
}
