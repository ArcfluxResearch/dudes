//! The bounded browser-note read contract, emitted by every daemon run.

use dudes_types::{AgentId, GoalId, TurnId};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

pub(crate) struct ExplicitNullable<T>(std::marker::PhantomData<T>);

impl<T: JsonSchema> JsonSchema for ExplicitNullable<T> {
    fn inline_schema() -> bool {
        T::inline_schema()
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        format!("ExplicitNullable_{}", T::schema_name()).into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <Option<T>>::json_schema(generator)
    }
}

pub(crate) fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// The accepted Chat bound for `MAX_CHAT_TEXT_BYTES`.
pub const MAX_CHAT_TEXT_BYTES: usize = 4_096;

/// Trim ASCII edges and admit the bounded text shared by finite messages and answers.
#[must_use]
pub fn normalized_chat_text(text: &str) -> Option<&str> {
    let text = text.trim_matches([' ', '\t', '\r', '\n']);
    (!text.is_empty()
        && text.len() <= MAX_CHAT_TEXT_BYTES
        && !text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\t' | '\n')))
    .then_some(text)
}
/// The accepted Chat bound for `MAX_CHAT_PENDING_PER_AGENT`.
pub const MAX_CHAT_PENDING_PER_AGENT: usize = 20;
/// The accepted Chat bound for `MAX_CHAT_PENDING`.
pub const MAX_CHAT_PENDING: usize = 128;
/// The accepted Chat bound for `MAX_CHAT_PENDING_BYTES`.
pub const MAX_CHAT_PENDING_BYTES: usize = 131_072;
/// The accepted Chat bound for `MAX_CHAT_TURNS_PER_AGENT`.
pub const MAX_CHAT_TURNS_PER_AGENT: usize = 8;
/// The accepted Chat bound for `MAX_CHAT_TURNS`.
pub const MAX_CHAT_TURNS: usize = 64;
/// The accepted Chat bound for `MAX_CHAT_TURN_BYTES_PER_AGENT`.
pub const MAX_CHAT_TURN_BYTES_PER_AGENT: usize = 131_072;
/// The accepted Chat bound for `MAX_CHAT_TURN_BYTES`.
pub const MAX_CHAT_TURN_BYTES: usize = 524_288;
/// The accepted Chat bound for `MAX_CHAT_CONTEXT_PER_AGENT`.
pub const MAX_CHAT_CONTEXT_PER_AGENT: usize = 20;
/// The accepted Chat bound for `MAX_CHAT_CONTEXT`.
pub const MAX_CHAT_CONTEXT: usize = 128;
/// The accepted Chat bound for `MAX_CHAT_CONTEXT_BYTES`.
pub const MAX_CHAT_CONTEXT_BYTES: usize = 16_384;
/// The accepted Chat bound for `MAX_CHAT_BYTES`.
pub const MAX_CHAT_BYTES: usize = 671_744;

/// A closed refusal vocabulary, never a rendered provider error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChatReason {
    /// Fixed wire spelling `agent_absent`.
    AgentAbsent,
    /// Fixed wire spelling `agent_retired`.
    AgentRetired,
    /// Fixed wire spelling `halted`.
    Halted,
    /// Fixed wire spelling `unsupported_delivery`.
    UnsupportedDelivery,
    /// Fixed wire spelling `goal_absent`.
    GoalAbsent,
    /// Fixed wire spelling `queue_full`.
    QueueFull,
    /// Fixed wire spelling `paused`.
    Paused,
    /// Fixed wire spelling `hold`.
    Hold,
    /// Fixed wire spelling `open_question`.
    OpenQuestion,
    /// Fixed wire spelling `budget`.
    Budget,
    /// Fixed wire spelling `hops`.
    Hops,
    /// Fixed wire spelling `provenance`.
    Provenance,
    /// Fixed wire spelling `goal_changed`.
    GoalChanged,
    /// Fixed wire spelling `mode_changed`.
    ModeChanged,
    /// Fixed wire spelling `delivery_uncertain`.
    DeliveryUncertain,
    /// Fixed wire spelling `delivery_started`.
    DeliveryStarted,
    /// Fixed wire spelling `note_unknown`.
    NoteUnknown,
    /// Fixed wire spelling `note_cancelled`.
    NoteCancelled,
    /// A provider billing credential requires a configured spending ceiling.
    CeilingRequired,
    /// A prior vendor context was invalidated or an acknowledgement no longer matches.
    ContextChanged,
}

/// The state of one retained pending browser note.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChatPendingState {
    /// Fixed wire spelling `queued`.
    Queued,
    /// Fixed wire spelling `reserved`.
    Reserved,
    /// Fixed wire spelling `delivery_uncertain`.
    DeliveryUncertain,
}

/// The state of a Managed note-bearing turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChatTurnState {
    /// Fixed wire spelling `reserved`.
    Reserved,
    /// Fixed wire spelling `running`.
    Running,
    /// Fixed wire spelling `ended`.
    Ended,
    /// Fixed wire spelling `failed`.
    Failed,
    /// Fixed wire spelling `cancelled`.
    Cancelled,
    /// Fixed wire spelling `delivery_uncertain`.
    DeliveryUncertain,
    /// Fixed wire spelling `reissue_cancelled`.
    ReissueCancelled,
}

