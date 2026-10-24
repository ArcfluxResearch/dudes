//! a refusal is the only `BudgetDecision`, and gate 5 refusing writes exactly one.
//!
//! Invariant 4 is the point — budget is admission, and exceeded means not issued. A refusal that
//! left no record would be a decision the journal cannot replay, and the claim that the
//! record makes a refusal auditable would rest on a variant nothing ever wrote.
//!
//! # Why three of the four tests here are negative
//!
//! "A refusal writes a record" passes for an implementation that writes one on *every* decision,
//! so the arms that discriminate are the ones asserting none. Gate 5 has two ways to fire and the
//! loop has six other gates, which is three distinct ways to be wrong:
//!
//! - an admitted turn — the volume the re-decide guard refuses to journal;
//! - gate 5 firing on the **breaker** rung, where no ceiling was reached and `admit()` allowed;
//! - a gate **before** 5 deciding, where admission was never checked at all.
//!
//! The suite-wide half of the rule — that no `BudgetDecision{allowed: true}` is ever written by
//! anything — is not here: it is asserted in `Harness::step`, so it holds over every test in this
//! crate rather than over this file.

use std::collections::BTreeMap;

use dudes_proto::goal::Evidence;
use dudes_proto::{
    BudgetScope, Ceiling, ExhaustedReason, ExitStatus, LoopDecision, Record, RecordKind, Spend,
    TurnOutcome,
};
use dudes_supervisor::testing::Harness;
use dudes_supervisor::{Command, Input, RuntimeSignal, SpendView};
use dudes_types::{AgentId, AgentStatus, Tokens, Usd};

/// Every `BudgetDecision` the machine asked to have written, oldest first.
fn refusals(harness: &Harness) -> Vec<&Record> {
    harness
        .records()
        .filter(|record| record.kind() == RecordKind::BudgetDecision)
        .collect()
}

/// The kinds journaled, for a failure message.
fn kinds(harness: &Harness) -> Vec<RecordKind> {
    harness.records().map(Record::kind).collect()
}

/// Every `BudgetDecision` is immediately followed by the `LoopDecided` it produced.
///
/// Over *every* pair rather than the first, and that is not fastidiousness: the tick's sweep
/// calls the same `decide()`, so one trace can hold several decisions and a
/// comparison of first positions would pair a refusal with another decision's verdict — the
/// `[.., BudgetDecision, LoopDecided, LoopDecided, ..]` shape the sweep test below produces.
/// Adjacency is what `decide()` guarantees, two consecutive `persist` calls with nothing between
/// them, so it is adjacency that is asserted. Callers assert separately that a refusal exists at
/// all; a trace with none passes here vacuously and is meant to.
fn each_refusal_precedes_its_verdict(order: &[RecordKind]) {
    for (index, kind) in order.iter().enumerate() {
        assert!(
            *kind != RecordKind::BudgetDecision
                || order.get(index + 1) == Some(&RecordKind::LoopDecided),
            "the BudgetDecision at {index} is not immediately followed by its verdict: {order:?}"
        );
    }
}

/// Tell the machine this agent has spent `usd` today, in the agent scope only.
fn spent_today(harness: &mut Harness, agent: AgentId, usd: Usd) {
    harness.drive(Input::Spend {
        agent,
        view: SpendView {
            agent_today: Spend {
                usd,
                tokens: Tokens::ZERO,
                work_tokens: Tokens::ZERO,
            },
            ..SpendView::default()
        },
    });
}

