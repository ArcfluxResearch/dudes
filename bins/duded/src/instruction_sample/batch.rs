use super::{
    InstructionComparison, InstructionCoverageGap, InstructionGapKind, InstructionSampleUsage,
    InstructionSnapshot, SampleProgress, observe_bounded,
};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use dudes_proto::instruction::{
    InstructionBaselineRef, InstructionEndBoundary, InstructionObservationOwner,
    InstructionPathContext, InstructionSettlement,
};
use dudes_proto::repository_instruction::{
    RepositoryInstructionComparison, RepositoryInstructionFinish, RepositoryInstructionSettlement,
    RepositoryInstructionStart,
};
use dudes_types::{AgentId, Lsn, Timestamp};

pub(crate) struct Progress {
    pub(crate) sample: RepositoryInstructionStart,
    pub(crate) completed: usize,
}

pub(super) struct Budget {
    batch: RepositoryInstructionStart,
    member: usize,
    pub(super) used: InstructionSampleUsage,
    pub(super) agent: AgentId,
}

fn marker(reason: InstructionGapKind) -> InstructionCoverageGap {
    InstructionCoverageGap {
        origin: dudes_proto::InstructionGapOrigin::Observed,
        reason,
        context: InstructionPathContext::Unresolved,
        agent: None,
        path: None,
    }
}

fn reserve(sample: &mut RepositoryInstructionStart, from: usize) {
    for member in sample.members.iter_mut().skip(from) {
        for reason in [
            InstructionGapKind::MetadataLimit,
            InstructionGapKind::ChangedDuringRead,
            InstructionGapKind::Deadline,
            InstructionGapKind::PathUnresolved,
        ] {
            if !member.snapshot.gaps.iter().any(|gap| gap.reason == reason) {
                let _ = member.snapshot.try_push_gap(marker(reason));
            }
        }
    }
}

pub(crate) fn seed_fits(sample: &RepositoryInstructionStart) -> bool {
    let mut reserved = sample.clone();
    reserve(&mut reserved, 0);
    reserved.validate().is_ok()
}

impl Budget {
    pub(super) fn fits(&self, snapshot: &InstructionSnapshot) -> bool {
        let mut batch = self.batch.clone();
        batch.members[self.member].snapshot = snapshot.clone();
        reserve(&mut batch, self.member);
        batch.validate().is_ok()
    }
}

pub(crate) fn observe_reporting(
    mut sample: RepositoryInstructionStart,
    progress: &Arc<Mutex<Progress>>,
    deadline: Instant,
    starting: bool,
) -> RepositoryInstructionStart {
    {
        let mut current = progress.lock().unwrap_or_else(PoisonError::into_inner);
        current.sample = sample.clone();
        current.completed = 0;
        for member in &mut current.sample.members {
            let _ = member
                .snapshot
                .try_push_gap(marker(InstructionGapKind::Deadline));
        }
    }
    let mut used = InstructionSampleUsage::default();
    for index in 0..sample.members.len() {
        let budget = Budget {
            batch: sample.clone(),
            member: index,
            used,
            agent: owner_agent(sample.owner),
        };
        let snapshot = observe_bounded(
            sample.members[index].snapshot.clone(),
            Some(SampleProgress::Repositories {
                progress: Arc::clone(progress),
                member: index,
            }),
            deadline,
            Some(budget),
        );
        used = used
            .checked_add(snapshot.usage)
            .expect("shared instruction work budget");
        sample.members[index].snapshot = snapshot;
        recheck_root(&mut sample.members[index], starting);
        let mut current = progress.lock().unwrap_or_else(PoisonError::into_inner);
        current.sample.members[index] = sample.members[index].clone();
        current.completed = index + 1;
    }
    assert!(
        sample.validate().is_ok(),
        "bounded repository instruction endpoint"
    );
    sample
}

fn owner_agent(owner: InstructionObservationOwner) -> AgentId {
    match owner {
        InstructionObservationOwner::Managed { agent, .. }
        | InstructionObservationOwner::AttachedProcess { agent, .. }
        | InstructionObservationOwner::AttachedTurn { agent, .. } => agent,
    }
}

