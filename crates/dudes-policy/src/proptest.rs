//! Strategies over [`LoopInput`] and the properties each gate protects.
//! Each property recomputes the gate's condition on its own, so a test never trusts the code
//! it checks.

use ::proptest::prelude::*;
use dudes_proto::{
    AgentBudget, BudgetScope, Ceiling, ExhaustedReason, Gate, GoalBudget, HoldReason,
    IDENTITY_BYTES, LoopDecision, NoteAdmission, Spend, TEACHING_BYTES,
};
use dudes_types::{BreakerRung, BreakerSignal, CardId, GoalId, Lsn, Provenance, Tokens, Usd};

use crate::breaker::{BreakerConfig, COMPACTION_EXEMPT, beat};
use crate::budget::{Admission, BudgetState, Ceilings, admit};
use crate::chat::{BrowserNoteInput, CompletedNoteInput, CompletedNoteState};
use crate::input::{
    AgentInput, AnswerInput, CardInput, Control, FloorInput, GoalInput, LoopInput, PredicateInput,
    WorkInput,
};
use crate::r#loop::{admit_completed_note, decide, decide_with_browser_note};
use crate::predicate::judge;

/// Spend up to $20 and 200k tokens; ceilings from a wider band (goal and agent up to $100 and
/// 1M tokens, the floor up to $50), so that ceilings are reached often enough to exercise gate 5
/// and seldom enough that the gates behind it still see traffic. [`sample_shares`] owns the
/// numbers and asserts them; no ratio is quoted here, because a quoted one drifts the first time
/// a strategy moves and nothing goes red.
fn any_spend() -> impl Strategy<Value = Spend> {
    (0..20_000_000u64, 0..200_000u64, 0..200_000u64).prop_map(|(usd, tokens, work_tokens)| Spend {
        usd: Usd::from_micros(usd),
        tokens: Tokens::new(tokens),
        work_tokens: Tokens::new(work_tokens.min(tokens)),
    })
}

/// A dollar ceiling in any of its three states: absent, zero, set.
///
/// One arm per state, because **every state that is not drawn is a state no property below
/// tests**, and two of these three were unreachable from the strategy this replaced.
///
/// - *Absent* was not expressible at all until these fields became `Option`s.
/// - *Zero* was expressible and never drawn: from `0..100_000_000` uniform it appeared **0 times
///   in 12 288 cells**, measured. So no property in this file had ever seen the middle
///   state, and the range now starts at 1 with zero drawn deliberately instead.
///
/// **The weights are measured, not chosen, and [`sample_shares`] is the guard on them.** Both
/// directions bite: an even absent/present split pushed two of the three scopes under that
/// guard's one-in-ten binding floor, and a zero arm at one in eight pushed the hop and work gates
/// under theirs, because a zero ceiling refuses at every spend. At `3 : 1 : 12` every floor clears
/// with margin — measured shares are in `sample_shares`' own doc.
fn any_ceiling_usd(max_micros: u64) -> impl Strategy<Value = Option<Usd>> {
    prop_oneof![
        3 => Just(None),
        1 => Just(Some(Usd::ZERO)),
        12 => (1..max_micros).prop_map(|micros| Some(Usd::from_micros(micros))),
    ]
}

fn any_token_cap() -> impl Strategy<Value = Option<Tokens>> {
    prop::option::of((0..1_000_000u64).prop_map(Tokens::new))
}

fn any_provenance() -> impl Strategy<Value = Provenance> {
    prop_oneof![Just(Provenance::Inferred), Just(Provenance::Observed)]
}

/// Weighted towards the rungs that let gate 5 pass; the breaker property draws its own.
fn any_rung() -> impl Strategy<Value = BreakerRung> {
    prop_oneof![
        5 => Just(BreakerRung::Healthy),
        3 => Just(BreakerRung::Steer),
        1 => Just(BreakerRung::Constrain),
        1 => Just(BreakerRung::Stop),
    ]
}

