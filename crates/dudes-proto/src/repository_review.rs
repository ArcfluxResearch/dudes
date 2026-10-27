//! Bounded review evidence for one frozen ticket declaration.

use dudes_types::{AgentId, GoalId, Lsn, TurnId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::diff_stamp::{
    BaselineOutcome, BaselinePayload, CaptureInterval, DigestHex, MAX_STAMP_PAYLOAD_BYTES,
    SamplerVersions, StampPayload, TicketBinding,
};
use crate::record::FileIdentity;
use crate::repository::{HireRepositories, RepoId, ScopedRepositories, TicketRepositoryRound};
use crate::repository_state::history::RepositoryStateError;
use crate::{DiffFreshness, DiffRefusal, Evidence, Predicate, Record};

/// Maximum repositories in one review batch.
pub const MAX_REVIEW_REPOSITORIES: usize = 8;
/// Eight bounded members, plus 1 KiB for the fixed header and JSON envelope.
pub const MAX_REVIEW_BATCH_BYTES: usize = MAX_REVIEW_REPOSITORIES * MAX_STAMP_PAYLOAD_BYTES + 1024;
/// Encoding revision of repository review batches.
pub const REVIEW_BATCH_VERSION: u32 = 1;

const ENTRY_ENVELOPE_BYTES: usize = br#""at":-9223372036854775808,"#.len();

/// Check the batch and its complete journal envelope before writing or accepting it.
pub(crate) fn validate_record(record: &Record) -> Result<(), RepositoryStateError> {
    match record {
        Record::TicketBaselineBatchRecorded(batch) => batch.validate()?,
        Record::TicketReviewBatchStamped(batch) => batch.validate()?,
        _ => return Ok(()),
    }
    if bounded(record, MAX_REVIEW_BATCH_BYTES - ENTRY_ENVELOPE_BYTES) {
        Ok(())
    } else {
        Err(RepositoryStateError::Review)
    }
}

/// References to the immutable start and hire which own paths and the check definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryReviewBinding {
    /// Frozen ticket round and claimant.
    pub ticket: TicketBinding,
    /// Actual ticket start position.
    pub start_lsn: Lsn,
    /// Actual hire position selected by the start.
    pub hired_lsn: Lsn,
}

/// One declared repository's baseline, including its identity in the member byte cap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryBaselineMember {
    /// Identity resolved through the recorded declaration and hire.
    pub repo_id: RepoId,
    /// Capture interval.
    pub interval: CaptureInterval,
    /// Sampler and policy revisions.
    pub versions: SamplerVersions,
    /// Observed root and initial state, or an explicit failure.
    pub outcome: BaselineOutcome,
}

/// One atomic baseline with exactly the declaration's members in its recorded order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryBaselineBatch {
    /// Encoding revision.
    pub v: u32,
    /// Immutable authority references.
    pub binding: RepositoryReviewBinding,
    /// Every declared member, including unavailable observations.
    #[schemars(length(min = 1, max = 8))]
    pub members: Vec<RepositoryBaselineMember>,
}

/// A post-check observation carries the actual sampled root identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepositoryStampOutcome {
    /// A bounded scope was sampled from this root.
    Sampled {
        /// Filesystem identity of the opened execution root.
        root: FileIdentity,
        /// The eligible scope was sampled without a cap or refusal.
        complete: bool,
        /// Scope digest.
        digest: DigestHex,
        /// Omitted eligible pairs.
        omitted: u32,
        /// The omission count is a lower bound.
        omitted_lower_bound: bool,
        /// New files the listing could not name.
        untracked: u32,
        /// The new-file count is a lower bound.
        untracked_lower_bound: bool,
    },
    /// The repository could not be sampled.
    Unavailable {
        /// Finite reason.
        reason: DiffRefusal,
    },
}

/// One declared repository's post-check sample, including its identity in the byte cap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryStampMember {
    /// Identity resolved through the recorded declaration and hire.
    pub repo_id: RepoId,
    /// Capture interval.
    pub interval: CaptureInterval,
    /// Sampler and policy revisions.
    pub versions: SamplerVersions,
    /// Sample or explicit failure.
    pub outcome: RepositoryStampOutcome,
}

/// One atomic sample of the same declaration after one recorded check result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryStampBatch {
    /// Encoding revision.
    pub v: u32,
    /// Immutable authority references.
    pub binding: RepositoryReviewBinding,
    /// Exact baseline batch position.
    pub baseline_lsn: Lsn,
    /// Exact observed predicate-result position.
    pub check_lsn: Lsn,
    /// Every declared member, including unavailable observations.
    #[schemars(length(min = 1, max = 8))]
    pub members: Vec<RepositoryStampMember>,
}

