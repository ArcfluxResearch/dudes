//! Runs a ticket round's baseline and stamp observations off the loop thread and posts each
//! answer back as an input. An answer is always posted: the loop's next step waits for it.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use dudes_proto::DiffRefusal;
use dudes_proto::diff_stamp::{BaselinePayload, TicketBinding};
use dudes_supervisor::host::Clock;
use dudes_supervisor::{Input, Supervisor};
use dudes_types::{AgentId, CardId, GoalId, Lsn};

use super::{Ctx, PredicateRunner, ProcessHost, Shared};
use crate::diff_capture::DiffCapture;
use crate::floor_git::GitTarget;

impl<H: ProcessHost, P: PredicateRunner, C: Clock> Ctx<H, P, C> {
    pub(super) fn execute_ticket_capture<R: dudes_supervisor::host::Rng>(
        &self,
        effect: &dudes_supervisor::Effect,
        sup: &Supervisor<Shared<C>, R>,
        queue: &mut VecDeque<Input>,
    ) {
        match effect {
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
