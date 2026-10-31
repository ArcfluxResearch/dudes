//! Decision and issue authority for a turn that wake-worthy mail starts for an idle chat agent.

use dudes_types::{AgentId, Lsn, ModelSelection, SessionId, ThreadId, TurnId};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize};

use crate::mail_wake::{
    CarrierSchema, DigestSchema, MailRef, ModelSchema, TRACE_NOTE_BYTES, TraceSchema,
    bounded_model, bounded_session, bounded_trace, positive_lsn,
};
use crate::record::Hex32;
use crate::{ExhaustedReason, Gate, GateStep, HoldReason};

/// The most mail one mail turn covers.
pub const MAX_COVERED_MAIL: usize = 32;

/// The exact task of a mail turn: the agent's line, the covered-mail count, then the teaching.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MailTurnTask(#[serde(deserialize_with = "exact_task")] String);

impl MailTurnTask {
    /// Build the exact task for a covered-mail count in one through thirty-two.
    #[must_use]
    pub fn new(line: &crate::task::IdentityLine, covered: u8) -> Option<Self> {
        crate::task::identified_mail_turn_task(line, covered).map(Self)
    }

    /// The immutable launch bytes.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The count of covered mail this task names.
    #[must_use]
    pub fn covered_count(&self) -> u8 {
        crate::task::recognize_identified_mail_turn_task(&self.0, None)
            .map(|(_, count)| count)
            .expect("validated mail turn task")
    }

    /// Whether the task opens with the identity line of `agent`.
    #[must_use]
    pub fn names_agent(&self, agent: AgentId) -> bool {
        crate::task::recognize_identified_mail_turn_task(&self.0, Some(agent)).is_some()
    }
}

fn exact_task<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let task = String::deserialize(d)?;
    if crate::task::recognize_identified_mail_turn_task(&task, None).is_none() {
        return Err(serde::de::Error::custom("invalid exact mail turn task"));
    }
    Ok(task)
}

impl JsonSchema for MailTurnTask {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "MailTurnTask".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        use crate::task::TaskCommandSet;
        let notice = "\\[(?:[1-9]|[12][0-9]|3[0-2]) new mail\\]";
        let patterns = [TaskCommandSet::Legacy, TaskCommandSet::ScopedFileMail].map(|commands| {
            format!(
                "{}{notice}{}$",
                crate::task::schema_identity_prefix(commands),
                crate::task::schema_current_teaching_suffix(commands)
            )
        });
        let unconfined = format!(
            "{}{notice}{}$",
            crate::task::schema_unconfined_identity_prefix(),
            crate::task::schema_current_teaching_suffix(TaskCommandSet::Legacy)
        );
        schemars::json_schema!({"type":"string","pattern":format!("(?:{}|{}|{})", patterns[0], patterns[1], unconfined)})
    }
}

/// What the admission gates decided for one thread's wake-worthy mail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MailTurnAdmission {
    /// Start a turn with this task.
    Issue {
        /// The exact task.
        task: MailTurnTask,
    },
    /// An operator question stands.
    Wait {
        /// The standing question's position.
        #[serde(deserialize_with = "positive_lsn")]
        #[schemars(range(min = 1))]
        question_lsn: Lsn,
    },
    /// A control or provenance gate refuses.
    Hold {
        /// Why.
        reason: HoldReason,
    },
    /// A ceiling, the breaker or a mail turn cap refuses.
    Exhausted {
        /// Which limit.
        reason: ExhaustedReason,
    },
}

