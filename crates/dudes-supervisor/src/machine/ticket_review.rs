//! Review metadata for a ticket round: the baseline observed before the round's first turn, and
//! the sample taken after each predicate result.
//!
//! Both observations are effects the daemon runs and answers; the machine journals the answer
//! and only then runs the continuation it parked, so the first turn waits for the baseline and
//! the next decision waits for the stamp. Nothing here reads a file or a clock.

use std::collections::{BTreeMap, BTreeSet};

use crate::effect::RepositoryCapture;
use dudes_proto::diff_stamp::{BaselinePayload, StampPayload};
use dudes_proto::repository::{HireRepositories, RepositoryTurnGrant};
use dudes_proto::repository_review::{RepositoryBaselineBatch, RepositoryStampBatch};

use super::{AgentId, Clock, Effect, GoalId, Lsn, Record, Rng, Supervisor, Then};

#[derive(Debug, Default)]
pub(super) struct State {
    baselines_requested: BTreeSet<GoalId>,
    repository_baselines: BTreeMap<GoalId, RepositoryCapture>,
    stamps_parked: BTreeMap<GoalId, StampWait>,
}

#[derive(Debug)]
enum StampWait {
    Legacy(Lsn, Box<Then>),
    Repositories(Box<RepositoryCapture>, Lsn, Box<Then>),
}

impl<C: Clock, R: Rng> Supervisor<C, R> {
    fn repository_capture(&self, agent: AgentId) -> Option<RepositoryCapture> {
        let row = self.world.agents.get(&agent).filter(|row| !row.retired)?;
        let HireRepositories::Scoped(hire) = &row.repositories else {
            return None;
        };
        if hire.policy.require_confined().is_err() {
            return None;
        }
        let round = self.world.active_repository_round(agent).ok()??;
        let review = self.world.repository_review(round.goal)?;
        Some(RepositoryCapture {
            binding: review.binding,
            grant: RepositoryTurnGrant::select(row.mcp.hire_lsn, hire, Some(round)).ok()?,
        })
    }

    pub(super) fn hold_for_repository_baseline(&mut self, agent: AgentId) -> bool {
        let round = match self.world.active_repository_round(agent) {
            Ok(Some(round)) => round,
            Ok(None) => return false,
            Err(_) => return true,
        };
        let goal = round.goal;
        let Some(review) = self.world.repository_review(goal) else {
            return true;
        };
        if review
            .baseline
            .as_ref()
            .is_some_and(|(_, batch)| batch.complete())
        {
            return false;
        }
        if review.first_issue_lsn.is_some()
            || self.ticket_review.repository_baselines.contains_key(&goal)
        {
            return true;
        }
        if let Some(capture) = self.repository_capture(agent) {
            self.ticket_review
                .repository_baselines
                .insert(goal, capture.clone());
            self.pending.push(Effect::CaptureRepositoryBaseline {
                capture: Box::new(capture),
            });
        }
        true
    }

    pub(super) fn hold_for_repository_stamp(&mut self, agent: AgentId) -> bool {
        let waiting = self
            .world
            .active_repository_round(agent)
            .ok()
            .flatten()
            .and_then(|round| {
                let review = self.world.repository_review(round.goal)?;
                let check = review.check?;
                review
                    .stamp
                    .as_ref()
                    .is_none_or(|(_, stamp)| stamp.check_lsn != check.lsn || !stamp.complete())
                    .then_some((round.goal, check.lsn, review.stamp.is_none()))
            });
        let Some((goal, check_lsn, missing)) = waiting else {
            return false;
        };
        if missing && self.repository_admitted(agent) {
            let _ = self.request_repository_stamp(goal, check_lsn, Then::Decide { agent });
        }
        true
    }