fn bounded<T: Serialize>(value: &T, cap: usize) -> bool {
    serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= cap)
}

fn member_shape<T: Serialize>(value: &T, interval: CaptureInterval) -> bool {
    interval.started_at <= interval.ended_at && bounded(value, MAX_STAMP_PAYLOAD_BYTES)
}

fn batch_shape<'a, T: Serialize>(
    value: &T,
    version: u32,
    binding: RepositoryReviewBinding,
    ids: impl Iterator<Item = &'a RepoId>,
) -> bool {
    let ids: Vec<_> = ids.collect();
    version == REVIEW_BATCH_VERSION
        && binding.ticket.round > 0
        && binding.hired_lsn != Lsn::ZERO
        && binding.hired_lsn < binding.start_lsn
        && (1..=MAX_REVIEW_REPOSITORIES).contains(&ids.len())
        && ids.iter().collect::<std::collections::BTreeSet<_>>().len() == ids.len()
        && bounded(value, MAX_REVIEW_BATCH_BYTES)
}

impl RepositoryBaselineBatch {
    /// Validate bounded encoding and position-independent structure.
    pub fn validate(&self) -> Result<(), RepositoryStateError> {
        if batch_shape(
            self,
            self.v,
            self.binding,
            self.members.iter().map(|m| &m.repo_id),
        ) && self.members.iter().all(|m| member_shape(m, m.interval))
        {
            Ok(())
        } else {
            Err(RepositoryStateError::Review)
        }
    }

    /// Whether every declared member has an observed baseline.
    #[must_use]
    pub fn complete(&self) -> bool {
        !self.members.is_empty()
            && self
                .members
                .iter()
                .all(|m| matches!(m.outcome, BaselineOutcome::Captured { .. }))
    }
}

impl RepositoryStampBatch {
    /// Validate bounded encoding and position-independent structure.
    pub fn validate(&self) -> Result<(), RepositoryStateError> {
        if batch_shape(
            self,
            self.v,
            self.binding,
            self.members.iter().map(|m| &m.repo_id),
        ) && self.binding.start_lsn < self.baseline_lsn
            && self.baseline_lsn < self.check_lsn
            && self.members.iter().all(|m| member_shape(m, m.interval))
        {
            Ok(())
        } else {
            Err(RepositoryStateError::Review)
        }
    }

    /// Whether every declared scope was sampled completely.
    #[must_use]
    pub fn complete(&self) -> bool {
        !self.members.is_empty()
            && self.members.iter().all(|m| {
                matches!(
                    m.outcome,
                    RepositoryStampOutcome::Sampled {
                        complete: true,
                        omitted: 0,
                        omitted_lower_bound: false,
                        untracked: 0,
                        untracked_lower_bound: false,
                        ..
                    }
                )
            })
    }

    fn freshness(&self, current: &Self) -> DiffFreshness {
        if self.validate().is_err()
            || current.validate().is_err()
            || !self.complete()
            || !current.complete()
            || self.binding != current.binding
            || self.baseline_lsn != current.baseline_lsn
            || self.check_lsn != current.check_lsn
            || self.members.len() != current.members.len()
        {
            return DiffFreshness::Unknown;
        }
        let mut changed = false;
        for (recorded, current) in self.members.iter().zip(&current.members) {
            let (
                RepositoryStampOutcome::Sampled { root, digest, .. },
                RepositoryStampOutcome::Sampled {
                    root: current_root,
                    digest: current_digest,
                    ..
                },
            ) = (recorded.outcome, current.outcome)
            else {
                return DiffFreshness::Unknown;
            };
            if recorded.repo_id != current.repo_id
                || recorded.versions != current.versions
                || root != current_root
            {
                return DiffFreshness::Unknown;
            }
            changed |= digest != current_digest;
        }
        if changed {
            DiffFreshness::ChangedAfterCheck
        } else {
            DiffFreshness::MatchesReviewSample
        }
    }
}

/// Historical scalar metadata. Its first-baseline and payload-only stamp rules are preserved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketReview {
    /// Position of the first baseline.
    pub baseline_lsn: Lsn,
    /// Round-start observation.
    pub baseline: BaselinePayload,
    /// Newest qualifying scalar stamp.
    pub stamp: Option<(Lsn, StampPayload)>,
}

/// A real observed command result for the frozen check and its work boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryReviewCheck {
    /// Predicate-result record position.
    pub lsn: Lsn,
    /// Whether the check passed.
    pub pass: bool,
}

