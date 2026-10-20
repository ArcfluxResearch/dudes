//! The journal's authority: *replay of any prefix of the log yields a valid
//! projection*. A stateful generator produces record sequences the single writer could have
//! produced; a plain in-test model folded over the same records is the oracle; the properties
//! compare the redb projection against it at a random cut, after resumption, against a
//! from-scratch replay, and after a second replay.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use dudes_journal::{
    AgentRow, Fsync, GoalRow, JournalOptions, Projections, Reader, TurnRow, open, replay,
};
use dudes_proto::recovery::RecoveryWork;
use dudes_proto::recovery_state::history::{
    RecoveryWitness, RecoveryWitnessOwner, RecoveryWitnessRef,
};
use dudes_proto::{
    AgentBudget, ClosingPhase, Entry, Evidence, ExhaustedReason, ExitStatus, Goal, HoldReason,
    KillReason, LoopDecision, Predicate, Record, Spend, TurnOutcome,
};
use dudes_types::{
    AgentId, AgentStatus, BreakerRung, GoalId, GoalStatus, Lsn, Mode, ModelSelection, Provenance,
    Provider, SessionId, Timestamp, Tokens, TurnId, TurnStatus, Usage, Usd, UtcDay,
};
use proptest::prelude::*;

fn recovery_owner_for_generated_record(record: &Record) -> Option<RecoveryWitnessOwner> {
    use RecoveryWitnessOwner as Owner;
    match record {
        Record::AgentHired { id: agent, .. }
        | Record::AgentRetired { id: agent, .. }
        | Record::HumanAnswer { agent, .. }
        | Record::Attached { agent, .. }
        | Record::Detached { agent, .. }
        | Record::Pause { agent, .. }
        | Record::Resume { agent, .. }
        | Record::Halt { agent, .. }
        | Record::LoopDecided {
            agent,
            decision: LoopDecision::Done,
            ..
        } => Some(Owner::Agent(*agent)),
        Record::GoalSet { agent, goal, .. } => Some(Owner::Goal {
            agent: *agent,
            goal: goal.id,
        }),
        Record::GoalCleared { agent, goal }
        | Record::GoalReassigned {
            from: agent, goal, ..
        } => Some(Owner::Goal {
            agent: *agent,
            goal: *goal,
        }),
        Record::TurnIssued {
            agent, goal, turn, ..
        } => Some(Owner::Work(RecoveryWork {
            agent: *agent,
            goal: *goal,
            turn: *turn,
        })),
        Record::TurnSpawned { agent, turn, .. }
        | Record::SessionLearned { agent, turn, .. }
        | Record::TurnEnded { agent, turn, .. } => Some(Owner::Turn {
            agent: *agent,
            turn: *turn,
        }),
        Record::PredicateResult { goal, turn, .. } => Some(Owner::Predicate {
            goal: *goal,
            turn: *turn,
        }),
        Record::LoopDecided { .. }
        | Record::ClosingTime { .. }
        | Record::Checkpoint { .. }
        | Record::MemoryAppend { .. }
        | Record::HumanQuestion { .. }
        | Record::BreakerBeat { .. } => None,
        _ => panic!("generated record needs an explicit recovery witness expectation"),
    }
}

fn exported_recovery_references(export: &str) -> BTreeMap<Lsn, RecoveryWitnessRef> {
    let mut references = BTreeMap::new();
    for line in export
        .lines()
        .filter_map(|line| line.strip_prefix("recovery "))
    {
        let (_, json) = line.split_once(' ').unwrap();
        let cell: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(cell["kind"], "witness");
        let witness: RecoveryWitness = serde_json::from_value(cell["value"].clone()).unwrap();
        let reference = witness.reference();
        assert!(references.insert(reference.lsn, reference).is_none());
    }
    references
}

// ---- generator ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Op {
    Hire,
    Retire(usize),
    SetGoal(usize),
    ClearGoal(usize),
    Reassign(usize, usize),
    Issue(usize, bool),
    Spawn(usize, bool),
    Learn(usize),
    /// `k` picks how the turn ended: 0 = `Inferred`, no outcome; 1 = `Completed`;
    /// 2 = `Killed`; 3 = `Vanished`.
    End(usize, u8),
    AttachedEnd(usize),
    /// A `PredicateResult` on the agent's goal; the bool is whether it passed.
    Judge(usize, bool),
    Question(usize),
    Answer(usize),
    Attach(usize),
    Detach(usize),
    Pause(usize),
    Resume(usize),
    Halt(usize),
    Decide(usize, u8),
    Beat(usize, u8),
    Noise(u8),
}

fn op() -> impl Strategy<Value = Op> {
    let ix = 0usize..6;
    prop_oneof![
        2 => Just(Op::Hire),
        1 => ix.clone().prop_map(Op::Retire),
        3 => ix.clone().prop_map(Op::SetGoal),
        1 => ix.clone().prop_map(Op::ClearGoal),
        1 => (ix.clone(), ix.clone()).prop_map(|(a, b)| Op::Reassign(a, b)),
        5 => (ix.clone(), any::<bool>()).prop_map(|(a, s)| Op::Issue(a, s)),
        4 => (ix.clone(), any::<bool>()).prop_map(|(a, r)| Op::Spawn(a, r)),
        2 => ix.clone().prop_map(Op::Learn),
        5 => (ix.clone(), 0u8..4).prop_map(|(a, k)| Op::End(a, k)),
        1 => ix.clone().prop_map(Op::AttachedEnd),
        2 => (ix.clone(), any::<bool>()).prop_map(|(a, p)| Op::Judge(a, p)),
        2 => ix.clone().prop_map(Op::Question),
        2 => ix.clone().prop_map(Op::Answer),
        1 => ix.clone().prop_map(Op::Attach),
        1 => ix.clone().prop_map(Op::Detach),
        1 => ix.clone().prop_map(Op::Pause),
        1 => ix.clone().prop_map(Op::Resume),
        1 => ix.clone().prop_map(Op::Halt),
        3 => (ix.clone(), 0u8..4).prop_map(|(a, k)| Op::Decide(a, k)),
        1 => (ix.clone(), 0u8..4).prop_map(|(a, r)| Op::Beat(a, r)),
        1 => (0u8..3).prop_map(Op::Noise),
    ]
}

fn agent_id(n: u128) -> AgentId {
    AgentId::from_parts(1, n + 1)
}

fn goal(n: u128) -> Goal {
    Goal {
        completion_check: None,
        id: GoalId::from_parts(3, n + 1),
        text: format!("goal {n}"),
        predicate: Predicate::Command {
            argv: vec!["true".into()],
            timeout_s: 1,
        },
        budget: dudes_proto::GoalBudget {
            usd: Some(Usd::from_dollars(1)),
            tokens: None,
        },
        hop_cap: 3,
        baseline: None,
    }
}