fn every_rung() -> impl Strategy<Value = BreakerRung> {
    prop::sample::select(LADDER.to_vec())
}

/// Every variant, uniformly. Drawn from `BreakerSignal::ALL` rather than a `prop_oneof!` of
/// `Just`s: a hand-written strategy is the one shape of drift no assertion here can see — a
/// seventh variant it never generates leaves every property below green over six sevenths of the
/// enum, silently. `ALL` is pinned to serde's derive by `dudes-types`'
/// `breaker_signal_all_is_every_variant`, so a new variant enters this strategy on the day it
/// enters the enum.
fn any_signal() -> impl Strategy<Value = BreakerSignal> {
    prop::sample::select(BreakerSignal::ALL.to_vec())
}

fn any_signals() -> impl Strategy<Value = Vec<BreakerSignal>> {
    prop::collection::vec(any_signal(), 0..4)
}

fn any_predicate() -> impl Strategy<Value = PredicateInput> {
    (any::<bool>(), any_provenance(), any::<bool>()).prop_map(|(pass, provenance, not_run)| {
        PredicateInput {
            pass,
            provenance,
            not_run: not_run && !pass,
        }
    })
}

fn any_state() -> impl Strategy<Value = BudgetState> {
    (
        any_spend(),
        0..4_u32,
        any_spend(),
        0..4_u32,
        any_spend(),
        0..4_u32,
    )
        .prop_map(
            |(
                goal,
                goal_unknown_ends,
                agent_today,
                agent_unknown_ends_today,
                floor_today,
                floor_unknown_ends_today,
            )| BudgetState {
                goal,
                goal_unknown_ends,
                agent_today,
                agent_unknown_ends_today,
                floor_today,
                floor_unknown_ends_today,
            },
        )
}

fn any_ceilings() -> impl Strategy<Value = Ceilings> {
    (
        any_ceiling_usd(100_000_000),
        any_token_cap(),
        any_ceiling_usd(100_000_000),
        any_token_cap(),
        any_ceiling_usd(50_000_000),
    )
        .prop_map(
            |(goal_usd, goal_tokens, agent_usd, agent_tokens, floor)| Ceilings {
                goal: GoalBudget {
                    usd: goal_usd,
                    tokens: goal_tokens,
                },
                agent: AgentBudget {
                    usd_per_day: agent_usd,
                    tokens_per_day: agent_tokens,
                },
                floor_usd_per_day: floor,
            },
        )
}

fn any_control() -> impl Strategy<Value = Control> {
    // Biased towards "clear" so later gates see traffic.
    (
        prop::bool::weighted(0.06),
        prop::bool::weighted(0.06),
        prop::bool::weighted(0.06),
        prop_oneof![9 => Just(None), 1 => prop::sample::select(vec![HoldReason::RepeatedFastFailure, HoldReason::LaunchRefused, HoldReason::LaunchFailed]).prop_map(Some)],
    )
        .prop_map(|(halted, paused, attached, automatic_hold)| Control {
            halted,
            paused,
            attached,
            automatic_hold,
        })
}

fn any_goal() -> impl Strategy<Value = GoalInput> {
    (
        "[a-z ]{0,20}",
        0..50u32,
        0..50u32,
        any_ceilings(),
        any_spend(),
        prop::option::of(any_predicate()),
    )
        .prop_map(
            |(text, hop_cap, turns_on_goal, ceilings, spent, predicate)| GoalInput {
                text,
                hop_cap,
                turns_on_goal,
                budget: ceilings.goal,
                spent,
                unknown_ends: 0,
                predicate,
                baseline: None,
            },
        )
}

fn any_identity() -> dudes_proto::IdentityLine {
    dudes_proto::IdentityLine::new(dudes_types::AgentId::NIL, "Kim", "backend engineer")
}

