//! `agent.hire` and `goal.set` refuse a call that sets no ceiling while the daemon's own
//! environment carries a billing credential a managed child would inherit.
//!
//! # Why this asserts on the journal and not on the reply alone
//!
//! `Record::AgentHired` and `Record::GoalSet` cannot be un-journaled (invariant 1). So "refused"
//! has to mean *refused before the append*, not "answered with an error after the record reached
//! the disk" — and only the disk can tell those two apart. A gate moved to *after* the append
//! answers `-32004` exactly the same way. `Fixture::records` reads the files the daemon is
//! writing, which is the only thing that discriminates.
//!
//! # The five arms
//!
//! 1. **The refusal fires.** A key and no ceiling anywhere: refused, typed, nothing journaled.
//!    Kills deleting the arm, and — through the journal count — moving it behind the pump. Both
//!    methods carry it: on the hire directly, and on the goal through a daemon that gained the key
//!    between runs, which is the configuration in which an agent can exist with no ceiling at all.
//! 2. **A ceiling present is admitted.** The same environment with a ceiling on the call itself.
//!    Kills a query given projection state alone: on a hire there is no row to read a ceiling from,
//!    so that implementation refuses a hire carrying `--usd-per-day 25`.
//! 3. **The key absent is admitted.** Kills a query that ignores the environment and refuses on the
//!    ceilings alone.
//! 4. **Present is not positive.** `--usd-per-day 0` under a visible key is admitted *at the hire*
//!    and refused *by the budget gate at the first turn*. Kills the natural-looking `any ceiling > 0`, which
//!    passes arms 1–3 and turns a deliberate "spend nothing" into a false refusal aimed at exactly
//!    the operator this rule exists to protect.
//! 5. **Set-but-empty is admitted.** An exported but blank variable authenticates nothing and bills
//!    nothing. Kills a membership test that drops the non-empty half, which would regress behaviour
//!    the provider crate already holds.
//!
//! Arms 2, 3 and 5 each differ from arm 1 in exactly one input, and each arm that refuses carries
//! its own admitted control one input away — so a refusal that fired always, or never, is red in
//! several rows at once.
//!
//! # What the five do not cover, and the arms that do
//!
//! **Where the refusal must stay silent.** `RefusalGate::CeilingRequired` means what its doc says
//! and nothing wider, so `goal.set` on an agent that is missing or retired is **not** this arm's
//! question: `goal.set` answers it as absent, and journals nothing.
//!
//! Refusing with this arm there would put a message asserting a
//! billing credential in front of an operator whose daemon had none and whose call carried
//! `--usd 5`. The last three tests here hold that: the two shapes answered as absent, and a third
//! under a visible key with no ceiling on the call, with a live ceiling-less agent beside them as
//! the control proving the arm is armed.
//!
//! **A configured floor is also a ceiling**, so a call that sets none of its own is admitted under
//! one. That arm kills an implementation reading only the call's own params, which would pass every
//! arm above while telling an operator with a floor configured to set a ceiling they already have.
//!
//! # The same questions, now asked of every name
//!
//! The adapter's billing list is no longer one variable, so arms 1, 2 and 5 are asked **per name**,
//! driven by `ClaudeAdapter::billing_env_vars()` and never by a literal here: a name added to the
//! constant with no arm beside it would otherwise ship unasserted, which is the failure this whole
//! file is shaped against. [`billing_names`] asserts the denominator before any of those loops,
//! because a loop over an emptied list passes and reports that as proof.
//!
//! Two controls join them. A variable that is **not** on the list is not a credential, tested in
//! both directions — one name that contains a listed name and one that a listed name contains —
//! because a membership test that decayed to `starts_with` or `contains` can decay either way and
//! an unrelated name like `EDITOR` leaves both green. And the goal-side arm is asked of
//! `ANTHROPIC_AUTH_TOKEN` rather than of the API key: it is the name the vendor's own credential
//! report on the init line reads as `"none"`, so it is the member of the list that nothing except
//! this environment query can see.
#![cfg(unix)]

use std::sync::Arc;

use duded::settings::Seams;
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::rpc::agent::HireParams;
use dudes_proto::rpc::goal::GoalSetParams;
use dudes_proto::rpc::{ErrorCode, RefusalGate, RefusedData, RpcError};
use dudes_proto::{BudgetScope, Ceiling, Record, Spend};
use dudes_providers::adapter::Adapter;
use dudes_providers::claude::ClaudeAdapter;
use dudes_providers::codex::CodexAdapter;
use dudes_providers::launch::EnvVar;
use dudes_providers::pi::PiAdapter;
use dudes_providers::registry::Adapters;
use dudes_types::{AgentId, Provider, Tokens, Usd};

