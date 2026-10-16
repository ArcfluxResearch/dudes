//! Immutable record witnesses. Current lifecycle and admission facts belong to their existing folds.

use std::collections::BTreeMap;
use std::fmt;

use dudes_types::{
    AgentId, CardId, GoalId, Lsn, Mode, Provenance, Provider, SessionId, TurnId, WorktreeId,
};
use serde::{Deserialize, Serialize};

use crate::record::{Actor, BrowserControl, Entry, Hex32, Record, RecordKind, TurnOutcome};
use crate::recovery::{RecoveryIssue, RecoveryIssued, RecoveryWork};
use crate::{AutomaticHold, BrowserNoteDestination, Evidence, LoopDecision};

#[cfg(test)]
mod tests;

/// Exact identity carried by a source record, without inferring a current owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RecoveryWitnessOwner {
    /// Agent-scoped evidence.
    Agent(AgentId),
    /// A recorded goal installation or clearing.
    Goal {
        /// Agent named by the record.
        agent: AgentId,
        /// Goal named by the record.
        goal: GoalId,
    },
    /// A ticket's recorded goal installation, without a derived round.
    Ticket {
        /// Agent named by the record.
        agent: AgentId,
        /// Installed goal.
        goal: GoalId,
        /// Started card.
        card: CardId,
    },
    /// A goal-bearing issue with its explicit goal.
    Work(RecoveryWork),
    /// A physical attempt or result whose goal comes from its issue.
    Turn {
        /// Agent named by the record.
        agent: AgentId,
        /// Original turn identity.
        turn: TurnId,
    },
    /// A goal predicate result carries no agent identity.
    Predicate {
        /// Observed goal.
        goal: GoalId,
        /// Recorded turn, absent for turn-free checks.
        #[serde(deserialize_with = "crate::chat::required_nullable")]
        turn: Option<TurnId>,
    },
    /// A card mutation carries no inferred current claimant.
    Card(CardId),
    /// A daemon-created checkout lease.
    Worktree(WorktreeId),
}

/// A historical lookup must match position, record kind and structural owner together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryWitnessRef {
    /// Actual source position.
    pub lsn: Lsn,
    /// Actual source payload kind.
    pub kind: RecordKind,
    /// Identity explicitly carried by the source.
    pub owner: RecoveryWitnessOwner,
}

/// Additional immutable references carried by a goal-bearing issue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RecoveryIssueCarrier {
    /// Ordinary goal work.
    Ordinary,
    /// Goal work carrying a whole reserved browser note.
    BrowserNote {
        /// Exact reserved note.
        note_lsn: Lsn,
    },
    /// A note under an already-completed goal, distinct from unfinished-goal recovery.
    CompletedNote {
        /// Exact reserved note.
        note_lsn: Lsn,
        /// Completion named by the source carrier.
        completion_lsn: Lsn,
    },
    /// Recorded mail-wake carrier references.
    MailWake {
        /// Root mail carrier.
        root_lsn: Lsn,
        /// Completion named by the source carrier.
        completion_lsn: Lsn,
    },
}

/// Fixed outcome class without provider error or response text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryEndClass {
    /// Stream reached a recorded completed outcome.
    Completed,
    /// Stream reached the provider turn cap.
    MaxTurns,
    /// Stream reached the provider budget cap.
    BudgetExhausted,
    /// A recorded `Failed` outcome; failure text stays in the original record.
    Failed,
    /// A recorded `NoResult` outcome.
    NoResult,
    /// A recorded `Vanished` outcome.
    Vanished,
    /// A recorded `NotLaunched` outcome.
    NotLaunched,
    /// A recorded `LaunchRefused` outcome.
    LaunchRefused,
    /// Another known outcome, not interchangeable with those above.
    Other,
    /// Older history did not record an outcome.
    Unrecorded,
}

/// The held cause's structural evidence, without raw error text or clearing authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryHoldWitness {
    /// Exact two failures named by the hold.
    RepeatedFastFailure {
        /// First failed turn named by the hold.
        first: TurnId,
        /// Latest failed turn named by the hold.
        latest: TurnId,
    },
    /// Adapter refusal.
    LaunchRefused,
    /// Host-confirmed failed launch.
    LaunchFailed,
    /// The original recorded lease must be restored and independently validated.
    WorktreeMissing(WorktreeId),
    /// An unreadable future cause remains unavailable for recovery.
    Unknown,
}

