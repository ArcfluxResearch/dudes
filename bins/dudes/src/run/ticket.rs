//! `dudes ticket`: each command is one or two operator calls, answered at once.

use std::io::Write;

use dudes_proto::instruction::InstructionReviewWatermark;
use dudes_proto::rpc::board::CardListParams;
use dudes_proto::rpc::ticket::{
    TicketAcceptParams, TicketAcceptResult, TicketAssignParams, TicketAssignResult,
    TicketCheckoutParams, TicketCheckoutResult, TicketCreateParams, TicketCreateResult,
    TicketEditParams, TicketReadParams, TicketReadResult, TicketRemoveParams, TicketStopParams,
};
use dudes_proto::rpc::{CurrentMethod, Empty, ErrorCode, Method, RpcError};
use dudes_proto::{AgentView, TicketRefusal};
use dudes_types::{AgentId, CardId, CardStatus, Seq};

use super::{
    Failure, Socket, answer_result, call_presenting, connect, encode, local, operator_credential,
    typed,
};
use crate::args::{AgentRef, EditorSelection, TicketCommand, TicketStatus};
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
    fn open(
        &mut self,
        card: CardId,
        selection: &EditorSelection,
        machine: bool,
        out: &mut dyn Write,
    ) -> Result<(), Failure> {
        let expected_seq = self.seq(card, None)?;
        let cred = operator_credential(self.env)?;
        let params = encode(&TicketCheckoutParams { card, expected_seq })?;
        let result: TicketCheckoutResult = typed(answer_result(
            self.client
                .call_current_presenting(CurrentMethod::TicketCheckout, Some(params), cred)
                .map_err(local)?,
        )?)?;
        crate::checkout::validate(&result).map_err(local)?;
        let disposition = crate::editor::handoff(&result, selection, machine).map_err(local)?;
        if machine {
            render::json(out, &result).map_err(local)?;
        } else {
            writeln!(
                out,
                "{}",
                render::Escaped(&result.directory.to_string_lossy())
            )
            .map_err(local)?;
            if disposition == crate::editor::PrintDisposition::LaunchForm {
                writeln!(
                    out,
                    "To open an editor, run `dudes ticket open {card} --editor /absolute/program`."
                )
                .map_err(local)?;
            }
        }
        Ok(())
    }

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
        self.assign(card, agent, message, named, None)
    }

    fn assign(
        &mut self,
        card: CardId,
        agent: AgentId,
        message: Option<String>,
        named: Option<Seq>,
        review_watermark: Option<InstructionReviewWatermark>,
    ) -> Result<(TicketAssignResult, String), Failure> {
        let expected_seq = self.seq(card, named)?;
        let params = encode(&TicketAssignParams {
            card,
            agent,
            message,
            expected_seq,
            review_watermark,
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
        let (expected_seq, review_watermark) = review_parameters(&read, named)?;
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
            Some(expected_seq),
            review_watermark,
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

fn review_parameters(
    read: &TicketReadResult,
    named: Option<Seq>,
) -> Result<(Seq, Option<InstructionReviewWatermark>), Failure> {
    let Some(review) = &read.ticket.instruction_review else {
        return Ok((named.unwrap_or(read.card.seq), None));
    };
    let Some(expected_seq) = named else {
        return Err(Failure::Refused(RpcError {
            code: ErrorCode::Refused.code(),
            message: format!(
                "{} Read `dudes ticket show {}`, then retry with --expected-seq {}.",
                TicketRefusal::InstructionReviewRequired.sentence(read.card.status),
                read.card.id,
                read.card.seq.0
            ),
            data: Some(
                serde_json::json!({"reason":TicketRefusal::InstructionReviewRequired.code()}),
            ),
        }));
    };
    if expected_seq != read.card.seq {
        return Err(Failure::Refused(RpcError {
            code: ErrorCode::Conflict.code(),
            message: TicketRefusal::StaleCard.sentence(read.card.status),
            data: Some(serde_json::json!({"current_seq":read.card.seq})),
        }));
    }
    if review.pending_comparison {
        return Err(Failure::Refused(RpcError {
            code: ErrorCode::Refused.code(),
            message: TicketRefusal::InstructionComparisonPending.sentence(read.card.status),
            data: Some(
                serde_json::json!({"reason":TicketRefusal::InstructionComparisonPending.code()}),
            ),
        }));
    }
    Ok((expected_seq, Some(review.watermark)))
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
        TicketCommand::Open { card, selection } => {
            session.open(*card, selection, machine, out)?;
        }
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
            let read = session.read(*card)?;
            let (expected_seq, review_watermark) = review_parameters(&read, *expected_seq)?;
            let params = encode(&TicketAcceptParams {
                card: *card,
                expected_seq,
                review_watermark,
            })?;
            let result: TicketAcceptResult =
                typed(session.call(Method::TicketAccept, Some(params))?)?;
            render::ticket::accepted(out, machine, result).map_err(local)?;
        }
        TicketCommand::Stop { card, expected_seq } => {
            let params = encode(&TicketStopParams {
                card: *card,
                expected_seq: session.seq(*card, *expected_seq)?,
            })?;
            let result: Empty = typed(session.call(Method::TicketStop, Some(params))?)?;
            render::ticket::changed(out, machine, "Ticket stopped.", result).map_err(local)?;
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

#[cfg(test)]
mod instruction_tests {
    use super::*;
    use dudes_proto::instruction::InstructionPendingReview;
    use dudes_proto::record::Hex32;
    use dudes_types::Lsn;

    fn reading() -> TicketReadResult {
        serde_json::from_value(serde_json::json!({
            "card": {"id":"01JBQ8XK2M3N4P5Q6R7S8T9VWX", "title":"Review instructions", "department":"engineering",
                "status":"blocked", "status_by":{"kind":"system"}, "seq":7},
            "ticket": {"description":"Read the evidence", "project":"service", "round":1,
                "done_when":{"id":"default", "label":"passes", "predicate":{"command":{"argv":["false"], "timeout_s":1}}}},
        })).expect("read fixture")
    }

    fn shown(status: &str) -> String {
        let mut read: TicketReadResult = serde_json::from_value(serde_json::json!({
            "card": {"id":"01JBQ8XK2M3N4P5Q6R7S8T9VWX", "title":"Ready", "department":"engineering",
                "status":status, "status_by":{"kind":"system"}, "seq":7},
            "ticket": {"description":"", "project":"service", "round":1,
                "done_when":{"id":"default", "label":"passes", "predicate":{"command":{"argv":["true"], "timeout_s":1}}},
                "coverage": {"provider":"codex", "limits":["configuration_unknown","root_marker_unknown"], "saw_no_change":true}},
        })).expect("read fixture");
        read.ticket.summary = Some("done".to_owned());
        let mut out = Vec::new();
        render::ticket::show(&mut out, false, &read, &AgentNames::new(), &chrono::Utc)
            .expect("write");
        String::from_utf8(out).expect("utf8")
    }

    #[test]
    fn a_ready_or_held_ticket_names_what_dudes_could_not_see() {
        assert!(shown("review").contains(
            "Coverage: Dudes cannot see Codex's settings in its home folder or root markers. It saw no change to the instruction and settings files it can read.\n"
        ));
        assert!(shown("blocked").contains(
            "Coverage: Dudes cannot see Codex's settings in its home folder or root markers."
        ));
    }

    #[test]
    fn instruction_review_requires_the_sequence_the_operator_named() {
        let mut read = reading();
        let watermark = InstructionReviewWatermark {
            through_lsn: Lsn(9_007_199_254_740_993),
            set_digest: Hex32::new([0xdd; 32]),
        };
        read.ticket.instruction_review = Some(InstructionPendingReview {
            watermark,
            pending_comparison: false,
            intervals: Vec::new(),
            changed_paths: Vec::new(),
            gaps: Vec::new(),
        });
        let Failure::Refused(missing) =
            review_parameters(&read, None).expect_err("explicit sequence required")
        else {
            panic!("refusal");
        };
        assert_eq!(
            missing.data.expect("reason")["reason"],
            "instruction_review_required"
        );
        let Failure::Refused(stale) =
            review_parameters(&read, Some(Seq(6))).expect_err("old sequence")
        else {
            panic!("conflict");
        };
        assert_eq!(stale.code, ErrorCode::Conflict.code());
        assert_eq!(stale.data.expect("sequence")["current_seq"], 7);
        assert_eq!(
            review_parameters(&read, Some(Seq(7))).expect("exact review"),
            (Seq(7), Some(watermark))
        );
        read.ticket
            .instruction_review
            .as_mut()
            .expect("review")
            .pending_comparison = true;
        let Failure::Refused(pending) =
            review_parameters(&read, Some(Seq(7))).expect_err("pending comparison")
        else {
            panic!("refusal");
        };
        assert_eq!(
            pending.data.expect("reason")["reason"],
            "instruction_comparison_pending"
        );
        read.ticket.instruction_review = None;
        assert_eq!(
            review_parameters(&read, None).expect("ordinary"),
            (Seq(7), None)
        );
        assert_eq!(
            review_parameters(&read, Some(Seq(6))).expect("ordinary named"),
            (Seq(6), None)
        );
    }
}
