//! The Dudes contract: every struct that crosses a process boundary, and nothing else.
//!
//! - [`Event`] / [`HookReply`] — the normalised hook wire between a provider CLI and the daemon.
//! - [`Entry`] / [`Record`] — the journal payload: a record stamped with `at`, and the record
//!   itself; the only persistent state.
//! - [`rpc`] — JSON-RPC 2.0 envelopes and the typed params and results of every method. Params
//!   fail closed and results do not; [`rpc`] states that distinction and its three edges.
//! - [`FloorProjection`] — what clients see; fed to the floor by `floor.subscribe`.
//! - [`providers`] — raw vendor payloads, one module per CLI, `#[serde(default)]` everywhere and
//!   never `deny_unknown_fields`: vendors add fields weekly.
//! - [`schema`] — exports the four JSON Schema documents under `schemas/` (`cargo xtask schemas`).
//!
//! Wire conventions: `snake_case` names; data-carrying enums are internally tagged (the journal
//! `Record` on `t`, everything else on `kind`); money is integer micro-dollars; ids are ULID
//! strings; timestamps are integer Unix milliseconds. Adding a field or a variant is a minor
//! change. Removing or renaming one is a major change and a new [`CONTRACT_VERSION`], with one
//! exception, now spent: **before the first release** a journal payload field or variant may be
//! removed within frame `ver = 1`, which is how `MailAct::Refuse` and `HookTokenMinted.port`
//! went, and how [`Entry`] took a *required* `at`. From the
//! first release on the rule holds without exception, and a nested enum that may grow carries a
//! catch-all variant so an older reader degrades to "not recognised" instead of refusing the
//! frame.

pub mod budget;
pub mod chat;
pub mod decision;
pub mod event;
pub mod git;
pub mod goal;
pub mod mail_wake;
pub mod messages;
pub mod projection;
pub mod providers;
pub mod question_history;
pub mod record;
pub mod reply;
pub mod rpc;
pub mod schema;
pub mod task;
pub mod ticket;
pub mod traces;

/// The current RPC contract major. `goal.replace` checks [`rpc::Request::v`] against it;
/// legacy methods do not gate on that field.
pub const CONTRACT_VERSION: u16 = 1;

pub use budget::{AgentBudget, BudgetScope, Ceiling, GoalBudget, Spend};
pub use decision::{
    AutomaticHold, ExhaustedReason, Gate, GateStep, HoldReason, LoopDecision, NoteAdmission,
};
pub use event::{Event, EventKind};
pub use git::{
    GIT_ID_DIGITS, GitAgainst, GitBranch, GitChangeKind, GitCommit, GitCommittedFile, GitHead,
    GitSection, GitState, GitView, GitWorkingChange, MAX_GIT_BRANCH_BYTES, MAX_GIT_BRANCHES,
    MAX_GIT_CHANGE_BYTES, MAX_GIT_COMMIT_BYTES, MAX_GIT_COMMITS, MAX_GIT_COMMITTED_BYTES,
    MAX_GIT_HEAD_BYTES, MAX_GIT_NAME_BYTES, MAX_GIT_PARENTS, MAX_GIT_PATH_BYTES,
    MAX_GIT_SUBJECT_BYTES, MAX_GIT_VIEW_BYTES,
};
pub use goal::{CheckBaseline, CheckBaselineKind, CompletionCheckRef, Evidence, Goal, Predicate};
pub use mail_wake::{
    MailRef, MailWakeAdmission, MailWakeDecision, MailWakeDigest, MailWakeDigestDomain,
    MailWakeIssue, MailWakePlan, MailWakePurpose, MailWakeTarget, MailWakeTask,
};
pub use messages::{
    MAX_MESSAGES, MAX_MESSAGES_BYTES, MAX_THREADS, MAX_THREADS_BYTES, MessageState, MessageView,
    ThreadView,
};
pub use projection::{
    ActivityAgent, ActivityCard, ActivityEvent, ActivityView, AgentView, CardTicket, CardView,
    FloorDelta, FloorProjection, GoalView, LiveActivityKind, LiveActivityView, MAX_ACTIVITY_BYTES,
    MAX_ACTIVITY_ROWS, MAX_RETIRED_AGENT_BYTES, MAX_RETIRED_AGENTS, MailView, PredicateOutcome,
    RetiredAgentView, WorktreeView, WorktreeViewState,
};
pub use question_history::{
    AnsweredQuestionView, MAX_AGENT_QUESTION_HISTORY, MAX_QUESTION_HISTORY,
    MAX_QUESTION_HISTORY_BYTES,
};
pub use record::{
    Actor, BodyPath, BrowserNoteAttribution, BrowserNoteDestination, ClosingPhase,
    ConditionedMethod, ConditionedTarget, ConversationAnswerAuthority, ConversationChargeScope,
    ConversationQuestionOrigin, Entry, ExitStatus, KillDeferredReason, KillReason, MailAct,
    MailFrom, MailTo, PayloadDigest, PayloadRef, PeerProvenance, PtyCloseOutcome, PtyWriteOutcome,
    Record, RecordKind, TurnOutcome, WorktreeRemoval,
};
pub use reply::{HookDecision, HookReply, VendorReply};
pub use rpc::hook::{HookToken, InvalidHookToken};
pub use task::{
    MAIL_INSTRUCTION, MEMORY_INSTRUCTION, NoteContract, TEACHING_BYTES, normalized_browser_note,
    recognize_taught_note_task, taught_note_task, valid_taught_admission, valid_taught_decision,
};
pub use ticket::{
    QueueClass, QueueKey, TicketCheck, TicketCheckResult, TicketReason, TicketRefusal, TicketRow,
    TicketWait, TicketWaiting,
};
pub use traces::{
    MAX_AGENT_TRACE_BYTES, MAX_AGENT_TRACES, MAX_TRACE_BYTES, MAX_TRACES, TraceOutcome,
    TurnTraceView,
};

// The vocabulary crate is re-exported so a client of the contract needs one dependency.
pub use dudes_types as types;

pub use chat::{
    ChatContextState, ChatContextView, ChatPendingState, ChatPendingView, ChatReason, ChatReply,
    ChatReplyCompleteness, ChatTurnState, ChatTurnView, ChatView, MAX_CHAT_BYTES, MAX_CHAT_CONTEXT,
    MAX_CHAT_CONTEXT_BYTES, MAX_CHAT_CONTEXT_PER_AGENT, MAX_CHAT_PENDING, MAX_CHAT_PENDING_BYTES,
    MAX_CHAT_PENDING_PER_AGENT, MAX_CHAT_REPLY_BYTES, MAX_CHAT_TEXT_BYTES, MAX_CHAT_TURN_BYTES,
    MAX_CHAT_TURN_BYTES_PER_AGENT, MAX_CHAT_TURNS, MAX_CHAT_TURNS_PER_AGENT,
    conversation_answer_task, normalized_chat_text,
};
