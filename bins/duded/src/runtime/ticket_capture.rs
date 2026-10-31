//! Runs a ticket round's baseline and stamp observations off the loop thread and posts each
//! answer back as an input. An answer is always posted: the loop's next step waits for it.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use dudes_proto::DiffRefusal;
use dudes_proto::diff_stamp::BaselineOutcome;
use dudes_proto::diff_stamp::{BaselinePayload, TicketBinding};
use dudes_proto::repository_review::RepositoryBaselineBatch;
use dudes_supervisor::effect::RepositoryCapture;
use dudes_supervisor::host::Clock;
use dudes_supervisor::{Input, Supervisor};
use dudes_types::{AgentId, CardId, GoalId, Lsn};

use super::{Ctx, PredicateRunner, ProcessHost, Shared};
use crate::diff_capture::DiffCapture;
use crate::floor_git::GitTarget;
use crate::git_read::Tree;
use crate::git_read::diff::{BaselineCapture, ReviewSubject, RootIdentity};

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    pub(super) fn execute_ticket_capture<R: dudes_supervisor::host::Rng>(
        &self,
        effect: &dudes_supervisor::Effect,
        sup: &Supervisor<Shared<C>, R>,
        queue: &mut VecDeque<Input>,
    ) {
        match effect {
            dudes_supervisor::Effect::CaptureRepositoryBaseline { capture } => {
                let subjects = repository_subjects(sup.world(), capture, None);
                let service = Arc::clone(&self.ticket_capture);
                let request = capture.clone();
                let unavailable: Arc<dyn Fn() -> Input + Send + Sync> = {
                    let service = Arc::clone(&service);
                    let capture = capture.clone();
                    Arc::new(move || Input::RepositoryBaselineCaptured {
                        capture: capture.clone(),
                        baseline: service.baseline_batch_unavailable(
                            &capture.grant.repositories,
                            capture.binding,
                        ),
                    })
                };
                let cancelled = self.handle.stopping_flag();
                let failed = Arc::clone(&unavailable);
                self.answer_capture(
                    format!("dudes-baseline-{}", capture.binding.ticket.goal),
                    move || {
                        let Some(subjects) = subjects else {
                            return failed();
                        };
                        let baseline =
                            service.baseline_batch(&subjects, request.binding, cancelled);
                        Input::RepositoryBaselineCaptured {
                            capture: request,
                            baseline,
                        }
                    },
                    &unavailable,
                    queue,
                );
            }
            dudes_supervisor::Effect::CaptureRepositoryStamp {
                capture,
                baseline_lsn,
                baseline,
                check_lsn,
            } => {
                self.capture_repository_stamp(
                    sup.world(),
                    capture,
                    *baseline_lsn,
                    baseline,
                    *check_lsn,
                    queue,
                );
            }
            dudes_supervisor::Effect::CaptureBaseline {
                agent,
                card,
                round,
                goal,
            } => self.capture_baseline(sup, (*agent, *card, *round, *goal), queue),
            dudes_supervisor::Effect::CaptureStamp {
                agent,
                card,
                round,
                goal,
                baseline_lsn,
                baseline,
                check_lsn,
            } => self.capture_stamp(
                sup,
                (*agent, *card, *round, *goal),
                (*baseline_lsn, *baseline, *check_lsn),
                queue,
            ),
            _ => {}
        }
    }

    fn capture_repository_stamp(
        &self,
        world: &dudes_supervisor::World,
        capture: &RepositoryCapture,
        baseline_lsn: Lsn,
        baseline: &RepositoryBaselineBatch,
        check_lsn: Lsn,
        queue: &mut VecDeque<Input>,
    ) {
        let subjects = repository_subjects(world, capture, Some(baseline));
        let service = Arc::clone(&self.ticket_capture);
        let request = Box::new(capture.clone());
        let unavailable: Arc<dyn Fn() -> Input + Send + Sync> = {
            let service = Arc::clone(&service);
            let capture = Box::new(capture.clone());
            Arc::new(move || Input::RepositoryStampCaptured {
                capture: capture.clone(),
                stamp: service.stamp_batch_unavailable(
                    &capture.grant.repositories,
                    capture.binding,
                    baseline_lsn,
                    check_lsn,
                ),
            })
        };
        let cancelled = self.handle.stopping_flag();
        let failed = Arc::clone(&unavailable);
        self.answer_capture(
            format!("dudes-stamp-{}", capture.binding.ticket.goal),
            move || {
                let Some(subjects) = subjects else {
                    return failed();
                };
                let stamp = service.stamp_batch(
                    &subjects,
                    request.binding,
                    baseline_lsn,
                    check_lsn,
                    cancelled,
                );
                Input::RepositoryStampCaptured {
                    capture: request,
                    stamp,
                }
            },
            &unavailable,
            queue,
        );
    }

    fn capture_baseline<R: dudes_supervisor::host::Rng>(
        &self,
        sup: &Supervisor<Shared<C>, R>,
        (agent, card, round, goal): (AgentId, CardId, u32, GoalId),
        queue: &mut VecDeque<Input>,
    ) {
        let binding = TicketBinding {
            card,
            round,
            goal,
            claimant: agent,
        };
        let target = GitTarget::of(sup.world(), agent);
        let service = Arc::clone(&self.ticket_capture);
        let stopping = self.handle.stopping_flag();
        let unavailable: Arc<dyn Fn() -> Input + Send + Sync> = {
            let service = Arc::clone(&service);
            Arc::new(move || Input::BaselineCaptured {
                goal,
                baseline: service.baseline_unavailable(binding, DiffRefusal::Unreadable),
            })
        };
        let observe = move || baseline_input(&service, target.as_ref(), binding, goal, stopping);
        self.answer_capture(
            format!("dudes-baseline-{goal}"),
            observe,
            &unavailable,
            queue,
        );
    }

    fn capture_stamp<R: dudes_supervisor::host::Rng>(
        &self,
        sup: &Supervisor<Shared<C>, R>,
        (agent, card, round, goal): (AgentId, CardId, u32, GoalId),
        (baseline_lsn, baseline, check_lsn): (Lsn, BaselinePayload, Lsn),
        queue: &mut VecDeque<Input>,
    ) {
        let binding = TicketBinding {
            card,
            round,
            goal,
            claimant: agent,
        };
        let target = GitTarget::of(sup.world(), agent);
        let service = Arc::clone(&self.ticket_capture);
        let stopping = self.handle.stopping_flag();
        let unavailable: Arc<dyn Fn() -> Input + Send + Sync> = {
            let service = Arc::clone(&service);
            Arc::new(move || Input::StampCaptured {
                goal,
                check_lsn,
                stamp: service.stamp_unavailable(
                    binding,
                    baseline_lsn,
                    check_lsn,
                    DiffRefusal::Unreadable,
                ),
            })
        };
        let observe = move || {
            stamp_input(
                &service,
                target.as_ref(),
                (&baseline, baseline_lsn, check_lsn),
                binding,
                stopping,
            )
        };
        self.answer_capture(format!("dudes-stamp-{goal}"), observe, &unavailable, queue);
    }

    fn answer_capture(
        &self,
        name: String,
        observe: impl FnOnce() -> Input + Send + 'static,
        unavailable: &Arc<dyn Fn() -> Input + Send + Sync>,
        queue: &mut VecDeque<Input>,
    ) {
        let handle = self.handle.clone();
        let on_panic = Arc::clone(unavailable);
        let spawned = std::thread::Builder::new().name(name).spawn(move || {
            let input = catch_unwind(AssertUnwindSafe(observe)).unwrap_or_else(|_| on_panic());
            handle.input(input);
        });
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start a review capture thread");
            queue.push_back(unavailable());
        }
    }
}

