//! `AgentMoved`: folding it, and checking it against the history before it.

use dudes_proto::placement::{AgentMove, Here};
use dudes_proto::{Entry, Record};
use dudes_types::{AgentId, Lsn};

use super::{AgentRow, World};
use crate::world::repositories::Ticket;

/// A recorded move contradicts the history before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveHistoryError {
    /// The agent was never hired, or is retired.
    UnknownAgent,
    /// The agent holds several repositories, so it has no single place to leave.
    MultiRepository,
    /// The agent was hired before projects were recorded, so no project is its own.
    NoProject,
    /// The agent is already in the target project.
    SameProject,
    /// The agent has a turn in flight or a human holding its keyboard.
    Busy,
    /// The agent has a goal it has not finished, or a ticket in work or in review.
    Unfinished,
    /// The agent's last chat resume lost its context and the reset is not yet acknowledged.
    ContextLoss,
    /// The agent holds answers that have not yet been delivered to it.
    UndeliveredAnswers,
    /// The record names an own copy that is not one the agent already holds and may return to.
    ForeignCopy,
    /// The agent holds a copy in the target project that this move does not return to.
    AbandonedCopy,
}

impl std::fmt::Display for MoveHistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UnknownAgent => "the move names an agent that is not hired or is retired",
            Self::MultiRepository => "the move names an agent that holds several repositories",
            Self::NoProject => "the move names an agent that was hired outside any project",
            Self::SameProject => "the move names the project the agent is already in",
            Self::Busy => "the move names an agent with an open turn or a human attached",
            Self::Unfinished => "the move names an agent with an unfinished goal or ticket",
            Self::ContextLoss => "the move names an agent with an unacknowledged context reset",
            Self::UndeliveredAnswers => "the move names an agent with answers not yet delivered",
            Self::ForeignCopy => "the move names an own copy the agent does not hold",
            Self::AbandonedCopy => "the move leaves behind the agent's own copy in that project",
        })
    }
}

impl std::error::Error for MoveHistoryError {}

impl World {
    /// Check a prospective move against this exact recorded prefix. Replay runs it before folding
    /// the record; it changes nothing.
    ///
    /// # Errors
    /// Returns the first contradiction of the recorded history.
    pub fn validate_move_record(&self, entry: &Entry) -> Result<(), MoveHistoryError> {
        let Record::AgentMoved { agent, to } = &entry.record else {
            return Ok(());
        };
        let row = self
            .agents
            .get(agent)
            .filter(|row| !row.retired)
            .ok_or(MoveHistoryError::UnknownAgent)?;
        let from = row
            .repositories
            .singleton()
            .ok_or(MoveHistoryError::MultiRepository)?
            .workspace()
            .ok_or(MoveHistoryError::NoProject)?;
        if from.id == to.workspace.id {
            return Err(MoveHistoryError::SameProject);
        }
        if row.open_turn.is_some() || row.attached {
            return Err(MoveHistoryError::Busy);
        }
        if self.holds_unfinished_work(*agent, row) {
            return Err(MoveHistoryError::Unfinished);
        }
        if self.conversation_context_loss(*agent).is_some() {
            return Err(MoveHistoryError::ContextLoss);
        }
        if !row.pending_answers.is_empty() {
            return Err(MoveHistoryError::UndeliveredAnswers);
        }
        match (to.worktree, row.other_projects.get(&to.workspace.id)) {
            (Some(copy), Some(stored))
                if to.is_place_of(stored) && self.owns_live_copy(*agent, copy) =>
            {
                Ok(())
            }
            (Some(_), _) => Err(MoveHistoryError::ForeignCopy),
            (None, Some(stored)) if stored.repository.worktree().is_some() => {
                Err(MoveHistoryError::AbandonedCopy)
            }
            (None, _) => Ok(()),
        }
    }

    fn owns_live_copy(&self, agent: AgentId, copy: dudes_types::WorktreeId) -> bool {
        self.worktrees
            .get(&copy)
            .is_some_and(|lease| lease.agent == Some(agent) && lease.removed.is_none())
    }

