//! The fakes the method-surface tests share: a `Control`, a `JournalRead`, and the one
//! `Authorizer` in this repository that mints an operator.
//!
//! # Why `AdmitAsOperator` lives here and not in `dudes_rpc::testing`
//!
//! Every method in this surface requires `Operator`, so a refusal test needs an *admitting* arm to
//! be worth anything — a server that answers `-32001` to everything passes a refusal-only check
//! perfectly. That arm needs an `Authorizer` that resolves a peer to `PeerRole::Operator`.
//!
//! Such a fake is a bypass, and a strictly worse one than `dudes_rpc::testing`'s `AdmitAll`:
//! `AdmitAll` deliberately returns `Unproven` — it admits without claiming to have identified
//! anyone — whereas this one manufactures the exact role that unlocks all 28 operator-only methods.
//! So it is defined **in this test target**, rather than in `src/testing.rs`.
//!
//! # What that distinction buys
//!
//! `src/testing.rs` is behind the crate's `testing` feature, so a default build contains no
//! `AdmitAll`. The two containments are still not equal, and the weaker one is the module's:
//!
//! - A **test target** is not a library target. It is compiled into nothing that depends on this
//!   crate, at any feature set, and there is no flag that changes that.
//! - A **gated module** is one manifest line from a shipping build. `dudes-rpc = { …, features =
//!   ["testing"] }` in a dependent's `[dependencies]` compiles it in, and nothing but
//!   `tests/no_default_fakes.rs`'s scan of this workspace's manifests stands in the way — which
//!   says nothing about a manifest outside it.
//!
//! So the worse fake stays in the place with no flag at all. `tests/no_operator_fake.rs` is the regression guard for this file,
//! with its own planted-token control.
#![allow(
    dead_code,
    reason = "one support module for several test binaries; each uses a different subset"
)]

use std::sync::{Arc, Mutex};

use dudes_proto::decision::{Gate, GateStep, LoopDecision};
use dudes_proto::goal::Predicate;
use dudes_proto::record::{Entry, Record};
use dudes_proto::rpc::agent::{
    HireParams, HireResult, McpParams, McpResult, RetireParams, WhyResult,
};
use dudes_proto::rpc::goal::{GoalSetParams, GoalSetResult};
use dudes_proto::rpc::{AgentText, ErrorCode, RpcError};
use dudes_rpc::PersistenceOperation;
use dudes_rpc::auth::{Authorizer, PeerProvenance, PeerRole, Presented, Refusal};
use dudes_rpc::methods::{ConditionedAgentControl, Control, JournalRead};
use dudes_types::{AgentId, GoalId, Lsn, Timestamp, Tokens, Usd};

/// An [`Authorizer`] that resolves every peer to [`PeerRole::Operator`].
///
/// **Test-target only.** See this module's own documentation for why that is a hard constraint
/// rather than a preference.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AdmitAsOperator;

impl Authorizer for AdmitAsOperator {
    fn authorize(
        &self,
        _presented: Presented,
        _provenance: PeerProvenance,
        _method: &str,
    ) -> Result<PeerRole, Refusal> {
        Ok(PeerRole::Operator)
    }
}

/// The agent `agent.hire` answers with. Distinct from [`TARGET`], so a handler wired to echo its
/// input cannot pass for one that returned the daemon's answer.
pub(crate) const HIRED: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
/// The agent the `loop.*` and `agent.why` calls name.
pub(crate) const TARGET: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAY";
/// The goal `goal.set` answers with.
pub(crate) const GOAL: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";
/// The digest [`FakeJournal`] reports for `agent.why`. Distinctive, so "a `WhyResult` came back"
/// cannot be satisfied by a default-constructed one.
pub(crate) const DIGEST: &str = "d1d3541e7ede6a17e0d2f9c6e4b1a0d9";

pub(crate) fn hired() -> AgentId {
    AgentId::parse(HIRED).unwrap()
}

pub(crate) fn target() -> AgentId {
    AgentId::parse(TARGET).unwrap()
}

