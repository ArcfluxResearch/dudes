//! Pure journal-derived instruction obligations shared by lifecycle and projections.

pub mod cache;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use dudes_types::{AgentId, CardId, GoalId, Lsn, Mode, Seq, Timestamp, TurnId};
use serde::{Deserialize, Serialize};

use crate::instruction::{
    InstructionAcceptCompletion, InstructionBaselineRef, InstructionBoundaryAbsenceProof,
    InstructionChangedPath, InstructionCoverageGap, InstructionEndBoundary, InstructionGapKind,
    InstructionGoalClear, InstructionIssueAssociation, InstructionNativeBoundaryReceipt,
    InstructionObservationOwner, InstructionPendingReview, InstructionReviewAction,
    InstructionReviewInterval, InstructionReviewWatermark, InstructionSettlement,
    InstructionSnapshot, InstructionTicketAssociation, InstructionUnknownReason,
    InstructionValidationError, MAX_INSTRUCTION_INPUTS, MAX_INSTRUCTION_METADATA_BYTES,
    validate_instruction_record,
};
use crate::record::{Entry, Hex32, Record};
use crate::ticket::TicketReason;

/// Terminal comparison evidence. Its original position survives duplicate delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionFinished {
    /// First terminal record position.
    pub lsn: Lsn,
    /// Immutable settlement.
    pub settlement: InstructionSettlement,
}

/// One recorded attempt or a turn interval missing its starting observation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionAttempt {
    /// Exact starting record or actual missing turn boundary.
    pub baseline: InstructionBaselineRef,
    /// Interval owner.
    pub owner: InstructionObservationOwner,
    /// Captured ticket association.
    pub ticket: Option<InstructionTicketAssociation>,
    /// Original baseline; absent only for explicit missing-boundary evidence.
    pub snapshot: Option<InstructionSnapshot>,
    /// First terminal comparison or uncertainty.
    pub finished: Option<InstructionFinished>,
    /// Review receipt that consumed this interval, without proving child absence.
    pub reviewed_lsn: Option<Lsn>,
    /// Actual spawn receipt associated with this attempt, when available.
    pub spawn_lsn: Option<Lsn>,
    /// Scoped native-boundary absence, independent of comparison and review.
    pub boundary_settled: Option<InstructionBoundarySettlement>,
}

/// First monotonic scoped boundary receipt for one exact attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionBoundarySettlement {
    /// First qualifying durable position.
    pub lsn: Lsn,
    /// Exact qualifying proof.
    pub proof: InstructionBoundaryAbsenceProof,
}

/// Existing card and qualified completion facts at the serialized review boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstructionReviewFacts {
    /// Current ticket.
    pub card: CardId,
    /// Current card sequence.
    pub seq: Seq,
    /// Current ticket goal, not the goal of an earlier captured round.
    pub goal: Option<GoalId>,
    /// Existing first qualified observed completion of the current goal.
    pub completed_lsn: Option<Lsn>,
    /// Exact incumbent rework clearing decision from current holder, goal and completion facts.
    pub clear_goal: Option<InstructionGoalClear>,
}

/// Changes for the caller's existing card fold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstructionFold {
    /// Cards whose fresh observation evidence advances their sequence once.
    pub evidence_cards: Vec<CardId>,
    /// Validated atomic action applied using existing acceptance or assignment semantics.
    pub reviewed: Option<(CardId, InstructionReviewAction)>,
    /// Validated goal clearing carried by the atomic rework record. Never set by acceptance.
    pub cleared_goal: Option<InstructionGoalClear>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Issue {
    agent: AgentId,
    lsn: Lsn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct End {
    agent: AgentId,
    mode: Mode,
    lsn: Lsn,
}

/// Journal facts only. No filesystem state, clock, launch permission or mutable task lives here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionState {
    attempts: BTreeMap<Lsn, InstructionAttempt>,
    issues: BTreeMap<TurnId, Issue>,
    ends: BTreeMap<TurnId, End>,
    attachments: BTreeMap<Lsn, AgentId>,
    attachment_ends: BTreeMap<Lsn, Lsn>,
    current_turns: BTreeMap<TurnId, Lsn>,
    current_attachments: BTreeMap<Lsn, Lsn>,
    attachment_attempts: BTreeMap<Lsn, Lsn>,
    unfinished: BTreeSet<Lsn>,
    unresolved: BTreeMap<AgentId, BTreeSet<Lsn>>,
    by_card: BTreeMap<CardId, BTreeSet<Lsn>>,
    by_goal: BTreeMap<GoalId, BTreeSet<Lsn>>,
    assignments: BTreeMap<CardId, Lsn>,
    card_goals: BTreeMap<CardId, GoalId>,
    associations: BTreeMap<GoalId, InstructionTicketAssociation>,
    ticket_history: BTreeMap<GoalId, InstructionTicketAssociation>,
    native_receipts: BTreeMap<Lsn, (Lsn, InstructionNativeBoundaryReceipt)>,
    pty_attachments: BTreeMap<crate::rpc::pty::PtyId, Lsn>,
    close_requests: BTreeMap<Lsn, Lsn>,
    latest_attachments: BTreeMap<AgentId, Lsn>,
    no_host_holds: BTreeMap<Lsn, AgentId>,
    no_host_claims: BTreeMap<Lsn, Lsn>,
    observed_turns: BTreeSet<TurnId>,
    last_applied: Lsn,
}

/// A rejected durable relationship, without task, configuration or instruction contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstructionStateError {
    /// Pure carrier validation failed.
    Contract(InstructionValidationError),
    /// Required owner or historical record does not match.
    Boundary,
    /// A second terminal result conflicts with the first.
    ConflictingSettlement,
    /// A second scoped boundary receipt conflicts with the first.
    ConflictingBoundarySettlement,
    /// The ticket or current sequence is stale.
    StaleCard,
    /// The exact set differs from the caller's reviewed set.
    StaleReview,
    /// A related comparison remains outstanding.
    PendingComparison,
    /// Existing qualified completion does not support the action.
    Completion,
    /// A bounded canonical digest or evidence view could not be encoded.
    Encoding,
}

impl fmt::Display for InstructionStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid instruction history: {self:?}")
    }
}
impl std::error::Error for InstructionStateError {}

impl From<InstructionValidationError> for InstructionStateError {
    fn from(error: InstructionValidationError) -> Self {
        Self::Contract(error)
    }
}

#[derive(Clone, Copy)]
enum ReviewAuthority {
    Current(Option<InstructionReviewFacts>),
    Validated,
}

impl InstructionState {
    /// Validate admission against the committed prefix without previewing a rotated journal LSN
    /// or changing state. Actual positions are folded only after append has synced.
    pub fn validate_next(
        &self,
        entry: &Entry,
        facts: Option<InstructionReviewFacts>,
        hash: impl Fn(&[u8]) -> Hex32,
    ) -> Result<(), InstructionStateError> {
        validate_instruction_record(&entry.record)?;
        let after = Lsn(self
            .last_applied
            .0
            .checked_add(1)
            .ok_or(InstructionStateError::Boundary)?);
        match &entry.record {
            Record::InstructionObservationStarted { owner, ticket, .. } => {
                self.start_valid(*owner)?;
                self.ticket_valid(*ticket)
            }
            Record::InstructionObservationFinished {
                owner,
                ticket,
                baseline,
                settlement,
            } => self.finish_valid(after, *owner, *ticket, *baseline, settlement),
            Record::TicketInstructionReviewed {
                card,
                expected_seq,
                watermark,
                action,
                ..
            } => self.review_valid(
                *card,
                *expected_seq,
                *watermark,
                action,
                ReviewAuthority::Current(facts),
                hash,
            ),
            Record::InstructionAttemptBoundarySettled {
                owner,
                started_lsn,
                proof,
            } => self.boundary_valid(after, *owner, *started_lsn, proof),
            _ => Ok(()),
        }
    }

    /// Fold one canonical entry. Callers preflight this same operation before append.
    /// Repeated positions do nothing; conflicting second settlements at new positions refuse.
    pub fn apply(
        &mut self,
        lsn: Lsn,
        entry: &Entry,
        facts: Option<InstructionReviewFacts>,
        hash: impl Fn(&[u8]) -> Hex32,
    ) -> Result<InstructionFold, InstructionStateError> {
        self.apply_inner(lsn, entry, facts, hash, false)
    }

    /// Fold canonical entries after serialized daemon admission or full Reader prefix validation established card
    /// sequence and qualified completion authority. This never fabricates those facts in World.
    pub fn apply_validated(
        &mut self,
        lsn: Lsn,
        entry: &Entry,
        hash: impl Fn(&[u8]) -> Hex32,
    ) -> Result<InstructionFold, InstructionStateError> {
        self.apply_inner(lsn, entry, None, hash, true)
    }

    fn apply_inner(
        &mut self,
        lsn: Lsn,
        entry: &Entry,
        facts: Option<InstructionReviewFacts>,
        hash: impl Fn(&[u8]) -> Hex32,
        validated: bool,
    ) -> Result<InstructionFold, InstructionStateError> {
        if lsn <= self.last_applied {
            return Ok(InstructionFold::default());
        }
        validate_instruction_record(&entry.record)?;
        let mut folded = InstructionFold::default();
        match &entry.record {
            Record::InstructionObservationStarted {
                owner,
                ticket,
                snapshot,
            } => {
                self.start_attempt(lsn, *owner, *ticket, snapshot)?;
                note_card(&mut folded.evidence_cards, *ticket);
            }
            Record::InstructionObservationFinished {
                owner,
                ticket,
                baseline,
                settlement,
            } => {
                if self.finish_attempt(lsn, *owner, *ticket, *baseline, settlement)? {
                    note_card(&mut folded.evidence_cards, *ticket);
                }
            }
            Record::TicketInstructionReviewed {
                card,
                expected_seq,
                watermark,
                action,
                ..
            } => {
                self.review_valid(
                    *card,
                    *expected_seq,
                    *watermark,
                    action,
                    if validated {
                        ReviewAuthority::Validated
                    } else {
                        ReviewAuthority::Current(facts)
                    },
                    &hash,
                )?;
                let keys: Vec<_> = self.review_attempts(*card).map(|(key, _)| *key).collect();
                for key in keys {
                    if let Some(attempt) = self.attempts.get_mut(&key) {
                        attempt.reviewed_lsn = Some(lsn);
                    }
                    self.refresh_pending(key);
                }
                folded.reviewed = Some((*card, action.clone()));
                if let InstructionReviewAction::ChangesRequested { clear_goal, .. } = action {
                    folded.cleared_goal = *clear_goal;
                    self.assignments.insert(*card, lsn);
                    self.remove_association(*card);
                }
            }
            Record::InstructionAttemptBoundarySettled {
                owner,
                started_lsn,
                proof,
            } => {
                self.boundary_valid(lsn, *owner, *started_lsn, proof)?;
                let attempt = self
                    .attempts
                    .get_mut(started_lsn)
                    .ok_or(InstructionStateError::Boundary)?;
                if attempt.boundary_settled.is_none() {
                    attempt.boundary_settled = Some(InstructionBoundarySettlement {
                        lsn,
                        proof: proof.clone(),
                    });
                    self.clear_unresolved(*owner, *started_lsn);
                }
            }
            _ => self.fold_boundaries(lsn, &entry.record, &mut folded),
        }
        self.last_applied = lsn;
        Ok(folded)
    }

