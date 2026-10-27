//! All hired repositories share one physical instruction observation and one bounded budget.

use std::collections::BTreeSet;

use dudes_types::{AgentId, Lsn};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Record;
use crate::instruction::{
    InstructionBaselineRef, InstructionComparison, InstructionIssueAssociation,
    InstructionObservationOwner, InstructionSampleUsage, InstructionSettlement,
    InstructionSnapshot, InstructionTicketAssociation, InstructionValidationError,
    MAX_INSTRUCTION_INPUTS, MAX_INSTRUCTION_METADATA_BYTES, MAX_INSTRUCTION_PARTICIPANTS,
};
use crate::record::FileIdentity;
use crate::repository::{HireRepositories, RepoId, RepositoryTurnGrant, TicketRepositoryRound};

/// Revision of the bounded repository instruction carriers.
pub const INSTRUCTION_BATCH_VERSION: u32 = 1;

/// Immutable hire and ticket preparation selected for one physical attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryInstructionBinding {
    /// Exact recorded hire, including repositories outside the ticket declaration.
    pub hired_lsn: Lsn,
    /// Exact ticket start, or explicit null for work outside a held ticket.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    #[schemars(with = "crate::chat::ExplicitNullable<Lsn>")]
    pub ticket_start_lsn: Option<Lsn>,
}

/// One hired member's starting observation or explicit unresolved coverage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryInstructionSnapshot {
    /// Stable repository identity in hire order.
    pub repo_id: RepoId,
    /// Identity of the sampled execution root; absent only when the root was unresolved.
    pub root: Option<FileIdentity>,
    /// Frozen discovery provenance and bounded fingerprints.
    pub snapshot: InstructionSnapshot,
}

/// One durable start for every member of a physical attempt's exact hire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryInstructionStart {
    /// Encoding revision.
    pub v: u32,
    /// Exact preceding authority.
    pub binding: RepositoryInstructionBinding,
    /// Physical attempt owner.
    pub owner: InstructionObservationOwner,
    /// Ticket association, independently of charged goal.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    #[schemars(with = "crate::chat::ExplicitNullable<InstructionTicketAssociation>")]
    pub ticket: Option<InstructionTicketAssociation>,
    /// The complete hired set, including explicitly unknown or unvisited members.
    #[schemars(length(min = 1, max = 8))]
    pub members: Vec<RepositoryInstructionSnapshot>,
}

/// One hired member's known changes and uncertainty under the common physical boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryInstructionSettlement {
    /// Stable repository identity in hire order.
    pub repo_id: RepoId,
    /// The same physical settlement as every other member, with this member's evidence.
    pub settlement: InstructionSettlement,
}

/// One terminal record for a complete hired set and one exact physical attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryInstructionFinish {
    /// Encoding revision.
    pub v: u32,
    /// Exact authority retained from the start.
    pub binding: RepositoryInstructionBinding,
    /// Physical attempt owner.
    pub owner: InstructionObservationOwner,
    /// Ticket association captured at its start.
    #[serde(deserialize_with = "crate::chat::required_nullable")]
    #[schemars(with = "crate::chat::ExplicitNullable<InstructionTicketAssociation>")]
    pub ticket: Option<InstructionTicketAssociation>,
    /// Exact physical attempt or actual missing boundary.
    pub baseline: InstructionBaselineRef,
    /// The complete hired set, including explicit unknowns.
    #[schemars(length(min = 1, max = 8))]
    pub members: Vec<RepositoryInstructionSettlement>,
}

fn shape<'a>(
    v: u32,
    binding: RepositoryInstructionBinding,
    ticket: Option<InstructionTicketAssociation>,
    ids: impl Iterator<Item = &'a RepoId>,
) -> Result<(), InstructionValidationError> {
    let ids: Vec<_> = ids.collect();
    if v != INSTRUCTION_BATCH_VERSION
        || binding.hired_lsn == Lsn::ZERO
        || ticket.is_some() != binding.ticket_start_lsn.is_some()
        || binding
            .ticket_start_lsn
            .is_some_and(|start| start <= binding.hired_lsn)
        || !(1..=8).contains(&ids.len())
        || ids.iter().collect::<BTreeSet<_>>().len() != ids.len()
    {
        return Err(InstructionValidationError::InvalidBoundary);
    }
    Ok(())
}

fn metadata(record: Record) -> Result<(), InstructionValidationError> {
    let entry = crate::Entry {
        at: dudes_types::Timestamp::from_unix_millis(i64::MIN),
        record,
    };
    if serde_json::to_vec(&entry).is_ok_and(|bytes| bytes.len() <= MAX_INSTRUCTION_METADATA_BYTES) {
        Ok(())
    } else {
        Err(InstructionValidationError::MetadataTooLarge)
    }
}

