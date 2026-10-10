//! Exact mail-only authority and canonical preparation joins.

use std::collections::BTreeSet;

use dudes_types::{
    AgentId, BreakerSignal, GoalId, Lsn, MailId, MailState, Mode, ModelSelection, SessionId,
    Timestamp, TurnId,
};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize};

use crate::record::Hex32;
use crate::{Entry, ExhaustedReason, Gate, GateStep, HoldReason, MailTo, Record};

const CARRIER_BYTES: usize = crate::rpc::MAX_FRAME_BYTES / 16;
const TRACE_NOTE_BYTES: usize = 512;

/// The canonical item identity and its original receipt position.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MailRef {
    /// The recipient's immutable item identity.
    pub id: MailId,
    /// Its original Mail position, never a later state position.
    #[serde(deserialize_with = "positive_lsn")]
    #[schemars(range(min = 1))]
    pub lsn: Lsn,
}

/// One exact notification increment under its preparation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "TargetFields", deny_unknown_fields)]
#[schemars(!try_from)]
pub struct MailWakeTarget {
    /// Original canonical item reference.
    pub mail: MailRef,
    /// Durable is zero; eligible Notified counts are one through four.
    #[schemars(range(min = 0, max = 4))]
    pub original_count: u8,
    /// Exactly `original_count` plus one, at most five.
    #[schemars(range(min = 1, max = 5))]
    pub target_count: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetFields {
    mail: MailRef,
    original_count: u8,
    target_count: u8,
}

impl TryFrom<TargetFields> for MailWakeTarget {
    type Error = &'static str;
    fn try_from(value: TargetFields) -> Result<Self, Self::Error> {
        if value.original_count > 4 || value.target_count != value.original_count + 1 {
            return Err("invalid mail-wake notification increment");
        }
        Ok(Self {
            mail: value.mail,
            original_count: value.original_count,
            target_count: value.target_count,
        })
    }
}

/// The complete fixed notice and both teaching instructions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MailWakeTask(#[serde(deserialize_with = "exact_task")] String);

impl MailWakeTask {
    /// Build the exact task for an unread count in one through thirty-two.
    #[must_use]
    pub fn new(unread: u8) -> Option<Self> {
        crate::task::mail_wake_task(unread).map(Self)
    }

    /// The immutable launch bytes.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The complete unread snapshot carried by this task.
    #[must_use]
    pub fn unread_count(&self) -> u8 {
        crate::task::recognize_mail_wake_task(&self.0).expect("validated mail-only task")
    }
}

impl JsonSchema for MailWakeTask {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "MailWakeTask".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        let tasks: Vec<_> = (1..=32)
            .map(|count| crate::task::mail_wake_task(count).expect("bounded task"))
            .collect();
        schemars::json_schema!({"type":"string","enum":tasks})
    }
}

/// The most unread mails and targets one wake plan carries; older unread mail waits for a later plan.
pub const MAX_WAKE_TARGETS: usize = 32;

/// One immutable, bounded incrementing plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "PlanFields", deny_unknown_fields)]
#[schemars(!try_from)]
pub struct MailWakePlan {
    /// The oldest owed qualifying arrival.
    pub trigger: MailRef,
    /// Recipient arrivals covered by this preparation, through this position.
    #[schemars(range(min = 1))]
    pub frontier: Lsn,
    /// Complete unread snapshot, including unread items already notified five times.
    #[schemars(range(min = 1, max = 32))]
    pub unread_count: u8,
    /// Exact notice and fixed teaching, retained even after partial terminalization.
    pub task: MailWakeTask,
    /// Distinct targets in original Mail LSN then ID order.
    #[schemars(length(min = 1, max = 32))]
    pub targets: Vec<MailWakeTarget>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanFields {
    trigger: MailRef,
    #[serde(deserialize_with = "positive_lsn")]
    frontier: Lsn,
    unread_count: u8,
    task: MailWakeTask,
    targets: Vec<MailWakeTarget>,
}

impl MailWakePlan {
    /// Whether all local bounds and exact snapshot relationships hold.
    #[must_use]
    pub fn valid(&self) -> bool {
        let mut ids = BTreeSet::new();
        self.trigger.lsn != Lsn::ZERO
            && self.frontier >= self.trigger.lsn
            && (1..=32).contains(&self.unread_count)
            && self.task.unread_count() == self.unread_count
            && (1..=usize::from(self.unread_count)).contains(&self.targets.len())
            && self.targets.iter().all(|target| {
                target.mail.lsn != Lsn::ZERO
                    && target.mail.lsn <= self.frontier
                    && target.original_count <= 4
                    && target.target_count == target.original_count + 1
                    && ids.insert(target.mail.id)
            })
            && self.targets.windows(2).all(|pair| {
                (pair[0].mail.lsn, pair[0].mail.id) < (pair[1].mail.lsn, pair[1].mail.id)
            })
    }
}

impl TryFrom<PlanFields> for MailWakePlan {
    type Error = &'static str;
    fn try_from(value: PlanFields) -> Result<Self, Self::Error> {
        let plan = Self {
            trigger: value.trigger,
            frontier: value.frontier,
            unread_count: value.unread_count,
            task: value.task,
            targets: value.targets,
        };
        if !plan.valid() {
            return Err("invalid mail-wake preparation plan");
        }
        Ok(plan)
    }
}

/// Which authority a decision may create or reconsider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MailWakePurpose {
    /// Only this purpose with Issue creates incrementing authority.
    FreshPreparation {
        /// Exact admitted task and targets.
        plan: MailWakePlan,
    },
    /// Reconsider the existing plan without creating another notification charge.
    ExistingPreparation {
        /// The positive position of the fresh Issue decision.
        #[serde(deserialize_with = "positive_lsn")]
        #[schemars(range(min = 1))]
        root_lsn: Lsn,
    },
    /// Refusal bookkeeping before a preparation exists.
    UnpreparedCandidate {
        /// Original qualifying receipt.
        trigger: MailRef,
        /// Current coalesced arrival frontier.
        #[serde(deserialize_with = "positive_lsn")]
        #[schemars(range(min = 1))]
        frontier: Lsn,
    },
}

/// Distinct canonical digest domains for fresh and resumed admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MailWakeDigestDomain {
    /// Fresh preparation or an unprepared candidate.
    Fresh,
    /// Existing preparation and its canonical progress.
    Resume,
}

/// A complete input digest with an explicit admission domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailWakeDigest {
    /// Fresh and resume inputs cannot compare as the same authority.
    pub domain: MailWakeDigestDomain,
    /// Exactly sixty-four lowercase hexadecimal characters.
    #[schemars(with = "DigestSchema")]
    pub value: Hex32,
}