fn any_work() -> impl Strategy<Value = WorkInput> {
    (
        prop::option::of("[a-z ]{1,20}"),
        0..5u32,
        prop::option::of("[a-z ]{1,20}"),
    )
        .prop_map(|(card, unread_mail, memory_digest)| WorkInput {
            claimable_card: card.map(|title| CardInput {
                id: CardId::NIL,
                title,
            }),
            held_card_context: None,
            unread_mail,
            memory_digest: memory_digest.unwrap_or_default(),
            identity: Some(any_identity()),
        })
}

fn any_input() -> impl Strategy<Value = LoopInput> {
    (
        any_control(),
        prop_oneof![
            1 => Just(None),
            1 => Just(Some(Provenance::Inferred)),
            6 => Just(Some(Provenance::Observed)),
        ],
        prop::option::weighted(0.12, "[a-z ?]{1,20}"),
        any_goal(),
        (any_ceilings(), any_spend(), any_rung()),
        (any_ceiling_usd(50_000_000), any_spend()),
        any_spend(),
        any_work(),
    )
        .prop_map(
            |(
                control,
                last_turn_end,
                open_question,
                goal,
                (ceilings, spent_today, rung),
                (usd_per_day, floor_spent),
                per_turn_floor,
                work,
            )| LoopInput {
                command_set: dudes_proto::TaskCommandSet::Legacy,
                control,
                last_turn_end,
                open_question,
                goal,
                agent: AgentInput {
                    budget: ceilings.agent,
                    spent_today,
                    unknown_ends_today: 0,
                    rung,
                },
                floor: FloorInput {
                    usd_per_day,
                    spent_today: floor_spent,
                    unknown_ends_today: 0,
                },
                per_turn_floor,
                work,
                // Drawn by the one property that is about them, so every gate share
                // `sample_shares` measures is unchanged.
                answers: Vec::new(),
            },
        )
}

/// The gate conditions, written down independently of `r#loop.rs`.
fn fires(gate: Gate, input: &LoopInput) -> bool {
    match gate {
        Gate::Control => {
            input.control.halted
                || input.control.paused
                || input.control.attached
                || input.control.automatic_hold.is_some()
        }
        Gate::Provenance => input.last_turn_end == Some(Provenance::Inferred),
        Gate::Human => input.open_question.is_some(),
        Gate::Done => {
            input.goal.predicate
                == Some(PredicateInput {
                    pass: true,
                    provenance: Provenance::Observed,
                    not_run: false,
                })
        }
        Gate::Budget => {
            reached_scope(
                &input.budget_state(),
                &input.ceilings(),
                input.per_turn_floor,
            )
            .is_some()
                || input.agent.rung >= BreakerRung::Constrain
        }
        Gate::Hops => input.goal.turns_on_goal >= input.goal.hop_cap,
        Gate::Work => true,
    }
}

/// The first scope whose ceiling is reached, recomputed without [`admit()`].
fn reached_scope(
    state: &BudgetState,
    ceilings: &Ceilings,
    per_turn_floor: Spend,
) -> Option<BudgetScope> {
    let hit = |spent: Spend, unknown_ends: u32, usd: Option<Usd>, tokens: Option<Tokens>| {
        let count = u128::from(unknown_ends);
        let usd_total = (u128::from(spent.usd.micros())
            + u128::from(per_turn_floor.usd.micros()) * count)
            .min(u128::from(u64::MAX));
        let token_total = (u128::from(spent.work_tokens.count())
            + u128::from(per_turn_floor.work_tokens.count()) * count)
            .min(u128::from(u64::MAX));
        usd.is_some_and(|cap| usd_total >= u128::from(cap.micros()))
            || tokens.is_some_and(|cap| token_total >= u128::from(cap.count()))
    };
    if hit(
        state.goal,
        state.goal_unknown_ends,
        ceilings.goal.usd,
        ceilings.goal.tokens,
    ) {
        Some(BudgetScope::Goal)
    } else if hit(
        state.agent_today,
        state.agent_unknown_ends_today,
        ceilings.agent.usd_per_day,
        ceilings.agent.tokens_per_day,
    ) {
        Some(BudgetScope::Agent)
    } else if hit(
        state.floor_today,
        state.floor_unknown_ends_today,
        ceilings.floor_usd_per_day,
        None,
    ) {
        Some(BudgetScope::Floor)
    } else {
        None
    }
}