/// Raw predicate card evidence; qualification stays with the existing predicate rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryCardEvidence {
    /// Explicit observed card.
    pub card: CardId,
    /// Explicit observed status.
    pub status: dudes_types::CardStatus,
    /// Recorded author, not authority supplied by a recovery request.
    pub by: Actor,
}

/// Bounded source fields needed by later historical joins, without current-state interpretation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RecoveryEvidence {
    /// Recorded hire configuration and role source.
    Hire {
        /// Recorded provider.
        provider: Provider,
        /// Recorded checkout identity.
        #[serde(deserialize_with = "crate::chat::required_nullable")]
        worktree: Option<WorktreeId>,
        /// Digest of the original role bytes.
        role_digest: Hex32,
    },
    /// A lease's immutable origin relationship; layout remains in its existing fold.
    Lease {
        /// Recorded parent lease, when present.
        #[serde(deserialize_with = "crate::chat::required_nullable")]
        from: Option<WorktreeId>,
    },
    /// Exact issued-task witness and carrier references.
    Issue {
        /// Recorded mode, without treating every carrier as managed.
        mode: Mode,
        /// Exact issued-task witness.
        issued: RecoveryIssued,
        /// Exact carrier references.
        carrier: RecoveryIssueCarrier,
    },
    /// Goal-free finite issue, never assigned an invented `GoalId`.
    FiniteIssue {
        /// Digest of the original task bytes.
        task_digest: Hex32,
        /// Exact reserved note.
        note_lsn: Lsn,
        /// Source context, never substituted for a hire.
        context_lsn: Lsn,
    },
    /// Physical launch announcement, separate from the original issue announcement.
    Spawn {
        /// Announcement for this physical attempt.
        #[serde(deserialize_with = "crate::chat::required_nullable")]
        session: Option<SessionId>,
        /// Recorded repeat-attempt bit.
        reissued: bool,
    },
    /// Vendor-confirmed identifier for the recorded attempt.
    Confirmation {
        /// Vendor-confirmed conversation identity.
        session: SessionId,
    },
    /// Recorded terminal facts, not a derived no-effect or completion verdict.
    End {
        /// Exact result of the existing recorded-end value rule, without asserting a hold.
        fast_failure: bool,
        /// Recorded mode.
        mode: Mode,
        /// Recorded observer.
        provenance: Provenance,
        /// Fixed result class.
        outcome: RecoveryEndClass,
    },
    /// Existing pause/resume attribution, without an inferred local actor or previous control.
    Control {
        /// Existing browser attribution, without inferring a local actor when absent.
        #[serde(deserialize_with = "crate::chat::required_nullable")]
        browser: Option<BrowserControl>,
    },
    /// Raw hold evidence, without interpreting it as releasable.
    Hold {
        /// Recorded observer.
        provenance: Provenance,
        /// Structural cause without raw failure text.
        cause: RecoveryHoldWitness,
    },
    /// The record's text source is retained by digest rather than copied into another state.
    Text {
        /// Digest of the exact source bytes.
        digest: Hex32,
    },
    /// A goal reassignment names its next owner explicitly.
    Reassigned {
        /// Explicit next owner.
        to: AgentId,
    },
    /// A ticket assignment names its target explicitly.
    Assigned {
        /// Explicit assigned agent.
        agent: AgentId,
    },
    /// Raw predicate evidence; `reported_pass` is not qualified completion authority.
    Predicate {
        /// Recorded observer.
        provenance: Provenance,
        /// Raw result before the existing completion qualification rule.
        reported_pass: bool,
        /// Raw card status and author, when recorded.
        #[serde(deserialize_with = "crate::chat::required_nullable")]
        card: Option<RecoveryCardEvidence>,
    },
    /// The record explicitly contains a Done decision; current goal and eligibility are external.
    DoneDecision,
    /// The actual kind and owner are sufficient evidence; no derived effect is stored.
    Marker,
}

/// Standing values copied from the existing agent fold, without granting operator permission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryStandingControl {
    /// An ordinary pause is standing.
    pub paused: bool,
    /// Halt remains standing even when a later pause changed the control identity.
    pub halted: bool,
    /// Exact current automatic cause, including an unreadable future cause.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    pub hold: Option<RecoveryHoldWitness>,
}

/// The preceding control read from the existing agent fold before this record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPriorControl {
    /// Actual standing values before the named control record.
    pub standing: RecoveryStandingControl,
    /// Actual affected agent.
    pub agent: AgentId,
    /// Previous control identity; explicit absence differs from unavailable context.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    pub lsn: Option<Lsn>,
}