/// Mail-only admission cannot complete or reopen a goal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MailWakeAdmission {
    /// Admit this immutable mail-only task.
    Issue {
        /// Complete snapshot bytes.
        task: MailWakeTask,
    },
    /// Retain debt while the canonical human question stands.
    Wait {
        /// The canonical question identity, without repeating arbitrary question text.
        #[serde(deserialize_with = "positive_lsn")]
        #[schemars(range(min = 1))]
        question_lsn: Lsn,
    },
    /// A current control or provenance gate refuses.
    Hold {
        /// Existing fixed reason.
        reason: HoldReason,
    },
    /// A budget, breaker or mail-wake ceiling refuses.
    Exhausted {
        /// Existing ceiling vocabulary, including the distinct mail-wake limit.
        reason: ExhaustedReason,
    },
}

/// Required decision authority, separate from ordinary or browser-note decisions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "DecisionFields", deny_unknown_fields)]
#[schemars(!try_from)]
pub struct MailWakeDecision {
    /// Recipient.
    pub agent: AgentId,
    /// Original completed goal.
    pub goal: GoalId,
    /// Exact ordinary Done position.
    #[schemars(range(min = 1))]
    pub completion_lsn: Lsn,
    /// Fresh plan, current root or refusal-only candidate.
    pub purpose: MailWakePurpose,
    /// Complete admission input identity.
    pub input_digest: MailWakeDigest,
    /// Ordered common-gate prefix, with only its final gate firing.
    #[schemars(with = "TraceSchema")]
    pub gate_trace: Vec<GateStep>,
    /// No Done variant exists.
    pub decision: MailWakeAdmission,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionFields {
    agent: AgentId,
    goal: GoalId,
    #[serde(deserialize_with = "positive_lsn")]
    completion_lsn: Lsn,
    purpose: MailWakePurpose,
    input_digest: MailWakeDigest,
    #[serde(deserialize_with = "bounded_trace")]
    gate_trace: Vec<GateStep>,
    decision: MailWakeAdmission,
}

impl MailWakeDecision {
    /// Whether its bounded payload, purpose, task, domain and firing trace agree.
    #[must_use]
    pub fn valid(&self) -> bool {
        if self.completion_lsn == Lsn::ZERO
            || self.gate_trace.is_empty()
            || self.gate_trace.len() > Gate::ORDER.len()
            || !self
                .gate_trace
                .iter()
                .zip(Gate::ORDER)
                .enumerate()
                .all(|(index, (step, gate))| {
                    step.gate == gate
                        && step.fired == (index + 1 == self.gate_trace.len())
                        && !(step.fired && matches!(gate, Gate::Done | Gate::Hops))
                        && step
                            .note
                            .as_ref()
                            .is_none_or(|note| note.len() <= TRACE_NOTE_BYTES)
                })
        {
            return false;
        }
        let domain = match &self.purpose {
            MailWakePurpose::ExistingPreparation { root_lsn } if *root_lsn != Lsn::ZERO => {
                MailWakeDigestDomain::Resume
            }
            MailWakePurpose::FreshPreparation { plan } if plan.valid() => {
                let MailWakeAdmission::Issue { task } = &self.decision else {
                    return false;
                };
                if task != &plan.task {
                    return false;
                }
                MailWakeDigestDomain::Fresh
            }
            MailWakePurpose::UnpreparedCandidate { trigger, frontier }
                if trigger.lsn != Lsn::ZERO && *frontier >= trigger.lsn =>
            {
                if matches!(self.decision, MailWakeAdmission::Issue { .. }) {
                    return false;
                }
                MailWakeDigestDomain::Fresh
            }
            _ => return false,
        };
        if self.input_digest.domain != domain {
            return false;
        }
        let gate = self.gate_trace.last().expect("nonempty trace").gate;
        match &self.decision {
            MailWakeAdmission::Issue { .. } => gate == Gate::Work,
            MailWakeAdmission::Wait { question_lsn } => {
                *question_lsn != Lsn::ZERO && gate == Gate::Human
            }
            MailWakeAdmission::Hold { reason } => {
                gate == Gate::Control
                    || (gate == Gate::Provenance && *reason == HoldReason::InferredTurnEnd)
            }
            MailWakeAdmission::Exhausted { reason } => {
                gate == Gate::Budget && *reason != ExhaustedReason::Hops
            }
        }
    }
}

impl TryFrom<DecisionFields> for MailWakeDecision {
    type Error = &'static str;
    fn try_from(value: DecisionFields) -> Result<Self, Self::Error> {
        let decision = Self {
            agent: value.agent,
            goal: value.goal,
            completion_lsn: value.completion_lsn,
            purpose: value.purpose,
            input_digest: value.input_digest,
            gate_trace: value.gate_trace,
            decision: value.decision,
        };
        if !decision.valid() {
            return Err("invalid mail-wake decision contract");
        }
        Ok(decision)
    }
}

/// One normal managed issue carrying strict mail-only authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "IssueFields", deny_unknown_fields)]
#[schemars(!try_from)]
pub struct MailWakeIssue {
    /// Recipient.
    pub agent: AgentId,
    /// Existing normal turn identity, minted only at issue.
    pub turn: TurnId,
    /// Original completed goal, used for normal usage and hop accounting.
    pub goal: GoalId,
    /// Exact ordinary Done position.
    #[schemars(range(min = 1))]
    pub completion_lsn: Lsn,
    /// Fresh preparation position.
    #[schemars(range(min = 1))]
    pub root_lsn: Lsn,
    /// Current admitting resumed decision position.
    #[schemars(range(min = 1))]
    pub lsn_of_decision: Lsn,
    /// Exact prepared task.
    pub task: MailWakeTask,
    /// Required announced session or explicit null, recovered normally on reissue.
    #[schemars(with = "crate::chat::ExplicitNullable<CarrierSchema>")]
    pub session: Option<SessionId>,
    /// Always Managed.
    #[schemars(with = "ManagedSchema")]
    pub mode: Mode,
    /// Required recorded selection; default remains distinct from absent.
    #[schemars(with = "ModelSchema")]
    pub model_selection: ModelSelection,
    /// Required empty carrier, leaving pending answers deferred.
    #[schemars(length(max = 0))]
    pub answers: Vec<Lsn>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueFields {
    agent: AgentId,
    turn: TurnId,
    goal: GoalId,
    #[serde(deserialize_with = "positive_lsn")]
    completion_lsn: Lsn,
    #[serde(deserialize_with = "positive_lsn")]
    root_lsn: Lsn,
    #[serde(deserialize_with = "positive_lsn")]
    lsn_of_decision: Lsn,
    task: MailWakeTask,
    #[serde(deserialize_with = "bounded_session")]
    session: Option<SessionId>,
    mode: Mode,
    #[serde(deserialize_with = "bounded_model")]
    model_selection: ModelSelection,
    answers: Vec<Lsn>,
}

impl MailWakeIssue {
    /// Whether the normal carrier stays bounded, Managed, exact and answer-free.
    #[must_use]
    pub fn valid(&self) -> bool {
        self.completion_lsn != Lsn::ZERO
            && self.root_lsn != Lsn::ZERO
            && self.lsn_of_decision > self.root_lsn
            && self.mode == Mode::Managed
            && self.answers.is_empty()
            && self
                .session
                .as_ref()
                .is_none_or(|id| id.as_str().len() <= CARRIER_BYTES)
            && self
                .model_selection
                .selector()
                .is_none_or(|text| text.len() <= CARRIER_BYTES)
    }
}

impl TryFrom<IssueFields> for MailWakeIssue {
    type Error = &'static str;
    fn try_from(value: IssueFields) -> Result<Self, Self::Error> {
        let issue = Self {
            agent: value.agent,
            turn: value.turn,
            goal: value.goal,
            completion_lsn: value.completion_lsn,
            root_lsn: value.root_lsn,
            lsn_of_decision: value.lsn_of_decision,
            task: value.task,
            session: value.session,
            mode: value.mode,
            model_selection: value.model_selection,
            answers: value.answers,
        };
        if !issue.valid() {
            return Err("invalid mail-wake managed issue carrier");
        }
        Ok(issue)
    }
}

struct CarrierSchema;
impl JsonSchema for CarrierSchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "MailWakeCarrierText".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({"type":"string","maxLength":CARRIER_BYTES,
            "description":"Opaque recorded bytes, bounded at65,536 UTF-8 bytes."})
    }
}