/// Gate 5 refusing journals one `BudgetDecision{allowed: false}` naming the binding ceiling and
/// the spend in that scope, immediately before the `LoopDecided{Exhausted}` it produced.
///
/// The *position* is asserted, not just the presence: "journaled before the verdict" was stated in
/// three comments and pinned by nothing, so inverting the two writes left the whole suite green.
///
/// The harness hires on a $10/day agent ceiling; $12 spent reaches it, so the agent scope binds —
/// the goal's $5 is untouched, which is also what makes the reported ceiling assertable.
#[test]
fn a_refused_admission_journals_one_budget_decision() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    spent_today(&mut harness, agent, Usd::from_dollars(12));
    let goal = harness.set_goal(agent, &["/usr/bin/false"]);

    let refused = refusals(&harness);
    assert_eq!(
        refused.len(),
        1,
        "exactly one refusal was journaled: {:?}",
        kinds(&harness)
    );
    let Record::BudgetDecision {
        agent: whose,
        goal: scoped_to,
        // Deliberately not asserted here. `Harness::step` runs the suite-wide refusal check on
        // every record *before* it reaches the trace `records()` reads, so a call-site assertion
        // on this field could never be the one that fails — it would read as coverage and be
        // documentation. Bound and discarded rather than elided with `..`, so the destructure
        // stays exhaustive and a new field on the variant breaks this test.
        allowed: _,
        spent,
        unknown_ends,
        per_turn_floor,
        ceiling,
        reason,
    } = refused[0]
    else {
        unreachable!("filtered on the kind")
    };
    assert_eq!(*whose, agent, "the refusal names the agent it refused");
    assert_eq!(
        *scoped_to,
        Some(goal),
        "and the goal whose loop asked for the turn"
    );
    assert_eq!(
        *ceiling,
        Ceiling {
            scope: BudgetScope::Agent,
            usd: Some(Usd::from_dollars(10)),
            tokens: None,
        },
        "the binding ceiling, as gate 5 read it"
    );
    assert_eq!(
        spent.usd,
        Usd::from_dollars(12),
        "and the observed spend in that scope"
    );
    assert_eq!(*unknown_ends, 0);
    assert_eq!(per_turn_floor.usd, Usd::from_micros(1));
    assert_eq!(
        reason.as_deref(),
        Some("agent ceiling reached: $12.000000 of $10.000000"),
        "with the detail gate 5 already wrote, so the two cannot disagree"
    );

    // Beside, and *before*: the admission check runs before the turn is issued, so its record
    // precedes the verdict it produced and the journal reads in the order the daemon decided in.
    // Adjacent, because one `handle()` call emits both.
    each_refusal_precedes_its_verdict(&kinds(&harness));

    // And it is beside, not instead of: the verdict stays on its own record.
    assert!(
        harness.records().any(|record| matches!(
            record,
            Record::LoopDecided {
                decision: LoopDecision::Exhausted {
                    reason: ExhaustedReason::AgentBudget
                },
                ..
            }
        )),
        "the verdict is journaled too: {:?}",
        kinds(&harness)
    );
    assert!(
        !harness
            .records()
            .any(|record| matches!(record, Record::TurnIssued { .. })),
        "and invariant 4 held: nothing was issued: {:?}",
        kinds(&harness)
    );
}

/// The control that makes the rule falsifiable: the identical fixture **under** the ceiling issues
/// a turn and journals no `BudgetDecision` at all.
///
/// Without this arm an implementation that journaled one on every decision would pass the test
/// above. An admitted case writes nothing: gate 5's passing note on
/// the `LoopDecided` already carries every scope, every ceiling and every spend.
#[test]
fn an_admitted_turn_journals_no_budget_decision() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    spent_today(&mut harness, agent, Usd::from_cents(1));
    harness.set_goal(agent, &["/usr/bin/false"]);

    assert!(
        harness
            .records()
            .any(|record| matches!(record, Record::TurnIssued { .. })),
        "control: under the ceiling the turn was issued: {:?}",
        kinds(&harness)
    );
    assert!(
        refusals(&harness).is_empty(),
        "and an admission is not journaled: {:?}",
        kinds(&harness)
    );
}