/// Gates 1–2 clear, so the decision reaches gate 3 or later.
fn clear_to_human(input: &mut LoopInput) {
    input.control = Control::default();
    input.last_turn_end = Some(Provenance::Observed);
}

/// The ladder as a line, so "one rung" is an index difference.
const LADDER: [BreakerRung; 4] = [
    BreakerRung::Healthy,
    BreakerRung::Steer,
    BreakerRung::Constrain,
    BreakerRung::Stop,
];

fn level(rung: BreakerRung) -> usize {
    LADDER.iter().position(|r| *r == rung).unwrap()
}

/// The strategies are biased so every gate, every scope and every ceiling state sees traffic:
/// with 4096 samples from a fixed seed, the hop and work gates each decide at least one case in
/// ten, each budget scope binds `admit()` at least one case in ten while most cases are allowed,
/// and each of the three ceiling states reaches a dollar ceiling at least one case in
/// ten. A property that is never reached proves nothing; this keeps the bias honest when a
/// strategy changes.
///
/// **The three-state count is the guard on [`any_ceiling_usd`]'s weight, and it reads both ways.**
/// Drop the `Option` and *absent* falls to zero; make absence too likely and the scope shares fall
/// under their floor — which is exactly what an even split did when these fields became optional.
///
/// The exact shares measured at the weights in force are **not quoted here**: nothing would pin
/// them, and a number no assertion holds is a number that drifts silently. What this test asserts
/// is the floors.
#[test]
fn sample_shares() {
    use ::proptest::strategy::ValueTree;
    use ::proptest::test_runner::TestRunner;

    const SAMPLES: usize = 4096;
    let mut runner = TestRunner::deterministic();
    let mut by_gate = [0usize; 7];
    let mut by_scope = [0usize; 4];
    // The three ceiling states, counted over every dollar cell drawn: absent, zero, set.
    let mut by_state = [0usize; 3];
    for _ in 0..SAMPLES {
        let input = any_input().new_tree(&mut runner).unwrap().current();
        let last = decide(&input).gate_trace.last().unwrap().gate;
        by_gate[Gate::ORDER.iter().position(|g| *g == last).unwrap()] += 1;
        let state = any_state().new_tree(&mut runner).unwrap().current();
        let ceilings = any_ceilings().new_tree(&mut runner).unwrap().current();
        for cell in [
            ceilings.goal.usd,
            ceilings.agent.usd_per_day,
            ceilings.floor_usd_per_day,
        ] {
            by_state[match cell {
                None => 0,
                Some(Usd::ZERO) => 1,
                Some(_) => 2,
            }] += 1;
        }
        by_scope[match admit(&state, &ceilings, Spend::default()) {
            Admission::Refused { ceiling, .. } => match ceiling.scope {
                BudgetScope::Goal => 0,
                BudgetScope::Agent => 1,
                BudgetScope::Floor => 2,
            },
            Admission::Allowed => 3,
        }] += 1;
    }
    let at_least = |n: usize, tenths: usize| n * 10 >= SAMPLES * tenths;
    assert!(by_gate.iter().all(|n| *n > 0), "gate shares {by_gate:?}");
    assert!(
        at_least(by_gate[5], 1) && at_least(by_gate[6], 1),
        "gate shares {by_gate:?}"
    );
    assert!(
        by_scope[..3].iter().all(|n| at_least(*n, 1)) && at_least(by_scope[3], 4),
        "scope shares {by_scope:?}"
    );
    assert!(
        by_state.iter().all(|n| at_least(*n, 1)),
        "ceiling-state shares (absent, zero, set) {by_state:?}"
    );
}