mod common;

use common::{Fixture, ScriptedRunner, SharedHost, TestClock, seams};

/// A value shaped like a credential and usable as none. Never a real key.
const FIXTURE_KEY: &str = "fixture-value-not-a-credential";

/// The variable the shipped Claude adapter answers with.
const KEY_VAR: &str = "ANTHROPIC_API_KEY";

/// A running daemon whose environment is `env` and whose floor ceiling is `floor`.
struct Daemon {
    fixture: Fixture,
    running: common::Running,
}

impl Daemon {
    fn new(env: Vec<EnvVar>, floor: Option<Usd>) -> Self {
        Self::boot_on(Fixture::new(), env, floor)
    }

    /// Boot over a state directory that already holds a journal, so a second run can meet an
    /// environment the first one did not have.
    fn boot_on(fixture: Fixture, env: Vec<EnvVar>, floor: Option<Usd>) -> Self {
        let mut settings = fixture.settings();
        settings.daemon_env = env;
        settings.floor_usd_per_day = floor;
        let daemon = boot(
            &settings,
            seams(
                SharedHost::new(),
                Arc::new(ScriptedRunner::scripted([common::passes()])),
                Arc::new(TestClock::new()),
                Witness::default(),
            ),
        )
        .expect("boot");
        Self {
            fixture,
            running: common::start(daemon),
        }
    }

    fn hire(&self, budget: dudes_proto::AgentBudget) -> Result<AgentId, RpcError> {
        let params = HireParams {
            budget,
            ..common::hire_params(self.fixture.state.path())
        };
        self.running
            .handle
            .ask(duded::ports::Ask::Hire(Box::new(params)))
            .map(|answered| match answered {
                duded::ports::Answered::Hired(result) => result.agent,
                other => panic!("agent.hire answered {other:?}"),
            })
    }

    fn set_goal(&self, params: GoalSetParams) -> Result<(), RpcError> {
        self.running
            .handle
            .ask(duded::ports::Ask::SetGoal(Box::new(params)))
            .map(|_| ())
    }

    fn count(&self, wanted: fn(&Record) -> bool) -> usize {
        self.fixture
            .records()
            .iter()
            .filter(|record| wanted(record))
            .count()
    }

    /// Stop the daemon, then read its journal — in that order, so a turn the loop was still
    /// issuing cannot be missed — and hand back the state directory so a caller may boot over it
    /// again with a different environment.
    fn stop(mut self) -> (Fixture, Vec<Record>) {
        self.running.stop(ExitCause::Ended);
        let records = self.fixture.records();
        (self.fixture, records)
    }
}

/// The environment a daemon that can be billed has.
fn keyed() -> Vec<EnvVar> {
    vec![
        EnvVar::new("PATH", "/usr/bin:/bin"),
        EnvVar::new(KEY_VAR, FIXTURE_KEY),
    ]
}

/// The same environment with the variable gone.
fn keyless() -> Vec<EnvVar> {
    vec![EnvVar::new("PATH", "/usr/bin:/bin")]
}

/// The shipped billing list, read through the trait the daemon reads it through.
///
/// **Never a copy of the constant.** An arm driven by a literal written here stays green when a
/// name is added to the adapter and nothing else — the one regression the per-name arms exist to
/// catch. The length is asserted before any caller loops: a loop over an emptied or truncated list
/// passes vacuously, and a vacuous pass is indistinguishable from a real one in the output.
fn billing_names() -> &'static [&'static str] {
    let names = ClaudeAdapter::default().billing_env_vars();
    assert_eq!(
        names.len(),
        6,
        "the denominator the loops below run over, asserted before they run: {names:?}"
    );
    names
}

/// A daemon environment carrying exactly one candidate credential, `name`, set to `value`.
fn env_with(name: &str, value: &str) -> Vec<EnvVar> {
    vec![
        EnvVar::new("PATH", "/usr/bin:/bin"),
        EnvVar::new(name, value),
    ]
}

/// A budget with no ceiling in either unit.
fn no_ceiling() -> dudes_proto::AgentBudget {
    dudes_proto::AgentBudget {
        usd_per_day: None,
        tokens_per_day: None,
    }
}

/// `goal.set` params on `agent` with no ceiling in either unit.
fn goal_without_a_ceiling(agent: AgentId) -> GoalSetParams {
    let mut params = common::goal_params(agent, &["/usr/bin/true"]);
    params.budget.usd = None;
    params.budget.tokens = None;
    params
}