/// Immutable relationship captured from the existing turn and agent rows before an attempt record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_field_names,
    reason = "lsn suffixes identify serialized journal positions for source-position normalization"
)]
pub struct RecoveryAttemptContext {
    /// Actual issue that owns the physical turn.
    pub issue_lsn: Lsn,
    /// Latest preceding physical spawn, if any.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    pub previous_spawn_lsn: Option<Lsn>,
    /// Latest preceding terminal source, if any.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    pub previous_end_lsn: Option<Lsn>,
    /// Agent's confirmed conversation before this record, not a permission or history guarantee.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    pub confirmed_session_lsn: Option<Lsn>,
}

/// Exact existing goal qualified by its unchanged predicate rule at the source record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryGoalTarget {
    /// Existing owner before the result.
    pub agent: AgentId,
    /// Existing goal identity.
    pub goal: GoalId,
}

/// Qualify only the supplied existing goal's result; this does not fold goal lifecycle.
#[must_use]
pub fn qualified_completion(
    agent: AgentId,
    goal: GoalId,
    predicate: &crate::Predicate,
    record: &Record,
) -> Option<RecoveryGoalTarget> {
    let Record::PredicateResult {
        goal: seen,
        provenance,
        pass,
        evidence,
        ..
    } = record
    else {
        return None;
    };
    let card = match evidence {
        crate::Evidence::Card { card, status, by } => Some((*card, *status, by)),
        _ => None,
    };
    (*seen == goal
        && *pass
        && provenance.is_observed()
        && crate::goal::predicate_card_evidence_eligible(predicate, card))
    .then_some(RecoveryGoalTarget { agent, goal })
}

/// Narrow before-record context supplied by the existing validated folds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryWitnessContext {
    /// Existing predicate qualification at this actual result, without changing goal policy.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    pub qualified_completion: Option<RecoveryGoalTarget>,
    /// Existing row relationships, absent when no turn row was available.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    pub attempt: Option<RecoveryAttemptContext>,
    /// Present only when the existing agent row supplied the preceding control.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    pub previous_control: Option<RecoveryPriorControl>,
    /// Only the instruction fold's validated derived goal clear, never a raw action label.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    pub cleared_goal: Option<crate::instruction::InstructionGoalClear>,
}

/// One immutable projection of an actual journal record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryWitness {
    pub(super) reference: RecoveryWitnessRef,
    pub(super) evidence: RecoveryEvidence,
    context: RecoveryWitnessContext,
}

impl RecoveryWitness {
    /// Actual before-record context, absent when it was not supplied by a fold.
    #[must_use]
    pub const fn context(&self) -> RecoveryWitnessContext {
        self.context
    }

    /// Exact position, kind and owner.
    #[must_use]
    pub const fn reference(&self) -> RecoveryWitnessRef {
        self.reference
    }

    /// Raw immutable source evidence, without recovery eligibility.
    #[must_use]
    pub const fn evidence(&self) -> &RecoveryEvidence {
        &self.evidence
    }

    /// The original goal-bearing issue witness, including its actual position.
    #[must_use]
    pub fn issue(&self) -> Option<RecoveryIssue> {
        let RecoveryEvidence::Issue { issued, .. } = &self.evidence else {
            return None;
        };
        Some(RecoveryIssue {
            lsn: self.reference.lsn,
            issued: issued.clone(),
        })
    }