    fn start_attempt(
        &mut self,
        lsn: Lsn,
        owner: InstructionObservationOwner,
        ticket: Option<InstructionTicketAssociation>,
        snapshot: &InstructionSnapshot,
    ) -> Result<(), InstructionStateError> {
        self.start_valid(owner)?;
        self.ticket_valid(ticket)?;
        self.attempts.insert(
            lsn,
            InstructionAttempt {
                baseline: InstructionBaselineRef::Started { lsn },
                owner,
                ticket,
                snapshot: Some(snapshot.clone()),
                finished: None,
                reviewed_lsn: None,
                spawn_lsn: None,
                boundary_settled: None,
            },
        );
        self.unfinished.insert(lsn);
        self.refresh_pending(lsn);
        self.note_turn(owner);
        match owner {
            InstructionObservationOwner::Managed { turn, issue, .. } => {
                self.current_turns.insert(turn, lsn);
                if matches!(issue, InstructionIssueAssociation::Issued { .. }) {
                    self.note_unresolved(owner, lsn);
                }
            }
            InstructionObservationOwner::AttachedProcess { attachment_lsn, .. } => {
                self.current_attachments.insert(attachment_lsn, lsn);
                self.attachment_attempts.insert(attachment_lsn, lsn);
                self.note_unresolved(owner, lsn);
            }
            InstructionObservationOwner::AttachedTurn { .. } => {}
        }
        Ok(())
    }

    fn finish_attempt(
        &mut self,
        lsn: Lsn,
        owner: InstructionObservationOwner,
        ticket: Option<InstructionTicketAssociation>,
        baseline: InstructionBaselineRef,
        settlement: &InstructionSettlement,
    ) -> Result<bool, InstructionStateError> {
        let key = baseline_key(baseline);
        self.finish_valid(lsn, owner, ticket, baseline, settlement)?;
        if self
            .attempts
            .get(&key)
            .is_none_or(|attempt| attempt.finished.is_none())
        {
            let attempt = self.attempts.entry(key).or_insert(InstructionAttempt {
                baseline,
                owner,
                ticket,
                snapshot: None,
                finished: None,
                reviewed_lsn: None,
                spawn_lsn: None,
                boundary_settled: None,
            });
            attempt.finished = Some(InstructionFinished {
                lsn,
                settlement: settlement.clone(),
            });
            self.unfinished.remove(&key);
            match owner {
                InstructionObservationOwner::Managed { turn, .. } => {
                    if self.current_turns.get(&turn) == Some(&key) {
                        self.current_turns.remove(&turn);
                    }
                }
                InstructionObservationOwner::AttachedProcess { attachment_lsn, .. } => {
                    self.current_attachments.remove(&attachment_lsn);
                }
                InstructionObservationOwner::AttachedTurn { .. } => {}
            }
            match settlement {
                InstructionSettlement::Unknown {
                    reason:
                        InstructionUnknownReason::UnrecordedLaunchBoundary
                        | InstructionUnknownReason::CleanupUnconfirmed,
                    ..
                } => self.note_unresolved(owner, key),
                InstructionSettlement::NoProcess { proof_lsn, .. } => {
                    self.no_host_claims.insert(*proof_lsn, key);
                    self.clear_unresolved(owner, key);
                }
                InstructionSettlement::NoIssuedTurn => self.clear_unresolved(owner, key),
                _ => {}
            }
            self.refresh_pending(key);
            self.note_turn(owner);
            return Ok(true);
        }
        Ok(false)
    }

    /// Every durable attempt in canonical position order, including reviewed uncertainty.
    pub fn attempts(&self) -> impl Iterator<Item = (&Lsn, &InstructionAttempt)> {
        self.attempts.iter()
    }

    /// One recorded instruction interval, including consumed review or settled boundaries.
    #[must_use]
    pub fn attempt(&self, boundary_lsn: Lsn) -> Option<&InstructionAttempt> {
        self.attempts.get(&boundary_lsn)
    }

