//! Output for the `ticket` commands.

use std::collections::HashMap;
use std::io::{self, Write};

use dudes_proto::rpc::Empty;
use dudes_proto::rpc::ticket::{
    TicketAssignOutcome, TicketAssignResult, TicketCreateResult, TicketReadResult,
};
use dudes_types::{AgentId, CardStatus};

use super::{Escaped, json};

/// Agent display names by id, for the lines that say who holds a ticket. An id with no name here
/// prints as the id.
pub type AgentNames = HashMap<AgentId, String>;

/// An agent's display name, escaped for the terminal.
pub fn agent_name(names: &AgentNames, agent: AgentId) -> String {
    names
        .get(&agent)
        .map_or_else(|| agent.to_string(), |name| Escaped(name).to_string())
}

fn ordinal(position: u32) -> String {
    let suffix = match (position % 100, position % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    format!("{position}{suffix}")
}

/// `ticket.create`: the new card id, which is all a script needs from the line.
///
/// # Errors
///
/// Whatever writing returns.
pub fn added(out: &mut dyn Write, machine: bool, result: &TicketCreateResult) -> io::Result<()> {
    if machine {
        return json(out, result);
    }
    writeln!(out, "ticket {}", result.card)
}

/// `ticket.assign`: where the ticket stands in the agent's queue.
///
/// # Errors
///
/// Whatever writing returns.
pub fn given(
    out: &mut dyn Write,
    machine: bool,
    agent: &str,
    result: TicketAssignResult,
) -> io::Result<()> {
    if machine {
        return json(out, &result);
    }
    match result.outcome {
        TicketAssignOutcome::Queued { position } => {
            writeln!(out, "Queued for {agent}, {}.", ordinal(position))
        }
    }
}

/// `ticket.edit` and `ticket.remove` answer with nothing, so the line says what was done.
///
/// # Errors
///
/// Whatever writing returns.
pub fn changed(out: &mut dyn Write, machine: bool, done: &str, result: Empty) -> io::Result<()> {
    if machine {
        return json(out, &result);
    }
    writeln!(out, "{done}")
}

fn state(read: &TicketReadResult, names: &AgentNames) -> String {
    let state = dudes_proto::ticket::state_name(read.card.status);
    match (read.card.status, read.card.claimed_by, read.position) {
        (CardStatus::Claimed, Some(agent), Some(position)) => format!(
            "{state}, queued for {}, {}",
            agent_name(names, agent),
            ordinal(position)
        ),
        (_, Some(agent), _) => format!("{state}, {}", agent_name(names, agent)),
        _ => state.to_owned(),
    }
}

/// `ticket list`: one line per ticket, or nothing when none matches.
///
/// # Errors
///
/// Whatever writing returns.
pub fn list(
    out: &mut dyn Write,
    machine: bool,
    tickets: &[TicketReadResult],
    names: &AgentNames,
) -> io::Result<()> {
    if machine {
        return json(out, &tickets);
    }
    for read in tickets {
        writeln!(
            out,
            "{}  {}  {}  {}",
            read.card.id,
            state(read, names),
            Escaped(&read.ticket.project),
            Escaped(&read.card.title)
        )?;
    }
    Ok(())
}

/// `ticket.read`: everything the board keeps of one ticket.
///
/// # Errors
///
/// Whatever writing returns.
pub fn show(
    out: &mut dyn Write,
    machine: bool,
    read: &TicketReadResult,
    names: &AgentNames,
) -> io::Result<()> {
    if machine {
        return json(out, read);
    }
    writeln!(
        out,
        "Ticket {}: {}",
        read.card.id,
        Escaped(&read.card.title)
    )?;
    writeln!(out, "State: {}", state(read, names))?;
    if let Some(reason) = read.ticket.reason {
        writeln!(out, "Needs you: {}", reason.sentence())?;
    }
    writeln!(out, "Project: {}", Escaped(&read.ticket.project))?;
    writeln!(out, "Done when: {}", Escaped(&read.ticket.done_when.label))?;
    writeln!(out, "Round: {}", read.ticket.round)?;
    if !read.ticket.description.is_empty() {
        writeln!(out, "Note:")?;
        for line in read.ticket.description.lines() {
            writeln!(out, "  {}", Escaped(line))?;
        }
    }
    Ok(())
}