    pub(super) fn validate(&self) -> Result<(), RecoveryHistoryError> {
        use RecordKind as K;
        use RecoveryEvidence as E;
        use RecoveryWitnessOwner as O;
        if self.reference.lsn == Lsn::ZERO {
            return Err(RecoveryHistoryError::Position);
        }
        if let Some(attempt) = self.context.attempt
            && (!matches!(
                self.reference.kind,
                K::TurnSpawned | K::SessionLearned | K::TurnEnded
            ) || !matches!(self.reference.owner, O::Turn { .. })
                || attempt.issue_lsn == Lsn::ZERO
                || attempt.issue_lsn >= self.reference.lsn
                || [
                    attempt.previous_spawn_lsn,
                    attempt.previous_end_lsn,
                    attempt.confirmed_session_lsn,
                ]
                .into_iter()
                .flatten()
                .any(|at| at == Lsn::ZERO || at >= self.reference.lsn))
        {
            return Err(RecoveryHistoryError::Identity);
        }
        if let Some(previous) = self.context.previous_control
            && (self.reference.owner != O::Agent(previous.agent)
                || !matches!(
                    self.reference.kind,
                    K::Pause | K::Resume | K::Halt | K::AgentHeld
                )
                || previous
                    .lsn
                    .is_some_and(|at| at == Lsn::ZERO || at >= self.reference.lsn))
        {
            return Err(RecoveryHistoryError::Identity);
        }
        if let Some(target) = self.context.qualified_completion
            && (self.reference.kind != K::PredicateResult
                || !matches!(self.reference.owner, O::Predicate { goal, .. } if goal == target.goal)
                || !matches!(
                    self.evidence,
                    E::Predicate {
                        provenance: Provenance::Observed,
                        reported_pass: true,
                        ..
                    }
                ))
        {
            return Err(RecoveryHistoryError::Identity);
        }
        if self.context.cleared_goal.is_some()
            && self.reference.kind != K::TicketInstructionReviewed
        {
            return Err(RecoveryHistoryError::Identity);
        }
        let valid = match (&self.evidence, self.reference.owner) {
            (E::Hire { .. }, O::Agent(_)) => self.reference.kind == K::AgentHired,
            (E::Lease { .. }, O::Worktree(_)) => self.reference.kind == K::WorktreeLeased,
            (
                E::Issue {
                    issued, carrier, ..
                },
                O::Work(work),
            ) => issued.work == work && issue_kind(self.reference.kind, carrier),
            (E::FiniteIssue { .. }, O::Turn { .. }) => {
                self.reference.kind == K::ConversationTurnIssued
            }
            (E::Spawn { .. }, O::Turn { .. }) => self.reference.kind == K::TurnSpawned,
            (E::Confirmation { .. }, O::Turn { .. }) => self.reference.kind == K::SessionLearned,
            (E::End { .. }, O::Turn { .. }) => self.reference.kind == K::TurnEnded,
            (E::Control { browser }, O::Agent(_)) => {
                matches!(self.reference.kind, K::Pause | K::Resume)
                    || (self.reference.kind == K::Halt && browser.is_none())
            }
            (E::Hold { .. }, O::Agent(_)) => self.reference.kind == K::AgentHeld,
            (E::Text { .. }, owner) => text_owner(self.reference.kind, owner),
            (E::Reassigned { .. }, O::Goal { .. }) => self.reference.kind == K::GoalReassigned,
            (E::Assigned { .. }, O::Card(_)) => self.reference.kind == K::TicketAssigned,
            (E::Predicate { .. }, O::Predicate { .. }) => self.reference.kind == K::PredicateResult,
            (E::DoneDecision, O::Agent(_)) => self.reference.kind == K::LoopDecided,
            (E::Marker, owner) => marker_owner(self.reference.kind, owner),
            _ => false,
        };
        if !valid {
            return Err(RecoveryHistoryError::Identity);
        }
        Ok(())
    }
}

/// A missing or malformed historical dependency, without source contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryHistoryError {
    /// A source position is not an actual positive position.
    Position,
    /// The requested kind or owner does not match the source.
    Identity,
    /// Required source evidence was not retained or is unavailable.
    MissingWitness,
    /// The same actual position names different evidence.
    ConflictingWitness,
}

impl fmt::Display for RecoveryHistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid recovery witness: {self:?}")
    }
}
impl std::error::Error for RecoveryHistoryError {}

/// Immutable witnesses only. No current goal, ticket round, control or grant is reconstructed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryHistory {
    pub(super) witnesses: BTreeMap<Lsn, RecoveryWitness>,
}

impl RecoveryHistory {
    /// Read one actual source position; its kind and owner still require the caller's join.
    pub fn at(&self, lsn: Lsn) -> Result<&RecoveryWitness, RecoveryHistoryError> {
        self.witnesses
            .get(&lsn)
            .ok_or(RecoveryHistoryError::MissingWitness)
    }

    /// Retain narrow source evidence at its actual position. Repeated equal observations are inert.
    pub fn observe(
        &mut self,
        lsn: Lsn,
        entry: &Entry,
        hash: impl Fn(&[u8]) -> Hex32,
    ) -> Result<(), RecoveryHistoryError> {
        self.observe_with_context(lsn, entry, RecoveryWitnessContext::default(), hash)
    }

