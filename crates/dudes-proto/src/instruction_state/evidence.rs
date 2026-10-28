use serde::{Deserialize, Serialize};

use crate::instruction::{InstructionComparison, InstructionSettlement, InstructionSnapshot};
use crate::repository::RepoId;
use crate::repository_instruction::{
    RepositoryInstructionBinding, RepositoryInstructionComparison, RepositoryInstructionFinish,
    RepositoryInstructionStart,
};

/// Starting evidence kept in the attempt's existing immutable snapshot cell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", content = "sample", rename_all = "snake_case")]
pub enum InstructionSnapshotEvidence {
    /// Historical singleton evidence.
    Legacy(InstructionSnapshot),
    /// The exact hired repository set under one physical attempt.
    Repositories(RepositoryInstructionStart),
}

impl InstructionSnapshotEvidence {
    /// Historical singleton sample; scoped evidence never becomes a singleton implicitly.
    #[must_use]
    pub const fn legacy(&self) -> Option<&InstructionSnapshot> {
        match self {
            Self::Legacy(snapshot) => Some(snapshot),
            Self::Repositories(_) => None,
        }
    }

    /// All samples in this exact attempt, retaining repository identity.
    pub fn samples(&self) -> impl Iterator<Item = (Option<&RepoId>, &InstructionSnapshot)> {
        self.legacy()
            .map(|snapshot| (None, snapshot))
            .into_iter()
            .chain(self.repositories().into_iter().flat_map(|batch| {
                batch
                    .members
                    .iter()
                    .map(|member| (Some(&member.repo_id), &member.snapshot))
            }))
    }

    /// The explicit all-hire carrier, when scoped.
    #[must_use]
    pub const fn repositories(&self) -> Option<&RepositoryInstructionStart> {
        match self {
            Self::Repositories(batch) => Some(batch),
            Self::Legacy(_) => None,
        }
    }
}

/// Ending evidence normalized through the same physical lifecycle fold.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", content = "comparisons", rename_all = "snake_case")]
pub enum InstructionComparisonEvidence {
    /// Historical singleton comparison.
    Legacy(InstructionComparison),
    /// Ordered comparison evidence from every hired repository.
    Repositories(Vec<RepositoryInstructionComparison>),
}

impl InstructionComparisonEvidence {
    /// Every retained comparison with its original repository identity.
    pub fn comparisons(&self) -> impl Iterator<Item = (Option<&RepoId>, &InstructionComparison)> {
        let legacy = match self {
            Self::Legacy(value) => Some(value),
            Self::Repositories(_) => None,
        };
        let members = match self {
            Self::Repositories(members) => members.as_slice(),
            Self::Legacy(_) => &[],
        };
        legacy.map(|value| (None, value)).into_iter().chain(
            members
                .iter()
                .map(|member| (Some(&member.repo_id), &member.comparison)),
        )
    }
}

pub(super) fn legacy_settlement(
    settlement: &InstructionSettlement,
) -> InstructionSettlement<InstructionComparisonEvidence> {
    settlement
        .clone()
        .map_comparison(InstructionComparisonEvidence::Legacy)
}

pub(super) fn batch_settlement(
    batch: &RepositoryInstructionFinish,
) -> InstructionSettlement<InstructionComparisonEvidence> {
    let comparisons = batch
        .members
        .iter()
        .filter_map(|member| {
            crate::repository_instruction::comparison(&member.settlement).map(|comparison| {
                RepositoryInstructionComparison {
                    repo_id: member.repo_id.clone(),
                    ending_root: member.ending_root,
                    comparison: comparison.clone(),
                }
            })
        })
        .collect();
    batch.members[0]
        .settlement
        .clone()
        .map_comparison(|_| InstructionComparisonEvidence::Repositories(comparisons))
}

impl super::InstructionAttempt {
    /// Starting uncertainty retains both repository scope and provider declaration provenance.
    pub fn starting_gaps(
        &self,
    ) -> impl Iterator<Item = (Option<&RepoId>, &crate::instruction::InstructionCoverageGap)> {
        self.snapshot
            .as_ref()
            .into_iter()
            .flat_map(InstructionSnapshotEvidence::samples)
            .flat_map(|(repo, sample)| {
                sample
                    .gaps
                    .iter()
                    .chain(
                        sample
                            .participants
                            .iter()
                            .flat_map(|participant| &participant.discovery.gaps),
                    )
                    .map(move |gap| (repo, gap))
            })
    }
    /// Comparable boot identity only when every retained starting sample agrees.
    #[must_use]
    pub fn common_boot(&self) -> Option<&crate::instruction::InstructionBootIdentity> {
        let mut samples = self.snapshot.as_ref()?.samples();
        let first = samples.next()?.1.boot.as_ref()?;
        samples
            .all(|(_, sample)| sample.boot.as_ref() == Some(first))
            .then_some(first)
    }
    /// Legacy sampler callers cannot collapse an all-hire attempt to one repository.
    #[must_use]
    pub fn legacy_snapshot(&self) -> Option<&InstructionSnapshot> {
        self.snapshot
            .as_ref()
            .and_then(InstructionSnapshotEvidence::legacy)
    }

    /// Whether this physical attempt carries an explicit all-hire binding.
    #[must_use]
    pub fn repository_binding(&self) -> Option<RepositoryInstructionBinding> {
        self.snapshot
            .as_ref()
            .and_then(InstructionSnapshotEvidence::repositories)
            .map(|batch| batch.binding)
            .or_else(|| {
                self.finished
                    .as_ref()
                    .and_then(|finished| finished.repository_binding)
            })
    }

    /// Every retained ending comparison, without flattening repository path identities.
    pub fn comparisons(&self) -> impl Iterator<Item = (Option<&RepoId>, &InstructionComparison)> {
        let evidence = self
            .finished
            .as_ref()
            .and_then(|finished| match &finished.settlement {
                InstructionSettlement::Compared { comparison, .. }
                | InstructionSettlement::Unknown { comparison, .. } => Some(comparison),
                _ => None,
            });
        evidence
            .into_iter()
            .flat_map(InstructionComparisonEvidence::comparisons)
    }

    /// Scoped acceptance requires complete starting and ending coverage for a launched interval.
    #[must_use]
    pub fn coverage_unknown(&self) -> bool {
        if self.repository_binding().is_none() {
            return false;
        }
        match self.finished.as_ref().map(|finished| &finished.settlement) {
            Some(InstructionSettlement::NoIssuedTurn | InstructionSettlement::NoProcess { .. }) => {
                false
            }
            Some(InstructionSettlement::Compared { comparison, .. }) => {
                let Some(start) = self
                    .snapshot
                    .as_ref()
                    .and_then(InstructionSnapshotEvidence::repositories)
                else {
                    return true;
                };
                let InstructionComparisonEvidence::Repositories(members) = comparison else {
                    return true;
                };
                self.starting_gaps().next().is_some()
                    || !members
                        .iter()
                        .map(|member| &member.repo_id)
                        .eq(start.members.iter().map(|member| &member.repo_id))
                    || members.iter().zip(&start.members).any(|(ending, before)| {
                        ending.ending_root.is_none() || ending.ending_root != before.root
                    })
                    || self
                        .comparisons()
                        .any(|(_, comparison)| !comparison.gaps.is_empty())
            }
            Some(InstructionSettlement::Unknown { .. }) | None => true,
        }
    }
}