fn repository_subjects(
    world: &dudes_supervisor::World,
    capture: &RepositoryCapture,
    baseline: Option<&RepositoryBaselineBatch>,
) -> Option<Vec<ReviewSubject>> {
    let row = world.agents.get(&capture.binding.ticket.claimant)?;
    if row.retired
        || row.mcp.hire_lsn != capture.binding.hired_lsn
        || capture.grant.hired_lsn != capture.binding.hired_lsn
    {
        return None;
    }
    let dudes_proto::repository::HireRepositories::Scoped(hire) = &row.repositories else {
        return None;
    };
    let full_hire = hire.recorded();
    capture
        .grant
        .repositories
        .entries()
        .iter()
        .map(|entry| {
            let baseline = baseline.and_then(|batch| captured_baseline(batch, &entry.repo_id));
            Some(ReviewSubject {
                repo_id: Some(entry.repo_id.clone()),
                tree: Tree::recorded(&entry.value, &full_hire).ok()?,
                baseline,
                stamp: None,
            })
        })
        .collect()
}

/// The round-start sample a baseline batch recorded for `repo_id`, when it captured one.
fn captured_baseline(
    batch: &RepositoryBaselineBatch,
    repo_id: &dudes_proto::repository::RepoId,
) -> Option<BaselineCapture> {
    let member = batch
        .members
        .iter()
        .find(|member| &member.repo_id == repo_id)?;
    match member.outcome {
        BaselineOutcome::Captured {
            device,
            inode,
            head,
            preexisting_tracked,
            preexisting_untracked,
        } => Some(BaselineCapture {
            root: RootIdentity { device, inode },
            head,
            preexisting_tracked,
            preexisting_untracked,
        }),
        _ => None,
    }
}

/// What it takes to ask whether one copy of an unconfined hire holds committed work.
pub(super) struct CommitProbe {
    tree: Tree,
    baseline: BaselineCapture,
}

