//! A Floor session's ticket action, on the writer thread. It resolves the session to the author of
//! every record the action appends, runs the same method the socket runs, and turns the method's
//! answer into one of the fixed replies. The browser names no author, program, argv or ceiling.

use dudes_proto::rpc::ticket::TicketAssignOutcome;
use dudes_proto::rpc::{ErrorCode, RpcError};
use dudes_proto::{Actor, TicketWait};
use dudes_types::CardId;
use serde_json::{Value, json};

use super::{Answered, Ask, Clock, Daemon, PredicateRunner, ProcessHost, Rng, TicketAsk};
use crate::floor_ticket::{TicketReply, TicketRequest, done_body, read_body, refusal_code};

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    /// One admitted ticket request. The author is the session, derived here and never read from
    /// the request.
    pub(super) fn browser_ticket(
        &mut self,
        session: &crate::floor_session::SessionIdentity,
        request: TicketRequest,
    ) -> TicketReply {
        if !self.ctx.board_enabled || !session.is_valid() {
            return TicketReply::Forbidden;
        }
        if self.ctx.journal_writes_stopped() && !matches!(request, TicketRequest::Read(_)) {
            return TicketReply::Unavailable;
        }
        let by = Actor::BrowserSession {
            session_digest: session.digest(),
        };
        let (ask, state, card) = match request {
            TicketRequest::Create(request) => (TicketAsk::Create { request, by }, "created", None),
            TicketRequest::Edit(request) => {
                let card = request.card;
                (TicketAsk::Edit { request, by }, "edited", Some(card))
            }
            TicketRequest::Assign(request) => {
                let card = request.card;
                (TicketAsk::Assign { request, by }, "assigned", Some(card))
            }
            TicketRequest::Accept(request) => {
                let card = request.card;
                (TicketAsk::Accept { request, by }, "accepted", Some(card))
            }
            TicketRequest::Stop(request) => {
                let card = request.card;
                (TicketAsk::Stop { request, by }, "stopped", Some(card))
            }
            TicketRequest::Remove(request) => {
                let card = request.card;
                (TicketAsk::Remove { request, by }, "removed", Some(card))
            }
            TicketRequest::Read(request) => (TicketAsk::Read(request), "ok", None),
        };
        self.ctx.browser_session = Some(session.clone());
        let answer = self.ask(Ask::Ticket(ask));
        self.ctx.browser_session = None;
        match answer {
            Ok(answered) => self.ticket_done(state, card, answered),
            Err(error) => Self::ticket_error(session, &error),
        }
    }

    fn ticket_done(
        &self,
        state: &'static str,
        asked: Option<CardId>,
        answered: Answered,
    ) -> TicketReply {
        let lsn = self.ctx.projections.last_applied_lsn();
        let seq_of = |card: CardId| {
            self.ctx
                .projections
                .card(card)
                .ok()
                .flatten()
                .map(|view| view.seq)
        };
        match answered {
            Answered::TicketCreated(created) => TicketReply::Done(done_body(
                state,
                created.card,
                seq_of(created.card),
                lsn,
                None,
            )),
            Answered::TicketAssigned(assigned) => {
                let Some(card) = asked else {
                    return TicketReply::Unavailable;
                };
                TicketReply::Done(done_body(
                    state,
                    card,
                    seq_of(card),
                    lsn,
                    Some(start_of(&assigned.outcome)),
                ))
            }
            Answered::TicketRead(read) => TicketReply::Done(read_body(&read)),
            Answered::Done => match asked {
                Some(card) => TicketReply::Done(done_body(state, card, seq_of(card), lsn, None)),
                None => TicketReply::Unavailable,
            },
            _ => TicketReply::Unavailable,
        }
    }

    /// A refusal is one of the fixed codes, a malformed request is `400`, and a failed append is
    /// `403` when the session lost its permit and `503` otherwise.
    fn ticket_error(
        session: &crate::floor_session::SessionIdentity,
        error: &RpcError,
    ) -> TicketReply {
        if error.code == ErrorCode::Conflict.code() {
            return TicketReply::Refused("stale_card");
        }
        if error.code == ErrorCode::Refused.code() {
            return error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(Value::as_str)
                .and_then(refusal_code)
                .map_or(TicketReply::Unavailable, TicketReply::Refused);
        }
        if error.code == ErrorCode::NotFound.code() {
            return TicketReply::Refused("stale_card");
        }
        if error.code == ErrorCode::InvalidParams.code() {
            return TicketReply::Invalid;
        }
        tracing::warn!(code = error.code, message = %error.message, "a Floor ticket action failed");
        if session.is_valid() {
            TicketReply::Unavailable
        } else {
            TicketReply::Forbidden
        }
    }
}

/// What became of an assigned ticket, as codes. The page words them from its own table, so no
/// ticket title or agent name rides in a reply that is held to a few hundred bytes.
fn start_of(outcome: &TicketAssignOutcome) -> Value {
    match outcome {
        TicketAssignOutcome::Started => json!({"state": "started"}),
        TicketAssignOutcome::Queued { position, waiting } => {
            json!({"state": "queued", "position": position, "wait": wait_kind(waiting.wait)})
        }
        TicketAssignOutcome::NeedsYou { reason, .. } => {
            json!({"state": "needs_you", "reason": reason})
        }
    }
}

const fn wait_kind(wait: TicketWait) -> &'static str {
    match wait {
        TicketWait::NotGiven => "not_given",
        TicketWait::AgentBusy => "agent_busy",
        TicketWait::Ticket { .. } => "ticket",
        TicketWait::ProjectBusy => "project_busy",
        TicketWait::AgentPaused => "agent_paused",
        TicketWait::AgentAttached => "agent_attached",
        TicketWait::Closing => "closing",
        TicketWait::Stopping => "stopping",
        TicketWait::AgentLimit => "agent_limit",
        TicketWait::FloorLimit => "floor_limit",
    }
}