struct ModelSchema;
impl JsonSchema for ModelSchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "MailWakeModelSelection".into()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({"type":"object","additionalProperties":false,
            "properties":{"selector":{"anyOf":[g.subschema_for::<CarrierSchema>(),{"type":"null"}]}}})
    }
}

struct DigestSchema;
impl JsonSchema for DigestSchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "MailWakeDigestHex".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({"type":"string","minLength":64,"maxLength":64,"pattern":"^[0-9a-f]{64}$"})
    }
}

struct TraceSchema;
impl JsonSchema for TraceSchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "MailWakeGateTrace".into()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({"type":"array","minItems":1,"maxItems":7,
            "items":{"allOf":[g.subschema_for::<GateStep>(),{"additionalProperties":false,"properties":{"gate":{},"fired":{},"note":{"type":"string","maxLength":TRACE_NOTE_BYTES}}}]},
            "description":"An ordered common-gate prefix whose final gate alone fires; Done and Hops never fire. Notes are bounded at512 UTF-8 bytes."})
    }
}

struct ManagedSchema;
impl JsonSchema for ManagedSchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "MailWakeManagedMode".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({"type":"string","const":"managed"})
    }
}

fn positive_lsn<'de, D: Deserializer<'de>>(d: D) -> Result<Lsn, D::Error> {
    let lsn = Lsn::deserialize(d)?;
    if lsn == Lsn::ZERO {
        return Err(serde::de::Error::custom(
            "expected a positive canonical position",
        ));
    }
    Ok(lsn)
}

fn exact_task<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let task = String::deserialize(d)?;
    if crate::task::recognize_mail_wake_task(&task).is_none() {
        return Err(serde::de::Error::custom("invalid exact mail-only task"));
    }
    Ok(task)
}

fn bounded_session<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SessionId>, D::Error> {
    let session = Option::<SessionId>::deserialize(d)?;
    if session
        .as_ref()
        .is_some_and(|id| id.as_str().len() > CARRIER_BYTES)
    {
        return Err(serde::de::Error::custom(
            "mail-wake session exceeds the carrier bound",
        ));
    }
    Ok(session)
}

fn bounded_model<'de, D: Deserializer<'de>>(d: D) -> Result<ModelSelection, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fields {
        #[serde(default)]
        selector: Option<String>,
    }
    let fields = Fields::deserialize(d)?;
    if fields
        .selector
        .as_ref()
        .is_some_and(|text| text.len() > CARRIER_BYTES)
    {
        return Err(serde::de::Error::custom(
            "mail-wake model exceeds the carrier bound",
        ));
    }
    Ok(ModelSelection {
        selector: fields.selector,
    })
}

fn bounded_trace<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<GateStep>, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Step {
        gate: Gate,
        fired: bool,
        #[serde(default)]
        note: Option<String>,
    }
    let steps = Vec::<Step>::deserialize(d)?;
    if steps.is_empty()
        || steps.len() > 7
        || steps.iter().any(|step| {
            step.note
                .as_ref()
                .is_some_and(|note| note.len() > TRACE_NOTE_BYTES)
        })
    {
        return Err(serde::de::Error::custom(
            "mail-wake trace exceeds its bounds",
        ));
    }
    Ok(steps
        .into_iter()
        .map(|step| GateStep {
            gate: step.gate,
            fired: step.fired,
            note: step.note,
        })
        .collect())
}

/// One canonical receipt's current delivery facts, without content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailWakeMail {
    /// Immutable receipt identity.
    pub reference: MailRef,
    /// Zero for Durable, the exact Notified count, or absent for terminal mail.
    pub count: Option<u8>,
    /// Exact canonical terminal position when terminal.
    pub terminal_lsn: Option<Lsn>,
}

impl MailWakeMail {
    /// Narrow a canonical receipt/state row to the facts the wake fold reads.
    #[must_use]
    pub const fn from_state(reference: MailRef, state: &MailState, state_lsn: Lsn) -> Self {
        match state {
            MailState::Durable => Self {
                reference,
                count: Some(0),
                terminal_lsn: None,
            },
            MailState::Notified { n } => Self {
                reference,
                count: Some(*n),
                terminal_lsn: None,
            },
            MailState::Acked | MailState::Bounced { .. } => Self {
                reference,
                count: None,
                terminal_lsn: Some(state_lsn),
            },
        }
    }
}

/// Canonical facts supplied by the caller's existing goal, agent, turn and mail folds.
#[derive(Clone, Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent supplied canonical facts are not mutually exclusive lifecycle states"
)]
pub struct MailWakeContext {
    /// Current uncleared original goal and its first ordinary Done position.
    pub completion: Option<(GoalId, Lsn)>,
    /// Retirement has no managed recipient authority.
    pub retired: bool,
    /// A managed issue cannot own an attached agent's slot.
    pub attached: bool,
    /// Current normal issued/spawned turn, if any.
    pub open_turn: Option<TurnId>,
    /// Whether the proposed issue's `TurnId` already exists in the normal turn fold.
    pub turn_exists: bool,
    /// Whether any canonical mailbox already owns the incoming `MailId`.
    pub receipt_exists: bool,
    /// Current canonical human question identity.
    pub question_lsn: Option<Lsn>,
    /// This recipient's canonical receipt rows before the incoming `MailState` or Mail.
    pub mails: Vec<MailWakeMail>,
}

