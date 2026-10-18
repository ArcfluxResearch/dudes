//! Pure episode exclusivity and permanent recovery receipt history.

pub mod admission;
pub mod cache;
pub mod history;
pub mod transition;

use std::collections::BTreeMap;
use std::fmt;

use dudes_types::{Lsn, SessionId, TurnId};
use serde::{Deserialize, Serialize};

use crate::record::Hex32;
use crate::recovery::{
    MAX_RECOVERY_RECAP_BYTES, RecoveryAuthorized, RecoveryBinding, RecoveryClaimRef,
    RecoveryContext, RecoveryEpisodeId, RecoveryInvalidationEvidence, RecoveryIssue,
    RecoveryIssued, RecoveryOrigin, RecoveryPlan, RecoveryReason, RecoveryRequired, RecoveryWork,
};

#[cfg(test)]
mod tests;

/// Actual positions retained by the existing turn fold, without a liveness conclusion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_field_names,
    reason = "lsn suffixes distinguish journal positions from physical receipts and session identities"
)]
pub struct RecoveryAttempt {
    /// Latest physical spawn, absent when no spawn was recorded.
    pub spawn_lsn: Option<Lsn>,
    /// Latest terminal record, which may precede a later spawn.
    pub end_lsn: Option<Lsn>,
    /// Accepted confirmation for the current attempt, absent after an unconfirmed reissue.
    pub confirmation_lsn: Option<Lsn>,
}

/// Existing turn-row facts for settling a physical receipt, without requiring a current goal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryAttemptFacts {
    /// Actual before-record prefix.
    pub through: Lsn,
    /// Immutable issue joined to the existing turn, independent of later goal changes.
    pub issue: RecoveryIssue,
    /// Actual preceding physical-attempt positions.
    pub attempt: RecoveryAttempt,
}

/// Facts from the same committed journal cut, before the proposed transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryFacts {
    /// Actual committed prefix position, including ordinary non-recovery records.
    pub through: Lsn,
    /// Current binding, derived from hire, goal, ticket and lease records.
    pub binding: RecoveryBinding,
    /// Exact source issue, including its immutable task witness and answers.
    pub source_issue: RecoveryIssue,
    /// Latest physical attempt positions, read from the existing turn owner.
    pub attempt: RecoveryAttempt,
    /// Current control identity.
    pub control_lsn: Option<Lsn>,
    /// Actual current standing causes, copied from the existing owner.
    pub control: history::RecoveryStandingControl,
    /// Exact still-confirmed conversation, when selectable.
    pub confirmed_session: Option<SessionId>,
}

/// One canonical episode and the first authorization that claimed it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryEpisode {
    /// Logical identity, independent of barrier storage position.
    pub id: RecoveryEpisodeId,
    /// Actual durable required-barrier position.
    pub barrier_lsn: Lsn,
    /// Immutable original target.
    pub binding: RecoveryBinding,
    /// Context restriction.
    pub reason: RecoveryReason,
    /// First and only authorization, retained after closure.
    pub authorization_lsn: Option<Lsn>,
}

/// Monotonic lifecycle of one exclusive authorization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryClaimState {
    /// Normal admission has not issued the planned new turn.
    Pending,
    /// Matching issue is durable; no matching Spawn has consumed the attempt yet.
    IssueBound(RecoveryIssue),
    /// One physical attempt was durably observed.
    Consumed {
        /// Immutable issue retained for historical joins and receipts.
        issue: RecoveryIssue,
        /// Actual Spawn or positively observed terminal no-Spawn position.
        evidence_lsn: Lsn,
    },
    /// The unused authorization closed on journaled invalidating evidence.
    Invalidated {
        /// Actual required invalidation position.
        invalidation_lsn: Lsn,
        /// Actual preceding change or the durable observation made by this invalidation.
        evidence: RecoveryInvalidationEvidence,
    },
    /// A new missing-Spawn episode closed the old context consent atomically.
    Superseded {
        /// Immutable owed issue retained across the new barrier.
        issue: RecoveryIssue,
        /// New canonical episode requiring fresh consent.
        successor: RecoveryEpisodeId,
        /// Actual superseding barrier position.
        barrier_lsn: Lsn,
    },
}