fn recheck_root(
    member: &mut dudes_proto::repository_instruction::RepositoryInstructionSnapshot,
    starting: bool,
) {
    let actual = member
        .snapshot
        .execution_folder
        .as_ref()
        .and_then(|path| path.to_path().ok())
        .filter(|path| {
            path.canonicalize()
                .is_ok_and(|canonical| canonical == *path)
        })
        .filter(|path| path.is_dir())
        .and_then(|path| crate::worktree::trust::identity(&path).ok());
    if member.root != actual {
        member.root = if starting { None } else { actual };
        if member.root.is_none() {
            member.snapshot.execution_folder = None;
        }
        let reason = if member.root.is_none() {
            InstructionGapKind::PathUnresolved
        } else {
            InstructionGapKind::ChangedDuringRead
        };
        let _ = member.snapshot.try_push_gap(marker(reason));
    }
}

pub(crate) fn deadline(
    progress: &Mutex<Progress>,
    sampled_at: Timestamp,
) -> RepositoryInstructionStart {
    let current = progress.lock().unwrap_or_else(PoisonError::into_inner);
    let mut sample = current.sample.clone();
    for member in sample.members.iter_mut().skip(current.completed) {
        member.root = None;
        member.snapshot.execution_folder = None;
        member.snapshot.sampled_at = sampled_at;
        let _ = member
            .snapshot
            .try_push_gap(marker(InstructionGapKind::PathUnresolved));
        let _ = member
            .snapshot
            .try_push_gap(marker(InstructionGapKind::Deadline));
    }
    sample
}

fn finish_fits(
    sample: &RepositoryInstructionStart,
    comparisons: &[RepositoryInstructionComparison],
    reserve_gaps: bool,
) -> bool {
    let InstructionObservationOwner::Managed { turn, .. } = sample.owner else {
        return false;
    };
    let finish = RepositoryInstructionFinish {
        v: sample.v,
        binding: sample.binding,
        owner: sample.owner,
        ticket: sample.ticket,
        baseline: InstructionBaselineRef::Started { lsn: Lsn(u64::MAX) },
        members: comparisons
            .iter()
            .map(|member| {
                let mut comparison = member.comparison.clone();
                if reserve_gaps
                    && !comparison
                        .gaps
                        .iter()
                        .any(|gap| gap.reason == InstructionGapKind::MetadataLimit)
                {
                    let _ = comparison.try_push_gap(InstructionCoverageGap::metadata_limit());
                }
                RepositoryInstructionSettlement {
                    repo_id: member.repo_id.clone(),
                    ending_root: member.ending_root,
                    settlement: InstructionSettlement::Compared {
                        end: InstructionEndBoundary::ManagedTurn {
                            turn,
                            end_lsn: Lsn(u64::MAX),
                        },
                        sampled_at: Timestamp::from_unix_millis(i64::MIN),
                        recovery: false,
                        comparison,
                    },
                }
            })
            .collect(),
    };
    finish.validate().is_ok()
}

pub(crate) fn compare(
    before: &RepositoryInstructionStart,
    after: &RepositoryInstructionStart,
) -> Vec<RepositoryInstructionComparison> {
    let full: Vec<_> = before
        .members
        .iter()
        .zip(&after.members)
        .map(|(before, after)| super::compare(&before.snapshot, &after.snapshot))
        .collect();
    let mut comparisons: Vec<_> = after
        .members
        .iter()
        .map(|member| RepositoryInstructionComparison {
            repo_id: member.repo_id.clone(),
            ending_root: member.root,
            comparison: InstructionComparison {
                changes: Vec::new(),
                gaps: if member.root.is_none() {
                    vec![marker(InstructionGapKind::PathUnresolved)]
                } else {
                    Vec::new()
                },
                usage: member.snapshot.usage,
            },
        })
        .collect();
    for (index, full) in full.into_iter().enumerate() {
        for change in full.changes {
            comparisons[index].comparison.changes.push(change);
            if !finish_fits(after, &comparisons, true) {
                comparisons[index].comparison.changes.pop();
                let _ = comparisons[index]
                    .comparison
                    .try_push_gap(InstructionCoverageGap::metadata_limit());
                break;
            }
        }
        for gap in full.gaps {
            comparisons[index].comparison.gaps.push(gap);
            if !finish_fits(after, &comparisons, true) {
                comparisons[index].comparison.gaps.pop();
                let _ = comparisons[index]
                    .comparison
                    .try_push_gap(InstructionCoverageGap::metadata_limit());
                break;
            }
        }
    }
    assert!(
        finish_fits(after, &comparisons, false),
        "bounded repository instruction comparison"
    );
    comparisons
}

#[cfg(all(test, unix))]
#[path = "batch/tests.rs"]
mod tests;
