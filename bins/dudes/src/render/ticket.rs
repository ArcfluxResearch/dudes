//! Output for the `ticket` commands.

use std::collections::HashMap;
use std::io::{self, Write};

use chrono::TimeZone;
use dudes_proto::instruction::{
    InstructionBaselineRef, InstructionEndBoundary, InstructionGapKind,
    InstructionIssueAssociation, InstructionObservationOwner, InstructionPathContext,
    InstructionPendingReview, InstructionReviewInterval,
};
use dudes_proto::rpc::Empty;
use dudes_proto::rpc::ticket::{
    TicketAcceptOutcome, TicketAcceptResult, TicketAssignOutcome, TicketAssignResult,
    TicketCreateResult, TicketReadResult,
};
use dudes_proto::{TicketCheckResult, TicketWait};
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
        TicketAssignOutcome::Started => writeln!(out, "{agent} is working on it."),
        TicketAssignOutcome::Queued { position, waiting } => match waiting.wait {
            TicketWait::AgentPaused => writeln!(out, "Queued. {agent} is paused."),
            _ => writeln!(
                out,
                "Queued for {agent}, {}. {}",
                ordinal(position),
                waiting.note
            ),
        },
        TicketAssignOutcome::NeedsYou { note, .. } => writeln!(out, "Needs you: {note}"),
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