/// An agent id no hire in these tests ever produces.
fn unknown_agent() -> AgentId {
    AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a ulid")
}

/// Retire `agent` through the same port `agent.retire` reaches the loop by.
fn retire(daemon: &Daemon, agent: AgentId) {
    daemon
        .running
        .handle
        .ask(duded::ports::Ask::Retire(Box::new(
            dudes_proto::rpc::agent::RetireParams {
                agent,
                reason: "retired by the test".to_owned(),
            },
        )))
        .expect("the retirement");
}

/// The typed gate on a refusal, or a panic saying what came instead.
fn gate_of(error: &RpcError) -> RefusalGate {
    assert_eq!(
        error.code,
        ErrorCode::Refused.code(),
        "refused with -32004, not some other failure: {error:?}"
    );
    let data = error.data.clone().expect("the refusal carries typed data");
    let parsed: RefusedData =
        serde_json::from_value(data).expect("the data is the contract's RefusedData");
    parsed.gate
}

/// **Arm 1, on the hire.** A visible key and no ceiling in any scope or unit: `agent.hire` is
/// refused, with the typed gate, before anything is written.
///
/// The second hire is this arm's own control: the same daemon, the same environment, one input
/// changed — a ceiling on the call — and it is admitted and journaled. Without it, a daemon that
/// refused *every* hire would satisfy the first half.
#[test]
fn a_hire_with_no_ceiling_under_a_visible_key_is_refused_before_the_append() {
    let daemon = Daemon::new(keyed(), None);

    assert_eq!(
        daemon.count(|record| matches!(record, Record::AgentHired { .. })),
        0,
        "nothing is hired before this test hires"
    );

    let refusal = daemon
        .hire(no_ceiling())
        .expect_err("a hire with no ceiling under a visible key must be refused");
    assert_eq!(gate_of(&refusal), RefusalGate::CeilingRequired);
    assert_eq!(
        daemon.count(|record| matches!(record, Record::AgentHired { .. })),
        0,
        "the refusal is in front of the append: AgentHired cannot be un-journaled"
    );

    daemon
        .hire(dudes_proto::AgentBudget {
            usd_per_day: Some(Usd::from_dollars(25)),
            tokens_per_day: None,
        })
        .expect("control: the same call carrying a ceiling is admitted");
    assert_eq!(
        daemon.count(|record| matches!(record, Record::AgentHired { .. })),
        1
    );
    let _ = daemon.stop();
}

/// **Arm 1, on the goal — and the scenario it exists for.**
///
/// On a daemon that has carried the key since boot, every agent that exists was hired under a
/// ceiling, because the hire arm above refuses the alternative. So `goal.set`'s own guard looks
/// redundant until you ask when the agent was hired: **the environment can change between runs.**
/// An operator hires on a subscription with no ceiling anywhere — admitted, correctly — then
/// exports an API key and restarts. The agent row in the replayed journal has no ceiling, and the
/// daemon can now be billed.
///
/// This test is that sequence: one boot without the key to create the row, a second boot over the
/// same state directory with it. Without the `goal.set` guard, the first goal after that restart
/// runs on a billable key with nothing but the hop cap to stop it.
///
/// **Control, in the second boot and one input apart:** the same `goal.set` carrying a token
/// ceiling is admitted. It is what makes this a test of the missing ceiling rather than of the
/// restart.
///
/// This is when the guard *fires*; it is consulted far more often than that and must stay silent
/// for everything else. The last three tests in this file are the silent half.
#[test]
fn a_goal_on_an_agent_hired_before_the_key_appeared_is_refused() {
    let first = Daemon::new(keyless(), None);
    let agent = first
        .hire(no_ceiling())
        .expect("no key at hire time, so no ceiling is required");
    let (fixture, records) = first.stop();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::AgentHired { .. }))
            .count(),
        1,
        "the ceiling-less agent is on the disk for the second boot to replay"
    );

    let daemon = Daemon::boot_on(fixture, keyed(), None);
    let before = daemon.count(|record| matches!(record, Record::GoalSet { .. }));
    let refusal = daemon
        .set_goal(goal_without_a_ceiling(agent))
        .expect_err("the key is visible now and nothing caps this agent");
    assert_eq!(gate_of(&refusal), RefusalGate::CeilingRequired);
    assert_eq!(
        daemon.count(|record| matches!(record, Record::GoalSet { .. })),
        before,
        "and nothing was journaled for it: GoalSet cannot be un-journaled either"
    );

    let mut with_ceiling = goal_without_a_ceiling(agent);
    with_ceiling.budget.tokens = Some(Tokens::new(1_000));
    daemon
        .set_goal(with_ceiling)
        .expect("control: the same goal carrying a ceiling is admitted");
    assert_eq!(
        daemon.count(|record| matches!(record, Record::GoalSet { .. })),
        before + 1
    );
    let _ = daemon.stop();
}