    /// Retain actual fold context with the source; this operation supplies no certification.
    pub fn observe_with_context(
        &mut self,
        lsn: Lsn,
        entry: &Entry,
        context: RecoveryWitnessContext,
        hash: impl Fn(&[u8]) -> Hex32,
    ) -> Result<(), RecoveryHistoryError> {
        if let Some(mut witness) = witness(lsn, &entry.record, hash)? {
            witness.context = context;
            self.insert(witness)?;
        } else if lsn == Lsn::ZERO {
            return Err(RecoveryHistoryError::Position);
        } else if self.witnesses.contains_key(&lsn) {
            return Err(RecoveryHistoryError::ConflictingWitness);
        }
        Ok(())
    }

    /// Resolve one exact source reference; absence never becomes a default fact.
    pub fn get(
        &self,
        reference: RecoveryWitnessRef,
    ) -> Result<&RecoveryWitness, RecoveryHistoryError> {
        let witness = self
            .witnesses
            .get(&reference.lsn)
            .ok_or(RecoveryHistoryError::MissingWitness)?;
        if witness.reference != reference {
            return Err(RecoveryHistoryError::Identity);
        }
        Ok(witness)
    }

    pub(super) fn insert(&mut self, witness: RecoveryWitness) -> Result<(), RecoveryHistoryError> {
        witness.validate()?;
        if let Some(old) = self.witnesses.get(&witness.reference.lsn) {
            if *old != witness {
                return Err(RecoveryHistoryError::ConflictingWitness);
            }
        } else {
            self.witnesses.insert(witness.reference.lsn, witness);
        }
        Ok(())
    }
}

fn issue_kind(kind: RecordKind, carrier: &RecoveryIssueCarrier) -> bool {
    matches!(
        (kind, carrier),
        (RecordKind::TurnIssued, RecoveryIssueCarrier::Ordinary)
            | (
                RecordKind::TurnIssuedWithBrowserNote,
                RecoveryIssueCarrier::BrowserNote { .. }
            )
            | (
                RecordKind::NoteOnlyTurnIssued | RecordKind::TaughtNoteOnlyTurnIssued,
                RecoveryIssueCarrier::CompletedNote { .. }
            )
            | (
                RecordKind::MailWakeOnlyTurnIssued,
                RecoveryIssueCarrier::MailWake { .. }
            )
    )
}

fn text_owner(kind: RecordKind, owner: RecoveryWitnessOwner) -> bool {
    matches!(
        (kind, owner),
        (
            RecordKind::HumanAnswer | RecordKind::Steer | RecordKind::AgentLookChanged,
            RecoveryWitnessOwner::Agent(_)
        ) | (RecordKind::GoalSet, RecoveryWitnessOwner::Goal { .. })
            | (
                RecordKind::TicketStarted,
                RecoveryWitnessOwner::Ticket { .. }
            )
            | (
                RecordKind::BrowserNoteQueued,
                RecoveryWitnessOwner::Agent(_) | RecoveryWitnessOwner::Goal { .. }
            )
    )
}

fn marker_owner(kind: RecordKind, owner: RecoveryWitnessOwner) -> bool {
    matches!(
        (kind, owner),
        (RecordKind::GoalCleared, RecoveryWitnessOwner::Goal { .. })
            | (
                RecordKind::CardCreated
                    | RecordKind::CardTransition
                    | RecordKind::Claim
                    | RecordKind::TicketCreated
                    | RecordKind::TicketRemoved
                    | RecordKind::TicketInstructionReviewed,
                RecoveryWitnessOwner::Card(_)
            )
            | (
                RecordKind::AgentRetired | RecordKind::Attached | RecordKind::Detached,
                RecoveryWitnessOwner::Agent(_)
            )
            | (
                RecordKind::WorktreeRemoved,
                RecoveryWitnessOwner::Worktree(_)
            )
            | (
                RecordKind::BrowserNoteIssuedCancelled | RecordKind::BrowserNoteReissueCancelled,
                RecoveryWitnessOwner::Turn { .. }
            )
    )
}

fn text_digest(text: &str, hash: &impl Fn(&[u8]) -> Hex32) -> Hex32 {
    let mut bytes = b"dudes.recovery.source-text.v1\0".to_vec();
    bytes.extend_from_slice(text.as_bytes());
    hash(&bytes)
}

