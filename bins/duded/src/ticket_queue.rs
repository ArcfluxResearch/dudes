//! Which ticket starts next, and which moves the daemon owes: pure functions over the board and
//! the supervisor's world. They read no clock and do no I/O, so the same answer comes out live and
//! after a restart, and a crash between two records repairs itself on the next evaluation.

use std::collections::{BTreeMap, BTreeSet};

use dudes_proto::repository::HireRepositories;
use dudes_proto::{
    CardView, ExhaustedReason, HoldReason, QueueKey, TicketReason, TicketRow, TicketWait,
    WorktreeRemoval,
};
use dudes_supervisor::world::{AgentRow, World};
use dudes_types::{AgentId, CardId, CardStatus, GoalId, Seq, WorktreeId};

/// Where an agent works: the lease of its own copy, or `None` for the project's shared folder.
pub(crate) type Tree = Option<WorktreeId>;

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
        if self.working.contains(&agent)
            || row.open_turn.is_some()
            || row.open_question.is_some()
            || self.has_unfinished_goal(row)
        {
            return Some(TicketWait::AgentBusy);
        }
        row.goal
            .filter(|goal| self.world.instructions.pending_for_goal(*goal))
            .map(|goal| {
                self.tickets
                    .iter()
                    .find(|(view, ticket)| {
                        ticket.repositories.project() == project
                            && ticket.goal == Some(goal)
                            && view.claimed_by == Some(agent)
                    })
                    .map_or(TicketWait::AgentBusy, |(view, _)| TicketWait::Ticket {
                        card: view.id,
                    })
            })
    }

    /// Whether every agent of `project` shares one working tree: the project records more than one
    /// repository, or an agent still hired into it holds a multi-repo hire. Such a project runs
    /// one ticket at a time.
    fn is_one_tree(&self, project: &str) -> bool {
        let catalogued = self
            .world
            .repository_projects
            .get(project)
            .and_then(|history| history.revisions.values().next_back())
            .is_some_and(|latest| latest.entries().len() > 1);
        catalogued
            || self.hires.iter().any(|(agent, hired)| {
                hired == project && self.world.agents.get(agent).is_some_and(|row| {
                    !row.retired
                        && matches!(
                            &row.repositories,
                            HireRepositories::Scoped(set) if set.repositories.entries().len() > 1
                        )
                })
            })
    }

    /// The working tree of `agent` hired into `project`: the copy its hire leased, whatever the
    /// lease's state, or `None` for the project's shared folder. A failed lease belongs to no agent.
    #[cfg(test)]
    pub(crate) fn tree_of(&self, agent: AgentId, project: &str) -> Tree {
        self.tree_in(agent, self.is_one_tree(project))
    }

    /// `tree_of` for a project whose `is_one_tree` the caller already computed.
    fn tree_in(&self, agent: AgentId, one_tree: bool) -> Tree {
        if one_tree {
            return None;
        }
        let lease = self
            .world
            .agents
            .get(&agent)?
            .repositories
            .primary_worktree()?;
        self.world
            .worktrees
            .get(&lease)
            .filter(|row| row.removed != Some(WorktreeRemoval::NeverAssigned))
            .map(|_| lease)
    }

    /// Why the working tree `tree` of `project` cannot start another ticket now, or `None` when
    /// it is free.
    #[cfg(test)]
    pub(crate) fn tree_wait(&self, project: &str, tree: Tree) -> Option<TicketWait> {
        self.tree_wait_in(project, tree, self.is_one_tree(project))
    }

    fn tree_wait_in(&self, project: &str, tree: Tree, one_tree: bool) -> Option<TicketWait> {
        let in_tree = |agent: Option<AgentId>| {
            agent.is_none_or(|agent| self.tree_in(agent, one_tree) == tree)
        };
        let holder = self
            .tickets
            .iter()
            .filter(|(_, ticket)| ticket.repositories.project() == project)
            .filter(|(view, _)| in_tree(view.claimed_by))
            .find(|(view, ticket)| match view.status {
                CardStatus::Doing | CardStatus::Review => true,
                CardStatus::Blocked => {
                    self.holds_unfinished_goal(view, ticket)
                        || ticket
                            .goal
                            .is_some_and(|goal| self.world.instructions.pending_for_goal(goal))
                }
                _ => false,
            });
        if let Some((view, _)) = holder {
            return Some(TicketWait::Ticket { card: view.id });
        }
        self.hires
            .iter()
            .filter(|(agent, hired)| hired.as_str() == project && in_tree(Some(**agent)))
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

    /// The tickets waiting in `project`'s queues, in serving order: given-again work first, then by
    /// priority, then by the position of the assignment, across every agent hired into the project.
    pub(crate) fn queue(&self, project: &str) -> Vec<(CardId, AgentId, QueueKey)> {
        let mut queue: Vec<_> = self
            .tickets
            .iter()
            .filter(|(view, ticket)| {
                view.status == CardStatus::Claimed && ticket.repositories.project() == project
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
            .map(|(_, ticket)| ticket.repositories.project())
            .collect()
    }

    /// The tickets `project` starts now, at most one per free working tree: walking its queue in
    /// order, the first whose agent can take it in each tree that no ticket or turn holds. An
    /// agent that cannot is skipped, so one stuck agent does not hold its tree.
    pub(crate) fn next_tickets(&self, project: &str) -> Vec<CardId> {
        let one_tree = self.is_one_tree(project);
        let mut taken = BTreeSet::new();
        let mut next = Vec::new();
        for (card, agent, _) in self.queue(project) {
            let tree = self.tree_in(agent, one_tree);
            if taken.contains(&tree)
                || self.tree_wait_in(project, tree, one_tree).is_some()
                || self.agent_wait(agent, project).is_some()
            {
                continue;
            }
            taken.insert(tree);
            next.push(card);
        }
        next
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
                let project = ticket.repositories.project();
                let one_tree = self.is_one_tree(project);
                let tree = self.tree_in(agent, one_tree);
                self.agent_wait(agent, project)
                    .or_else(|| self.tree_wait_in(project, tree, one_tree))
                    .or_else(|| {
                        let starting = self.next_tickets(project);
                        starting
                            .into_iter()
                            .find(|next| {
                                *next != card
                                    && self
                                        .ticket(*next)
                                        .and_then(|(other, _)| other.claimed_by)
                                        .is_some_and(|other| self.tree_in(other, one_tree) == tree)
                            })
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
                let recovery = crate::ticket_recovery::binding(self.world, view, ticket)
                    .map_or(crate::ticket_recovery::RecoveryProgress::None, |binding| {
                        crate::ticket_recovery::progress(self.world, binding)
                    });
                let waiting = recovery == crate::ticket_recovery::RecoveryProgress::Waiting;
                if waiting
                    && matches!(view.status, CardStatus::Doing | CardStatus::Blocked)
                    && ticket.reason != Some(TicketReason::RecoveryRequired)
                    && self.world.completed_goal_lsn(goal).is_none()
                {
                    return Some(Owed {
                        card: view.id,
                        from: view.status,
                        to: CardStatus::Blocked,
                        seq: view.seq,
                        reason: Some(TicketReason::RecoveryRequired),
                    });
                }
                if self.world.instructions.pending_for_goal(goal) {
                    return None;
                }
                let (to, reason) = if matches!(view.status, CardStatus::Doing | CardStatus::Blocked)
                    && self.world.completed_goal_lsn(goal).is_some()
                {
                    (CardStatus::Review, None)
                } else if view.status == CardStatus::Blocked
                    && ticket.reason == Some(TicketReason::RecoveryRequired)
                    && recovery == crate::ticket_recovery::RecoveryProgress::Admitted
                    && self.stall(view, goal).is_none()
                {
                    (CardStatus::Doing, None)
                } else if view.status == CardStatus::Doing && waiting {
                    (CardStatus::Blocked, Some(TicketReason::RecoveryRequired))
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
            HoldReason::SharedInstructionsReview => self.world.instructions.ticket_reason(view.id),
            HoldReason::Halted | HoldReason::Paused | HoldReason::Attached => None,
        }
    }
}

#[cfg(test)]
#[path = "ticket_queue_review_tests.rs"]
mod review_tests;

#[cfg(test)]
mod tests {
    use dudes_proto::record::samples;
    use dudes_proto::{Actor, Entry, Goal, GoalBudget, Predicate, Record, RecordKind, TicketCheck};
    use dudes_proto::{Priority, QueueClass, QueueKey};
    use dudes_types::{GoalId, Lsn, Timestamp, WorktreeId};

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

    fn goal_set(owner: AgentId, goal: GoalId) -> Record {
        Record::GoalSet {
            agent: owner,
            goal: Goal {
                id: goal,
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
            world.apply(Lsn(lsn), &entry(goal_set(*owner, *goal)));
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
                ticket: None,
            },
            TicketRow {
                instruction_review: None,
                coverage: None,
                description: String::new(),
                repositories: PROJECT.into(),
                done_when: TicketCheck {
                    id: "default".into(),
                    label: "passes".into(),
                    predicate: Predicate::Command {
                        argv: vec!["/usr/bin/true".into()],
                        timeout_s: 1,
                    },
                },
                priority: Priority::Unset,
                goal: None,
                queue: queued_at.map(|lsn| QueueKey {
                    class: QueueClass::New,
                    lsn: Lsn(lsn),
                    rank: 2,
                }),
                message: None,
                round: 0,
                reason: None,
                summary: None,
                last_turn_end: None,
                check: None,
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
            assert_eq!(facts.tree_wait(PROJECT, None), None);
            assert_eq!(
                facts.agent_wait(first, PROJECT),
                Some(TicketWait::AgentBusy)
            );
            assert!(facts.next_tickets(PROJECT).is_empty());
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
                facts.tree_wait(PROJECT, None),
                Some(TicketWait::Ticket { card: working.0.id })
            );
            assert!(facts.next_tickets(PROJECT).is_empty());
        });
    }

    fn exhaust(world: &mut World, lsn: u64, agent: AgentId) {
        world.apply(
            Lsn(lsn),
            &entry(Record::LoopDecided {
                agent,
                input_digest: String::new(),
                decision: dudes_proto::LoopDecision::Exhausted {
                    reason: ExhaustedReason::Hops,
                },
                gate_trace: Vec::new(),
            }),
        );
    }

    /// Only the project's rule keeps a free agent from starting while another ticket sits in
    /// Needs you with its goal still the stalled agent's.
    #[test]
    fn a_stalled_ticket_alone_holds_the_project() {
        let (first, second) = (agent(1), agent(2));
        let goal = GoalId::from_parts(8, 1);
        let mut world = world(&[first, second], &[(first, goal)]);
        exhaust(&mut world, 10, first);
        let mut stalled = ticket(1, CardStatus::Blocked, Some(first), None);
        stalled.1.goal = Some(goal);
        let tickets = [
            stalled.clone(),
            ticket(2, CardStatus::Claimed, Some(second), Some(6)),
        ];
        with_facts(&world, &tickets, &[first, second], |facts| {
            assert_eq!(facts.agent_wait(second, PROJECT), None);
            assert_eq!(
                facts.tree_wait(PROJECT, None),
                Some(TicketWait::Ticket { card: stalled.0.id })
            );
            assert!(facts.next_tickets(PROJECT).is_empty());
        });
    }

    /// Only the agent's own rule keeps it from starting while its goal has run out: nothing else
    /// holds the project, and an exhausted goal is not finished.
    #[test]
    fn an_exhausted_goal_alone_holds_its_agent() {
        let first = agent(1);
        let mut world = world(&[first], &[(first, GoalId::from_parts(8, 1))]);
        exhaust(&mut world, 10, first);
        let tickets = [ticket(2, CardStatus::Claimed, Some(first), Some(6))];
        with_facts(&world, &tickets, &[first], |facts| {
            assert_eq!(facts.tree_wait(PROJECT, None), None);
            assert_eq!(
                facts.agent_wait(first, PROJECT),
                Some(TicketWait::AgentBusy)
            );
            assert!(facts.next_tickets(PROJECT).is_empty());
        });
    }

    fn prioritized(
        mut waiting: (CardView, TicketRow),
        priority: Priority,
    ) -> (CardView, TicketRow) {
        waiting.1.priority = priority;
        if let Some(key) = &mut waiting.1.queue {
            key.rank = priority.rank();
        }
        waiting
    }

    /// The project's queue, each agent's own places and the next ticket to start all follow one
    /// key: rework, then priority, then the position of the assignment.
    #[test]
    fn the_queue_and_every_place_in_it_follow_priority_then_age() {
        let (first, second) = (agent(1), agent(2));
        let world = world(&[first, second], &[]);
        let mut rework = ticket(6, CardStatus::Claimed, Some(first), Some(50));
        rework.1.queue = rework.1.queue.map(|key| QueueKey {
            class: QueueClass::Rework,
            ..key
        });
        let tickets = [
            prioritized(
                ticket(1, CardStatus::Claimed, Some(first), Some(10)),
                Priority::Low,
            ),
            prioritized(
                ticket(2, CardStatus::Claimed, Some(second), Some(11)),
                Priority::Unset,
            ),
            prioritized(
                ticket(3, CardStatus::Claimed, Some(first), Some(12)),
                Priority::Urgent,
            ),
            prioritized(
                ticket(4, CardStatus::Claimed, Some(second), Some(13)),
                Priority::Medium,
            ),
            prioritized(
                ticket(5, CardStatus::Claimed, Some(second), Some(14)),
                Priority::High,
            ),
            prioritized(rework, Priority::Low),
        ];
        let card = |n: u128| CardId::from_parts(4, n);
        with_facts(&world, &tickets, &[first, second], |facts| {
            let queue: Vec<_> = facts
                .queue(PROJECT)
                .into_iter()
                .map(|(card, agent, _)| (card, agent))
                .collect();
            assert_eq!(
                queue,
                [
                    (card(6), first),
                    (card(3), first),
                    (card(5), second),
                    (card(2), second),
                    (card(4), second),
                    (card(1), first),
                ]
            );
            for agent in [first, second] {
                let own: Vec<_> = queue
                    .iter()
                    .filter(|(_, holder)| *holder == agent)
                    .map(|(card, _)| *card)
                    .collect();
                for (index, queued) in own.iter().enumerate() {
                    assert_eq!(
                        facts.position(agent, *queued),
                        u32::try_from(index + 1).ok(),
                        "{queued}"
                    );
                }
            }
            assert_eq!(facts.next_tickets(PROJECT), [card(6)]);
        });
    }

    /// Priority orders what is waiting and starts nothing: a free agent's Low ticket still starts
    /// while an Urgent ticket waits behind its own busy agent.
    #[test]
    fn an_urgent_ticket_behind_a_busy_agent_does_not_hold_a_free_agents_low_one() {
        let (busy, free) = (agent(1), agent(2));
        let world = world(&[busy, free], &[(busy, GoalId::from_parts(8, 1))]);
        let tickets = [
            prioritized(
                ticket(1, CardStatus::Claimed, Some(busy), Some(10)),
                Priority::Urgent,
            ),
            prioritized(
                ticket(2, CardStatus::Claimed, Some(free), Some(11)),
                Priority::Low,
            ),
        ];
        let hires = [busy, free]
            .iter()
            .map(|id| (*id, PROJECT.to_owned()))
            .collect();
        let working = BTreeSet::new();
        let facts = Facts {
            world: &world,
            tickets: &tickets,
            hires: &hires,
            working: &working,
            closing: false,
        };
        assert_eq!(facts.queue(PROJECT)[0].0, CardId::from_parts(4, 1));
        assert_eq!(facts.next_tickets(PROJECT), [CardId::from_parts(4, 2)]);
    }

    /// A world with each agent hired, some into a copy of their own (the lease number), and the
    /// lease records ahead of the hires that name them.
    fn copies(agents: &[(AgentId, Option<u128>)]) -> World {
        let mut world = World::default();
        let mut lsn = 0;
        for (id, copy) in agents {
            if let Some(n) = copy {
                let mut leased = samples::record(RecordKind::WorktreeLeased);
                if let Record::WorktreeLeased { worktree, .. } = &mut leased {
                    *worktree = WorktreeId::from_bytes((*n).to_be_bytes());
                }
                lsn += 1;
                world.apply(Lsn(lsn), &entry(leased));
            }
            let mut hired = samples::record(RecordKind::AgentHired);
            if let Record::AgentHired {
                id: hired_id,
                worktree,
                ..
            } = &mut hired
            {
                *hired_id = *id;
                *worktree = copy.map(|n| WorktreeId::from_bytes(n.to_be_bytes()));
            }
            lsn += 1;
            world.apply(Lsn(lsn), &entry(hired));
        }
        world
    }

    /// A hire that owns copies of two repositories.
    fn two_repositories() -> HireRepositories {
        use dudes_proto::record::{Hex32, TrustPin};
        use dudes_proto::repository::{OwnedRepository, ScopedHireRepositories};
        use dudes_proto::repository::{RepoId, RepositoryEntry, RepositorySet};
        let Record::AgentHiredWithRepositories(hire) =
            samples::record(RecordKind::AgentHiredWithRepositories)
        else {
            unreachable!("the sample is a repository hire");
        };
        let recorded = hire.repositories;
        let pin = TrustPin {
            claude_tree: None,
            mcp_json: None,
            attributes: Hex32::new([7; 32]),
            config: Hex32::new([8; 32]),
        };
        // Only replay builds a hired copy, after validating its trust record, so decode one.
        let hired = |repo_id, value: OwnedRepository| RepositoryEntry {
            repo_id,
            value: serde_json::from_value(
                serde_json::json!({ "recorded": value, "original_pin": pin }),
            )
            .unwrap(),
        };
        let first = recorded.repositories.entries()[0].clone();
        let mut second = first.value.clone();
        second.worktree = WorktreeId::from_bytes([9; 16]);
        let entries = vec![
            hired(first.repo_id.clone(), first.value),
            hired(RepoId::parse("docs").unwrap(), second),
        ];
        HireRepositories::Scoped(ScopedHireRepositories {
            project: recorded.project,
            catalog_lsn: recorded.catalog_lsn,
            policy: recorded.policy,
            repositories: RepositorySet::new(first.repo_id, entries).unwrap(),
        })
    }

    fn card(n: u128) -> CardId {
        CardId::from_parts(4, n)
    }

    /// Agents in their own copies do not hold each other, while agents that share the folder do.
    #[test]
    fn one_evaluation_starts_one_ticket_in_each_free_tree() {
        let [a, b, c, d] = [agent(1), agent(2), agent(3), agent(4)];
        let world = copies(&[(a, Some(11)), (b, Some(12)), (c, None), (d, None)]);
        let tickets = [
            ticket(1, CardStatus::Claimed, Some(a), Some(10)),
            ticket(2, CardStatus::Claimed, Some(b), Some(11)),
            ticket(3, CardStatus::Claimed, Some(c), Some(12)),
            ticket(4, CardStatus::Claimed, Some(d), Some(13)),
        ];
        with_facts(&world, &tickets, &[a, b, c, d], |facts| {
            assert_eq!(facts.next_tickets(PROJECT), [card(1), card(2), card(3)]);
        });
    }

    /// The same facts, built twice from the same records, give the same starts in the same order.
    #[test]
    fn the_same_facts_give_the_same_starts() {
        let [a, b, c, d] = [agent(1), agent(2), agent(3), agent(4)];
        let tickets = [
            ticket(4, CardStatus::Claimed, Some(d), Some(13)),
            ticket(3, CardStatus::Claimed, Some(c), Some(12)),
            ticket(2, CardStatus::Claimed, Some(b), Some(11)),
            ticket(1, CardStatus::Claimed, Some(a), Some(10)),
        ];
        for _ in 0..2 {
            let world = copies(&[(a, Some(11)), (b, Some(12)), (c, None), (d, None)]);
            with_facts(&world, &tickets, &[a, b, c, d], |facts| {
                assert_eq!(facts.next_tickets(PROJECT), [card(1), card(2), card(3)]);
            });
        }
    }

    /// An agent's Ready-for-you ticket holds its own copy, so its next ticket waits and names it,
    /// while another copy and the shared folder start theirs.
    #[test]
    fn a_ready_ticket_holds_only_its_own_copy_and_the_wait_names_it() {
        let [a, b, c] = [agent(1), agent(2), agent(3)];
        let world = copies(&[(a, Some(11)), (b, Some(12)), (c, None)]);
        let tickets = [
            ticket(1, CardStatus::Review, Some(a), None),
            ticket(2, CardStatus::Claimed, Some(a), Some(10)),
            ticket(3, CardStatus::Claimed, Some(b), Some(11)),
            ticket(4, CardStatus::Claimed, Some(c), Some(12)),
        ];
        with_facts(&world, &tickets, &[a, b, c], |facts| {
            assert_eq!(facts.next_tickets(PROJECT), [card(3), card(4)]);
            assert_eq!(
                facts.wait_of(card(2)),
                Some(TicketWait::Ticket { card: card(1) })
            );
            assert_eq!(facts.wait_of(card(3)), None);
        });
    }

    /// A shared-folder agent waits for the ticket that holds the folder, not for one in a copy.
    #[test]
    fn a_shared_folder_ticket_names_the_folders_holder() {
        let [a, b, c] = [agent(1), agent(2), agent(3)];
        let world = copies(&[(a, Some(11)), (b, None), (c, None)]);
        let tickets = [
            ticket(1, CardStatus::Doing, Some(a), None),
            ticket(2, CardStatus::Doing, Some(b), None),
            ticket(3, CardStatus::Claimed, Some(c), Some(10)),
        ];
        with_facts(&world, &tickets, &[a, b, c], |facts| {
            assert_eq!(
                facts.wait_of(card(3)),
                Some(TicketWait::Ticket { card: card(2) })
            );
        });
    }

    /// A retired agent's released copy is still its tree, and a lease no agent holds is the shared
    /// folder.
    #[test]
    fn a_retired_agents_copy_stays_its_tree() {
        let [a, b] = [agent(1), agent(2)];
        let mut world = copies(&[(a, Some(11)), (b, Some(12))]);
        world.apply(
            Lsn(50),
            &entry(Record::AgentRetired {
                id: a,
                reason: "done".into(),
                browser_retire: None,
            }),
        );
        let tickets = [
            ticket(1, CardStatus::Review, Some(a), None),
            ticket(2, CardStatus::Claimed, Some(b), Some(10)),
        ];
        with_facts(&world, &tickets, &[a, b], |facts| {
            assert_eq!(
                facts.tree_of(a, PROJECT),
                Some(WorktreeId::from_bytes(11_u128.to_be_bytes()))
            );
            assert_eq!(facts.next_tickets(PROJECT), [card(2)]);
        });
    }

    /// A project with a multi-repo hire is one tree, even beside a hire in its own copy.
    #[test]
    fn a_multi_repo_hire_keeps_the_project_to_one_ticket() {
        let [a, b] = [agent(1), agent(2)];
        let mut world = copies(&[(a, Some(11)), (b, Some(12))]);
        world.agents.get_mut(&b).unwrap().repositories = two_repositories();
        let tickets = [
            ticket(1, CardStatus::Doing, Some(a), None),
            ticket(2, CardStatus::Claimed, Some(b), Some(10)),
        ];
        with_facts(&world, &tickets, &[a, b], |facts| {
            assert_eq!(facts.tree_of(a, PROJECT), None);
            assert!(facts.next_tickets(PROJECT).is_empty());
            assert_eq!(
                facts.wait_of(card(2)),
                Some(TicketWait::Ticket { card: card(1) })
            );
        });
    }

    /// Two recorded repositories make the project one tree, with no multi-repo hire at all.
    #[test]
    fn a_catalog_of_two_repositories_keeps_the_project_to_one_ticket() {
        use dudes_proto::repository::CatalogRepository;
        use dudes_proto::repository_state::history::ProjectRepositoryHistory;
        let [a, b] = [agent(1), agent(2)];
        let mut world = copies(&[(a, Some(11)), (b, Some(12))]);
        let HireRepositories::Scoped(scoped) = two_repositories() else {
            unreachable!("two repositories are a scoped hire");
        };
        let entries = scoped
            .repositories
            .entries()
            .iter()
            .map(|entry| dudes_proto::repository::RepositoryEntry {
                repo_id: entry.repo_id.clone(),
                value: CatalogRepository {
                    label: "repo".into(),
                    origin: entry.value.recorded.origin.clone(),
                },
            })
            .collect();
        let catalog = dudes_proto::repository::RepositorySet::new(
            scoped.repositories.primary_id().clone(),
            entries,
        )
        .unwrap();
        world.repository_projects.insert(
            PROJECT.into(),
            ProjectRepositoryHistory {
                project: PROJECT.into(),
                revisions: BTreeMap::from([(Lsn(1), catalog)]),
                bindings: BTreeMap::new(),
            },
        );
        let tickets = [
            ticket(1, CardStatus::Claimed, Some(a), Some(10)),
            ticket(2, CardStatus::Claimed, Some(b), Some(11)),
        ];
        with_facts(&world, &tickets, &[a, b], |facts| {
            assert_eq!(facts.tree_of(b, PROJECT), None);
            assert_eq!(facts.next_tickets(PROJECT), [card(1)]);
        });
    }

    /// A retired multi-repo hire no longer forces one tree.
    #[test]
    fn a_retired_multi_repo_hire_no_longer_forces_one_tree() {
        let [a, b, c] = [agent(1), agent(2), agent(3)];
        let mut world = copies(&[(a, Some(11)), (b, Some(12)), (c, Some(13))]);
        world.agents.get_mut(&c).unwrap().repositories = two_repositories();
        world.apply(
            Lsn(50),
            &entry(Record::AgentRetired {
                id: c,
                reason: "done".into(),
                browser_retire: None,
            }),
        );
        let tickets = [
            ticket(1, CardStatus::Claimed, Some(a), Some(10)),
            ticket(2, CardStatus::Claimed, Some(b), Some(11)),
        ];
        with_facts(&world, &tickets, &[a, b, c], |facts| {
            assert_eq!(facts.next_tickets(PROJECT), [card(1), card(2)]);
        });
    }

    /// A lease that no hire ever took belongs to no agent: its holder works in the shared folder.
    #[test]
    fn a_never_assigned_lease_is_the_shared_folder() {
        let a = agent(1);
        let mut world = copies(&[(a, Some(11))]);
        let lease = WorktreeId::from_bytes(11_u128.to_be_bytes());
        with_facts(&world, &[], &[a], |facts| {
            assert_eq!(facts.tree_of(a, PROJECT), Some(lease));
        });
        world.worktrees.get_mut(&lease).unwrap().removed = Some(WorktreeRemoval::NeverAssigned);
        with_facts(&world, &[], &[a], |facts| {
            assert_eq!(facts.tree_of(a, PROJECT), None);
        });
    }

    /// A Working ticket in Needs you whose goal is still its agent's holds its own copy and no
    /// other tree.
    #[test]
    fn a_blocked_ticket_holding_its_goal_holds_only_its_own_tree() {
        let [a, b, c] = [agent(1), agent(2), agent(3)];
        let goal = GoalId::from_parts(8, 1);
        let mut world = copies(&[(a, Some(11)), (b, Some(12)), (c, None)]);
        world.apply(Lsn(40), &entry(goal_set(a, goal)));
        exhaust(&mut world, 41, a);
        let mut stalled = ticket(1, CardStatus::Blocked, Some(a), None);
        stalled.1.goal = Some(goal);
        let tickets = [
            stalled,
            ticket(2, CardStatus::Claimed, Some(b), Some(10)),
            ticket(3, CardStatus::Claimed, Some(c), Some(11)),
        ];
        with_facts(&world, &tickets, &[a, b, c], |facts| {
            let own = WorktreeId::from_bytes(11_u128.to_be_bytes());
            assert_eq!(
                facts.tree_wait(PROJECT, Some(own)),
                Some(TicketWait::Ticket { card: card(1) })
            );
            assert_eq!(facts.tree_wait(PROJECT, None), None);
            assert_eq!(facts.next_tickets(PROJECT), [card(2), card(3)]);
        });
    }
}