/// **Arm 2.** The same environment, with a ceiling on the call itself: admitted, and the record is
/// written.
///
/// This is the arm that goes red if the query reads projection state alone. On `agent.hire` there
/// is no agent row yet — the arm sits in front of the append by construction — so the only place
/// the hire's own ceiling exists is `HireParams.budget`.
#[test]
fn a_ceiling_on_the_call_itself_is_admitted_under_a_visible_key() {
    let daemon = Daemon::new(keyed(), None);

    let agent = daemon
        .hire(dudes_proto::AgentBudget {
            usd_per_day: Some(Usd::from_dollars(25)),
            tokens_per_day: None,
        })
        .expect("a dollar ceiling on the hire admits it");
    assert_eq!(
        daemon.count(|record| matches!(record, Record::AgentHired { .. })),
        1,
        "and the hire is on the disk, not merely answered"
    );

    // A token ceiling on the goal, with the goal's dollars absent: either unit satisfies the rule,
    // so this also pins that the query asks about ceilings and not about dollars.
    let mut params = goal_without_a_ceiling(agent);
    params.budget.tokens = Some(Tokens::new(1_000));
    daemon
        .set_goal(params)
        .expect("a token ceiling on the goal admits it");
    assert_eq!(
        daemon.count(|record| matches!(record, Record::GoalSet { .. })),
        1
    );

    let _ = daemon.fixture.await_turn();
    let (_fixture, records) = daemon.stop();
    assert!(
        records
            .iter()
            .any(|record| matches!(record, Record::TurnIssued { .. })),
        "and the goal issued a turn: admitted means the loop runs, not merely that a record \
         landed"
    );
}

/// **Arm 3.** The same two calls with the variable unset are admitted with no ceiling anywhere.
///
/// Nothing visible says a bill is running, so nothing is required. A subscription operator who
/// never exported a key types no dollar number at all.
#[test]
fn no_ceiling_with_the_key_unset_is_admitted() {
    let daemon = Daemon::new(keyless(), None);

    let agent = daemon
        .hire(no_ceiling())
        .expect("no key, no ceiling required");
    daemon
        .set_goal(goal_without_a_ceiling(agent))
        .expect("no key, no ceiling required on the goal either");

    let _ = daemon.fixture.await_turn();
    let (_fixture, records) = daemon.stop();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::AgentHired { .. }))
            .count(),
        1
    );
    assert!(
        records
            .iter()
            .any(|record| matches!(record, Record::TurnIssued { .. })),
        "and the turn issued: an all-absent configuration is bounded by the hop cap, not refused"
    );
}

/// **Arm 4: *present* is not *positive*.** `--usd-per-day 0` under a visible key is **admitted at
/// the hire** and **refused by the budget gate at the first turn**.
///
/// Zero is a ceiling the operator set — a ceiling of nothing — and it is the instruction of an
/// operator who means *spend nothing*. An implementation testing *is any ceiling greater than
/// zero* passes arms 1, 2 and 3 and fails here, refusing that operator at the door for having
/// asked for the strictest thing the CLI can express.
///
/// Both halves are asserted, because either alone is satisfiable by the wrong code: admission
/// alone would pass for an implementation that had stopped checking ceilings at all, and the gate
/// 5 refusal alone is what the zero row has always meant.
#[test]
fn a_zero_ceiling_is_admitted_at_the_hire_and_refused_at_the_first_turn() {
    let daemon = Daemon::new(keyed(), None);

    let agent = daemon
        .hire(dudes_proto::AgentBudget {
            usd_per_day: Some(Usd::ZERO),
            tokens_per_day: None,
        })
        .expect("a zero ceiling is a ceiling: the hire is admitted");
    assert_eq!(
        daemon.count(|record| matches!(record, Record::AgentHired { .. })),
        1,
        "and it is journaled"
    );

    daemon
        .set_goal(goal_without_a_ceiling(agent))
        .expect("the agent's zero ceiling satisfies the rule for the goal too");

    let (_fixture, records) = daemon.stop();
    let refusals: Vec<&Record> = records
        .iter()
        .filter(|record| matches!(record, Record::BudgetDecision { allowed: false, .. }))
        .collect();
    assert!(
        matches!(
            refusals.as_slice(),
            [
                Record::BudgetDecision {
                    ceiling: Ceiling {
                        scope: BudgetScope::Agent,
                        usd: Some(Usd::ZERO),
                        tokens: None,
                    },
                    spent: Spend { usd: Usd::ZERO, .. },
                    ..
                },
                ..
            ]
        ),
        "the budget gate refuses on the agent's zero ceiling before any spend: {refusals:?}"
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::TurnIssued { .. })),
        "and no turn was ever issued: 'spend nothing' means nothing runs"
    );
}

