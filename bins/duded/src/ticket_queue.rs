//! Which ticket starts next, and which moves the daemon owes: pure functions over the board and
//! the supervisor's world. They read no clock and do no I/O, so the same answer comes out live and
//! after a restart, and a crash between two records repairs itself on the next evaluation.

use std::collections::{BTreeMap, BTreeSet};

use dudes_proto::CardView;
use dudes_proto::{QueueKey, TicketRow, TicketWait};
use dudes_supervisor::world::{AgentRow, World};
use dudes_types::{AgentId, CardId, CardStatus, Seq};

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
    fn holds_unfinished_goal(&self, view: &CardView, ticket: &TicketRow) -> bool {
        let (Some(agent), Some(goal)) = (view.claimed_by, ticket.goal) else {
            return false;
        };
        self.world
            .agents
            .get(&agent)
            .is_some_and(|row| row.goal == Some(goal) && self.has_unfinished_goal(row))
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
            CardStatus::Todo => Some(TicketWait::NotGiven),
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
    /// latched Done owes Ready for you, whether it is Working or was in Needs you.
    pub(crate) fn owed_ticket_moves(&self) -> Vec<Owed> {
        self.tickets
            .iter()
            .filter(|(view, _)| matches!(view.status, CardStatus::Doing | CardStatus::Blocked))
            .filter(|(_, ticket)| {
                ticket
                    .goal
                    .is_some_and(|goal| self.world.completed_goal_lsn(goal).is_some())
            })
            .map(|(view, _)| Owed {
                card: view.id,
                from: view.status,
                to: CardStatus::Review,
                seq: view.seq,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use dudes_proto::record::samples;
    use dudes_proto::{Actor, Entry, Goal, GoalBudget, Predicate, Record, RecordKind, TicketCheck};
    use dudes_proto::{QueueClass, QueueKey};
    use dudes_types::{GoalId, Lsn, Timestamp};

    use super::*;

    const PROJECT: &str = "service";

    fn agent(n: u128) -> AgentId {
        AgentId::from_parts(1, n)
    }

    fn entry(record: Record) -> Entry {
        Entry {
            at: Timestamp::from_unix_millis(1),
            record,
        }
    }

    /// A world with these agents hired, and `goals` set for them.
    fn world(agents: &[AgentId], goals: &[(AgentId, GoalId)]) -> World {
        let mut world = World::default();
        let mut lsn = 0;
        for id in agents {
            let mut hired = samples::record(RecordKind::AgentHired);
            if let Record::AgentHired { id: hired_id, .. } = &mut hired {
                *hired_id = *id;
            }
            lsn += 1;
            world.apply(Lsn(lsn), &entry(hired));
        }
        for (owner, goal) in goals {
            lsn += 1;
            world.apply(
                Lsn(lsn),
                &entry(Record::GoalSet {
                    agent: *owner,
                    goal: Goal {
                        id: *goal,
                        text: "work".into(),
                        predicate: Predicate::Command {
                            argv: vec!["/usr/bin/true".into()],
                            timeout_s: 1,
                        },
                        completion_check: None,
                        baseline: None,
                        budget: GoalBudget {
                            usd: None,
                            tokens: None,
                        },
                        hop_cap: 4,
                    },
                    browser_onboarding: None,
                    browser_goal: None,
                    goal_replacement: None,
                }),
            );
        }
        world
    }

    fn ticket(
        n: u128,
        status: CardStatus,
        holder: Option<AgentId>,
        queued_at: Option<u64>,
    ) -> (CardView, TicketRow) {
        (
            CardView {
                id: CardId::from_parts(4, n),
                title: format!("ticket {n}"),
                department: "engineering".into(),
                status,
                status_by: Actor::System,
                seq: Seq::ZERO,
                claimed_by: holder,
                deps: Vec::new(),
            },
            TicketRow {
                description: String::new(),
                project: PROJECT.into(),
                done_when: TicketCheck {
                    id: "default".into(),
                    label: "passes".into(),
                    predicate: Predicate::Command {
                        argv: vec!["/usr/bin/true".into()],
                        timeout_s: 1,
                    },
                },
                goal: None,
                queue: queued_at.map(|lsn| QueueKey {
                    class: QueueClass::New,
                    lsn: Lsn(lsn),
                }),
                message: None,
                round: 0,
                reason: None,
            },
        )
    }

    fn with_facts<T>(
        world: &World,
        tickets: &[(CardView, TicketRow)],
        agents: &[AgentId],
        read: impl FnOnce(&Facts<'_>) -> T,
    ) -> T {
        let hires = agents.iter().map(|id| (*id, PROJECT.to_owned())).collect();
        let working = BTreeSet::new();
        read(&Facts {
            world,
            tickets,
            hires: &hires,
            working: &working,
            closing: false,
        })
    }

    /// Only the queue's own filter keeps a ticket that is already Working out of it.
    #[test]
    fn a_ticket_already_working_is_not_in_a_queue() {
        let (first, world) = (agent(1), world(&[agent(1)], &[]));
        let tickets = [ticket(1, CardStatus::Doing, Some(first), Some(5))];
        let queue = with_facts(&world, &tickets, &[first], |facts| facts.queue(PROJECT));
        assert!(queue.is_empty(), "{queue:?}");
    }

    /// Only the agent's own unfinished goal keeps it from starting: the project is free.
    #[test]
    fn an_agent_with_an_unfinished_goal_starts_nothing() {
        let first = agent(1);
        let world = world(&[first], &[(first, GoalId::from_parts(8, 1))]);
        let tickets = [ticket(1, CardStatus::Claimed, Some(first), Some(5))];
        with_facts(&world, &tickets, &[first], |facts| {
            assert_eq!(facts.project_wait(PROJECT), None);
            assert_eq!(
                facts.agent_wait(first, PROJECT),
                Some(TicketWait::AgentBusy)
            );
            assert_eq!(facts.next_ticket(PROJECT), None);
        });
    }

    /// Only the project's rule keeps a free agent from starting while another ticket is Working
    /// there.
    #[test]
    fn another_ticket_working_in_the_project_holds_it() {
        let (first, second) = (agent(1), agent(2));
        let world = world(&[first, second], &[(first, GoalId::from_parts(8, 1))]);
        let working = ticket(1, CardStatus::Doing, Some(first), None);
        let mut waiting = ticket(2, CardStatus::Claimed, Some(second), Some(6));
        waiting.1.goal = None;
        let tickets = [working.clone(), waiting];
        with_facts(&world, &tickets, &[first, second], |facts| {
            assert_eq!(facts.agent_wait(second, PROJECT), None);
            assert_eq!(
                facts.project_wait(PROJECT),
                Some(TicketWait::Ticket { card: working.0.id })
            );
            assert_eq!(facts.next_ticket(PROJECT), None);
        });
    }
}