/// Expand ops into the entries the writer would have produced, skipping ops the current
/// state does not admit. Uses the same [`Model`] as the oracle to know the state.
///
/// Entries, not records: the budget tables bucket by the UTC day of `Entry.at`, so the oracle
/// cannot fold a record without the stamp that goes with it. Each entry's stamp is
/// [`stamp`] of its position, computed once here — so slicing the result for a prefix test
/// leaves every stamp exactly where it was.
#[allow(clippy::too_many_lines)] // one arm per op; a table, not logic
fn expand(ops: &[Op]) -> Vec<Entry> {
    let mut model = Model::default();
    let mut out: Vec<Entry> = Vec::new();
    let mut next = (0u128, 0u128, 0u128); // agents, turns, goals minted
    let mut ends = 0u64; // ends generated, which is what varies their usage
    let mut push = |model: &mut Model, record: Record| {
        let entry = Entry {
            at: stamp(out.len()),
            record,
        };
        model.apply(&entry);
        out.push(entry);
    };
    for op in ops {
        let live: Vec<AgentId> = model.agents.keys().copied().collect();
        let pick = |i: usize| live.get(i % live.len().max(1)).copied();
        match *op {
            Op::Hire => {
                let id = agent_id(next.0);
                next.0 += 1;
                push(
                    &mut model,
                    Record::AgentHired {
                        character: None,
                        accent: None,
                        id,
                        name: format!("agent {}", next.0),
                        role: "engineer".into(),
                        department: "eng".into(),
                        provider: Provider::Claude,
                        cwd: "/work".into(),
                        budget: AgentBudget {
                            usd_per_day: Some(Usd::from_dollars(5)),
                            tokens_per_day: None,
                        },
                        sandbox: None,
                        isolation: None,
                        max_turns: None,
                        max_budget: None,
                        trusted: false,
                        workspace: None,
                        worktree: None,
                        mcp_servers: Vec::new(),
                        browser_onboarding: None,
                    },
                );
            }
            Op::Noise(k) => push(
                &mut model,
                match k {
                    0 => Record::ClosingTime {
                        phase: ClosingPhase::Announced,
                    },
                    1 => Record::Checkpoint {
                        projection_version: 1,
                        segment: 1,
                        offset: 0,
                    },
                    _ => Record::MemoryAppend {
                        agent: AgentId::NIL,
                        text: "noise".into(),
                        body: None,
                        provenance: None,
                    },
                },
            ),
            _ if live.is_empty() => {}
            // Retire is generated whether or not a turn is open: the supervisor does not
            // refuse `AgentRetired` mid-turn, and the projection pins what that leaves.
            Op::Retire(i) => push(
                &mut model,
                Record::AgentRetired {
                    id: pick(i).unwrap(),
                    reason: "done".into(),
                    browser_retire: None,
                },
            ),
            Op::SetGoal(i) => {
                let id = pick(i).unwrap();
                if model.agents[&id].goal.is_none() {
                    let g = goal(next.2);
                    next.2 += 1;
                    push(
                        &mut model,
                        Record::GoalSet {
                            agent: id,
                            goal: g,
                            browser_onboarding: None,
                            browser_goal: None,
                            goal_replacement: None,
                        },
                    );
                }
            }
            Op::ClearGoal(i) => {
                let id = pick(i).unwrap();
                let a = &model.agents[&id];
                if let (Some(g), None) = (a.goal, a.open) {
                    push(&mut model, Record::GoalCleared { agent: id, goal: g });
                }
            }
            Op::Reassign(i, j) => {
                let (from, to) = (pick(i).unwrap(), pick(j).unwrap());
                let (a, b) = (&model.agents[&from], &model.agents[&to]);
                if let Some(g) = a.goal
                    && from != to
                    && a.open.is_none()
                    && b.goal.is_none()
                {
                    push(
                        &mut model,
                        Record::GoalReassigned {
                            goal: g,
                            from,
                            to,
                            reason: None,
                        },
                    );
                }
            }
            Op::Issue(i, with_session) => {
                let id = pick(i).unwrap();
                let a = &model.agents[&id];
                if let (Some(g), None, Mode::Managed, false) = (a.goal, a.open, a.mode, a.question)
                {
                    let turn = TurnId::from_parts(2, next.1 + 1);
                    next.1 += 1;
                    push(
                        &mut model,
                        Record::LoopDecided {
                            agent: id,
                            input_digest: "00".repeat(32),
                            decision: LoopDecision::Continue {
                                task: "work".into(),
                            },
                            gate_trace: vec![],
                        },
                    );
                    push(
                        &mut model,
                        Record::TurnIssued {
                            agent: id,
                            turn,
                            goal: g,
                            task: "work".into(),
                            session: with_session.then(|| SessionId::new("s-issued")),
                            mode: Mode::Managed,
                            model_selection: selection(next.1),
                            lsn_of_decision: Lsn::ZERO,
                            answers: Vec::new(),
                        },
                    );
                }
            }
            Op::Spawn(i, reissued) => {
                let id = pick(i).unwrap();
                if let Some(turn) = model.agents[&id].open
                    && model.turns[&turn].status == TurnStatus::Issued
                {
                    push(
                        &mut model,
                        Record::TurnSpawned {
                            agent: id,
                            turn,
                            pid: 100 + u32::try_from(next.1).unwrap(),
                            start_time: 7,
                            boot: None,
                            session: reissued.then(|| SessionId::new("s-spawned")),
                            reissued,
                            mcp_grant: None,
                        },
                    );
                }
            }
            Op::Learn(i) => {
                let id = pick(i).unwrap();
                if let Some(turn) = model.agents[&id].open {
                    push(
                        &mut model,
                        Record::SessionLearned {
                            agent: id,
                            turn,
                            session: SessionId::new("s-learned"),
                        },
                    );
                }
            }
            Op::End(i, k) => {
                let id = pick(i).unwrap();
                if let Some(turn) = model.agents[&id].open {
                    push(&mut model, ended(id, turn, Mode::Managed, k, ends));
                    ends += 1;
                }
            }
            Op::AttachedEnd(i) => {
                let id = pick(i).unwrap();
                if model.agents[&id].mode == Mode::Attached {
                    let turn = TurnId::from_parts(2, next.1 + 1);
                    next.1 += 1;
                    push(&mut model, ended(id, turn, Mode::Attached, 1, ends));
                    ends += 1;
                }
            }
            Op::Judge(i, pass) => {
                let id = pick(i).unwrap();
                if let Some(g) = model.agents[&id].goal {
                    push(
                        &mut model,
                        Record::PredicateResult {
                            goal: g,
                            turn: None,
                            provenance: Provenance::Observed,
                            pass,
                            evidence: Evidence::Command {
                                argv: vec!["true".into()],
                                program: Some("/usr/bin/true".into()),
                                exit: Some(i32::from(!pass)),
                                output_tail: String::new(),
                            },
                        },
                    );
                }
            }
            Op::Question(i) => {
                let id = pick(i).unwrap();
                push(
                    &mut model,
                    Record::HumanQuestion {
                        agent: id,
                        text: "which?".into(),
                    },
                );
            }
            Op::Answer(i) => {
                let id = pick(i).unwrap();
                if model.agents[&id].question {
                    push(
                        &mut model,
                        Record::HumanAnswer {
                            agent: id,
                            text: "that".into(),
                            provenance: None,
                            browser_answer: None,
                        },
                    );
                }
            }
            Op::Attach(i) => {
                let id = pick(i).unwrap();
                let a = &model.agents[&id];
                if a.open.is_none() && a.mode == Mode::Managed {
                    push(
                        &mut model,
                        Record::Attached {
                            agent: id,
                            by: dudes_proto::Actor::System,
                            hook_port: None,
                            provenance: None,
                        },
                    );
                }
            }
            Op::Detach(i) => {
                let id = pick(i).unwrap();
                if model.agents[&id].mode == Mode::Attached {
                    push(
                        &mut model,
                        Record::Detached {
                            agent: id,
                            by: dudes_proto::Actor::System,
                            provenance: None,
                        },
                    );
                }
            }
            Op::Pause(i) => push(
                &mut model,
                Record::Pause {
                    agent: pick(i).unwrap(),
                    browser: None,
                },
            ),
            Op::Resume(i) => push(
                &mut model,
                Record::Resume {
                    agent: pick(i).unwrap(),
                    browser: None,
                },
            ),
            Op::Halt(i) => push(
                &mut model,
                Record::Halt {
                    agent: pick(i).unwrap(),
                    text: None,
                },
            ),
            Op::Decide(i, k) => {
                let id = pick(i).unwrap();
                let a = &model.agents[&id];
                if a.open.is_none() && a.mode == Mode::Managed && !a.question {
                    let decision = match k {
                        0 => LoopDecision::Wait {
                            question: "q".into(),
                        },
                        1 => LoopDecision::Done,
                        2 => LoopDecision::Exhausted {
                            reason: ExhaustedReason::Hops,
                        },
                        _ => LoopDecision::Hold {
                            reason: HoldReason::Paused,
                        },
                    };
                    push(
                        &mut model,
                        Record::LoopDecided {
                            agent: id,
                            input_digest: "00".repeat(32),
                            decision,
                            gate_trace: vec![],
                        },
                    );
                }
            }
            Op::Beat(i, r) => {
                let id = pick(i).unwrap();
                let rung_after = [
                    BreakerRung::Healthy,
                    BreakerRung::Steer,
                    BreakerRung::Constrain,
                    BreakerRung::Stop,
                ][usize::from(r)];
                let rung_before = model.agents[&id].breaker;
                push(
                    &mut model,
                    Record::BreakerBeat {
                        agent: id,
                        rung_before,
                        rung_after,
                        signals: vec![],
                    },
                );
            }
        }
    }
    out
}

