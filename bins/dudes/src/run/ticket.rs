//! `dudes ticket`: each command is one or two operator calls, answered at once.

use std::io::Write;

use dudes_proto::rpc::board::CardListParams;
use dudes_proto::rpc::ticket::{
    TicketAcceptParams, TicketAssignParams, TicketAssignResult, TicketCreateParams,
    TicketCreateResult, TicketEditParams, TicketReadParams, TicketReadResult, TicketRemoveParams,
    TicketStopParams,
};
use dudes_proto::rpc::{Empty, ErrorCode, Method, RpcError};
use dudes_proto::{AgentView, TicketRefusal};
use dudes_types::{AgentId, CardId, CardStatus, Seq};

use super::{
    Failure, Socket, answer_result, call_presenting, connect, encode, local, operator_credential,
    typed,
};
use crate::args::{AgentRef, TicketCommand, TicketStatus};
use crate::env::Environment;
use crate::render::ticket::{AgentNames, agent_name};
use crate::{exit, render};

impl TicketStatus {
    fn matches(self, status: CardStatus) -> bool {
        match self {
            Self::Todo => matches!(status, CardStatus::Todo | CardStatus::Claimed),
            Self::Working => status == CardStatus::Doing,
            Self::Ready => status == CardStatus::Review,
            Self::Done => status == CardStatus::Done,
            Self::NeedsYou => status == CardStatus::Blocked,
        }
    }
}

struct Session<'a> {
    client: Socket,
    env: &'a Environment,
}

impl Session<'_> {
    fn call(
        &mut self,
        method: Method,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, Failure> {
        let cred = operator_credential(self.env)?;
        answer_result(call_presenting(&mut self.client, method, params, cred)?)
    }

    fn create(
        &mut self,
        params: &TicketCreateParams,
        machine: bool,
    ) -> Result<TicketCreateResult, Failure> {
        let result = self.call(Method::TicketCreate, Some(encode(params)?));
        typed(result.map_err(|failure| {
            if !machine && let Failure::Refused(mut error) = failure {
                if error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("reason"))
                    .and_then(serde_json::Value::as_str)
                    == Some("project_unavailable")
                    && super::no_projects(self.env)
                {
                    crate::setup::NO_PROJECT_HINT.clone_into(&mut error.message);
                }
                Failure::Refused(error)
            } else {
                failure
            }
        })?)
    }

    fn read(&mut self, card: CardId) -> Result<TicketReadResult, Failure> {
        let params = encode(&TicketReadParams { card })?;
        typed(self.call(Method::TicketRead, Some(params))?)
    }

    /// The sequence the caller named, or the ticket's own as of now.
    fn seq(&mut self, card: CardId, named: Option<Seq>) -> Result<Seq, Failure> {
        match named {
            Some(seq) => Ok(seq),
            None => Ok(self.read(card)?.card.seq),
        }
    }

    fn agents(&mut self) -> Result<Vec<AgentView>, Failure> {
        typed(self.call(Method::AgentList, None)?)
    }

    fn names(&mut self) -> AgentNames {
        self.agents()
            .unwrap_or_default()
            .into_iter()
            .map(|agent| (agent.id, agent.name))
            .collect()
    }

    /// The agent an id or a name stands for. A name must match exactly one live agent. An id is
    /// passed on as typed, and the daemon decides whether it can take the ticket.
    fn resolve(&mut self, agent: &AgentRef) -> Result<AgentId, Failure> {
        let name = match agent {
            AgentRef::Id(id) => return Ok(*id),
            AgentRef::Name(name) => name,
        };
        let matching: Vec<AgentId> = self
            .agents()?
            .into_iter()
            .filter(|live| live.name == *name)
            .map(|live| live.id)
            .collect();
        match matching[..] {
            [only] => Ok(only),
            [] => Err(local(format!("No live agent is named \"{name}\"."))),
            _ => {
                let ids: Vec<String> = matching.iter().map(ToString::to_string).collect();
                Err(local(format!(
                    "More than one live agent is named \"{name}\": {}. Use an agent id.",
                    ids.join(", ")
                )))
            }
        }
    }

    fn give(
        &mut self,
        card: CardId,
        agent: &AgentRef,
        message: Option<String>,
        named: Option<Seq>,
    ) -> Result<(TicketAssignResult, String), Failure> {
        let agent = self.resolve(agent)?;
        self.assign(card, agent, message, named)
    }

    fn assign(
        &mut self,
        card: CardId,
        agent: AgentId,
        message: Option<String>,
        named: Option<Seq>,
    ) -> Result<(TicketAssignResult, String), Failure> {
        let expected_seq = self.seq(card, named)?;
        let params = encode(&TicketAssignParams {
            card,
            agent,
            message,
            expected_seq,
        })?;
        let result = typed(self.call(Method::TicketAssign, Some(params))?)?;
        let name = agent_name(&self.names(), agent);
        Ok((result, name))
    }

    /// Give a ticket that is Ready for you or in Needs you back with what to change, to the agent
    /// named or else the one that did the work.
    fn send_back(
        &mut self,
        card: CardId,
        message: String,
        agent: Option<&AgentRef>,
        named: Option<Seq>,
    ) -> Result<(TicketAssignResult, String), Failure> {
        let read = self.read(card)?;
        if !matches!(read.card.status, CardStatus::Review | CardStatus::Blocked) {
            return Err(Failure::Refused(RpcError {
                code: ErrorCode::Refused.code(),
                message: TicketRefusal::WrongState.sentence(read.card.status),
                data: None,
            }));
        }
        let agent = match (agent, read.card.claimed_by) {
            (Some(named), _) => self.resolve(named)?,
            (None, Some(previous)) => previous,
            (None, None) => {
                return Err(local(
                    "No agent has worked on this ticket. Name one with --agent.",
                ));
            }
        };
        self.assign(
            card,
            agent,
            Some(message),
            Some(named.unwrap_or(read.card.seq)),
        )
    }

    /// Every ticket on the board. The board holds plain cards too, and a plain card is refused by
    /// `ticket.read` as not a ticket, which here only means it is not on the list.
    fn tickets(&mut self) -> Result<Vec<TicketReadResult>, Failure> {
        let params = encode(&CardListParams {
            department: None,
            status: None,
            claimed_by: None,
        })?;
        let cards: Vec<dudes_proto::CardView> = typed(self.call(Method::BoardList, Some(params))?)?;
        let mut tickets = Vec::new();
        for card in cards {
            match self.read(card.id) {
                Ok(read) => tickets.push(read),
                Err(Failure::Refused(error)) if is_not_a_ticket(&error) => {}
                Err(other) => return Err(other),
            }
        }
        Ok(tickets)
    }
}