/// Historical receipt combines immutable intent with its current durable outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReceipt<'a> {
    /// Exact authorization and original episode.
    pub claim: RecoveryClaimRef,
    /// Unchanged request and context intent.
    pub authorization: &'a RecoveryAuthorized,
    /// Derived monotonic claim state.
    pub state: &'a RecoveryClaimState,
}

/// Detached pure inputs. Journal registration and historical evidence validation are separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryEvent<'a> {
    /// Create a canonical barrier, possibly superseding one issue-bound claim.
    Required(&'a RecoveryRequired),
    /// Exclusively claim one canonical episode.
    Authorized(&'a RecoveryAuthorized),
    /// Bind a new issue to its authorization after normal task composition.
    Issued {
        /// Exact owning authorization.
        claim: RecoveryClaimRef,
        /// Actual issued task witness.
        issued: &'a RecoveryIssued,
    },
    /// Consume the matching issue through one recorded physical attempt.
    Consumed {
        /// Exact owning authorization.
        claim: RecoveryClaimRef,
        /// Exact durable issue.
        issue: &'a RecoveryIssue,
        /// Exact launch announcement, independent of an owed issue's old announcement.
        session: Option<&'a SessionId>,
    },
    /// Close unused consent, without releasing the original issue or accounting.
    Invalidated {
        /// Exact owning authorization.
        claim: RecoveryClaimRef,
        /// Actual preceding change or durable authority observation.
        evidence: RecoveryInvalidationEvidence,
    },
}

/// A rejected relationship, without task, path, session or provider error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryStateError {
    /// A position is zero, unavailable or out of order.
    Position,
    /// Required identity, target or current journal fact does not match.
    Binding,
    /// Another origin already names this unresolved work.
    EpisodeAlias,
    /// A request identity already has a durable authorization.
    DuplicateRequest,
    /// A receipt identity names different canonical request fields.
    RequestConflict,
    /// The episode was already exclusively claimed, including a closed claim.
    Claimed,
    /// A planned turn already belongs to another authorization.
    PlannedTurn,
    /// Context choice or recap is not permitted by this boundary.
    Context,
    /// The authorization's control transition does not match the committed cut.
    Control,
    /// The claim cannot make this lifecycle transition.
    Lifecycle,
}

impl fmt::Display for RecoveryStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid recovery history: {self:?}")
    }
}

impl std::error::Error for RecoveryStateError {}

/// Pure recovery relationships. No I/O, minting, admission, task composition or launch occurs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryState {
    episodes: BTreeMap<RecoveryEpisodeId, RecoveryEpisode>,
    current: BTreeMap<RecoveryWork, RecoveryEpisodeId>,
    authorizations: BTreeMap<Lsn, RecoveryAuthorized>,
    claims: BTreeMap<Lsn, RecoveryClaimState>,
    requests: BTreeMap<Hex32, Lsn>,
    planned_turns: BTreeMap<TurnId, Lsn>,
    last_applied: Lsn,
}

impl RecoveryState {
    /// Bounded immutable ticket history. An incomplete scan cannot establish absence.
    #[must_use]
    pub fn ticket_episodes(
        &self,
        binding: crate::diff_stamp::TicketBinding,
        limit: usize,
    ) -> (Vec<&RecoveryEpisode>, bool) {
        const SCAN_LIMIT: usize = 512;
        let mut rows: Vec<_> = self
            .episodes
            .values()
            .take(SCAN_LIMIT)
            .filter(|episode| episode.binding.ticket == Some(binding))
            .collect();
        rows.sort_unstable_by_key(|episode| std::cmp::Reverse(episode.barrier_lsn));
        let truncated = self.episodes.len() > SCAN_LIMIT || rows.len() > limit;
        rows.truncate(limit);
        (rows, truncated)
    }

    /// Canonical latest episode, retained even after its claim closes.
    #[must_use]
    pub fn current(&self, work: RecoveryWork) -> Option<&RecoveryEpisode> {
        self.episodes.get(self.current.get(&work)?)
    }

    /// Latest current episode among recorded works with this exact ticket binding.
    #[must_use]
    pub fn latest_ticket_episode(
        &self,
        binding: crate::diff_stamp::TicketBinding,
        works: impl IntoIterator<Item = RecoveryWork>,
    ) -> Option<&RecoveryEpisode> {
        works
            .into_iter()
            .filter_map(|work| self.current(work))
            .filter(|episode| episode.binding.ticket == Some(binding))
            .max_by_key(|episode| episode.barrier_lsn)
    }