pub(crate) fn goal_id() -> GoalId {
    GoalId::parse(GOAL).unwrap()
}

/// A well-formed `agent.hire` request.
pub(crate) fn hire_params() -> HireParams {
    HireParams {
        character: None,
        accent: None,
        name: "Kim".to_owned(),
        role: "backend engineer".to_owned(),
        department: "engineering".to_owned(),
        provider: dudes_types::Provider::Claude,
        cwd: std::env::temp_dir().join("dudes-fixture"),
        budget: dudes_proto::budget::AgentBudget {
            usd_per_day: Some(Usd::from_dollars(10)),
            tokens_per_day: Some(Tokens::new(1_000)),
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        model: None,
        trusted: false,
        checkout: None,
        from: None,
        mcp_servers: Vec::new(),
    }
}

/// A well-formed `goal.set` request. The predicate is a `Command`, whose `argv` is a typed list —
/// nothing on this path joins it into a string (invariant 7).
pub(crate) fn goal_set_params() -> GoalSetParams {
    GoalSetParams {
        agent: target(),
        text: "make the suite green".to_owned(),
        predicate: Predicate::Command {
            argv: vec!["cargo".to_owned(), "nextest".to_owned(), "run".to_owned()],
            timeout_s: 600,
        },
        budget: dudes_proto::budget::GoalBudget {
            usd: Some(Usd::from_dollars(5)),
            tokens: None,
        },
        hop_cap: 12,
        check: None,
        may_already_pass: false,
    }
}

/// The `agent.why` answer [`FakeJournal`] gives.
pub(crate) fn why_result() -> WhyResult {
    WhyResult {
        lsn: Lsn(4_242),
        input_digest: DIGEST.to_owned(),
        decision: LoopDecision::Hold {
            reason: dudes_proto::decision::HoldReason::Paused,
        },
        gate_trace: vec![GateStep {
            gate: Gate::Control,
            fired: true,
            note: Some("paused by the operator".to_owned()),
        }],
    }
}

/// A journal entry whose payload names its own lsn, so a fan-out that delivered the *same* entry
/// several times fails a test that only reads the payloads.
pub(crate) fn entry(lsn: u64) -> Entry {
    Entry {
        at: Timestamp::from_unix_millis(1_787_394_600_000),
        record: Record::Steer {
            agent: target(),
            text: format!("entry {lsn}"),
            provenance: None,
        },
    }
}

/// The text of an [`entry`], for an assertion that wants the payload rather than the shape.
pub(crate) fn entry_text(entry: &Entry) -> String {
    match &entry.record {
        Record::Steer { text, .. } => text.clone(),
        other => panic!("the fixture writes Steer records, not {other:?}"),
    }
}

/// A [`Control`] that records what it was asked and answers with distinctive canned values.
#[derive(Debug, Default)]
pub(crate) struct FakeControl {
    calls: Mutex<Vec<String>>,
    /// When set, every method refuses with this code instead of answering.
    refuse_with: Option<ErrorCode>,
}

impl FakeControl {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A control that refuses everything with `code` — for asserting that the daemon's chosen
    /// contract code reaches the caller unchanged.
    pub(crate) fn refusing(code: ErrorCode) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            refuse_with: Some(code),
        })
    }

    /// What has been asked of it, in order.
    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn record<T>(&self, what: String, answer: T) -> Result<T, RpcError> {
        self.calls.lock().unwrap().push(what);
        match self.refuse_with {
            Some(code) => Err(RpcError {
                code: code.code(),
                message: "the fake control refused".to_owned(),
                data: None,
            }),
            None => Ok(answer),
        }
    }
}

