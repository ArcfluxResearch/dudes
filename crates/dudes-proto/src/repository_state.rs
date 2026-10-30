//! One pure interpretation of hire, ticket-start and issue repository authority.

pub mod history;

use dudes_types::{AgentId, CardId, GoalId, Lsn, Mode, ModelSelection, SessionId, TurnId};

use crate::repository::{
    HireRepositories, TicketRepositories, TicketRepositoryRound, TurnRepositories,
};
use crate::{Goal, Record};

/// A borrowed hire carrier. The source remains available for its ordinary lifecycle fields.
#[derive(Clone, Copy, Debug)]
pub struct HireRecord<'a> {
    /// Original carrier, without synthetic lowering.
    pub source: &'a Record,
    /// Hired agent.
    pub agent: AgentId,
    /// Ordinary hire fields, borrowed from either carrier.
    pub fields: HireFields<'a>,
    /// Recorded repository authority, without current enrollment lookup.
    pub repositories: HireRepositoryRecord<'a>,
}

/// Ordinary agent lifecycle fields shared by historical and scoped hires.
#[derive(Clone, Copy, Debug)]
pub struct HireFields<'a> {
    /// Display name.
    pub name: &'a str,
    /// Display role.
    pub role: &'a str,
    /// Floor department.
    pub department: &'a str,
    /// Selected engine.
    pub provider: dudes_types::Provider,
    /// Authored appearance.
    pub character: Option<dudes_types::CastCharacter>,
    /// Selected accent.
    pub accent: Option<dudes_types::AccentColour>,
    /// Daily admission ceilings.
    pub budget: crate::AgentBudget,
    /// Historical sandbox selection.
    pub sandbox: Option<bool>,
    /// Historical isolation selection.
    pub isolation: Option<dudes_types::IsolationMode>,
    /// Recorded provider turn limit.
    pub max_turns: Option<u32>,
    /// Recorded provider budget backstop.
    pub max_budget: Option<dudes_types::Usd>,
    /// Recorded model override.
    pub model: Option<&'a dudes_types::ModelName>,
    /// Historical singleton trust. Scoped trust is held per repository.
    pub trusted: bool,
    /// Historical MCP selection. Scoped policy admits no inherited servers.
    pub mcp_servers: &'a [dudes_types::McpServerName],
    /// Whether this complete hire records browser onboarding.
    pub browser_onboarding: bool,
}

/// The original authority carried by a hire.
#[derive(Clone, Copy, Debug)]
pub enum HireRepositoryRecord<'a> {
    /// Historical singleton facts.
    Legacy {
        /// Recorded execution root.
        cwd: &'a std::path::Path,
        /// Optional recorded enrollment.
        workspace: Option<&'a crate::record::RecordedWorkspace>,
        /// Optional owned-copy lease.
        worktree: Option<dudes_types::WorktreeId>,
    },
    /// A required carrier's complete restricted set.
    Scoped(&'a crate::repository::ScopedRepositories),
}

impl HireRepositoryRecord<'_> {
    /// Every recorded lease, without constructing normalized execution authority.
    pub fn worktrees(&self) -> impl Iterator<Item = dudes_types::WorktreeId> + '_ {
        let (legacy, scoped) = match self {
            Self::Legacy { worktree, .. } => (*worktree, None),
            Self::Scoped(scope) => (None, Some(*scope)),
        };
        legacy
            .into_iter()
            .chain(scoped.into_iter().flat_map(|scope| {
                scope
                    .repositories
                    .entries()
                    .iter()
                    .map(|entry| entry.value.worktree)
            }))
    }
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
    /// Exact round and restricted selection, present only on the required scoped carrier.
    pub repositories: Option<(u32, &'a crate::repository::TicketStartRepositories)>,
}