impl CommitProbe {
    pub(super) fn committed_since(
        &self,
        service: &DiffCapture,
        cancelled: Arc<AtomicBool>,
    ) -> Option<bool> {
        service
            .committed_since(&self.tree, &self.baseline, cancelled)
            .ok()
    }
}

/// One probe per granted copy, in grant order, for the files-changed evidence of an unconfined
/// hire. A copy without a recorded baseline has none, so its commits read as unknown.
pub(super) fn commit_probes(
    world: &dudes_supervisor::World,
    goal: GoalId,
    hire: &dudes_proto::repository::ScopedHireRepositories,
    grant: &dudes_proto::repository::RepositoryTurnGrant,
) -> Vec<Option<CommitProbe>> {
    let full_hire = hire.recorded();
    let baseline = world
        .repository_review(goal)
        .and_then(|review| review.baseline.as_ref())
        .map(|(_, batch)| batch);
    grant
        .repositories
        .entries()
        .iter()
        .map(|entry| {
            Some(CommitProbe {
                baseline: captured_baseline(baseline?, &entry.repo_id)?,
                tree: Tree::recorded(&entry.value, &full_hire).ok()?,
            })
        })
        .collect()
}

/// When each granted copy's round baseline finished, in grant order. A copy with no captured
/// baseline has none, so every uncommitted file in it counts.
pub(super) fn baseline_times(
    world: &dudes_supervisor::World,
    goal: GoalId,
    grant: &dudes_proto::repository::RepositoryTurnGrant,
) -> Vec<Option<std::time::SystemTime>> {
    let batch = world
        .repository_review(goal)
        .and_then(|review| review.baseline.as_ref())
        .map(|(_, batch)| batch);
    let ids = grant
        .repositories
        .entries()
        .iter()
        .map(|entry| &entry.repo_id);
    captured_times(batch, ids)
}

fn captured_times<'a>(
    batch: Option<&RepositoryBaselineBatch>,
    repos: impl Iterator<Item = &'a dudes_proto::repository::RepoId>,
) -> Vec<Option<std::time::SystemTime>> {
    repos
        .map(|repo_id| {
            let member = batch?
                .members
                .iter()
                .find(|member| &member.repo_id == repo_id)?;
            captured_at(&member.outcome, member.interval.ended_at)
        })
        .collect()
}

/// When the round baseline of a single-folder hire finished, if it captured one.
pub(super) fn legacy_baseline_time(
    world: &dudes_supervisor::World,
    goal: GoalId,
) -> Option<std::time::SystemTime> {
    let baseline = &world.ticket_review(goal)?.baseline;
    captured_at(&baseline.outcome, baseline.interval.ended_at)
}

fn captured_at(
    outcome: &BaselineOutcome,
    ended_at: dudes_types::Timestamp,
) -> Option<std::time::SystemTime> {
    let millis = u64::try_from(ended_at.unix_millis()).ok()?;
    // The record keeps whole milliseconds, which round down; the next one is the earliest moment a
    // file written by the round can carry.
    matches!(outcome, BaselineOutcome::Captured { .. })
        .then(|| std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis + 1))
}

/// The baseline a round records for `target`. A target that cannot be read records an
/// unavailable one, and a capture the daemon's stop cancels records the stop's timeout.
fn baseline_input(
    service: &DiffCapture,
    target: Option<&GitTarget>,
    binding: TicketBinding,
    goal: GoalId,
    cancelled: Arc<AtomicBool>,
) -> Input {
    let unreadable = || service.baseline_unavailable(binding, DiffRefusal::Unreadable);
    let baseline = match target {
        Some(GitTarget::Read(tree)) => service
            .baseline(tree, binding, cancelled)
            .unwrap_or_else(|_| unreadable()),
        _ => unreadable(),
    };
    Input::BaselineCaptured { goal, baseline }
}