    /// Owners with at least one unresolved launch boundary.
    pub fn unresolved_owners(&self) -> impl Iterator<Item = AgentId> + '_ {
        self.unresolved.keys().copied()
    }

    /// Whether any starting or missing-boundary observation was recorded for this turn.
    #[must_use]
    pub fn has_attempt_for_turn(&self, turn: TurnId) -> bool {
        self.observed_turns.contains(&turn)
    }

    /// The latest actual attachment record for this agent, without claiming it remains open.
    #[must_use]
    pub fn attachment_lsn(&self, agent: AgentId) -> Option<Lsn> {
        self.latest_attachments.get(&agent).copied()
    }

    /// The first recorded confirmed close for this attachment.
    #[must_use]
    pub fn attachment_end_lsn(&self, attachment: Lsn) -> Option<Lsn> {
        self.attachment_ends.get(&attachment).copied()
    }

    /// Current unfinished exact attempt for an immutable managed turn.
    #[must_use]
    pub fn current_for_turn(&self, turn: TurnId) -> Option<(Lsn, &InstructionAttempt)> {
        let key = *self.current_turns.get(&turn)?;
        Some((key, self.attempts.get(&key)?))
    }

    /// Later Managed attempt identified by its first canonical spawn receipt.
    /// Retention after completion does not settle the earlier observation's native boundary.
    #[must_use]
    pub fn later_managed_attempt_for_spawn(
        &self,
        agent: AgentId,
        turn: TurnId,
        earlier_started: Lsn,
        spawn_lsn: Lsn,
    ) -> Option<(Lsn, &InstructionAttempt)> {
        let &(started, receipt) = self.native_receipts.get(&spawn_lsn)?;
        if started <= earlier_started
            || receipt != (InstructionNativeBoundaryReceipt::Spawned { lsn: spawn_lsn })
        {
            return None;
        }
        let attempt = self.attempts.get(&started)?;
        let issue = self
            .issues
            .get(&turn)
            .filter(|issue| issue.agent == agent)?;
        let matches_issue = match attempt.owner {
            InstructionObservationOwner::Managed {
                agent: prior_agent,
                turn: prior_turn,
                issue: association,
            } if prior_agent == agent && prior_turn == turn => match association {
                InstructionIssueAssociation::Prospective => started < issue.lsn,
                InstructionIssueAssociation::Issued { issue_lsn } => issue_lsn == issue.lsn,
            },
            _ => false,
        };
        (matches_issue && attempt.spawn_lsn == Some(spawn_lsn)).then_some((started, attempt))
    }

    /// Current unfinished whole-process attempt for a real attachment.
    #[must_use]
    pub fn current_for_attachment(&self, attachment: Lsn) -> Option<(Lsn, &InstructionAttempt)> {
        let key = *self.current_attachments.get(&attachment)?;
        Some((key, self.attempts.get(&key)?))
    }

    /// Latest whole-process attempt for this exact attachment, retained after settlement.
    #[must_use]
    pub fn attempt_for_attachment(&self, attachment: Lsn) -> Option<(Lsn, &InstructionAttempt)> {
        let key = *self.attachment_attempts.get(&attachment)?;
        Some((key, self.attempts.get(&key)?))
    }

    /// Unfinished obligations without scanning historical terminal attempts.
    pub fn unfinished(&self) -> impl Iterator<Item = (Lsn, &InstructionAttempt)> {
        self.unfinished
            .iter()
            .filter_map(|key| self.attempts.get(key).map(|attempt| (*key, attempt)))
    }

    /// Actual immutable issue position, when one was recorded.
    #[must_use]
    pub fn issue_lsn(&self, turn: TurnId) -> Option<Lsn> {
        self.issues.get(&turn).map(|issue| issue.lsn)
    }

    /// First actual observed end position, when one was recorded.
    #[must_use]
    pub fn end_lsn(&self, turn: TurnId) -> Option<Lsn> {
        self.ends.get(&turn).map(|end| end.lsn)
    }

    /// First observed Managed end recorded for this exact agent and turn.
    #[must_use]
    pub fn managed_end_lsn(&self, agent: AgentId, turn: TurnId) -> Option<Lsn> {
        self.ends
            .get(&turn)
            .filter(|end| end.agent == agent && end.mode == Mode::Managed)
            .map(|end| end.lsn)
    }

    /// Current ticket association with the card sequence the caller observes now.
    #[must_use]
    pub fn association_for_goal(
        &self,
        goal: GoalId,
        current_seq: Seq,
    ) -> Option<InstructionTicketAssociation> {
        self.associations
            .get(&goal)
            .map(|association| InstructionTicketAssociation {
                seq_at_start: current_seq,
                ..*association
            })
    }

    /// The association recorded when a ticket installed this goal. It retains that record's
    /// sequence; callers must establish current card eligibility separately.
    #[must_use]
    pub fn recorded_association_for_goal(
        &self,
        goal: GoalId,
    ) -> Option<InstructionTicketAssociation> {
        self.ticket_history.get(&goal).copied()
    }

    /// Attempts whose launch ownership is still uncertain. Accept never removes this evidence.
    pub fn unresolved_launch_attempts(
        &self,
        agent: AgentId,
    ) -> impl Iterator<Item = (&Lsn, &InstructionAttempt)> {
        self.unresolved
            .get(&agent)
            .into_iter()
            .flatten()
            .filter_map(|key| self.attempts.get_key_value(key))
    }

    /// Unresolved native boundaries matching this starting folder, including boundaries whose
    /// starting folder was not recorded. Missing folder evidence cannot prove a different scope.
    pub fn unresolved_in_folder<'a>(
        &'a self,
        agent: AgentId,
        folder: &'a crate::instruction::NativeInstructionPath,
    ) -> impl Iterator<Item = (&'a Lsn, &'a InstructionAttempt)> {
        self.unresolved_launch_attempts(agent)
            .filter(move |(_, attempt)| {
                attempt
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.execution_folder.as_ref())
                    .is_none_or(|recorded| recorded == folder)
            })
    }

    fn note_unresolved(&mut self, owner: InstructionObservationOwner, key: Lsn) {
        if self
            .attempts
            .get(&key)
            .is_some_and(|attempt| attempt.boundary_settled.is_some())
        {
            return;
        }
        self.unresolved
            .entry(owner_agent(owner))
            .or_default()
            .insert(key);
    }

    fn note_turn(&mut self, owner: InstructionObservationOwner) {
        if let InstructionObservationOwner::Managed { turn, .. }
        | InstructionObservationOwner::AttachedTurn { turn, .. } = owner
        {
            self.observed_turns.insert(turn);
        }
    }

    fn clear_unresolved(&mut self, owner: InstructionObservationOwner, key: Lsn) {
        remove_member(&mut self.unresolved, &owner_agent(owner), key);
    }

    fn refresh_pending(&mut self, key: Lsn) {
        let Some(attempt) = self.attempts.get(&key) else {
            return;
        };
        let Some(ticket) = attempt.ticket else {
            return;
        };
        if attempt.reviewed_lsn.is_none()
            && (attempt.finished.is_none() || requires_review(attempt))
        {
            self.by_card.entry(ticket.card).or_default().insert(key);
            self.by_goal.entry(ticket.goal).or_default().insert(key);
        } else {
            remove_member(&mut self.by_card, &ticket.card, key);
            remove_member(&mut self.by_goal, &ticket.goal, key);
        }
    }

    fn remove_association(&mut self, card: CardId) {
        if let Some(goal) = self.card_goals.remove(&card) {
            self.associations.remove(&goal);
        }
    }

    /// Cards with unfinished observations or unconsumed instruction review.
    pub fn pending_cards(&self) -> impl Iterator<Item = CardId> + '_ {
        self.by_card.keys().copied()
    }

    /// Whether the captured goal owes an ending comparison or unconsumed review.
    /// A running baseline alone does not block normal work or create Needs you.
    #[must_use]
    pub fn pending_for_goal(&self, goal: GoalId) -> bool {
        self.by_goal.get(&goal).into_iter().flatten().any(|key| {
            self.attempts.get(key).is_some_and(|attempt| {
                requires_review(attempt)
                    || (attempt.finished.is_none() && self.end_for(attempt.owner).is_some())
            })
        })
    }

    /// Whether removal or a review action must wait for an unfinished observation.
    #[must_use]
    pub fn observation_pending(&self, card: CardId) -> bool {
        self.review_attempts(card)
            .any(|(_, attempt)| attempt.finished.is_none())
    }

    /// Shared changes take precedence over independent uncertainty; local-only changes do not hold.
    #[must_use]
    pub fn ticket_reason(&self, card: CardId) -> Option<TicketReason> {
        let mut unknown = false;
        for (_, attempt) in self.review_attempts(card) {
            if let Some(comparison) = comparison(attempt)
                && comparison.has_shared_changes()
            {
                return Some(TicketReason::SharedInstructionsChanged);
            }
            unknown |= requires_review(attempt);
        }
        unknown.then_some(TicketReason::SharedInstructionsUnknown)
    }

    /// Exact full-set watermark, including intervals omitted from the bounded detail view.
    /// Each hash input is bounded and domain-separated; the chain covers every ordered member.
    pub fn watermark(
        &self,
        card: CardId,
        hash: impl Fn(&[u8]) -> Hex32,
    ) -> Result<Option<InstructionReviewWatermark>, InstructionStateError> {
        let mut digest = hash(b"dudes.instructions.review.v1");
        let mut through = Lsn::ZERO;
        for (key, attempt) in self.review_attempts(card) {
            let finished = attempt.finished.as_ref().map(|finished| finished.lsn);
            let end = self.attempt_end(attempt);
            let mut bytes = b"dudes.instructions.review.member.v1\0".to_vec();
            bytes.extend(
                serde_json::to_vec(&(
                    digest,
                    card,
                    key,
                    attempt.baseline,
                    attempt.owner,
                    attempt.ticket,
                    finished,
                    end,
                ))
                .map_err(|_| InstructionStateError::Encoding)?,
            );
            digest = hash(&bytes);
            through = through
                .max(*key)
                .max(finished.unwrap_or(Lsn::ZERO))
                .max(end.map_or(Lsn::ZERO, end_position));
        }
        Ok(
            (through != Lsn::ZERO).then_some(InstructionReviewWatermark {
                through_lsn: through,
                set_digest: digest,
            }),
        )
    }

    /// Bounded operator evidence. Overflow remains a named gap and does not shrink the watermark.
    pub fn review(
        &self,
        card: CardId,
        hash: impl Fn(&[u8]) -> Hex32,
    ) -> Result<Option<InstructionPendingReview>, InstructionStateError> {
        let Some(watermark) = self.watermark(card, hash)? else {
            return Ok(None);
        };
        let mut review = InstructionPendingReview {
            watermark,
            pending_comparison: self.observation_pending(card),
            intervals: Vec::new(),
            changed_paths: Vec::new(),
            gaps: Vec::new(),
        };
        for (_, attempt) in self.review_attempts(card) {
            if let Some(comparison) = comparison(attempt) {
                for change in &comparison.changes {
                    review.changed_paths.push(InstructionChangedPath {
                        path: change.path.clone(),
                        context: change.context,
                    });
                    if !review_fits(&review) {
                        let _ = review.changed_paths.pop();
                        overflow(&mut review);
                    }
                }
            }
        }
        for (_, attempt) in self.review_attempts(card) {
            let Some(ticket) = attempt.ticket else {
                continue;
            };
            let (sampled_at, recovery) = sample_time(attempt);
            review.intervals.push(InstructionReviewInterval {
                owner: attempt.owner,
                baseline: attempt.baseline,
                ticket,
                finished_lsn: attempt.finished.as_ref().map(|finished| finished.lsn),
                end: self.attempt_end(attempt),
                sampled_at,
                recovery,
            });
            if !review_fits(&review) {
                review.intervals.pop();
                overflow(&mut review);
            }
            if let Some(comparison) = comparison(attempt) {
                for gap in &comparison.gaps {
                    review.gaps.push(gap.clone());
                    if !review_fits(&review) {
                        review.gaps.pop();
                        overflow(&mut review);
                    }
                }
            }
        }
        review.validate()?;
        Ok(Some(review))
    }

    fn review_attempts(&self, card: CardId) -> impl Iterator<Item = (&Lsn, &InstructionAttempt)> {
        self.by_card
            .get(&card)
            .into_iter()
            .flatten()
            .filter_map(|key| self.attempts.get_key_value(key))
    }

    fn review_valid(
        &self,
        card: CardId,
        expected: Seq,
        watermark: InstructionReviewWatermark,
        action: &InstructionReviewAction,
        authority: ReviewAuthority,
        hash: impl Fn(&[u8]) -> Hex32,
    ) -> Result<(), InstructionStateError> {
        if let ReviewAuthority::Current(facts) = authority {
            let facts = facts.ok_or(InstructionStateError::StaleCard)?;
            if facts.card != card || facts.seq != expected {
                return Err(InstructionStateError::StaleCard);
            }
            match action {
                InstructionReviewAction::Accept {
                    completion: InstructionAcceptCompletion::Completed { completion_lsn },
                } if facts.goal.is_none() || facts.completed_lsn != Some(*completion_lsn) => {
                    return Err(InstructionStateError::Completion);
                }
                InstructionReviewAction::Accept {
                    completion: InstructionAcceptCompletion::Continue,
                } if facts.completed_lsn.is_some() => {
                    return Err(InstructionStateError::Completion);
                }
                InstructionReviewAction::ChangesRequested { clear_goal, .. }
                    if *clear_goal != facts.clear_goal =>
                {
                    return Err(InstructionStateError::Completion);
                }
                _ => {}
            }
        }
        if self.observation_pending(card) {
            return Err(InstructionStateError::PendingComparison);
        }
        if self.watermark(card, hash)? != Some(watermark) {
            return Err(InstructionStateError::StaleReview);
        }
        Ok(())
    }

    fn boundary_valid(
        &self,
        lsn: Lsn,
        owner: InstructionObservationOwner,
        started: Lsn,
        proof: &InstructionBoundaryAbsenceProof,
    ) -> Result<(), InstructionStateError> {
        let attempt = self
            .attempts
            .get(&started)
            .ok_or(InstructionStateError::Boundary)?;
        if started >= lsn
            || attempt.baseline != (InstructionBaselineRef::Started { lsn: started })
            || attempt.owner != owner
        {
            return Err(InstructionStateError::Boundary);
        }
        if let Some(settled) = &attempt.boundary_settled {
            return if settled.proof == *proof {
                Ok(())
            } else {
                Err(InstructionStateError::ConflictingBoundarySettlement)
            };
        }
        match proof {
            InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent { receipt } => {
                let receipt_lsn = match receipt {
                    InstructionNativeBoundaryReceipt::Spawned { lsn }
                    | InstructionNativeBoundaryReceipt::LaunchFailureEnded { lsn } => *lsn,
                };
                if receipt_lsn >= lsn
                    || self.native_receipts.get(&receipt_lsn) != Some(&(started, *receipt))
                    || match receipt {
                        InstructionNativeBoundaryReceipt::Spawned { lsn } => {
                            attempt.spawn_lsn != Some(*lsn)
                        }
                        InstructionNativeBoundaryReceipt::LaunchFailureEnded { .. } => {
                            attempt.spawn_lsn.is_some()
                        }
                    }
                {
                    return Err(InstructionStateError::Boundary);
                }
            }
            InstructionBoundaryAbsenceProof::PriorBootAbsent { previous, current } => {
                if attempt
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.boot.as_ref())
                    != Some(previous)
                    || previous == current
                {
                    return Err(InstructionStateError::Boundary);
                }
            }
        }
        Ok(())
    }

    fn start_valid(&self, owner: InstructionObservationOwner) -> Result<(), InstructionStateError> {
        if matches!(owner, InstructionObservationOwner::Managed { turn, .. } if self.current_turns.contains_key(&turn) || self.ends.contains_key(&turn))
            || matches!(owner, InstructionObservationOwner::AttachedProcess { attachment_lsn, .. } if self.current_attachments.contains_key(&attachment_lsn))
        {
            return Err(InstructionStateError::Boundary);
        }
        match owner {
            InstructionObservationOwner::Managed {
                agent,
                turn,
                issue: InstructionIssueAssociation::Issued { issue_lsn },
            } if self.issues.get(&turn)
                != Some(&Issue {
                    agent,
                    lsn: issue_lsn,
                }) =>
            {
                Err(InstructionStateError::Boundary)
            }
            InstructionObservationOwner::Managed {
                turn,
                issue: InstructionIssueAssociation::Prospective,
                ..
            } if self.issues.contains_key(&turn) => Err(InstructionStateError::Boundary),
            InstructionObservationOwner::AttachedProcess {
                agent,
                attachment_lsn,
            } if self.attachments.get(&attachment_lsn) != Some(&agent) => {
                Err(InstructionStateError::Boundary)
            }
            InstructionObservationOwner::AttachedTurn { .. } => {
                Err(InstructionStateError::Boundary)
            }
            _ => Ok(()),
        }
    }

    fn finish_valid(
        &self,
        lsn: Lsn,
        owner: InstructionObservationOwner,
        ticket: Option<InstructionTicketAssociation>,
        baseline: InstructionBaselineRef,
        settlement: &InstructionSettlement,
    ) -> Result<(), InstructionStateError> {
        if let Some(attempt) = self.attempts.get(&baseline_key(baseline)) {
            if attempt.baseline != baseline || attempt.owner != owner || attempt.ticket != ticket {
                return Err(InstructionStateError::Boundary);
            }
            if let Some(finished) = &attempt.finished {
                return if finished.settlement == *settlement {
                    Ok(())
                } else {
                    Err(InstructionStateError::ConflictingSettlement)
                };
            }
        } else {
            match baseline {
                InstructionBaselineRef::MissingTurnBoundary { .. } => {}
                InstructionBaselineRef::MissingIssueBoundary { turn, issue_lsn } => {
                    self.missing_issue_valid(owner, turn, issue_lsn)?;
                }
                InstructionBaselineRef::Started { .. } => {
                    return Err(InstructionStateError::Boundary);
                }
            }
            self.ticket_valid(ticket)?;
        }
        if baseline_key(baseline) >= lsn {
            return Err(InstructionStateError::Boundary);
        }
        match settlement {
            InstructionSettlement::NoIssuedTurn => {
                if let InstructionObservationOwner::Managed { turn, .. } = owner
                    && self.issues.contains_key(&turn)
                {
                    return Err(InstructionStateError::Boundary);
                }
            }
            InstructionSettlement::Compared { end, .. }
            | InstructionSettlement::Unknown { end: Some(end), .. } => {
                self.check_end(owner, *end)?;
                if matches!(baseline, InstructionBaselineRef::Started { .. })
                    && end_position(*end) <= baseline_key(baseline)
                {
                    return Err(InstructionStateError::Boundary);
                }
            }
            InstructionSettlement::NoProcess { proof_lsn, .. } => {
                self.no_host_valid(lsn, owner, baseline, *proof_lsn)?;
            }
            InstructionSettlement::Unknown { end: None, .. } => {}
        }
        Ok(())
    }

    fn missing_issue_valid(
        &self,
        owner: InstructionObservationOwner,
        turn: TurnId,
        issue_lsn: Lsn,
    ) -> Result<(), InstructionStateError> {
        if self.issues.get(&turn)
            != Some(&Issue {
                agent: owner_agent(owner),
                lsn: issue_lsn,
            })
            || self.ends.contains_key(&turn)
            || self.current_turns.contains_key(&turn)
            || self.has_attempt_for_turn(turn)
        {
            return Err(InstructionStateError::Boundary);
        }
        Ok(())
    }

    fn no_host_valid(
        &self,
        lsn: Lsn,
        owner: InstructionObservationOwner,
        baseline: InstructionBaselineRef,
        proof_lsn: Lsn,
    ) -> Result<(), InstructionStateError> {
        let (
            InstructionObservationOwner::Managed { agent, turn, .. },
            InstructionBaselineRef::Started { lsn: started },
        ) = (owner, baseline)
        else {
            return Err(InstructionStateError::Boundary);
        };
        let corroborated = self.no_host_holds.get(&proof_lsn) == Some(&agent)
            || (self.managed_end_lsn(agent, turn) == Some(proof_lsn)
                && self.native_receipts.get(&proof_lsn)
                    == Some(&(
                        started,
                        InstructionNativeBoundaryReceipt::LaunchFailureEnded { lsn: proof_lsn },
                    )));
        if proof_lsn >= lsn
            || proof_lsn <= started
            || !corroborated
            || !self
                .issues
                .get(&turn)
                .is_some_and(|issue| issue.agent == agent && issue.lsn < proof_lsn)
            || self.current_turns.get(&turn) != Some(&started)
            || self
                .attempts
                .get(&started)
                .is_none_or(|attempt| attempt.spawn_lsn.is_some())
            || self
                .no_host_claims
                .get(&proof_lsn)
                .is_some_and(|claimed| *claimed != started)
        {
            return Err(InstructionStateError::Boundary);
        }
        Ok(())
    }

    fn ticket_valid(
        &self,
        ticket: Option<InstructionTicketAssociation>,
    ) -> Result<(), InstructionStateError> {
        if let Some(ticket) = ticket
            && !self.ticket_history.get(&ticket.goal).is_some_and(|known| {
                known.card == ticket.card && known.assignment_lsn == ticket.assignment_lsn
            })
        {
            return Err(InstructionStateError::Boundary);
        }
        Ok(())
    }

    fn check_end(
        &self,
        owner: InstructionObservationOwner,
        end: InstructionEndBoundary,
    ) -> Result<(), InstructionStateError> {
        if self.end_for(owner) == Some(end) {
            Ok(())
        } else {
            Err(InstructionStateError::Boundary)
        }
    }

    fn end_for(&self, owner: InstructionObservationOwner) -> Option<InstructionEndBoundary> {
        match owner {
            InstructionObservationOwner::Managed { agent, turn, .. } => self
                .managed_end_lsn(agent, turn)
                .map(|end_lsn| InstructionEndBoundary::ManagedTurn { turn, end_lsn }),
            InstructionObservationOwner::AttachedTurn { agent, turn, .. } => self
                .ends
                .get(&turn)
                .filter(|end| end.agent == agent && end.mode == Mode::Attached)
                .map(|end| InstructionEndBoundary::AttachedTurn {
                    turn,
                    end_lsn: end.lsn,
                }),
            InstructionObservationOwner::AttachedProcess { attachment_lsn, .. } => self
                .attachment_ends
                .get(&attachment_lsn)
                .map(|end_lsn| InstructionEndBoundary::AttachedProcess {
                    attachment_lsn,
                    end_lsn: *end_lsn,
                }),
        }
    }

    fn attempt_end(&self, attempt: &InstructionAttempt) -> Option<InstructionEndBoundary> {
        match attempt
            .finished
            .as_ref()
            .map(|finished| &finished.settlement)
        {
            Some(InstructionSettlement::Compared { end, .. }) => Some(*end),
            Some(InstructionSettlement::Unknown { end, .. }) => *end,
            Some(InstructionSettlement::NoIssuedTurn | InstructionSettlement::NoProcess { .. }) => {
                None
            }
            None => self.end_for(attempt.owner),
        }
    }

    fn fold_boundaries(&mut self, lsn: Lsn, record: &Record, folded: &mut InstructionFold) {
        if let Some((agent, turn)) = issue_identity(record) {
            self.issues.entry(turn).or_insert(Issue { agent, lsn });
            if let Some(&key) = self.current_turns.get(&turn)
                && let Some(attempt) = self.attempts.get(&key)
            {
                self.note_unresolved(attempt.owner, key);
            }
        }
        match record {
            Record::AgentHeld {
                agent,
                provenance,
                cause: crate::AutomaticHold::LaunchRefused { .. },
            } if provenance.is_observed() => {
                self.no_host_holds.insert(lsn, *agent);
            }
            Record::TurnEnded {
                agent,
                turn,
                mode,
                provenance,
                outcome,
                ..
            } if provenance.is_observed() => {
                if self.ends.contains_key(turn) {
                    return;
                }
                self.ends.insert(
                    *turn,
                    End {
                        agent: *agent,
                        mode: *mode,
                        lsn,
                    },
                );
                if let Some(&key) = self.current_turns.get(turn)
                    && let Some(attempt) = self.attempts.get(&key)
                {
                    note_card(&mut folded.evidence_cards, attempt.ticket);
                    if owner_agent(attempt.owner) == *agent
                        && matches!(
                            outcome,
                            Some(crate::record::TurnOutcome::NotLaunched { .. })
                        )
                    {
                        self.native_receipts.insert(
                            lsn,
                            (
                                key,
                                InstructionNativeBoundaryReceipt::LaunchFailureEnded { lsn },
                            ),
                        );
                    }
                }
            }
            Record::Attached { agent, .. } => {
                self.attachments.insert(lsn, *agent);
                self.latest_attachments.insert(*agent, lsn);
            }
            Record::TurnSpawned { agent, turn, .. } => {
                if let Some(&key) = self.current_turns.get(turn)
                    && let Some(attempt) = self.attempts.get_mut(&key)
                    && owner_agent(attempt.owner) == *agent
                {
                    attempt.spawn_lsn.get_or_insert(lsn);
                    self.native_receipts.insert(
                        lsn,
                        (key, InstructionNativeBoundaryReceipt::Spawned { lsn }),
                    );
                    let owner = attempt.owner;
                    self.clear_unresolved(owner, key);
                }
            }
            Record::PtySpawned { .. }
            | Record::PtyCloseRequested { .. }
            | Record::PtyCloseCompleted { .. } => self.fold_pty(lsn, record, folded),
            Record::TicketAssigned { .. }
            | Record::TicketStarted { .. }
            | Record::TicketRemoved { .. } => self.fold_ticket(lsn, record),
            _ => {}
        }
    }
    fn fold_ticket(&mut self, lsn: Lsn, record: &Record) {
        match record {
            Record::TicketAssigned { card, .. } => {
                self.assignments.insert(*card, lsn);
                self.remove_association(*card);
            }
            Record::TicketStarted {
                card,
                goal,
                expected_seq,
                ..
            } => {
                if let Some(&assignment_lsn) = self.assignments.get(card) {
                    self.remove_association(*card);
                    self.card_goals.insert(*card, goal.id);
                    self.associations.insert(
                        goal.id,
                        InstructionTicketAssociation {
                            card: *card,
                            goal: goal.id,
                            assignment_lsn,
                            seq_at_start: expected_seq.next(),
                        },
                    );
                    if let Some(association) = self.associations.get(&goal.id) {
                        self.ticket_history.insert(goal.id, *association);
                    }
                }
            }
            Record::TicketRemoved { card, .. } => {
                self.assignments.remove(card);
                self.remove_association(*card);
            }
            _ => {}
        }
    }

    fn fold_pty(&mut self, lsn: Lsn, record: &Record, folded: &mut InstructionFold) {
        match record {
            Record::PtySpawned { agent, pty, .. } => {
                if let Some(&attachment) = self.latest_attachments.get(agent) {
                    self.pty_attachments.insert(*pty, attachment);
                    if let Some(&key) = self.current_attachments.get(&attachment) {
                        if let Some(attempt) = self.attempts.get_mut(&key) {
                            attempt.spawn_lsn.get_or_insert(lsn);
                        }
                        self.native_receipts.insert(
                            lsn,
                            (key, InstructionNativeBoundaryReceipt::Spawned { lsn }),
                        );
                        self.clear_unresolved(
                            InstructionObservationOwner::AttachedProcess {
                                agent: *agent,
                                attachment_lsn: attachment,
                            },
                            key,
                        );
                    }
                }
            }
            Record::PtyCloseRequested { pty, .. } => {
                if let Some(&attachment) = self.pty_attachments.get(pty) {
                    self.close_requests.insert(lsn, attachment);
                }
            }
            Record::PtyCloseCompleted {
                request_lsn,
                outcome:
                    crate::record::PtyCloseOutcome::Closed
                    | crate::record::PtyCloseOutcome::AlreadyClosed,
            } => {
                if let Some(&attachment) = self.close_requests.get(request_lsn) {
                    self.attachment_ends.entry(attachment).or_insert(lsn);
                    if let Some(&key) = self.current_attachments.get(&attachment)
                        && let Some(attempt) = self.attempts.get(&key)
                    {
                        note_card(&mut folded.evidence_cards, attempt.ticket);
                    }
                }
            }
            _ => {}
        }
    }
}

