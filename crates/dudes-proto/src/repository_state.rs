//! One pure interpretation of hire, ticket-start and issue repository authority.

use dudes_types::{AgentId, CardId, GoalId, Lsn, Mode, ModelSelection, SessionId, TurnId};

use crate::repository::{
    HireRepositories, SingletonRepository, TicketRepositories, TicketRepositoryRound,
    TurnRepositories,
};
use crate::{Goal, Record};

/// A borrowed hire carrier. The source remains available for its ordinary lifecycle fields.
#[derive(Clone, Copy, Debug)]
pub struct HireRecord<'a> {
    /// Original carrier, without synthetic lowering.
    pub source: &'a Record,
    /// Hired agent.
    pub agent: AgentId,
    /// Recorded root.
    pub cwd: &'a std::path::Path,
    /// Optional recorded enrollment.
    pub workspace: Option<&'a crate::record::RecordedWorkspace>,
    /// Optional recorded own-copy lease.
    pub worktree: Option<dudes_types::WorktreeId>,
}

/// The atomic goal installation and card movement of a ticket start.
#[derive(Clone, Copy, Debug)]
pub struct TicketStart<'a> {
    /// Original carrier.
    pub source: &'a Record,
    /// Ticket.
    pub card: CardId,
    /// Holder.
    pub agent: AgentId,
    /// Atomically installed goal.
    pub goal: &'a Goal,
    /// Card revision named by the start.
    pub expected_seq: dudes_types::Seq,
}

/// Common issue fields borrowed from the original source-specific carrier.
#[derive(Clone, Copy, Debug)]
pub struct TurnIssue<'a> {
    /// Original carrier, preserving browser reservations and mail accounting.
    pub source: &'a Record,
    /// Owner.
    pub agent: AgentId,
    /// Immutable turn identity.
    pub turn: TurnId,
    /// Goal, absent for finite conversation turns.
    pub goal: Option<GoalId>,
    /// Recorded launch mode.
    pub mode: Mode,
    /// Exact task payload.
    pub task: &'a str,
    /// Recorded announced session.
    pub session: Option<&'a SessionId>,
    /// Exact model selection, absent only in older ordinary issues.
    pub model_selection: Option<&'a ModelSelection>,
    /// Recorded delivered answers.
    pub answers: &'a [Lsn],
}

impl Record {
    /// Borrow the hire without manufacturing another record carrier.
    #[must_use]
    pub fn hire(&self) -> Option<HireRecord<'_>> {
        match self {
            Self::AgentHired {
                id,
                cwd,
                workspace,
                worktree,
                ..
            } => Some(HireRecord {
                source: self,
                agent: *id,
                cwd,
                workspace: workspace.as_ref(),
                worktree: *worktree,
            }),
            _ => None,
        }
    }

    /// Borrow the ticket start's complete atomic lifecycle carrier.
    #[must_use]
    pub const fn ticket_start(&self) -> Option<TicketStart<'_>> {
        match self {
            Self::TicketStarted {
                card,
                agent,
                goal,
                expected_seq,
            } => Some(TicketStart {
                source: self,
                card: *card,
                agent: *agent,
                goal,
                expected_seq: *expected_seq,
            }),
            _ => None,
        }
    }

    /// Borrow common fields from every managed issue source, keeping its original payload.
    #[must_use]
    #[expect(
        clippy::too_many_lines,
        reason = "one borrowed view keeps every issue carrier and its original payload together"
    )]
    pub fn turn_issue(&self) -> Option<TurnIssue<'_>> {
        let (agent, turn, goal, mode, task, session, model_selection, answers) = match self {
            Self::TurnIssued {
                agent,
                turn,
                goal,
                mode,
                task,
                session,
                model_selection,
                answers,
                ..
            } => (
                *agent,
                *turn,
                Some(*goal),
                *mode,
                task.as_str(),
                session.as_ref(),
                model_selection.as_ref(),
                answers.as_slice(),
            ),
            Self::NoteOnlyTurnIssued {
                agent,
                turn,
                goal,
                mode,
                task,
                session,
                model_selection,
                answers,
                ..
            }
            | Self::TaughtNoteOnlyTurnIssued {
                agent,
                turn,
                goal,
                mode,
                task,
                session,
                model_selection,
                answers,
                ..
            }
            | Self::IdentifiedTaughtNoteOnlyTurnIssued {
                agent,
                turn,
                goal,
                mode,
                task,
                session,
                model_selection,
                answers,
                ..
            }
            | Self::TurnIssuedWithBrowserNote {
                agent,
                turn,
                goal,
                mode,
                task,
                session,
                model_selection,
                answers,
                ..
            } => (
                *agent,
                *turn,
                Some(*goal),
                *mode,
                task.as_str(),
                session.as_ref(),
                Some(model_selection),
                answers.as_slice(),
            ),
            Self::ConversationTurnIssued {
                agent,
                turn,
                task,
                session,
                model_selection,
                ..
            }
            | Self::IdentifiedConversationTurnIssued {
                agent,
                turn,
                task,
                session,
                model_selection,
                ..
            } => (
                *agent,
                *turn,
                None,
                Mode::Managed,
                task.as_str(),
                session.as_ref(),
                Some(model_selection),
                &[][..],
            ),
            Self::MailWakeOnlyTurnIssued(issue) | Self::IdentifiedMailWakeOnlyTurnIssued(issue) => {
                (
                    issue.agent,
                    issue.turn,
                    Some(issue.goal),
                    issue.mode,
                    issue.task.as_str(),
                    issue.session.as_ref(),
                    Some(&issue.model_selection),
                    &[][..],
                )
            }
            _ => return None,
        };
        Some(TurnIssue {
            source: self,
            agent,
            turn,
            goal,
            mode,
            task,
            session,
            model_selection,
            answers,
        })
    }
}