/// Completed acceptance or explicit instruction acknowledgment that continues work.
///
/// # Errors
/// Returns the output writer's error.
pub fn accepted(out: &mut dyn Write, machine: bool, result: TicketAcceptResult) -> io::Result<()> {
    if machine {
        return json(out, &result);
    }
    writeln!(
        out,
        "{}",
        match result.outcome {
            Some(TicketAcceptOutcome::InstructionsAcknowledged) =>
                "Instruction review acknowledged. The ticket remains open.",
            None => "Ticket accepted.",
        }
    )
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
pub fn show<Tz: TimeZone>(
    out: &mut dyn Write,
    machine: bool,
    read: &TicketReadResult,
    names: &AgentNames,
    zone: &Tz,
) -> io::Result<()>
where
    Tz::Offset: std::fmt::Display,
{
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
    writeln!(out, "Sequence: {}", read.card.seq.0)?;
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
    if let Some(check) = read.ticket.check {
        writeln!(out, "Check: {}", check_line(check, zone))?;
    }
    if let Some(summary) = &read.ticket.summary {
        writeln!(out, "The agent says:")?;
        for line in summary.lines() {
            writeln!(out, "  {}", Escaped(line))?;
        }
    } else if read.ticket.round > 0 {
        writeln!(out, "The agent left no summary.")?;
    }
    if let Some(review) = &read.ticket.instruction_review {
        instruction_evidence(out, review, zone)?;
    }
    if read.ticket.round > 0 {
        writeln!(out, "Open the Floor to see the changed files.")?;
    }
    Ok(())
}

fn instruction_evidence<Tz: TimeZone>(
    out: &mut dyn Write,
    review: &InstructionPendingReview,
    zone: &Tz,
) -> io::Result<()>
where
    Tz::Offset: std::fmt::Display,
{
    writeln!(
        out,
        "Instruction review through record {}",
        review.watermark.through_lsn.0
    )?;
    write!(out, "Review digest: ")?;
    for byte in review.watermark.set_digest.bytes() {
        write!(out, "{byte:02x}")?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "These sampled endpoints do not identify an exclusive writer or prove that instructions were unchanged between samples."
    )?;
    if review.pending_comparison {
        writeln!(
            out,
            "The instruction comparison is still pending. Review actions are unavailable."
        )?;
    }
    writeln!(out, "Observed intervals:")?;
    for interval in &review.intervals {
        instruction_interval(out, interval, zone)?;
    }
    writeln!(out, "Known changed paths:")?;
    if review.changed_paths.is_empty() {
        writeln!(out, "  No known changed paths were retained.")?;
    }
    for change in &review.changed_paths {
        writeln!(
            out,
            "  {}{} ({})",
            change.path.display(),
            if change.path.display_truncated() {
                " [display truncated; use --json for the complete native path]"
            } else {
                ""
            },
            instruction_context(change.context)
        )?;
    }
    writeln!(out, "Coverage gaps:")?;
    if review.gaps.is_empty() {
        writeln!(out, "  No coverage gaps were recorded.")?;
    }
    for gap in &review.gaps {
        let reason = if gap.reason == InstructionGapKind::EndingBoundaryMissing {
            "The provider process ending boundary was not observed after restart".to_owned()
        } else {
            serde_json::to_value(gap.reason)
                .map_err(io::Error::other)?
                .as_str()
                .expect("instruction gap serializes as a string")
                .replace('_', " ")
        };
        write!(out, "  {} ({})", reason, instruction_context(gap.context))?;
        if let Some(agent) = gap.agent {
            write!(out, ", agent {agent}")?;
        }
        if let Some(path) = &gap.path {
            write!(
                out,
                ", {}{}",
                path.display(),
                if path.display_truncated() {
                    " [display truncated; use --json for the complete native path]"
                } else {
                    ""
                }
            )?;
        }
        writeln!(out, ".")?;
    }
    Ok(())
}

fn instruction_interval<Tz: TimeZone>(
    out: &mut dyn Write,
    interval: &InstructionReviewInterval,
    zone: &Tz,
) -> io::Result<()>
where
    Tz::Offset: std::fmt::Display,
{
    match interval.owner {
        InstructionObservationOwner::Managed { agent, turn, issue } => {
            writeln!(out, "  Managed turn {turn}, agent {agent}.")?;
            match issue {
                InstructionIssueAssociation::Prospective => {
                    writeln!(out, "  Prospective issue.")?;
                }
                InstructionIssueAssociation::Issued { issue_lsn } => {
                    writeln!(out, "  Issue record {}.", issue_lsn.0)?;
                }
            }
        }
        InstructionObservationOwner::AttachedProcess {
            agent,
            attachment_lsn,
        } => {
            writeln!(
                out,
                "  Attached process, agent {agent}, attachment record {}.",
                attachment_lsn.0
            )?;
        }
        InstructionObservationOwner::AttachedTurn {
            agent,
            turn,
            attachment_lsn,
        } => {
            writeln!(
                out,
                "  Attached turn {turn}, agent {agent}, attachment record {}.",
                attachment_lsn.map_or_else(|| "unknown".to_owned(), |lsn| lsn.0.to_string())
            )?;
        }
    }
    writeln!(
        out,
        "  Goal {}, assignment record {}, recorded ticket sequence {}.",
        interval.ticket.goal, interval.ticket.assignment_lsn.0, interval.ticket.seq_at_start.0
    )?;
    match interval.baseline {
        InstructionBaselineRef::Started { lsn } => {
            writeln!(out, "  Baseline record {}.", lsn.0)?;
        }
        InstructionBaselineRef::MissingIssueBoundary { turn, issue_lsn } => {
            writeln!(
                out,
                "  The pre-launch instruction baseline is missing for issued turn {turn}. Actual issue record {}. Provider launch is uncertain.",
                issue_lsn.0
            )?;
        }
        InstructionBaselineRef::MissingTurnBoundary { turn, end_lsn } => {
            writeln!(
                out,
                "  Starting boundary missing for turn {turn}, end record {}.",
                end_lsn.0
            )?;
        }
    }
    if let Some(end) = interval.end {
        let lsn = match end {
            InstructionEndBoundary::ManagedTurn { end_lsn, .. }
            | InstructionEndBoundary::AttachedProcess { end_lsn, .. }
            | InstructionEndBoundary::AttachedTurn { end_lsn, .. } => end_lsn,
        };
        writeln!(out, "  End record {}.", lsn.0)?;
    }
    if let Some(lsn) = interval.finished_lsn {
        writeln!(out, "  Settlement record {}.", lsn.0)?;
    } else {
        writeln!(out, "  Comparison is outstanding.")?;
    }
    if let Some(at) = interval.sampled_at {
        writeln!(
            out,
            "  Sampled at {}{}.",
            super::zoned(at, zone),
            if interval.recovery {
                " during recovery"
            } else {
                ""
            }
        )?;
    }
    Ok(())
}

fn instruction_context(context: InstructionPathContext) -> String {
    match context {
        InstructionPathContext::OwnedWorktree { lease } => format!("owned worktree {lease}"),
        InstructionPathContext::SharedProject => "shared project".to_owned(),
        InstructionPathContext::UnsharedProject => "unshared project".to_owned(),
        InstructionPathContext::SharedInput { source } => {
            let source = serde_json::to_value(source).expect("instruction source is serializable");
            format!(
                "shared {}",
                source
                    .as_str()
                    .expect("instruction source serializes as a string")
                    .replace('_', " ")
            )
        }
        InstructionPathContext::Unresolved => "unresolved context".to_owned(),
    }
}

fn check_line<Tz: TimeZone>(check: TicketCheckResult, zone: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let verdict = if check.pass { "passed" } else { "failed" };
    let exit = check.exit.map_or_else(
        || "no exit status".to_owned(),
        |code| format!("exit status {code}"),
    );
    format!("{verdict}, {exit}, {}", super::zoned(check.at, zone))
}

#[cfg(test)]
mod instruction_tests {
    use super::*;

    #[test]
    fn ordinary_acceptance_keeps_the_empty_machine_reply_and_completed_copy() {
        let mut machine = Vec::new();
        accepted(&mut machine, true, TicketAcceptResult::default()).expect("write");
        assert_eq!(String::from_utf8(machine).expect("utf8"), "{}\n");
        let mut human = Vec::new();
        accepted(&mut human, false, TicketAcceptResult::default()).expect("write");
        assert_eq!(
            String::from_utf8(human).expect("utf8"),
            "Ticket accepted.\n"
        );
    }

    #[test]
    fn acknowledgment_reports_continuing_work_in_both_formats() {
        let result = TicketAcceptResult {
            outcome: Some(TicketAcceptOutcome::InstructionsAcknowledged),
        };
        let mut machine = Vec::new();
        accepted(&mut machine, true, result).expect("write");
        assert_eq!(
            String::from_utf8(machine).expect("utf8"),
            "{\"outcome\":\"instructions_acknowledged\"}\n"
        );
        let mut human = Vec::new();
        accepted(&mut human, false, result).expect("write");
        assert_eq!(
            String::from_utf8(human).expect("utf8"),
            "Instruction review acknowledged. The ticket remains open.\n"
        );
    }

    #[test]
    fn a_missing_issue_reports_launch_uncertainty_without_an_invented_start_or_end() {
        let review: InstructionPendingReview = serde_json::from_value(serde_json::json!({
            "watermark":{"through_lsn":14,"set_digest":"d".repeat(64)}, "pending_comparison":false,
            "intervals":[{
                "owner":{"kind":"managed","agent":"01JBQ8XK2M3N4P5Q6R7S8T9VWX","turn":"01JBQ8XK2M3N4P5Q6R7S8T0040",
                    "issue":{"kind":"issued","issue_lsn":10}},
                "baseline":{"kind":"missing_issue_boundary","turn":"01JBQ8XK2M3N4P5Q6R7S8T0040","issue_lsn":10},
                "ticket":{"card":"01JBQ8XK2M3N4P5Q6R7S8T0001","goal":"01JBQ8XK2M3N4P5Q6R7S8T0041","assignment_lsn":9,"seq_at_start":2},
                "finished_lsn":14,"end":null,"sampled_at":null,"recovery":true
            }], "changed_paths":[],
            "gaps":[{"reason":"missing_baseline","context":{"kind":"unresolved"},"agent":null,"path":null}]
        })).expect("recorded missing-issue review");
        let mut output = Vec::new();
        instruction_evidence(&mut output, &review, &chrono::Utc).expect("write");
        let output = String::from_utf8(output).expect("utf8");
        assert!(output.contains("The pre-launch instruction baseline is missing for issued turn"));
        assert!(output.contains("Actual issue record 10. Provider launch is uncertain."));
        assert!(output.contains("recorded ticket sequence 2"));
        assert!(
            !output.contains("Baseline record")
                && !output.contains("End record")
                && !output.contains("Sampled at")
        );
    }

    #[test]
    fn a_missing_recovered_ending_does_not_imply_a_missing_start_or_confirmed_end() {
        let review: InstructionPendingReview = serde_json::from_value(serde_json::json!({
            "watermark":{"through_lsn":14,"set_digest":"d".repeat(64)}, "pending_comparison":false,
            "intervals":[{
                "owner":{"kind":"managed","agent":"01JBQ8XK2M3N4P5Q6R7S8T9VWX","turn":"01JBQ8XK2M3N4P5Q6R7S8T0040",
                    "issue":{"kind":"issued","issue_lsn":10}},
                "baseline":{"kind":"started","lsn":11},
                "ticket":{"card":"01JBQ8XK2M3N4P5Q6R7S8T0001","goal":"01JBQ8XK2M3N4P5Q6R7S8T0041","assignment_lsn":9,"seq_at_start":2},
                "finished_lsn":14,"end":null,"sampled_at":null,"recovery":true
            }], "changed_paths":[],
            "gaps":[{"reason":"ending_boundary_missing","context":{"kind":"unshared_project"},"agent":null,"path":null}]
        })).expect("recorded missing-ending review");
        let mut output = Vec::new();
        instruction_evidence(&mut output, &review, &chrono::Utc).expect("write");
        let output = String::from_utf8(output).expect("utf8");
        assert!(
            output.contains("The provider process ending boundary was not observed after restart")
        );
        assert!(output.contains("Baseline record 11."));
        assert!(
            !output.contains("baseline is missing")
                && !output.contains("End record")
                && !output.contains("Sampled at")
        );
        assert!(!output.contains("process ended") && !output.contains("cleanup attempted"));
    }
}