    /// Original reservation for an actual new issue, without admitting that issue.
    #[must_use]
    pub fn planned_claim(&self, turn: TurnId) -> Option<RecoveryClaimRef> {
        self.planned_turns.get(&turn).map(|lsn| RecoveryClaimRef {
            episode: self.authorizations[lsn].episode,
            authorization_lsn: *lsn,
        })
    }

    /// Latest consent for this physical work, following repeat supersession before reservation.
    /// An existing unclaimed boundary refuses rather than reviving an older claim.
    pub fn work_claim(
        &self,
        work: RecoveryWork,
    ) -> Result<Option<RecoveryClaimRef>, RecoveryStateError> {
        if let Some(episode) = self.current(work) {
            return Ok(Some(RecoveryClaimRef {
                episode: episode.id,
                authorization_lsn: episode
                    .authorization_lsn
                    .ok_or(RecoveryStateError::Lifecycle)?,
            }));
        }
        Ok(self.planned_claim(work.turn))
    }

    /// Immutable intent and committed outcome for an already identified claim.
    /// This lookup confers no permission to issue or launch work.
    pub fn claim_receipt(
        &self,
        claim: RecoveryClaimRef,
    ) -> Result<RecoveryReceipt<'_>, RecoveryStateError> {
        let (authorization, state) = self.claim(claim)?;
        Ok(RecoveryReceipt {
            claim,
            authorization,
            state,
        })
    }

    /// Read an original receipt without changing authority or submitting work.
    pub fn receipt(
        &self,
        request: Hex32,
        digest: Hex32,
    ) -> Result<Option<RecoveryReceipt<'_>>, RecoveryStateError> {
        let receipt = self.receipt_by_request(request)?;
        if receipt
            .as_ref()
            .is_some_and(|receipt| receipt.authorization.request_digest != digest)
        {
            return Err(RecoveryStateError::RequestConflict);
        }
        Ok(receipt)
    }

    /// Read an original receipt using only the identity retained before submission.
    pub fn receipt_by_request(
        &self,
        request: Hex32,
    ) -> Result<Option<RecoveryReceipt<'_>>, RecoveryStateError> {
        let Some(lsn) = self.requests.get(&request) else {
            return Ok(None);
        };
        let authorization = &self.authorizations[lsn];
        self.claim_receipt(RecoveryClaimRef {
            episode: authorization.episode,
            authorization_lsn: *lsn,
        })
        .map(Some)
    }

    /// Check one transition without predicting its eventual physical append position.
    pub fn validate_next(
        &self,
        event: RecoveryEvent<'_>,
        facts: &RecoveryFacts,
    ) -> Result<(), RecoveryStateError> {
        self.validate(event, facts, None)
    }

    /// Apply one actual durable transition after validating every affected relationship.
    pub fn apply(
        &mut self,
        lsn: Lsn,
        event: RecoveryEvent<'_>,
        facts: &RecoveryFacts,
    ) -> Result<(), RecoveryStateError> {
        if lsn <= self.last_applied {
            return Ok(());
        }
        self.validate(event, facts, Some(lsn))?;
        match event {
            RecoveryEvent::Required(required) => {
                let id = episode_id(required, lsn);
                if let Some(previous) = required.predecessor
                    && let RecoveryClaimState::IssueBound(issue) =
                        &self.claims[&previous.authorization_lsn]
                {
                    self.claims.insert(
                        previous.authorization_lsn,
                        RecoveryClaimState::Superseded {
                            issue: issue.clone(),
                            successor: id,
                            barrier_lsn: lsn,
                        },
                    );
                }
                self.episodes.insert(
                    id,
                    RecoveryEpisode {
                        id,
                        barrier_lsn: lsn,
                        binding: required.binding.clone(),
                        reason: required.reason,
                        authorization_lsn: None,
                    },
                );
                self.current.insert(id.work, id);
            }
            RecoveryEvent::Authorized(authorization) => {
                self.episodes
                    .get_mut(&authorization.episode)
                    .expect("validated episode")
                    .authorization_lsn = Some(lsn);
                let state = match &authorization.plan {
                    RecoveryPlan::New { turn, .. } => {
                        self.planned_turns.insert(*turn, lsn);
                        RecoveryClaimState::Pending
                    }
                    RecoveryPlan::RepeatIssued { issue, .. } => {
                        RecoveryClaimState::IssueBound(issue.clone())
                    }
                };
                self.claims.insert(lsn, state);
                self.requests.insert(authorization.request_id, lsn);
                self.authorizations.insert(lsn, authorization.clone());
            }
            RecoveryEvent::Issued { claim, issued } => {
                self.claims.insert(
                    claim.authorization_lsn,
                    RecoveryClaimState::IssueBound(RecoveryIssue {
                        lsn,
                        issued: issued.clone(),
                    }),
                );
            }
            RecoveryEvent::Consumed { claim, issue, .. } => {
                self.claims.insert(
                    claim.authorization_lsn,
                    RecoveryClaimState::Consumed {
                        issue: issue.clone(),
                        evidence_lsn: lsn,
                    },
                );
            }
            RecoveryEvent::Invalidated { claim, evidence } => {
                self.claims.insert(
                    claim.authorization_lsn,
                    RecoveryClaimState::Invalidated {
                        invalidation_lsn: lsn,
                        evidence,
                    },
                );
            }
        }
        self.last_applied = lsn;
        Ok(())
    }

    fn validate(
        &self,
        event: RecoveryEvent<'_>,
        facts: &RecoveryFacts,
        actual: Option<Lsn>,
    ) -> Result<(), RecoveryStateError> {
        if facts.through < self.last_applied || actual.is_some_and(|lsn| lsn <= facts.through) {
            return Err(RecoveryStateError::Position);
        }
        let prior = |lsn: Lsn| lsn != Lsn::ZERO && lsn <= facts.through;
        match event {
            RecoveryEvent::Required(required) => self.validate_required(required, facts, actual)?,
            RecoveryEvent::Authorized(authorization) => {
                self.validate_authorization(authorization, facts)?;
            }
            RecoveryEvent::Issued { claim, issued } => {
                let (authorization, state) = self.claim(claim)?;
                if *state != RecoveryClaimState::Pending {
                    return Err(RecoveryStateError::Lifecycle);
                }
                let RecoveryPlan::New { turn, .. } = authorization.plan else {
                    return Err(RecoveryStateError::Lifecycle);
                };
                if issued.work.agent != claim.episode.work.agent
                    || issued.work.goal != claim.episode.work.goal
                    || issued.work.turn != turn
                    || !ordered_positions(&issued.answers)
                    || issued.answers.iter().any(|lsn| !prior(*lsn))
                    || issued.model_selection != authorization.binding.model_selection
                    || issued.session.as_ref() != planned_session(&authorization.plan)
                {
                    return Err(RecoveryStateError::Binding);
                }
                valid_binding(&authorization.binding, facts)?;
                if authorization.resulting_control != facts.control_lsn {
                    return Err(RecoveryStateError::Control);
                }
                if matches!(&authorization.plan,
                    RecoveryPlan::New { context: RecoveryContext::TryRecorded { session }, .. }
                    if facts.confirmed_session.as_ref() != Some(session))
                {
                    return Err(RecoveryStateError::Context);
                }
            }
            RecoveryEvent::Consumed {
                claim,
                issue,
                session,
            } => {
                let (authorization, state) = self.claim(claim)?;
                if !matches!(state, RecoveryClaimState::IssueBound(bound) if bound == issue) {
                    return Err(RecoveryStateError::Lifecycle);
                }
                if !prior(issue.lsn) {
                    return Err(RecoveryStateError::Position);
                }
                if session != planned_session(&authorization.plan) {
                    return Err(RecoveryStateError::Context);
                }
            }
            RecoveryEvent::Invalidated { claim, evidence } => {
                self.validate_invalidation_claim(claim, evidence, facts.through)?;
            }
        }
        Ok(())
    }

    fn validate_invalidation_claim(
        &self,
        claim: RecoveryClaimRef,
        evidence: RecoveryInvalidationEvidence,
        through: Lsn,
    ) -> Result<(), RecoveryStateError> {
        let (authorization, state) = self.claim(claim)?;
        if through < self.last_applied || !matches!(state, RecoveryClaimState::Pending) {
            return Err(RecoveryStateError::Lifecycle);
        }
        match evidence {
            RecoveryInvalidationEvidence::RecordedChange { lsn, .. }
                if lsn <= claim.authorization_lsn || lsn > through =>
            {
                Err(RecoveryStateError::Position)
            }
            RecoveryInvalidationEvidence::BrowserAuthorityUnavailable { session_digest } if !matches!(authorization.attribution, crate::recovery::RecoveryAttribution::BrowserSession { session_digest: original } if original == session_digest) => {
                Err(RecoveryStateError::Binding)
            }
            _ => Ok(()),
        }
    }

    fn validate_required(
        &self,
        required: &RecoveryRequired,
        facts: &RecoveryFacts,
        actual: Option<Lsn>,
    ) -> Result<(), RecoveryStateError> {
        let prior = |lsn: Lsn| lsn != Lsn::ZERO && lsn <= facts.through;
        valid_binding(&required.binding, facts)?;
        if !prior(required.binding.issue_lsn)
            || !prior(required.binding.hired_lsn)
            || required.binding.hired_lsn >= required.binding.issue_lsn
            || matches!(required.origin, RecoveryOrigin::Existing { lsn } if !prior(lsn) || lsn <= required.binding.issue_lsn)
        {
            return Err(RecoveryStateError::Position);
        }
        if matches!(required.origin, RecoveryOrigin::Existing { .. })
            != matches!(required.reason, RecoveryReason::HeldOrPausedFailure)
        {
            return Err(RecoveryStateError::Binding);
        }
        let incumbent = self.current(required.binding.work);
        if incumbent.is_none()
            && self
                .planned_turns
                .get(&required.binding.work.turn)
                .is_some_and(|owner| {
                    required.predecessor.map(|claim| claim.authorization_lsn) != Some(*owner)
                })
        {
            return Err(RecoveryStateError::Binding);
        }
        if let Some(previous) = required.predecessor {
            let (_, state) = self.claim(previous)?;
            let old = &self.episodes[&previous.episode];
            let mut expected = old.binding.clone();
            match state {
                RecoveryClaimState::IssueBound(issue) => {
                    expected.work = issue.issued.work;
                    expected.issue_lsn = issue.lsn;
                    if required.reason != RecoveryReason::MissingSpawn
                        || required.origin != RecoveryOrigin::ThisBarrier
                        || facts.source_issue != *issue
                    {
                        return Err(RecoveryStateError::Binding);
                    }
                }
                RecoveryClaimState::Consumed {
                    issue,
                    evidence_lsn,
                } => {
                    expected.work = issue.issued.work;
                    expected.issue_lsn = issue.lsn;
                    if required.reason == RecoveryReason::MissingSpawn
                        || matches!(required.origin, RecoveryOrigin::Existing { lsn } if lsn <= *evidence_lsn)
                    {
                        return Err(RecoveryStateError::Binding);
                    }
                }
                RecoveryClaimState::Invalidated {
                    invalidation_lsn, ..
                } => {
                    if required.reason == RecoveryReason::MissingSpawn
                        || matches!(required.origin, RecoveryOrigin::Existing { lsn } if lsn <= *invalidation_lsn)
                    {
                        return Err(RecoveryStateError::Binding);
                    }
                }
                _ => return Err(RecoveryStateError::Lifecycle),
            }
            if required.binding != expected
                || self.current(previous.episode.work).map(|e| e.id) != Some(previous.episode)
                || incumbent.is_some_and(|e| e.id != previous.episode)
                || actual.is_some_and(|lsn| episode_id(required, lsn) == previous.episode)
            {
                return Err(RecoveryStateError::Binding);
            }
        } else if incumbent.is_some() {
            return Err(RecoveryStateError::EpisodeAlias);
        }
        Ok(())
    }

    fn validate_authorization(
        &self,
        authorization: &RecoveryAuthorized,
        facts: &RecoveryFacts,
    ) -> Result<(), RecoveryStateError> {
        let prior = |lsn: Lsn| lsn != Lsn::ZERO && lsn <= facts.through;
        if self.requests.contains_key(&authorization.request_id) {
            return Err(RecoveryStateError::DuplicateRequest);
        }
        valid_binding(&authorization.binding, facts)?;
        let episode = self
            .current(authorization.episode.work)
            .filter(|e| e.id == authorization.episode)
            .ok_or(RecoveryStateError::Binding)?;
        if episode.authorization_lsn.is_some() {
            return Err(RecoveryStateError::Claimed);
        }
        if episode.binding != authorization.binding {
            return Err(RecoveryStateError::Binding);
        }
        if !prior(episode.barrier_lsn) {
            return Err(RecoveryStateError::Position);
        }
        if [
            authorization.expected_control,
            authorization.resulting_control,
            authorization.resume_lsn,
        ]
        .into_iter()
        .flatten()
        .any(|lsn| !prior(lsn))
        {
            return Err(RecoveryStateError::Position);
        }
        match authorization.resume_lsn {
            Some(resume) => {
                if authorization.resulting_control != Some(resume)
                    || resume <= episode.barrier_lsn
                    || authorization
                        .expected_control
                        .is_some_and(|old| old >= resume)
                {
                    return Err(RecoveryStateError::Control);
                }
            }
            None if authorization.expected_control != authorization.resulting_control => {
                return Err(RecoveryStateError::Control);
            }
            None => {}
        }
        if authorization.resulting_control != facts.control_lsn {
            return Err(RecoveryStateError::Control);
        }
        match &authorization.plan {
            RecoveryPlan::New { turn, context } => {
                if episode.reason == RecoveryReason::MissingSpawn {
                    return Err(RecoveryStateError::Context);
                }
                if *turn == episode.id.work.turn || self.planned_turns.contains_key(turn) {
                    return Err(RecoveryStateError::PlannedTurn);
                }
                match context {
                    RecoveryContext::TryRecorded { session } => {
                        if episode.reason == RecoveryReason::UnconfirmedContext
                            || facts.confirmed_session.as_ref() != Some(session)
                        {
                            return Err(RecoveryStateError::Context);
                        }
                    }
                    RecoveryContext::Fresh { recap, .. } => {
                        if recap.text.len() > MAX_RECOVERY_RECAP_BYTES
                            || !ordered_positions(&recap.sources)
                            || recap.sources.iter().any(|lsn| !prior(*lsn))
                        {
                            return Err(RecoveryStateError::Context);
                        }
                    }
                }
            }
            RecoveryPlan::RepeatIssued { issue, .. } => {
                if episode.reason != RecoveryReason::MissingSpawn || *issue != facts.source_issue {
                    return Err(RecoveryStateError::Context);
                }
            }
        }
        Ok(())
    }

    fn claim(
        &self,
        claim: RecoveryClaimRef,
    ) -> Result<(&RecoveryAuthorized, &RecoveryClaimState), RecoveryStateError> {
        let authorization = self
            .authorizations
            .get(&claim.authorization_lsn)
            .filter(|a| a.episode == claim.episode)
            .ok_or(RecoveryStateError::Binding)?;
        Ok((authorization, &self.claims[&claim.authorization_lsn]))
    }
}