/// Gate 5's other half journals nothing. The breaker rung refuses the turn with
/// `Exhausted{Breaker}` while no ceiling was reached, so `admit()` allowed — and the
/// writes the record on the admission check, not on the gate.
#[test]
fn the_breaker_half_of_gate_5_journals_no_budget_decision() {
    let mut harness = Harness::new();
    let (agent, goal) = harness.hire_with_goal(&["/usr/bin/false"]);
    let turn = harness.open_turn(agent).expect("a turn was issued");

    // Two beats with a signal in each: `Healthy` → `Steer` → `Constrain`, one rung per beat.
    for _ in 0..2 {
        harness.drive(Input::Signal {
            agent,
            signal: RuntimeSignal::UnackedMail,
        });
        harness.drive(Input::Tick);
    }
    // End the turn and fail the predicate, so the loop decides again and reaches gate 5.
    harness.drive(Input::ChildExited {
        report: dudes_proto::TurnReport::default(),
        final_reply: None,
        elapsed_ms: None,
        turn,
        exit: ExitStatus {
            code: Some(0),
            signal: None,
        },
        usage: None,
        model_usage: BTreeMap::new(),
        outcome: TurnOutcome::Completed,
        last_message: None,
    });
    harness.drive(Input::PredicateDone {
        goal,
        pass: false,
        evidence: Evidence::Command {
            argv: vec!["/usr/bin/false".into()],
            program: Some("/usr/bin/false".into()),
            exit: Some(1),
            output_tail: String::new(),
        },
    });

    assert!(
        harness.records().any(|record| matches!(
            record,
            Record::LoopDecided {
                decision: LoopDecision::Exhausted {
                    reason: ExhaustedReason::Breaker
                },
                ..
            }
        )),
        "control: the breaker fired gate 5: {:?}",
        kinds(&harness)
    );
    assert!(
        refusals(&harness).is_empty(),
        "and no ceiling was reached, so nothing was refused: {:?}",
        kinds(&harness)
    );
}

/// The tick's sweep re-decides an exhausted agent whose input changed, and when the
/// new input is admitted it journals **no** second `BudgetDecision`.
///
/// The interaction of the sweep and the refusal record. The sweep calls the same `decide()`, so a second
/// verdict lands in the same trace — measured shape, `[AgentHired, GoalSet, BudgetDecision,
/// LoopDecided, LoopDecided, TurnIssued, HookTokenMinted, TurnSpawned]` — which is exactly what
/// breaks an ordering assertion written over first positions instead of every pair. It also pins
/// the re-decide guard under its new mechanism: re-deciding is not a second refusal unless it refuses.
#[test]
fn a_swept_re_decide_that_is_admitted_journals_no_second_refusal() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    spent_today(&mut harness, agent, Usd::from_dollars(12));
    harness.set_goal(agent, &["/usr/bin/false"]);
    assert_eq!(
        refusals(&harness).len(),
        1,
        "control: the agent was refused to begin with: {:?}",
        kinds(&harness)
    );
    assert_eq!(
        harness.supervisor.status_of(agent),
        AgentStatus::Exhausted,
        "control: and the sweep's candidate filter will see it"
    );

    // The UTC day rolls over: the runtime pushes the new spend, and the next beat sweeps.
    spent_today(&mut harness, agent, Usd::from_cents(1));
    harness.drive(Input::Tick);

    let order = kinds(&harness);
    assert!(
        harness
            .records()
            .any(|record| matches!(record, Record::TurnIssued { .. })),
        "control: the sweep re-decided and the turn was issued: {order:?}"
    );
    assert_eq!(
        refusals(&harness).len(),
        1,
        "and the admitted re-decide added no second BudgetDecision: {order:?}"
    );
    each_refusal_precedes_its_verdict(&order);
}

/// A gate before 5 decides and admission is never checked, so there is nothing to journal — even
/// on a spend far past every ceiling. Invariant 4's record is of a check that ran.
#[test]
fn a_gate_before_5_journals_no_budget_decision() {
    let mut harness = Harness::new();
    let agent = harness.hire();
    spent_today(&mut harness, agent, Usd::from_dollars(1_000));
    harness.drive(Input::Command(Command::Halt { agent, text: None }));
    harness.set_goal(agent, &["/usr/bin/false"]);

    assert!(
        harness.records().any(|record| matches!(
            record,
            Record::LoopDecided {
                decision: LoopDecision::Hold { .. },
                ..
            }
        )),
        "control: gate 1 held the agent before gate 5 was reached: {:?}",
        kinds(&harness)
    );
    assert!(
        refusals(&harness).is_empty(),
        "and an unchecked admission is not a refusal: {:?}",
        kinds(&harness)
    );
}