fn remove_member<K: Ord>(index: &mut BTreeMap<K, BTreeSet<Lsn>>, id: &K, key: Lsn) {
    if let Some(keys) = index.get_mut(id) {
        keys.remove(&key);
        if keys.is_empty() {
            index.remove(id);
        }
    }
}

fn baseline_key(baseline: InstructionBaselineRef) -> Lsn {
    match baseline {
        InstructionBaselineRef::Started { lsn } => lsn,
        InstructionBaselineRef::MissingTurnBoundary { end_lsn, .. } => end_lsn,
        InstructionBaselineRef::MissingIssueBoundary { issue_lsn, .. } => issue_lsn,
    }
}

fn end_position(end: InstructionEndBoundary) -> Lsn {
    match end {
        InstructionEndBoundary::ManagedTurn { end_lsn, .. }
        | InstructionEndBoundary::AttachedTurn { end_lsn, .. }
        | InstructionEndBoundary::AttachedProcess { end_lsn, .. } => end_lsn,
    }
}

fn owner_agent(owner: InstructionObservationOwner) -> AgentId {
    match owner {
        InstructionObservationOwner::Managed { agent, .. }
        | InstructionObservationOwner::AttachedProcess { agent, .. }
        | InstructionObservationOwner::AttachedTurn { agent, .. } => agent,
    }
}