/// Required decision authority for one mail turn, separate from every other decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "DecisionFields", deny_unknown_fields)]
#[schemars(!try_from)]
pub struct MailTurnDecision {
    /// The idle chat agent the mail is for.
    pub agent: AgentId,
    /// The conversation the covered mail belongs to.
    pub thread: ThreadId,
    /// The wake-worthy mail this decision covers, oldest first.
    #[schemars(length(min = 1, max = 32))]
    pub covered: Vec<MailRef>,
    /// Digest of what the mailbox inspection reported for the agent's inbox.
    #[schemars(with = "DigestSchema")]
    pub inspection_digest: Hex32,
    /// Digest of the complete admission input.
    #[schemars(with = "DigestSchema")]
    pub input_digest: Hex32,
    /// Ordered gate prefix, with only its final gate firing.
    #[schemars(with = "TraceSchema")]
    pub gate_trace: Vec<GateStep>,
    /// The verdict.
    pub decision: MailTurnAdmission,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionFields {
    agent: AgentId,
    thread: ThreadId,
    covered: Vec<MailRef>,
    inspection_digest: Hex32,
    input_digest: Hex32,
    #[serde(deserialize_with = "bounded_trace")]
    gate_trace: Vec<GateStep>,
    decision: MailTurnAdmission,
}

fn covered_valid(covered: &[MailRef]) -> bool {
    (1..=MAX_COVERED_MAIL).contains(&covered.len())
        && covered
            .windows(2)
            .all(|pair| (pair[0].lsn, pair[0].id) < (pair[1].lsn, pair[1].id))
}

impl MailTurnDecision {
    /// Whether its bounded payload, task, covered mail and firing trace agree.
    #[must_use]
    pub fn valid(&self) -> bool {
        if !covered_valid(&self.covered)
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
        let gate = self.gate_trace.last().expect("nonempty trace").gate;
        match &self.decision {
            MailTurnAdmission::Issue { task } => {
                gate == Gate::Work
                    && task.names_agent(self.agent)
                    && usize::from(task.covered_count()) == self.covered.len()
            }
            MailTurnAdmission::Wait { question_lsn } => {
                *question_lsn != Lsn::ZERO && gate == Gate::Human
            }
            MailTurnAdmission::Hold { reason } => {
                gate == Gate::Control
                    || (gate == Gate::Provenance && *reason == HoldReason::InferredTurnEnd)
            }
            MailTurnAdmission::Exhausted { reason } => {
                gate == Gate::Budget && *reason != ExhaustedReason::Hops
            }
        }
    }
}

impl TryFrom<DecisionFields> for MailTurnDecision {
    type Error = &'static str;
    fn try_from(value: DecisionFields) -> Result<Self, Self::Error> {
        let decision = Self {
            agent: value.agent,
            thread: value.thread,
            covered: value.covered,
            inspection_digest: value.inspection_digest,
            input_digest: value.input_digest,
            gate_trace: value.gate_trace,
            decision: value.decision,
        };
        if !decision.valid() {
            return Err("invalid mail turn decision contract");
        }
        Ok(decision)
    }
}

/// One managed issue carrying a mail turn's exact task and the mail it covers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "IssueFields", deny_unknown_fields)]
#[schemars(!try_from)]
pub struct MailTurnIssue {
    /// The idle chat agent.
    pub agent: AgentId,
    /// The single logical turn identity retained during crash recovery.
    pub turn: TurnId,
    /// The conversation the covered mail belongs to.
    pub thread: ThreadId,
    /// The mail this turn covers, oldest first.
    #[schemars(length(min = 1, max = 32))]
    pub covered: Vec<MailRef>,
    /// The exact task, persisted before launch.
    pub task: MailTurnTask,
    /// The announced session or an explicit null.
    #[schemars(with = "crate::chat::ExplicitNullable<CarrierSchema>")]
    pub session: Option<SessionId>,
    /// The recorded prelaunch model selection.
    #[schemars(with = "ModelSchema")]
    pub model_selection: ModelSelection,
    /// The admitting decision's position.
    #[schemars(range(min = 1))]
    pub lsn_of_decision: Lsn,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueFields {
    agent: AgentId,
    turn: TurnId,
    thread: ThreadId,
    covered: Vec<MailRef>,
    task: MailTurnTask,
    #[serde(deserialize_with = "bounded_session")]
    session: Option<SessionId>,
    #[serde(deserialize_with = "bounded_model")]
    model_selection: ModelSelection,
    #[serde(deserialize_with = "positive_lsn")]
    lsn_of_decision: Lsn,
}

impl MailTurnIssue {
    /// Whether the carrier stays bounded and its task names its agent and covered count.
    #[must_use]
    pub fn valid(&self) -> bool {
        covered_valid(&self.covered)
            && self.lsn_of_decision > self.covered[self.covered.len() - 1].lsn
            && usize::from(self.task.covered_count()) == self.covered.len()
            && self.task.names_agent(self.agent)
            && self
                .session
                .as_ref()
                .is_none_or(|id| id.as_str().len() <= crate::mail_wake::CARRIER_BYTES)
            && self
                .model_selection
                .selector()
                .is_none_or(|text| text.len() <= crate::mail_wake::CARRIER_BYTES)
    }
}

impl TryFrom<IssueFields> for MailTurnIssue {
    type Error = &'static str;
    fn try_from(value: IssueFields) -> Result<Self, Self::Error> {
        let issue = Self {
            agent: value.agent,
            turn: value.turn,
            thread: value.thread,
            covered: value.covered,
            task: value.task,
            session: value.session,
            model_selection: value.model_selection,
            lsn_of_decision: value.lsn_of_decision,
        };
        if !issue.valid() {
            return Err("invalid mail turn issue carrier");
        }
        Ok(issue)
    }
}