/// The model selection the `n`th generated issue records, cycling all three wire states
/// so the properties below are not green over one of them.
///
/// A pure function of `n`, like [`ended`]'s usage and for the same reason: both halves of a prefix
/// cut regenerate the identical record.
fn selection(n: u128) -> Option<ModelSelection> {
    match n % 3 {
        // A journal written before the field: the case a fold must not confuse with the next one.
        0 => None,
        // Explicitly the provider default.
        1 => Some(ModelSelection::provider_default()),
        // A raw provider spelling, varying with `n` so a fold that kept the *first* selection it
        // saw, or a constant, is not equal to the oracle.
        _ => Some(ModelSelection::named(format!("model-{n}"))),
    }
}

/// The observed per-model breakdown the `n`th generated end records.
///
/// **Zero, one and two keys all occur**, and the token counts differ between the two keys of the
/// same end. That is what makes the replay property a claim about the *map*: a fold that kept only
/// the first key, or that copied one key's counts onto the other, is unequal to the oracle. A
/// one-key generator could not see either mutation.
///
/// `k == 3` is `Vanished` — the process that would have reported a breakdown is gone — so it
/// reports none, exactly as it reports no usage.
fn breakdown(k: u8, n: u64) -> BTreeMap<String, Usage> {
    if k == 3 {
        return BTreeMap::new();
    }
    let one = Usage {
        input: Tokens::new(n * 2 + 3),
        output: Tokens::new(n + 5),
        cache_read: Tokens::ZERO,
        cache_creation: Tokens::ZERO,
        usd: Some(Usd::from_micros(n * 7 + 11)),
    };
    let mut map = BTreeMap::from([("main-model".to_owned(), one)]);
    if n.is_multiple_of(2) {
        // A subagent's own key, with its own counts and no cost at all: the arm that says a
        // missing per-key cost stays missing rather than taking a share of anything.
        map.insert(
            "subagent-model".to_owned(),
            Usage {
                input: Tokens::new(n + 1),
                output: Tokens::new(n * 3 + 2),
                cache_read: Tokens::new(n % 5),
                cache_creation: Tokens::ZERO,
                usd: None,
            },
        );
    }
    map
}

/// `k` as [`Op::End`] defines it, and `n` the number of ends generated so far. A `Killed` end is
/// generated so `TurnStatus::Killed` inherits the four properties below — prefix validity,
/// live-equals-replay, idempotence and rotation — rather than needing four tests of its own.
///
/// **`n` is what stops the budget properties being vacuous.** Were every generated end to carry
/// `usage: None`, `budget_by_goal` and `budget_by_day` could never hold a row and the prefix
/// property would have been green over three permanently empty tables. The usage varies with
/// `n` and with `k`, and is a pure function of both, so the two halves of a prefix cut still
/// agree on every record:
///
/// * `k == 3` (`Vanished`) reports nothing — the process that would have reported it is gone.
///   That is the `charge_unknown_usage` path, and it must stay charge-free under every property.
/// * `k == 0` (an `Inferred` end) reports tokens but no dollars, the case `Usage.usd`'s own doc
///   names: both token columns charge and the dollar column does not.
/// * everything else reports both.
fn ended(agent: AgentId, turn: TurnId, mode: Mode, k: u8, n: u64) -> Record {
    let observed = k != 0;
    let outcome = match k {
        0 => None,
        1 => Some(TurnOutcome::Completed),
        2 => Some(TurnOutcome::Killed {
            by: KillReason::Halt,
        }),
        _ => Some(TurnOutcome::Vanished),
    };
    let counted = Usage {
        input: Tokens::new(n * 10 + 1),
        output: Tokens::new(n * 3 + 2),
        cache_read: Tokens::new(n % 7),
        cache_creation: Tokens::ZERO,
        usd: Some(Usd::from_micros(n * 1_000 + 17)),
    };
    let usage = match k {
        3 => None,
        0 => Some(Usage {
            usd: None,
            ..counted
        }),
        _ => Some(counted),
    };
    Record::TurnEnded {
        final_reply: None,
        elapsed_ms: None,
        session_usage: None,
        agent,
        turn,
        mode,
        provenance: if observed {
            Provenance::Observed
        } else {
            Provenance::Inferred
        },
        exit: ExitStatus {
            code: observed.then_some(0),
            signal: None,
        },
        usage,
        model_usage: breakdown(k, n),
        last_message: None,
        session: observed.then(|| SessionId::new("s-ended")),
        outcome,
    }
}

// ---- oracle ------------------------------------------------------------------------------

/// The last terminal status for an agent, including settlement of an achieved goal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Done,
    Exhausted,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