/// **Arm 5: set-but-empty is admitted.** An exported but blank variable is not a credential.
///
/// The provider crate's own predicate already holds this line — an empty value authenticates
/// nothing — and a new list-based check that treated a present-but-empty variable as a billing
/// credential would regress it, refusing a hire for a key that cannot pay for anything.
#[test]
fn a_set_but_empty_key_is_not_a_billing_credential() {
    let daemon = Daemon::new(
        vec![
            EnvVar::new("PATH", "/usr/bin:/bin"),
            EnvVar::new(KEY_VAR, ""),
        ],
        None,
    );

    let agent = daemon
        .hire(no_ceiling())
        .expect("an empty value bills nothing, so no ceiling is required");
    daemon
        .set_goal(goal_without_a_ceiling(agent))
        .expect("and the goal is admitted for the same reason");

    let (_fixture, records) = daemon.stop();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::AgentHired { .. }))
            .count(),
        1
    );
}

/// The floor ceiling satisfies the rule on its own: an operator who configured
/// `[budget] floor_usd_per_day` has set a ceiling, and neither call needs another.
///
/// The control that keeps arms 1–5 honest about *where* a ceiling may live. Without it, an
/// implementation that only ever looked at the call's own params would pass every arm above, and
/// an operator with a floor configured would be told to set a ceiling they already have.
#[test]
fn a_configured_floor_ceiling_admits_a_call_that_sets_none_of_its_own() {
    let daemon = Daemon::new(keyed(), Some(Usd::from_dollars(100)));

    let agent = daemon
        .hire(no_ceiling())
        .expect("the configured floor is a ceiling in a scope");
    daemon
        .set_goal(goal_without_a_ceiling(agent))
        .expect("and it covers the goal too");

    let (_fixture, records) = daemon.stop();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::AgentHired { .. }))
            .count(),
        1
    );
}

/// **A goal on an agent that does not exist is not a ceiling problem.** `goal.set` answers it as
/// absent, before this arm reads the call's ceilings or the environment, and journals nothing.
///
/// The failure this guards against is looking the row up *before*
/// reading the call's own ceilings or the environment, which answers an unknown agent
/// `CeilingRequired` — for want of a ceiling the call plainly carried, on a keyless daemon, with a
/// message asserting a billing credential that daemon did not have.
#[test]
fn a_goal_on_an_unknown_agent_is_absent_not_a_ceiling_problem() {
    let daemon = Daemon::new(keyless(), None);

    let mut params = goal_without_a_ceiling(unknown_agent());
    params.budget.usd = Some(Usd::from_dollars(5));
    let refusal = daemon
        .set_goal(params)
        .expect_err("an unknown agent has no goal to set");
    assert_eq!(refusal.code, ErrorCode::NotFound.code(), "{refusal:?}");

    let (_fixture, records) = daemon.stop();
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::GoalSet { .. } | Record::TurnIssued { .. })),
        "nothing is journaled for it: {:?}",
        records.iter().map(Record::kind).collect::<Vec<_>>()
    );
}

/// **A goal on a retired agent, likewise** — the sibling of the arm above.
///
/// **Control:** the identical call one moment earlier, while the agent is live, is admitted — so
/// this arm asserts the retirement is what changed the answer.
#[test]
fn a_goal_on_a_retired_agent_is_absent_not_a_ceiling_problem() {
    let daemon = Daemon::new(keyless(), None);
    let agent = daemon
        .hire(no_ceiling())
        .expect("a hire on a keyless daemon");

    let mut before_retirement = goal_without_a_ceiling(agent);
    before_retirement.budget.usd = Some(Usd::from_dollars(5));
    daemon
        .set_goal(before_retirement)
        .expect("control: admitted while the agent is live");

    retire(&daemon, agent);

    let mut after_retirement = goal_without_a_ceiling(agent);
    after_retirement.budget.usd = Some(Usd::from_dollars(5));
    let refusal = daemon
        .set_goal(after_retirement)
        .expect_err("a retired agent has no goal to set");
    assert_eq!(refusal.code, ErrorCode::NotFound.code(), "{refusal:?}");
    let _ = daemon.stop();
}