impl InstructionSampleUsage {
    /// Add work across repositories while retaining maxima for per-path depth limits.
    pub fn checked_add(self, other: Self) -> Result<Self, InstructionValidationError> {
        let invalid = InstructionValidationError::WorkLimit;
        let usage = Self {
            ancestor_steps: self
                .ancestor_steps
                .checked_add(other.ancestor_steps)
                .ok_or(invalid)?,
            descendant_entries: self
                .descendant_entries
                .checked_add(other.descendant_entries)
                .ok_or(invalid)?,
            descendant_depth: self.descendant_depth.max(other.descendant_depth),
            import_edges: self
                .import_edges
                .checked_add(other.import_edges)
                .ok_or(invalid)?,
            symlink_hops: self.symlink_hops.max(other.symlink_hops),
            source_bytes: self
                .source_bytes
                .checked_add(other.source_bytes)
                .ok_or(invalid)?,
        };
        usage.validate()?;
        Ok(usage)
    }
}

impl RepositoryInstructionStart {
    /// Validate structure and one aggregate budget without dropping any hired member.
    pub fn validate(&self) -> Result<(), InstructionValidationError> {
        shape(
            self.v,
            self.binding,
            self.ticket,
            self.members.iter().map(|m| &m.repo_id),
        )?;
        let mut usage = InstructionSampleUsage::default();
        let mut inputs = 0;
        let mut participants = 0;
        let mut gaps = 0;
        let mut inventoried = 0;
        for member in &self.members {
            member.snapshot.validate()?;
            if member.root.is_some() != member.snapshot.execution_folder.is_some() {
                return Err(InstructionValidationError::InvalidBoundary);
            }
            inputs += member.snapshot.inputs.len();
            participants += member.snapshot.participants.len();
            gaps += member.snapshot.gaps.len();
            inventoried += member.snapshot.inventoried.len();
            usage = usage.checked_add(member.snapshot.usage)?;
        }
        if inputs > MAX_INSTRUCTION_INPUTS
            || participants > MAX_INSTRUCTION_PARTICIPANTS
            || gaps > MAX_INSTRUCTION_INPUTS
            || inventoried > MAX_INSTRUCTION_INPUTS
        {
            return Err(InstructionValidationError::WorkLimit);
        }
        metadata(Record::InstructionObservationBatchStarted(self.clone()))
    }
}

impl RepositoryInstructionFinish {
    /// Validate exact physical boundaries and the combined ending evidence budget.
    pub fn validate(&self) -> Result<(), InstructionValidationError> {
        shape(
            self.v,
            self.binding,
            self.ticket,
            self.members.iter().map(|m| &m.repo_id),
        )?;
        let boundary = self.members[0].settlement.clone().map_comparison(|_| ());
        let mut usage = InstructionSampleUsage::default();
        let mut changes = 0;
        let mut gaps = 0;
        for member in &self.members {
            crate::instruction::settlement_valid(self.owner, self.baseline, &member.settlement)?;
            if member.settlement.clone().map_comparison(|_| ()) != boundary {
                return Err(InstructionValidationError::InvalidBoundary);
            }
            if let Some(comparison) = comparison(&member.settlement) {
                changes += comparison.changes.len();
                gaps += comparison.gaps.len();
                usage = usage.checked_add(comparison.usage)?;
            }
        }
        if changes > MAX_INSTRUCTION_INPUTS || gaps > MAX_INSTRUCTION_INPUTS {
            return Err(InstructionValidationError::WorkLimit);
        }
        metadata(Record::InstructionObservationBatchFinished(self.clone()))
    }
}

/// Comparison evidence, when the physical settlement includes a sample.
#[must_use]
pub fn comparison(settlement: &InstructionSettlement) -> Option<&InstructionComparison> {
    match settlement {
        InstructionSettlement::Compared { comparison, .. }
        | InstructionSettlement::Unknown { comparison, .. } => Some(comparison),
        InstructionSettlement::NoIssuedTurn | InstructionSettlement::NoProcess { .. } => None,
    }
}

