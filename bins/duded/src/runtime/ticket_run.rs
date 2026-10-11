//! Starting tickets and moving them for the daemon. A ticket starts on the existing goal
//! admission, with the baseline the daemon chooses and no check run first. The moves the daemon
//! owes are evaluated only after a whole handler's records are acknowledged and folded, so they
//! never fall between one handler's appends, and again at boot.

use std::collections::{BTreeMap, BTreeSet};

use dudes_proto::rpc::ErrorCode;
use dudes_proto::rpc::RpcError;
use dudes_proto::{Actor, CardView, Record, TicketReason, TicketRow, TicketWait, TicketWaiting};
use dudes_supervisor::Input;
use dudes_supervisor::input::Command;
use dudes_types::{AgentId, CardId, CardStatus, Lsn};

use super::{Clock, Daemon, PredicateRunner, ProcessHost, Rng, error};
use crate::floor_actions::Profile;
use crate::goal_replacement::Allowance;
use crate::ticket_queue::{Facts, Owed};

/// Most moves one evaluation makes before it stops. Every move changes the board, so the bound
/// is only a guard against a fold that never settles.
const MOST_MOVES: usize = 256;

/// The line between a ticket's text and the message that sent it back.
const SENT_BACK: &str = "The operator reviewed the previous work and asked for changes:";

/// What the pure functions read, owned so that the daemon can append while it holds the answer.
pub(super) struct Snapshot {
    pub(super) tickets: Vec<(CardView, TicketRow)>,
    hires: BTreeMap<AgentId, String>,
    working: BTreeSet<AgentId>,
}

/// The goal text of a ticket: its title, then its note, then the operator's message when the
/// work was sent back.
pub(super) fn goal_text(title: &str, note: &str, message: Option<&str>) -> String {
    let mut text = title.to_owned();
    if !note.is_empty() {
        text.push_str("\n\n");
        text.push_str(note);
    }
    if let Some(message) = message {
        text.push_str("\n\n");
        text.push_str(SENT_BACK);
        text.push('\n');
        text.push_str(message);
    }
    text
}

impl<H: ProcessHost, P: PredicateRunner, C: Clock + Send + Sync, R: Rng> Daemon<H, P, C, R> {
    pub(super) fn ticket_snapshot(&self) -> Result<Snapshot, RpcError> {
        let unreadable = |failure: dudes_journal::Error| {
            error(
                ErrorCode::InternalError,
                format!("could not read the board: {failure}"),
            )
        };
        let tickets = self.ctx.projections.tickets().map_err(unreadable)?;
        let hires: BTreeMap<AgentId, String> = self
            .ctx
            .projections
            .agents()
            .map_err(unreadable)?
            .into_iter()
            .filter_map(|row| Some((row.id, row.workspace?.id)))
            .collect();
        let working = hires
            .keys()
            .copied()
            .filter(|agent| self.work_beside(*agent))
            .collect();
        Ok(Snapshot {
            tickets,
            hires,
            working,
        })
    }