/// Hash the exact issued task bytes using the recovery witness domain.
#[must_use]
pub fn task_digest(task: &str, hash: &impl Fn(&[u8]) -> Hex32) -> Hex32 {
    let mut bytes = b"dudes.recovery.task.v1\0".to_vec();
    bytes.extend_from_slice(task.as_bytes());
    hash(&bytes)
}

fn issued(
    work: RecoveryWork,
    task: &str,
    answers: &[Lsn],
    model_selection: Option<dudes_types::ModelSelection>,
    session: Option<SessionId>,
    hash: &impl Fn(&[u8]) -> Hex32,
) -> RecoveryIssued {
    RecoveryIssued {
        work,
        task_digest: task_digest(task, hash),
        answers: answers.to_vec(),
        model_selection,
        session,
    }
}

pub(super) fn witness(
    lsn: Lsn,
    record: &Record,
    hash: impl Fn(&[u8]) -> Hex32,
) -> Result<Option<RecoveryWitness>, RecoveryHistoryError> {
    if lsn == Lsn::ZERO {
        return Err(RecoveryHistoryError::Position);
    }
    let Some((owner, evidence)) = record_evidence(record, &hash) else {
        return Ok(None);
    };
    Ok(Some(RecoveryWitness {
        reference: RecoveryWitnessRef {
            lsn,
            kind: record.kind(),
            owner,
        },
        evidence,
        context: RecoveryWitnessContext::default(),
    }))
}

fn record_evidence(
    record: &Record,
    hash: &impl Fn(&[u8]) -> Hex32,
) -> Option<(RecoveryWitnessOwner, RecoveryEvidence)> {
    ordinary_issue_evidence(record, hash)
        .or_else(|| note_issue_evidence(record, hash))
        .or_else(|| attempt_evidence(record))
        .or_else(|| target_evidence(record, hash))
        .or_else(|| source_evidence(record, hash))
}

fn ordinary_issue_evidence(
    record: &Record,
    hash: &impl Fn(&[u8]) -> Hex32,
) -> Option<(RecoveryWitnessOwner, RecoveryEvidence)> {
    use RecoveryEvidence as E;
    use RecoveryWitnessOwner as O;
    match record {
        Record::TurnIssued {
            agent,
            goal,
            turn,
            task,
            answers,
            model_selection,
            session,
            mode,
            ..
        } => {
            let issued = issued(
                RecoveryWork {
                    agent: *agent,
                    goal: *goal,
                    turn: *turn,
                },
                task,
                answers,
                model_selection.clone(),
                session.clone(),
                hash,
            );
            Some((
                O::Work(issued.work),
                E::Issue {
                    mode: *mode,
                    issued,
                    carrier: RecoveryIssueCarrier::Ordinary,
                },
            ))
        }
        Record::MailWakeOnlyTurnIssued(value) => {
            let issued = issued(
                RecoveryWork {
                    agent: value.agent,
                    goal: value.goal,
                    turn: value.turn,
                },
                value.task.as_str(),
                &value.answers,
                Some(value.model_selection.clone()),
                value.session.clone(),
                hash,
            );
            Some((
                O::Work(issued.work),
                E::Issue {
                    mode: value.mode,
                    issued,
                    carrier: RecoveryIssueCarrier::MailWake {
                        root_lsn: value.root_lsn,
                        completion_lsn: value.completion_lsn,
                    },
                },
            ))
        }
        Record::ConversationTurnIssued {
            agent,
            turn,
            task,
            note_lsn,
            context_lsn,
            ..
        } => Some((
            O::Turn {
                agent: *agent,
                turn: *turn,
            },
            E::FiniteIssue {
                task_digest: task_digest(task, hash),
                note_lsn: *note_lsn,
                context_lsn: *context_lsn,
            },
        )),
        _ => None,
    }
}

