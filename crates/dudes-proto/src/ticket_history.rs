//! What happened to a ticket and when, as the board keeps it: a bounded list of closed events. An
//! event copies no title, note, message, summary, path or question text, so nothing in it needs
//! scrubbing.

use dudes_types::{AgentId, Timestamp};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::record::Actor;
use crate::ticket::{Priority, TicketReason};

/// Most events one ticket keeps. A longer history loses its oldest events and counts them.
pub const MAX_TICKET_HISTORY: usize = 48;

/// Who caused an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HistoryBy {
    /// The operator, from the command line or the Floor.
    Operator,
    /// The daemon itself.
    Daemon,
    /// An agent.
    Agent,
}

impl From<Actor> for HistoryBy {
    fn from(actor: Actor) -> Self {
        match actor {
            Actor::Peer { .. } | Actor::BrowserSession { .. } => Self::Operator,
            Actor::System => Self::Daemon,
            Actor::Agent { .. } => Self::Agent,
        }
    }
}

/// What happened. Each member is a closed token, an agent id, a round, a priority or an exit
/// code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TicketHistoryKind {
    /// The ticket was made.
    Created,
    /// Its title, note or check changed.
    Edited,
    /// It joined an agent's queue.
    Assigned {
        /// The agent.
        agent: AgentId,
    },
    /// Finished or stopped work went back to an agent.
    SentBack {
        /// The agent.
        agent: AgentId,
    },
    /// Work started.
    Started {
        /// The agent.
        agent: AgentId,
        /// Which start this was, counting from 1.
        round: u32,
    },
    /// The daemon moved it into Needs you.
    Moved {
        /// Why.
        reason: TicketReason,
    },
    /// The check passed.
    CheckPassed {
        /// The check's exit code, when it ran to one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit: Option<i32>,
    },
    /// The check failed.
    CheckFailed {
        /// The check's exit code, when it ran to one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit: Option<i32>,
    },
    /// The priority changed.
    PrioritySet {
        /// The new priority.
        priority: Priority,
    },
    /// The operator accepted the work.
    Accepted,
    /// Its agent was retired, so it went back to To do.
    Returned {
        /// The retired agent.
        agent: AgentId,
    },
    /// Its agent asked the operator a question while working on it.
    Asked {
        /// The asking agent.
        agent: AgentId,
        /// The question's identity: the canonical decimal position of the question record.
        question_lsn: String,
    },
    /// A question it was asked got an answer. The answer is not kept here.
    Answered {
        /// The agent that asked.
        agent: AgentId,
        /// The question's identity, as on the matching `asked` event.
        question_lsn: String,
    },
}

impl TicketHistoryKind {
    const fn is_check(&self) -> bool {
        matches!(self, Self::CheckPassed { .. } | Self::CheckFailed { .. })
    }
}

/// One event. Its position is decimal text to preserve every `u64` in browsers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketHistoryEvent {
    /// Canonical decimal journal position of the record that caused it.
    pub lsn: String,
    /// The journal entry's recorded time.
    pub at: Timestamp,
    /// Who caused it.
    pub by: HistoryBy,
    /// What happened.
    #[serde(flatten)]
    pub kind: TicketHistoryKind,
}

/// A ticket's newest events, oldest first, and how many it has had in all.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TicketHistory {
    /// Every event the ticket has had, kept or not.
    pub total: u32,
    /// The newest [`MAX_TICKET_HISTORY`] events at most.
    pub events: Vec<TicketHistoryEvent>,
}

impl TicketHistory {
    /// Add an event. A check result replaces an immediately preceding one and leaves `total`
    /// alone, so repeated runs of one check do not push everything else out.
    pub fn push(&mut self, event: TicketHistoryEvent) {
        if event.kind.is_check()
            && let Some(last) = self.events.last_mut()
            && last.kind.is_check()
        {
            *last = event;
            return;
        }
        self.total = self.total.saturating_add(1);
        self.events.push(event);
        if self.events.len() > MAX_TICKET_HISTORY {
            self.events.remove(0);
        }
    }

    /// How many events the ticket has had that are not in [`Self::events`].
    #[must_use]
    pub fn omitted(&self) -> u32 {
        self.total
            .saturating_sub(u32::try_from(self.events.len()).unwrap_or(u32::MAX))
    }

    /// Whether the ticket has had no event.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.total == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(lsn: u64, kind: TicketHistoryKind) -> TicketHistoryEvent {
        TicketHistoryEvent {
            lsn: lsn.to_string(),
            at: Timestamp::from_unix_millis(1_000),
            by: HistoryBy::Operator,
            kind,
        }
    }

    #[test]
    fn a_check_result_replaces_the_one_before_it_and_counts_once() {
        let mut history = TicketHistory::default();
        history.push(event(1, TicketHistoryKind::Created));
        history.push(event(2, TicketHistoryKind::CheckFailed { exit: Some(1) }));
        history.push(event(3, TicketHistoryKind::CheckPassed { exit: Some(0) }));
        assert_eq!(history.total, 2);
        assert_eq!(
            history.events,
            [
                event(1, TicketHistoryKind::Created),
                event(3, TicketHistoryKind::CheckPassed { exit: Some(0) }),
            ]
        );
        history.push(event(4, TicketHistoryKind::Accepted));
        history.push(event(5, TicketHistoryKind::CheckFailed { exit: None }));
        assert_eq!(history.total, 4);
    }

    #[test]
    fn the_oldest_events_drop_past_the_cap_and_are_counted() {
        let mut history = TicketHistory::default();
        for lsn in 1..=50 {
            history.push(event(lsn, TicketHistoryKind::Edited));
        }
        assert_eq!(history.total, 50);
        assert_eq!(history.events.len(), 48);
        assert_eq!(history.events[0].lsn, "3");
        assert_eq!(history.omitted(), 2);
    }

    #[test]
    fn an_event_serializes_flat() {
        let json = serde_json::to_value(event(
            7,
            TicketHistoryKind::PrioritySet {
                priority: Priority::Urgent,
            },
        ))
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "lsn": "7", "at": json["at"], "by": "operator",
                "kind": "priority_set", "priority": "urgent"
            })
        );
    }
}
