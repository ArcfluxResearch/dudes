//! Pure, completed-goal mail admission with immutable notification preparation inputs.

use dudes_proto::mail_wake::{MailWakeCandidate, MailWakePreparation, MailWakeProgress};
use dudes_proto::{GateStep, MailWakeAdmission, MailWakeDigestDomain, MailWakePlan, MailWakeTask};
use dudes_types::{AgentId, GoalId, Lsn, TurnId};
use serde::Serialize;

use crate::CompletedNoteState;

/// Current canonical ownership, slot, question, debt and stamped quota facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MailWakeFacts {
    /// Recipient whose original completed goal is being considered.
    pub agent: AgentId,
    /// Current uncleared original goal and its canonical ordinary completion.
    pub completion: Option<(GoalId, Lsn)>,
    /// A retired recipient cannot own preparation admission.
    pub retired: bool,
    /// A normal issued or spawned turn already owns the recipient's slot.
    pub open_turn: Option<TurnId>,
    /// Eligible browser-note work owns this pass before mail.
    pub browser_note_priority: bool,
    /// Exact standing unanswered question; present iff common state has question text.
    pub question_lsn: Option<Lsn>,
    /// Unique committed mail issues in the supervisor-supplied current bucket.
    pub wakes_today: u8,
    /// Latest canonical four-to-five notification crossing.
    pub fifth_crossing: Lsn,
    /// Canonical consuming breaker beat, not signal selection or acknowledgement.
    pub fifth_consumed: Lsn,
}

/// Fresh selection or the one immutable preparation with its exact canonical progress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MailWakeWork {
    /// Fix the oldest owed trigger, coalesced frontier, complete count and ordered targets.
    Fresh {
        /// Canonical arrival ownership and frontier.
        candidate: MailWakeCandidate,
        /// Bounded snapshot; targets start at zero through four, including four-to-five.
        plan: MailWakePlan,
    },
    /// Reconsider charged progress without applying fresh notification eligibility again.
    Resume {
        /// The original task and targets survive partial terminalization unchanged.
        preparation: MailWakePreparation,
    },
}

impl MailWakeWork {
    fn binding(&self) -> (GoalId, Lsn) {
        match self {
            Self::Fresh { candidate, .. } => (candidate.goal, candidate.completion_lsn),
            Self::Resume { preparation } => (preparation.goal, preparation.completion_lsn),
        }
    }

    fn valid(&self) -> bool {
        match self {
            Self::Fresh { candidate, plan } => {
                candidate.completion_lsn != Lsn::ZERO
                    && candidate.completion_lsn < candidate.trigger.lsn
                    && candidate.trigger == plan.trigger
                    && candidate.frontier == plan.frontier
                    && plan.valid()
            }
            Self::Resume { preparation } => {
                preparation.completion_lsn != Lsn::ZERO
                    && preparation.root_lsn > preparation.completion_lsn
                    && preparation.plan.valid()
                    && preparation.completion_lsn < preparation.plan.trigger.lsn
                    && preparation.plan.frontier < preparation.root_lsn
                    && preparation.progress.len() == preparation.plan.targets.len()
                    && preparation.progress.iter().all(|progress| match progress {
                        MailWakeProgress::Pending => true,
                        MailWakeProgress::Notified { lsn } => *lsn > preparation.root_lsn,
                        MailWakeProgress::Terminal { lsn, notified_lsn } => {
                            *lsn > preparation.root_lsn
                                && notified_lsn.is_none_or(|notified| {
                                    notified > preparation.root_lsn && notified < *lsn
                                })
                        }
                    })
                    && preparation
                        .progress
                        .iter()
                        .any(|progress| !matches!(progress, MailWakeProgress::Terminal { .. }))
            }
        }
    }
}

/// Complete separately digestible input with caller eligibility checked before gate walking.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MailWakeInput {
    facts: MailWakeFacts,
    work: MailWakeWork,
    state: CompletedNoteState,
}

impl MailWakeInput {
    /// Bind an eligible recipient to the exact current completion and bounded work.
    ///
    /// Partial notification progress and unconsumed crossing debt are retained here: earlier
    /// control, provenance, question and budget refusals must still be recordable.
    #[must_use]
    pub fn new(
        facts: MailWakeFacts,
        work: MailWakeWork,
        state: CompletedNoteState,
    ) -> Option<Self> {
        let question_matches = state.open_question.is_some() == facts.question_lsn.is_some()
            && facts.question_lsn != Some(Lsn::ZERO);
        if facts.retired
            || facts.open_turn.is_some()
            || facts.browser_note_priority
            || facts.completion != Some(work.binding())
            || !question_matches
            || !work.valid()
        {
            return None;
        }
        Some(Self { facts, work, state })
    }

