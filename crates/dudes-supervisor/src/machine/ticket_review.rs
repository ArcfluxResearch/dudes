//! Review metadata for a ticket round: the baseline observed before the round's first turn, and
//! the sample taken after each predicate result.
//!
//! Both observations are effects the daemon runs and answers; the machine journals the answer
//! and only then runs the continuation it parked, so the first turn waits for the baseline and
//! the next decision waits for the stamp. Nothing here reads a file or a clock.

use std::collections::{BTreeMap, BTreeSet};

use dudes_proto::diff_stamp::{BaselinePayload, StampPayload};

use super::{AgentId, Clock, Effect, GoalId, Lsn, Record, Rng, Supervisor, Then};

#[derive(Debug, Default)]
pub(super) struct State {
    baselines_requested: BTreeSet<GoalId>,
    stamps_parked: BTreeMap<GoalId, (Lsn, Box<Then>)>,
}

impl<C: Clock, R: Rng> Supervisor<C, R> {
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
        if self.world.ticket_review(goal).is_some() {
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
                    .insert(goal, (check_lsn, Box::new(then)));
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
        let (parked_lsn, then) = self.ticket_review.stamps_parked.remove(&goal)?;
        if parked_lsn != check_lsn {
            self.ticket_review
                .stamps_parked
                .insert(goal, (parked_lsn, then));
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
}