/// Preceding owning-row facts, borrowed by both projection adapters.
#[derive(Clone, Copy)]
pub struct RepositoryInstructionFacts<'a> {
    /// Recorded owner.
    pub agent: AgentId,
    /// Exact hire authority, possibly retained by an already issued turn.
    pub hire: Option<(Lsn, &'a HireRepositories)>,
    /// Active prospective round or immutable issued round.
    pub round: Option<&'a TicketRepositoryRound>,
    /// Exact existing issue and its grant, only for an issued-attempt owner.
    pub issue: Option<(Lsn, &'a RepositoryTurnGrant)>,
}

/// Validate authority without configuration, filesystem reads or creating an attempt.
pub fn validate_start(
    lsn: Lsn,
    batch: &RepositoryInstructionStart,
    facts: RepositoryInstructionFacts<'_>,
) -> Result<(), InstructionValidationError> {
    batch.validate()?;
    validate_binding(lsn, batch.binding, batch.owner, batch.ticket, facts)?;
    let Some((_, HireRepositories::Scoped(hire))) = facts.hire else {
        return Err(InstructionValidationError::InvalidBoundary);
    };
    if !batch.members.iter().map(|m| &m.repo_id).eq(hire
        .repositories
        .entries()
        .iter()
        .map(|m| &m.repo_id))
    {
        return Err(InstructionValidationError::InvalidBoundary);
    }
    for (member, expected) in batch.members.iter().zip(hire.repositories.entries()) {
        if let Some(root) = member.root
            && (root != expected.value.execution.directory_identity
                || member
                    .snapshot
                    .execution_folder
                    .as_ref()
                    .and_then(|path| path.to_path().ok())
                    .as_deref()
                    != Some(expected.value.execution.directory.as_path()))
        {
            return Err(InstructionValidationError::InvalidBoundary);
        }
        for context in member
            .snapshot
            .inputs
            .iter()
            .map(|input| input.context)
            .chain(member.snapshot.gaps.iter().map(|gap| gap.context))
        {
            if matches!(context, crate::InstructionPathContext::OwnedWorktree { lease } if lease != expected.value.worktree)
            {
                return Err(InstructionValidationError::InvalidBoundary);
            }
        }
    }
    Ok(())
}

/// Validate the frozen hire and ticket association independently of goal billing.
pub fn validate_binding(
    lsn: Lsn,
    binding: RepositoryInstructionBinding,
    owner: InstructionObservationOwner,
    ticket: Option<InstructionTicketAssociation>,
    facts: RepositoryInstructionFacts<'_>,
) -> Result<(), InstructionValidationError> {
    let Some((hired_lsn, HireRepositories::Scoped(_))) = facts.hire else {
        return Err(InstructionValidationError::InvalidBoundary);
    };
    let InstructionObservationOwner::Managed { agent, issue, .. } = owner else {
        return Err(InstructionValidationError::InvalidBoundary);
    };
    if agent != facts.agent || binding.hired_lsn != hired_lsn || hired_lsn >= lsn {
        return Err(InstructionValidationError::InvalidBoundary);
    }
    let round = match issue {
        InstructionIssueAssociation::Prospective => facts.round,
        InstructionIssueAssociation::Issued { issue_lsn } => {
            let Some((actual, grant)) = facts.issue else {
                return Err(InstructionValidationError::InvalidBoundary);
            };
            if actual != issue_lsn || actual >= lsn || grant.hired_lsn != hired_lsn {
                return Err(InstructionValidationError::InvalidBoundary);
            }
            grant.ticket.as_ref()
        }
    };
    match (ticket, round) {
        (None, None) if binding.ticket_start_lsn.is_none() => Ok(()),
        (Some(ticket), Some(round))
            if ticket.card == round.card
                && ticket.goal == round.goal
                && binding.ticket_start_lsn == Some(round.start_lsn)
                && round.start_lsn < lsn
                && round
                    .authority
                    .as_ref()
                    .is_some_and(|authority| authority.hired_lsn == hired_lsn) =>
        {
            Ok(())
        }
        _ => Err(InstructionValidationError::InvalidBoundary),
    }
}

/// Observation owner whose preceding authority the adapter must load.
#[must_use]
pub fn observation_owner(record: &Record) -> Option<InstructionObservationOwner> {
    match record {
        Record::InstructionObservationBatchStarted(batch) => Some(batch.owner),
        Record::InstructionObservationBatchFinished(batch) => Some(batch.owner),
        Record::InstructionObservationStarted { owner, .. }
        | Record::InstructionObservationFinished { owner, .. } => Some(*owner),
        _ => None,
    }
}

/// Validate hired membership at a new start or an explicitly missing historical boundary.
pub fn validate_observation(
    lsn: Lsn,
    record: &Record,
    facts: RepositoryInstructionFacts<'_>,
) -> Result<(), InstructionValidationError> {
    match record {
        Record::InstructionObservationBatchStarted(batch) => validate_start(lsn, batch, facts),
        Record::InstructionObservationBatchFinished(batch) => {
            if matches!(batch.baseline, InstructionBaselineRef::Started { .. }) {
                return Ok(());
            }
            validate_binding(lsn, batch.binding, batch.owner, batch.ticket, facts)?;
            let Some((_, HireRepositories::Scoped(hire))) = facts.hire else {
                return Err(InstructionValidationError::InvalidBoundary);
            };
            if batch.members.iter().map(|member| &member.repo_id).eq(hire
                .repositories
                .entries()
                .iter()
                .map(|member| &member.repo_id))
            {
                Ok(())
            } else {
                Err(InstructionValidationError::InvalidBoundary)
            }
        }
        Record::InstructionObservationStarted { .. }
        | Record::InstructionObservationFinished { .. }
            if matches!(facts.hire, Some((_, HireRepositories::Scoped(_)))) =>
        {
            Err(InstructionValidationError::InvalidBoundary)
        }
        _ => Ok(()),
    }
}

/// Existing completion actions must also retain scoped physical instruction coverage.
#[must_use]
pub fn completion_card(record: &Record) -> Option<dudes_types::CardId> {
    match record {
        Record::CardTransition {
            card,
            to: dudes_types::CardStatus::Done,
            ..
        }
        | Record::TicketInstructionReviewed {
            card,
            action: crate::InstructionReviewAction::Accept { .. },
            ..
        } => Some(*card),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