    /// Original recipient.
    #[must_use]
    pub const fn agent(&self) -> AgentId {
        self.facts.agent
    }

    /// Unchanged original completed goal.
    #[must_use]
    pub fn goal(&self) -> GoalId {
        self.work.binding().0
    }

    /// Exact canonical ordinary completion.
    #[must_use]
    pub fn completion_lsn(&self) -> Lsn {
        self.work.binding().1
    }

    /// Distinct fresh or resume identity for canonical digests.
    #[must_use]
    pub const fn domain(&self) -> MailWakeDigestDomain {
        match self.work {
            MailWakeWork::Fresh { .. } => MailWakeDigestDomain::Fresh,
            MailWakeWork::Resume { .. } => MailWakeDigestDomain::Resume,
        }
    }

    /// The exact immutable notice and both teaching instructions.
    #[must_use]
    pub fn task(&self) -> &MailWakeTask {
        match &self.work {
            MailWakeWork::Fresh { plan, .. } => &plan.task,
            MailWakeWork::Resume { preparation } => &preparation.plan.task,
        }
    }

    pub(crate) const fn state(&self) -> &CompletedNoteState {
        &self.state
    }
    pub(crate) const fn question_lsn(&self) -> Option<Lsn> {
        self.facts.question_lsn
    }
    pub(crate) const fn wakes_today(&self) -> u8 {
        self.facts.wakes_today
    }

    pub(crate) fn ready(&self) -> bool {
        match &self.work {
            MailWakeWork::Fresh { .. } => self.facts.fifth_crossing <= self.facts.fifth_consumed,
            MailWakeWork::Resume { preparation } => {
                self.facts.fifth_crossing <= self.facts.fifth_consumed
                    && preparation.progress.len() == preparation.plan.targets.len()
                    && preparation
                        .progress
                        .iter()
                        .all(|progress| !matches!(progress, MailWakeProgress::Pending))
                    && preparation
                        .progress
                        .iter()
                        .any(|progress| matches!(progress, MailWakeProgress::Notified { .. }))
            }
        }
    }
}

/// A real consulted-gate prefix; unfinished Work instead returns no recordable decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MailWakeDecided {
    /// Mail-only issue or an existing refusal; this type cannot complete a goal.
    pub decision: MailWakeAdmission,
    /// Exact first-firing common gate prefix.
    pub gate_trace: Vec<GateStep>,
}

#[cfg(test)]
mod tests {
    use dudes_proto::{ExhaustedReason, Gate, HoldReason, MailRef, MailWakeTarget, Spend};
    use dudes_types::{BreakerRung, MailId, Provenance, Tokens, Usd};
    use proptest::prelude::*;

