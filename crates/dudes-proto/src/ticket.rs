//! A ticket is a board card with a note, a project and a required completion check. These are the
//! values its journal records carry and the words the operator sees when a request is refused.

use dudes_types::{CardId, CardStatus, GoalId, Lsn, Timestamp};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::goal::Predicate;
use crate::instruction::InstructionPendingReview;

/// Most bytes in a ticket title.
pub const MAX_TITLE_BYTES: usize = 256;
/// Most bytes in a ticket note.
pub const MAX_NOTE_BYTES: usize = 8_192;
/// Most bytes in the message that accompanies an assignment.
pub const MAX_MESSAGE_BYTES: usize = 2_048;
/// Most tickets one agent's queue holds.
pub const MAX_QUEUE: usize = 8;
/// Most tickets the board holds that are neither removed nor done.
pub const MAX_OPEN_TICKETS: usize = 512;
/// Most bytes of an agent's last message the board keeps and shows as a ticket's summary.
pub const MAX_SUMMARY_BYTES: usize = 4_096;

/// The check a ticket is finished by, as it was when the ticket was made or last edited. Storing
/// the resolved predicate means a later enrollment edit cannot change what an existing ticket is
/// checked by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketCheck {
    /// The check's id within its project.
    pub id: String,
    /// The check's label.
    pub label: String,
    /// The typed command the daemon runs.
    pub predicate: Predicate,
}

/// What the daemon's latest run of a ticket's check found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketCheckResult {
    /// Whether the check passed.
    pub pass: bool,
    /// The check's exit code; absent when it was killed or never ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    /// When the run was recorded.
    pub at: Timestamp,
}

/// What the board keeps of a ticket beyond what every card has.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketRow {
    /// The note; empty when there is none.
    pub description: String,
    /// The enrolled project the ticket runs in.
    pub project: String,
    /// The check that finishes it.
    pub done_when: TicketCheck,
    /// The goal the ticket's latest start installed. Kept as history after the work stops, and
    /// cleared when the ticket is given to an agent again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalId>,
    /// Its place in its holder's queue, while it waits there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<QueueKey>,
    /// What the operator said when sending the work back, until the next start uses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// How many times work on the ticket has started.
    #[serde(default)]
    pub round: u32,
    /// Why the daemon moved the ticket into Needs you, while it is there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<TicketReason>,
    /// What the agent said at the end of its newest turn on the ticket's goal, cut to
    /// [`MAX_SUMMARY_BYTES`]. These are the agent's own words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The daemon's newest run of the check on the ticket's goal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<TicketCheckResult>,
    /// Bounded instruction evidence and the exact pending review set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_review: Option<InstructionPendingReview>,
    /// The standing engine limits Dudes met while this ticket's work passed without a seen
    /// change. It is shown on Ready for you and holds nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<crate::instruction::TicketCoverageNote>,
}

/// Which side of the queue a ticket sits on. Given-again work is served before new work.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum QueueClass {
    /// Given again from Ready for you or Needs you.
    Rework,
    /// Given for the first time.
    New,
}

/// A ticket's place in its agent's queue: class first, then the position of the assignment.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub struct QueueKey {
    /// The class.
    pub class: QueueClass,
    /// The journal position of the `TicketAssigned` that queued it. Positions are unique, so keys
    /// never tie.
    pub lsn: Lsn,
}

/// Why a ticket is in Needs you. Written on the daemon's own move into that lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TicketReason {
    /// The project is not enrolled any more.
    ProjectUnavailable,
    /// The check's program was not found, or it is a file inside the project that an agent could
    /// change.
    CheckCannotRun,
    /// A spending limit is required and none is set.
    CeilingRequired,
    /// The per-turn spending reserve is not set.
    ReserveMissing,
    /// The agent's goal was changed outside the board.
    GoalChanged,
    /// The ticket reached its spending limit.
    LimitGoal,
    /// The agent kept failing and was slowed down.
    Slowed,
    /// The agent used all its turns on the ticket.
    NoTurns,
    /// The agent's engine would not start.
    HeldLaunch,
    /// The agent's turns kept failing at once.
    HeldFailing,
    /// The daemon could not confirm how the agent's last turn ended.
    HeldUnsure,
    /// The agent's worktree is missing.
    WorktreeMissing,
    /// Known net changes to declared shared instructions need Operator review.
    SharedInstructionsChanged,
    /// Shared instruction coverage is incomplete and needs Operator review.
    SharedInstructionsUnknown,
    /// Interrupted work requires an explicit recovery choice.
    RecoveryRequired,
}