    /// Run `read` over the pure facts of the board as it stands.
    pub(super) fn with_ticket_facts<T>(
        &self,
        read: impl FnOnce(&Facts<'_>) -> T,
    ) -> Result<T, RpcError> {
        let snapshot = self.ticket_snapshot()?;
        let facts = Facts {
            world: self.sup.world(),
            tickets: &snapshot.tickets,
            hires: &snapshot.hires,
            working: &snapshot.working,
            closing: self.sup.is_closing(),
        };
        Ok(read(&facts))
    }

    /// The sentence for a wait, naming the holder and the ticket it needs to name.
    pub(super) fn waiting(
        &self,
        wait: TicketWait,
        agent: Option<AgentId>,
        tickets: &[(CardView, TicketRow)],
    ) -> TicketWaiting {
        let name = agent
            .and_then(|agent| self.sup.world().agents.get(&agent))
            .map_or("the agent", |row| row.name.as_str());
        let title = match wait {
            TicketWait::Ticket { card } => tickets
                .iter()
                .find(|(view, _)| view.id == card)
                .map_or("another ticket", |(view, _)| view.title.as_str()),
            _ => "",
        };
        TicketWaiting {
            wait,
            note: wait.sentence(name, title),
        }
    }

    /// Make every move the board owes and start every ticket a project can take, until nothing
    /// is left. Called with no handler half done: after a job, after an assignment's own append,
    /// and at boot.
    pub(super) fn ticket_reactions(&mut self) -> Result<(), RpcError> {
        let mut failed = BTreeSet::new();
        let mut failure = None;
        for _ in 0..MOST_MOVES {
            if self.ctx.journal_writes_stopped() || self.ctx.closing_failed_after.is_some() {
                break;
            }
            let (owed, heads) = self.with_ticket_facts(|facts| {
                let heads: Vec<CardId> = facts
                    .queued_projects()
                    .into_iter()
                    .filter_map(|project| facts.next_ticket(project))
                    .collect();
                (facts.owed_ticket_moves().into_iter().next(), heads)
            })?;
            let before = self.sup.world().last_applied_lsn;
            if let Some(owed) = owed {
                self.move_owed(&owed)?;
            } else if let Some(card) = heads.into_iter().find(|card| !failed.contains(card)) {
                // A start that fails must not keep the other projects from starting theirs.
                if let Err(refused) = self.start_ticket(card) {
                    failed.insert(card);
                    failure = Some(refused);
                    continue;
                }
            } else {
                break;
            }
            if self.sup.world().last_applied_lsn == before {
                break;
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Run the reactions when the journal moved since they last ran, or when `force` says a
    /// change that left no record may have freed an agent or a project.
    pub(super) fn react_to_tickets(&mut self, force: bool) {
        let at: Option<Lsn> = self.sup.world().last_applied_lsn;
        if !force && at == self.ctx.ticket_reacted {
            return;
        }
        // Only a clean evaluation marks the journal position as handled, so a start that failed
        // is offered again on the next job.
        if let Err(failure) = self.ticket_reactions() {
            tracing::error!(failure = %failure.message, "the ticket queue could not be advanced");
        } else {
            self.ctx.ticket_reacted = self.sup.world().last_applied_lsn;
        }
    }

    /// Append one move the board owes, as the daemon.
    fn move_owed(&mut self, owed: &Owed) -> Result<(), RpcError> {
        self.board_append(Record::CardTransition {
            card: owed.card,
            from: owed.from,
            to: owed.to,
            by: Actor::System,
            expected_seq: owed.seq,
            reason: None,
        })
    }

    /// Start a ticket that `next_ticket` named: build its goal, admit it as any goal is admitted,
    /// and append one record. A reason only the operator can fix moves the ticket to Needs you
    /// instead, so it leaves the queue and does not hold the project.
    fn start_ticket(&mut self, card: CardId) -> Result<(), RpcError> {
        let (view, ticket) = self.ticket_row(card)?;
        let Some(agent) = view.claimed_by else {
            return Ok(());
        };
        let admitted = match self.admit_ticket(agent, &ticket) {
            Ok(allowance) => allowance,
            Err(reason) => return self.refuse_start(&view, reason),
        };
        let text = goal_text(&view.title, &ticket.description, ticket.message.as_deref());
        self.pump_or_refuse(Input::Command(Command::TicketStart {
            card,
            agent,
            text,
            predicate: ticket.done_when.predicate.clone(),
            budget: admitted.budget,
            hop_cap: admitted.hop_cap,
            completion_check: crate::ticket_index::goal_check(&ticket.done_when),
            expected_seq: view.seq,
        }))
    }

    /// The admission `goal.set` applies, less the work-beside test the queue already guarantees:
    /// the project is enrolled, the agent is live, a goal that needs a ceiling has one, the
    /// check's program can run, and the per-turn reserve is set.
    fn admit_ticket(&self, agent: AgentId, ticket: &TicketRow) -> Result<Allowance, TicketReason> {
        let profile: &Profile = self
            .ctx
            .enrollment
            .profile(&ticket.project)
            .ok_or(TicketReason::ProjectUnavailable)?;
        let allowance = Allowance::of(profile);
        if self.goal_wants_a_ceiling(agent, &allowance.budget) {
            return Err(TicketReason::CeilingRequired);
        }
        self.check_can_run(agent, &ticket.done_when.predicate)
            .map_err(|_| TicketReason::CheckCannotRun)?;
        self.require_reserve(crate::reserve::Candidate::Goal(&allowance.budget))
            .map_err(|_| TicketReason::ReserveMissing)?;
        Ok(allowance)
    }

    /// Move a ticket that could not start into Needs you with the reason.
    fn refuse_start(&mut self, view: &CardView, reason: TicketReason) -> Result<(), RpcError> {
        self.board_append(Record::CardTransition {
            card: view.id,
            from: view.status,
            to: CardStatus::Blocked,
            by: Actor::System,
            expected_seq: view.seq,
            reason: Some(reason),
        })
    }
}