/// Saved evidence for one declared round. It grants neither acceptance nor execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryReview {
    /// Frozen authority references.
    pub binding: RepositoryReviewBinding,
    /// Exact ordered declaration, even when observations are unavailable.
    pub expected: Vec<RepoId>,
    /// First scoped issue, independent of its charged goal.
    pub first_issue_lsn: Option<Lsn>,
    /// Latest scoped issue and its turn.
    pub last_issue: Option<(Lsn, TurnId)>,
    /// Last admissible baseline. An incomplete pre-issue capture may be replaced.
    pub baseline: Option<(Lsn, RepositoryBaselineBatch)>,
    /// Latest result qualifying for this frozen check.
    pub check: Option<RepositoryReviewCheck>,
    /// Latest sample tied to that result and baseline.
    pub stamp: Option<(Lsn, RepositoryStampBatch)>,
}

/// The evidence still needed for a scoped review, independently of instruction review.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryReviewReadiness {
    /// No pre-issue capture has been recorded.
    MissingBaseline,
    /// A pre-issue capture has an unavailable member and may be replaced.
    IncompleteBaseline,
    /// Work was already issued without complete baseline evidence. Later capture cannot repair it.
    MissingAfterIssue,
    /// No qualifying observed result follows the required baseline or latest scoped issue.
    MissingCheck,
    /// The latest qualifying result did not pass.
    CheckFailed,
    /// No sample follows the latest qualifying result.
    MissingStamp,
    /// A post-check sample has unavailable or incomplete coverage.
    UnknownStamp,
    /// Baseline and post-check evidence are complete. Other admission gates still apply.
    Complete,
}

impl RepositoryReview {
    /// Compare a new sample against the saved declaration. Missing coverage is unknown.
    #[must_use]
    pub fn freshness(&self, current: &RepositoryStampBatch) -> DiffFreshness {
        if self
            .baseline
            .as_ref()
            .is_none_or(|(_, batch)| !batch.complete())
            || !current
                .members
                .iter()
                .map(|member| &member.repo_id)
                .eq(self.expected.iter())
        {
            return DiffFreshness::Unknown;
        }
        self.stamp
            .as_ref()
            .map_or(DiffFreshness::Unknown, |(_, recorded)| {
                recorded.freshness(current)
            })
    }

    /// Derive evidence readiness without repairing missing history from current disk state.
    #[must_use]
    pub fn readiness(&self) -> RepositoryReviewReadiness {
        use RepositoryReviewReadiness as Ready;
        if self
            .baseline
            .as_ref()
            .is_none_or(|(_, batch)| !batch.complete())
        {
            return if self.first_issue_lsn.is_some() {
                Ready::MissingAfterIssue
            } else if self.baseline.is_some() {
                Ready::IncompleteBaseline
            } else {
                Ready::MissingBaseline
            };
        }
        match self.check {
            None => Ready::MissingCheck,
            Some(check) if !check.pass => Ready::CheckFailed,
            Some(_) => match &self.stamp {
                None => Ready::MissingStamp,
                Some((_, stamp)) if !stamp.complete() => Ready::UnknownStamp,
                Some(_) => Ready::Complete,
            },
        }
    }
}

/// One goal-owned review value shared by World and durable projections.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TicketReviewState {
    /// Historical singleton semantics.
    Legacy(TicketReview),
    /// Explicit declaration and coverage.
    Scoped(RepositoryReview),
}

/// The preceding goal and hire facts needed by the pure review fold.
#[derive(Clone, Copy, Debug)]
pub struct ReviewFacts<'a> {
    /// Goal identity.
    pub goal: GoalId,
    /// Current goal owner.
    pub agent: AgentId,
    /// Frozen predicate.
    pub predicate: &'a Predicate,
    /// The goal's recorded pre-work check policy.
    pub check_baseline: Option<&'a crate::CheckBaseline>,
    /// Frozen ticket start, when present.
    pub round: Option<&'a TicketRepositoryRound>,
    /// Exact current hire.
    pub hire: Option<(Lsn, &'a HireRepositories)>,
    /// Existing bounded evidence.
    pub review: Option<&'a TicketReviewState>,
}