/// Affected row components at the preceding journal position.
#[derive(Clone, Copy, Debug, Default)]
pub struct RepositoryFacts<'a> {
    /// Hire selected for the issue.
    pub hire: Option<(Lsn, &'a HireRepositories)>,
    /// Declaration and preceding round counter selected by a start.
    pub ticket: Option<(&'a TicketRepositories, u32)>,
    /// Historical counter when no creation was recorded.
    pub prior_round: u32,
    /// Ticket round selected by the issue.
    pub round: Option<&'a TicketRepositoryRound>,
}

/// Normalized authority replacements for existing row owners.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepositoryFold {
    /// No repository-bearing carrier.
    Unchanged,
    /// Authority established by a hire.
    Hire(HireRepositories),
    /// Declaration established by ticket creation.
    Ticket(TicketRepositories),
    /// Immutable round established by a ticket start.
    Start(TicketRepositoryRound),
    /// Issue authority, unknown when no preceding hire exists.
    Issue(Option<TurnRepositories>),
}

/// Interpret recorded repository authority without configuration or filesystem observations.
#[must_use]
pub fn fold(lsn: Lsn, record: &Record, before: RepositoryFacts<'_>) -> RepositoryFold {
    if let Some(hire) = record.hire() {
        return RepositoryFold::Hire(HireRepositories::Legacy(SingletonRepository::recorded(
            hire.cwd,
            hire.workspace,
            hire.worktree,
        )));
    }
    if let Record::TicketCreated { project, .. } = record {
        return RepositoryFold::Ticket(TicketRepositories::recorded(project));
    }
    if let Some(start) = record.ticket_start() {
        return RepositoryFold::Start(TicketRepositoryRound {
            card: start.card,
            goal: start.goal.id,
            start_lsn: lsn,
            round: before
                .ticket
                .map_or(before.prior_round, |(_, round)| round)
                .saturating_add(1),
            repositories: before.ticket.map(|(repositories, _)| repositories.clone()),
        });
    }
    if record.turn_issue().is_some() {
        return RepositoryFold::Issue(
            before
                .hire
                .map(|(lsn, hire)| TurnRepositories::recorded(lsn, hire, before.round)),
        );
    }
    RepositoryFold::Unchanged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RecordKind;
    use crate::record::samples;

    #[test]
    fn a_singleton_hire_keeps_unknown_enrollment_unknown() {
        let mut record = samples::record(RecordKind::AgentHired);
        let Record::AgentHired {
            cwd,
            workspace,
            worktree,
            ..
        } = &mut record
        else {
            unreachable!();
        };
        *cwd = "/recorded/copy".into();
        *workspace = None;
        *worktree = Some(dudes_types::WorktreeId::from_bytes([3; 16]));
        let view = record.hire().unwrap();
        assert!(std::ptr::eq(view.source, &raw const record));
        let RepositoryFold::Hire(hire) = fold(Lsn(3), &record, RepositoryFacts::default()) else {
            panic!("hire authority");
        };
        let repository = hire.singleton().unwrap();
        assert_eq!(repository.cwd(), std::path::Path::new("/recorded/copy"));
        assert_eq!(repository.workspace(), None);
        assert_eq!(
            repository.worktree(),
            Some(dudes_types::WorktreeId::from_bytes([3; 16]))
        );
    }

    #[test]
    fn every_issue_view_preserves_its_original_source_and_payload() {
        let kinds = [
            RecordKind::TurnIssued,
            RecordKind::NoteOnlyTurnIssued,
            RecordKind::TaughtNoteOnlyTurnIssued,
            RecordKind::IdentifiedTaughtNoteOnlyTurnIssued,
            RecordKind::TurnIssuedWithBrowserNote,
            RecordKind::ConversationTurnIssued,
            RecordKind::IdentifiedConversationTurnIssued,
            RecordKind::MailWakeOnlyTurnIssued,
            RecordKind::IdentifiedMailWakeOnlyTurnIssued,
        ];
        for kind in kinds {
            let record = samples::record(kind);
            let issue = record.turn_issue().unwrap();
            assert!(std::ptr::eq(issue.source, &raw const record));
            assert_eq!(issue.source.kind(), kind);
            let serialized = serde_json::to_value(&record).unwrap();
            assert_eq!(serialized["task"].as_str(), Some(issue.task));
            assert_eq!(
                serialized["turn"],
                serde_json::to_value(issue.turn).unwrap()
            );
            assert_eq!(
                serialized["agent"],
                serde_json::to_value(issue.agent).unwrap()
            );
            assert_eq!(
                fold(Lsn(9), &record, RepositoryFacts::default()),
                RepositoryFold::Issue(None)
            );
        }
    }

    #[test]
    fn singleton_ticket_authority_keeps_its_existing_json_and_schema() {
        let project: TicketRepositories = "service".into();
        assert_eq!(
            serde_json::to_value(&project).unwrap(),
            serde_json::json!("service")
        );
        assert_eq!(
            serde_json::from_str::<TicketRepositories>("\"service\"").unwrap(),
            project
        );
        assert_eq!(
            schemars::schema_for!(TicketRepositories).get("type"),
            Some(&serde_json::json!("string"))
        );
    }
}