/// The stamp a check's result records for `target`, with the same fallbacks as
/// [`baseline_input`].
fn stamp_input(
    service: &DiffCapture,
    target: Option<&GitTarget>,
    (baseline, baseline_lsn, check_lsn): (&BaselinePayload, Lsn, Lsn),
    binding: TicketBinding,
    cancelled: Arc<AtomicBool>,
) -> Input {
    let unreadable =
        || service.stamp_unavailable(binding, baseline_lsn, check_lsn, DiffRefusal::Unreadable);
    let stamp = match target {
        Some(GitTarget::Read(tree)) => service
            .stamp(tree, baseline, baseline_lsn, binding, check_lsn, cancelled)
            .unwrap_or_else(|_| unreadable()),
        _ => unreadable(),
    };
    Input::StampCaptured {
        goal: binding.goal,
        check_lsn,
        stamp,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    use dudes_proto::diff_stamp::BaselineOutcome;
    use dudes_types::Timestamp;

    use super::*;
    use crate::git_read::Tree;
    use crate::git_read::diff::DiffReader;
    use crate::ports::Handle;
    use crate::predicate::{PredicateCaptured, PredicateOutcome, PredicateRunner};

    struct NeverRuns;
    impl PredicateRunner for NeverRuns {
        fn run(
            &self,
            _spec: &dudes_providers::launch::LaunchSpec,
            _timeout: Duration,
        ) -> io::Result<PredicateOutcome> {
            panic!("a cancelled capture ran a program")
        }
        fn run_captured(
            &self,
            _spec: &dudes_providers::launch::LaunchSpec,
            _timeout: Duration,
            _cap: usize,
        ) -> io::Result<PredicateCaptured> {
            panic!("a cancelled capture ran a program")
        }
        fn sweep(&self) {}
    }

    fn service(scratch: &std::path::Path) -> DiffCapture {
        DiffCapture::new(
            Arc::new(DiffReader::new(Arc::new(NeverRuns), None, scratch)),
            Box::new(|| Timestamp::from_unix_millis(7)),
        )
    }

    fn binding() -> TicketBinding {
        TicketBinding {
            card: CardId::from_parts(1, 1),
            round: 1,
            goal: GoalId::from_parts(1, 2),
            claimant: AgentId::from_parts(1, 3),
        }
    }

    fn outcome(input: &Input) -> BaselineOutcome {
        let Input::BaselineCaptured { baseline, .. } = input else {
            panic!("a baseline answer");
        };
        baseline.outcome
    }

    #[test]
    fn a_copy_is_dated_only_by_a_baseline_that_was_captured() {
        use dudes_proto::diff_stamp::{CaptureInterval, SamplerVersions};
        use dudes_proto::repository::RepoId;
        use dudes_proto::repository_review::{RepositoryBaselineMember, RepositoryReviewBinding};

        let at = |millis| CaptureInterval {
            started_at: Timestamp::from_unix_millis(millis - 5),
            ended_at: Timestamp::from_unix_millis(millis),
        };
        let versions = SamplerVersions {
            sampler: 1,
            policy: 1,
        };
        let member = |name: &str, outcome| RepositoryBaselineMember {
            repo_id: RepoId::parse(name).unwrap(),
            interval: at(5_000),
            versions,
            outcome,
        };
        let batch = RepositoryBaselineBatch {
            v: 1,
            binding: RepositoryReviewBinding {
                ticket: binding(),
                start_lsn: Lsn::ZERO,
                hired_lsn: Lsn::ZERO,
            },
            members: vec![
                member(
                    "web",
                    BaselineOutcome::Captured {
                        device: 1,
                        inode: 2,
                        head: dudes_types::GitCommit::parse(&"ab".repeat(20)).unwrap(),
                        preexisting_tracked: true,
                        preexisting_untracked: false,
                    },
                ),
                member(
                    "api",
                    BaselineOutcome::Unavailable {
                        reason: DiffRefusal::Unreadable,
                    },
                ),
            ],
        };
        let ids = ["web", "api", "docs"].map(|name| RepoId::parse(name).unwrap());
        assert_eq!(
            captured_times(Some(&batch), ids.iter()),
            vec![
                Some(std::time::UNIX_EPOCH + Duration::from_millis(5_001)),
                None,
                None
            ],
            "a captured baseline dates its copy; an unavailable or absent one dates nothing"
        );
        assert_eq!(captured_times(None, ids.iter()), vec![None, None, None]);
    }

    #[test]
    fn a_target_that_cannot_be_read_records_an_unavailable_baseline() {
        let scratch = tempfile::tempdir().unwrap();
        let service = service(scratch.path());
        let unreadable = BaselineOutcome::Unavailable {
            reason: DiffRefusal::Unreadable,
        };
        for target in [None, Some(GitTarget::Released), Some(GitTarget::Removed)] {
            let input = baseline_input(
                &service,
                target.as_ref(),
                binding(),
                binding().goal,
                Arc::default(),
            );
            assert_eq!(outcome(&input), unreadable, "{target:?}");
        }
    }

    #[test]
    fn a_stopping_daemon_cancels_a_capture_that_has_not_finished() {
        let scratch = tempfile::tempdir().unwrap();
        let service = service(scratch.path());
        let (jobs, _loop_side) = mpsc::channel();
        let handle = Handle::new(jobs);
        let flag = handle.stopping_flag();
        let target = GitTarget::Read(Tree::Checkout(PathBuf::from("/no/such/checkout")));
        handle.stop_waiting();
        let input = baseline_input(&service, Some(&target), binding(), binding().goal, flag);
        assert_eq!(
            outcome(&input),
            BaselineOutcome::Unavailable {
                reason: DiffRefusal::TimedOut
            }
        );
    }
}