impl TicketReason {
    /// The sentence the operator reads, which names the next step.
    #[must_use]
    pub const fn sentence(self) -> &'static str {
        match self {
            Self::RecoveryRequired => {
                "This work was interrupted. Recovery keeps the same goal and ticket round. Review recovery to see what was recorded and choose how to continue. Earlier effects may repeat."
            }
            Self::ProjectUnavailable => {
                "This project is not set up any more. Enroll it again with `dudes project add`, then Give to agent, or Remove the ticket."
            }
            Self::CheckCannotRun => {
                "The check cannot run: its program is missing, or it is a file inside the project that an agent could change. Install the program, or use one outside the project's repository. To use a script from the repository, run it through its interpreter, for example `sh test.sh` for a shell script; an agent can still edit that script, so review its changes before you accept the work. Then Give to agent. If the check itself is wrong, Remove this ticket and create it again."
            }
            Self::CeilingRequired => {
                "Set a spending limit first, because your account bills per use. Add one to the project's enrollment (`dudes project add`) or the agent's hire, then Give to agent."
            }
            Self::ReserveMissing => {
                "A per-turn spending reserve is missing. Run `dudes config set budget.per_turn_floor.usd <dollars>` (or `budget.per_turn_floor.tokens <count>`), then Give to agent."
            }
            Self::GoalChanged => {
                "The agent's goal was changed outside the board. Give to agent to try again, or Stop."
            }
            Self::LimitGoal => {
                "This ticket reached its spending limit. Give to agent to try again, or Stop."
            }
            Self::Slowed => {
                "The agent kept failing and was slowed down. Look at the agent, then Give to agent to try again, or Stop."
            }
            Self::NoTurns => {
                "The agent used all its turns on this ticket. Give to agent to try again, or Stop."
            }
            Self::HeldLaunch => {
                "The agent's engine would not start. Fix the cause, then resume the agent."
            }
            Self::HeldFailing => {
                "The agent's turns kept failing at once. Fix the cause, then resume the agent."
            }
            Self::HeldUnsure => {
                "Dudes could not confirm how the agent's last turn ended. Resume the agent, or Stop."
            }
            Self::WorktreeMissing => {
                "The agent's worktree is missing. Restore it and resume the agent, or Stop and give the ticket to another agent."
            }
            Self::SharedInstructionsChanged => {
                "Shared project instructions changed during this work. Review the change, then Accept or Ask for changes."
            }
            Self::SharedInstructionsUnknown => {
                "Dudes could not determine whether shared project instructions changed during this work. Review the work, then Accept or Ask for changes."
            }
        }
    }
}

/// Why a ticket that was given to an agent has not started. Nothing is journaled for it: the
/// card shows the note, and the daemon offers the ticket again when something relevant changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TicketWait {
    /// The ticket has not been given to an agent.
    NotGiven,
    /// The agent has a turn, a check run or an unfinished goal.
    AgentBusy,
    /// Another ticket holds the project.
    Ticket {
        /// The ticket that holds it.
        card: CardId,
    },
    /// The project has a turn or a check run that no ticket started.
    ProjectBusy,
    /// The agent is halted, paused or held.
    AgentPaused,
    /// A person holds the agent's keyboard.
    AgentAttached,
    /// The daemon is shutting down.
    Closing,
    /// The ticket was stopped and the agent is still finishing its current step.
    Stopping,
    /// The agent reached its daily limit and carries on by itself when it resets.
    AgentLimit,
    /// The Floor reached its daily limit and carries on by itself when it resets.
    FloorLimit,
}

/// A wait and the sentence that says it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketWaiting {
    /// Which wait.
    pub wait: TicketWait,
    /// The sentence the operator reads.
    pub note: String,
}

impl TicketWait {
    /// The sentence the operator reads. `agent` is the holder's name and `title` the title of the
    /// ticket that holds the project; each is used only by the wait that names it.
    #[must_use]
    pub fn sentence(self, agent: &str, title: &str) -> String {
        match self {
            Self::NotGiven => "Not given to an agent yet. Choose Give to agent.".to_owned(),
            Self::AgentBusy => format!("Waiting for {agent} to finish its current work."),
            Self::Ticket { .. } => {
                format!("Waiting for {title} to be finished, accepted or sent back.")
            }
            Self::ProjectBusy => "Waiting for the project to be free.".to_owned(),
            Self::AgentPaused => format!("Waiting for {agent} to be resumed."),
            Self::AgentAttached => format!("Waiting for {agent}'s keyboard session to end."),
            Self::Closing => "Dudes is shutting down.".to_owned(),
            Self::Stopping => "Stopping: the agent is finishing its current step.".to_owned(),
            Self::AgentLimit => {
                "The agent reached its daily limit. It carries on by itself when the limit resets."
                    .to_owned()
            }
            Self::FloorLimit => {
                "The Floor reached its daily limit. It carries on by itself when the limit resets."
                    .to_owned()
            }
        }
    }
}

