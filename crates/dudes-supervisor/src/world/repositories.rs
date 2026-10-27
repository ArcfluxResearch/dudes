//! Row-owned repository history and the common pure relationship fold.

use dudes_proto::repository::{HireRepositories, TicketRepositories};
use dudes_proto::repository_state::history::{
    self, RepositoryHistoryChange, RepositoryHistoryFacts, RepositoryLease, RepositoryStateError,
    RepositoryTicketFacts,
};
use dudes_proto::{Entry, Record};
use dudes_types::{AgentId, CardStatus, GoalId, Lsn, Seq};

use super::World;

impl World {
    #[expect(
        clippy::too_many_lines,
        reason = "one adapter borrows preceding owning rows for the shared relationship validator"
    )]
    pub(super) fn prepare_repository_record(
        &self,
        lsn: Lsn,
        entry: &Entry,
    ) -> Result<RepositoryHistoryChange, RepositoryStateError> {
        let ticket_id = entry
            .record
            .ticket_created()
            .map(|created| created.card)
            .or_else(|| entry.record.ticket_edited().map(|edited| edited.card))
            .or_else(|| entry.record.ticket_start().map(|start| start.card));
        let ticket = ticket_id
            .and_then(|card| self.repository_tickets.get(&card))
            .and_then(Ticket::created);
        let agent = entry
            .record
            .hire()
            .map(|hire| hire.agent)
            .or_else(|| entry.record.turn_issue().map(|issue| issue.agent))
            .or_else(|| entry.record.ticket_start().map(|start| start.agent));
        let owner = agent
            .and_then(|agent| self.agents.get(&agent))
            .filter(|row| !row.retired);
        let project = match &entry.record {
            Record::ProjectRepositoriesRecorded { project, .. }
            | Record::RepositoryTrustSet { project, .. } => Some(project.as_str()),
            Record::AgentHiredWithRepositories(hire) => Some(hire.repositories.project.id.as_str()),
            _ => entry
                .record
                .ticket_created()
                .map(|created| created.project)
                .or_else(|| ticket.map(|ticket| ticket.repositories.project()))
                .or_else(|| {
                    owner
                        .and_then(|owner| owner.repositories.workspace())
                        .map(|project| project.id.as_str())
                }),
        };
        let needs_hire = matches!(entry.record, Record::AgentHiredWithRepositories(_));
        let catalog_change = matches!(entry.record, Record::ProjectRepositoriesRecorded { .. });
        let trust: Vec<_> = if needs_hire || entry.record.ticket_start().is_some() {
            self.repository_trust
                .values()
                .filter(|row| Some(row.project.as_str()) == project)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let leases: Vec<_> = if needs_hire || entry.record.ticket_start().is_some() {
            self.worktrees
                .iter()
                .map(|(id, row)| RepositoryLease {
                    worktree: *id,
                    leased_lsn: row.leased_lsn,
                    origin: &row.origin,
                    path: &row.path,
                    base: row.base,
                    agent: row.agent,
                    removed: row.removed.is_some(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let active_hires: Vec<_> = if catalog_change {
            self.agents
                .values()
                .filter(|row| !row.retired)
                .filter_map(|row| match &row.repositories {
                    HireRepositories::Scoped(hire) => Some(hire),
                    HireRepositories::Legacy(_) => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        let active_tickets: Vec<_> = if catalog_change {
            self.repository_tickets
                .values()
                .filter_map(Ticket::created)
                .filter(|ticket| ticket.status != CardStatus::Done)
                .filter_map(|ticket| {
                    ticket
                        .repositories
                        .declared()
                        .map(|scope| &scope.declaration)
                })
                .collect()
        } else {
            Vec::new()
        };
        history::prepare(
            lsn,
            &entry.record,
            RepositoryHistoryFacts {
                project: project.and_then(|id| self.repository_projects.get(id)),
                trust: &trust,
                leases: &leases,
                active_hires: &active_hires,
                hire: owner.map(|row| &row.repositories),
                hired_lsn: owner.map(|row| row.mcp.hire_lsn),
                ticket: ticket.map(CreatedTicket::facts),
                active_tickets: &active_tickets,
                card_exists: ticket_id
                    .is_some_and(|card| self.recovery_card_changes.contains_key(&card)),
                goal_exists: entry
                    .record
                    .ticket_start()
                    .is_some_and(|start| self.goals.contains_key(&start.goal.id)),
                hire_class: agent.and_then(|id| self.repository_hires.get(&id)).copied(),
            },
        )
    }

    pub(super) fn apply_repository_change(&mut self, change: RepositoryHistoryChange) {
        match change {
            RepositoryHistoryChange::Hired { agent, class } => {
                self.repository_hires.insert(agent, class);
            }
            RepositoryHistoryChange::Project(row) => {
                self.repository_projects.insert(row.project.clone(), row);
            }
            RepositoryHistoryChange::Trust(row) => {
                self.repository_trust
                    .insert((row.project.clone(), row.repo_id.clone()), row);
            }
            RepositoryHistoryChange::Unchanged => {}
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct CreatedTicket {
    pub repositories: TicketRepositories,
    check: dudes_proto::TicketCheck,
    status: CardStatus,
    holder: Option<AgentId>,
    seq: Seq,
    pub round: u32,
    goal: Option<GoalId>,
    queued: bool,
    recovery_wait: bool,
    removed: bool,
}

impl CreatedTicket {
    fn facts(&self) -> RepositoryTicketFacts<'_> {
        RepositoryTicketFacts {
            repositories: &self.repositories,
            check: &self.check,
            status: self.status,
            holder: self.holder,
            seq: self.seq,
            round: self.round,
            queued: self.queued,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum Ticket {
    Created(Box<CreatedTicket>),
    MissingCreation { round: u32 },
}

impl Ticket {
    pub(super) fn created(&self) -> Option<&CreatedTicket> {
        match self {
            Self::Created(ticket) if !ticket.removed => Some(ticket),
            Self::Created(_) | Self::MissingCreation { .. } => None,
        }
    }

    fn created_mut(&mut self) -> Option<&mut CreatedTicket> {
        match self {
            Self::Created(ticket) if !ticket.removed => Some(ticket),
            Self::Created(_) | Self::MissingCreation { .. } => None,
        }
    }

    pub(super) fn round(&self) -> u32 {
        match self {
            Self::Created(ticket) => ticket.round,
            Self::MissingCreation { round } => *round,
        }
    }
}

impl World {
    #[expect(
        clippy::too_many_lines,
        reason = "one ticket row owns declaration, queue and review lifecycle facts"
    )]
    pub(super) fn fold_repository_ticket(
        &mut self,
        lsn: Lsn,
        record: &Record,
        folded: &dudes_proto::instruction_state::InstructionFold,
    ) {
        if let Some(created) = record.ticket_created() {
            let dudes_proto::repository_state::RepositoryFold::Ticket(repositories) =
                dudes_proto::repository_state::fold(
                    lsn,
                    record,
                    dudes_proto::repository_state::RepositoryFacts::default(),
                )
            else {
                return;
            };
            self.repository_tickets.insert(
                created.card,
                Ticket::Created(Box::new(CreatedTicket {
                    repositories,
                    check: created.done_when.clone(),
                    status: CardStatus::Todo,
                    holder: None,
                    seq: Seq::ZERO,
                    round: 0,
                    goal: None,
                    queued: false,
                    recovery_wait: false,
                    removed: false,
                })),
            );
        } else if let Some(edited) = record.ticket_edited() {
            if let Some(ticket) = self
                .repository_tickets
                .get_mut(&edited.card)
                .and_then(Ticket::created_mut)
            {
                ticket.check = edited.done_when.clone();
                if let Some(repo) = edited.check_repo {
                    ticket.repositories.set_check_repo(repo);
                }
                ticket.seq = edited.expected_seq.next();
            }
        } else if let Some(start) = record.ticket_start() {
            let row = self
                .repository_tickets
                .entry(start.card)
                .or_insert(Ticket::MissingCreation { round: 0 });
            match row {
                Ticket::Created(ticket) => {
                    ticket.status = CardStatus::Doing;
                    ticket.holder = Some(start.agent);
                    ticket.seq = start.expected_seq.next();
                    ticket.round = ticket.round.saturating_add(1);
                    ticket.goal = Some(start.goal.id);
                    ticket.queued = false;
                    ticket.recovery_wait = false;
                }
                Ticket::MissingCreation { round } => *round = round.saturating_add(1),
            }
        } else {
            match record {
                Record::TicketAssigned {
                    card,
                    agent,
                    expected_seq,
                    ..
                } => {
                    if let Some(ticket) = self
                        .repository_tickets
                        .get_mut(card)
                        .and_then(Ticket::created_mut)
                    {
                        ticket.status = CardStatus::Claimed;
                        ticket.holder = Some(*agent);
                        ticket.goal = None;
                        ticket.queued = true;
                        ticket.recovery_wait = false;
                        ticket.seq = expected_seq.next();
                    }
                }
                Record::CardCreated { card, .. } => {
                    self.repository_tickets.remove(card);
                }
                Record::Claim {
                    card,
                    agent,
                    expected_seq,
                } => {
                    if let Some(ticket) = self
                        .repository_tickets
                        .get_mut(card)
                        .and_then(Ticket::created_mut)
                    {
                        ticket.status = CardStatus::Claimed;
                        ticket.holder = Some(*agent);
                        ticket.seq = expected_seq.next();
                    }
                }
                Record::TicketRemoved { card, .. } => {
                    if let Some(ticket) = self
                        .repository_tickets
                        .get_mut(card)
                        .and_then(Ticket::created_mut)
                    {
                        ticket.removed = true;
                    }
                }
                Record::CardTransition {
                    card,
                    to,
                    expected_seq,
                    reason,
                    ..
                } => {
                    if let Some(ticket) = self
                        .repository_tickets
                        .get_mut(card)
                        .and_then(Ticket::created_mut)
                    {
                        ticket.status = *to;
                        if *to != CardStatus::Claimed {
                            ticket.queued = false;
                        }
                        ticket.recovery_wait =
                            *reason == Some(dudes_proto::TicketReason::RecoveryRequired);
                        ticket.seq = expected_seq.next();
                        if *to == CardStatus::Todo {
                            ticket.holder = None;
                        }
                    }
                }
                Record::TicketInstructionReviewed {
                    card,
                    expected_seq,
                    action,
                    ..
                } => {
                    if let Some(ticket) = self
                        .repository_tickets
                        .get_mut(card)
                        .and_then(Ticket::created_mut)
                    {
                        ticket.seq = expected_seq.next();
                        match action {
                            dudes_proto::InstructionReviewAction::ChangesRequested {
                                agent,
                                ..
                            } => {
                                ticket.status = CardStatus::Claimed;
                                ticket.holder = Some(*agent);
                                ticket.goal = None;
                                ticket.queued = true;
                                ticket.recovery_wait = false;
                            }
                            dudes_proto::InstructionReviewAction::Accept { completion } => {
                                ticket.status = match completion {
                                    dudes_proto::InstructionAcceptCompletion::Completed {
                                        ..
                                    } => CardStatus::Done,
                                    dudes_proto::InstructionAcceptCompletion::Continue => {
                                        if ticket.recovery_wait {
                                            CardStatus::Blocked
                                        } else if ticket.queued {
                                            CardStatus::Claimed
                                        } else if ticket.goal.is_some() && ticket.holder.is_some() {
                                            CardStatus::Doing
                                        } else {
                                            CardStatus::Todo
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Record::AgentRetired { id, .. } => {
                    for ticket in self
                        .repository_tickets
                        .values_mut()
                        .filter_map(Ticket::created_mut)
                        .filter(|ticket| {
                            ticket.holder == Some(*id) && ticket.status != CardStatus::Review
                        })
                    {
                        ticket.holder = None;
                        ticket.queued = false;
                        ticket.seq = ticket.seq.next();
                        if ticket.status != CardStatus::Done {
                            ticket.status = CardStatus::Todo;
                        }
                    }
                }
                _ => {}
            }
        }
        for card in folded.ticket_cards(&self.instructions, record) {
            if let Some(ticket) = self
                .repository_tickets
                .get_mut(&card)
                .and_then(Ticket::created_mut)
            {
                if folded.evidence_cards.contains(&card) {
                    ticket.seq = ticket.seq.next();
                }
                if self.instructions.ticket_reason(card).is_some() {
                    ticket.status = CardStatus::Blocked;
                }
            }
        }
    }
}