/// Oldest owed arrival and its deterministic coalesced frontier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailWakeCandidate {
    /// Goal completed at this receipt's prefix.
    pub goal: GoalId,
    /// Exact completion at that prefix.
    pub completion_lsn: Lsn,
    /// Oldest owed qualifying receipt.
    pub trigger: MailRef,
    /// Latest coalesced qualifying receipt.
    pub frontier: Lsn,
}

/// Exact progress for one target, retaining both notification and terminal facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MailWakeProgress {
    /// No matching advance or terminal fact committed yet.
    Pending,
    /// This exact notification advance committed.
    Notified {
        /// Matching `MailState` position.
        lsn: Lsn,
    },
    /// The target is permanently unnecessary for this unissued plan.
    Terminal {
        /// Canonical Acked/Bounced position.
        lsn: Lsn,
        /// Earlier matching notification, when this target was already charged.
        notified_lsn: Option<Lsn>,
    },
}

/// The one active incrementing preparation for a recipient.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailWakePreparation {
    /// Exact fresh Issue position.
    pub root_lsn: Lsn,
    /// Original completed goal.
    pub goal: GoalId,
    /// Exact ordinary completion.
    pub completion_lsn: Lsn,
    /// Immutable bounded snapshot.
    pub plan: MailWakePlan,
    /// One exact progress value per ordered target.
    pub progress: Vec<MailWakeProgress>,
}

/// The latest accepted admission, rather than a permanent object per historical plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailWakeLatest {
    /// Canonical decision position.
    pub lsn: Lsn,
    /// Exact final stamp that an admitted issue must share.
    pub at: Timestamp,
    /// Resumed root, absent for fresh/candidate admission.
    pub root_lsn: Option<Lsn>,
    /// Complete domain-qualified input identity.
    pub input_digest: MailWakeDigest,
    /// Bounded no-Done verdict.
    pub decision: MailWakeAdmission,
}

/// Rebuildable bounded wake state for one recipient.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailWakeState {
    /// Last canonical record affecting this row.
    pub last_lsn: Lsn,
    /// Only issued or canonically retired preparation frontiers are covered.
    pub covered_frontier: Lsn,
    /// Next owed qualifying arrival, including arrivals after an active plan.
    pub candidate: Option<MailWakeCandidate>,
    /// One at-most-thirty-two-target preparation.
    pub preparation: Option<MailWakePreparation>,
    /// Latest bounded admission identity and verdict.
    pub latest: Option<MailWakeLatest>,
    /// Newest joined four-to-five transition.
    pub fifth_crossing: Lsn,
    /// Canonical same-agent `UnackedMail` beat LSN that consumed an owed earlier crossing prefix.
    pub fifth_consumed: Lsn,
}

/// One validated fold result; callers store the state and charge only an accepted issue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailWakeApplied {
    /// Resulting bounded row.
    pub state: MailWakeState,
    /// Exactly this incoming required issue owns one normal issue/day/hop charge.
    pub issued: bool,
}

/// A named failed canonical join, never admission authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailWakeError {
    /// Payload bounds, task, purpose, trace or digest domain disagree.
    Contract,
    /// The recipient no longer owns this exact ordinary completion.
    Completion,
    /// The oldest owed candidate or its frontier does not match.
    Candidate,
    /// A preparation is absent, retired, conflicting or named incorrectly.
    Root,
    /// A target's original receipt, owner, count or snapshot does not match.
    Target,
    /// A notification or terminal transition cannot belong to this active root.
    Progress,
    /// An admitted resume is not ready or still owes fifth-crossing consumption.
    NotReady,
    /// Current issue identity, task, decision, timestamp or normal slot does not match.
    Issue,
    /// Twenty issues already belong to the stamped UTC day.
    Limit,
    /// Identical refusal input would append another decision.
    DuplicateRefusal,
}

impl std::fmt::Display for MailWakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mail-wake {self:?} join failed")
    }
}
impl std::error::Error for MailWakeError {}