/// The goal whose review this record can affect. Scoped issues use their explicit ticket.
#[must_use]
pub fn affected_goal(record: &Record) -> Option<GoalId> {
    match record {
        Record::TicketStartedWithRepositories(start) => Some(start.goal.id),
        Record::TicketBaselineBatchRecorded(batch) => Some(batch.binding.ticket.goal),
        Record::TicketReviewBatchStamped(batch) => Some(batch.binding.ticket.goal),
        Record::TicketBaselineRecorded { goal, .. }
        | Record::TicketReviewStamped { goal, .. }
        | Record::PredicateResult { goal, .. } => Some(*goal),
        _ => record
            .turn_issue()?
            .repositories?
            .ticket
            .as_ref()
            .map(|round| round.goal),
    }
}

/// Prepare a replacement for the existing goal owner, without mutating on refusal.
pub fn prepare(
    lsn: Lsn,
    record: &Record,
    facts: Option<ReviewFacts<'_>>,
) -> Result<Option<TicketReviewState>, RepositoryStateError> {
    if let Record::TicketStartedWithRepositories(start) = record {
        return Ok(Some(TicketReviewState::Scoped(RepositoryReview {
            binding: RepositoryReviewBinding {
                ticket: TicketBinding {
                    card: start.card,
                    round: start.round,
                    goal: start.goal.id,
                    claimant: start.agent,
                },
                start_lsn: lsn,
                hired_lsn: start.repositories.hired_lsn,
            },
            expected: start
                .repositories
                .declaration
                .repositories
                .entries()
                .iter()
                .map(|entry| entry.repo_id.clone())
                .collect(),
            first_issue_lsn: None,
            last_issue: None,
            baseline: None,
            check: None,
            stamp: None,
        })));
    }
    let Some(facts) = facts else {
        return if matches!(
            record,
            Record::TicketBaselineBatchRecorded(_) | Record::TicketReviewBatchStamped(_)
        ) {
            Err(RepositoryStateError::Review)
        } else {
            Ok(None)
        };
    };
    if let Some(TicketReviewState::Scoped(review)) = facts.review {
        let mut review = review.clone();
        fold_scoped(lsn, record, facts, &mut review)?;
        return Ok(Some(TicketReviewState::Scoped(review)));
    }
    if matches!(
        record,
        Record::TicketBaselineBatchRecorded(_) | Record::TicketReviewBatchStamped(_)
    ) {
        return Err(RepositoryStateError::Review);
    }
    let mut review = facts.review.cloned();
    match record {
        Record::TicketBaselineRecorded {
            card,
            goal,
            baseline,
        } if review.is_none()
            && baseline.binding.card == *card
            && baseline.binding.goal == *goal
            && baseline.binding.claimant == facts.agent
            && baseline.encode().is_ok() =>
        {
            review = Some(TicketReviewState::Legacy(TicketReview {
                baseline_lsn: lsn,
                baseline: *baseline,
                stamp: None,
            }));
        }
        Record::TicketReviewStamped { card, stamp, .. } => {
            if let Some(TicketReviewState::Legacy(review)) = &mut review
                && stamp.binding.card == *card
                && stamp.binding == review.baseline.binding
                && stamp.baseline_lsn == review.baseline_lsn
                && stamp.encode().is_ok()
            {
                review.stamp = Some((lsn, *stamp));
            }
        }
        _ => {}
    }
    Ok(review)
}

fn authority<'a>(
    lsn: Lsn,
    binding: RepositoryReviewBinding,
    facts: ReviewFacts<'a>,
    review: &RepositoryReview,
) -> Result<&'a ScopedRepositories, RepositoryStateError> {
    let round = facts.round.ok_or(RepositoryStateError::Review)?;
    let authority = round
        .authority
        .as_ref()
        .ok_or(RepositoryStateError::Review)?;
    let Some((hired_lsn, HireRepositories::Scoped(hire))) = facts.hire else {
        return Err(RepositoryStateError::Review);
    };
    if binding != review.binding
        || binding.ticket.goal != facts.goal
        || binding.ticket.claimant != facts.agent
        || round.card != binding.ticket.card
        || round.goal != binding.ticket.goal
        || round.round != binding.ticket.round
        || round.start_lsn != binding.start_lsn
        || binding.start_lsn >= lsn
        || binding.hired_lsn != authority.hired_lsn
        || binding.hired_lsn != hired_lsn
        || authority.check.predicate != *facts.predicate
        || hire
            .repositories
            .get(&authority.check_repo)
            .is_none_or(|repo| repo.execution != authority.check_root)
        || !authority
            .declaration
            .repositories
            .entries()
            .iter()
            .map(|entry| &entry.repo_id)
            .eq(review.expected.iter())
    {
        return Err(RepositoryStateError::Review);
    }
    Ok(hire)
}