fn valid_binding(
    binding: &RecoveryBinding,
    facts: &RecoveryFacts,
) -> Result<(), RecoveryStateError> {
    if *binding != facts.binding
        || binding.issue_lsn != facts.source_issue.lsn
        || binding.work != facts.source_issue.issued.work
        || binding.model_selection != facts.source_issue.issued.model_selection
        || !ordered_positions(&facts.source_issue.issued.answers)
        || facts
            .source_issue
            .issued
            .answers
            .iter()
            .any(|lsn| *lsn >= binding.issue_lsn)
        || binding.ticket.is_some_and(|t| {
            t.goal != binding.work.goal || t.claimant != binding.work.agent || t.round == 0
        })
    {
        return Err(RecoveryStateError::Binding);
    }
    Ok(())
}

fn planned_session(plan: &RecoveryPlan) -> Option<&SessionId> {
    match plan {
        RecoveryPlan::New {
            context: RecoveryContext::TryRecorded { session },
            ..
        } => Some(session),
        RecoveryPlan::New {
            context: RecoveryContext::Fresh { session, .. },
            ..
        }
        | RecoveryPlan::RepeatIssued { session, .. } => session.as_ref(),
    }
}

fn ordered_positions(positions: &[Lsn]) -> bool {
    positions.first().is_none_or(|lsn| *lsn != Lsn::ZERO)
        && positions.windows(2).all(|pair| pair[0] < pair[1])
}

fn episode_id(required: &RecoveryRequired, actual_lsn: Lsn) -> RecoveryEpisodeId {
    RecoveryEpisodeId {
        work: required.binding.work,
        origin: match required.origin {
            RecoveryOrigin::Existing { lsn } => lsn,
            RecoveryOrigin::ThisBarrier => actual_lsn,
        },
    }
}
