//! `dudes ticket`: each command is one or two operator calls, answered at once.

use std::io::Write;

use dudes_proto::rpc::board::CardListParams;
use dudes_proto::rpc::ticket::{
    TicketAssignParams, TicketAssignResult, TicketCreateResult, TicketEditParams, TicketReadParams,
    TicketReadResult, TicketRemoveParams,
};
use dudes_proto::rpc::{Empty, Method};
use dudes_types::{AgentId, CardId, CardStatus, Seq};

use super::{
    Failure, Socket, answer_result, call_presenting, connect, encode, local, operator_credential,
    typed,
};
use crate::args::{TicketCommand, TicketStatus};
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

    fn names(&mut self) -> AgentNames {
        let Ok(agents) = self.call(Method::AgentList, None) else {
            return AgentNames::new();
        };
        typed::<Vec<dudes_proto::AgentView>>(agents)
            .unwrap_or_default()
            .into_iter()
            .map(|agent| (agent.id, agent.name))
            .collect()
    }

    fn give(
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
            let created: TicketCreateResult =
                typed(session.call(Method::TicketCreate, Some(encode(params)?))?)?;
            render::ticket::added(out, machine, &created).map_err(local)?;
            if let Some(agent) = give_to {
                let (result, name) = session.give(created.card, *agent, None, None)?;
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
            let (result, name) = session.give(*card, *agent, message.clone(), *expected_seq)?;
            render::ticket::given(out, machine, &name, result).map_err(local)?;
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
            render::ticket::show(out, machine, &read, &names).map_err(local)?;
        }
    }
    Ok(exit::SUCCESS)
}