/// Why a ticket request was refused before anything was written. A malformed request, such as an
/// empty title, is an ordinary invalid-params error and is not one of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TicketRefusal {
    /// The card's sequence moved on.
    StaleCard,
    /// The card is a plain board card.
    NotATicket,
    /// The card is a ticket, and a board method was asked to move it.
    TicketCard,
    /// The ticket's state does not allow the request.
    WrongState,
    /// The project is not enrolled.
    ProjectUnavailable,
    /// The project's check changed since the caller looked.
    DoneWhenChanged,
    /// The agent is unknown, retired, or not hired into the ticket's project.
    AgentUnavailable,
    /// The agent was hired before projects were recorded.
    AgentNoProject,
    /// The agent's queue is full.
    QueueFull,
    /// The board holds too many open tickets.
    LimitReached,
    /// The session already has a ticket request running.
    Busy,
    /// A related instruction comparison is unfinished.
    InstructionComparisonPending,
    /// The caller supplied no exact current instruction review evidence.
    InstructionReviewRequired,
}

impl TicketRefusal {
    /// The fixed code, as it appears in `data.reason`.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::StaleCard => "stale_card",
            Self::NotATicket => "not_a_ticket",
            Self::TicketCard => "ticket_card",
            Self::WrongState => "wrong_state",
            Self::ProjectUnavailable => "project_unavailable",
            Self::DoneWhenChanged => "done_when_changed",
            Self::AgentUnavailable => "agent_unavailable",
            Self::AgentNoProject => "agent_no_project",
            Self::QueueFull => "queue_full",
            Self::LimitReached => "limit_reached",
            Self::Busy => "busy",
            Self::InstructionComparisonPending => "instruction_comparison_pending",
            Self::InstructionReviewRequired => "instruction_review_required",
        }
    }

    /// The sentence the operator reads. `state` is the ticket's state, used by `wrong_state` only.
    #[must_use]
    pub fn sentence(self, state: CardStatus) -> String {
        match self {
            Self::StaleCard => {
                "This ticket changed while you were looking. Reload and try again.".to_owned()
            }
            Self::NotATicket => "That card is not a ticket.".to_owned(),
            Self::TicketCard => "That card is a ticket. Use the ticket commands.".to_owned(),
            Self::WrongState => {
                format!("You can't do that to a ticket that is {}.", state_name(state))
            }
            Self::ProjectUnavailable => "That project is not set up any more.".to_owned(),
            Self::DoneWhenChanged => {
                "The Done when check for this project changed. Choose it again.".to_owned()
            }
            Self::AgentUnavailable => "That agent is not available for this project.".to_owned(),
            Self::AgentNoProject => {
                "That agent was hired before projects were recorded. Hire a new agent in this project with `dudes hire`."
                    .to_owned()
            }
            Self::QueueFull => "That agent already has 8 tickets waiting.".to_owned(),
            Self::LimitReached => "Too many tickets for now. Remove some, or wait.".to_owned(),
            Self::Busy => "Another ticket request is still running. Try again in a moment.".to_owned(),
            Self::InstructionComparisonPending => "Instruction comparison is still pending. Read the ticket again when it finishes.".to_owned(),
            Self::InstructionReviewRequired => "Read the current instruction review before acknowledging it.".to_owned(),
        }
    }
}

/// The agent's words as the board keeps and shows them: control characters other than newline and
/// tab removed, then cut to [`MAX_SUMMARY_BYTES`] at a character boundary.
#[must_use]
pub fn summary_text(text: &str) -> String {
    let mut kept = String::new();
    for c in text.chars() {
        if (c.is_control() && c != '\n' && c != '\t') || is_bidi_control(c) {
            continue;
        }
        if kept.len() + c.len_utf8() > MAX_SUMMARY_BYTES {
            break;
        }
        kept.push(c);
    }
    kept
}

/// What the operator calls a ticket in this state.
#[must_use]
pub const fn state_name(status: CardStatus) -> &'static str {
    match status {
        CardStatus::Todo | CardStatus::Claimed => "To do",
        CardStatus::Doing => "Working",
        CardStatus::Review => "Ready for you",
        CardStatus::Done => "Done",
        CardStatus::Blocked => "Needs you",
    }
}