impl MailWakeState {
    /// Fold one canonical record against existing facts without mutating the prior row on error.
    #[expect(
        clippy::too_many_lines,
        reason = "one domain reducer owns each canonical wake transition"
    )]
    pub fn apply(
        &self,
        agent: AgentId,
        context: &MailWakeContext,
        wakes_today: u8,
        lsn: Lsn,
        entry: &Entry,
    ) -> Result<MailWakeApplied, MailWakeError> {
        if !self.progress_shape_matches() {
            return Err(MailWakeError::Progress);
        }
        let mut row = self.clone();
        let mut issued = false;
        if lsn <= row.last_lsn {
            return Ok(MailWakeApplied { state: row, issued });
        }
        match &entry.record {
            Record::Mail {
                id,
                to: MailTo::Agent { agent: recipient },
                ..
            } if *recipient == agent => {
                if !context.receipt_exists
                    && !context.mails.iter().any(|mail| mail.reference.id == *id)
                    && !context.retired
                    && let Some((goal, completion_lsn)) = context.completion
                    && lsn > row.covered_frontier
                {
                    let trigger = MailRef { id: *id, lsn };
                    if let Some(candidate) = &mut row.candidate {
                        if (candidate.goal, candidate.completion_lsn) != (goal, completion_lsn) {
                            return Err(MailWakeError::Candidate);
                        }
                        candidate.frontier = lsn;
                    } else {
                        row.candidate = Some(MailWakeCandidate {
                            goal,
                            completion_lsn,
                            trigger,
                            frontier: lsn,
                        });
                    }
                }
            }
            Record::GoalCleared { agent: owner, .. } if *owner == agent => {
                row.retire();
                row.candidate = None;
            }
            record
                if record
                    .goal_install()
                    .is_some_and(|(owner, _)| owner == agent) =>
            {
                row.retire();
                row.candidate = None;
            }
            Record::GoalReassigned { from, to, .. } if *from == agent || *to == agent => {
                row.retire();
                row.candidate = None;
            }
            Record::AgentRetired { id, .. } if *id == agent => {
                row.retire();
                row.candidate = None;
            }
            Record::MailState { id, state } => {
                let owns_target = row.preparation.as_ref().is_some_and(|preparation| {
                    preparation
                        .plan
                        .targets
                        .iter()
                        .any(|target| target.mail.id == *id)
                });
                if !owns_target
                    && matches!(state, MailState::Notified { .. })
                    && context.mails.iter().any(|mail| {
                        mail.reference.id == *id && mail.reference.lsn <= row.covered_frontier
                    })
                {
                    return Err(MailWakeError::Progress);
                }
                if let Some(preparation) = &mut row.preparation {
                    let target_index = preparation
                        .plan
                        .targets
                        .iter()
                        .position(|target| target.mail.id == *id);
                    if let Some(index) = target_index {
                        let target = &preparation.plan.targets[index];
                        let mail = context
                            .mails
                            .iter()
                            .find(|mail| mail.reference == target.mail)
                            .ok_or(MailWakeError::Target)?;
                        if mail.terminal_lsn.is_some() {
                            return Err(MailWakeError::Progress);
                        }
                        match state {
                            MailState::Notified { n } => {
                                if !matches!(preparation.progress[index], MailWakeProgress::Pending)
                                    || mail.count != Some(target.original_count)
                                    || *n != target.target_count
                                {
                                    return Err(MailWakeError::Progress);
                                }
                                preparation.progress[index] = MailWakeProgress::Notified { lsn };
                                if *n == 5 {
                                    row.fifth_crossing = lsn;
                                }
                            }
                            MailState::Acked | MailState::Bounced { .. } => {
                                let notified_lsn = match preparation.progress[index] {
                                    MailWakeProgress::Pending => None,
                                    MailWakeProgress::Notified { lsn } => Some(lsn),
                                    MailWakeProgress::Terminal { .. } => {
                                        return Err(MailWakeError::Progress);
                                    }
                                };
                                preparation.progress[index] =
                                    MailWakeProgress::Terminal { lsn, notified_lsn };
                            }
                            MailState::Durable => return Err(MailWakeError::Progress),
                        }
                    } else if matches!(state, MailState::Notified { .. })
                        && context.mails.iter().any(|mail| mail.reference.id == *id)
                    {
                        return Err(MailWakeError::Progress);
                    }
                    if preparation
                        .progress
                        .iter()
                        .all(|progress| matches!(progress, MailWakeProgress::Terminal { .. }))
                    {
                        row.retire();
                    }
                }
            }
            Record::BreakerBeat {
                agent: owner,
                signals,
                ..
            } if *owner == agent => {
                if !context.retired
                    && signals.contains(&BreakerSignal::UnackedMail)
                    && row.fifth_consumed < row.fifth_crossing
                    && row.fifth_crossing < lsn
                {
                    row.fifth_consumed = lsn;
                }
            }
            Record::MailWakeTurnDecided(decision) if decision.agent == agent => {
                row.decide(context, wakes_today, lsn, entry.at, decision)?;
            }
            Record::MailWakeOnlyTurnIssued(issue) if issue.agent == agent => {
                row.issue(context, wakes_today, lsn, entry.at, issue)?;
                issued = true;
            }
            _ => {}
        }
        row.last_lsn = lsn;
        Ok(MailWakeApplied { state: row, issued })
    }

    fn completion_matches(context: &MailWakeContext, goal: GoalId, completion_lsn: Lsn) -> bool {
        !context.retired && context.completion == Some((goal, completion_lsn))
    }

    fn decide(
        &mut self,
        context: &MailWakeContext,
        wakes_today: u8,
        lsn: Lsn,
        at: Timestamp,
        decision: &MailWakeDecision,
    ) -> Result<(), MailWakeError> {
        if !decision.valid() || decision.completion_lsn >= lsn {
            return Err(MailWakeError::Contract);
        }
        if !Self::completion_matches(context, decision.goal, decision.completion_lsn) {
            return Err(MailWakeError::Completion);
        }
        if let MailWakeAdmission::Wait { question_lsn } = decision.decision
            && context.question_lsn != Some(question_lsn)
        {
            return Err(MailWakeError::Contract);
        }
        if !matches!(decision.decision, MailWakeAdmission::Issue { .. })
            && self
                .latest
                .as_ref()
                .is_some_and(|latest| latest.input_digest == decision.input_digest)
        {
            return Err(MailWakeError::DuplicateRefusal);
        }
        let root_lsn = match &decision.purpose {
            MailWakePurpose::FreshPreparation { plan } => {
                self.prepare_root(context, wakes_today, lsn, decision, plan)?;
                None
            }
            MailWakePurpose::UnpreparedCandidate { trigger, frontier } => {
                if self.preparation.is_some() || *frontier >= lsn {
                    return Err(MailWakeError::Root);
                }
                self.candidate_matches(decision, *trigger, *frontier)?;
                None
            }
            MailWakePurpose::ExistingPreparation { root_lsn } => {
                let preparation = self.preparation.as_ref().ok_or(MailWakeError::Root)?;
                if preparation.root_lsn != *root_lsn
                    || *root_lsn >= lsn
                    || (preparation.goal, preparation.completion_lsn)
                        != (decision.goal, decision.completion_lsn)
                {
                    return Err(MailWakeError::Root);
                }
                if let MailWakeAdmission::Issue { task } = &decision.decision {
                    if task != &preparation.plan.task {
                        return Err(MailWakeError::Contract);
                    }
                    if !self.ready() {
                        return Err(MailWakeError::NotReady);
                    }
                    if wakes_today >= 20 {
                        return Err(MailWakeError::Limit);
                    }
                    if context.attached || context.open_turn.is_some() {
                        return Err(MailWakeError::Issue);
                    }
                }
                Some(*root_lsn)
            }
        };
        if matches!(
            decision.decision,
            MailWakeAdmission::Exhausted {
                reason: ExhaustedReason::MailWakeLimit
            }
        ) && wakes_today < 20
        {
            return Err(MailWakeError::Limit);
        }
        self.latest = Some(MailWakeLatest {
            lsn,
            at,
            root_lsn,
            input_digest: decision.input_digest,
            decision: decision.decision.clone(),
        });
        Ok(())
    }

    fn prepare_root(
        &mut self,
        context: &MailWakeContext,
        wakes_today: u8,
        lsn: Lsn,
        decision: &MailWakeDecision,
        plan: &MailWakePlan,
    ) -> Result<(), MailWakeError> {
        if self.preparation.is_some() {
            return Err(MailWakeError::Root);
        }
        self.candidate_matches(decision, plan.trigger, plan.frontier)?;
        if wakes_today >= 20 {
            return Err(MailWakeError::Limit);
        }
        if context.attached || context.open_turn.is_some() {
            return Err(MailWakeError::Issue);
        }
        let unread = context
            .mails
            .iter()
            .filter(|mail| mail.count.is_some())
            .count()
            .min(MAX_WAKE_TARGETS);
        if unread != usize::from(plan.unread_count)
            || plan.frontier >= lsn
            || plan.targets.iter().any(|target| {
                !context.mails.iter().any(|mail| {
                    mail.reference == target.mail && mail.count == Some(target.original_count)
                })
            })
        {
            return Err(MailWakeError::Target);
        }
        self.preparation = Some(MailWakePreparation {
            root_lsn: lsn,
            goal: decision.goal,
            completion_lsn: decision.completion_lsn,
            plan: plan.clone(),
            progress: vec![MailWakeProgress::Pending; plan.targets.len()],
        });
        self.candidate = None;
        Ok(())
    }

    fn candidate_matches(
        &self,
        decision: &MailWakeDecision,
        trigger: MailRef,
        frontier: Lsn,
    ) -> Result<(), MailWakeError> {
        let candidate = self.candidate.as_ref().ok_or(MailWakeError::Candidate)?;
        if (
            candidate.goal,
            candidate.completion_lsn,
            candidate.trigger,
            candidate.frontier,
        ) != (decision.goal, decision.completion_lsn, trigger, frontier)
        {
            return Err(MailWakeError::Candidate);
        }
        Ok(())
    }

    fn issue(
        &mut self,
        context: &MailWakeContext,
        wakes_today: u8,
        lsn: Lsn,
        at: Timestamp,
        issue: &MailWakeIssue,
    ) -> Result<(), MailWakeError> {
        if !issue.valid() {
            return Err(MailWakeError::Contract);
        }
        if !Self::completion_matches(context, issue.goal, issue.completion_lsn) {
            return Err(MailWakeError::Completion);
        }
        let preparation = self.preparation.as_ref().ok_or(MailWakeError::Root)?;
        let latest = self.latest.as_ref().ok_or(MailWakeError::Issue)?;
        if preparation.root_lsn != issue.root_lsn
            || (preparation.goal, preparation.completion_lsn) != (issue.goal, issue.completion_lsn)
            || preparation.plan.task != issue.task
            || latest.lsn != issue.lsn_of_decision
            || latest.lsn >= lsn
            || latest.root_lsn != Some(issue.root_lsn)
            || latest.at != at
            || !matches!(&latest.decision, MailWakeAdmission::Issue { task } if task == &issue.task)
            || context.attached
            || context.open_turn.is_some()
            || context.turn_exists
        {
            return Err(MailWakeError::Issue);
        }
        if !self.ready() {
            return Err(MailWakeError::NotReady);
        }
        if wakes_today >= 20 {
            return Err(MailWakeError::Limit);
        }
        self.retire();
        Ok(())
    }

    /// Whether a current plan's exact progress and crossing consumption permit final admission.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.progress_shape_matches()
            && self.fifth_crossing <= self.fifth_consumed
            && self.preparation.as_ref().is_some_and(|preparation| {
                preparation
                    .progress
                    .iter()
                    .all(|progress| !matches!(progress, MailWakeProgress::Pending))
                    && preparation
                        .progress
                        .iter()
                        .any(|progress| matches!(progress, MailWakeProgress::Notified { .. }))
            })
    }

    fn progress_shape_matches(&self) -> bool {
        self.preparation
            .as_ref()
            .is_none_or(|preparation| preparation.progress.len() == preparation.plan.targets.len())
    }

    fn retire(&mut self) {
        if let Some(preparation) = self.preparation.take() {
            self.covered_frontier = self.covered_frontier.max(preparation.plan.frontier);
        }
        if self
            .candidate
            .as_ref()
            .is_some_and(|candidate| candidate.frontier <= self.covered_frontier)
        {
            self.candidate = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dudes_types::BreakerRung;

    fn actor() -> AgentId {
        AgentId::from_parts(1, 1)
    }
    fn goal() -> GoalId {
        GoalId::from_parts(1, 2)
    }
    fn mail(index: u64) -> Record {
        let mut record = crate::record::samples::record(crate::RecordKind::Mail);
        let Record::Mail { id, to, state, .. } = &mut record else {
            unreachable!();
        };
        *id = MailId::from_parts(2, u128::from(index));
        *to = MailTo::Agent { agent: actor() };
        *state = MailState::Durable;
        record
    }
    fn beat(owner: AgentId, signals: Vec<BreakerSignal>) -> Record {
        Record::BreakerBeat {
            agent: owner,
            rung_before: BreakerRung::Stop,
            rung_after: BreakerRung::Stop,
            signals,
        }
    }

    #[test]
    fn wire_records_refuse_plan_trace_carrier_and_cross_domain_corruption() {
        use serde_json::{Value, json};
        let root = serde_json::to_value(crate::record::samples::record(
            crate::RecordKind::MailWakeTurnDecided,
        ))
        .unwrap();
        let issue = serde_json::to_value(crate::record::samples::record(
            crate::RecordKind::MailWakeOnlyTurnIssued,
        ))
        .unwrap();
        for value in [&root, &issue] {
            let decoded: Record = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), *value);
        }
        let mut invalid = Vec::new();
        let mut change = |source: &Value, pointer: &str, value: Value| {
            let mut damaged = source.clone();
            *damaged.pointer_mut(pointer).unwrap() = value;
            invalid.push(damaged);
        };
        change(&root, "/completion_lsn", json!(0));
        change(&root, "/purpose/plan/frontier", json!(0));
        change(&root, "/purpose/plan/trigger/lsn", json!(0));
        change(&root, "/purpose/plan/unread_count", json!(0));
        change(&root, "/purpose/plan/unread_count", json!(33));
        change(&root, "/purpose/plan/unread_count", json!(2));
        change(&root, "/purpose/plan/targets", json!([]));
        change(&root, "/purpose/plan/targets/0/original_count", json!(5));
        change(&root, "/purpose/plan/targets/0/target_count", json!(2));
        change(&root, "/purpose/plan/task", json!("[1 unread mail]"));
        change(&root, "/input_digest/domain", json!("resume"));
        change(&root, "/input_digest/value", json!("01".repeat(31)));
        change(&root, "/gate_trace", json!([]));
        change(&root, "/gate_trace/0/gate", json!("provenance"));
        change(&root, "/gate_trace/0/fired", json!(true));
        change(&root, "/decision", json!({"kind":"done"}));
        change(&issue, "/mode", json!("attached"));
        change(&issue, "/answers", json!([1]));
        change(&issue, "/root_lsn", issue["lsn_of_decision"].clone());
        change(
            &issue,
            "/session",
            json!("\u{0001}".repeat(CARRIER_BYTES + 1)),
        );
        change(
            &issue,
            "/model_selection",
            json!({"selector":"x".repeat(CARRIER_BYTES + 1)}),
        );
        for key in ["session", "model_selection", "answers"] {
            let mut damaged = issue.clone();
            damaged.as_object_mut().unwrap().remove(key);
            invalid.push(damaged);
        }
        for pointer in ["", "/model_selection"] {
            let mut damaged = issue.clone();
            damaged
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("extra".into(), json!("x"));
            invalid.push(damaged);
        }
        for note in [
            json!("\u{0001}".repeat(TRACE_NOTE_BYTES + 1)),
            json!("€".repeat(TRACE_NOTE_BYTES)),
        ] {
            let mut damaged = root.clone();
            damaged["gate_trace"][0]["note"] = note;
            invalid.push(damaged);
        }
        let mut duplicated = root.clone();
        let target = duplicated["purpose"]["plan"]["targets"][0].clone();
        duplicated["purpose"]["plan"]["targets"]
            .as_array_mut()
            .unwrap()
            .push(target);
        invalid.push(duplicated);
        for value in invalid {
            assert!(serde_json::from_value::<Record>(value).is_err());
        }
        for kind in [
            crate::RecordKind::NoteTurnDecided,
            crate::RecordKind::TaughtNoteTurnDecided,
        ] {
            let record = crate::record::samples::record(kind);
            let mut value = serde_json::to_value(&record).unwrap();
            assert_eq!(
                serde_json::from_value::<Record>(value.clone()).unwrap(),
                record
            );
            value["decision"] = json!({"kind":"exhausted","reason":"mail_wake_limit"});
            assert!(serde_json::from_value::<Record>(value).is_err());
        }
    }
    fn emit(row: &mut MailWakeState, context: &mut MailWakeContext, position: u64, record: Record) {
        context.receipt_exists = matches!(&record, Record::Mail { id, .. } if context.mails.iter().any(|mail| mail.reference.id == *id));
        let lsn = Lsn(position);
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record,
        };
        *row = row.apply(actor(), context, 0, lsn, &entry).unwrap().state;
        match &entry.record {
            Record::Mail { id, state, .. } => context.mails.push(MailWakeMail::from_state(
                MailRef { id: *id, lsn },
                state,
                lsn,
            )),
            Record::MailState { id, state } => {
                let mail = context
                    .mails
                    .iter_mut()
                    .find(|mail| mail.reference.id == *id)
                    .unwrap();
                *mail = MailWakeMail::from_state(mail.reference, state, lsn);
            }
            _ => {}
        }
    }

    fn preparation_record(
        context: &MailWakeContext,
        unread_count: u8,
        original_count: u8,
        digest: u8,
    ) -> Record {
        assert_eq!(context.mails.len(), usize::from(unread_count));
        let task = MailWakeTask::new(unread_count).unwrap();
        Record::MailWakeTurnDecided(MailWakeDecision {
            agent: actor(),
            goal: goal(),
            completion_lsn: Lsn(1),
            purpose: MailWakePurpose::FreshPreparation {
                plan: MailWakePlan {
                    trigger: context.mails[0].reference,
                    frontier: context.mails.last().unwrap().reference.lsn,
                    unread_count,
                    task: task.clone(),
                    targets: context
                        .mails
                        .iter()
                        .map(|mail| MailWakeTarget {
                            mail: mail.reference,
                            original_count,
                            target_count: original_count + 1,
                        })
                        .collect(),
                },
            },
            input_digest: MailWakeDigest {
                domain: MailWakeDigestDomain::Fresh,
                value: Hex32::new([digest; 32]),
            },
            gate_trace: Gate::ORDER
                .into_iter()
                .map(|gate| GateStep {
                    gate,
                    fired: gate == Gate::Work,
                    note: None,
                })
                .collect(),
            decision: MailWakeAdmission::Issue { task },
        })
    }

    /// A goal installed over the agent's completed goal retires the wake candidate it owed, whether
    /// the goal arrives as a `GoalSet` or as a ticket's start.
    #[test]
    fn ticket_start_retires_a_wake_candidate() {
        let goal_set = crate::record::samples::record(crate::RecordKind::GoalSet);
        let Record::GoalSet {
            goal: installed, ..
        } = goal_set.clone()
        else {
            unreachable!("the GoalSet sample is a GoalSet");
        };
        let carriers = [
            Record::GoalSet {
                agent: actor(),
                goal: installed.clone(),
                browser_onboarding: None,
                browser_goal: None,
                goal_replacement: None,
            },
            Record::TicketStarted {
                card: dudes_types::CardId::from_parts(9, 9),
                agent: actor(),
                goal: installed,
                expected_seq: dudes_types::Seq(0),
            },
        ];
        for carrier in carriers {
            let mut row = MailWakeState::default();
            let mut context = MailWakeContext {
                completion: Some((goal(), Lsn(1))),
                ..MailWakeContext::default()
            };
            emit(&mut row, &mut context, 2, mail(1));
            assert!(row.candidate.is_some(), "the arrival owes a wake");
            emit(&mut row, &mut context, 3, carrier.clone());
            assert!(row.candidate.is_none(), "{carrier:?} retires the candidate");
        }
    }

    #[test]
    fn retired_terminal_target_refuses_stale_notification() {
        let mut row = MailWakeState::default();
        let mut context = MailWakeContext {
            completion: Some((goal(), Lsn(1))),
            ..MailWakeContext::default()
        };
        emit(&mut row, &mut context, 2, mail(1));
        let preparation = preparation_record(&context, 1, 0, 5);
        emit(&mut row, &mut context, 3, preparation);
        assert_eq!(row.preparation.as_ref().unwrap().root_lsn, Lsn(3));
        let id = context.mails[0].reference.id;
        emit(
            &mut row,
            &mut context,
            4,
            Record::MailState {
                id,
                state: MailState::Acked,
            },
        );
        assert!(row.preparation.is_none());
        assert_eq!(row.covered_frontier, Lsn(2));
        assert_eq!(row.last_lsn, Lsn(4));
        assert_eq!(context.mails[0].terminal_lsn, Some(Lsn(4)));
        assert_eq!(context.mails[0].count, None);
        let before = row.clone();
        let result = row.apply(
            actor(),
            &context,
            0,
            Lsn(5),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record: Record::MailState {
                    id,
                    state: MailState::Notified { n: 1 },
                },
            },
        );
        assert_eq!(row, before);
        assert!(
            matches!(result, Err(MailWakeError::Progress)),
            "the retired required preparation must preserve its canonical terminal target: {result:?}"
        );
    }

    #[test]
    fn legacy_without_preparation_keeps_notification_state_meaning() {
        let mut row = MailWakeState::default();
        let mut context = MailWakeContext::default();
        emit(&mut row, &mut context, 2, mail(1));
        let id = context.mails[0].reference.id;
        emit(
            &mut row,
            &mut context,
            3,
            Record::MailState {
                id,
                state: MailState::Acked,
            },
        );
        assert_eq!(row.covered_frontier, Lsn::ZERO);
        assert_eq!(context.mails[0].terminal_lsn, Some(Lsn(3)));
        emit(
            &mut row,
            &mut context,
            4,
            Record::MailState {
                id,
                state: MailState::Notified { n: 1 },
            },
        );
        assert_eq!(row.covered_frontier, Lsn::ZERO);
        assert_eq!(context.mails[0].count, Some(1));
        assert_eq!(context.mails[0].terminal_lsn, None);
    }

    #[test]
    fn post_frontier_and_unknown_receipts_keep_legacy_notification_meaning() {
        let mut row = MailWakeState::default();
        let mut context = MailWakeContext {
            completion: Some((goal(), Lsn(1))),
            ..MailWakeContext::default()
        };
        emit(&mut row, &mut context, 2, mail(1));
        let preparation = preparation_record(&context, 1, 0, 5);
        emit(&mut row, &mut context, 3, preparation);
        let old = context.mails[0].reference.id;
        emit(
            &mut row,
            &mut context,
            4,
            Record::MailState {
                id: old,
                state: MailState::Acked,
            },
        );
        assert_eq!(row.covered_frontier, Lsn(2));
        emit(&mut row, &mut context, 5, mail(2));
        let later = context.mails[1].reference.id;
        emit(
            &mut row,
            &mut context,
            6,
            Record::MailState {
                id: later,
                state: MailState::Acked,
            },
        );
        emit(
            &mut row,
            &mut context,
            7,
            Record::MailState {
                id: later,
                state: MailState::Notified { n: 1 },
            },
        );
        assert_eq!(context.mails[1].count, Some(1));
        assert_eq!(row.covered_frontier, Lsn(2));
        let result = row
            .apply(
                actor(),
                &context,
                0,
                Lsn(8),
                &Entry {
                    at: Timestamp::UNIX_EPOCH,
                    record: Record::MailState {
                        id: MailId::from_parts(2, 999),
                        state: MailState::Notified { n: 1 },
                    },
                },
            )
            .unwrap();
        assert!(!result.issued);
        assert_eq!(result.state.covered_frontier, Lsn(2));
        assert_eq!(result.state.candidate, row.candidate);
    }

    #[test]
    fn derived_progress_shape_requires_exact_target_length() {
        let mut row = MailWakeState::default();
        let mut context = MailWakeContext {
            completion: Some((goal(), Lsn(1))),
            ..MailWakeContext::default()
        };
        for index in 1..=2 {
            emit(&mut row, &mut context, index + 1, mail(index));
        }
        let preparation = preparation_record(&context, 2, 0, 9);
        emit(&mut row, &mut context, 4, preparation);
        let pending = row.clone();
        let original_context = context.clone();
        let entry = Entry {
            at: Timestamp::UNIX_EPOCH,
            record: Record::MailState {
                id: context.mails[1].reference.id,
                state: MailState::Notified { n: 1 },
            },
        };
        let positive = pending
            .apply(actor(), &original_context, 0, Lsn(5), &entry)
            .unwrap();
        assert!(matches!(
            positive.state.preparation.as_ref().unwrap().progress[1],
            MailWakeProgress::Notified { lsn: Lsn(5) }
        ));
        emit(&mut row, &mut context, 5, entry.record.clone());
        emit(
            &mut row,
            &mut context,
            6,
            Record::MailState {
                id: original_context.mails[0].reference.id,
                state: MailState::Notified { n: 1 },
            },
        );
        assert!(row.ready(), "the exact two-target positive is ready");

        let mut rejected = Vec::new();
        let mut results = Vec::new();
        for (name, length) in [("short", 1), ("long", 3)] {
            let mut value = serde_json::to_value(&pending).unwrap();
            let progress = value["preparation"]["progress"].as_array_mut().unwrap();
            progress.resize(
                length,
                serde_json::to_value(MailWakeProgress::Pending).unwrap(),
            );
            let damaged: MailWakeState = serde_json::from_value(value).unwrap();
            let applied = std::panic::catch_unwind(|| {
                damaged.apply(actor(), &original_context, 0, Lsn(5), &entry)
            });
            let refused_without_panic = matches!(
                &applied,
                Ok(Err(MailWakeError::Progress | MailWakeError::Contract))
            );
            let duplicate = std::panic::catch_unwind(|| {
                damaged.apply(actor(), &original_context, 0, Lsn(4), &entry)
            });
            let duplicate_refused = matches!(&duplicate, Ok(Err(MailWakeError::Progress)));
            let mut value = serde_json::to_value(&row).unwrap();
            value["preparation"]["progress"]
                .as_array_mut()
                .unwrap()
                .resize(
                    length,
                    serde_json::to_value(MailWakeProgress::Notified { lsn: Lsn(6) }).unwrap(),
                );
            let damaged_ready: MailWakeState = serde_json::from_value(value).unwrap();
            let ready = damaged_ready.ready();
            rejected.push(refused_without_panic && duplicate_refused && !ready);
            results.push((name, length, applied, duplicate, ready));
        }
        assert!(
            rejected.into_iter().all(|value| value),
            "each derived shape mismatch must refuse without panic and cannot be ready: {results:?}"
        );
    }

    #[test]
    fn consuming_beat_watermark_tracks_only_owed_earlier_crossings() {
        let mut row = MailWakeState::default();
        let mut context = MailWakeContext {
            completion: Some((goal(), Lsn(1))),
            ..MailWakeContext::default()
        };
        emit(
            &mut row,
            &mut context,
            1,
            beat(actor(), vec![BreakerSignal::UnackedMail]),
        );
        assert_eq!((row.fifth_crossing, row.fifth_consumed), (Lsn(0), Lsn(0)));
        for index in 1..=3 {
            emit(&mut row, &mut context, index + 1, mail(index));
        }
        for index in 1..=3 {
            emit(
                &mut row,
                &mut context,
                index + 4,
                Record::MailState {
                    id: MailId::from_parts(2, u128::from(index)),
                    state: MailState::Notified { n: 4 },
                },
            );
        }
        let preparation = preparation_record(&context, 3, 4, 1);
        emit(&mut row, &mut context, 8, preparation);
        for index in 1..=2 {
            emit(
                &mut row,
                &mut context,
                index + 8,
                Record::MailState {
                    id: MailId::from_parts(2, u128::from(index)),
                    state: MailState::Notified { n: 5 },
                },
            );
        }
        emit(
            &mut row,
            &mut context,
            11,
            beat(AgentId::from_parts(1, 9), vec![BreakerSignal::UnackedMail]),
        );
        emit(&mut row, &mut context, 12, beat(actor(), vec![]));
        assert_eq!((row.fifth_crossing, row.fifth_consumed), (Lsn(10), Lsn(0)));
        emit(
            &mut row,
            &mut context,
            13,
            beat(actor(), vec![BreakerSignal::UnackedMail]),
        );
        assert_eq!((row.fifth_crossing, row.fifth_consumed), (Lsn(10), Lsn(13)));
        emit(
            &mut row,
            &mut context,
            14,
            beat(actor(), vec![BreakerSignal::UnackedMail]),
        );
        assert_eq!(row.fifth_consumed, Lsn(13));
        emit(
            &mut row,
            &mut context,
            15,
            Record::MailState {
                id: MailId::from_parts(2, 3),
                state: MailState::Notified { n: 5 },
            },
        );
        assert_eq!((row.fifth_crossing, row.fifth_consumed), (Lsn(15), Lsn(13)));
        assert!(!row.ready());
        context.retired = true;
        emit(
            &mut row,
            &mut context,
            16,
            beat(actor(), vec![BreakerSignal::UnackedMail]),
        );
        assert_eq!(row.fifth_consumed, Lsn(13));
        context.retired = false;
        emit(
            &mut row,
            &mut context,
            17,
            beat(actor(), vec![BreakerSignal::UnackedMail]),
        );
        assert_eq!((row.fifth_crossing, row.fifth_consumed), (Lsn(15), Lsn(17)));
        assert!(row.ready());
    }
}