fn note_issue_evidence(
    record: &Record,
    hash: &impl Fn(&[u8]) -> Hex32,
) -> Option<(RecoveryWitnessOwner, RecoveryEvidence)> {
    use RecoveryEvidence as E;
    use RecoveryWitnessOwner as O;
    match record {
        Record::TurnIssuedWithBrowserNote {
            agent,
            goal,
            turn,
            task,
            answers,
            model_selection,
            session,
            mode,
            note_lsn,
            ..
        } => {
            let issued = issued(
                RecoveryWork {
                    agent: *agent,
                    goal: *goal,
                    turn: *turn,
                },
                task,
                answers,
                Some(model_selection.clone()),
                session.clone(),
                hash,
            );
            Some((
                O::Work(issued.work),
                E::Issue {
                    mode: *mode,
                    issued,
                    carrier: RecoveryIssueCarrier::BrowserNote {
                        note_lsn: *note_lsn,
                    },
                },
            ))
        }
        Record::NoteOnlyTurnIssued {
            agent,
            goal,
            turn,
            task,
            answers,
            model_selection,
            session,
            mode,
            note_lsn,
            completion_lsn,
            ..
        }
        | Record::TaughtNoteOnlyTurnIssued {
            agent,
            goal,
            turn,
            task,
            answers,
            model_selection,
            session,
            mode,
            note_lsn,
            completion_lsn,
            ..
        } => {
            let issued = issued(
                RecoveryWork {
                    agent: *agent,
                    goal: *goal,
                    turn: *turn,
                },
                task,
                answers,
                Some(model_selection.clone()),
                session.clone(),
                hash,
            );
            Some((
                O::Work(issued.work),
                E::Issue {
                    mode: *mode,
                    issued,
                    carrier: RecoveryIssueCarrier::CompletedNote {
                        note_lsn: *note_lsn,
                        completion_lsn: *completion_lsn,
                    },
                },
            ))
        }
        _ => None,
    }
}

fn attempt_evidence(record: &Record) -> Option<(RecoveryWitnessOwner, RecoveryEvidence)> {
    use RecoveryEvidence as E;
    use RecoveryWitnessOwner as O;
    match record {
        Record::TurnSpawned {
            agent,
            turn,
            session,
            reissued,
            ..
        } => Some((
            O::Turn {
                agent: *agent,
                turn: *turn,
            },
            E::Spawn {
                session: session.clone(),
                reissued: *reissued,
            },
        )),
        Record::SessionLearned {
            agent,
            turn,
            session,
        } => Some((
            O::Turn {
                agent: *agent,
                turn: *turn,
            },
            E::Confirmation {
                session: session.clone(),
            },
        )),
        Record::TurnEnded {
            agent,
            turn,
            mode,
            provenance,
            outcome,
            ..
        } => Some((
            O::Turn {
                agent: *agent,
                turn: *turn,
            },
            E::End {
                fast_failure: crate::decision::is_fast_failure(record),
                mode: *mode,
                provenance: *provenance,
                outcome: end_class(outcome.as_ref()),
            },
        )),
        Record::AgentHeld {
            agent,
            provenance,
            cause,
        } => Some((
            O::Agent(*agent),
            E::Hold {
                provenance: *provenance,
                cause: RecoveryHoldWitness::from(cause),
            },
        )),
        Record::Pause { agent, browser } | Record::Resume { agent, browser } => Some((
            O::Agent(*agent),
            E::Control {
                browser: browser.clone(),
            },
        )),
        Record::Halt { agent, .. } => Some((O::Agent(*agent), E::Control { browser: None })),
        _ => None,
    }
}

fn target_evidence(
    record: &Record,
    hash: &impl Fn(&[u8]) -> Hex32,
) -> Option<(RecoveryWitnessOwner, RecoveryEvidence)> {
    use RecoveryEvidence as E;
    use RecoveryWitnessOwner as O;
    match record {
        Record::AgentHired {
            id,
            provider,
            worktree,
            role,
            ..
        } => Some((
            O::Agent(*id),
            E::Hire {
                provider: *provider,
                worktree: *worktree,
                role_digest: text_digest(role, hash),
            },
        )),
        Record::WorktreeLeased { worktree, from, .. } => {
            Some((O::Worktree(*worktree), E::Lease { from: *from }))
        }
        Record::GoalSet { agent, goal, .. } => Some((
            O::Goal {
                agent: *agent,
                goal: goal.id,
            },
            E::Text {
                digest: text_digest(&goal.text, hash),
            },
        )),
        Record::TicketStarted {
            agent, card, goal, ..
        } => Some((
            O::Ticket {
                agent: *agent,
                goal: goal.id,
                card: *card,
            },
            E::Text {
                digest: text_digest(&goal.text, hash),
            },
        )),
        Record::GoalCleared { agent, goal } => Some((
            O::Goal {
                agent: *agent,
                goal: *goal,
            },
            E::Marker,
        )),
        Record::GoalReassigned { from, to, goal, .. } => Some((
            O::Goal {
                agent: *from,
                goal: *goal,
            },
            E::Reassigned { to: *to },
        )),
        Record::AgentRetired { id, .. } => Some((O::Agent(*id), E::Marker)),
        Record::Attached { agent, .. } | Record::Detached { agent, .. } => {
            Some((O::Agent(*agent), E::Marker))
        }
        Record::WorktreeRemoved { worktree, .. } => Some((O::Worktree(*worktree), E::Marker)),
        Record::CardCreated { card, .. }
        | Record::CardTransition { card, .. }
        | Record::Claim { card, .. }
        | Record::TicketCreated { card, .. } => Some((O::Card(*card), E::Marker)),
        _ => None,
    }
}