    fn holds_unfinished_work(&self, agent: AgentId, row: &AgentRow) -> bool {
        let goal_open = row.goal.is_some_and(|goal| {
            self.goals.get(&goal).is_some_and(|row| !row.cleared)
                && !self.completed_goals.contains_key(&goal)
        });
        goal_open
            || self
                .repository_tickets
                .values()
                .filter_map(Ticket::created)
                .any(|ticket| ticket.is_in_hand_of(agent))
    }

    /// Fold a move. `hire_lsn` becomes the move's position, so everything bound to a hire
    /// position treats the new place as a new hire.
    pub(super) fn fold_agent_move(&mut self, lsn: Lsn, agent: AgentId, to: &AgentMove) {
        let Some(row) = self.agents.get_mut(&agent) else {
            return;
        };
        let landed = to.land(Here {
            repositories: &mut row.repositories,
            trusted: &mut row.trusted,
            session: &mut row.session,
            session_lsn: &mut row.session_lsn,
            other_projects: &mut row.other_projects,
        });
        if landed {
            row.budget = to.budget;
            row.mcp.hire_lsn = lsn;
        }
    }
}

#[cfg(test)]
mod tests {
    use dudes_proto::placement::AgentMove;
    use dudes_proto::record::{RecordedWorkspace, samples};
    use dudes_proto::{AgentBudget, Entry, Record, RecordKind, TurnOutcome};
    use dudes_types::{Lsn, Mode, SessionId, Timestamp, TurnId};

    use super::{MoveHistoryError, World};

    fn apply(world: &mut World, at: u64, record: Record) {
        let entry = Entry {
            at: Timestamp::from_unix_millis(0),
            record,
        };
        world.apply_checked(Lsn(at), &entry).unwrap();
    }

    #[test]
    fn a_finite_resume_that_lost_its_context_keeps_its_reset_in_the_project_it_belongs_to() {
        let Record::AgentHired { id: agent, .. } = samples::record(RecordKind::AgentHired) else {
            unreachable!()
        };
        let mut hire = samples::record(RecordKind::AgentHired);
        let Record::AgentHired {
            workspace,
            worktree,
            mcp_servers,
            ..
        } = &mut hire
        else {
            unreachable!()
        };
        *workspace = Some(RecordedWorkspace {
            id: "service".into(),
            label: "service".into(),
        });
        *worktree = None;
        mcp_servers.clear();
        let session = SessionId::new("service-session");
        let turn = TurnId::from_bytes([5; 16]);
        let mut issue = samples::record(RecordKind::TurnIssued);
        let Record::TurnIssued {
            turn: issued,
            session: announced,
            ..
        } = &mut issue
        else {
            unreachable!()
        };
        (*issued, *announced) = (turn, Some(session.clone()));
        let mut end = samples::record(RecordKind::TurnEnded);
        let Record::TurnEnded {
            turn: ended,
            mode,
            outcome,
            session: ran_on,
            ..
        } = &mut end
        else {
            unreachable!()
        };
        (*ended, *mode, *outcome, *ran_on) =
            (turn, Mode::Managed, Some(TurnOutcome::NoResult), None);

        let mut world = World::default();
        apply(&mut world, 100, hire);
        apply(
            &mut world,
            110,
            Record::SessionLearned {
                agent,
                turn: TurnId::from_bytes([4; 16]),
                session,
            },
        );
        apply(&mut world, 120, issue);
        world.test_browser_issue(turn, agent);
        apply(&mut world, 130, end);
        assert!(world.conversation_context_loss(agent).is_some());

        let entry = Entry {
            at: Timestamp::from_unix_millis(0),
            record: Record::AgentMoved {
                agent,
                to: AgentMove {
                    cwd: "/repo/billing".into(),
                    workspace: RecordedWorkspace {
                        id: "billing".into(),
                        label: "billing".into(),
                    },
                    worktree: None,
                    budget: AgentBudget {
                        usd_per_day: None,
                        tokens_per_day: None,
                    },
                },
            },
        };
        assert_eq!(
            world.validate_move_record(&entry),
            Err(MoveHistoryError::ContextLoss)
        );
    }
}