/// Common creation fields, borrowed from the original carrier.
#[derive(Clone, Copy, Debug)]
pub struct TicketCreated<'a> {
    /// Original carrier.
    pub source: &'a Record,
    /// Ticket identity.
    pub card: CardId,
    /// Trimmed title.
    pub title: &'a str,
    /// Operator note.
    pub description: &'a str,
    /// Project department.
    pub department: &'a str,
    /// Recorded project identity.
    pub project: &'a str,
    /// Resolved check definition.
    pub done_when: &'a crate::TicketCheck,
    /// Author.
    pub by: crate::Actor,
    /// Browser retry attribution.
    pub request: Option<&'a crate::record::TicketRequestAttribution>,
    /// The priority chosen at creation. A scoped carrier names none.
    pub priority: Option<crate::ticket::Priority>,
    /// Explicit immutable declaration and check target.
    pub repositories: Option<(
        &'a crate::repository::TicketDeclaration,
        &'a crate::repository::RepoId,
    )>,
}

/// Common complete edit fields, without authority to replace the declaration.
#[derive(Clone, Copy, Debug)]
pub struct TicketEdited<'a> {
    /// Ticket identity.
    pub card: CardId,
    /// Trimmed title.
    pub title: &'a str,
    /// Operator note.
    pub description: &'a str,
    /// Resolved replacement check.
    pub done_when: &'a crate::TicketCheck,
    /// Author.
    pub by: crate::Actor,
    /// Preceding card revision.
    pub expected_seq: dudes_types::Seq,
    /// Explicit target on the scoped edit carrier.
    pub check_repo: Option<&'a crate::repository::RepoId>,
    /// The priority after the edit. A scoped carrier names none, and so keeps the current one.
    pub priority: Option<crate::ticket::Priority>,
}

/// Common issue fields borrowed from the original source-specific carrier.
#[derive(Clone, Copy, Debug)]
pub struct TurnIssue<'a> {
    /// Original carrier, preserving browser reservations and mail accounting.
    pub source: &'a Record,
    /// Complete semantic payload, independent of the physical carrier.
    pub payload: crate::issue::ManagedIssueRef<'a>,
    /// Explicit frozen authority on a scoped issue.
    pub repositories: Option<&'a crate::repository::RepositoryTurnGrant>,
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

impl TurnIssue<'_> {
    /// The instruction round's goal, independent of finite-turn budget charging.
    #[must_use]
    pub fn instruction_goal(&self) -> Option<GoalId> {
        match self.repositories {
            Some(grant) => grant.ticket.as_ref().map(|round| round.goal),
            None => self.goal,
        }
    }

    /// Whether an explicit recovery-new claim can bind this managed issue family.
    #[must_use]
    pub fn recovery_new(&self) -> bool {
        self.mode == Mode::Managed
            && matches!(
                self.payload,
                crate::issue::ManagedIssueRef::Ordinary { .. }
                    | crate::issue::ManagedIssueRef::GoalNote { .. }
            )
    }
}

