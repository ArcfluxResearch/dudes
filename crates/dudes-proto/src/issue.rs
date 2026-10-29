//! Closed issue payloads shared by legacy and repository-bound journal carriers.

use dudes_types::{AgentId, GoalId, Lsn, Mode, ModelSelection, SessionId, TurnId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::record::ConversationChargeScope;
use crate::{Record, mail_wake::MailWakeIssue};

macro_rules! managed_issues {
    ($( $record:ident => $kind:ident { $( $(#[$attribute:meta])* $field:ident: $ty:ty, )* }, )*) => {
        /// An issued task, without authority to carry unrelated journal events.
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        pub enum ManagedIssuePayload {
            $(
                #[doc = concat!("The `", stringify!($record), "` payload.")]
                $kind { $( $(#[$attribute])* $field: $ty, )* },
            )*
            /// Historical mail-only work under its exact preparation.
            MailWake(#[serde(deserialize_with = "crate::mail_wake::legacy_issue")] MailWakeIssue),
            /// Identified mail-only work under its exact preparation.
            IdentifiedMailWake(#[serde(deserialize_with = "crate::mail_wake::identified_issue")] MailWakeIssue),
        }

        /// Semantic issue family, independent of its physical journal carrier.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum ManagedIssueKind {
            $( #[doc = concat!("The `", stringify!($record), "` semantics.")] $kind, )*
            /// Historical mail-only work.
            MailWake,
            /// Identified mail-only work.
            IdentifiedMailWake,
        }

        /// Borrowed task and source-specific admission facts.
        #[derive(Clone, Copy, Debug)]
        pub enum ManagedIssueRef<'a> {
            $(
                #[doc = concat!("The `", stringify!($record), "` semantics.")]
                $kind { $( #[doc = concat!("Recorded `", stringify!($field), "`.")] $field: &'a $ty, )* },
            )*
            /// Historical mail-only work.
            MailWake(&'a MailWakeIssue),
            /// Identified mail-only work.
            IdentifiedMailWake(&'a MailWakeIssue),
        }

        impl ManagedIssuePayload {
            /// Restore the legacy carrier for this exact semantic payload.
            #[must_use]
            pub fn into_legacy_record(self) -> Record {
                match self {
                    $( Self::$kind { $($field,)* } => Record::$record { $($field,)* }, )*
                    Self::MailWake(issue) => Record::MailWakeOnlyTurnIssued(issue),
                    Self::IdentifiedMailWake(issue) => Record::IdentifiedMailWakeOnlyTurnIssued(issue),
                }
            }

            /// Borrow the payload without constructing another journal record.
            #[must_use]
            pub fn as_ref(&self) -> ManagedIssueRef<'_> {
                match self {
                    $( Self::$kind { $($field,)* } => ManagedIssueRef::$kind { $($field,)* }, )*
                    Self::MailWake(issue) => ManagedIssueRef::MailWake(issue),
                    Self::IdentifiedMailWake(issue) => ManagedIssueRef::IdentifiedMailWake(issue),
                }
            }
        }

        impl ManagedIssueRef<'_> {
            /// Copy only an issue payload into a new owned carrier.
            #[must_use]
            pub fn to_owned(self) -> ManagedIssuePayload {
                match self {
                    $( Self::$kind { $($field,)* } => ManagedIssuePayload::$kind { $($field: $field.to_owned(),)* }, )*
                    Self::MailWake(issue) => ManagedIssuePayload::MailWake(issue.clone()),
                    Self::IdentifiedMailWake(issue) => ManagedIssuePayload::IdentifiedMailWake(issue.clone()),
                }
            }

            /// Classify the semantic payload without changing physical provenance.
            #[must_use]
            pub const fn kind(self) -> ManagedIssueKind {
                match self {
                    $( Self::$kind { .. } => ManagedIssueKind::$kind, )*
                    Self::MailWake(_) => ManagedIssueKind::MailWake,
                    Self::IdentifiedMailWake(_) => ManagedIssueKind::IdentifiedMailWake,
                }
            }
        }

        impl Record {
            /// Borrow all issue metadata, regardless of the physical carrier.
            #[must_use]
            pub fn issue_payload(&self) -> Option<ManagedIssueRef<'_>> {
                Some(match self {
                    $( Self::$record { $($field,)* } => ManagedIssueRef::$kind { $($field,)* }, )*
                    Self::MailWakeOnlyTurnIssued(issue) => ManagedIssueRef::MailWake(issue),
                    Self::IdentifiedMailWakeOnlyTurnIssued(issue) => ManagedIssueRef::IdentifiedMailWake(issue),
                    Self::TurnIssuedWithRepositories(issue) => issue.payload.as_ref(),
                    _ => return None,
                })
            }
        }
    };
}

managed_issues! {
    TurnIssued => Ordinary {
        /// The agent.
        agent: AgentId,
        /// The new turn.
        turn: TurnId,
        /// The goal it serves.
        goal: GoalId,
        /// The task text, one argv element or stdin.
        task: String,
        /// Announced session, distinct from a later spawn announcement.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
        /// Managed issuance only.
        #[serde(deserialize_with = "crate::record::managed_mode")]
        #[schemars(with = "crate::record::ManagedModeSchema")]
        mode: Mode,
        /// Exact prelaunch choice, absent only in historical ordinary work.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_selection: Option<ModelSelection>,
        /// The `LoopDecided` that produced it.
        lsn_of_decision: Lsn,
        /// Answer positions carried in the task, oldest first.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        answers: Vec<Lsn>,
    },
    TurnIssuedWithBrowserNote => GoalNote {
        /// The owning agent.
        agent: AgentId,
        /// The exact issued turn identity.
        turn: TurnId,
        /// The original charged goal.
        goal: GoalId,
        /// The exact immutable task persisted before launch.
        task: String,
        /// The explicitly recorded announced session, or null.
        #[serde(deserialize_with = "crate::chat::required_nullable")]
        #[schemars(with = "crate::chat::ExplicitNullable<SessionId>")]
        session: Option<SessionId>,
        /// Managed issuance only.
        #[serde(deserialize_with = "crate::record::managed_mode")]
        #[schemars(with = "crate::record::ManagedModeSchema")]
        mode: Mode,
        /// The explicit prelaunch model selection.
        model_selection: ModelSelection,
        /// The exact ordinary or completed-note decision identity.
        lsn_of_decision: Lsn,
        /// The exact carried answer identities, empty on a note-only turn.
        answers: Vec<Lsn>,
        /// The exact `BrowserNoteQueued` identity.
        note_lsn: Lsn,
    },
    NoteOnlyTurnIssued => RawNote {
        /// The owning agent.
        agent: AgentId,
        /// The exact issued turn identity.
        turn: TurnId,
        /// The original charged goal.
        goal: GoalId,
        /// The exact immutable task persisted before launch.
        task: String,
        /// The explicitly recorded announced session, or null.
        #[serde(deserialize_with = "crate::chat::required_nullable")]
        #[schemars(with = "crate::chat::ExplicitNullable<SessionId>")]
        session: Option<SessionId>,
        /// Managed issuance only.
        #[serde(deserialize_with = "crate::record::managed_mode")]
        #[schemars(with = "crate::record::ManagedModeSchema")]
        mode: Mode,
        /// The explicit prelaunch model selection.
        model_selection: ModelSelection,
        /// The exact ordinary or completed-note decision identity.
        lsn_of_decision: Lsn,
        /// The exact carried answer identities, empty on a note-only turn.
        #[serde(deserialize_with = "crate::record::no_answers")]
        #[schemars(length(max = 0))]
        answers: Vec<Lsn>,
        /// The exact `BrowserNoteQueued` identity.
        note_lsn: Lsn,
        /// The durable ordinary goal Done identity.
        completion_lsn: Lsn,
    },
    TaughtNoteOnlyTurnIssued => TaughtNote {
        /// The owning agent.
        agent: AgentId,
        /// The exact issued turn identity.
        turn: TurnId,
        /// The original charged goal.
        goal: GoalId,
        /// The exact immutable task persisted before launch.
        #[serde(deserialize_with = "crate::task::taught_task")]
        #[schemars(with = "crate::task::TaughtTaskSchema")]
        task: String,
        /// The explicitly recorded announced session, or null.
        #[serde(deserialize_with = "crate::chat::required_nullable")]
        #[schemars(with = "crate::chat::ExplicitNullable<SessionId>")]
        session: Option<SessionId>,
        /// Managed issuance only.
        #[serde(deserialize_with = "crate::record::managed_mode")]
        #[schemars(with = "crate::record::ManagedModeSchema")]
        mode: Mode,
        /// The explicit prelaunch model selection.
        model_selection: ModelSelection,
        /// The exact taught completed-note decision identity.
        lsn_of_decision: Lsn,
        /// The exact carried answer identities, empty on a note-only turn.
        #[serde(deserialize_with = "crate::record::no_answers")]
        #[schemars(length(max = 0))]
        answers: Vec<Lsn>,
        /// The exact `BrowserNoteQueued` identity.
        note_lsn: Lsn,
        /// The durable ordinary goal Done identity.
        completion_lsn: Lsn,
    },
    IdentifiedTaughtNoteOnlyTurnIssued => IdentifiedTaughtNote {
        /// The owning agent.
        agent: AgentId,
        /// The exact issued turn identity.
        turn: TurnId,
        /// The original charged goal.
        goal: GoalId,
        /// The exact immutable task persisted before launch.
        #[serde(deserialize_with = "crate::task::identified_taught_task")]
        #[schemars(with = "crate::task::IdentifiedTaughtTaskSchema")]
        task: String,
        /// The explicitly recorded announced session, or null.
        #[serde(deserialize_with = "crate::chat::required_nullable")]
        #[schemars(with = "crate::chat::ExplicitNullable<SessionId>")]
        session: Option<SessionId>,
        /// Managed issuance only.
        #[serde(deserialize_with = "crate::record::managed_mode")]
        #[schemars(with = "crate::record::ManagedModeSchema")]
        mode: Mode,
        /// The explicit prelaunch model selection.
        model_selection: ModelSelection,
        /// The exact taught completed-note decision identity.
        lsn_of_decision: Lsn,
        /// The exact carried answer identities, empty on a note-only turn.
        #[serde(deserialize_with = "crate::record::no_answers")]
        #[schemars(length(max = 0))]
        answers: Vec<Lsn>,
        /// The exact `BrowserNoteQueued` identity.
        note_lsn: Lsn,
        /// The durable ordinary goal Done identity.
        completion_lsn: Lsn,
    },
    ConversationTurnIssued => Conversation {
        /// Owning agent.
        agent: AgentId,
        /// Single logical turn identity retained during crash recovery.
        turn: TurnId,
        /// Exact accepted message.
        note_lsn: Lsn,
        /// Captured context generation.
        context_lsn: Lsn,
        /// Immutable budget scope.
        charge_scope: ConversationChargeScope,
        /// Exact task, persisted before launch.
        task: String,
        /// Explicit launch session or null.
        #[serde(deserialize_with = "crate::chat::required_nullable")]
        #[schemars(with = "crate::chat::ExplicitNullable<SessionId>")]
        session: Option<SessionId>,
        /// Explicit prelaunch model selection.
        model_selection: ModelSelection,
        /// Exact admitted decision.
        lsn_of_decision: Lsn,
    },
    IdentifiedConversationTurnIssued => IdentifiedConversation {
        /// Owning agent.
        agent: AgentId,
        /// Single logical turn identity retained during crash recovery.
        turn: TurnId,
        /// Exact accepted message.
        note_lsn: Lsn,
        /// Captured context generation.
        context_lsn: Lsn,
        /// Immutable budget scope.
        charge_scope: ConversationChargeScope,
        /// Exact task, persisted before launch, opening with the agent's identity line.
        task: String,
        /// Explicit launch session or null.
        #[serde(deserialize_with = "crate::chat::required_nullable")]
        #[schemars(with = "crate::chat::ExplicitNullable<SessionId>")]
        session: Option<SessionId>,
        /// Explicit prelaunch model selection.
        model_selection: ModelSelection,
        /// Exact admitted decision.
        lsn_of_decision: Lsn,
    },
}

impl<'a> ManagedIssueRef<'a> {
    /// Mail admission payload and the contract selected by its carrier.
    #[must_use]
    pub const fn mail_wake(self) -> Option<(&'a MailWakeIssue, crate::task::TaskContract)> {
        match self {
            Self::MailWake(issue) => Some((issue, crate::task::TaskContract::Legacy)),
            Self::IdentifiedMailWake(issue) => Some((issue, crate::task::TaskContract::Identified)),
            _ => None,
        }
    }
}

impl Record {
    /// Ordinary goal issue, including a repository-bound carrier.
    #[must_use]
    pub fn ordinary_issue(&self) -> Option<crate::repository_state::TurnIssue<'_>> {
        self.turn_issue()
            .filter(|issue| matches!(issue.payload, ManagedIssueRef::Ordinary { .. }))
    }

    /// Mail wake payload with the contract selected by the semantic carrier.
    #[must_use]
    pub fn mail_issue(&self) -> Option<(&MailWakeIssue, crate::task::TaskContract)> {
        self.issue_payload()?.mail_wake()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RecordKind;
    use crate::record::samples;
    use crate::repository::TicketRepositoryRound;

    fn scoped(payload: ManagedIssuePayload) -> Record {
        let Record::TurnIssuedWithRepositories(mut issued) =
            samples::record(RecordKind::TurnIssuedWithRepositories)
        else {
            unreachable!()
        };
        issued.grant.hired_lsn = Lsn(5);
        issued.payload = payload;
        Record::TurnIssuedWithRepositories(issued)
    }

    #[test]
    fn all_issue_families_keep_their_exact_payload_and_physical_identity() {
        for kind in [
            RecordKind::TurnIssued,
            RecordKind::TurnIssuedWithBrowserNote,
            RecordKind::NoteOnlyTurnIssued,
            RecordKind::TaughtNoteOnlyTurnIssued,
            RecordKind::IdentifiedTaughtNoteOnlyTurnIssued,
            RecordKind::ConversationTurnIssued,
            RecordKind::IdentifiedConversationTurnIssued,
            RecordKind::MailWakeOnlyTurnIssued,
            RecordKind::IdentifiedMailWakeOnlyTurnIssued,
        ] {
            let legacy = samples::record(kind);
            assert_eq!(
                legacy
                    .issue_payload()
                    .unwrap()
                    .to_owned()
                    .into_legacy_record(),
                legacy
            );
            let wrapped = scoped(legacy.issue_payload().unwrap().to_owned());
            let encoded = serde_json::to_vec(&wrapped).unwrap();
            let decoded: Record = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(wrapped, decoded);
            let old = legacy.turn_issue().unwrap();
            let new = decoded.turn_issue().unwrap();
            assert_eq!(
                (new.agent, new.turn, new.goal, new.mode),
                (old.agent, old.turn, old.goal, old.mode)
            );
            assert_eq!(
                (new.task, new.session, new.model_selection, new.answers),
                (old.task, old.session, old.model_selection, old.answers)
            );
            assert_eq!(new.payload.to_owned(), old.payload.to_owned());
            assert_eq!(new.payload.kind(), old.payload.kind());
            assert_eq!(new.source.kind(), RecordKind::TurnIssuedWithRepositories);
            assert!(std::ptr::eq(new.source, &raw const decoded));
            assert_eq!(new.recovery_new(), old.recovery_new());
            assert_eq!(new.source.mail_issue(), old.source.mail_issue());
        }
        let legacy = samples::record(RecordKind::TurnIssued);
        let mut payload = legacy.issue_payload().unwrap().to_owned();
        let ManagedIssuePayload::Ordinary {
            model_selection, ..
        } = &mut payload
        else {
            unreachable!()
        };
        *model_selection = None;
        let wrapped = scoped(payload);
        assert_eq!(wrapped.turn_issue().unwrap().model_selection, None);
    }

    #[test]
    fn finite_issue_round_is_explicit_and_never_becomes_its_charge_goal() {
        let Record::TicketStartedWithRepositories(start) =
            samples::record(RecordKind::TicketStartedWithRepositories)
        else {
            unreachable!()
        };
        let mut wrapped = scoped(
            samples::record(RecordKind::IdentifiedConversationTurnIssued)
                .issue_payload()
                .unwrap()
                .to_owned(),
        );
        let Record::TurnIssuedWithRepositories(issue) = &mut wrapped else {
            unreachable!()
        };
        issue.grant.ticket = Some(TicketRepositoryRound {
            card: start.card,
            goal: start.goal.id,
            start_lsn: Lsn(8),
            round: 1,
            repositories: None,
            authority: Some(start.repositories.clone()),
        });
        let view = wrapped.turn_issue().unwrap();
        assert_eq!(view.goal, None);
        assert_eq!(view.instruction_goal(), Some(start.goal.id));
        let crate::repository_state::RepositoryFold::Issue(Some(frozen)) =
            crate::repository_state::fold(
                Lsn(9),
                &wrapped,
                crate::repository_state::RepositoryFacts {
                    hire: Some((Lsn(5), &crate::repository::HireRepositories::default())),
                    ..crate::repository_state::RepositoryFacts::default()
                },
            )
        else {
            panic!("frozen issue")
        };
        assert_eq!(frozen.ticket().unwrap().goal, start.goal.id);
        assert_eq!(frozen.grant(), view.repositories);
    }

    #[test]
    fn scoped_issue_payload_is_closed_and_preserves_required_contract_fields() {
        let wrapped = scoped(
            samples::record(RecordKind::IdentifiedTaughtNoteOnlyTurnIssued)
                .issue_payload()
                .unwrap()
                .to_owned(),
        );
        let good = serde_json::to_value(&wrapped).unwrap();
        for field in [
            "session",
            "model_selection",
            "lsn_of_decision",
            "completion_lsn",
        ] {
            let mut bad = good.clone();
            bad["payload"].as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<Record>(bad).is_err(), "{field}");
        }
        for (field, value) in [
            ("task", serde_json::json!("raw note")),
            ("answers", serde_json::json!([1])),
            ("mode", serde_json::json!("attached")),
            ("kind", serde_json::json!("agent_hired")),
            ("future_permission", serde_json::json!(true)),
        ] {
            let mut bad = good.clone();
            bad["payload"][field] = value;
            assert!(serde_json::from_value::<Record>(bad).is_err(), "{field}");
        }
        let mut bad = good;
        bad["grant"].as_object_mut().unwrap().remove("ticket");
        assert!(serde_json::from_value::<Record>(bad).is_err());
    }
    #[test]
    fn explicit_general_scope_does_not_rejoin_a_completed_ticket_by_its_charge_goal() {
        let mut instructions = crate::instruction_state::InstructionState::default();
        for (lsn, record) in [
            (Lsn(1), samples::record(RecordKind::TicketAssigned)),
            (Lsn(2), samples::record(RecordKind::TicketStarted)),
            (
                Lsn(3),
                Record::CardTransition {
                    card: samples::card(),
                    from: dudes_types::CardStatus::Doing,
                    to: dudes_types::CardStatus::Done,
                    by: crate::Actor::System,
                    expected_seq: dudes_types::Seq(3),
                    reason: None,
                },
            ),
        ] {
            instructions
                .apply_validated(
                    lsn,
                    &crate::Entry {
                        at: dudes_types::Timestamp::UNIX_EPOCH,
                        record,
                    },
                    |_| crate::record::Hex32::new([0; 32]),
                )
                .unwrap();
        }
        let legacy = samples::record(RecordKind::NoteOnlyTurnIssued);
        let charged = legacy.turn_issue().unwrap().goal.unwrap();
        assert!(
            instructions
                .association_for_goal(charged, dudes_types::Seq(4))
                .is_some()
        );
        let general = scoped(legacy.issue_payload().unwrap().to_owned());
        assert_eq!(general.turn_issue().unwrap().goal, Some(charged));
        assert_eq!(general.turn_issue().unwrap().instruction_goal(), None);
        assert_eq!(
            legacy.turn_issue().unwrap().instruction_goal(),
            Some(charged)
        );
        let crate::repository_state::RepositoryFold::Issue(Some(frozen)) =
            crate::repository_state::fold(
                Lsn(9),
                &general,
                crate::repository_state::RepositoryFacts {
                    hire: Some((Lsn(5), &crate::repository::HireRepositories::default())),
                    ..crate::repository_state::RepositoryFacts::default()
                },
            )
        else {
            panic!("frozen issue")
        };
        assert_eq!(frozen.instruction_goal(Some(charged)), None);
    }
}