proptest! {
    /// Invariant 3 (Done is Observed): an `Inferred` predicate never yields `Done`, whatever it
    /// says; and every `Done` traces back to an `Observed` pass.
    #[test]
    fn no_done_from_an_inferred_predicate(mut input in any_input(), pass in any::<bool>()) {
        input.goal.predicate = Some(PredicateInput { pass, provenance: Provenance::Inferred, not_run: false });
        prop_assert_ne!(decide(&input).decision, LoopDecision::Done);
        prop_assert_eq!(judge(PredicateInput { pass, provenance: Provenance::Inferred, not_run: false }), None);
    }

    /// Invariant 3, the converse: `Done` implies the latest predicate is an `Observed` pass, and
    /// `judge()` says `Done` exactly then.
    #[test]
    fn done_implies_an_observed_pass(input in any_input()) {
        if decide(&input).decision == LoopDecision::Done {
            prop_assert_eq!(
                input.goal.predicate,
                Some(PredicateInput { pass: true, provenance: Provenance::Observed, not_run: false })
            );
        }
        if let Some(result) = input.goal.predicate {
            prop_assert_eq!(
                judge(result).is_some(),
                result.provenance.is_observed() && result.pass
            );
        }
    }

    /// Gate order: `Wait` beats `Done`. With gates 1–2 clear, an open question decides at gate
    /// 3 even when the goal is observably done.
    #[test]
    fn wait_beats_done(mut input in any_input(), question in "[a-z ?]{1,20}") {
        clear_to_human(&mut input);
        input.open_question = Some(question.clone());
        input.goal.predicate = Some(PredicateInput { pass: true, provenance: Provenance::Observed, not_run: false });
        let decided = decide(&input);
        prop_assert_eq!(decided.decision, LoopDecision::Wait { question });
        prop_assert_eq!(decided.gate_trace.last().map(|s| s.gate), Some(Gate::Human));
    }

    /// Gate order: budget beats hops. With gates 1–4 clear, a reached ceiling or a high rung
    /// decides at gate 5 with its own reason, never `Hops`, even when the hop cap is reached.
    /// An input whose budget gate would pass is pushed over by one of its four causes in turn.
    #[test]
    fn budget_beats_hops(mut input in any_input(), cause in 0..4u8) {
        clear_to_human(&mut input);
        input.open_question = None;
        input.goal.predicate = None;
        input.goal.turns_on_goal = input.goal.hop_cap;
        // Each cause forces the ceiling to exist *at* the drawn spend rather than moving the
        // spend up to the ceiling. The drawn ceiling may be absent, and an absent one cannot be
        // reached by any spend — so setting the spend would leave three of the four causes unable
        // to fire the gate at all, and the property would quietly stop testing them.
        if !fires(Gate::Budget, &input) {
            match cause {
                0 => input.goal.budget.usd = Some(input.goal.spent.usd),
                1 => input.agent.budget.usd_per_day = Some(input.agent.spent_today.usd),
                2 => input.floor.usd_per_day = Some(input.floor.spent_today.usd),
                _ => input.agent.rung = BreakerRung::Constrain,
            }
        }
        let decided = decide(&input);
        match decided.decision {
            LoopDecision::Exhausted { reason } => {
                prop_assert_ne!(reason, ExhaustedReason::Hops);
            }
            other => prop_assert!(false, "expected Exhausted, got {other:?}"),
        }
        prop_assert_eq!(decided.gate_trace.last().map(|s| s.gate), Some(Gate::Budget));
    }

    /// Invariant 5: the first firing gate decides. The trace is a non-empty prefix of
    /// `Gate::ORDER`; exactly its last step fired; that gate is the first whose condition —
    /// recomputed here — holds; and the decision variant is the one that gate produces.
    #[test]
    fn first_firing_gate_decides(input in any_input()) {
        let decided = decide(&input);
        let consulted: Vec<Gate> = decided.gate_trace.iter().map(|s| s.gate).collect();
        prop_assert!(!consulted.is_empty());
        prop_assert_eq!(&consulted[..], &Gate::ORDER[..consulted.len()]);
        let fired: Vec<bool> = decided.gate_trace.iter().map(|s| s.fired).collect();
        prop_assert_eq!(fired.iter().filter(|f| **f).count(), 1);
        prop_assert!(fired.last() == Some(&true));
        let expected = Gate::ORDER.into_iter().find(|gate| fires(*gate, &input)).unwrap();
        let last = *consulted.last().unwrap();
        prop_assert_eq!(last, expected);
        let matches = matches!(
            (last, &decided.decision),
            (Gate::Control | Gate::Provenance, LoopDecision::Hold { .. })
                | (Gate::Human, LoopDecision::Wait { .. })
                | (Gate::Done, LoopDecision::Done)
                | (Gate::Budget | Gate::Hops, LoopDecision::Exhausted { .. })
                | (Gate::Work, LoopDecision::Continue { .. })
        );
        prop_assert!(matches, "{last:?} produced {:?}", decided.decision);
        // An `Exhausted` names its cause: the binding scope (goal → agent → floor), else the
        // breaker, else the hop cap.
        if let LoopDecision::Exhausted { reason } = decided.decision {
            let expected = match last {
                Gate::Budget => reached_scope(
                    &input.budget_state(),
                    &input.ceilings(),
                    input.per_turn_floor,
                )
                    .map_or(ExhaustedReason::Breaker, |scope| match scope {
                        BudgetScope::Goal => ExhaustedReason::GoalBudget,
                        BudgetScope::Agent => ExhaustedReason::AgentBudget,
                        BudgetScope::Floor => ExhaustedReason::FloorBudget,
                    }),
                _ => ExhaustedReason::Hops,
            };
            prop_assert_eq!(reason, expected);
        }
        prop_assert!(decided.gate_trace.last().unwrap().note.is_some());
    }

    #[test]
    fn breaker_ladder_steps_one_rung(
        rung in every_rung(),
        signals in any_signals(),
        compacting in any::<bool>(),
        hard_stop in any::<bool>(),
    ) {
        let config = BreakerConfig { hard_stop };
        let after = beat(rung, &signals, compacting, config);
        let counting = signals
            .iter()
            .any(|s| *s != BreakerSignal::RapidFailure && !(compacting && COMPACTION_EXEMPT.contains(s)));
        let (before, after_level) = (level(rung), level(after));
        if counting {
            let cap = level(config.cap());
            let expected = if before >= cap { before } else { before + 1 };
            prop_assert_eq!(after_level, expected);
        } else if signals.contains(&BreakerSignal::RapidFailure) {
            prop_assert_eq!(after_level, before.max(level(BreakerRung::Steer)));
        } else {
            prop_assert_eq!(after_level, before.saturating_sub(1));
        }
        prop_assert!(after_level.abs_diff(before) <= 1);
        if !hard_stop && rung <= BreakerRung::Constrain {
            prop_assert!(after <= BreakerRung::Constrain);
        }
        // Exemption is exactly "PtySilence while compacting": while compacting, stripping it
        // is the same beat; without it, compacting changes nothing.
        let stripped: Vec<_> = signals
            .iter()
            .copied()
            .filter(|s| !COMPACTION_EXEMPT.contains(s))
            .collect();
        if compacting {
            prop_assert_eq!(after, beat(rung, &stripped, true, config));
        }
        prop_assert_eq!(beat(rung, &stripped, true, config), beat(rung, &stripped, false, config));
    }

    /// one observed isolation leak is one non-terminal breaker rung, regardless of
    /// compaction or the configured top rung. It cannot jump a healthy agent to `Stop`.
    #[test]
    fn one_isolation_leak_is_one_non_terminal_rung(
        compacting in any::<bool>(),
        hard_stop in any::<bool>(),
    ) {
        let after = beat(
            BreakerRung::Healthy,
            &[BreakerSignal::IsolationLeak],
            compacting,
            BreakerConfig { hard_stop },
        );
        prop_assert_eq!(after, BreakerRung::Steer);
        prop_assert_ne!(after, BreakerRung::Stop);
    }

    /// Invariant 4 (budget is admission): `admit()` refuses iff some ceiling is reached; the
    /// binding ceiling is the first reached in goal → agent → floor order, reported with that
    /// scope's spend; and a spend under an allowed one is still allowed.
    #[test]
    fn admit_refuses_iff_a_ceiling_is_reached(
        state in any_state(),
        ceilings in any_ceilings(),
        per_turn_floor in any_spend(),
    ) {
        let admission = admit(&state, &ceilings, per_turn_floor);
        match (admission, reached_scope(&state, &ceilings, per_turn_floor)) {
            (Admission::Allowed, None) => {
                let lower = BudgetState {
                    goal: Spend { usd: Usd::ZERO, ..state.goal },
                    agent_today: Spend {
                        tokens: Tokens::ZERO,
                        work_tokens: Tokens::ZERO,
                        ..state.agent_today
                    },
                    floor_today: Spend::default(),
                    ..state
                };
                prop_assert_eq!(admit(&lower, &ceilings, Spend::default()), Admission::Allowed);
            }
            (Admission::Refused { ceiling, spent, .. }, Some(scope)) => {
                prop_assert_eq!(ceiling.scope, scope);
                let (expected, expected_spent) = match scope {
                    BudgetScope::Goal => (
                        Ceiling { scope, usd: ceilings.goal.usd, tokens: ceilings.goal.tokens },
                        state.goal,
                    ),
                    BudgetScope::Agent => (
                        Ceiling {
                            scope,
                            usd: ceilings.agent.usd_per_day,
                            tokens: ceilings.agent.tokens_per_day,
                        },
                        state.agent_today,
                    ),
                    BudgetScope::Floor => (
                        Ceiling { scope, usd: ceilings.floor_usd_per_day, tokens: None },
                        state.floor_today,
                    ),
                };
                prop_assert_eq!(ceiling, expected);
                prop_assert_eq!(spent, expected_spent);
            }
            (admission, reached) => {
                prop_assert!(false, "admit said {admission:?} but reached is {reached:?}");
            }
        }
    }

    /// The answer-carry computation, recomputed independently: every `Continue` task stays within
    /// 65,536 bytes; the answers it carries are the oldest ones, in order; each carried answer
    /// that could fit whole is present verbatim; and a verdict other than `Continue` carries none.
    #[test]
    fn a_task_carries_the_oldest_answers_whole_within_the_cap(
        mut input in any_input(),
        sizes in prop::collection::vec((1..70_000usize, 0..2_000usize), 0..4),
    ) {
        input.answers = sizes
            .iter()
            .enumerate()
            .map(|(i, (text, question))| AnswerInput {
                lsn: Lsn(u64::try_from(i).unwrap() + 1),
                question: "q".repeat(*question),
                text: char::from(b'a' + u8::try_from(i).unwrap()).to_string().repeat(*text),
            })
            .collect();
        let decided = decide(&input);
        let LoopDecision::Continue { task } = &decided.decision else {
            prop_assert!(decided.answers.is_empty());
            return Ok(());
        };
        prop_assert!(task.len() <= 65_536, "{}", task.len());
        let expected: Vec<Lsn> = input.answers.iter().map(|a| a.lsn).collect();
        prop_assert_eq!(&decided.answers[..], &expected[..decided.answers.len()]);
        prop_assert_eq!(decided.answers.is_empty(), input.answers.is_empty());
        for carried in &input.answers[..decided.answers.len()] {
            // Framing plus the answer, with the question at most 1,024 bytes. The loop keeps room
            // for the identity line whether or not this agent has one.
            if carried.text.len() + 1_024 + 64 + TEACHING_BYTES + IDENTITY_BYTES <= 65_536 {
                prop_assert!(task.contains(carried.text.as_str()), "answer {} cut", carried.lsn.0);
            }
        }
    }

    /// Invariant 5 (the loop is pure): the same input always yields the same decision and
    /// trace, across calls and across a clone — nothing read but the input.
    #[test]
    fn decide_is_deterministic(input in any_input()) {
        let first = decide(&input);
        prop_assert_eq!(&first, &decide(&input));
        prop_assert_eq!(&first, &decide(&input.clone()));
        let bytes = serde_json::to_vec(&input).unwrap();
        prop_assert_eq!(bytes, serde_json::to_vec(&input).unwrap());
    }

    #[test]
    fn completed_notes_share_every_common_gate(input in any_input(), text in "[a-zé]{1,40}") {
        prop_assume!(!input.goal.predicate.is_some_and(|p| p.pass && p.provenance.is_observed()));
        let note = BrowserNoteInput::new(Lsn(91), GoalId::NIL, text.clone()).unwrap();
        let completed = CompletedNoteInput::new(
            GoalId::NIL,
            Lsn(90),
            note,
            any_identity(),
            CompletedNoteState {
                control: input.control,
                last_turn_end: input.last_turn_end,
                open_question: input.open_question.clone(),
                budget_state: input.budget_state(),
                ceilings: input.ceilings(),
                per_turn_floor: input.per_turn_floor,
                rung: input.agent.rung,
                turns_on_goal: input.goal.turns_on_goal,
                hop_cap: input.goal.hop_cap,
            },
        ).unwrap();
        let ordinary = decide(&input);
        let actual = admit_completed_note(&completed);
        let expected = match ordinary.decision {
            LoopDecision::Continue { .. } | LoopDecision::Exhausted { reason:ExhaustedReason::Hops } =>
                NoteAdmission::Issue { task:dudes_proto::identified_taught_note_task(&any_identity(), &text).unwrap() },
            LoopDecision::Wait { question } => NoteAdmission::Wait { question },
            LoopDecision::Hold { reason } => NoteAdmission::Hold { reason },
            LoopDecision::Exhausted { reason } => NoteAdmission::Exhausted { reason },
            LoopDecision::Done => unreachable!("the strategy excluded ordinary completion"),
        };
        prop_assert_eq!(&actual.decision, &expected);
        let shared:Vec<_> = actual.gate_trace.iter().filter(|step| step.gate != Gate::Hops && step.gate != Gate::Work)
            .map(|step| (step.gate,step.fired)).collect();
        let ordinary_shared:Vec<_> = ordinary.gate_trace.iter().filter(|step| step.gate != Gate::Hops && step.gate != Gate::Work)
            .map(|step| (step.gate,step.fired)).collect();
        prop_assert_eq!(shared,ordinary_shared);
        prop_assert_eq!(&actual, &admit_completed_note(&completed.clone()));
    }

    #[test]
    fn browser_note_tasks_are_bounded_and_carry_oldest_answers(
        note_chars in 1_usize..=2_048,
        answer_lengths in prop::collection::vec(0_usize..65_537, 0..8),
        context_chars in 0_usize..40_000,
    ) {
        let mut input = crate::r#loop::tests::idle();
        input.goal.text = "é".repeat(context_chars);
        input.work.memory_digest = "m".repeat(context_chars);
        input.answers = answer_lengths.iter().enumerate().map(|(index, len)| AnswerInput {
            lsn: Lsn(u64::try_from(index).unwrap() + 1),
            question: "which branch?".into(),
            text: "a".repeat(*len),
        }).collect();
        let text = "é".repeat(note_chars);
        let note = BrowserNoteInput::new(Lsn(91), GoalId::NIL, text.clone()).unwrap();
        let actual = decide_with_browser_note(&input, &note);
        let LoopDecision::Continue { task } = actual.decision else {
            panic!("all ordinary admission gates pass");
        };
        prop_assert!(task.len() <= 65_536, "task has {} bytes", task.len());
        prop_assert!(
            task.ends_with(&format!("\n\n[browser note]\n{text}\n[end browser note]")),
            "complete browser note section missing"
        );
        let oldest: Vec<Lsn> = input.answers.iter().map(|answer| answer.lsn).collect();
        prop_assert_eq!(&actual.answers, &oldest[..actual.answers.len()]);
        prop_assert_eq!(actual.answers.is_empty(), input.answers.is_empty());
    }
}
