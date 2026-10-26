//! The pure loop of the Dudes supervisor (invariant 5: the loop is pure).
//!
//! [`decide()`] walks the seven gates — control → provenance → human →
//! done → budget → hops → work — over a [`LoopInput`] the supervisor assembled from values it has
//! already journaled, and returns the verdict with the trace of gates it consulted. [`admit()`]
//! is the budget gate on its own (invariant 4: budget is admission); [`beat()`] moves the breaker
//! one rung per beat; [`judge()`] is the rule that only an `Observed` pass is `Done`
//! (invariant 3).
//!
//! [`admit_completed_note()`] shares those admission gates but requests one note turn against
//! a durable original goal completion. Its [`NoteDecided`] verdict cannot complete a goal.
//!
//! Nothing here performs I/O, reads a clock or depends on a crate that acts. The layering gate
//! (`cargo xtask layering`) and `tests/purity.rs` enforce it; the proptests in `src/proptest.rs`
//! state the properties each gate protects.

pub mod breaker;
pub mod budget;
pub mod chat;
pub mod closing;
pub mod input;
pub mod r#loop;
pub mod mail;
pub mod predicate;
#[cfg(test)]
mod proptest;
// A live nested corpus member makes the purity walk's recursive arm observable.
#[cfg(test)]
mod purity_witness;

pub use breaker::{
    BreakerConfig, COMPACTION_EXEMPT, COMPACTION_GRACE_MS, COMPACTION_MAX_SUPPRESSED_BEATS,
    COMPACTION_TRAILING_GRACE_MS, beat,
};
pub use budget::{Admission, BudgetState, Ceilings, admit};
pub use chat::{
    BrowserNoteInput, CompletedNoteInput, CompletedNoteState, ConversationInput, NoteDecided,
};
pub use closing::{
    ClosingDecided, ClosingLoopInput, ClosingWorkChoice, ClosingWorkDeferral, closing_notice,
};
pub use input::{
    AgentInput, AnswerInput, CardInput, Control, Decided, DepartmentPeer, FloorInput, GoalInput,
    HeldCardContext, LoopInput, PredicateInput, ReverseDependent, WorkInput,
};
pub use r#loop::{
    admit_completed_note, admit_conversation, admit_mail_wake, closing_input, decide,
    decide_closing, decide_with_browser_note, decision, supplemental_context,
};
pub use mail::{MailWakeDecided, MailWakeFacts, MailWakeInput, MailWakeWork};
pub use predicate::judge;
// probe: rust-only change