/// **The missing and retired rows under a visible key, with no ceiling anywhere on the call.**
///
/// The two arms above each carry `--usd 5`, which ends the ceiling question before a row is read.
/// Here nothing does, so this is the arm that shows an unknown or retired agent is answered as
/// absent and never `CeilingRequired`.
///
/// The agents are hired on a keyless boot so they can exist with no ceiling at all, then met by a
/// daemon that can be billed — the same restart the goal arm's own refusal needs.
///
/// **Control, in the same configuration:** a *live* ceiling-less agent **is** refused with
/// `CeilingRequired`, so the arm is armed here.
#[test]
fn the_missing_and_retired_rows_are_absent_under_a_visible_key() {
    let first = Daemon::new(keyless(), None);
    let live = first.hire(no_ceiling()).expect("a ceiling-less hire");
    let retired = first
        .hire(no_ceiling())
        .expect("a second ceiling-less hire");
    retire(&first, retired);
    let (fixture, _) = first.stop();

    let daemon = Daemon::boot_on(fixture, keyed(), None);

    for agent in [unknown_agent(), retired] {
        let refusal = daemon
            .set_goal(goal_without_a_ceiling(agent))
            .expect_err("an absent agent");
        assert_eq!(refusal.code, ErrorCode::NotFound.code(), "{refusal:?}");
    }

    let refusal = daemon
        .set_goal(goal_without_a_ceiling(live))
        .expect_err("control: a live ceiling-less agent under a key is refused");
    assert_eq!(
        gate_of(&refusal),
        RefusalGate::CeilingRequired,
        "the arm is armed in this configuration, so the two answers above mean something"
    );

    let (_fixture, records) = daemon.stop();
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::TurnIssued { .. })),
        "and nothing ran: {:?}",
        records.iter().map(Record::kind).collect::<Vec<_>>()
    );
}

/// **Arm 1, per name.** Every variable the adapter calls a billing credential refuses a
/// ceiling-less `agent.hire`, before the append, with the typed gate.
///
/// Each row carries its own control one input away: the same daemon, the same environment, a
/// ceiling on the call — admitted and journaled. Without it a daemon that refused every hire would
/// satisfy the first half of every row at once.
///
/// **What one row per name catches, and what it does not.** Not deletion from the constant: a
/// deleted name fails the by-name unit list in `dudes-providers` and `billing_names()`'s
/// denominator assertion, and that assertion runs before any row does, so none of these rows
/// executes. These rows are the check on the *consumer* — the daemon reading fewer names than
/// the adapter lists. Measured with the daemon's query truncated to `billing_env_vars()[..1]` and
/// to `[..5]`: the denominator still holds, the ceiling-less hire is admitted under the first
/// dropped name, and the panic below names that name rather than reporting a count. A single arm
/// standing for six would admit five names in silence.
#[test]
fn every_billing_variable_refuses_a_ceiling_less_hire() {
    for name in billing_names() {
        let daemon = Daemon::new(env_with(name, FIXTURE_KEY), None);

        let refusal = match daemon.hire(no_ceiling()) {
            Err(error) => error,
            Ok(agent) => panic!("{name}: a ceiling-less hire was admitted as {agent:?}"),
        };
        assert_eq!(
            gate_of(&refusal),
            RefusalGate::CeilingRequired,
            "{name}: refused by the ceiling gate, not by something else"
        );
        assert_eq!(
            daemon.count(|record| matches!(record, Record::AgentHired { .. })),
            0,
            "{name}: the refusal is in front of the append"
        );

        daemon
            .hire(dudes_proto::AgentBudget {
                usd_per_day: Some(Usd::from_dollars(25)),
                tokens_per_day: None,
            })
            .unwrap_or_else(|error| {
                panic!("{name}: control, the same call carrying a ceiling: {error:?}")
            });
        assert_eq!(
            daemon.count(|record| matches!(record, Record::AgentHired { .. })),
            1,
            "{name}: the control is the one hire on the disk"
        );
        let _ = daemon.stop();
    }
}