fn comparison(attempt: &InstructionAttempt) -> Option<&crate::instruction::InstructionComparison> {
    match &attempt.finished.as_ref()?.settlement {
        InstructionSettlement::Compared { comparison, .. }
        | InstructionSettlement::Unknown { comparison, .. } => Some(comparison),
        _ => None,
    }
}

fn requires_review(attempt: &InstructionAttempt) -> bool {
    if comparison(attempt)
        .is_some_and(|comparison| comparison.has_shared_changes() || comparison.has_shared_gaps())
    {
        return true;
    }
    matches!(
        &attempt.finished,
        Some(InstructionFinished {
            settlement: InstructionSettlement::Unknown {
                reason: InstructionUnknownReason::MissingBaseline
                    | InstructionUnknownReason::UnrecordedLaunchBoundary
                    | InstructionUnknownReason::CleanupUnconfirmed
                    | InstructionUnknownReason::AttachedTurnStartMissing,
                ..
            },
            ..
        })
    )
}

fn sample_time(attempt: &InstructionAttempt) -> (Option<Timestamp>, bool) {
    match attempt
        .finished
        .as_ref()
        .map(|finished| &finished.settlement)
    {
        Some(InstructionSettlement::Compared {
            sampled_at,
            recovery,
            ..
        }) => (Some(*sampled_at), *recovery),
        Some(InstructionSettlement::Unknown {
            sampled_at,
            recovery,
            ..
        }) => (*sampled_at, *recovery),
        _ => (
            attempt
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.sampled_at),
            false,
        ),
    }
}

fn note_card(cards: &mut Vec<CardId>, ticket: Option<InstructionTicketAssociation>) {
    if let Some(ticket) = ticket
        && !cards.contains(&ticket.card)
    {
        cards.push(ticket.card);
    }
}

fn issue_identity(record: &Record) -> Option<(AgentId, TurnId)> {
    match record {
        Record::TurnIssued {
            agent,
            turn,
            mode: Mode::Managed,
            ..
        }
        | Record::NoteOnlyTurnIssued {
            agent,
            turn,
            mode: Mode::Managed,
            ..
        }
        | Record::TurnIssuedWithBrowserNote {
            agent,
            turn,
            mode: Mode::Managed,
            ..
        }
        | Record::TaughtNoteOnlyTurnIssued {
            agent,
            turn,
            mode: Mode::Managed,
            ..
        }
        | Record::ConversationTurnIssued { agent, turn, .. } => Some((*agent, *turn)),
        Record::MailWakeOnlyTurnIssued(issue) => Some((issue.agent, issue.turn)),
        _ => None,
    }
}

fn review_fits(review: &InstructionPendingReview) -> bool {
    review.intervals.len() <= MAX_INSTRUCTION_INPUTS
        && review.changed_paths.len() <= MAX_INSTRUCTION_INPUTS
        && review.gaps.len() < MAX_INSTRUCTION_INPUTS
        && serde_json::to_vec(review)
            .is_ok_and(|bytes| bytes.len() <= MAX_INSTRUCTION_METADATA_BYTES - 1_280)
}