fn source_evidence(
    record: &Record,
    hash: &impl Fn(&[u8]) -> Hex32,
) -> Option<(RecoveryWitnessOwner, RecoveryEvidence)> {
    use RecoveryEvidence as E;
    use RecoveryWitnessOwner as O;
    match record {
        Record::HumanAnswer { agent, text, .. } | Record::Steer { agent, text, .. } => Some((
            O::Agent(*agent),
            E::Text {
                digest: text_digest(text, hash),
            },
        )),
        Record::AgentLookChanged { id, role, .. } => Some((
            O::Agent(*id),
            E::Text {
                digest: text_digest(role, hash),
            },
        )),
        Record::BrowserNoteQueued {
            agent,
            text,
            destination,
            ..
        } => {
            let owner = match destination {
                BrowserNoteDestination::ManagedGoal { goal } => O::Goal {
                    agent: *agent,
                    goal: *goal,
                },
                _ => O::Agent(*agent),
            };
            Some((
                owner,
                E::Text {
                    digest: text_digest(text, hash),
                },
            ))
        }
        Record::PredicateResult {
            goal,
            turn,
            provenance,
            pass,
            evidence,
            ..
        } => {
            let card = match evidence {
                Evidence::Card { card, status, by } => Some(RecoveryCardEvidence {
                    card: *card,
                    status: *status,
                    by: *by,
                }),
                _ => None,
            };
            Some((
                O::Predicate {
                    goal: *goal,
                    turn: *turn,
                },
                E::Predicate {
                    provenance: *provenance,
                    reported_pass: *pass,
                    card,
                },
            ))
        }
        Record::LoopDecided {
            agent,
            decision: LoopDecision::Done,
            ..
        } => Some((O::Agent(*agent), E::DoneDecision)),
        Record::TicketAssigned { card, agent, .. } => {
            Some((O::Card(*card), E::Assigned { agent: *agent }))
        }
        Record::TicketRemoved { card, .. } | Record::TicketInstructionReviewed { card, .. } => {
            Some((O::Card(*card), E::Marker))
        }
        Record::BrowserNoteIssuedCancelled { agent, turn, .. }
        | Record::BrowserNoteReissueCancelled { agent, turn, .. } => Some((
            O::Turn {
                agent: *agent,
                turn: *turn,
            },
            E::Marker,
        )),
        _ => None,
    }
}

fn end_class(value: Option<&TurnOutcome>) -> RecoveryEndClass {
    match value {
        Some(TurnOutcome::Completed) => RecoveryEndClass::Completed,
        Some(TurnOutcome::MaxTurns) => RecoveryEndClass::MaxTurns,
        Some(TurnOutcome::BudgetExhausted) => RecoveryEndClass::BudgetExhausted,
        Some(TurnOutcome::Failed { .. }) => RecoveryEndClass::Failed,
        Some(TurnOutcome::NoResult) => RecoveryEndClass::NoResult,
        Some(TurnOutcome::Vanished) => RecoveryEndClass::Vanished,
        Some(TurnOutcome::NotLaunched { .. }) => RecoveryEndClass::NotLaunched,
        Some(TurnOutcome::LaunchRefused { .. }) => RecoveryEndClass::LaunchRefused,
        Some(_) => RecoveryEndClass::Other,
        None => RecoveryEndClass::Unrecorded,
    }
}

impl From<&AutomaticHold> for RecoveryHoldWitness {
    fn from(value: &AutomaticHold) -> Self {
        match value {
            AutomaticHold::RepeatedFastFailure { first, latest } => Self::RepeatedFastFailure {
                first: *first,
                latest: *latest,
            },
            AutomaticHold::LaunchRefused { .. } => Self::LaunchRefused,
            AutomaticHold::LaunchFailed { .. } => Self::LaunchFailed,
            AutomaticHold::WorktreeMissing { worktree } => Self::WorktreeMissing(*worktree),
            AutomaticHold::Unknown => Self::Unknown,
        }
    }
}
