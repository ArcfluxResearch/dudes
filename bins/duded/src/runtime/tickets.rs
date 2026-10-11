//! The ticket methods. Each runs on the writer thread, decides against the board projection and
//! appends one record. A refusal appends nothing.

use super::{Answered, Clock, Daemon, PredicateRunner, ProcessHost, Rng, TicketAsk, error};
use dudes_proto::rpc::ticket::{
    TicketAcceptParams, TicketAssignOutcome, TicketAssignParams, TicketAssignResult,
    TicketCreateParams, TicketCreateResult, TicketEditParams, TicketReadParams, TicketReadResult,
    TicketRemoveParams, TicketStopParams,
};
use dudes_proto::rpc::{ErrorCode, RpcError};
use dudes_proto::ticket::{
    MAX_MESSAGE_BYTES, MAX_NOTE_BYTES, MAX_OPEN_TICKETS, MAX_QUEUE, MAX_TITLE_BYTES, clean_text,
    clean_title, trim,
};
use dudes_proto::{Actor, CardView, Record, TicketCheck, TicketRefusal, TicketRow, TicketWait};
use dudes_types::{AgentId, CardId, CardStatus, Seq};

use crate::floor_actions::Profile;

/// The refusal for `kind`. `state` is the ticket's state when the sentence names it.
pub(super) fn refusal(kind: TicketRefusal, state: CardStatus) -> RpcError {
    RpcError {
        code: ErrorCode::Refused.code(),
        message: kind.sentence(state),
        data: Some(serde_json::json!({"reason": kind.code()})),
    }
}

/// The refusal for a request made against a sequence the card has moved past.
fn stale(card: &CardView) -> RpcError {
    RpcError {
        code: ErrorCode::Conflict.code(),
        message: TicketRefusal::StaleCard.sentence(card.status),
        data: Some(serde_json::json!({"current_seq": card.seq})),
    }
}

/// The board projection could not be read.
fn unreadable(failure: &dudes_journal::Error) -> RpcError {
    error(
        ErrorCode::InternalError,
        format!("could not read the board: {failure}"),
    )
}

fn invalid(message: &str) -> RpcError {
    error(ErrorCode::InvalidParams, message)
}

fn operator(by: Actor) -> Result<(), RpcError> {
    if matches!(by, Actor::Peer { .. } | Actor::BrowserSession { .. }) {
        Ok(())
    } else {
        Err(error(
            ErrorCode::Unauthorized,
            "ticket methods require an operator",
        ))
    }
}

fn title_of(text: &str) -> Result<String, RpcError> {
    let title = trim(text);
    if title.is_empty() || !clean_title(title, MAX_TITLE_BYTES) {
        return Err(invalid(
            "the title must be 1 to 256 bytes on one line without control characters",
        ));
    }
    Ok(title.to_owned())
}

/// The note is kept exactly as written.
fn note_of(text: &str) -> Result<String, RpcError> {
    if !clean_text(text, MAX_NOTE_BYTES) {
        return Err(invalid(
            "the note must be at most 8192 bytes without control characters",
        ));
    }
    Ok(text.to_owned())
}