fn overflow(review: &mut InstructionPendingReview) {
    if !review
        .gaps
        .iter()
        .any(|gap| gap.reason == InstructionGapKind::MetadataLimit)
    {
        review.gaps.push(InstructionCoverageGap::metadata_limit());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instruction::{
        InstructionBootIdentity, InstructionComparison, InstructionPathContext,
    };
    use crate::record::{Actor, RecordKind, TurnOutcome, samples};

    fn hash(bytes: &[u8]) -> Hex32 {
        let mut value = [0_u8; 32];
        for (index, byte) in bytes.iter().enumerate() {
            value[index % 32] = value[index % 32].wrapping_mul(31).wrapping_add(*byte);
        }
        Hex32::new(value)
    }

    fn stamp(record: Record) -> Entry {
        Entry {
            at: Timestamp::UNIX_EPOCH,
            record,
        }
    }

    fn ticket() -> InstructionTicketAssociation {
        InstructionTicketAssociation {
            card: samples::card(),
            goal: samples::goal(),
            assignment_lsn: Lsn(2),
            seq_at_start: Seq(2),
        }
    }

    fn seeded() -> InstructionState {
        let mut state = InstructionState::default();
        for (index, kind) in [
            RecordKind::TicketCreated,
            RecordKind::TicketAssigned,
            RecordKind::TicketStarted,
        ]
        .into_iter()
        .enumerate()
        {
            state
                .apply(
                    Lsn(u64::try_from(index).unwrap() + 1),
                    &stamp(samples::record(kind)),
                    None,
                    hash,
                )
                .unwrap();
        }
        state
    }

    fn owner(issue: InstructionIssueAssociation) -> InstructionObservationOwner {
        InstructionObservationOwner::Managed {
            agent: samples::agent(),
            turn: samples::turn(),
            issue,
        }
    }

    fn start(issue: InstructionIssueAssociation) -> Record {
        let Record::InstructionObservationStarted { snapshot, .. } =
            samples::record(RecordKind::InstructionObservationStarted)
        else {
            unreachable!()
        };
        Record::InstructionObservationStarted {
            owner: owner(issue),
            ticket: Some(ticket()),
            snapshot,
        }
    }

    fn unknown(issue: InstructionIssueAssociation, started: u64) -> Record {
        Record::InstructionObservationFinished {
            owner: owner(issue),
            ticket: Some(ticket()),
            baseline: InstructionBaselineRef::Started { lsn: Lsn(started) },
            settlement: InstructionSettlement::Unknown {
                reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                end: None,
                sampled_at: None,
                recovery: true,
                comparison: InstructionComparison {
                    changes: Vec::new(),
                    gaps: vec![InstructionCoverageGap {
                        reason: InstructionGapKind::UnrecordedLaunchBoundary,
                        context: InstructionPathContext::SharedProject,
                        agent: Some(samples::agent()),
                        path: None,
                    }],
                    usage: crate::InstructionSampleUsage::default(),
                },
            },
        }
    }

    fn reviewed(state: &InstructionState, seq: Seq) -> Record {
        Record::TicketInstructionReviewed {
            card: samples::card(),
            by: Actor::Peer {
                pid: 77,
                uid: Some(501),
            },
            expected_seq: seq,
            watermark: state.watermark(samples::card(), hash).unwrap().unwrap(),
            action: InstructionReviewAction::Accept {
                completion: InstructionAcceptCompletion::Continue,
            },
        }
    }

    fn facts(seq: Seq) -> InstructionReviewFacts {
        InstructionReviewFacts {
            card: samples::card(),
            seq,
            goal: Some(samples::goal()),
            completed_lsn: None,
            clear_goal: None,
        }
    }

    #[test]
    fn running_baseline_does_not_hold_work_but_an_ended_comparison_does() {
        let mut state = seeded();
        let began = state
            .apply(
                Lsn(4),
                &stamp(start(InstructionIssueAssociation::Prospective)),
                None,
                hash,
            )
            .unwrap();
        assert_eq!(began.evidence_cards, vec![samples::card()]);
        assert!(!state.pending_for_goal(samples::goal()));
        assert_eq!(state.ticket_reason(samples::card()), None);
        state
            .apply(
                Lsn(5),
                &stamp(samples::record(RecordKind::TurnIssued)),
                None,
                hash,
            )
            .unwrap();
        assert_eq!(state.issue_lsn(samples::turn()), Some(Lsn(5)));
        state
            .apply(
                Lsn(6),
                &stamp(samples::record(RecordKind::TurnEnded)),
                None,
                hash,
            )
            .unwrap();
        assert!(state.pending_for_goal(samples::goal()));
        assert_eq!(state.end_lsn(samples::turn()), Some(Lsn(6)));
        let finished = Record::InstructionObservationFinished {
            owner: owner(InstructionIssueAssociation::Prospective),
            ticket: Some(ticket()),
            baseline: InstructionBaselineRef::Started { lsn: Lsn(4) },
            settlement: InstructionSettlement::Compared {
                end: InstructionEndBoundary::ManagedTurn {
                    turn: samples::turn(),
                    end_lsn: Lsn(6),
                },
                sampled_at: Timestamp::UNIX_EPOCH,
                recovery: false,
                comparison: InstructionComparison::default(),
            },
        };
        state.apply(Lsn(7), &stamp(finished), None, hash).unwrap();
        assert!(!state.pending_for_goal(samples::goal()));
        assert_eq!(state.review(samples::card(), hash).unwrap(), None);
        assert_eq!(
            state.unresolved_launch_attempts(samples::agent()).count(),
            1,
            "comparison is not native absence proof"
        );
    }

    #[test]
    fn distinct_attempts_and_accepted_review_preserve_old_launch_uncertainty() {
        let mut state = seeded();
        state
            .apply(
                Lsn(4),
                &stamp(start(InstructionIssueAssociation::Prospective)),
                None,
                hash,
            )
            .unwrap();
        state
            .apply(
                Lsn(5),
                &stamp(samples::record(RecordKind::TurnIssued)),
                None,
                hash,
            )
            .unwrap();
        let terminal = unknown(InstructionIssueAssociation::Prospective, 4);
        state
            .apply(Lsn(6), &stamp(terminal.clone()), None, hash)
            .unwrap();
        let receipt = reviewed(&state, Seq(4));
        state
            .apply(Lsn(7), &stamp(receipt), Some(facts(Seq(4))), hash)
            .unwrap();
        assert!(!state.pending_for_goal(samples::goal()));
        assert_eq!(
            state.unresolved_launch_attempts(samples::agent()).count(),
            1
        );
        let retry = InstructionIssueAssociation::Issued { issue_lsn: Lsn(5) };
        state
            .apply(Lsn(8), &stamp(start(retry)), None, hash)
            .unwrap();
        state
            .apply(
                Lsn(9),
                &stamp(samples::record(RecordKind::TurnSpawned)),
                None,
                hash,
            )
            .unwrap();
        assert_eq!(state.attempts().count(), 2);
        assert_eq!(state.current_for_turn(samples::turn()).unwrap().0, Lsn(8));
        let wrong = Record::InstructionAttemptBoundarySettled {
            owner: owner(InstructionIssueAssociation::Prospective),
            started_lsn: Lsn(4),
            proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                receipt: InstructionNativeBoundaryReceipt::Spawned { lsn: Lsn(9) },
            },
        };
        assert_eq!(
            state.apply(Lsn(10), &stamp(wrong), None, hash),
            Err(InstructionStateError::Boundary)
        );
        assert_eq!(
            state.unresolved_launch_attempts(samples::agent()).count(),
            1
        );
        let duplicate = state.apply(Lsn(11), &stamp(terminal), None, hash).unwrap();
        assert!(duplicate.evidence_cards.is_empty());
        assert_eq!(
            state
                .attempts()
                .next()
                .unwrap()
                .1
                .finished
                .as_ref()
                .unwrap()
                .lsn,
            Lsn(6)
        );
    }

    #[test]
    fn stale_card_stale_set_pending_and_conflicting_terminal_results_refuse_without_mutation() {
        let mut state = seeded();
        state
            .apply(
                Lsn(4),
                &stamp(start(InstructionIssueAssociation::Prospective)),
                None,
                hash,
            )
            .unwrap();
        let receipt = reviewed(&state, Seq(3));
        let before = state.clone();
        assert_eq!(
            state.apply(Lsn(5), &stamp(receipt.clone()), Some(facts(Seq(9))), hash),
            Err(InstructionStateError::StaleCard)
        );
        assert_eq!(state, before);
        assert_eq!(
            state.apply(Lsn(5), &stamp(receipt), Some(facts(Seq(3))), hash),
            Err(InstructionStateError::PendingComparison)
        );
        assert_eq!(state, before);
        state
            .apply(
                Lsn(5),
                &stamp(samples::record(RecordKind::TurnIssued)),
                None,
                hash,
            )
            .unwrap();
        state
            .apply(
                Lsn(6),
                &stamp(unknown(InstructionIssueAssociation::Prospective, 4)),
                None,
                hash,
            )
            .unwrap();
        let mut receipt = reviewed(&state, Seq(4));
        if let Record::TicketInstructionReviewed { watermark, .. } = &mut receipt {
            watermark.set_digest = Hex32::new([0; 32]);
        }
        let before = state.clone();
        assert_eq!(
            state.apply(Lsn(7), &stamp(receipt), Some(facts(Seq(4))), hash),
            Err(InstructionStateError::StaleReview)
        );
        assert_eq!(state, before);
        let conflicting = Record::InstructionObservationFinished {
            owner: owner(InstructionIssueAssociation::Prospective),
            ticket: Some(ticket()),
            baseline: InstructionBaselineRef::Started { lsn: Lsn(4) },
            settlement: InstructionSettlement::NoProcess {
                proof_lsn: Lsn(5),
                proof: crate::InstructionNoProcessProof::NoHostCall,
            },
        };
        assert_eq!(
            state.apply(Lsn(7), &stamp(conflicting), None, hash),
            Err(InstructionStateError::ConflictingSettlement)
        );
        assert_eq!(state, before);
    }

    #[test]
    fn noissued_orphan_and_wrong_end_do_not_become_unchanged() {
        let mut state = seeded();
        state
            .apply(
                Lsn(4),
                &stamp(start(InstructionIssueAssociation::Prospective)),
                None,
                hash,
            )
            .unwrap();
        let absent = Record::InstructionObservationFinished {
            owner: owner(InstructionIssueAssociation::Prospective),
            ticket: Some(ticket()),
            baseline: InstructionBaselineRef::Started { lsn: Lsn(4) },
            settlement: InstructionSettlement::NoIssuedTurn,
        };
        state.apply(Lsn(5), &stamp(absent), None, hash).unwrap();
        assert_eq!(state.unfinished().count(), 0);
        assert_eq!(
            state.unresolved_launch_attempts(samples::agent()).count(),
            0
        );
        let mut state = seeded();
        state
            .apply(
                Lsn(4),
                &stamp(start(InstructionIssueAssociation::Prospective)),
                None,
                hash,
            )
            .unwrap();
        let wrong = Record::InstructionObservationFinished {
            owner: owner(InstructionIssueAssociation::Prospective),
            ticket: Some(ticket()),
            baseline: InstructionBaselineRef::Started { lsn: Lsn(4) },
            settlement: InstructionSettlement::Compared {
                end: InstructionEndBoundary::ManagedTurn {
                    turn: samples::turn(),
                    end_lsn: Lsn(5),
                },
                sampled_at: Timestamp::UNIX_EPOCH,
                recovery: false,
                comparison: InstructionComparison::default(),
            },
        };
        assert_eq!(
            state.apply(Lsn(6), &stamp(wrong), None, hash),
            Err(InstructionStateError::Boundary)
        );
    }

    #[test]
    fn observed_notlaunched_receipt_settles_only_its_native_boundary_and_keeps_comparison() {
        for outcome in [
            TurnOutcome::NotLaunched {
                reason: "confirmed scoped cleanup".into(),
            },
            TurnOutcome::Vanished,
            TurnOutcome::Failed {
                reason: "failed".into(),
            },
            TurnOutcome::LaunchRefused {
                reason: "refused".into(),
            },
        ] {
            let mut state = seeded();
            state
                .apply(
                    Lsn(4),
                    &stamp(start(InstructionIssueAssociation::Prospective)),
                    None,
                    hash,
                )
                .unwrap();
            state
                .apply(
                    Lsn(5),
                    &stamp(samples::record(RecordKind::TurnIssued)),
                    None,
                    hash,
                )
                .unwrap();
            let mut ended = samples::record(RecordKind::TurnEnded);
            if let Record::TurnEnded {
                outcome: recorded, ..
            } = &mut ended
            {
                *recorded = Some(outcome.clone());
            }
            state.apply(Lsn(6), &stamp(ended), None, hash).unwrap();
            state
                .apply(
                    Lsn(7),
                    &stamp(unknown(InstructionIssueAssociation::Prospective, 4)),
                    None,
                    hash,
                )
                .unwrap();
            let receipt = Record::InstructionAttemptBoundarySettled {
                owner: owner(InstructionIssueAssociation::Prospective),
                started_lsn: Lsn(4),
                proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                    receipt: InstructionNativeBoundaryReceipt::LaunchFailureEnded { lsn: Lsn(6) },
                },
            };
            let result = state.apply(Lsn(8), &stamp(receipt.clone()), None, hash);
            if matches!(outcome, TurnOutcome::NotLaunched { .. }) {
                assert!(result.is_ok());
                assert_eq!(
                    state.unresolved_launch_attempts(samples::agent()).count(),
                    0
                );
                assert_eq!(
                    state.ticket_reason(samples::card()),
                    Some(TicketReason::SharedInstructionsUnknown)
                );
                state.apply(Lsn(9), &stamp(receipt), None, hash).unwrap();
                assert_eq!(
                    state
                        .attempts()
                        .next()
                        .unwrap()
                        .1
                        .boundary_settled
                        .as_ref()
                        .unwrap()
                        .lsn,
                    Lsn(8)
                );
            } else {
                assert_eq!(result, Err(InstructionStateError::Boundary));
            }
        }
    }

    #[test]
    fn prior_boot_proof_requires_exact_baseline_and_a_different_supported_boot() {
        let mut state = seeded();
        let previous = InstructionBootIdentity::LinuxKernelBootId {
            value: "boot-a".into(),
        };
        let current = InstructionBootIdentity::LinuxKernelBootId {
            value: "boot-b".into(),
        };
        let mut began = start(InstructionIssueAssociation::Prospective);
        if let Record::InstructionObservationStarted { snapshot, .. } = &mut began {
            snapshot.boot = Some(previous.clone());
        }
        state.apply(Lsn(4), &stamp(began), None, hash).unwrap();
        state
            .apply(
                Lsn(5),
                &stamp(samples::record(RecordKind::TurnIssued)),
                None,
                hash,
            )
            .unwrap();
        state
            .apply(
                Lsn(6),
                &stamp(unknown(InstructionIssueAssociation::Prospective, 4)),
                None,
                hash,
            )
            .unwrap();
        let receipt = Record::InstructionAttemptBoundarySettled {
            owner: owner(InstructionIssueAssociation::Prospective),
            started_lsn: Lsn(4),
            proof: InstructionBoundaryAbsenceProof::PriorBootAbsent {
                previous: previous.clone(),
                current: current.clone(),
            },
        };
        state.apply(Lsn(7), &stamp(receipt), None, hash).unwrap();
        assert_eq!(
            state.unresolved_launch_attempts(samples::agent()).count(),
            0
        );
        assert!(state.pending_for_goal(samples::goal()));
        let wrong = Record::InstructionAttemptBoundarySettled {
            owner: owner(InstructionIssueAssociation::Prospective),
            started_lsn: Lsn(4),
            proof: InstructionBoundaryAbsenceProof::PriorBootAbsent {
                previous: current,
                current: previous,
            },
        };
        assert_eq!(
            state.apply(Lsn(8), &stamp(wrong), None, hash),
            Err(InstructionStateError::ConflictingBoundarySettlement)
        );
    }

    fn no_host_finished(started: u64, proof: u64) -> Record {
        Record::InstructionObservationFinished {
            owner: owner(InstructionIssueAssociation::Prospective),
            ticket: Some(ticket()),
            baseline: InstructionBaselineRef::Started { lsn: Lsn(started) },
            settlement: InstructionSettlement::NoProcess {
                proof_lsn: Lsn(proof),
                proof: crate::InstructionNoProcessProof::NoHostCall,
            },
        }
    }

    fn refusal(agent: AgentId, provenance: dudes_types::Provenance) -> Record {
        Record::AgentHeld {
            agent,
            provenance,
            cause: crate::AutomaticHold::LaunchRefused {
                reason: "admission refused".into(),
            },
        }
    }

    #[test]
    fn no_host_assertion_requires_a_matching_observed_refusal_after_baseline_and_issue() {
        use dudes_types::Provenance;
        for (hold, proof, spawn, valid) in [
            (
                refusal(samples::agent(), Provenance::Observed),
                6,
                false,
                true,
            ),
            (
                refusal(AgentId::from_parts(1, 77), Provenance::Observed),
                6,
                false,
                false,
            ),
            (
                refusal(samples::agent(), Provenance::Inferred),
                6,
                false,
                false,
            ),
            (
                refusal(samples::agent(), Provenance::Observed),
                5,
                false,
                false,
            ),
            (
                refusal(samples::agent(), Provenance::Observed),
                6,
                true,
                false,
            ),
        ] {
            let mut state = seeded();
            state
                .apply(
                    Lsn(4),
                    &stamp(start(InstructionIssueAssociation::Prospective)),
                    None,
                    hash,
                )
                .unwrap();
            state
                .apply(
                    Lsn(5),
                    &stamp(samples::record(RecordKind::TurnIssued)),
                    None,
                    hash,
                )
                .unwrap();
            state.apply(Lsn(6), &stamp(hold), None, hash).unwrap();
            if spawn {
                state
                    .apply(
                        Lsn(7),
                        &stamp(samples::record(RecordKind::TurnSpawned)),
                        None,
                        hash,
                    )
                    .unwrap();
            }
            let before = state.clone();
            let finish = stamp(no_host_finished(4, proof));
            let result = state.apply(Lsn(8), &finish, None, hash);
            if valid {
                assert!(result.is_ok());
                assert_eq!(
                    state.unresolved_launch_attempts(samples::agent()).count(),
                    0
                );
                assert!(!state.pending_for_goal(samples::goal()));
                state.apply(Lsn(9), &finish, None, hash).unwrap();
                assert_eq!(
                    state.attempts[&Lsn(4)].finished.as_ref().unwrap().lsn,
                    Lsn(8)
                );
            } else {
                assert_eq!(result, Err(InstructionStateError::Boundary));
                assert_eq!(state, before);
            }
        }
    }

    #[test]
    fn old_refusal_and_crash_after_hold_do_not_prove_no_process() {
        let mut state = seeded();
        state
            .apply(
                Lsn(4),
                &stamp(refusal(samples::agent(), dudes_types::Provenance::Observed)),
                None,
                hash,
            )
            .unwrap();
        state
            .apply(
                Lsn(5),
                &stamp(start(InstructionIssueAssociation::Prospective)),
                None,
                hash,
            )
            .unwrap();
        state
            .apply(
                Lsn(6),
                &stamp(samples::record(RecordKind::TurnIssued)),
                None,
                hash,
            )
            .unwrap();
        assert_eq!(
            state.apply(Lsn(7), &stamp(no_host_finished(5, 4)), None, hash),
            Err(InstructionStateError::Boundary)
        );
        state
            .apply(
                Lsn(8),
                &stamp(refusal(samples::agent(), dudes_types::Provenance::Observed)),
                None,
                hash,
            )
            .unwrap();
        assert_eq!(state.unfinished().count(), 1);
        assert_eq!(
            state.unresolved_launch_attempts(samples::agent()).count(),
            1
        );
        state
            .apply(
                Lsn(9),
                &stamp(unknown(InstructionIssueAssociation::Prospective, 5)),
                None,
                hash,
            )
            .unwrap();
        assert_eq!(
            state.unresolved_launch_attempts(samples::agent()).count(),
            1
        );
        assert_eq!(
            state.ticket_reason(samples::card()),
            Some(TicketReason::SharedInstructionsUnknown)
        );
    }

    #[test]
    fn one_refusal_cannot_corroborate_two_physical_attempts() {
        let mut state = seeded();
        let second_turn = TurnId::from_parts(1, 99);
        state
            .apply(
                Lsn(4),
                &stamp(start(InstructionIssueAssociation::Prospective)),
                None,
                hash,
            )
            .unwrap();
        state
            .apply(
                Lsn(5),
                &stamp(samples::record(RecordKind::TurnIssued)),
                None,
                hash,
            )
            .unwrap();
        let mut second_start = start(InstructionIssueAssociation::Prospective);
        if let Record::InstructionObservationStarted {
            owner: InstructionObservationOwner::Managed { turn, .. },
            ..
        } = &mut second_start
        {
            *turn = second_turn;
        }
        state
            .apply(Lsn(6), &stamp(second_start), None, hash)
            .unwrap();
        let mut issue = samples::record(RecordKind::TurnIssued);
        if let Record::TurnIssued { turn, .. } = &mut issue {
            *turn = second_turn;
        }
        state.apply(Lsn(7), &stamp(issue), None, hash).unwrap();
        state
            .apply(
                Lsn(8),
                &stamp(refusal(samples::agent(), dudes_types::Provenance::Observed)),
                None,
                hash,
            )
            .unwrap();
        state
            .apply(Lsn(9), &stamp(no_host_finished(4, 8)), None, hash)
            .unwrap();
        let mut second_finish = no_host_finished(6, 8);
        if let Record::InstructionObservationFinished {
            owner: InstructionObservationOwner::Managed { turn, .. },
            ..
        } = &mut second_finish
        {
            *turn = second_turn;
        }
        let before = state.clone();
        assert_eq!(
            state.apply(Lsn(10), &stamp(second_finish), None, hash),
            Err(InstructionStateError::Boundary)
        );
        assert_eq!(state, before);
    }

    fn missing_issue(issue_lsn: Lsn) -> Record {
        let mut record = unknown(
            InstructionIssueAssociation::Issued { issue_lsn },
            issue_lsn.0,
        );
        if let Record::InstructionObservationFinished {
            baseline,
            settlement: InstructionSettlement::Unknown { comparison, .. },
            ..
        } = &mut record
        {
            *baseline = InstructionBaselineRef::MissingIssueBoundary {
                turn: samples::turn(),
                issue_lsn,
            };
            comparison.gaps.push(InstructionCoverageGap {
                reason: InstructionGapKind::MissingBaseline,
                context: InstructionPathContext::Unresolved,
                agent: Some(samples::agent()),
                path: None,
            });
        }
        record
    }

    #[test]
    fn legacy_missing_issue_survives_retry_end_review_and_repeated_replay() {
        let mut state = seeded();
        state
            .apply(
                Lsn(4),
                &stamp(samples::record(RecordKind::TurnIssued)),
                None,
                hash,
            )
            .unwrap();
        let missing = stamp(missing_issue(Lsn(4)));
        state.apply(Lsn(5), &missing, None, hash).unwrap();
        let path = crate::instruction::NativeInstructionPath::Utf8 {
            value: "/different-folder".into(),
        };
        assert_eq!(
            state.unresolved_in_folder(samples::agent(), &path).count(),
            1
        );
        assert!(state.attempt(Lsn(4)).unwrap().snapshot.is_none());
        assert!(state.current_for_turn(samples::turn()).is_none());
        let retry = InstructionIssueAssociation::Issued { issue_lsn: Lsn(4) };
        state
            .apply(Lsn(6), &stamp(start(retry)), None, hash)
            .unwrap();
        state
            .apply(
                Lsn(7),
                &stamp(samples::record(RecordKind::TurnSpawned)),
                None,
                hash,
            )
            .unwrap();
        state
            .apply(
                Lsn(8),
                &stamp(samples::record(RecordKind::TurnEnded)),
                None,
                hash,
            )
            .unwrap();
        let report = state.review(samples::card(), hash).unwrap().unwrap();
        assert_eq!(
            report.intervals[0].end, None,
            "a retry's end cannot become the legacy attempt's end"
        );
        state
            .apply(
                Lsn(9),
                &stamp(Record::InstructionObservationFinished {
                    owner: owner(retry),
                    ticket: Some(ticket()),
                    baseline: InstructionBaselineRef::Started { lsn: Lsn(6) },
                    settlement: InstructionSettlement::Compared {
                        end: InstructionEndBoundary::ManagedTurn {
                            turn: samples::turn(),
                            end_lsn: Lsn(8),
                        },
                        sampled_at: Timestamp::UNIX_EPOCH,
                        recovery: false,
                        comparison: InstructionComparison::default(),
                    },
                }),
                None,
                hash,
            )
            .unwrap();
        let review = stamp(reviewed(&state, Seq(20)));
        state
            .apply(Lsn(10), &review, Some(facts(Seq(20))), hash)
            .unwrap();
        assert_eq!(
            state.unresolved_launch_attempts(samples::agent()).count(),
            1
        );
        let duplicate = state.apply(Lsn(11), &missing, None, hash).unwrap();
        assert!(duplicate.evidence_cards.is_empty());
        assert_eq!(
            state
                .attempt(Lsn(4))
                .unwrap()
                .finished
                .as_ref()
                .unwrap()
                .lsn,
            Lsn(5)
        );
        assert!(state.review(samples::card(), hash).unwrap().is_none());
    }

    #[test]
    fn missing_issue_rejects_wrong_or_already_observed_boundaries() {
        for observed in [
            None,
            Some(RecordKind::TurnEnded),
            Some(RecordKind::InstructionObservationStarted),
        ] {
            let mut state = seeded();
            state
                .apply(
                    Lsn(4),
                    &stamp(samples::record(RecordKind::TurnIssued)),
                    None,
                    hash,
                )
                .unwrap();
            if let Some(kind) = observed {
                let record = if kind == RecordKind::InstructionObservationStarted {
                    start(InstructionIssueAssociation::Issued { issue_lsn: Lsn(4) })
                } else {
                    samples::record(kind)
                };
                state.apply(Lsn(5), &stamp(record), None, hash).unwrap();
            }
            let before = state.clone();
            let missing = missing_issue(if observed.is_some() { Lsn(4) } else { Lsn(3) });
            assert_eq!(
                state.apply(Lsn(6), &stamp(missing), None, hash),
                Err(InstructionStateError::Boundary)
            );
            assert_eq!(state, before);
        }
    }

    #[test]
    fn missing_issue_can_claim_only_unsampled_recovery_uncertainty() {
        let original = missing_issue(Lsn(4));
        for change in 0..7 {
            let mut record = original.clone();
            if let Record::InstructionObservationFinished { settlement, .. } = &mut record {
                if change == 0 {
                    *settlement = InstructionSettlement::NoProcess {
                        proof_lsn: Lsn(5),
                        proof: crate::InstructionNoProcessProof::NoHostCall,
                    };
                } else if let InstructionSettlement::Unknown {
                    reason,
                    end,
                    sampled_at,
                    recovery,
                    comparison,
                } = settlement
                {
                    match change {
                        1 => *sampled_at = Some(Timestamp::UNIX_EPOCH),
                        2 => *recovery = false,
                        3 => comparison
                            .gaps
                            .retain(|gap| gap.reason != InstructionGapKind::MissingBaseline),
                        4 => {
                            *end = Some(InstructionEndBoundary::ManagedTurn {
                                turn: samples::turn(),
                                end_lsn: Lsn(5),
                            });
                        }
                        5 => *reason = InstructionUnknownReason::IncompleteCoverage,
                        _ => comparison.usage.source_bytes = 1,
                    }
                }
            }
            assert!(validate_instruction_record(&record).is_err());
        }
        assert_eq!(validate_instruction_record(&original), Ok(()));
    }

    fn assert_exact_later_spawn_index(state: &InstructionState, agent: AgentId, turn: TurnId) {
        assert_eq!(
            state
                .later_managed_attempt_for_spawn(agent, turn, Lsn(3), Lsn(6))
                .map(|(started, _)| started),
            Some(Lsn(4))
        );
        assert!(
            state
                .later_managed_attempt_for_spawn(agent, turn, Lsn(4), Lsn(6))
                .is_none()
        );
        assert!(
            state
                .later_managed_attempt_for_spawn(AgentId::NIL, turn, Lsn(3), Lsn(6))
                .is_none()
        );
        assert!(
            state
                .later_managed_attempt_for_spawn(agent, TurnId::NIL, Lsn(3), Lsn(6))
                .is_none()
        );
        assert!(
            state
                .later_managed_attempt_for_spawn(agent, turn, Lsn(3), Lsn(7))
                .is_none()
        );
    }

    #[test]
    fn native_absence_must_name_the_attempts_first_spawn() {
        let mut state = seeded();
        state
            .apply(
                Lsn(4),
                &stamp(start(InstructionIssueAssociation::Prospective)),
                None,
                hash,
            )
            .unwrap();
        state
            .apply(
                Lsn(5),
                &stamp(samples::record(RecordKind::TurnIssued)),
                None,
                hash,
            )
            .unwrap();
        state
            .apply(
                Lsn(6),
                &stamp(samples::record(RecordKind::TurnSpawned)),
                None,
                hash,
            )
            .unwrap();
        let mut later = samples::record(RecordKind::TurnSpawned);
        if let Record::TurnSpawned {
            pid, start_time, ..
        } = &mut later
        {
            *pid += 1;
            *start_time += 1;
        }
        state.apply(Lsn(7), &stamp(later), None, hash).unwrap();
        let InstructionObservationOwner::Managed { agent, turn, .. } =
            owner(InstructionIssueAssociation::Prospective)
        else {
            unreachable!()
        };
        assert_exact_later_spawn_index(&state, agent, turn);
        let mut ended = samples::record(RecordKind::TurnEnded);
        if let Record::TurnEnded { outcome, .. } = &mut ended {
            *outcome = Some(crate::record::TurnOutcome::NotLaunched {
                reason: "another call was cleaned".into(),
            });
        }
        state.apply(Lsn(8), &stamp(ended), None, hash).unwrap();
        for receipt in [
            InstructionNativeBoundaryReceipt::Spawned { lsn: Lsn(7) },
            InstructionNativeBoundaryReceipt::LaunchFailureEnded { lsn: Lsn(8) },
        ] {
            let before = state.clone();
            let entry = stamp(Record::InstructionAttemptBoundarySettled {
                owner: owner(InstructionIssueAssociation::Prospective),
                started_lsn: Lsn(4),
                proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent { receipt },
            });
            assert_eq!(
                state.apply(Lsn(9), &entry, None, hash),
                Err(InstructionStateError::Boundary)
            );
            assert_eq!(state, before);
        }
        state
            .apply(
                Lsn(9),
                &stamp(Record::InstructionAttemptBoundarySettled {
                    owner: owner(InstructionIssueAssociation::Prospective),
                    started_lsn: Lsn(4),
                    proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                        receipt: InstructionNativeBoundaryReceipt::Spawned { lsn: Lsn(6) },
                    },
                }),
                None,
                hash,
            )
            .unwrap();
        assert_eq!(state.attempt(Lsn(4)).unwrap().spawn_lsn, Some(Lsn(6)));
        assert!(
            state
                .later_managed_attempt_for_spawn(agent, turn, Lsn(3), Lsn(6))
                .unwrap()
                .1
                .boundary_settled
                .is_some()
        );
    }

    #[test]
    fn recorded_ticket_association_retains_its_sequence_after_reassignment() {
        let mut state = seeded();
        let original = state
            .recorded_association_for_goal(samples::goal())
            .unwrap();
        assert_eq!(
            state
                .association_for_goal(samples::goal(), Seq(123))
                .unwrap()
                .seq_at_start,
            Seq(123)
        );
        state
            .apply(
                Lsn(4),
                &stamp(samples::record(RecordKind::TicketAssigned)),
                None,
                hash,
            )
            .unwrap();
        assert!(
            state
                .association_for_goal(samples::goal(), Seq(123))
                .is_none()
        );
        assert_eq!(
            state.recorded_association_for_goal(samples::goal()),
            Some(original)
        );
    }

    #[test]
    fn managed_end_lookup_requires_both_recorded_owner_and_mode() {
        for mode in [Mode::Managed, Mode::Attached] {
            let mut state = InstructionState::default();
            let mut end = samples::record(RecordKind::TurnEnded);
            if let Record::TurnEnded { mode: recorded, .. } = &mut end {
                *recorded = mode;
            }
            state.apply(Lsn(1), &stamp(end), None, hash).unwrap();
            assert_eq!(state.end_lsn(samples::turn()), Some(Lsn(1)));
            assert_eq!(
                state.managed_end_lsn(samples::agent(), samples::turn()),
                (mode == Mode::Managed).then_some(Lsn(1))
            );
            assert_eq!(
                state.managed_end_lsn(AgentId::from_parts(1, 99), samples::turn()),
                None
            );
        }
    }

    #[test]
    fn a_managed_baseline_cannot_restart_an_observably_ended_turn() {
        let mut state = seeded();
        state
            .apply(
                Lsn(4),
                &stamp(samples::record(RecordKind::TurnIssued)),
                None,
                hash,
            )
            .unwrap();
        state
            .apply(
                Lsn(5),
                &stamp(samples::record(RecordKind::TurnEnded)),
                None,
                hash,
            )
            .unwrap();
        let before = state.clone();
        let restarted = stamp(start(InstructionIssueAssociation::Issued {
            issue_lsn: Lsn(4),
        }));
        assert_eq!(
            state.apply(Lsn(6), &restarted, None, hash),
            Err(InstructionStateError::Boundary)
        );
        assert_eq!(state, before);
    }
}
