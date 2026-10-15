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
        let unavailable: Arc<dyn Fn() -> Input + Send + Sync> = {
            let service = Arc::clone(&service);
            Arc::new(move || Input::BaselineCaptured {
                goal,
                baseline: service.baseline_unavailable(binding, DiffRefusal::Unreadable),
            })
        };
        let observe = move || match &target {
            Some(GitTarget::Read(tree)) => Input::BaselineCaptured {
                goal,
                baseline: service
                    .baseline(tree, binding, Arc::new(AtomicBool::new(false)))
                    .unwrap_or_else(|_| {
                        service.baseline_unavailable(binding, DiffRefusal::Unreadable)
                    }),
            },
            _ => Input::BaselineCaptured {
                goal,
                baseline: service.baseline_unavailable(binding, DiffRefusal::Unreadable),
            },
        };
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
            let stamp = match &target {
                Some(GitTarget::Read(tree)) => service
                    .stamp(
                        tree,
                        &baseline,
                        baseline_lsn,
                        binding,
                        check_lsn,
                        Arc::new(AtomicBool::new(false)),
                    )
                    .unwrap_or_else(|_| {
                        service.stamp_unavailable(
                            binding,
                            baseline_lsn,
                            check_lsn,
                            DiffRefusal::Unreadable,
                        )
                    }),
                _ => service.stamp_unavailable(
                    binding,
                    baseline_lsn,
                    check_lsn,
                    DiffRefusal::Unreadable,
                ),
            };
            Input::StampCaptured {
                goal,
                check_lsn,
                stamp,
            }
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