/// **Arm 5, per name.** Every listed variable, exported but blank, is admitted with no ceiling
/// anywhere — on the hire and on the goal that follows it.
///
/// An empty value authenticates nothing and bills nothing, and the provider crate's own predicate
/// already says so. Widening the list to six names widens the blast radius of dropping the
/// non-empty half of the query, so the arm widens with it: five of these rows are environments the
/// single-name version of this test never met.
#[test]
fn every_billing_variable_set_but_empty_is_admitted() {
    for name in billing_names() {
        let daemon = Daemon::new(env_with(name, ""), None);

        let agent = daemon.hire(no_ceiling()).unwrap_or_else(|error| {
            panic!("{name}: an empty value bills nothing, so no ceiling is required: {error:?}")
        });
        daemon
            .set_goal(goal_without_a_ceiling(agent))
            .unwrap_or_else(|error| {
                panic!("{name}: and the goal is admitted for the same reason: {error:?}")
            });

        let (_fixture, records) = daemon.stop();
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, Record::AgentHired { .. }))
                .count(),
            1,
            "{name}: the hire reached the disk"
        );
    }
}

/// **Control: a variable that is not on the list is not a credential** — in both directions.
///
/// `ANTHROPIC_API_KEY_BACKUP` contains a listed name; `ANTHROPIC_AWS` is contained by one. A
/// membership test that decayed from whole-name equality to `starts_with` or `contains` decays in
/// one of those two directions, and only the matching half is red. An unrelated name — `EDITOR`,
/// say — leaves both bugs green, which is why neither of these is unrelated.
///
/// `ANTHROPIC_SETTINGS` is the third: a real vendor variable that is not a credential at all.
#[test]
fn a_variable_that_is_not_on_the_list_is_not_a_billing_credential() {
    let daemon = Daemon::new(
        vec![
            EnvVar::new("PATH", "/usr/bin:/bin"),
            EnvVar::new("ANTHROPIC_API_KEY_BACKUP", FIXTURE_KEY),
            EnvVar::new("ANTHROPIC_AWS", FIXTURE_KEY),
            EnvVar::new("ANTHROPIC_SETTINGS", FIXTURE_KEY),
        ],
        None,
    );

    let agent = daemon
        .hire(no_ceiling())
        .expect("none of these names selects a billing credential");
    daemon
        .set_goal(goal_without_a_ceiling(agent))
        .expect("and the goal is admitted for the same reason");

    let (_fixture, records) = daemon.stop();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::AgentHired { .. }))
            .count(),
        1
    );
}

/// **Arm 1 on the goal, for a name the vendor's own report cannot see.**
///
/// The sequence is the restart one: an agent hired with no ceiling on a daemon that could not be
/// billed, then a second boot over the same journal with a credential exported. This time the
/// credential arrives on `ANTHROPIC_AUTH_TOKEN`, which the vendor reports as `"none"` on the init
/// line of a turn it has already started — so the environment query asked here is the only thing
/// in the daemon that can refuse this goal, and if it read one name it would admit it.
///
/// **Control, second boot, one input apart:** the same goal carrying a token ceiling is admitted.
#[test]
fn a_goal_on_an_agent_hired_before_a_bearer_token_appeared_is_refused() {
    let first = Daemon::new(keyless(), None);
    let agent = first
        .hire(no_ceiling())
        .expect("no credential at hire time, so no ceiling is required");
    let (fixture, _records) = first.stop();

    let daemon = Daemon::boot_on(fixture, env_with("ANTHROPIC_AUTH_TOKEN", FIXTURE_KEY), None);
    let before = daemon.count(|record| matches!(record, Record::GoalSet { .. }));
    let refusal = daemon
        .set_goal(goal_without_a_ceiling(agent))
        .expect_err("a bearer token is visible now and nothing caps this agent");
    assert_eq!(gate_of(&refusal), RefusalGate::CeilingRequired);
    assert_eq!(
        daemon.count(|record| matches!(record, Record::GoalSet { .. })),
        before,
        "and nothing was journaled for it"
    );

    let mut with_ceiling = goal_without_a_ceiling(agent);
    with_ceiling.budget.tokens = Some(Tokens::new(1_000));
    daemon
        .set_goal(with_ceiling)
        .expect("control: the same goal carrying a ceiling is admitted");
    assert_eq!(
        daemon.count(|record| matches!(record, Record::GoalSet { .. })),
        before + 1
    );
    let _ = daemon.stop();
}