impl Control for FakeControl {
    /// The recorded line carries `sandbox` as well as the name, and that is not decoration.
    ///
    /// A fixture that records only the name cannot see what a handler *did* to the params on the
    /// way past. With `request.sandbox = Some(false)` inserted between `agent.hire`'s
    /// refusal gate and this call, a fixture without the field passes — a handler silently
    /// converting *"the provider's configured default"* into an explicit `Some(false)` was
    /// indistinguishable from one that passed the operator's request through, and it is
    /// `AgentHired { sandbox: Some(false) }` that would then reach the journal. So what arrives
    /// here is recorded, and `tests/hire_sandbox.rs`'s admission tests assert it.
    fn hire(&self, params: HireParams) -> Result<HireResult, RpcError> {
        self.record(
            format!("hire {} sandbox={:?}", params.name, params.sandbox),
            HireResult { agent: hired() },
        )
    }

    fn retire(&self, params: RetireParams) -> Result<(), RpcError> {
        self.record(format!("retire {} {}", params.agent, params.reason), ())
    }

    fn mcp(&self, params: McpParams) -> Result<McpResult, RpcError> {
        self.record(
            format!("mcp {}", serde_json::to_string(&params).unwrap()),
            McpResult {
                agent: hired(),
                mcp_servers: vec![dudes_types::McpServerName::parse("search").unwrap()],
                changed: false,
            },
        )
    }

    fn set_goal(
        &self,
        params: GoalSetParams,
        _hang_up: &dudes_rpc::HangUp,
    ) -> Result<GoalSetResult, RpcError> {
        self.record(
            format!("set_goal {} {}", params.agent, params.text),
            GoalSetResult { goal: goal_id() },
        )
    }

    fn pause(&self, agent: AgentId) -> Result<(), RpcError> {
        self.record(format!("pause {agent}"), ())
    }

    fn resume(&self, agent: AgentId) -> Result<(), RpcError> {
        self.record(format!("resume {agent}"), ())
    }

    fn halt(&self, agent: AgentId) -> Result<(), RpcError> {
        self.record(format!("halt {agent}"), ())
    }

    /// The recorded line carries the agent, which is the point of the method: `AskParams` has no
    /// `agent` field, so what arrives here can only have come from the resolved `PeerRole::Agent`.
    /// A fixture that recorded only `"ask"` could not tell a handler that read the role from one
    /// that invented an identity.
    fn ask(&self, agent: AgentId, text: String) -> Result<(), RpcError> {
        self.record(format!("ask {agent} {text}"), ())
    }
}

impl ConditionedAgentControl for FakeControl {
    fn answer(&self, _operation: PersistenceOperation<'_, AgentText>) -> Result<(), RpcError> {
        unreachable!(
            "the classification-table fixture refuses agent.answer at the Operator requirement"
        )
    }

    fn steer(&self, _operation: PersistenceOperation<'_, AgentText>) -> Result<(), RpcError> {
        unreachable!(
            "the classification-table fixture refuses agent.steer at the Operator requirement"
        )
    }
}

/// A [`JournalRead`] over a fixed list of entries.
#[derive(Debug, Default)]
pub(crate) struct FakeJournal {
    entries: Mutex<Vec<(Lsn, Entry)>>,
}

impl FakeJournal {
    /// A journal holding nothing.
    pub(crate) fn empty() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A journal holding one entry per lsn given.
    pub(crate) fn holding(lsns: &[u64]) -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(lsns.iter().map(|lsn| (Lsn(*lsn), entry(*lsn))).collect()),
        })
    }
}

impl JournalRead for FakeJournal {
    fn entries_from(&self, from_lsn: Option<Lsn>) -> Result<Vec<(Lsn, Entry)>, RpcError> {
        let entries = self.entries.lock().unwrap();
        Ok(match from_lsn {
            None => Vec::new(),
            Some(from) => entries
                .iter()
                .filter(|(lsn, _)| *lsn >= from)
                .cloned()
                .collect(),
        })
    }

    fn last_decision(&self, agent: AgentId) -> Result<WhyResult, RpcError> {
        if agent == target() {
            Ok(why_result())
        } else {
            Err(RpcError {
                code: ErrorCode::NotFound.code(),
                message: "no such agent".to_owned(),
                data: None,
            })
        }
    }
}