/// A bidirectional override, embedding or isolate: text that can make what is shown read in a
/// different order from the characters that were sent.
#[must_use]
pub const fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Whether `text`, already trimmed, is a valid ticket text: within `max` bytes, with no NUL, no
/// bidirectional control and no control character other than newline and tab.
#[must_use]
pub fn clean_text(text: &str, max: usize) -> bool {
    text.len() <= max
        && !text
            .chars()
            .any(|c| (c.is_control() && c != '\n' && c != '\t') || is_bidi_control(c))
}

/// Whether `text`, already trimmed, is a valid title: as [`clean_text`], on one line.
#[must_use]
pub fn clean_title(text: &str, max: usize) -> bool {
    text.len() <= max && !text.chars().any(|c| c.is_control() || is_bidi_control(c))
}

/// Trim leading and trailing ASCII space, tab, CR and LF, as every ticket text is trimmed.
#[must_use]
pub fn trim(text: &str) -> &str {
    text.trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_sentences_are_the_fixed_plain_sentences() {
        assert_eq!(
            TicketRefusal::WrongState.sentence(CardStatus::Doing),
            "You can't do that to a ticket that is Working."
        );
        assert_eq!(
            TicketRefusal::TicketCard.sentence(CardStatus::Todo),
            "That card is a ticket. Use the ticket commands."
        );
        assert_eq!(TicketRefusal::QueueFull.code(), "queue_full");
    }

    /// Every Needs-you reason has a sentence that says the next step and where to take it, and the
    /// reasons keep their wire names.
    #[test]
    fn every_needs_you_reason_has_its_own_sentence() {
        let reasons = [
            (TicketReason::ProjectUnavailable, "project_unavailable"),
            (TicketReason::CheckCannotRun, "check_cannot_run"),
            (TicketReason::CeilingRequired, "ceiling_required"),
            (TicketReason::ReserveMissing, "reserve_missing"),
            (TicketReason::GoalChanged, "goal_changed"),
            (TicketReason::LimitGoal, "limit_goal"),
            (TicketReason::Slowed, "slowed"),
            (TicketReason::NoTurns, "no_turns"),
            (TicketReason::HeldLaunch, "held_launch"),
            (TicketReason::HeldFailing, "held_failing"),
            (TicketReason::HeldUnsure, "held_unsure"),
            (TicketReason::WorktreeMissing, "worktree_missing"),
            (
                TicketReason::SharedInstructionsChanged,
                "shared_instructions_changed",
            ),
            (
                TicketReason::SharedInstructionsUnknown,
                "shared_instructions_unknown",
            ),
        ];
        let mut sentences = std::collections::BTreeSet::new();
        for (reason, name) in reasons {
            assert_eq!(serde_json::to_value(reason).unwrap(), name);
            assert!(reason.sentence().ends_with('.'), "{name}");
            assert!(
                sentences.insert(reason.sentence()),
                "{name} repeats a sentence"
            );
        }
        assert_eq!(
            TicketReason::NoTurns.sentence(),
            "The agent used all its turns on this ticket. Give to agent to try again, or Stop."
        );
    }

    #[test]
    fn a_summary_loses_control_bytes_and_is_cut_on_a_character() {
        assert_eq!(summary_text("a\u{7}b\nc\td\u{1b}[2J"), "ab\nc\td[2J");
        assert_eq!(
            summary_text("safe\u{202E}gnp.exe\u{2066}x\u{2069}"),
            "safegnp.exex"
        );
        let long = "é".repeat(3_000);
        let cut = summary_text(&long);
        assert_eq!(cut.len(), MAX_SUMMARY_BYTES);
        assert!(cut.chars().all(|c| c == 'é'));
        assert_eq!(summary_text(&"x".repeat(4_097)).len(), 4_096);
    }

    #[test]
    fn text_rules() {
        assert_eq!(trim(" \t\r\n a b \n"), "a b");
        assert!(clean_text("line\none\ttab", 64));
        assert!(!clean_text("nul\0", 64));
        assert!(!clean_text("bell\u{7}", 64));
        assert!(!clean_text("12345", 4));
        assert!(!clean_text("del\u{7f}", 64));
        assert!(!clean_text("c1\u{9b}", 64));
        assert!(clean_title("one line", 64));
        assert!(!clean_title("two\nlines", 64));
        assert!(!clean_title("tab\there", 64));
        assert!(!clean_title("nul\0", 64));
        assert!(!clean_title("mirror \u{202E}gnp.exe", 64));
        assert!(!clean_text("isolate \u{2066}x", 64));
    }

    #[test]
    fn rework_sorts_before_new_whatever_the_position() {
        let early = QueueKey {
            class: QueueClass::New,
            lsn: Lsn(1),
        };
        let late = QueueKey {
            class: QueueClass::Rework,
            lsn: Lsn(9),
        };
        assert!(late < early);
    }
}