/// The project's check named by `id`, or its own check; refused when `revision` says the caller
/// looked at an older one.
fn check_of(
    profile: &Profile,
    id: Option<&str>,
    revision: Option<dudes_proto::record::Hex32>,
) -> Result<TicketCheck, RpcError> {
    let check = match id {
        Some(id) => profile
            .check(id)
            .ok_or_else(|| invalid("the project has no check with that id"))?,
        None => profile.first_check(),
    };
    let reference = check
        .reference()
        .ok_or_else(|| invalid("the project's check has no usable name"))?;
    if revision.is_some_and(|seen| seen != reference.revision) {
        return Err(refusal(TicketRefusal::DoneWhenChanged, CardStatus::Todo));
    }
    Ok(TicketCheck {
        id: check.id.clone(),
        label: check.label.clone(),
        predicate: check.predicate.clone(),
    })
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    pub(super) fn ticket(&mut self, ask: TicketAsk) -> Result<Answered, RpcError> {
        match ask {
            TicketAsk::Create { request, by } => self.ticket_create(&request, by),
            TicketAsk::Edit { request, by } => self.ticket_edit(&request, by),
            TicketAsk::Assign { request, by } => self.ticket_assign(&request, by),
            TicketAsk::Remove { request, by } => self.ticket_remove(&request, by),
            TicketAsk::Accept { request, by } => self.ticket_accept(&request, by),
            TicketAsk::Stop { request, by } => self.ticket_stop(&request, by),
            TicketAsk::Read(request) => self.ticket_read(request),
        }
    }

    /// The card and what the board keeps of it, or the reason the card is not a ticket.
    pub(super) fn ticket_row(&self, card: CardId) -> Result<(CardView, TicketRow), RpcError> {
        let ticket = self
            .ctx
            .projections
            .ticket(card)
            .map_err(|failure| unreadable(&failure))?;
        if let Some(found) = ticket {
            return Ok(found);
        }
        match self.board_card(card)? {
            Some(view) => Err(refusal(TicketRefusal::NotATicket, view.status)),
            None => Err(error(ErrorCode::NotFound, "no such card")),
        }
    }

    fn ticket_create(
        &mut self,
        request: &TicketCreateParams,
        by: Actor,
    ) -> Result<Answered, RpcError> {
        operator(by)?;
        let title = title_of(&request.title)?;
        let description = note_of(request.description.as_deref().unwrap_or_default())?;
        let profile = match request.project.as_deref() {
            Some(id) => self.ctx.enrollment.profile(id),
            None => match self.ctx.enrollment.profiles.as_slice() {
                [only] => Some(only),
                [] => None,
                _ => return Err(invalid("several projects are enrolled; name one")),
            },
        }
        .ok_or_else(|| refusal(TicketRefusal::ProjectUnavailable, CardStatus::Todo))?;
        let done_when = check_of(
            profile,
            request.done_when.as_deref(),
            request.done_when_revision,
        )?;
        let (project, department) = (profile.id.clone(), profile.department.clone());
        let open = self
            .ctx
            .projections
            .tickets()
            .map_err(|failure| unreadable(&failure))?
            .iter()
            .filter(|(view, _)| view.status != CardStatus::Done)
            .count();
        if open >= MAX_OPEN_TICKETS {
            return Err(refusal(TicketRefusal::LimitReached, CardStatus::Todo));
        }
        let card = self.sup.mint_card_id();
        self.admit_new_card(CardView {
            id: card,
            title: title.clone(),
            department: department.clone(),
            status: CardStatus::Todo,
            status_by: by,
            seq: Seq::ZERO,
            claimed_by: None,
            deps: Vec::new(),
            ticket: Some(crate::board::largest_ticket()),
        })?;
        self.board_append(Record::TicketCreated {
            card,
            title,
            description,
            department,
            project,
            done_when,
            by,
        })?;
        Ok(Answered::TicketCreated(TicketCreateResult { card }))
    }

    fn ticket_edit(&mut self, request: &TicketEditParams, by: Actor) -> Result<Answered, RpcError> {
        operator(by)?;
        let (view, ticket) = self.ticket_row(request.card)?;
        if view.seq != request.expected_seq {
            return Err(stale(&view));
        }
        if !matches!(view.status, CardStatus::Todo | CardStatus::Claimed) {
            return Err(refusal(TicketRefusal::WrongState, view.status));
        }
        if request.title.is_none() && request.description.is_none() && request.done_when.is_none() {
            return Err(invalid("there is nothing to change"));
        }
        if request.done_when.is_none() && request.done_when_revision.is_some() {
            return Err(invalid("a check revision needs the check it belongs to"));
        }
        let title = match &request.title {
            Some(text) => title_of(text)?,
            None => view.title.clone(),
        };
        let description = match &request.description {
            Some(text) => note_of(text)?,
            None => ticket.description.clone(),
        };
        let done_when = if request.done_when.is_some() {
            let profile = self
                .ctx
                .enrollment
                .profile(&ticket.project)
                .ok_or_else(|| refusal(TicketRefusal::ProjectUnavailable, view.status))?;
            check_of(
                profile,
                request.done_when.as_deref(),
                request.done_when_revision,
            )?
        } else {
            ticket.done_when.clone()
        };
        self.board_append(Record::TicketEdited {
            card: view.id,
            title,
            description,
            done_when,
            by,
            expected_seq: request.expected_seq,
        })?;
        Ok(Answered::Done)
    }

    fn ticket_assign(
        &mut self,
        request: &TicketAssignParams,
        by: Actor,
    ) -> Result<Answered, RpcError> {
        operator(by)?;
        let (view, ticket) = self.ticket_row(request.card)?;
        if view.seq != request.expected_seq {
            return Err(stale(&view));
        }
        if !matches!(
            view.status,
            CardStatus::Todo | CardStatus::Claimed | CardStatus::Blocked | CardStatus::Review
        ) {
            return Err(refusal(TicketRefusal::WrongState, view.status));
        }
        let message = match request.message.as_deref() {
            Some(text) if !clean_text(trim(text), MAX_MESSAGE_BYTES) => {
                return Err(invalid(
                    "the message must be at most 2048 bytes without control characters",
                ));
            }
            Some(text) if trim(text).is_empty() => None,
            Some(text) => Some(trim(text).to_owned()),
            None => None,
        };
        self.require_ticket_agent(request.agent, &ticket)?;
        let waiting = self.agent_queue(request.agent)?;
        if waiting.iter().filter(|(card, _)| *card != view.id).count() >= MAX_QUEUE {
            return Err(refusal(TicketRefusal::QueueFull, view.status));
        }
        self.settle_goal(&view, &ticket)?;
        self.board_append(Record::TicketAssigned {
            card: view.id,
            agent: request.agent,
            by,
            message,
            expected_seq: request.expected_seq,
        })?;
        if let Err(failure) = self.ticket_reactions() {
            tracing::error!(
                failure = %failure.message,
                "the assignment is durable but the queue could not be advanced"
            );
        }
        let (now, ticket) = self.ticket_row(view.id)?;
        let outcome = match now.status {
            CardStatus::Doing => TicketAssignOutcome::Started,
            CardStatus::Blocked => {
                let reason = ticket.reason.ok_or_else(|| {
                    error(ErrorCode::InternalError, "a blocked ticket has no reason")
                })?;
                TicketAssignOutcome::NeedsYou {
                    reason,
                    note: reason.sentence().to_owned(),
                }
            }
            _ => {
                let (position, wait, tickets) = self.with_ticket_facts(|facts| {
                    (
                        facts.position(request.agent, view.id),
                        facts.wait_of(view.id),
                        facts.tickets.to_vec(),
                    )
                })?;
                TicketAssignOutcome::Queued {
                    position: position.unwrap_or(1),
                    waiting: self.waiting(
                        wait.unwrap_or(TicketWait::AgentBusy),
                        Some(request.agent),
                        &tickets,
                    ),
                }
            }
        };
        Ok(Answered::TicketAssigned(TicketAssignResult { outcome }))
    }

    /// The operator's verdict that a ticket that is Ready for you is done. It frees the project,
    /// and the reactions that follow start the next queued ticket.
    fn ticket_accept(
        &mut self,
        request: &TicketAcceptParams,
        by: Actor,
    ) -> Result<Answered, RpcError> {
        operator(by)?;
        let (view, _) = self.ticket_row(request.card)?;
        if view.seq != request.expected_seq {
            return Err(stale(&view));
        }
        if view.status != CardStatus::Review {
            return Err(refusal(TicketRefusal::WrongState, view.status));
        }
        self.board_append(Record::CardTransition {
            card: view.id,
            from: CardStatus::Review,
            to: CardStatus::Done,
            by,
            expected_seq: request.expected_seq,
            reason: None,
        })?;
        Ok(Answered::Done)
    }

    /// The agent must be live and hired into the ticket's project.
    fn require_ticket_agent(&self, agent: AgentId, ticket: &TicketRow) -> Result<(), RpcError> {
        let unavailable = || refusal(TicketRefusal::AgentUnavailable, CardStatus::Todo);
        if self
            .sup
            .world()
            .agents
            .get(&agent)
            .is_none_or(|row| row.retired)
        {
            return Err(unavailable());
        }
        let row = self
            .ctx
            .projections
            .agent(agent)
            .map_err(|failure| {
                error(
                    ErrorCode::InternalError,
                    format!("could not read the agent: {failure}"),
                )
            })?
            .ok_or_else(unavailable)?;
        match row.workspace {
            None => Err(refusal(TicketRefusal::AgentNoProject, CardStatus::Todo)),
            Some(workspace) if workspace.id == ticket.project => Ok(()),
            Some(_) => Err(unavailable()),
        }
    }

    /// An agent's queue in serving order: the cards it holds in To do, rework first and then by
    /// the position of their assignment.
    fn agent_queue(
        &self,
        agent: AgentId,
    ) -> Result<Vec<(CardId, dudes_proto::QueueKey)>, RpcError> {
        let mut queue: Vec<_> = self
            .ctx
            .projections
            .tickets()
            .map_err(|failure| unreadable(&failure))?
            .into_iter()
            .filter(|(view, _)| {
                view.status == CardStatus::Claimed && view.claimed_by == Some(agent)
            })
            .filter_map(|(view, ticket)| ticket.queue.map(|key| (view.id, key)))
            .collect();
        queue.sort_by_key(|(_, key)| *key);
        Ok(queue)
    }

    fn ticket_remove(
        &mut self,
        request: &TicketRemoveParams,
        by: Actor,
    ) -> Result<Answered, RpcError> {
        operator(by)?;
        let (view, ticket) = self.ticket_row(request.card)?;
        if view.seq != request.expected_seq {
            return Err(stale(&view));
        }
        if view.status == CardStatus::Doing {
            return Err(refusal(TicketRefusal::WrongState, view.status));
        }
        self.settle_goal(&view, &ticket)?;
        self.board_append(Record::TicketRemoved {
            card: view.id,
            by,
            expected_seq: request.expected_seq,
        })?;
        Ok(Answered::Done)
    }

    /// Take a Working ticket, or one in Needs you, back to To do, unassigned. The goal is cleared
    /// first when it is still unfinished, and the running turn is left to end by itself: it
    /// is not killed, and the project stays busy until it does. Both records are appended before
    /// anything else reads the board, so no move the board owes can fall between them.
    fn ticket_stop(&mut self, request: &TicketStopParams, by: Actor) -> Result<Answered, RpcError> {
        operator(by)?;
        let (view, ticket) = self.ticket_row(request.card)?;
        if view.seq != request.expected_seq {
            return Err(stale(&view));
        }
        if !matches!(view.status, CardStatus::Doing | CardStatus::Blocked) {
            return Err(refusal(TicketRefusal::WrongState, view.status));
        }
        self.settle_goal(&view, &ticket)?;
        self.board_append(Record::CardTransition {
            card: view.id,
            from: view.status,
            to: CardStatus::Todo,
            by,
            expected_seq: request.expected_seq,
            reason: None,
        })?;
        Ok(Answered::Done)
    }

    fn ticket_read(&self, request: TicketReadParams) -> Result<Answered, RpcError> {
        let (card, mut ticket) = self.ticket_row(request.card)?;
        ticket.summary = ticket
            .summary
            .as_deref()
            .map(dudes_proto::ticket::summary_text);
        let position = match (card.status, card.claimed_by) {
            (CardStatus::Claimed, Some(agent)) => self
                .agent_queue(agent)?
                .iter()
                .position(|(queued, _)| *queued == card.id)
                .map(|index| u32::try_from(index + 1).unwrap_or(u32::MAX)),
            _ => None,
        };
        let (wait, tickets) =
            self.with_ticket_facts(|facts| (facts.wait_of(card.id), facts.tickets.to_vec()))?;
        let waiting = wait.map(|wait| self.waiting(wait, card.claimed_by, &tickets));
        Ok(Answered::TicketRead(Box::new(TicketReadResult {
            card,
            ticket,
            position,
            waiting,
        })))
    }
}