fn is_not_a_ticket(error: &dudes_proto::rpc::RpcError) -> bool {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(serde_json::Value::as_str)
        == Some(dudes_proto::TicketRefusal::NotATicket.code())
}

pub(super) fn command(
    command: &TicketCommand,
    env: &Environment,
    machine: bool,
    out: &mut dyn Write,
) -> Result<u8, Failure> {
    let mut session = Session {
        client: connect(env)?,
        env,
    };
    match command {
        TicketCommand::Add { params, give_to } => {
            let created = session.create(params, machine)?;
            render::ticket::added(out, machine, &created).map_err(local)?;
            if let Some(agent) = give_to {
                let (result, name) = session.give(created.card, agent, None, None)?;
                render::ticket::given(out, machine, &name, result).map_err(local)?;
            }
        }
        TicketCommand::Edit {
            card,
            title,
            note,
            done_when,
            expected_seq,
        } => {
            let params = encode(&TicketEditParams {
                card: *card,
                title: title.clone(),
                description: note.clone(),
                done_when: done_when.clone(),
                done_when_revision: None,
                expected_seq: session.seq(*card, *expected_seq)?,
            })?;
            let result: Empty = typed(session.call(Method::TicketEdit, Some(params))?)?;
            render::ticket::changed(out, machine, "Ticket changed.", result).map_err(local)?;
        }
        TicketCommand::Give {
            card,
            agent,
            message,
            expected_seq,
        } => {
            let (result, name) = session.give(*card, agent, message.clone(), *expected_seq)?;
            render::ticket::given(out, machine, &name, result).map_err(local)?;
        }
        TicketCommand::Changes {
            card,
            message,
            agent,
            expected_seq,
        } => {
            let (result, name) =
                session.send_back(*card, message.clone(), agent.as_ref(), *expected_seq)?;
            render::ticket::given(out, machine, &name, result).map_err(local)?;
        }
        TicketCommand::Accept { card, expected_seq } => {
            let params = encode(&TicketAcceptParams {
                card: *card,
                expected_seq: session.seq(*card, *expected_seq)?,
            })?;
            let result: Empty = typed(session.call(Method::TicketAccept, Some(params))?)?;
            render::ticket::changed(out, machine, "Ticket accepted.", result).map_err(local)?;
        }
        TicketCommand::Stop { card, expected_seq } => {
            let params = encode(&TicketStopParams {
                card: *card,
                expected_seq: session.seq(*card, *expected_seq)?,
            })?;
            let result: Empty = typed(session.call(Method::TicketStop, Some(params))?)?;
            render::ticket::changed(out, machine, "Ticket stopped. It is back in To do.", result)
                .map_err(local)?;
        }
        TicketCommand::Remove { card, expected_seq } => {
            let params = encode(&TicketRemoveParams {
                card: *card,
                expected_seq: session.seq(*card, *expected_seq)?,
            })?;
            let result: Empty = typed(session.call(Method::TicketRemove, Some(params))?)?;
            render::ticket::changed(out, machine, "Ticket removed.", result).map_err(local)?;
        }
        TicketCommand::List { status } => {
            let mut tickets = session.tickets()?;
            if let Some(status) = status {
                tickets.retain(|read| status.matches(read.card.status));
            }
            let names = session.names();
            render::ticket::list(out, machine, &tickets, &names).map_err(local)?;
        }
        TicketCommand::Show { card } => {
            let read = session.read(*card)?;
            let names = session.names();
            render::ticket::show(out, machine, &read, &names, &chrono::Local).map_err(local)?;
        }
    }
    Ok(exit::SUCCESS)
}