struct ModelAgent {
    goal: Option<GoalId>,
    open: Option<TurnId>,
    turns: u64,
    mode: Mode,
    question: bool,
    paused: bool,
    halted: bool,
    breaker: BreakerRung,
    session: Option<SessionId>,
    last_flow: Flow,
}

#[derive(Clone, Debug, PartialEq)]
struct ModelTurn {
    agent: AgentId,
    goal: Option<GoalId>,
    mode: Mode,
    status: TurnStatus,
    pid: Option<u32>,
    reissued: bool,
    session: Option<SessionId>,
    session_confirmed: bool,
    provenance: Option<Provenance>,
    /// Stated independently of the fold: the selection is whatever `TurnIssued` carried and is
    /// never touched again, so an attached turn — which has no `TurnIssued` — keeps `None` for
    /// life.
    model_selection: Option<ModelSelection>,
    /// Stated independently of the fold: the breakdown is whatever `TurnEnded` carried, in full,
    /// and nothing before the end contributes to it.
    model_usage: BTreeMap<String, Usage>,
}

#[derive(Clone, Debug, PartialEq)]
struct ModelGoal {
    agent: AgentId,
    text: String,
    hop_cap: u32,
    status: GoalStatus,
    turns_on_goal: u32,
    judged: Option<(bool, Provenance)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Model {
    agents: BTreeMap<AgentId, ModelAgent>,
    turns: BTreeMap<TurnId, ModelTurn>,
    goals: BTreeMap<GoalId, ModelGoal>,
    by_goal: BTreeMap<GoalId, ModelBudget>,
    by_day: BTreeMap<(AgentId, UtcDay), ModelBudget>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ModelBudget {
    spend: Spend,
    unknown_ends: u32,
}

/// The charge computation, stated here independently of the projection's: the cost as the provider
/// reported it, or nothing where it reported none; every token counted once in `tokens`; and
/// `work_tokens` the same count without prompt-cache reads.
fn spend_of(usage: Usage) -> Spend {
    Spend {
        usd: usage.usd.unwrap_or(Usd::ZERO),
        tokens: usage.total_tokens(),
        work_tokens: usage
            .input
            .saturating_add(usage.output)
            .saturating_add(usage.cache_creation),
    }
}

fn add(a: Spend, b: Spend) -> Spend {
    Spend {
        usd: a.usd.saturating_add(b.usd),
        tokens: a.tokens.saturating_add(b.tokens),
        work_tokens: a.work_tokens.saturating_add(b.work_tokens),
    }
}

fn qualifying_unconfirmed_resume_end(outcome: Option<&TurnOutcome>) -> bool {
    matches!(
        outcome,
        Some(
            TurnOutcome::Completed
                | TurnOutcome::MaxTurns
                | TurnOutcome::BudgetExhausted
                | TurnOutcome::Failed { .. }
                | TurnOutcome::NoResult
        )
    )
}

impl Model {
    fn fold(entries: &[Entry]) -> Self {
        let mut model = Self::default();
        for entry in entries {
            model.apply(entry);
        }
        model
    }

    fn goal(&mut self, id: GoalId, f: impl FnOnce(&mut ModelGoal)) {
        if let Some(g) = self.goals.get_mut(&id) {
            f(g);
        }
    }

    /// Charge one end, by the rules the projection states: an unpriced end writes a count; the goal
    /// is the turn's, or the agent's current one for an attached turn whose row has none; the
    /// day is the entry's; and the floor row is `AgentId::NIL`, charged once even when the end
    /// itself names `NIL`.
    fn charge(&mut self, at: Timestamp, agent: AgentId, turn: TurnId, usage: Option<Usage>) {
        let (charge, unknown_ends) =
            usage.map_or((Spend::default(), 1), |usage| (spend_of(usage), 0));
        if charge == Spend::default() && unknown_ends == 0 {
            return;
        }
        let goal = self
            .turns
            .get(&turn)
            .and_then(|t| t.goal)
            .or_else(|| self.agents.get(&agent).and_then(|a: &ModelAgent| a.goal));
        if let Some(goal) = goal {
            let row = self.by_goal.entry(goal).or_default();
            row.spend = add(row.spend, charge);
            row.unknown_ends = row.unknown_ends.saturating_add(unknown_ends);
        }
        let day = at.utc_day();
        let mut scopes = BTreeSet::new();
        scopes.insert(agent);
        scopes.insert(AgentId::NIL);
        for scope in scopes {
            let row = self.by_day.entry((scope, day)).or_default();
            row.spend = add(row.spend, charge);
            row.unknown_ends = row.unknown_ends.saturating_add(unknown_ends);
        }
    }

    fn agent(&mut self, id: AgentId, flow: bool, f: impl FnOnce(&mut ModelAgent)) {
        let completed = self
            .agents
            .get(&id)
            .and_then(|agent| agent.goal)
            .is_some_and(|goal| self.goals[&goal].status == GoalStatus::Achieved);
        if let Some(a) = self.agents.get_mut(&id) {
            f(a);
            if flow {
                a.last_flow =
                    if completed && a.mode == Mode::Managed && !a.question && a.open.is_none() {
                        Flow::Done
                    } else {
                        Flow::Other
                    };
            }
        }
    }

    #[allow(clippy::too_many_lines)] // one arm per record kind the projection reads
    fn apply(&mut self, entry: &Entry) {
        let record = &entry.record;
        match record {
            Record::AgentHired { id, .. } => {
                self.agents.insert(
                    *id,
                    ModelAgent {
                        goal: None,
                        open: None,
                        turns: 0,
                        mode: Mode::Managed,
                        question: false,
                        paused: false,
                        halted: false,
                        breaker: BreakerRung::Healthy,
                        session: None,
                        last_flow: Flow::Other,
                    },
                );
            }
            Record::AgentRetired { id, .. } => {
                self.agents.remove(id);
            }
            Record::GoalSet { agent, goal, .. } => {
                self.agent(*agent, false, |a| a.goal = Some(goal.id));
                self.goals.insert(
                    goal.id,
                    ModelGoal {
                        agent: *agent,
                        text: goal.text.clone(),
                        hop_cap: goal.hop_cap,
                        status: GoalStatus::Set,
                        turns_on_goal: 0,
                        judged: None,
                    },
                );
            }
            Record::GoalCleared { agent, goal } => {
                self.agent(*agent, false, |a| a.goal = None);
                // The row stays, marked abandoned; only its status moves.
                self.goal(*goal, |g| g.status = GoalStatus::Abandoned);
            }
            Record::GoalReassigned { goal, from, to, .. } => {
                self.agent(*from, false, |a| a.goal = None);
                self.agent(*to, false, |a| a.goal = Some(*goal));
                // Owner only: the hop count is goal-scoped and the spend is keyed by the goal.
                self.goal(*goal, |g| g.agent = *to);
            }
            Record::PredicateResult {
                goal,
                provenance,
                pass,
                ..
            } => self.goal(*goal, |g| g.judged = Some((*pass, *provenance))),
            Record::TurnIssued {
                agent,
                turn,
                goal,
                session,
                model_selection,
                ..
            } => {
                // Gate 6 counts managed issues, which is every issue the writer emits.
                self.goal(*goal, |g| {
                    g.turns_on_goal += 1;
                    if g.status == GoalStatus::Set {
                        g.status = GoalStatus::InProgress;
                    }
                });
                self.turns.insert(
                    *turn,
                    ModelTurn {
                        agent: *agent,
                        goal: Some(*goal),
                        mode: Mode::Managed,
                        status: TurnStatus::Issued,
                        pid: None,
                        reissued: false,
                        session: session.clone(),
                        session_confirmed: false,
                        provenance: None,
                        model_selection: model_selection.clone(),
                        model_usage: BTreeMap::new(),
                    },
                );
                // The announced id moves the *turn* row above and no further: only
                // `SessionLearned` writes an agent's session. This model states the rule *separately* from the fold and the two
                // must agree — which is a differential check, not an independent one: the model
                // is written by hand against the same reading, so a rule mistaken in both places
                // is invisible here. What this catches is divergence,
                // in either direction.
                self.agent(*agent, true, |a| {
                    a.open = Some(*turn);
                    a.turns += 1;
                });
            }
            Record::TurnSpawned {
                agent,
                turn,
                pid,
                session,
                reissued,
                ..
            } => {
                if let Some(t) = self.turns.get_mut(turn) {
                    t.status = TurnStatus::Spawned;
                    t.pid = Some(*pid);
                    t.reissued = *reissued;
                    if session.is_some() {
                        t.session.clone_from(session);
                    }
                    if *reissued {
                        t.session_confirmed = false;
                    }
                }
                if *reissued {
                    self.agent(*agent, false, |a| a.session = None);
                }
            }
            Record::SessionLearned {
                agent,
                turn,
                session,
            } => {
                if let Some(t) = self.turns.get_mut(turn) {
                    t.session = Some(session.clone());
                    t.session_confirmed = true;
                }
                self.agent(*agent, false, |a| a.session = Some(session.clone()));
            }
            Record::TurnEnded {
                agent,
                turn,
                mode,
                provenance,
                session,
                outcome,
                usage,
                model_usage,
                ..
            } => {
                let invalidates_resume = *mode == Mode::Managed
                    && qualifying_unconfirmed_resume_end(outcome.as_ref())
                    && self.turns.get(turn).is_some_and(|turn| {
                        !turn.session_confirmed
                            && turn.session.as_ref().is_some_and(|announced| {
                                self.agents
                                    .get(agent)
                                    .and_then(|agent| agent.session.as_ref())
                                    == Some(announced)
                            })
                    });
                let t = self.turns.entry(*turn).or_insert(ModelTurn {
                    agent: *agent,
                    goal: None,
                    mode: *mode,
                    status: TurnStatus::Issued,
                    pid: None,
                    reissued: false,
                    session: None,
                    session_confirmed: false,
                    provenance: None,
                    // An attached turn is minted by its end and never had a `TurnIssued` to
                    // record a selection on.
                    model_selection: None,
                    model_usage: BTreeMap::new(),
                });
                // The same computation the projection applies, stated independently.
                t.status = if matches!(outcome, Some(TurnOutcome::Killed { .. })) {
                    TurnStatus::Killed
                } else {
                    TurnStatus::Ended
                };
                t.provenance = Some(*provenance);
                // The whole map, every key, whatever the end reported — including empty, which is
                // the same claim as a record written before the field.
                t.model_usage.clone_from(model_usage);
                if session.is_some() {
                    t.session.clone_from(session);
                }
                // Repeating an id on the end is not confirmation. A qualifying managed resume
                // without this turn's `SessionLearned` clears the agent session; other ends leave
                // it untouched.
                self.agent(*agent, true, |a| {
                    if *mode == Mode::Managed {
                        a.open = None;
                    }
                    if invalidates_resume {
                        a.session = None;
                    }
                });
                self.charge(entry.at, *agent, *turn, *usage);
            }
            Record::LoopDecided {
                agent, decision, ..
            } => {
                // `Continue` and `Hold` leave the status alone (Hold issues nothing;
                // Continue is followed by the `TurnIssued` that moves it).
                let flow = match decision {
                    LoopDecision::Done => Some(Flow::Done),
                    LoopDecision::Exhausted { .. } => Some(Flow::Exhausted),
                    LoopDecision::Wait { .. } => Some(Flow::Other),
                    LoopDecision::Continue { .. } | LoopDecision::Hold { .. } => None,
                };
                self.agent(*agent, false, |a| {
                    if let LoopDecision::Wait { .. } = decision {
                        a.question = true;
                    }
                    if let Some(flow) = flow {
                        a.last_flow = flow;
                    }
                });
                // `Achieved` follows the loop's verdict, on whatever goal the agent holds:
                // `LoopDecided` names an agent and no goal.
                if matches!(decision, LoopDecision::Done)
                    && let Some(goal) = self.agents.get(agent).and_then(|a| a.goal)
                {
                    self.goal(goal, |g| g.status = GoalStatus::Achieved);
                }
            }
            Record::HumanQuestion { agent, .. } => self.agent(*agent, true, |a| a.question = true),
            Record::HumanAnswer { agent, .. } => self.agent(*agent, true, |a| a.question = false),
            Record::Attached { agent, .. } => self.agent(*agent, true, |a| a.mode = Mode::Attached),
            Record::Detached { agent, .. } => self.agent(*agent, true, |a| a.mode = Mode::Managed),
            Record::Pause { agent, .. } => self.agent(*agent, false, |a| a.paused = true),
            // Stated independently, as the rest of this oracle is: `Resume` is the only record
            // that lifts a standing cause and it lifts both, which is what the supervisor's own
            // fold does and what `Then::ClearedCause` means. Written as
            // `paused` alone this line reproduced the projection's latch instead of checking it.
            Record::Resume { agent, .. } => self.agent(*agent, false, |a| {
                a.halted = false;
                a.paused = false;
            }),
            Record::Halt { agent, .. } => self.agent(*agent, false, |a| a.halted = true),
            Record::BreakerBeat {
                agent, rung_after, ..
            } => self.agent(*agent, false, |a| a.breaker = *rung_after),
            _ => {}
        }
    }
}

/// Every validity predicate, against the oracle for `entries` and the lsns they got.
#[allow(clippy::too_many_lines)] // one assertion per predicate; a checklist, not logic
fn check(p: &Projections, entries: &[Entry], lsns: &[Lsn]) -> Result<(), TestCaseError> {
    let model = Model::fold(entries);
    prop_assert_eq!(
        p.last_applied_lsn(),
        lsns.last().copied().unwrap_or(Lsn::ZERO)
    );

    let agents = p.agents()?;
    prop_assert_eq!(
        agents.iter().map(|a| a.id).collect::<Vec<_>>(),
        model.agents.keys().copied().collect::<Vec<_>>(),
        "exactly the hired-and-not-retired agents"
    );
    for row in &agents {
        let m = &model.agents[&row.id];
        prop_assert_eq!(row.goal, m.goal, "{}", row.id);
        prop_assert_eq!(row.open_turn, m.open, "{}", row.id);
        prop_assert_eq!(row.turns, m.turns, "{}", row.id);
        prop_assert_eq!(row.mode, m.mode, "{}", row.id);
        prop_assert_eq!(row.question.is_some(), m.question, "{}", row.id);
        prop_assert_eq!((row.paused, row.halted), (m.paused, m.halted), "{}", row.id);
        prop_assert_eq!(row.breaker, m.breaker, "{}", row.id);
        prop_assert_eq!(&row.session, &m.session, "{}", row.id);
        // The status transitions, as consequences rather than as a copy of the projection.
        let s = row.status;
        prop_assert_eq!(
            s == AgentStatus::Done,
            m.last_flow == Flow::Done,
            "{:?}",
            row
        );
        prop_assert_eq!(
            s == AgentStatus::Exhausted,
            m.last_flow == Flow::Exhausted,
            "{:?}",
            row
        );
        if s == AgentStatus::Running {
            prop_assert!(
                m.mode == Mode::Managed && !m.question && m.open.is_some(),
                "{:?}",
                row
            );
        }
        if s == AgentStatus::Idle {
            prop_assert!(
                m.mode == Mode::Managed && !m.question && m.open.is_none(),
                "{:?}",
                row
            );
        }
        if s == AgentStatus::Waiting {
            prop_assert!(m.question, "{:?}", row);
        }
        if s == AgentStatus::Attached || m.mode == Mode::Attached {
            prop_assert!(
                m.mode == Mode::Attached
                    && matches!(s, AgentStatus::Attached | AgentStatus::Waiting),
                "{:?}",
                row
            );
        }
        if m.question {
            prop_assert!(
                matches!(s, AgentStatus::Waiting | AgentStatus::Attached),
                "{:?}",
                row
            );
        }
        if let Some(open) = m.open {
            prop_assert!(
                matches!(s, AgentStatus::Running | AgentStatus::Waiting),
                "{:?}",
                row
            );
            prop_assert!(p.turn(open)?.is_some_and(|t| t.is_open()), "{:?}", row);
        }
    }

    let turns = p.turns()?;
    prop_assert_eq!(
        turns.iter().map(|t| t.turn).collect::<Vec<_>>(),
        model.turns.keys().copied().collect::<Vec<_>>()
    );
    let max_lsn = lsns.last().copied().unwrap_or(Lsn::ZERO);
    for row in &turns {
        let m = &model.turns[&row.turn];
        prop_assert_eq!(
            (
                row.agent,
                row.goal,
                row.mode,
                row.status,
                row.pid,
                row.reissued,
                &row.session,
                row.session_confirmed,
                row.provenance
            ),
            (
                m.agent,
                m.goal,
                m.mode,
                m.status,
                m.pid,
                m.reissued,
                &m.session,
                m.session_confirmed,
                m.provenance
            ),
            "{:?}",
            row
        );
        // The model-selection checks, as **three** assertions rather than one, because a single
        // "the model survived" check is satisfied by any non-empty map and by any selection.
        //
        // 1. The selection replays exactly, all three wire states: a fold that dropped it, or one
        //    that flattened absent into provider-default, is unequal here on the generated cycle.
        prop_assert_eq!(
            &row.model_selection,
            &m.model_selection,
            "the recorded model selection must survive replay: {:?}",
            row
        );
        // 2. Every key survives, so a fold that kept only the first key of a two-key end is
        //    unequal — which the one-key case alone could never show.
        prop_assert_eq!(
            row.model_usage.keys().collect::<Vec<_>>(),
            m.model_usage.keys().collect::<Vec<_>>(),
            "every observed model key must survive replay: {:?}",
            row
        );
        // 3. And each key's own counts survive, so altering one token count inside one key is
        //    unequal — which the key set alone could never show.
        prop_assert_eq!(
            &row.model_usage,
            &m.model_usage,
            "each observed model's own usage must survive replay: {:?}",
            row
        );
        prop_assert!(
            row.lsn_issued > Lsn::ZERO && row.lsn_issued <= max_lsn,
            "{:?}",
            row
        );
        // An open turn's owner, if still hired, points back at it; an agent retired mid-turn
        // leaves the row open and ownerless.
        if row.is_open()
            && let Some(owner) = agents.iter().find(|a| a.id == row.agent)
        {
            prop_assert_eq!(owner.open_turn, Some(row.turn), "{:?}", row);
        }
    }
    // `open_turns()` is the startup pass's worklist: open *and* owned by a hired agent.
    let open: Vec<TurnId> = p.open_turns()?.into_iter().map(|t| t.turn).collect();
    let expected_open: Vec<TurnId> = model
        .turns
        .iter()
        .filter(|(_, t)| {
            matches!(t.status, TurnStatus::Issued | TurnStatus::Spawned)
                && model.agents.contains_key(&t.agent)
        })
        .map(|(id, _)| *id)
        .collect();
    prop_assert_eq!(open, expected_open);

    // `goals`: every goal ever set, never removed — `GoalCleared` marks it abandoned.
    let goals = p.goals()?;
    prop_assert_eq!(
        goals.iter().map(|g| g.id).collect::<Vec<_>>(),
        model.goals.keys().copied().collect::<Vec<_>>()
    );
    for row in &goals {
        let m = &model.goals[&row.id];
        prop_assert_eq!(
            (
                row.agent,
                row.status,
                row.turns_on_goal,
                &row.text,
                row.hop_cap
            ),
            (m.agent, m.status, m.turns_on_goal, &m.text, m.hop_cap),
            "{:?}",
            row
        );
        prop_assert_eq!(
            row.last_result.map(|r| (r.pass, r.provenance)),
            m.judged,
            "{:?}",
            row
        );
    }

    // `budget_by_goal` and `budget_by_day`, whole: same keys, same sums, same order.
    prop_assert_eq!(
        p.budget_by_goal_rows()?,
        model
            .by_goal
            .iter()
            .map(|(k, v)| (*k, v.spend))
            .collect::<Vec<_>>()
    );
    prop_assert_eq!(
        p.budget_by_day_rows()?,
        model
            .by_day
            .iter()
            .map(|(k, v)| (*k, v.spend))
            .collect::<Vec<_>>()
    );
    // Every day row is reachable through the reader gate 5 uses, and the floor row of a day is
    // the sum of that day's agent rows — the property that makes `floor_today` a floor.
    let days: BTreeSet<UtcDay> = model.by_day.keys().map(|(_, d)| *d).collect();
    for day in days {
        let mut summed = Spend::default();
        let floor = p.spend_views(AgentId::NIL, GoalId::NIL, day)?;
        let mut summed_unknown = 0_u32;
        for ((agent, d), budget) in &model.by_day {
            if *d == day && *agent != AgentId::NIL {
                summed = add(summed, budget.spend);
                summed_unknown = summed_unknown.saturating_add(budget.unknown_ends);
                prop_assert_eq!(p.spend_by_day(*agent, day)?, budget.spend);
                prop_assert_eq!(
                    p.spend_views(*agent, GoalId::NIL, day)?
                        .agent_unknown_ends_today,
                    budget.unknown_ends
                );
            }
        }
        prop_assert_eq!(p.floor_spend_on(day)?, summed, "{:?}", day);
        prop_assert_eq!(floor.floor_unknown_ends_today, summed_unknown);
    }
    for (goal, budget) in &model.by_goal {
        prop_assert_eq!(p.spend_by_goal(*goal)?, budget.spend);
        prop_assert_eq!(
            p.spend_views(AgentId::NIL, *goal, UtcDay::from_days_since_epoch(0))?
                .goal_unknown_ends,
            budget.unknown_ends
        );
    }
    Ok(())
}

// ---- harness -----------------------------------------------------------------------------

fn options() -> JournalOptions {
    JournalOptions {
        fsync: Fsync::None,
        rotate_at: 4096,
        checkpoint_every: 0,
    }
}

/// A stamp far enough from the epoch to have the width a real one has.
const AT_BASE: i64 = 1_787_394_600_000;

/// Seven hours between consecutive entries.
///
/// A one-millisecond stride would make the stamps distinct and nothing else: every entry of an
/// 80-op sequence would land in one UTC day, so `budget_by_day` could hold at most one row per
/// agent and would never be asked to split. Seven hours puts any five consecutive entries across
/// at least one UTC midnight (four gaps is 28 h) while spanning about 23 days over a full
/// sequence, so the property meets both the split and the same-day sum. `check` never compares
/// `ended_at` or `last_turn_ended_at`.
const STRIDE_MS: i64 = 25_200_000;

/// The `at` of the entry at position `i`: a pure function of the position, so the two halves of
/// a prefix test agree on every stamp however the vector is sliced (`lsn` orders,
/// `at` labels — and now also buckets, which is why the stride matters).
fn stamp(i: usize) -> Timestamp {
    Timestamp::from_unix_millis(AT_BASE + i64::try_from(i).unwrap() * STRIDE_MS)
}

fn write(dir: &Path, entries: &[Entry]) -> Vec<Lsn> {
    let (mut w, _) = open(dir, options()).unwrap();
    entries.iter().map(|e| w.append(e).unwrap()).collect()
}

/// Every table, for the properties that compare two stores rather than a store and the oracle.
type Snapshot = (
    Vec<AgentRow>,
    Vec<TurnRow>,
    Vec<GoalRow>,
    Vec<(GoalId, Spend)>,
    Vec<((AgentId, UtcDay), Spend)>,
);

fn rows(p: &Projections) -> Snapshot {
    (
        p.agents().unwrap(),
        p.turns().unwrap(),
        p.goals().unwrap(),
        p.budget_by_goal_rows().unwrap(),
        p.budget_by_day_rows().unwrap(),
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// Prefix-valid and resumable: replaying `records[..k]` yields a
    /// projection the oracle accepts, and replaying the rest afterwards yields the full one.
    #[test]
    fn any_prefix_replays_to_a_valid_projection(ops in prop::collection::vec(op(), 0..80), cut in any::<prop::sample::Index>()) {
        let entries = expand(&ops);
        let k = cut.index(entries.len() + 1);
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let mut lsns = write(&journal, &entries[..k]);
        let mut p = Projections::open_with(&dir.path().join("state.redb"), Fsync::None).unwrap();
        let report = replay(&Reader::new(&journal), &mut p).unwrap();
        prop_assert_eq!(report.applied, k as u64);
        check(&p, &entries[..k], &lsns)?;

        lsns.extend(write(&journal, &entries[k..]));
        let report = replay(&Reader::new(&journal), &mut p).unwrap();
        prop_assert_eq!(report.applied, (entries.len() - k) as u64);
        check(&p, &entries, &lsns)?;
    }

    /// Same code live and on replay: applying each record as it is appended
    /// equals a from-scratch replay of the whole log, table by table. The two stores are
    /// in-memory: the property is about `apply`, not about the file.
    ///
    /// The witness is part of that equality. The live store's
    /// `last_applied_crc` comes from the writer, the replayed one's from the reader decoding
    /// the frame the writer produced; they agree, or a daemon that appended after a replay
    /// would leave behind a store its own next replay refuses.
    #[test]
    fn live_apply_equals_replay(ops in prop::collection::vec(op(), 0..20)) {
        let entries = expand(&ops);
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let (mut w, _) = open(&journal, options()).unwrap();
        let mut live = Projections::in_memory().unwrap();
        for entry in &entries {
            let (lsn, crc) = w.append_witnessed(entry).unwrap();
            prop_assert!(live.apply(lsn, crc, entry).unwrap());
        }
        let mut scratch = Projections::in_memory().unwrap();
        replay(&Reader::new(&journal), &mut scratch).unwrap();
        prop_assert_eq!(rows(&live), rows(&scratch));
        prop_assert_eq!(live.last_applied_lsn(), scratch.last_applied_lsn());
        prop_assert_eq!(live.last_applied_crc(), scratch.last_applied_crc());
        // The control: for a non-empty log the two agree on a witness that *exists*, so the
        // equality above is not passing on two `None`s.
        prop_assert_eq!(live.last_applied_crc().is_some(), !entries.is_empty());
    }

    /// **Identity of the export, over the same property.** The *rendering* that restart-versus-replay comparisons use —
    /// `Projections::export` — agrees between a live fold and a from-scratch replay of the same
    /// log, not merely the tuples `live_apply_equals_replay` compares.
    ///
    /// The two are not the same claim. That test compares typed rows in memory, so it pins the
    /// content and says nothing about how the content is written down; a `Serialize` that dropped
    /// a field, an ordering that came out of a set, or a key rendered from the row instead of from
    /// the table would leave it green and move that comparison. Both limits are stated on
    /// that test; this is the half it does not reach.
    ///
    /// **Two controls, and each has a bound worth stating.**
    ///
    /// The first is that a log one record longer exports differently — which stops the equality
    /// being `f(x) == f(x)` against an `export()` returning a constant. It is **weaker than it
    /// looks**: a shorter log stands at a lower cursor, so the `lsn` line differs on its own and
    /// this control still fires against an export that rendered *nothing but its two header
    /// lines*. Measured, with `lines.truncate(2)` inserted before the trailing-newline push in
    /// `Projections::export`: this test stays green.
    ///
    /// So the second control is the one that covers that: the document's line count equals the
    /// rows the projection holds. The same mutant reddens it
    /// — and reddens `the_export_names_every_table_the_store_holds`,
    /// `the_export_is_a_cursor_a_witness_and_one_line_per_row` and
    /// `the_export_changes_when_a_row_changes` beside it, though a claim carried only by other
    /// tests does not belong in this one's doc comment.
    #[test]
    fn the_export_agrees_between_a_live_fold_and_a_replay(ops in prop::collection::vec(op(), 1..20)) {
        let entries = expand(&ops);
        // `expand` drops an op with no subject — `Retire` before any hire — so a non-empty `ops`
        // can still expand to nothing. The control below needs a real prefix, and the empty log's
        // equality is already `live_apply_equals_replay`'s.
        prop_assume!(!entries.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let (mut w, _) = open(&journal, options()).unwrap();
        let mut live = Projections::in_memory().unwrap();
        let mut expected_recovery = BTreeMap::new();
        for entry in &entries {
            let (lsn, crc) = w.append_witnessed(entry).unwrap();
            if let Some(owner) = recovery_owner_for_generated_record(&entry.record) {
                expected_recovery.insert(lsn, RecoveryWitnessRef {
                    lsn,
                    kind: entry.record.kind(),
                    owner,
                });
            }
            prop_assert!(live.apply(lsn, crc, entry).unwrap());
        }
        let mut scratch = Projections::in_memory().unwrap();
        replay(&Reader::new(&journal), &mut scratch).unwrap();
        prop_assert_eq!(live.export().unwrap(), scratch.export().unwrap());

        // Control 1. `expand` yields at least one entry for a non-empty `ops`, so the prefix is
        // real; a shorter log stands at a lower cursor, so its first line alone already differs.
        let shorter_dir = tempfile::tempdir().unwrap();
        let shorter_journal = shorter_dir.path().join("journal");
        write(&shorter_journal, &entries[..entries.len() - 1]);
        let mut shorter = Projections::in_memory().unwrap();
        replay(&Reader::new(&shorter_journal), &mut shorter).unwrap();
        prop_assert_ne!(live.export().unwrap(), shorter.export().unwrap());

        let mut instruction_issues = BTreeSet::new();
        let mut instruction_ends = BTreeSet::new();
        let mut attached_agents = BTreeSet::new();
        let mut instruction_attachments = 0;
        for entry in &entries {
            match &entry.record {
                Record::TurnIssued {
                    turn,
                    mode: Mode::Managed,
                    ..
                } => {
                    instruction_issues.insert(*turn);
                }
                Record::TurnEnded {
                    turn,
                    provenance: Provenance::Observed,
                    ..
                } => {
                    instruction_ends.insert(*turn);
                }
                Record::Attached { agent, .. } => {
                    attached_agents.insert(*agent);
                    instruction_attachments += 1;
                }
                _ => {}
            }
        }
        let rows = live.agents().unwrap().len()
            + live.turns().unwrap().len()
            + live.goals().unwrap().len()
            + live.first_done_rows().unwrap()
            + live.budget_by_goal_rows().unwrap().len()
            + live.budget_by_day_rows().unwrap().len()
            + usize::from(!live.activity().unwrap().is_empty())
            + usize::from(!live.retired_agents().unwrap().is_empty())
            + usize::from(!live.question_history().unwrap().is_empty())
            + usize::from(!live.traces().unwrap().is_empty())
            + usize::from(!live.messages().unwrap().is_empty())
            + instruction_issues.len()
            + instruction_ends.len()
            + instruction_attachments
            + attached_agents.len()
            + expected_recovery.len();
        let export = live.export().unwrap();
        prop_assert_eq!(exported_recovery_references(&export), expected_recovery);
        prop_assert_eq!(
            export.lines().filter(|line| !line.starts_with("chat_")).count(),
            rows + 2,
            "the export should be a cursor, a witness and {} row(s):\n{}",
            rows,
            export
        );
    }

    /// Idempotent: a second replay applies nothing and changes nothing.
    #[test]
    fn replay_twice_is_a_no_op(ops in prop::collection::vec(op(), 0..60)) {
        let entries = expand(&ops);
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        write(&journal, &entries);
        let mut p = Projections::in_memory().unwrap();
        let reader = Reader::new(&journal);
        let first = replay(&reader, &mut p).unwrap();
        let before = rows(&p);
        let second = replay(&reader, &mut p).unwrap();
        prop_assert_eq!(second.applied, 0);
        prop_assert_eq!(second.last_applied, first.last_applied);
        prop_assert_eq!(rows(&p), before);
    }

    /// Segments: with a small `rotate_at`, lsns strictly increase, segment numbers never
    /// decrease, and `iter_from(Lsn::ZERO)` returns every record in order across files.
    #[test]
    fn rotation_preserves_order(ops in prop::collection::vec(op(), 0..80)) {
        let entries = expand(&ops);
        let dir = tempfile::tempdir().unwrap();
        let lsns = write(dir.path(), &entries);
        prop_assert!(lsns.windows(2).all(|w| w[0] < w[1]));
        prop_assert!(lsns.windows(2).all(|w| w[0].segment() <= w[1].segment()));
        let back: Vec<(Lsn, Entry)> = Reader::new(dir.path())
            .iter_from(Lsn::ZERO)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        prop_assert_eq!(back.iter().map(|(l, _)| *l).collect::<Vec<_>>(), lsns);
        prop_assert_eq!(back.into_iter().map(|(_, e)| e).collect::<Vec<_>>(), entries);
    }
}

/// The control the four properties above need: **the generator produces spend, in more than one
/// UTC day.** Without it they are all green over three permanently empty tables, which is what
/// the previous generator would have delivered — one `usage: None` in the whole file, and every
/// stamp inside UTC day 20687.
///
/// Asserted on a fixed op vector rather than a generated one, because a control that can be
/// skipped is not a control: this is the same [`expand`], [`stamp`] and [`ended`] the properties
/// use, so if any of them stops producing spend or stops crossing a midnight, this fails and
/// they do not.
#[test]
fn the_generator_spends_across_more_than_one_utc_day() {
    let ops = [
        Op::Hire,
        Op::SetGoal(0),
        Op::Issue(0, false),
        Op::Spawn(0, false),
        Op::End(0, 1),
        Op::Issue(0, false),
        Op::End(0, 1),
        Op::Issue(0, false),
        Op::End(0, 0),
        Op::Issue(0, false),
        Op::End(0, 3),
    ];
    let entries = expand(&ops);
    let days: BTreeSet<UtcDay> = entries.iter().map(|e| e.at.utc_day()).collect();
    assert!(
        days.len() > 1,
        "the stamps never cross a UTC midnight: {days:?}"
    );
    assert!(
        entries.iter().any(|e| matches!(
            &e.record,
            Record::TurnEnded {
                usage: Some(u),
                ..
            } if *u != Usage::ZERO
        )),
        "no generated end reports any usage"
    );

    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    let lsns = write(&journal, &entries);
    let mut p = Projections::in_memory().unwrap();
    replay(&Reader::new(&journal), &mut p).unwrap();
    check(&p, &entries, &lsns).unwrap();

    let day_rows = p.budget_by_day_rows().unwrap();
    let charged: BTreeSet<UtcDay> = day_rows.iter().map(|((_, d), _)| *d).collect();
    assert!(
        charged.len() > 1,
        "spend lands in one day row only, so the split is never exercised: {day_rows:?}"
    );
    assert!(
        !p.budget_by_goal_rows().unwrap().is_empty(),
        "no goal is ever charged"
    );
    // The `Vanished` end at the tail charging nothing is deliberately *not* asserted here.
    // [`check`], called above, already holds **every** day's `floor_spend_on` to the oracle's
    // sum over the ends that reported usage, and the oracle charges nothing for an end that
    // reported none — so any mis-charge fails there first, one call earlier, for every day
    // rather than only this one. A restatement here is unreachable as a failure: both
    // mutations that could break it (an end with no usage charging, and `floor_spend_on`
    // reading the wrong day) are caught by `check` before this line runs. This control's own
    // job is the three assertions above — that the generator produces spend at all, in more
    // than one day, against a goal.
}