    use super::*;
    use crate::{admit_mail_wake, r#loop::tests::idle};

    fn fresh() -> MailWakeInput {
        let ordinary = idle();
        let trigger = MailRef {
            id: MailId::from_parts(1, 1),
            lsn: Lsn(20),
        };
        let plan = MailWakePlan {
            trigger,
            frontier: trigger.lsn,
            unread_count: 1,
            task: MailWakeTask::new(1).unwrap(),
            targets: vec![MailWakeTarget {
                mail: trigger,
                original_count: 4,
                target_count: 5,
            }],
        };
        MailWakeInput::new(
            MailWakeFacts {
                agent: AgentId::from_parts(1, 1),
                completion: Some((GoalId::NIL, Lsn(10))),
                retired: false,
                open_turn: None,
                browser_note_priority: false,
                question_lsn: None,
                wakes_today: 0,
                fifth_crossing: Lsn::ZERO,
                fifth_consumed: Lsn::ZERO,
            },
            MailWakeWork::Fresh {
                candidate: MailWakeCandidate {
                    goal: GoalId::NIL,
                    completion_lsn: Lsn(10),
                    trigger,
                    frontier: trigger.lsn,
                },
                plan,
            },
            CompletedNoteState {
                control: ordinary.control,
                last_turn_end: ordinary.last_turn_end,
                open_question: None,
                budget_state: ordinary.budget_state(),
                ceilings: ordinary.ceilings(),
                per_turn_floor: ordinary.per_turn_floor,
                rung: ordinary.agent.rung,
                turns_on_goal: ordinary.goal.turns_on_goal,
                hop_cap: ordinary.goal.hop_cap,
            },
        )
        .unwrap()
    }

    fn resumed(progress: Vec<MailWakeProgress>) -> MailWakeInput {
        let mut input = fresh();
        let MailWakeWork::Fresh { plan, .. } = input.work else {
            unreachable!()
        };
        input.work = MailWakeWork::Resume {
            preparation: MailWakePreparation {
                root_lsn: Lsn(30),
                goal: GoalId::NIL,
                completion_lsn: Lsn(10),
                plan,
                progress,
            },
        };
        MailWakeInput::new(input.facts, input.work, input.state).unwrap()
    }

    fn verdict(input: &MailWakeInput) -> MailWakeDecided {
        admit_mail_wake(input).unwrap()
    }

    #[test]
    fn exact_fixed_work_passes_completion_and_hops_without_done_or_context() {
        let mut input = fresh();
        input.state.turns_on_goal = u32::MAX;
        input.state.hop_cap = 0;
        let decided = verdict(&input);
        assert_eq!(
            decided.decision,
            MailWakeAdmission::Issue {
                task: MailWakeTask::new(1).unwrap()
            }
        );
        assert_eq!(
            decided
                .gate_trace
                .iter()
                .map(|step| step.gate)
                .collect::<Vec<_>>(),
            Gate::ORDER
        );
        assert!(!decided.gate_trace[3].fired && !decided.gate_trace[5].fired);
        assert_eq!(input.completion_lsn(), Lsn(10));
        assert_eq!(
            input.task().as_str(),
            format!(
                "[1 unread mail]{}{}",
                dudes_proto::MEMORY_INSTRUCTION,
                dudes_proto::MAIL_INSTRUCTION_WITH_REPLIES
            )
        );
    }

    #[test]
    fn competing_gates_keep_exact_priority_on_pending_progress() {
        for index in 0..8 {
            let mut input = resumed(vec![MailWakeProgress::Pending]);
            input.state.ceilings.goal.usd = Some(Usd::ZERO);
            input.facts.wakes_today = 20;
            if index <= 5 {
                input.state.open_question = Some("real question".into());
                input.facts.question_lsn = Some(Lsn(90));
            }
            if index <= 4 {
                input.state.last_turn_end = Some(Provenance::Inferred);
            }
            let (gate, expected) = match index {
                0 => {
                    input.state.control.halted = true;
                    (
                        Gate::Control,
                        MailWakeAdmission::Hold {
                            reason: HoldReason::Halted,
                        },
                    )
                }
                1 => {
                    input.state.control.paused = true;
                    (
                        Gate::Control,
                        MailWakeAdmission::Hold {
                            reason: HoldReason::Paused,
                        },
                    )
                }
                2 => {
                    input.state.control.attached = true;
                    (
                        Gate::Control,
                        MailWakeAdmission::Hold {
                            reason: HoldReason::Attached,
                        },
                    )
                }
                3 => {
                    input.state.control.automatic_hold = Some(HoldReason::RepeatedFastFailure);
                    (
                        Gate::Control,
                        MailWakeAdmission::Hold {
                            reason: HoldReason::RepeatedFastFailure,
                        },
                    )
                }
                4 => (
                    Gate::Provenance,
                    MailWakeAdmission::Hold {
                        reason: HoldReason::InferredTurnEnd,
                    },
                ),
                5 => (
                    Gate::Human,
                    MailWakeAdmission::Wait {
                        question_lsn: Lsn(90),
                    },
                ),
                6 => (
                    Gate::Budget,
                    MailWakeAdmission::Exhausted {
                        reason: ExhaustedReason::GoalBudget,
                    },
                ),
                7 => {
                    input.state.ceilings.goal.usd = None;
                    (
                        Gate::Budget,
                        MailWakeAdmission::Exhausted {
                            reason: ExhaustedReason::MailWakeLimit,
                        },
                    )
                }
                _ => unreachable!(),
            };
            let decided = verdict(&input);
            assert_eq!(decided.decision, expected);
            assert_eq!(decided.gate_trace.last().unwrap().gate, gate);
            assert_eq!(
                decided.gate_trace.iter().filter(|step| step.fired).count(),
                1
            );
            assert_eq!(
                decided
                    .gate_trace
                    .iter()
                    .map(|step| step.gate)
                    .collect::<Vec<_>>(),
                Gate::ORDER[..decided.gate_trace.len()]
            );
        }
    }

    #[test]
    fn reserves_every_ceiling_and_breaker_precede_mail_quota() {
        for index in 0..6 {
            let mut input = fresh();
            input.facts.wakes_today = 20;
            let reason = match index {
                0 => {
                    input.state.ceilings.goal.tokens = Some(Tokens::ZERO);
                    ExhaustedReason::GoalBudget
                }
                1 => {
                    input.state.budget_state.goal_unknown_ends = 2;
                    input.state.per_turn_floor.usd = Usd::from_dollars(5);
                    ExhaustedReason::GoalBudget
                }
                2 => {
                    input.state.ceilings.agent.usd_per_day = Some(Usd::from_dollars(3));
                    ExhaustedReason::AgentBudget
                }
                3 => {
                    input.state.ceilings.agent.tokens_per_day = Some(Tokens::ZERO);
                    ExhaustedReason::AgentBudget
                }
                4 => {
                    input.state.ceilings.floor_usd_per_day = Some(Usd::from_dollars(12));
                    ExhaustedReason::FloorBudget
                }
                5 => {
                    input.state.rung = BreakerRung::Constrain;
                    ExhaustedReason::Breaker
                }
                _ => unreachable!(),
            };
            assert_eq!(
                verdict(&input).decision,
                MailWakeAdmission::Exhausted { reason }
            );
        }
    }

    #[test]
    fn quota_uses_supplied_issue_count_without_charge_or_refund() {
        let mut input = fresh();
        input.facts.wakes_today = 19;
        let before = input.clone();
        assert!(matches!(
            verdict(&input).decision,
            MailWakeAdmission::Issue { .. }
        ));
        assert_eq!(input, before);
        input.facts.wakes_today = 20;
        assert_eq!(
            verdict(&input).decision,
            MailWakeAdmission::Exhausted {
                reason: ExhaustedReason::MailWakeLimit
            }
        );
        input.facts.wakes_today = 0;
        assert!(matches!(
            verdict(&input).decision,
            MailWakeAdmission::Issue { .. }
        ));
        input.facts.wakes_today = 20;
        assert_eq!(
            verdict(&input).decision,
            MailWakeAdmission::Exhausted {
                reason: ExhaustedReason::MailWakeLimit
            }
        );
    }

    #[test]
    fn charged_five_and_partial_terminals_keep_snapshot_while_debt_defers_work_only() {
        let mut input = resumed(vec![MailWakeProgress::Notified { lsn: Lsn(31) }]);
        input.facts.fifth_crossing = Lsn(31);
        assert!(admit_mail_wake(&input).is_none());
        input.state.control.paused = true;
        assert_eq!(
            verdict(&input).decision,
            MailWakeAdmission::Hold {
                reason: HoldReason::Paused
            }
        );
        input.state.control.paused = false;
        input.facts.fifth_consumed = Lsn(32);
        assert_eq!(
            verdict(&input).decision,
            MailWakeAdmission::Issue {
                task: MailWakeTask::new(1).unwrap()
            }
        );
        input.facts.fifth_crossing = Lsn(33);
        assert!(admit_mail_wake(&input).is_none());
        let mut input = fresh();
        input.facts.fifth_crossing = Lsn(21);
        assert!(admit_mail_wake(&input).is_none());
        input.state.open_question = Some("owed question".into());
        input.facts.question_lsn = Some(Lsn(25));
        assert_eq!(
            verdict(&input).decision,
            MailWakeAdmission::Wait {
                question_lsn: Lsn(25)
            }
        );
    }

    #[test]
    fn partial_terminalization_preserves_original_count_and_task() {
        let mut input = fresh();
        let MailWakeWork::Fresh { plan, candidate } = &mut input.work else {
            unreachable!()
        };
        let later = MailRef {
            id: MailId::from_parts(1, 2),
            lsn: Lsn(21),
        };
        candidate.frontier = later.lsn;
        plan.frontier = later.lsn;
        plan.unread_count = 2;
        plan.task = MailWakeTask::new(2).unwrap();
        plan.targets.push(MailWakeTarget {
            mail: later,
            original_count: 0,
            target_count: 1,
        });
        let plan = plan.clone();
        let work = MailWakeWork::Resume {
            preparation: MailWakePreparation {
                root_lsn: Lsn(30),
                goal: GoalId::NIL,
                completion_lsn: Lsn(10),
                plan,
                progress: vec![
                    MailWakeProgress::Terminal {
                        lsn: Lsn(32),
                        notified_lsn: Some(Lsn(31)),
                    },
                    MailWakeProgress::Notified { lsn: Lsn(33) },
                ],
            },
        };
        let input = MailWakeInput::new(input.facts, work, input.state).unwrap();
        assert_eq!(
            verdict(&input).decision,
            MailWakeAdmission::Issue {
                task: MailWakeTask::new(2).unwrap()
            }
        );
    }

    #[test]
    fn constructor_refuses_lost_authority_slot_priority_and_question_identity() {
        let input = fresh();
        for index in 0..8 {
            let mut facts = input.facts.clone();
            let mut state = input.state.clone();
            match index {
                0 => facts.retired = true,
                1 => facts.completion = None,
                2 => facts.completion = Some((GoalId::from_parts(1, 2), Lsn(10))),
                3 => facts.open_turn = Some(TurnId::from_parts(1, 2)),
                4 => facts.browser_note_priority = true,
                5 => {
                    state.open_question = Some("question".into());
                    facts.question_lsn = Some(Lsn::ZERO);
                }
                6 => state.open_question = Some("missing identity".into()),
                7 => facts.question_lsn = Some(Lsn(90)),
                _ => unreachable!(),
            }
            assert!(MailWakeInput::new(facts, input.work.clone(), state).is_none());
        }
    }

    #[test]
    fn constructor_refuses_unordered_stale_task_and_malformed_terminal_progress() {
        let input = fresh();
        let MailWakeWork::Fresh { candidate, plan } = input.work.clone() else {
            unreachable!()
        };
        let mut stale = plan.clone();
        stale.unread_count = 2;
        assert!(
            MailWakeInput::new(
                input.facts.clone(),
                MailWakeWork::Fresh {
                    candidate: candidate.clone(),
                    plan: stale
                },
                input.state.clone()
            )
            .is_none()
        );
        let mut unordered = plan.clone();
        unordered.unread_count = 2;
        unordered.task = MailWakeTask::new(2).unwrap();
        unordered.frontier = Lsn(21);
        unordered.targets.push(MailWakeTarget {
            mail: MailRef {
                id: MailId::from_parts(1, 2),
                lsn: Lsn(21),
            },
            original_count: 0,
            target_count: 1,
        });
        unordered.targets.reverse();
        let mut candidate = candidate;
        candidate.frontier = Lsn(21);
        assert!(
            MailWakeInput::new(
                input.facts.clone(),
                MailWakeWork::Fresh {
                    candidate,
                    plan: unordered
                },
                input.state.clone()
            )
            .is_none()
        );
        for progress in [
            Vec::new(),
            vec![MailWakeProgress::Terminal {
                lsn: Lsn(32),
                notified_lsn: None,
            }],
            vec![MailWakeProgress::Notified { lsn: Lsn(29) }],
        ] {
            assert!(
                MailWakeInput::new(
                    input.facts.clone(),
                    MailWakeWork::Resume {
                        preparation: MailWakePreparation {
                            root_lsn: Lsn(30),
                            goal: GoalId::NIL,
                            completion_lsn: Lsn(10),
                            plan: plan.clone(),
                            progress
                        }
                    },
                    input.state.clone()
                )
                .is_none()
            );
        }
        assert!(
            MailWakeInput::new(
                input.facts.clone(),
                MailWakeWork::Resume {
                    preparation: MailWakePreparation {
                        root_lsn: plan.frontier,
                        goal: GoalId::NIL,
                        completion_lsn: Lsn(10),
                        plan,
                        progress: vec![MailWakeProgress::Notified { lsn: Lsn(31) }],
                    }
                },
                input.state.clone(),
            )
            .is_none()
        );
        assert!(admit_mail_wake(&resumed(vec![MailWakeProgress::Pending])).is_none());
    }

    #[test]
    fn unchanged_refusals_are_reproducible_without_mutating_original_costs() {
        let mut input = fresh();
        input.state.control.halted = true;
        input.state.per_turn_floor = Spend::DEFAULT_PER_TURN_FLOOR;
        let before = input.clone();
        let expected = MailWakeDecided {
            decision: MailWakeAdmission::Hold {
                reason: HoldReason::Halted,
            },
            gate_trace: vec![GateStep {
                gate: Gate::Control,
                fired: true,
                note: Some("Halted is standing".into()),
            }],
        };
        assert_eq!(admit_mail_wake(&input), Some(expected.clone()));
        assert_eq!(admit_mail_wake(&input), Some(expected));
        assert_eq!(input, before);
    }

    proptest! {
        #[test]
        fn randomized_earlier_gates_dominate_readiness_without_changing_owned_work(
            controls in any::<[bool; 4]>(),
            inferred in any::<bool>(),
            question in any::<bool>(),
            budget_reached in any::<[bool; 3]>(),
            constrained in any::<bool>(),
            quota in 0u8..=24,
            resume in any::<bool>(),
            pending in any::<bool>(),
            debt in any::<bool>(),
            original_count in 0u8..=4,
        ) {
            let mut input = if resume {
                resumed(vec![if pending { MailWakeProgress::Pending } else { MailWakeProgress::Notified { lsn: Lsn(31) } }])
            } else { fresh() };
            let plan = match &mut input.work {
                MailWakeWork::Fresh { plan, .. } => plan,
                MailWakeWork::Resume { preparation } => &mut preparation.plan,
            };
            plan.targets[0].original_count = original_count;
            plan.targets[0].target_count = original_count + 1;
            input.state.control.halted = controls[0];
            input.state.control.paused = controls[1];
            input.state.control.attached = controls[2];
            input.state.control.automatic_hold = controls[3].then_some(HoldReason::RepeatedFastFailure);
            input.state.last_turn_end = Some(if inferred { Provenance::Inferred } else { Provenance::Observed });
            input.state.open_question = question.then(|| "canonical question".to_owned());
            input.facts.question_lsn = question.then_some(Lsn(90));
            input.state.ceilings.goal.usd = Some(Usd::from_dollars(10));
            input.state.ceilings.agent.usd_per_day = Some(Usd::from_dollars(10));
            input.state.ceilings.floor_usd_per_day = Some(Usd::from_dollars(10));
            input.state.budget_state.goal.usd = if budget_reached[0] { Usd::from_dollars(10) } else { Usd::ZERO };
            input.state.budget_state.agent_today.usd = if budget_reached[1] { Usd::from_dollars(10) } else { Usd::ZERO };
            input.state.budget_state.floor_today.usd = if budget_reached[2] { Usd::from_dollars(10) } else { Usd::ZERO };
            input.state.rung = if constrained { BreakerRung::Constrain } else { BreakerRung::Healthy };
            input.facts.wakes_today = quota;
            input.facts.fifth_crossing = if debt { Lsn(31) } else { Lsn::ZERO };
            input.facts.fifth_consumed = Lsn::ZERO;
            input.state.turns_on_goal = u32::MAX;
            input.state.hop_cap = 0;
            let before = input.clone();
            let earlier = [
                (controls.iter().any(|standing| *standing), Gate::Control),
                (inferred, Gate::Provenance),
                (question, Gate::Human),
                (budget_reached.iter().any(|reached| *reached) || constrained || quota >= 20, Gate::Budget),
            ].into_iter().find_map(|(fires, gate)| fires.then_some(gate));
            let decided = admit_mail_wake(&input);
            prop_assert_eq!(&input, &before);
            if let Some(decided) = decided {
                let last = earlier.unwrap_or(Gate::Work);
                let prefix_len = Gate::ORDER.iter().position(|gate| *gate == last).unwrap() + 1;
                prop_assert_eq!(decided.gate_trace.iter().map(|step| step.gate).collect::<Vec<_>>(), &Gate::ORDER[..prefix_len]);
                prop_assert_eq!(decided.gate_trace.iter().filter(|step| step.fired).count(), 1);
                prop_assert!(!decided.gate_trace.iter().any(|step| step.fired && matches!(step.gate, Gate::Done | Gate::Hops)));
                if let MailWakeAdmission::Issue { task } = &decided.decision {
                    prop_assert!(earlier.is_none());
                    prop_assert!(!debt && (!resume || !pending));
                    prop_assert_eq!(task, input.task());
                } else {
                    prop_assert!(earlier.is_some());
                    let mut changed_later_work = input.clone();
                    changed_later_work.facts.fifth_crossing = Lsn(31);
                    changed_later_work.facts.fifth_consumed = Lsn::ZERO;
                    if let MailWakeWork::Resume { preparation } = &mut changed_later_work.work {
                        preparation.progress[0] = MailWakeProgress::Pending;
                    }
                    prop_assert_eq!(admit_mail_wake(&changed_later_work).unwrap(), decided);
                }
            } else {
                prop_assert!(earlier.is_none());
                prop_assert!(debt || (resume && pending));
            }
        }
    }
}