impl Record {
    /// Borrow every ticket creation without lowering its physical carrier.
    #[must_use]
    pub fn ticket_created(&self) -> Option<TicketCreated<'_>> {
        match self {
            Self::TicketCreated {
                card,
                title,
                description,
                department,
                project,
                done_when,
                by,
                request,
                priority,
            } => Some(TicketCreated {
                source: self,
                card: *card,
                title,
                description,
                department,
                project,
                done_when,
                by: *by,
                request: request.as_ref(),
                priority: *priority,
                repositories: None,
            }),
            Self::TicketCreatedWithRepositories(created) => Some(TicketCreated {
                source: self,
                card: created.card,
                title: &created.title,
                description: &created.description,
                department: &created.department,
                project: &created.declaration.project,
                done_when: &created.done_when,
                by: created.by,
                request: created.request.as_ref(),
                priority: None,
                repositories: Some((&created.declaration, &created.check_repo)),
            }),
            _ => None,
        }
    }

    /// Borrow every complete ticket edit.
    #[must_use]
    pub fn ticket_edited(&self) -> Option<TicketEdited<'_>> {
        match self {
            Self::TicketEdited {
                card,
                title,
                description,
                done_when,
                by,
                expected_seq,
                priority,
            } => Some(TicketEdited {
                card: *card,
                title,
                description,
                done_when,
                by: *by,
                expected_seq: *expected_seq,
                check_repo: None,
                priority: *priority,
            }),
            Self::TicketEditedWithRepositories(edited) => Some(TicketEdited {
                card: edited.card,
                title: &edited.title,
                description: &edited.description,
                done_when: &edited.done_when,
                by: edited.by,
                expected_seq: edited.expected_seq,
                check_repo: Some(&edited.check_repo),
                priority: None,
            }),
            _ => None,
        }
    }

    /// Borrow the hire without manufacturing another record carrier.
    #[must_use]
    pub fn hire(&self) -> Option<HireRecord<'_>> {
        match self {
            Self::AgentHired {
                id,
                name,
                role,
                department,
                provider,
                character,
                accent,
                budget,
                sandbox,
                isolation,
                max_turns,
                max_budget,
                model,
                trusted,
                mcp_servers,
                browser_onboarding,
                cwd,
                workspace,
                worktree,
            } => Some(HireRecord {
                source: self,
                agent: *id,
                fields: HireFields {
                    name,
                    role,
                    department,
                    provider: *provider,
                    character: *character,
                    accent: *accent,
                    budget: *budget,
                    sandbox: *sandbox,
                    isolation: *isolation,
                    max_turns: *max_turns,
                    max_budget: *max_budget,
                    model: model.as_ref(),
                    trusted: *trusted,
                    mcp_servers,
                    browser_onboarding: browser_onboarding.is_some(),
                },
                repositories: HireRepositoryRecord::Legacy {
                    cwd,
                    workspace: workspace.as_ref(),
                    worktree: *worktree,
                },
            }),
            Self::AgentHiredWithRepositories(hire)
            | Self::AgentHiredWithUnconfinedRepositories(hire) => Some(HireRecord {
                source: self,
                agent: hire.id,
                fields: HireFields {
                    name: &hire.name,
                    role: &hire.role,
                    department: &hire.department,
                    provider: hire.provider,
                    character: hire.character,
                    accent: hire.accent,
                    budget: hire.budget,
                    max_turns: hire.max_turns,
                    max_budget: hire.max_budget,
                    model: hire.model.as_ref(),
                    sandbox: None,
                    isolation: None,
                    trusted: false,
                    mcp_servers: &[],
                    browser_onboarding: hire.browser_onboarding.is_some(),
                },
                repositories: HireRepositoryRecord::Scoped(&hire.repositories),
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
                repositories: None,
            }),
            Self::TicketStartedWithRepositories(start) => Some(TicketStart {
                source: self,
                card: start.card,
                agent: start.agent,
                goal: &start.goal,
                expected_seq: start.expected_seq,
                repositories: Some((start.round, &start.repositories)),
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
        let payload = self.issue_payload()?;
        let (agent, turn, goal, mode, task, session, model_selection, answers) = match payload {
            crate::issue::ManagedIssueRef::Ordinary {
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
            crate::issue::ManagedIssueRef::RawNote {
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
            | crate::issue::ManagedIssueRef::TaughtNote {
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
            | crate::issue::ManagedIssueRef::IdentifiedTaughtNote {
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
            | crate::issue::ManagedIssueRef::GoalNote {
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
            crate::issue::ManagedIssueRef::Conversation {
                agent,
                turn,
                task,
                session,
                model_selection,
                ..
            }
            | crate::issue::ManagedIssueRef::IdentifiedConversation {
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
            crate::issue::ManagedIssueRef::MailWake(issue)
            | crate::issue::ManagedIssueRef::IdentifiedMailWake(issue) => (
                issue.agent,
                issue.turn,
                Some(issue.goal),
                issue.mode,
                issue.task.as_str(),
                issue.session.as_ref(),
                Some(&issue.model_selection),
                &[][..],
            ),
        };
        Some(TurnIssue {
            source: self,
            payload,
            repositories: match self {
                Record::TurnIssuedWithRepositories(issue) => Some(&issue.grant),
                _ => None,
            },
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

/// Ticket lifecycle facts needed to select the authority for any turn source.
#[derive(Clone, Copy, Debug)]
pub struct ActiveRepositoryRound<'a> {
    /// Current holder, cleared by reassignment or release.
    pub holder: Option<AgentId>,
    /// Current card status.
    pub status: dudes_types::CardStatus,
    /// Exact last start retained on the ticket's goal.
    pub round: &'a TicketRepositoryRound,
    /// The holder still owns this unfinished goal.
    pub unfinished: bool,
    /// The round still owes instruction review or comparison.
    pub instructions_pending: bool,
}

/// Select one currently held round without deriving scope from the charged goal.
pub fn active_round<'a>(
    agent: AgentId,
    candidates: impl IntoIterator<Item = ActiveRepositoryRound<'a>>,
) -> Result<Option<&'a TicketRepositoryRound>, history::RepositoryStateError> {
    use dudes_types::CardStatus;
    let mut selected = None;
    for candidate in candidates {
        if candidate.holder == Some(agent)
            && match candidate.status {
                CardStatus::Doing | CardStatus::Review => true,
                CardStatus::Blocked => candidate.unfinished || candidate.instructions_pending,
                _ => false,
            }
            && selected.replace(candidate.round).is_some()
        {
            return Err(history::RepositoryStateError::Issue);
        }
    }
    Ok(selected)
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
    if let Some(created) = record.ticket_created() {
        return RepositoryFold::Ticket(created.repositories.map_or_else(
            || TicketRepositories::recorded(created.project),
            |(declaration, check_repo)| {
                TicketRepositories::Declared(crate::repository::TicketRepositoryScope {
                    declaration: declaration.clone(),
                    check_repo: check_repo.clone(),
                })
            },
        ));
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
            authority: start
                .repositories
                .map(|(_, repositories)| repositories.clone()),
        });
    }
    if let Some(issue) = record.turn_issue() {
        return RepositoryFold::Issue(before.hire.map(|(lsn, hire)| {
            issue.repositories.map_or_else(
                || TurnRepositories::recorded(lsn, hire, before.round),
                |grant| TurnRepositories::scoped(hire, grant),
            )
        }));
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
        let history::RepositoryHistoryChange::Hired {
            repositories: hire, ..
        } = history::prepare(Lsn(3), &record, history::RepositoryHistoryFacts::default()).unwrap()
        else {
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
    fn singleton_ticket_authority_keeps_project_a_string() {
        let project: TicketRepositories = "service".into();
        let wire = serde_json::json!({"project": "service"});
        assert_eq!(serde_json::to_value(&project).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<TicketRepositories>(wire).unwrap(),
            project
        );
        let schema = serde_json::to_value(schemars::schema_for!(TicketRepositories)).unwrap();
        assert_eq!(schema["properties"]["project"]["type"], "string");
    }
    #[test]
    fn active_scope_follows_ticket_lifecycle_instead_of_a_historical_goal_join() {
        use dudes_types::CardStatus;
        let Record::TicketStartedWithRepositories(start) =
            samples::record(RecordKind::TicketStartedWithRepositories)
        else {
            unreachable!()
        };
        let round = TicketRepositoryRound {
            card: start.card,
            goal: start.goal.id,
            start_lsn: Lsn(10),
            round: 1,
            repositories: None,
            authority: Some(start.repositories.clone()),
        };
        let candidate = ActiveRepositoryRound {
            holder: Some(start.agent),
            status: CardStatus::Doing,
            round: &round,
            unfinished: false,
            instructions_pending: false,
        };
        for status in [CardStatus::Doing, CardStatus::Review] {
            assert_eq!(
                active_round(
                    start.agent,
                    [ActiveRepositoryRound {
                        status,
                        ..candidate
                    }]
                )
                .unwrap(),
                Some(&round)
            );
        }
        for status in [CardStatus::Claimed, CardStatus::Done, CardStatus::Blocked] {
            assert_eq!(
                active_round(
                    start.agent,
                    [ActiveRepositoryRound {
                        status,
                        ..candidate
                    }]
                )
                .unwrap(),
                None
            );
        }
        let pending = ActiveRepositoryRound {
            status: CardStatus::Blocked,
            instructions_pending: true,
            ..candidate
        };
        assert_eq!(active_round(start.agent, [pending]).unwrap(), Some(&round));
        assert_eq!(
            active_round(
                start.agent,
                [ActiveRepositoryRound {
                    unfinished: true,
                    instructions_pending: false,
                    ..pending
                }]
            )
            .unwrap(),
            Some(&round)
        );
        assert!(active_round(start.agent, [candidate, candidate]).is_err());
        assert_eq!(
            active_round(
                start.agent,
                [ActiveRepositoryRound {
                    holder: None,
                    ..candidate
                }]
            )
            .unwrap(),
            None
        );
    }
}