/// An Attached context emission, without a reply promise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChatContextState {
    /// Fixed wire spelling `sent_as_context`.
    SentAsContext,
}

/// Exact fields of the bounded `ChatPendingView` browser object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatPendingView {
    /// The `agent_id` value, with explicit null where applicable.
    pub agent_id: AgentId,
    /// The `note_lsn` value, with explicit null where applicable.
    pub note_lsn: String,
    /// The `goal_id` value, with explicit null where applicable.
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(with = "ExplicitNullable<GoalId>")]
    pub goal_id: Option<GoalId>,
    /// The `text` value, with explicit null where applicable.
    pub text: String,
    /// The `state` value, with explicit null where applicable.
    pub state: ChatPendingState,
    /// The `turn_id` value, with explicit null where applicable.
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(with = "ExplicitNullable<TurnId>")]
    pub turn_id: Option<TurnId>,
    /// The `cancellable` value, with explicit null where applicable.
    pub cancellable: bool,
    /// The `reason` value, with explicit null where applicable.
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(with = "ExplicitNullable<ChatReason>")]
    pub reason: Option<ChatReason>,
}

/// Exact fields of the bounded `ChatTurnView` browser object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatTurnView {
    /// A recorded recovery launch occurred. False does not prove there was no earlier delivery.
    pub reissued: bool,
    /// The `agent_id` value, with explicit null where applicable.
    pub agent_id: AgentId,
    /// The `turn_id` value, with explicit null where applicable.
    pub turn_id: TurnId,
    /// The `note_lsn` value, with explicit null where applicable.
    pub note_lsn: String,
    /// The `text` value, with explicit null where applicable.
    pub text: String,
    /// The `state` value, with explicit null where applicable.
    pub state: ChatTurnState,
    /// The `reply` value, with explicit null where applicable.
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(with = "ExplicitNullable<String>")]
    pub reply: Option<String>,
    /// The `truncated` value, with explicit null where applicable.
    #[serde(deserialize_with = "required_nullable")]
    #[schemars(with = "ExplicitNullable<bool>")]
    pub truncated: Option<bool>,
}

/// Exact fields of the bounded `ChatContextView` browser object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatContextView {
    /// The `agent_id` value, with explicit null where applicable.
    pub agent_id: AgentId,
    /// The `note_lsn` value, with explicit null where applicable.
    pub note_lsn: String,
    /// The `state` value, with explicit null where applicable.
    pub state: ChatContextState,
}

/// Exact fields of the bounded `ChatView` browser object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatView {
    /// The `enabled` value, with explicit null where applicable.
    pub enabled: bool,
    /// The `max_text_bytes` value, with explicit null where applicable.
    pub max_text_bytes: usize,
    /// The `pending` value, with explicit null where applicable.
    pub pending: Vec<ChatPendingView>,
    /// The `turns` value, with explicit null where applicable.
    pub turns: Vec<ChatTurnView>,
    /// The `context_sent` value, with explicit null where applicable.
    pub context_sent: Vec<ChatContextView>,
}

impl ChatView {
    /// An enabled view with no retained browser text or receipt.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            enabled: true,
            max_text_bytes: MAX_CHAT_TEXT_BYTES,
            pending: Vec::new(),
            turns: Vec::new(),
            context_sent: Vec::new(),
        }
    }
}

impl ChatPendingView {
    /// Serialized bytes reserved before append for the longest overlay state and reason.
    pub fn reserved_bytes(&self) -> Result<usize, serde_json::Error> {
        let mut worst = self.clone();
        worst.state = ChatPendingState::DeliveryUncertain;
        worst.reason = Some(ChatReason::UnsupportedDelivery);
        worst.goal_id = Some(GoalId::NIL);
        worst.turn_id = Some(TurnId::NIL);
        worst.cancellable = false;
        worst.note_lsn = u64::MAX.to_string();
        serde_json::to_vec(&worst).map(|bytes| bytes.len())
    }
}

/// Maximum retained UTF-8 bytes in a final assistant reply.
pub const MAX_CHAT_REPLY_BYTES: usize = 65_536;

/// Whether the final reply producer retained all attributable assistant text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChatReplyCompleteness {
    /// Entire reply retained.
    Complete,
    /// A bounded prefix was retained.
    Truncated,
}

/// Final reply evidence, separate from a process outcome or goal completion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChatReply {
    /// Attributable assistant text, bounded to 65,536 UTF-8 bytes.
    #[serde(deserialize_with = "bounded_reply_text")]
    pub text: String,
    /// Explicit producer completeness.
    pub completeness: ChatReplyCompleteness,
}

fn bounded_reply_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let text = String::deserialize(deserializer)?;
    if text.len() > MAX_CHAT_REPLY_BYTES {
        return Err(serde::de::Error::custom(
            "final reply exceeds 65536 UTF-8 bytes",
        ));
    }
    Ok(text)
}

/// Compose a finite continuation addressed to one explicitly answered question.
#[must_use]
pub fn conversation_answer_task(question: &str, answer: &str) -> String {
    let mut end = question.len().min(1024);
    while !question.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "[answer to your question]\nquestion: {}\nanswer:\n{answer}\n\n[The question is answered. Continue your reply without asking it again.]",
        &question[..end]
    )
}