/// A daemon over `fixture` whose registry holds Claude, Codex and Pi, with `env` and a $100 floor
/// ceiling.
fn every_engine(fixture: Fixture, env: Vec<EnvVar>) -> Daemon {
    let mut settings = fixture.settings();
    settings.daemon_env = env;
    settings.floor_usd_per_day = Some(Usd::from_dollars(100));
    let daemon = boot(
        &settings,
        Seams {
            adapters: Adapters::new([
                Box::new(ClaudeAdapter::default()) as Box<dyn Adapter>,
                Box::new(CodexAdapter::default()),
                Box::new(PiAdapter::default()),
            ]),
            ..seams(
                SharedHost::new(),
                Arc::new(ScriptedRunner::scripted([common::passes()])),
                Arc::new(TestClock::new()),
                Witness::default(),
            )
        },
    )
    .expect("boot");
    Daemon {
        fixture,
        running: common::start(daemon),
    }
}

/// `agent.hire` of an agent called `name` on `provider` with `budget`.
fn hire_of(
    daemon: &Daemon,
    name: &str,
    provider: Provider,
    budget: dudes_proto::AgentBudget,
) -> Result<AgentId, RpcError> {
    let params = HireParams {
        name: name.to_owned(),
        provider,
        budget,
        ..common::hire_params(daemon.fixture.state.path())
    };
    daemon
        .running
        .handle
        .ask(duded::ports::Ask::Hire(Box::new(params)))
        .map(|answered| match answered {
            duded::ports::Answered::Hired(result) => result.agent,
            other => panic!("agent.hire answered {other:?}"),
        })
}

/// Codex and Pi report no dollars, so under one of their billing variables only
/// an agent or goal token ceiling satisfies the guard, and the refusal names it. A dollar ceiling
/// in the agent, goal and floor scopes together is refused, on a hire and on the goal of an agent
/// hired before the variable appeared. Claude under the same daemon is the control: its variables
/// are absent.
#[test]
fn codex_and_pi_need_a_token_ceiling_under_their_billing_variable() {
    let dollars = dudes_proto::AgentBudget {
        usd_per_day: Some(Usd::from_dollars(10)),
        tokens_per_day: None,
    };
    let tokens = dudes_proto::AgentBudget {
        usd_per_day: None,
        tokens_per_day: Some(Tokens::new(60_000)),
    };
    let hired =
        |daemon: &Daemon| daemon.count(|record| matches!(record, Record::AgentHired { .. }));
    let goals = |daemon: &Daemon| daemon.count(|record| matches!(record, Record::GoalSet { .. }));
    for (provider, variable) in [
        (Provider::Codex, "CODEX_API_KEY"),
        (Provider::Pi, "OPENAI_API_KEY"),
        (Provider::Pi, "META_API_KEY"),
        (Provider::Pi, "TYPESAFE_API_KEY"),
    ] {
        let named = format!("{provider} reports no dollars, so a token ceiling is required");
        let keyed = every_engine(Fixture::new(), env_with(variable, FIXTURE_KEY));
        let refusal = hire_of(&keyed, "Dollars", provider, dollars)
            .expect_err("a dollar ceiling never caps an engine that reports no dollars");
        assert_eq!(
            gate_of(&refusal),
            RefusalGate::CeilingRequired,
            "{provider}"
        );
        assert!(refusal.message.contains(&named), "{provider}: {refusal:?}");
        assert_eq!(
            hired(&keyed),
            0,
            "{provider}: the refused hire appends nothing"
        );
        hire_of(&keyed, "Tokens", provider, tokens).expect("an agent token ceiling counts");
        hire_of(&keyed, "Claude", Provider::Claude, no_ceiling())
            .expect("control: Claude's own variables are absent");
        assert_eq!(hired(&keyed), 2, "{provider}");
        let _ = keyed.stop();

        let first = every_engine(Fixture::new(), keyless());
        let uncapped =
            hire_of(&first, "Uncapped", provider, no_ceiling()).expect("no key, no ceiling needed");
        let (fixture, _) = first.stop();
        let daemon = every_engine(fixture, env_with(variable, FIXTURE_KEY));
        let mut goal = goal_without_a_ceiling(uncapped);
        goal.budget.usd = Some(Usd::from_dollars(5));
        let refusal = daemon
            .set_goal(goal.clone())
            .expect_err("a goal dollar ceiling never caps it either");
        assert_eq!(
            gate_of(&refusal),
            RefusalGate::CeilingRequired,
            "{provider}"
        );
        assert!(refusal.message.contains(&named), "{provider}: {refusal:?}");
        assert_eq!(
            goals(&daemon),
            0,
            "{provider}: the refused goal appends nothing"
        );
        goal.budget.tokens = Some(Tokens::new(60_000));
        daemon.set_goal(goal).expect("a goal token ceiling counts");
        assert_eq!(goals(&daemon), 1, "{provider}");
        let _ = daemon.stop();
    }
}