fn fold_scoped(
    lsn: Lsn,
    record: &Record,
    facts: ReviewFacts<'_>,
    review: &mut RepositoryReview,
) -> Result<(), RepositoryStateError> {
    if let Some(issue) = record.turn_issue()
        && let Some(round) = issue.repositories.and_then(|grant| grant.ticket.as_ref())
        && round.goal == facts.goal
    {
        review.first_issue_lsn.get_or_insert(lsn);
        review.last_issue = Some((lsn, issue.turn));
        review.check = None;
        review.stamp = None;
        return Ok(());
    }
    match record {
        Record::TicketBaselineBatchRecorded(batch) => fold_baseline(lsn, batch, facts, review)?,
        Record::PredicateResult {
            goal,
            turn,
            provenance,
            pass,
            evidence,
        } => {
            review.check = None;
            review.stamp = None;
            let follows_work = match review.last_issue {
                Some((issue_lsn, issued)) => issue_lsn < lsn && *turn == Some(issued),
                None => {
                    turn.is_none()
                        && matches!(
                            facts.check_baseline,
                            Some(crate::CheckBaseline::MayAlreadyPass)
                        )
                        && review
                            .baseline
                            .as_ref()
                            .is_some_and(|(baseline_lsn, baseline)| {
                                *baseline_lsn < lsn && baseline.complete()
                            })
                }
            };
            if *goal == facts.goal
                && provenance.is_observed()
                && follows_work
                && let (
                    Predicate::Command { argv, .. },
                    Evidence::Command {
                        argv: observed,
                        program: Some(_),
                        ..
                    },
                ) = (facts.predicate, evidence)
                && argv == observed
                && !evidence.not_run(*pass)
            {
                review.check = Some(RepositoryReviewCheck { lsn, pass: *pass });
            }
        }
        Record::TicketReviewBatchStamped(batch) => fold_stamp(lsn, batch, facts, review)?,
        _ => {}
    }
    Ok(())
}

fn fold_baseline(
    lsn: Lsn,
    batch: &RepositoryBaselineBatch,
    facts: ReviewFacts<'_>,
    review: &mut RepositoryReview,
) -> Result<(), RepositoryStateError> {
    batch.validate()?;
    let hire = authority(lsn, batch.binding, facts, review)?;
    if review.first_issue_lsn.is_some()
        || review
            .baseline
            .as_ref()
            .is_some_and(|(_, old)| old.complete())
        || !batch
            .members
            .iter()
            .map(|member| &member.repo_id)
            .eq(review.expected.iter())
    {
        return Err(RepositoryStateError::Review);
    }
    for member in &batch.members {
        let repo = hire
            .repositories
            .get(&member.repo_id)
            .ok_or(RepositoryStateError::Review)?;
        if let BaselineOutcome::Captured { device, inode, .. } = member.outcome
            && repo.execution.directory_identity != (FileIdentity { device, inode })
        {
            return Err(RepositoryStateError::Review);
        }
    }
    review.baseline = Some((lsn, batch.clone()));
    review.check = None;
    review.stamp = None;
    Ok(())
}

fn fold_stamp(
    lsn: Lsn,
    batch: &RepositoryStampBatch,
    facts: ReviewFacts<'_>,
    review: &mut RepositoryReview,
) -> Result<(), RepositoryStateError> {
    batch.validate()?;
    let hire = authority(lsn, batch.binding, facts, review)?;
    let (baseline_lsn, baseline) = review
        .baseline
        .as_ref()
        .ok_or(RepositoryStateError::Review)?;
    if batch.baseline_lsn != *baseline_lsn
        || review
            .check
            .is_none_or(|check| check.lsn != batch.check_lsn)
        || batch.check_lsn >= lsn
        || !batch
            .members
            .iter()
            .map(|member| &member.repo_id)
            .eq(review.expected.iter())
    {
        return Err(RepositoryStateError::Review);
    }
    for (member, before) in batch.members.iter().zip(&baseline.members) {
        if member.versions != before.versions {
            return Err(RepositoryStateError::Review);
        }
        if let RepositoryStampOutcome::Sampled { root, .. } = member.outcome
            && (hire
                .repositories
                .get(&member.repo_id)
                .is_none_or(|repo| repo.execution.directory_identity != root)
                || !matches!(before.outcome, BaselineOutcome::Captured { device, inode, .. } if root == (FileIdentity { device, inode })))
        {
            return Err(RepositoryStateError::Review);
        }
    }
    review.stamp = Some((lsn, batch.clone()));
    Ok(())
}

#[cfg(test)]
mod tests;