    fn request_repository_stamp(
        &mut self,
        goal: GoalId,
        check_lsn: Lsn,
        then: Then,
    ) -> Option<Then> {
        let request = self.world.repository_review(goal).and_then(|review| {
            let capture = self.repository_capture(review.binding.ticket.claimant)?;
            let (baseline_lsn, baseline) = review.baseline.as_ref()?;
            (capture.binding == review.binding
                && baseline.complete()
                && review.check.is_some_and(|check| check.lsn == check_lsn))
            .then(|| (capture, *baseline_lsn, baseline.clone()))
        });
        let Some((capture, baseline_lsn, baseline)) = request else {
            return Some(then);
        };
        if let Some(StampWait::Repositories(current, current_lsn, current_then)) =
            self.ticket_review.stamps_parked.get_mut(&goal)
            && current.as_ref() == &capture
            && *current_lsn == check_lsn
        {
            if matches!(then, Then::InstructionRecoveryContinue) {
                **current_then = then;
            }
            return None;
        }
        self.pending.push(Effect::CaptureRepositoryStamp {
            capture: Box::new(capture.clone()),
            baseline_lsn,
            baseline: Box::new(baseline),
            check_lsn,
        });
        self.ticket_review.stamps_parked.insert(
            goal,
            StampWait::Repositories(Box::new(capture), check_lsn, Box::new(then)),
        );
        None
    }

    fn settle_repository_hold(
        &mut self,
        then: Then,
        check_lsn: Lsn,
        operation: &crate::RecoveryOperation<'_>,
    ) -> Option<crate::input::RecoveryUpdate> {
        if matches!(then, Then::InstructionRecoveryContinue) {
            self.resume_after(then, check_lsn, operation)
        } else {
            None
        }
    }

    pub(super) fn on_repository_baseline_captured(
        &mut self,
        capture: &RepositoryCapture,
        baseline: RepositoryBaselineBatch,
    ) {
        let goal = capture.binding.ticket.goal;
        if self.ticket_review.repository_baselines.get(&goal) != Some(capture) {
            return;
        }
        let agent = capture.binding.ticket.claimant;
        if !self.repository_admitted(agent)
            || self.repository_capture(agent).as_ref() != Some(capture)
        {
            self.ticket_review.repository_baselines.remove(&goal);
            return;
        }
        if baseline.binding != capture.binding {
            return;
        }
        let complete = baseline.complete();
        let entry = super::Entry {
            at: self.clock.now(),
            record: Record::TicketBaselineBatchRecorded(baseline),
        };
        if self.world.validate_review_record(&entry).is_ok() {
            self.ticket_review.repository_baselines.remove(&goal);
            self.persist_at(
                entry.at,
                entry.record,
                if complete {
                    Then::ClearedCause { agent }
                } else {
                    Then::Fold
                },
            );
        }
    }

    /// Whether the round's first turn must wait for its baseline. Asks for the observation once.
    ///
    /// Only a ticket goal that has neither a recorded baseline nor ever had a turn is owed one:
    /// work already issued keeps the checkout it was issued against and never gains a late
    /// baseline.
    pub(super) fn hold_for_ticket_baseline(&mut self, agent: AgentId) -> bool {
        let Some(goal) = self
            .world
            .agents
            .get(&agent)
            .filter(|row| !row.retired)
            .and_then(|row| row.goal)
        else {
            return false;
        };
        if self
            .world
            .goals
            .get(&goal)
            .is_none_or(|row| row.cleared || row.agent != agent)
        {
            return false;
        }
        let Some((card, round)) = self.world.ticket_goal(goal) else {
            return false;
        };
        if self.world.ticket_review(goal).is_some() || self.world.goal_has_turn(goal) {
            return false;
        }
        if self.ticket_review.baselines_requested.insert(goal) {
            self.pending.push(Effect::CaptureBaseline {
                agent,
                card,
                round,
                goal,
            });
        }
        true
    }

    pub(super) fn on_baseline_captured(&mut self, goal: GoalId, baseline: BaselinePayload) {
        self.ticket_review.baselines_requested.remove(&goal);
        let Some((card, round)) = self.world.ticket_goal(goal) else {
            return;
        };
        let Some(row) = self.world.goals.get(&goal).filter(|row| !row.cleared) else {
            return;
        };
        let agent = row.agent;
        let binding = baseline.binding;
        if self.world.agents.get(&agent).and_then(|row| row.goal) != Some(goal)
            || binding.card != card
            || binding.round != round
            || binding.goal != goal
            || binding.claimant != agent
            || self.world.ticket_review(goal).is_some()
            || self.world.goal_has_turn(goal)
        {
            return;
        }
        self.persist(
            Record::TicketBaselineRecorded {
                card,
                goal,
                baseline,
            },
            Then::ClearedCause { agent },
        );
    }

    /// Route a predicate result's continuation through the sample when the goal has a baseline.
    pub(super) fn after_predicate_result(&self, goal: GoalId, then: Then) -> Then {
        if self.world.ticket_review(goal).is_some() || self.world.repository_review(goal).is_some()
        {
            Then::TicketStamp {
                goal,
                then: Box::new(then),
            }
        } else {
            then
        }
    }

    pub(super) fn request_stamp(
        &mut self,
        goal: GoalId,
        then: Then,
        check_lsn: Lsn,
        operation: &crate::RecoveryOperation<'_>,
    ) -> Option<crate::input::RecoveryUpdate> {
        if self.world.repository_review(goal).is_some() {
            return self
                .request_repository_stamp(goal, check_lsn, then)
                .and_then(|held| self.settle_repository_hold(held, check_lsn, operation));
        }

        let request = self.world.ticket_goal(goal).and_then(|(card, round)| {
            let agent = self
                .world
                .goals
                .get(&goal)
                .filter(|row| !row.cleared)?
                .agent;
            let review = self.world.ticket_review(goal)?;
            Some(Effect::CaptureStamp {
                agent,
                card,
                round,
                goal,
                baseline_lsn: review.baseline_lsn,
                baseline: review.baseline,
                check_lsn,
            })
        });
        match request {
            Some(effect) => {
                self.ticket_review
                    .stamps_parked
                    .insert(goal, StampWait::Legacy(check_lsn, Box::new(then)));
                self.pending.push(effect);
                None
            }
            None => self.resume_after(then, check_lsn, operation),
        }
    }

    pub(super) fn on_stamp_captured(
        &mut self,
        goal: GoalId,
        check_lsn: Lsn,
        stamp: StampPayload,
        operation: &crate::RecoveryOperation<'_>,
    ) -> Option<crate::input::RecoveryUpdate> {
        let parked = self.ticket_review.stamps_parked.remove(&goal)?;
        let StampWait::Legacy(parked_lsn, then) = parked else {
            self.ticket_review.stamps_parked.insert(goal, parked);
            return None;
        };
        if parked_lsn != check_lsn {
            self.ticket_review
                .stamps_parked
                .insert(goal, StampWait::Legacy(parked_lsn, then));
            return None;
        }
        let record = self.world.ticket_goal(goal).and_then(|(card, _)| {
            let review = self.world.ticket_review(goal)?;
            (stamp.check_lsn == check_lsn
                && stamp.baseline_lsn == review.baseline_lsn
                && stamp.binding == review.baseline.binding)
                .then_some(Record::TicketReviewStamped { card, goal, stamp })
        });
        match record {
            Some(record) => {
                self.persist(record, *then);
                None
            }
            None => self.resume_after(*then, check_lsn, operation),
        }
    }

    pub(super) fn on_repository_stamp_captured(
        &mut self,
        capture: &RepositoryCapture,
        stamp: RepositoryStampBatch,
        operation: &crate::RecoveryOperation<'_>,
    ) -> Option<crate::input::RecoveryUpdate> {
        let goal = capture.binding.ticket.goal;
        let Some(StampWait::Repositories(expected, check_lsn, _)) =
            self.ticket_review.stamps_parked.get(&goal)
        else {
            return None;
        };
        if expected.as_ref() != capture
            || *check_lsn != stamp.check_lsn
            || stamp.binding != capture.binding
        {
            return None;
        }
        if !self.repository_admitted(capture.binding.ticket.claimant)
            || self
                .repository_capture(capture.binding.ticket.claimant)
                .as_ref()
                != Some(capture)
        {
            let StampWait::Repositories(_, check_lsn, then) =
                self.ticket_review.stamps_parked.remove(&goal)?
            else {
                return None;
            };
            return self.settle_repository_hold(*then, check_lsn, operation);
        }
        let complete = stamp.complete();
        let entry = super::Entry {
            at: self.clock.now(),
            record: Record::TicketReviewBatchStamped(stamp),
        };
        if self.world.validate_review_record(&entry).is_err() {
            return None;
        }
        let Some(StampWait::Repositories(_, _, then)) =
            self.ticket_review.stamps_parked.remove(&goal)
        else {
            return None;
        };
        self.persist_at(
            entry.at,
            entry.record,
            if complete || matches!(*then, Then::InstructionRecoveryContinue) {
                *then
            } else {
                Then::Fold
            },
        );
        None
    }
}
