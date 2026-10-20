//! The supervisor's own fold of confirmed entries, and the `LoopInput` it assembles from them.
//!
//! `dudes-supervisor` does not depend on `dudes-journal`: reading redb inside `handle()` is I/O,
//! and the crate is sans-io by rule. So it keeps this — a projection in the sense of invariant 1,
//! rebuildable, folded from entries that have actually reached the disk. `Input::Replayed` and
//! `Input::Persisted` call the *same* [`World::apply`], which is what makes a replayed decision
//! equal to the live one and the crash test meaningful rather than decorative.
//!
//! No spend arithmetic or ordinary display context lives here. Canonical mail-wake metadata
//! qualifies completed receipts and validates issued authority without retaining content. The fold holds
//! agent, goal and turn lifecycle, and the agent-token hashes a presented token resolves against
//! ([`World::agent_for_token_hash`]); the money arrives on
//! [`Input::Spend`](crate::Input::Spend), computed by the caller from the budget projections, and gate 7's card, mail count and memory digest arrive the same way on
//! [`Input::Work`](crate::Input::Work). The supervisor could not read any of them either way, so
//! there is nothing to duplicate — one rule, one implementation.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

#[cfg(test)]
mod attached_turn_end_tests;
mod mail_wakes;
mod mcp;
mod recovery;
mod worktrees;

pub use mcp::{McpGrant, McpHistoryError, McpPolicy};
pub use worktrees::{TrustGrant, WorktreeRow, WorktreeState};

use dudes_policy::{
    AgentInput, AnswerInput, Control, FloorInput, GoalInput, LoopInput, PredicateInput, WorkInput,
};
use dudes_proto::instruction::{
    InstructionBaselineRef, InstructionBoundaryAbsenceProof, InstructionNativeBoundaryReceipt,
    InstructionObservationOwner, InstructionTicketAssociation,
};
use dudes_proto::{
    AgentBudget, BrowserNoteDestination, Entry, GateStep, Goal, LoopDecision, NoteAdmission,
    NoteContract, Predicate, Record, TurnOutcome,
};
use dudes_types::{
    AgentId, AgentStatus, BreakerRung, CardId, GoalId, Lsn, ModelSelection, Provenance, Provider,
    SessionId, TurnId, TurnStatus, Usage, Usd, WorktreeId,
};

use crate::host::AdoptionKey;
use crate::input::SpendView;

/// How many undelivered steer notes one agent may hold before the oldest is dropped.
///
/// **The fold does not enforce this; the producer does.** Enforcing it here would mean dropping a
/// note inside [`World::apply`], and a drop that changes what the fold holds must be a record
/// (invariant 1) — which a fold cannot append, having no effects. So
/// `Supervisor::on_command(Command::Steer)` persists a `SteerCleared` before the `Steer` that
/// displaces it, and the fold below is append-and-remove with no eviction arm. The two paths then
/// see one record sequence and cannot disagree.
pub const MAX_PENDING_STEERS: usize = 20;

/// One agent, as the confirmed entries describe it.
///
/// The four flags are four separate journaled facts — `AgentRetired`, `Halt`, `Pause` and
/// `Attached` — read by different gates and cleared by different records. Packing them into one
/// enum would claim they are mutually exclusive, and a halted agent that is also attached is an
/// ordinary state.
#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct AgentRow {
    /// Display name.
    pub name: String,
    /// Authored cast character. Absence preserves the automatic appearance.
    pub character: Option<dudes_types::CastCharacter>,
    /// Named accent colour. Absence preserves the neutral accent.
    pub accent: Option<dudes_types::AccentColour>,
    /// Role text.
    pub role: String,
    /// Newest appearance edit, used to reject a stale browser edit. Absent before the first edit.
    pub look_lsn: Option<Lsn>,
    /// Department.
    pub department: String,
    /// Which CLI drives it.
    pub provider: Provider,
    /// The repository it works in.
    pub cwd: PathBuf,
    /// Daily ceiling.
    pub budget: AgentBudget,
    /// The provider's sandbox on-switch, as the hire asked for it.
    pub sandbox: Option<bool>,
    /// The vendor-run turn cap recorded at hire, or `None` for no cap. Replayed,
    /// never re-read from configuration, so a re-issue carries what the hire recorded.
    pub max_turns: Option<u32>,
    /// The vendor-run budget backstop recorded at hire, or `None` for none. Not a
    /// spend ceiling and no part of gate 5's admission — see `Record::AgentHired::max_budget`.
    pub max_budget: Option<Usd>,
    /// Whether the operator vouched for the repository at hire.
    pub trusted: bool,
    /// The lease this agent was hired into, when it was hired into a worktree.
    pub worktree: Option<WorktreeId>,
    /// Desired managed-launch names and the immutable recorded hire origin.
    pub mcp: McpPolicy,
    /// `AgentRetired` was appended.
    pub retired: bool,
    /// `loop.halt` is standing (gate 1).
    pub halted: bool,
    /// `loop.pause` is standing (gate 1).
    pub paused: bool,
    /// A human holds the keyboard (gate 1).
    pub attached: bool,
    /// The agent's control identity: where its newest `Pause`, `Resume`, `Halt` or `AgentHeld` sits, local or
    /// browser, older format or not. `None` until one is journaled. A Floor pause or resume applies
    /// only while the page presents this value.
    pub control_lsn: Option<Lsn>,
    /// Standing automatic cause, cleared by Resume or a new goal.
    pub automatic_hold: Option<dudes_proto::AutomaticHold>,
    /// First and latest consecutive qualifying failures of the current goal.
    pub fast_failures: Option<(TurnId, TurnId)>,
    /// The isolated beat for this failure episode is durable.
    pub rapid_failure_beat: bool,
    /// A current-goal launch failure whose automatic hold is not yet durable.
    pub pending_launch_failure: Option<dudes_proto::AutomaticHold>,
    /// A `HumanQuestion` without a `HumanAnswer` (gate 3).
    pub open_question: Option<String>,
    /// Where the `HumanQuestion` that opened [`AgentRow::open_question`] sits — the question's
    /// identity, which the Floor's answer names. Set and cleared with it.
    pub question_lsn: Option<Lsn>,
    /// Exact finite-turn origin when the current question belongs to a conversation.
    pub conversation_question: Option<dudes_proto::ConversationQuestionOrigin>,
    /// Answers that closed this agent's questions and that no spawned turn has carried yet,
    /// oldest first.
    ///
    /// **Delivered only by a `TurnSpawned`**, and only the answers its own `TurnIssued` listed: a
    /// turn that is never issued, never spawned or refused at launch leaves them here for the next
    /// one. The one other removal is a `TurnIssued` written before answers were
    /// recorded retires, undelivered, those decided before it. Removal is by position, so folding
    /// the same spawn twice removes nothing more. Like
    /// [`AgentRow::steers`], nothing durable holds this: it is the fold of the answer records.
    pub pending_answers: VecDeque<PendingAnswer>,
    /// Provenance of the highest-`lsn` `TurnEnded` for this agent (gate 2).
    pub last_turn_end: Option<Provenance>,
    /// The goal it pursues; `None` before `goal.set` and after `goal.clear`.
    pub goal: Option<GoalId>,
    /// Latest goal or vendor-context revision, initially the hire position.
    pub context_lsn: Lsn,
    /// Current breaker rung (gate 5).
    pub rung: BreakerRung,
    /// Operator corrections waiting to be delivered, oldest first, at most
    /// [`MAX_PENDING_STEERS`] of them.
    ///
    /// **A queue and not a slot, and each note is named by where it was journaled.** The `Lsn` is
    /// the position of the [`Record::Steer`] that queued it, which is what a later
    /// `SteerConsumed` or `SteerCleared` names — so removal is by identity rather than by
    /// position in the queue, and folding the same removal twice is a no-op. A slot would let a
    /// second correction destroy an undelivered first, and an index would let a concurrent
    /// removal take the wrong note.
    ///
    /// Bounded without a new cap on memory: one note is already capped at
    /// `dudes_rpc::conditioned::MAX_AGENT_STEER_TEXT_BYTES` (64 KiB), so an agent's queue is at
    /// most 1.25 MiB, here in the in-memory fold only. Nothing durable holds it: the redb
    /// `AgentRow` has no steer column, so `PROJECTION_VERSION` is untouched by this.
    pub steers: VecDeque<(Lsn, String)>,
    /// The vendor-confirmed session for this agent: the id the vendor named back to us, and the
    /// only one a later turn may resume. `SessionLearned` is its only establishing
    /// writer; a session-invalidating record and a replayed launch refusal may clear it. Announcing an
    /// id the daemon minted does not confirm it, so this stays `None` until the vendor speaks.
    pub session: Option<SessionId>,
    /// Actual confirmation that established the current agent session.
    pub session_lsn: Option<Lsn>,
    /// The turn in flight, when there is one.
    pub open_turn: Option<TurnId>,
    /// `input_digest` from this agent's **most recent** `LoopDecided`, and `None` before its first
    /// one.
    ///
    /// What the tick's re-examination of an `Exhausted` agent compares against. A projection like
    /// everything else here, which is what makes the guard survive a restart without a second
    /// mechanism and lets `dudes why` answer "why has nothing happened since 3am" out of the
    /// journal.
    ///
    /// Carries whatever the record carries, empty string included: the machine journals an empty
    /// `input_digest` when the input could not be serialised (`Supervisor::decide`), and this field
    /// is that record's field rather than a second opinion about it.
    pub last_input_digest: Option<String>,
    /// Why the newest decision for the agent's goal was `Exhausted`, and `None` after any other
    /// decision or when the goal changes. An exhausted goal is not finished: the loop decides it
    /// again when its limit resets.
    pub exhausted: Option<dudes_proto::ExhaustedReason>,
}

/// One answer waiting for a spawned turn to carry it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingAnswer {
    /// Where its `HumanAnswer` sits: what the carrying `TurnIssued.answers` names.
    pub lsn: Lsn,
    /// The question it closed, as it was asked.
    pub question: String,
    /// The answer, verbatim.
    pub text: String,
}

/// One goal, as the confirmed entries describe it.
#[derive(Clone, Debug)]
pub struct GoalRow {
    /// The goal itself.
    pub goal: Goal,
    /// Actual goal installation position.
    pub installed_lsn: Lsn,
    /// Who pursues it.
    pub agent: AgentId,
    /// Managed turn issues for this goal so far (gate 6), saturated at `u32::MAX`.
    /// Same-ID reissue and attached ends add no hop.
    pub turns_on_goal: u32,
    /// The latest `PredicateResult` (gate 4).
    pub predicate: Option<PredicateInput>,
    /// Actual latest predicate result position.
    pub predicate_lsn: Option<Lsn>,
    /// The most recent turn issued for this goal, so a predicate result names the turn it ran
    /// after.
    pub last_turn: Option<TurnId>,
    /// `GoalCleared` was appended.
    pub cleared: bool,
}

/// The review metadata one ticket goal's round holds: its baseline and the newest post-check
/// sample. Bounded payloads only; no file text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TicketReview {
    /// Position of the baseline record.
    pub baseline_lsn: Lsn,
    /// The round-start observation.
    pub baseline: dudes_proto::diff_stamp::BaselinePayload,
    /// Position and content of the newest post-check sample, once one is recorded.
    pub stamp: Option<(Lsn, dudes_proto::diff_stamp::StampPayload)>,
}

#[derive(Clone, Copy, Debug)]
struct ObservedPredicateResult {
    turn: TurnId,
    lsn: Lsn,
}

/// One turn, as the confirmed entries describe it.
#[derive(Clone, Debug)]
pub struct TurnRow {
    /// Actual immutable issue position.
    pub issue_lsn: Lsn,
    /// The agent.
    pub agent: AgentId,
    /// The goal it serves.
    pub goal: Option<GoalId>,
    /// Where it is in its life.
    pub status: TurnStatus,
    /// The latest id any of this turn's records named: `TurnIssued`'s, then any `TurnSpawned`,
    /// `SessionLearned` or `TurnEnded` that names one. A record naming none keeps it,
    /// so after a crash it can still hold the crashed launch's id. Not a resumable session — a
    /// re-issue reads [`AgentRow::session`], never this.
    pub session: Option<SessionId>,
    /// The session the turn's latest launch announced: `TurnIssued`'s, then each `TurnSpawned`'s,
    /// `None` where that launch announced nothing. Unlike [`TurnRow::session`], a launch that
    /// announced nothing replaces an earlier id.
    pub announced: Option<SessionId>,
    /// Whether this turn has a durable `SessionLearned`. The record is emitted only after the
    /// machine has checked that the vendor confirmed the session this launch announced, so this
    /// flag means "confirmed for this turn" rather than merely "some id was reported".
    pub session_confirmed: bool,
    /// The task text, kept so a re-issued turn launches the same work.
    pub task: String,
    /// The answers `TurnIssued` recorded as carried by [`TurnRow::task`]. Its `TurnSpawned`,
    /// first or re-issued, consumes exactly these.
    pub answers: Vec<Lsn>,
    /// The model selection `TurnIssued` recorded, kept for the same reason as the task: a
    /// re-issued turn launches under the model the journal says it was issued under, not under
    /// whatever the adapter is configured with now. `None` is a journal written
    /// before the field, and is the only case that falls back to current configuration.
    ///
    /// The observed per-model breakdown is deliberately **not** here: this crate decides nothing
    /// from it, and the run-local world carries only what a decision or a relaunch needs.
    /// It lives on the persistent turn row instead.
    pub model_selection: Option<ModelSelection>,
    /// The child's pid, once `TurnSpawned` recorded it.
    pub pid: Option<u32>,
    /// The child's platform-native start value.
    pub start_time: Option<u64>,
    /// The boot identity stamped at spawn.
    pub boot: Option<String>,
    /// Latest physical spawn receipt position, absent before a child was recorded.
    pub spawn_lsn: Option<Lsn>,
    /// Latest terminal source, including an end before a subsequent physical spawn.
    pub end_lsn: Option<Lsn>,
    /// Accepted confirmation source for the current physical attempt.
    pub confirmation_lsn: Option<Lsn>,
    /// Immutable grant paired with the latest physical spawn receipt.
    pub mcp_grant: Option<Lsn>,
}

/// A managed native spawn bound to one authoritative instruction-attempt start.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ManagedNativeSpawnReceipt {
    /// Exact managed owner captured by the starting record.
    pub owner: InstructionObservationOwner,
    /// Complete process identity recorded by this physical spawn.
    pub key: AdoptionKey,
    /// Actual position of this attempt's first physical spawn receipt.
    pub spawn_lsn: Lsn,
}

/// An attached native spawn bound to one exact attachment and instruction-attempt start.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AttachedNativeSpawnReceipt {
    /// Exact attached-process owner, including its actual attachment record position.
    pub owner: InstructionObservationOwner,
    /// Run-local PTY handle recorded by this physical spawn.
    pub pty: dudes_proto::rpc::pty::PtyId,
    /// Complete process identity recorded by this physical spawn.
    pub key: AdoptionKey,
    /// Actual position of this attempt's first physical spawn receipt.
    pub spawn_lsn: Lsn,
}

/// One actual observed Attached turn end awaiting its missing-start observation.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AttachedTurnEndReceipt {
    /// Exact hook-ended owner; its attachment identity is unrecorded and remains `None`.
    pub owner: InstructionObservationOwner,
    /// Canonical position of this turn's first observed end.
    pub end_lsn: Lsn,
    /// Actual current goal captured before the end was folded.
    pub goal: Option<GoalId>,
    /// Original current ticket-start association, independent of later card eligibility.
    pub ticket: Option<InstructionTicketAssociation>,
}

/// Whether a bounded query inspected all current unmatched Attached ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AttachedTurnEndQueryStatus {
    /// Every retained end fit; no instruction baseline, card eligibility or process absence is proved.
    Complete,
    /// Further current identities were retained but not inspected.
    Truncated {
        /// Number of current unmatched ends beyond the query bound.
        omitted: usize,
    },
}

impl TurnRow {
    /// A durable issue with no recorded spawn or end.
    /// This is owed work, not permission to launch or proof that no process ran.
    #[must_use]
    pub const fn is_issued_not_spawned(&self) -> bool {
        matches!(self.status, TurnStatus::Issued)
    }

    /// A child was recorded and no end was: the reaper asks about this one.
    #[must_use]
    pub const fn is_open_child(&self) -> bool {
        matches!(self.status, TurnStatus::Spawned)
    }

    /// A `TurnEnded` for this turn is fsynced and folded, whichever outcome it carried.
    ///
    /// The *persistent* half of "this turn is already ending". It cannot be the whole of it: the
    /// status moves on the acknowledged entry, so between an end being emitted and its
    /// acknowledgement this still answers `false` — which is exactly the crash window a second
    /// end would slip through. The run-local half lives on the machine.
    #[must_use]
    pub const fn has_ended(&self) -> bool {
        matches!(self.status, TurnStatus::Ended | TurnStatus::Killed)
    }

    /// The session this turn ends on: the one it confirmed, else the one its latest launch
    /// announced.
    #[must_use]
    pub fn ended_session(&self) -> Option<SessionId> {
        if self.session_confirmed {
            self.session.clone()
        } else {
            self.announced.clone()
        }
    }

    /// The key a `Kill` or a `Probe` names this turn's child by, when `TurnSpawned` recorded one.
    ///
    /// **The one place a key is rebuilt from the record**. A key is three fields
    /// that must travel together — the identifier, the opaque start value and the boot identity
    /// whose *presence* is itself part of the comparison — and a second site that assembled two of
    /// them and defaulted the third would reintroduce the false `Gone` that
    /// [`liveness()`](crate::host::liveness) exists to prevent.
    #[must_use]
    pub fn adoption_key(&self) -> Option<AdoptionKey> {
        Some(AdoptionKey {
            pid: self.pid?,
            start_time: self.start_time?,
            boot: self.boot.clone(),
        })
    }
}

/// Outcomes reached after the daemon read a managed child's stream through its end. The two
/// no-stream outcomes deliberately stay out: `Vanished` was observed by the reaper, and `Killed`
/// tests only that the daemon could stop the child.
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

/// The fold. Every field is derivable from the journal; nothing here is authoritative.
///
/// **`#[non_exhaustive]`, because a `World` a caller assembled is a `World` the journal never
/// produced.** Invariant 1 makes the journal the only writer and this struct a projection of it,
/// so the honest way to obtain one is [`World::default`] folded forward by [`World::apply`] —
/// the path `dudes replay` itself takes. A downstream struct expression would mint a projection
/// with no records behind it: a state no journal can emit and no replay can reproduce, carrying
/// whatever cross-field disagreement the caller happened to type. Closing the constructor is
/// therefore a property this type wants, not a cost it pays, and it should not rest on which
/// fields happen to be private this week.
///
/// Which is why the attribute and not just the private field that closed construction by
/// accident. `minted` is private because its key set is only half of token validity, and
/// making it private is what shuts the constructor, and adding a private field to a
/// constructible struct is a major break (`constructible_struct_adds_private_field`). A closure
/// resting on one field's visibility lasts exactly as long as that visibility: measured, making
/// `minted` public re-opens the struct to a downstream literal, and under this attribute it
/// stays shut. The attribute buys that for nothing — a struct with a private field and a
/// `#[non_exhaustive]` struct admit the same downstream forms (`Default`, public field reads,
/// patterns written with `..`) and refuse the same literals; only the diagnostic moves.
///
/// It does not undo the break it declares. A caller compiled against the struct while it was
/// open stays broken, and `cargo semver-checks` still requires a major version against any
/// pre-`minted` baseline — as `struct_marked_non_exhaustive` rather than the constructibility
/// lint. Only the version can settle that.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct World {
    recovery_state: dudes_proto::recovery_state::RecoveryState,
    recovery_history: dudes_proto::recovery_state::history::RecoveryHistory,
    recovery_history_error: Option<dudes_proto::recovery_state::history::RecoveryHistoryError>,
    recovery_card_changes: BTreeMap<CardId, Lsn>,
    recovery_last_retirement: Option<Lsn>,
    /// Durable instruction attempts and independently consumed review evidence.
    pub instructions: dudes_proto::instruction_state::InstructionState,
    /// Latest journal-derived closure; a completed session remains historical.
    pub closing: Option<crate::closing::ClosingSession>,
    /// Agents by id.
    pub agents: BTreeMap<AgentId, AgentRow>,
    active_folder_users: BTreeSet<AgentId>,
    open_turn_owners: BTreeSet<AgentId>,
    managed_native_spawns: BTreeMap<Lsn, ManagedNativeSpawnReceipt>,
    attached_native_spawns: BTreeMap<Lsn, AttachedNativeSpawnReceipt>,
    unmatched_attached_turn_ends: BTreeMap<Lsn, AttachedTurnEndReceipt>,
    /// Goals by id.
    pub goals: BTreeMap<GoalId, GoalRow>,
    observed_predicates: BTreeMap<GoalId, ObservedPredicateResult>,
    ticket_reviews: BTreeMap<GoalId, TicketReview>,
    ticket_goals: BTreeMap<GoalId, (CardId, u32)>,
    ticket_rounds: BTreeMap<CardId, u32>,
    /// Turns by id.
    pub turns: BTreeMap<TurnId, TurnRow>,
    mcp_grants: BTreeMap<Lsn, McpGrant>,
    /// The position of the last entry [`World::apply`] folded, or `None` before the first.
    ///
    /// The fold's own record of where it has reached, so that a record which has to be named by
    /// its position — a note removed by the `lsn` that set it — has a position to
    /// be named by. Recovery facts bound their source joins by this cut. It is not read by
    /// [`World::loop_input`] and never reaches `dudes_policy::decide()`, which reads no clock
    /// and no position (invariant 5).
    ///
    /// The same name as `dudes_journal::Projections::last_applied_lsn` because it is the same
    /// fact about the other projection of the same journal, and a second word for one fact is
    /// how a glossary rots. `Option` rather than that one's `Lsn::ZERO` sentinel is the one
    /// deliberate difference, and it is a typing decision rather than a value being reserved:
    /// [`Lsn`] derives `Default` and `Lsn::default()` is `Lsn::ZERO` (`dudes-types/src/id.rs`),
    /// so a bare field would leave [`World::default`] indistinguishable from a fold that stored
    /// `Lsn::ZERO` — which is exactly what a caller that dropped the position hands over. The
    /// `Option` makes "this fold has not started" a state every reader must handle rather than a
    /// value it has to recognise.
    pub last_applied_lsn: Option<Lsn>,
    /// The latest qualifying unconfirmed resume per agent, derived from the journal for
    /// `system.doctor` and read by no decision.
    unconfirmed_resumes: BTreeMap<AgentId, TurnId>,
    /// Which agent each journaled agent-token hash was minted for, keyed by the hash.
    ///
    /// **Keyed by the hash, not by the agent, and that is the whole of token validity.** A
    /// hash is valid by *set membership*, never by recency: the plaintext does not survive a
    /// restart, so a later run draws again for the same agent while a child adopted across that
    /// restart still presents the earlier token. A map `agent -> hash` would evict that child's
    /// credential and refuse its every call — after a crash only, for adopted children only,
    /// looking exactly like a permissions bug. Nothing is removed from here.
    ///
    /// **What a presented token resolves to is decided by `classify`**, cited and not restated —
    /// not by this field, which only feeds the honoured-token cell.
    /// `classify` decides which of the classification table's token rows a presented token
    /// selects; this map decides only whether a minting exists for its hash.
    ///
    /// **The one private field, because its key set is only part of the answer.** The rest is
    /// retirement and the retired agent's recorded children, which [`World::agent_for_token_hash`]
    /// applies and a caller reading the map directly would not. Membership without them cannot
    /// tell the table's token rows apart, so there are two doors and both apply them:
    /// [`World::agent_for_token_hash`] answers about one hash, and
    /// [`World::token_hash_owners`] answers about all of them at once for a reader on another
    /// thread, which is the one the daemon's `MintedTokens` adapter reads.
    minted: BTreeMap<String, AgentId>,
    /// Each attached agent's PTY child, by agent: the key its `PtySpawned` recorded, held until
    /// that agent's `Detached`. Keyed by agent and never by `PtyId`, which a
    /// later run reuses.
    attached_children: BTreeMap<AgentId, AdoptionKey>,
    /// Each session's running total as the journal last recorded it, over the turns whose
    /// session the vendor confirmed.
    session_totals: BTreeMap<SessionId, Usage>,
    /// Goals the loop has decided `Done`: the goal its agent held when each `LoopDecided::Done`
    /// was folded, the relation the journal's goals projection uses for `Achieved`. A goal here is
    /// not decided again, whoever holds it, so a replayed result that no longer passes cannot
    /// reopen it.
    completed_goals: BTreeMap<GoalId, Lsn>,
    browser_notes: BTreeMap<Lsn, BrowserNoteRow>,
    conversation_decisions: BTreeMap<AgentId, (Lsn, Lsn, Lsn, NoteAdmission, String)>,
    browser_issues: BTreeMap<TurnId, BrowserIssueRow>,
    note_decisions: BTreeMap<AgentId, NoteDecisionRow>,
    ordinary_continues: BTreeMap<Lsn, (AgentId, GoalId)>,
    mail_wakes: mail_wakes::Fold,
    /// Every lease the journal holds, by id, removed ones included.
    pub worktrees: BTreeMap<WorktreeId, WorktreeRow>,
    /// The projects the operator trusts, by project id: the latest `WorkspaceTrustSet` per id,
    /// dropped once that record says `trusted: false`.
    pub workspace_trust: BTreeMap<String, TrustGrant>,
}

/// Durable browser-note lifecycle, separate from legacy local steering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserNoteState {
    /// Awaiting an eligible delivery seam.
    Queued,
    /// Reserved by an exact immutable issue.
    Reserved {
        /// The carrying turn.
        turn: TurnId,
    },
    /// Consumed by a matched durable spawn.
    Delivered {
        /// The carrying turn.
        turn: TurnId,
    },
    /// Closed by an end without an observed spawn.
    Failed {
        /// The failed carrying turn.
        turn: TurnId,
    },
    /// Emitted through an Attached hook.
    ContextEmitted,
    /// Proven undelivered and durably cancelled.
    Cancelled,
    /// Future dispatch cancelled with possible past delivery.
    ReissueCancelled {
        /// The unresolved carrying turn.
        turn: TurnId,
    },
}

/// A journal-derived browser note, keyed by its queue lsn.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BrowserNoteRow {
    /// Owning agent.
    pub agent: AgentId,
    /// Complete normalized queued text.
    pub text: String,
    /// Continuation framing for an exact structured answer.
    pub task: Option<String>,
    /// The answered question identity gives continuations queue priority.
    pub answer_to: Option<Lsn>,
    /// Immutable admission destination.
    pub destination: BrowserNoteDestination,
    /// Durable delivery state.
    pub state: BrowserNoteState,
}

/// The durable cancellation disposition for an issued browser-note turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserIssueCancellation {
    /// Nonlaunch proven across every historical attempt.
    Clean {
        /// Cancellation position.
        lsn: Lsn,
    },
    /// Future dispatch stopped without claiming past nonlaunch.
    Ambiguous {
        /// Cancellation position.
        lsn: Lsn,
        /// The unresolved historical intent.
        dispatch_intent_lsn: Lsn,
    },
}

/// Exact issue ownership and every historical dispatch attempt.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BrowserIssueRow {
    /// Owning agent.
    pub agent: AgentId,
    /// Queued note identity.
    pub note_lsn: Lsn,
    /// Immutable issuance position.
    pub issue_lsn: Lsn,
    /// Original charged goal.
    pub goal: Option<GoalId>,
    /// Whether this task is a separately admitted completed-goal note.
    pub note_only: bool,
    /// Ordinary completion proof on note-only issuance.
    pub completion_lsn: Option<Lsn>,
    /// Raw or taught completed-note authority; absent for ordinary note-bearing work.
    pub contract: Option<NoteContract>,
    /// Every durable dispatch intent for this `TurnId`, in order.
    pub dispatch_intents: Vec<Lsn>,
    /// Prior dispatches already charged one conservative unknown-use reserve.
    pub uncertain_dispatches: Vec<Lsn>,
    /// Matching durable spawn observation.
    pub spawn_lsn: Option<Lsn>,
    /// Whether the latest recorded launch was a recovery attempt.
    pub reissued: bool,
    /// Matching end observation.
    pub end_lsn: Option<Lsn>,
    /// Durable clean or ambiguous cancellation tombstone.
    pub cancellation: Option<BrowserIssueCancellation>,
}

/// The latest separately typed completed-note admission for an agent.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct NoteDecisionRow {
    /// Decision position.
    pub lsn: Lsn,
    /// Original completed goal.
    pub goal: GoalId,
    /// Exact eligible note.
    pub note_lsn: Lsn,
    /// Ordinary Done proof.
    pub completion_lsn: Lsn,
    /// Completed-note input digest.
    pub input_digest: String,
    /// Ordered gate trace.
    pub gate_trace: Vec<GateStep>,
    /// Admission without a Done alternative.
    pub decision: NoteAdmission,
    /// The immutable payload contract admitted by this decision kind.
    pub contract: NoteContract,
}

impl World {
    /// Validate record authority held only by the committed World without changing it.
    ///
    /// Record shape, instruction history and current card admission still require their own checks.
    ///
    /// # Errors
    /// Rejects invalid final-reply authority, a mismatched native spawn receipt, or an unmatched
    /// Attached finish that changes its captured owner, end identity or supplied ticket association.
    pub fn validate_record_authority(&self, entry: &Entry) -> Result<(), &'static str> {
        if let Record::TurnEnded {
            final_reply: Some(reply),
            last_message,
            mode,
            provenance,
            ..
        } = &entry.record
            && (last_message.is_some()
                || reply.text.len() > dudes_proto::MAX_CHAT_REPLY_BYTES
                || *mode != dudes_types::Mode::Managed
                || *provenance != Provenance::Observed)
        {
            return Err("turn final reply lacks observed managed authority");
        }
        if let Record::InstructionAttemptBoundarySettled {
            owner,
            started_lsn,
            proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent { receipt },
        } = &entry.record
            && let Some((indexed_owner, indexed_spawn)) = self
                .managed_native_spawns
                .get(started_lsn)
                .map(|indexed| (indexed.owner, indexed.spawn_lsn))
                .or_else(|| {
                    self.attached_native_spawns
                        .get(started_lsn)
                        .map(|indexed| (indexed.owner, indexed.spawn_lsn))
                })
            && (*owner != indexed_owner
                || !matches!(receipt, InstructionNativeBoundaryReceipt::Spawned { lsn } if *lsn == indexed_spawn))
        {
            return Err("native boundary settlement does not match the first indexed spawn");
        }
        if let Record::InstructionObservationFinished {
            owner,
            ticket,
            baseline: InstructionBaselineRef::MissingTurnBoundary { turn, end_lsn },
            ..
        } = &entry.record
            && let Some(captured) = self.unmatched_attached_turn_ends.get(end_lsn)
            && matches!(
                captured.owner,
                InstructionObservationOwner::AttachedTurn {
                    attachment_lsn: None,
                    ..
                }
            )
        {
            if *owner != captured.owner
                || !matches!(captured.owner, InstructionObservationOwner::AttachedTurn { turn: actual, .. } if actual == *turn)
            {
                return Err("attached turn finish does not match the captured end owner");
            }
            if ticket.is_some() && *ticket != captured.ticket {
                return Err(
                    "attached turn finish ticket does not match the captured end association",
                );
            }
        }
        Ok(())
    }

    /// Fold one confirmed entry, appended at `lsn`.
    ///
    /// The `match` is exhaustive on `Record`, so a new variant fails to compile here rather than
    /// being silently ignored. Records the gates do not read yet get a named no-op arm, never a
    /// `_ =>`.
    ///
    /// The identical empty arms at the end are deliberately not merged: each names a different
    /// journaled fact and the reader it will get, and merging them would lose
    /// exactly that.
    ///
    /// `lsn` is the position of the frame the entry came out of, which the entry itself does not
    /// carry (`dudes_proto::Entry` is `at` plus the record). Both callers hold it — replay reads
    /// it from the log and the live path is acknowledged with it — so the two folds agree on the
    /// position of every entry, which is what lets a record be named by where it sits.
    pub fn apply(&mut self, lsn: Lsn, entry: &Entry) {
        let _ = self.apply_checked(lsn, entry);
    }

    /// Fold an entry only after its recovery relationships pass at the preceding cut.
    /// Callers must propagate refusal; successful historical folding grants no dispatch permission.
    #[allow(clippy::too_many_lines, clippy::match_same_arms)]
    pub fn apply_checked(
        &mut self,
        lsn: Lsn,
        entry: &Entry,
    ) -> Result<(), dudes_proto::recovery_state::RecoveryStateError> {
        use dudes_proto::recovery_state::RecoveryStateError;
        self.validate_record_authority(entry)
            .map_err(|_| RecoveryStateError::Binding)?;
        let prepared_recovery = self.prepare_recovery_record(lsn, entry)?;
        let mut recovery_context = self.recovery_before(entry);
        let attached_end = self.capture_attached_turn_end(lsn, entry);
        let Ok(instruction_fold) = self.instructions.apply_validated(lsn, entry, |bytes| {
            dudes_proto::record::Hex32::new(*blake3::hash(bytes).as_bytes())
        }) else {
            return Err(RecoveryStateError::Binding);
        };
        if let Some(prepared) = prepared_recovery {
            prepared.apply(lsn, &mut self.recovery_state, &self.recovery_history)?;
        }
        recovery_context.cleared_goal = instruction_fold.cleared_goal;
        if let Err(error) =
            self.recovery_history
                .observe_with_context(lsn, entry, recovery_context, recovery::hash)
        {
            self.recovery_history_error.get_or_insert(error);
        }
        self.capture_recovery_positions(lsn, entry);
        if let Some(captured) = attached_end
            && matches!(captured.owner, InstructionObservationOwner::AttachedTurn { turn, .. }
                if self.instructions.end_lsn(turn) == Some(captured.end_lsn))
        {
            self.unmatched_attached_turn_ends
                .entry(captured.end_lsn)
                .or_insert(captured);
        }
        self.retire_attached_turn_end(lsn, entry);
        if let Record::InstructionAttemptBoundarySettled { started_lsn, .. } = &entry.record {
            self.managed_native_spawns.remove(started_lsn);
            self.attached_native_spawns.remove(started_lsn);
        }
        if let Some(clear) = instruction_fold.cleared_goal {
            self.clear_goal(clear.agent, clear.goal, lsn);
        }
        let mut wakes = std::mem::take(&mut self.mail_wakes);
        wakes.apply(self, lsn, entry);
        self.mail_wakes = wakes;
        self.last_applied_lsn = Some(lsn);
        if matches!(
            entry.record,
            Record::ClosingTime {
                phase: dudes_proto::ClosingPhase::Complete
            }
        ) && self
            .closing
            .as_ref()
            .is_some_and(|session| !session.is_complete())
        {
            let managed = self
                .turns
                .iter()
                .filter_map(|(turn, row)| {
                    (row.is_open_child()
                        || (row.is_issued_not_spawned()
                            && !self.browser_issues.get(turn).is_some_and(|issue| {
                                matches!(
                                    issue.cancellation,
                                    Some(BrowserIssueCancellation::Clean { .. })
                                )
                            })))
                    .then_some(*turn)
                })
                .collect();
            let attached = self
                .agents
                .iter()
                .filter_map(|(agent, row)| row.attached.then_some(*agent))
                .collect();
            self.closing
                .as_mut()
                .expect("checked closing session")
                .complete_resources(managed, attached);
        }
        if matches!(
            entry.record,
            Record::ClosingTime {
                phase: dudes_proto::ClosingPhase::Announced
            }
        ) && self
            .closing
            .as_ref()
            .is_none_or(crate::closing::ClosingSession::is_complete)
        {
            self.closing = Some(crate::closing::ClosingSession::capture(self, lsn, entry.at));
        } else if let Some(closing) = self.closing.as_mut() {
            closing.fold(lsn, entry);
        }
        if let Some((agent, goal)) = entry.record.goal_install() {
            self.goals.insert(
                goal.id,
                GoalRow {
                    goal: goal.clone(),
                    installed_lsn: lsn,
                    agent,
                    turns_on_goal: 0,
                    predicate: None,
                    predicate_lsn: None,
                    last_turn: None,
                    cleared: false,
                },
            );
            if let Some(row) = self.agents.get_mut(&agent) {
                row.goal = Some(goal.id);
                row.context_lsn = lsn;
                row.automatic_hold = None;
                row.fast_failures = None;
                row.rapid_failure_beat = false;
                row.pending_launch_failure = None;
                row.exhausted = None;
            }
        }
        if let Record::TicketStarted { card, goal, .. } = &entry.record {
            let round = self.ticket_rounds.entry(*card).or_insert(0);
            *round = round.saturating_add(1);
            self.ticket_goals.insert(goal.id, (*card, *round));
        }
        match &entry.record {
            Record::ConversationDispatchUncertain {
                agent,
                turn,
                dispatch_intent_lsn,
                ..
            } => {
                if let Some(issue) = self.browser_issues.get_mut(turn)
                    && issue.agent == *agent
                    && issue.goal.is_none()
                    && issue.spawn_lsn.is_none()
                    && issue.end_lsn.is_none()
                    && issue.dispatch_intents.last() == Some(dispatch_intent_lsn)
                    && !issue.uncertain_dispatches.contains(dispatch_intent_lsn)
                {
                    issue.uncertain_dispatches.push(*dispatch_intent_lsn);
                }
            }
            Record::ConversationContextResetQueued {
                agent,
                text,
                context_lsn,
                context_reset_turn,
                ..
            } => {
                if self.conversation_context_loss(*agent) == Some(*context_reset_turn)
                    && self.agents.get(agent).is_some_and(|row| {
                        row.goal.is_none()
                            && !row.retired
                            && !row.attached
                            && row.context_lsn == *context_lsn
                    })
                {
                    self.unconfirmed_resumes.remove(agent);
                    self.browser_notes.insert(
                        lsn,
                        BrowserNoteRow {
                            agent: *agent,
                            text: text.clone(),
                            task: None,
                            answer_to: None,
                            destination: BrowserNoteDestination::ManagedConversation {
                                context_lsn: *context_lsn,
                            },
                            state: BrowserNoteState::Queued,
                        },
                    );
                }
            }
            Record::ConversationQuestion {
                agent,
                origin,
                text,
            } => {
                let valid = self.agents.get(agent).is_some_and(|row| {
                    row.open_turn == Some(origin.turn) && !row.retired && !row.attached
                }) && self.conversation_question_origin(origin.turn) == Some(*origin);
                if valid && let Some(row) = self.agents.get_mut(agent) {
                    row.open_question = Some(text.clone());
                    row.question_lsn = Some(lsn);
                    row.conversation_question = Some(*origin);
                }
            }
            Record::ConversationAnswerQueued {
                agent,
                question_lsn,
                origin,
                text,
                task,
                authority,
                ..
            } => {
                let Some(row) = self.agents.get_mut(agent) else {
                    return Ok(());
                };
                if row.question_lsn != Some(*question_lsn)
                    || row.conversation_question != Some(*origin)
                    || row.open_question.as_ref().is_none_or(|question| {
                        dudes_proto::conversation_answer_task(question, text) != *task
                    })
                    || matches!(authority, dudes_proto::ConversationAnswerAuthority::Browser { browser } if browser.question_lsn != *question_lsn)
                    || dudes_proto::normalized_chat_text(text) != Some(text.as_str())
                {
                    return Ok(());
                }
                row.open_question = None;
                row.question_lsn = None;
                row.conversation_question = None;
                self.browser_notes.insert(
                    lsn,
                    BrowserNoteRow {
                        agent: *agent,
                        text: text.clone(),
                        task: Some(task.clone()),
                        answer_to: Some(*question_lsn),
                        destination: BrowserNoteDestination::ManagedConversation {
                            context_lsn: origin.context_lsn,
                        },
                        state: BrowserNoteState::Queued,
                    },
                );
            }
            Record::ConversationMessageQueued {
                agent,
                text,
                browser: _,
                context_lsn,
                ..
            } => {
                if self.conversation_context_loss(*agent).is_none()
                    && self.agents.get(agent).is_some_and(|row| {
                        row.goal.is_none()
                            && !row.retired
                            && !row.attached
                            && row.context_lsn == *context_lsn
                    })
                {
                    self.browser_notes
                        .entry(lsn)
                        .or_insert_with(|| BrowserNoteRow {
                            agent: *agent,
                            text: text.clone(),
                            task: None,
                            answer_to: None,
                            destination: BrowserNoteDestination::ManagedConversation {
                                context_lsn: *context_lsn,
                            },
                            state: BrowserNoteState::Queued,
                        });
                }
            }
            Record::ConversationTurnDecided {
                input_digest,
                agent,
                note_lsn,
                context_lsn,
                decision,
                ..
            } => {
                if self
                    .conversation_message(*agent, *note_lsn, *context_lsn)
                    .is_some()
                    && !matches!(decision, NoteAdmission::Issue { task } if self.browser_notes[note_lsn].task.as_deref().unwrap_or(&self.browser_notes[note_lsn].text) != task)
                {
                    self.conversation_decisions.insert(
                        *agent,
                        (
                            lsn,
                            *note_lsn,
                            *context_lsn,
                            decision.clone(),
                            input_digest.clone(),
                        ),
                    );
                }
            }
            Record::ConversationTurnIssued {
                agent,
                turn,
                note_lsn,
                context_lsn,
                task,
                session,
                model_selection,
                lsn_of_decision,
                ..
            } => {
                let admitted = self.conversation_message(*agent, *note_lsn, *context_lsn).is_some_and(|note| note.task.as_deref().unwrap_or(&note.text) == task)
                    && self.eligible_browser_note(*agent, &BrowserNoteDestination::ManagedConversation { context_lsn: *context_lsn }).is_some_and(|(next, _)| next == *note_lsn)
                    && self.agents.get(agent).is_some_and(|row| row.open_turn.is_none())
                    && self.conversation_decisions.get(agent).is_some_and(|(at, note, context, decision, _)|
                        *at == *lsn_of_decision && *note == *note_lsn && *context == *context_lsn && matches!(decision, NoteAdmission::Issue { task: issued } if issued == task));
                if admitted && !self.turns.contains_key(turn) {
                    self.browser_notes
                        .get_mut(note_lsn)
                        .expect("validated message")
                        .state = BrowserNoteState::Reserved { turn: *turn };
                    self.install_browser_turn(
                        *turn,
                        BrowserIssueRow {
                            agent: *agent,
                            note_lsn: *note_lsn,
                            issue_lsn: lsn,
                            goal: None,
                            note_only: false,
                            completion_lsn: None,
                            contract: None,
                            dispatch_intents: Vec::new(),
                            uncertain_dispatches: Vec::new(),
                            spawn_lsn: None,
                            reissued: false,
                            end_lsn: None,
                            cancellation: None,
                        },
                        task,
                        session.as_ref(),
                        model_selection,
                        &[],
                    );
                }
            }
            Record::BrowserNoteQueued {
                agent,
                text,
                browser: _,
                destination,
            } => {
                if matches!(
                    destination,
                    BrowserNoteDestination::ManagedConversation { .. }
                ) {
                    return Ok(());
                }
                self.browser_notes
                    .entry(lsn)
                    .or_insert_with(|| BrowserNoteRow {
                        agent: *agent,
                        text: text.clone(),
                        task: None,
                        answer_to: None,
                        destination: destination.clone(),
                        state: BrowserNoteState::Queued,
                    });
            }
            Record::BrowserNoteCancelled {
                agent, note_lsn, ..
            } => {
                if let Some(note) = self.browser_notes.get_mut(note_lsn)
                    && note.agent == *agent
                    && note.state == BrowserNoteState::Queued
                {
                    note.state = BrowserNoteState::Cancelled;
                }
            }
            Record::NoteTurnDecided {
                agent,
                goal,
                note_lsn,
                completion_lsn,
                input_digest,
                gate_trace,
                decision,
            }
            | Record::TaughtNoteTurnDecided {
                agent,
                goal,
                note_lsn,
                completion_lsn,
                input_digest,
                gate_trace,
                decision,
            } => {
                let contract = if matches!(entry.record, Record::TaughtNoteTurnDecided { .. }) {
                    NoteContract::Taught
                } else {
                    NoteContract::Raw
                };
                let taught_valid=contract==NoteContract::Raw || (dudes_proto::valid_taught_decision(decision,gate_trace)
                    && self.browser_notes.get(note_lsn).is_some_and(|note|
                        !matches!(decision,NoteAdmission::Issue{task} if !contract.task_matches(&note.text,task))));
                if taught_valid
                    && self.completed_goal(*agent) == Some((*goal, *completion_lsn))
                    && self.browser_notes.get(note_lsn).is_some_and(|note| {
                        note.agent == *agent
                            && note.state == BrowserNoteState::Queued
                            && note.destination
                                == (BrowserNoteDestination::ManagedGoal { goal: *goal })
                    })
                {
                    self.note_decisions.insert(
                        *agent,
                        NoteDecisionRow {
                            lsn,
                            goal: *goal,
                            note_lsn: *note_lsn,
                            completion_lsn: *completion_lsn,
                            input_digest: input_digest.clone(),
                            gate_trace: gate_trace.clone(),
                            decision: decision.clone(),
                            contract,
                        },
                    );
                }
            }
            Record::NoteOnlyTurnIssued { .. }
            | Record::TaughtNoteOnlyTurnIssued { .. }
            | Record::TurnIssuedWithBrowserNote { .. } => {
                self.fold_browser_issue(lsn, &entry.record);
            }
            Record::BrowserNoteDispatchIntent {
                agent,
                note_lsn,
                turn,
            } => {
                if let Some(issue) = self.browser_issues.get_mut(turn)
                    && issue.agent == *agent
                    && issue.note_lsn == *note_lsn
                    && issue.cancellation.is_none()
                    && issue.spawn_lsn.is_none()
                    && issue.end_lsn.is_none()
                {
                    if issue.goal.is_none()
                        && issue
                            .dispatch_intents
                            .last()
                            .is_some_and(|prior| !issue.uncertain_dispatches.contains(prior))
                    {
                        return Ok(());
                    }
                    issue.dispatch_intents.push(lsn);
                }
            }
            Record::BrowserNoteIssuedCancelled {
                agent,
                note_lsn,
                turn,
                ..
            } => {
                if let Some(issue) = self.browser_issues.get_mut(turn)
                    && issue.agent == *agent
                    && issue.note_lsn == *note_lsn
                    && issue.goal.is_some()
                    && issue.dispatch_intents.is_empty()
                    && issue.spawn_lsn.is_none()
                    && issue.end_lsn.is_none()
                    && issue.cancellation.is_none()
                {
                    issue.cancellation = Some(BrowserIssueCancellation::Clean { lsn });
                    if let Some(note) = self.browser_notes.get_mut(note_lsn) {
                        note.state = BrowserNoteState::Cancelled;
                    }
                    if let Some(row) = self.agents.get_mut(agent)
                        && row.open_turn == Some(*turn)
                    {
                        row.open_turn = None;
                        self.open_turn_owners.remove(agent);
                    }
                }
            }
            Record::BrowserNoteReissueCancelled {
                agent,
                note_lsn,
                turn,
                dispatch_intent_lsn,
                ..
            } => {
                if let Some(issue) = self.browser_issues.get_mut(turn)
                    && issue.agent == *agent
                    && issue.note_lsn == *note_lsn
                    && issue.goal.is_some()
                    && issue.dispatch_intents.contains(dispatch_intent_lsn)
                    && issue.spawn_lsn.is_none()
                    && issue.end_lsn.is_none()
                    && issue.cancellation.is_none()
                {
                    issue.cancellation = Some(BrowserIssueCancellation::Ambiguous {
                        lsn,
                        dispatch_intent_lsn: *dispatch_intent_lsn,
                    });
                    if let Some(note) = self.browser_notes.get_mut(note_lsn) {
                        note.state = BrowserNoteState::ReissueCancelled { turn: *turn };
                    }
                }
            }
            Record::BrowserNoteContextEmitted { agent, note_lsn } => {
                if let Some(note) = self.browser_notes.get_mut(note_lsn)
                    && note.agent == *agent
                    && note.destination == BrowserNoteDestination::AttachedContext
                    && note.state == BrowserNoteState::Queued
                {
                    note.state = BrowserNoteState::ContextEmitted;
                }
            }
            Record::AgentRenamed { id, name, .. } => {
                if let Some(row) = self.agents.get_mut(id) {
                    row.name.clone_from(name);
                }
            }
            Record::AgentLookChanged {
                id,
                role,
                character,
                accent,
                ..
            } => {
                if let Some(row) = self.agents.get_mut(id) {
                    row.role.clone_from(role);
                    row.character = *character;
                    row.accent = *accent;
                    row.look_lsn = Some(lsn);
                }
            }
            Record::AgentMcpServersChanged { agent, mcp_servers } => {
                if let Some(row) = self.agents.get_mut(agent) {
                    row.mcp.replace(lsn, mcp_servers);
                }
            }
            Record::McpLaunchGranted {
                agent,
                turn,
                policy_lsn,
                mcp_servers,
            } => {
                self.mcp_grants.insert(
                    lsn,
                    McpGrant {
                        agent: *agent,
                        turn: *turn,
                        policy_lsn: *policy_lsn,
                        mcp_servers: mcp_servers.clone(),
                        spawned: None,
                    },
                );
            }
            Record::AgentHired {
                id,
                name,
                role,
                department,
                provider,
                character,
                accent,
                cwd,
                budget,
                sandbox,
                isolation,
                max_turns,
                max_budget,
                trusted,
                worktree,
                mcp_servers,
                browser_onboarding,
                ..
            } => {
                self.agents.insert(
                    *id,
                    AgentRow {
                        name: name.clone(),
                        role: role.clone(),
                        look_lsn: None,
                        department: department.clone(),
                        provider: *provider,
                        character: *character,
                        accent: *accent,
                        cwd: cwd.clone(),
                        budget: *budget,
                        sandbox: *sandbox,
                        max_turns: *max_turns,
                        max_budget: *max_budget,
                        trusted: *trusted,
                        worktree: *worktree,
                        mcp: McpPolicy::hired(
                            lsn,
                            mcp_servers,
                            *isolation,
                            browser_onboarding.is_some(),
                        ),
                        retired: false,
                        halted: false,
                        paused: false,
                        attached: false,
                        control_lsn: None,
                        automatic_hold: None,
                        fast_failures: None,
                        rapid_failure_beat: false,
                        pending_launch_failure: None,
                        open_question: None,
                        question_lsn: None,
                        conversation_question: None,
                        pending_answers: VecDeque::new(),
                        last_turn_end: None,
                        goal: None,
                        context_lsn: lsn,
                        rung: BreakerRung::Healthy,
                        steers: VecDeque::new(),
                        session: None,
                        session_lsn: None,
                        open_turn: None,
                        last_input_digest: None,
                        exhausted: None,
                    },
                );
                self.active_folder_users.insert(*id);
                if let Some(worktree) = worktree {
                    self.assign_worktree(*worktree, *id);
                }
            }
            Record::WorktreeLeased { .. } | Record::WorktreeRemoved { .. } => {
                self.fold_worktree(&entry.record);
            }
            Record::AgentRetired { id, .. } => {
                self.active_folder_users.remove(id);
                if let Some(row) = self.agents.get_mut(id) {
                    row.retired = true;
                    row.goal = None;
                    row.context_lsn = lsn;
                }
                for (goal, row) in &mut self.goals {
                    if !row.cleared
                        && !self.completed_goals.contains_key(goal)
                        && matches!(row.goal.predicate, Predicate::BoardState { .. })
                    {
                        row.predicate = None;
                    }
                }
            }
            Record::GoalCleared { agent, goal } => self.clear_goal(*agent, *goal, lsn),
            Record::GoalReassigned { goal, from, to, .. } => {
                if let Some(row) = self.goals.get_mut(goal) {
                    row.agent = *to;
                }
                if let Some(row) = self.agents.get_mut(from)
                    && row.goal == Some(*goal)
                {
                    row.goal = None;
                    row.context_lsn = lsn;
                }
                if let Some(row) = self.agents.get_mut(to) {
                    row.goal = Some(*goal);
                    row.context_lsn = lsn;
                }
            }
            Record::TurnIssued {
                agent,
                turn,
                goal,
                task,
                session,
                model_selection,
                lsn_of_decision,
                answers,
                ..
            } => {
                // a record that omits the agent's oldest pending answer, although that
                // answer precedes its decision, predates `answers` — this build's carry always lists
                // the oldest. An absent list is "not recorded", never "carried none",
                // and the agent has worked past every answer decided before it: each
                // is retired without delivery, its `HumanAnswer` still in the journal.
                if let Some(row) = self.agents.get_mut(agent)
                    && row.pending_answers.front().is_some_and(|oldest| {
                        oldest.lsn < *lsn_of_decision && !answers.contains(&oldest.lsn)
                    })
                {
                    row.pending_answers
                        .retain(|pending| pending.lsn > *lsn_of_decision);
                }
                self.install_managed_turn(
                    *turn,
                    TurnRow {
                        issue_lsn: lsn,
                        agent: *agent,
                        goal: Some(*goal),
                        status: TurnStatus::Issued,
                        session: session.clone(),
                        announced: session.clone(),
                        session_confirmed: false,
                        task: task.clone(),
                        answers: answers.clone(),
                        model_selection: model_selection.clone(),
                        pid: None,
                        start_time: None,
                        boot: None,
                        spawn_lsn: None,
                        end_lsn: None,
                        confirmation_lsn: None,
                        mcp_grant: None,
                    },
                );
            }
            Record::MailWakeTurnDecided(_) => {}
            Record::MailWakeOnlyTurnIssued(issue) => {
                if self.mail_wakes.accepted(lsn) {
                    self.install_managed_turn(
                        issue.turn,
                        TurnRow {
                            issue_lsn: lsn,
                            agent: issue.agent,
                            goal: Some(issue.goal),
                            status: TurnStatus::Issued,
                            session: issue.session.clone(),
                            announced: issue.session.clone(),
                            session_confirmed: false,
                            task: issue.task.as_str().to_owned(),
                            answers: Vec::new(),
                            model_selection: Some(issue.model_selection.clone()),
                            pid: None,
                            start_time: None,
                            boot: None,
                            spawn_lsn: None,
                            end_lsn: None,
                            confirmation_lsn: None,
                            mcp_grant: None,
                        },
                    );
                }
            }
            Record::TurnSpawned {
                agent,
                turn,
                pid,
                start_time,
                boot,
                session,
                reissued,
                mcp_grant,
            } => {
                if self.browser_issues.get(turn).is_some_and(|issue| {
                    matches!(
                        issue.cancellation,
                        Some(BrowserIssueCancellation::Clean { .. })
                    )
                }) {
                    return Ok(());
                }
                if self
                    .browser_issues
                    .get(turn)
                    .is_some_and(|issue| issue.agent != *agent || issue.dispatch_intents.is_empty())
                {
                    return Ok(());
                }
                if let Some((started_lsn, attempt)) = self.instructions.current_for_turn(*turn)
                    && matches!(attempt.owner, InstructionObservationOwner::Managed { agent: owner, turn: owned_turn, .. } if owner == *agent && owned_turn == *turn)
                    && attempt.spawn_lsn == Some(lsn)
                    && attempt.boundary_settled.is_none()
                {
                    self.managed_native_spawns
                        .entry(started_lsn)
                        .or_insert_with(|| ManagedNativeSpawnReceipt {
                            owner: attempt.owner,
                            key: AdoptionKey {
                                pid: *pid,
                                start_time: *start_time,
                                boot: boot.clone(),
                            },
                            spawn_lsn: lsn,
                        });
                }
                if let Some(issue) = self.browser_issues.get_mut(turn)
                    && issue.agent == *agent
                {
                    issue.spawn_lsn = Some(lsn);
                    issue.reissued = *reissued;
                    if let Some(note) = self.browser_notes.get_mut(&issue.note_lsn) {
                        note.state = BrowserNoteState::Delivered { turn: *turn };
                    }
                }
                if let Some(row) = self.turns.get_mut(turn) {
                    row.status = TurnStatus::Spawned;
                    row.pid = Some(*pid);
                    row.start_time = Some(*start_time);
                    row.boot.clone_from(boot);
                    row.spawn_lsn = Some(lsn);
                    row.mcp_grant = *mcp_grant;
                    if session.is_some() {
                        row.session.clone_from(session);
                    }
                    row.announced.clone_from(session);
                    // Any confirmation already folded for this TurnId belongs to an earlier
                    // launch attempt. The machine journals the current reissue's held confirmation
                    // only after this carrier is durable.
                    if *reissued {
                        row.session_confirmed = false;
                        row.confirmation_lsn = None;
                    }
                }
                if let Some(grant) = mcp_grant.and_then(|grant| self.mcp_grants.get_mut(&grant)) {
                    grant.spawned = Some((
                        lsn,
                        AdoptionKey {
                            pid: *pid,
                            start_time: *start_time,
                            boot: boot.clone(),
                        },
                    ));
                }
                if *reissued && let Some(row) = self.agents.get_mut(agent) {
                    row.session = None;
                    row.session_lsn = None;
                    if self
                        .browser_issues
                        .get(turn)
                        .is_some_and(|issue| issue.goal.is_none())
                    {
                        row.context_lsn = lsn;
                    }
                }
                // The consumption point: the answers this turn's task carries are
                // delivered now that a child exists, and not before.
                let carried = self
                    .turns
                    .get(turn)
                    .map(|row| row.answers.clone())
                    .unwrap_or_default();
                if !carried.is_empty()
                    && let Some(row) = self.agents.get_mut(agent)
                {
                    row.pending_answers
                        .retain(|pending| !carried.contains(&pending.lsn));
                }
            }
            Record::SessionLearned {
                agent,
                turn,
                session,
            } => {
                if let Some(row) = self.turns.get_mut(turn) {
                    row.session = Some(session.clone());
                    row.session_confirmed = true;
                    row.confirmation_lsn = Some(lsn);
                }
                if let Some(row) = self.agents.get_mut(agent) {
                    row.session = Some(session.clone());
                    row.session_lsn = Some(lsn);
                }
                self.unconfirmed_resumes.remove(agent);
            }
            Record::TurnEnded {
                agent,
                turn,
                mode,
                provenance,
                outcome,
                session,
                usage,
                session_usage,
                ..
            } => {
                if self.browser_issues.get(turn).is_some_and(|issue| {
                    matches!(
                        issue.cancellation,
                        Some(BrowserIssueCancellation::Clean { .. })
                    )
                }) {
                    return Ok(());
                }
                if let Some(issue) = self.browser_issues.get_mut(turn)
                    && issue.agent == *agent
                {
                    issue.end_lsn = Some(lsn);
                    if let Some(note) = self.browser_notes.get_mut(&issue.note_lsn) {
                        note.state = if issue.spawn_lsn.is_none() {
                            BrowserNoteState::Failed { turn: *turn }
                        } else {
                            BrowserNoteState::Delivered { turn: *turn }
                        };
                    }
                }
                let current_goal_end = self.turns.get(turn).is_some_and(|turn_row| {
                    self.agents
                        .get(agent)
                        .is_some_and(|row| row.goal == turn_row.goal)
                });
                let qualifies = dudes_proto::decision::is_fast_failure(&entry.record);
                if current_goal_end && let Some(row) = self.agents.get_mut(agent) {
                    row.pending_launch_failure = match outcome {
                        Some(TurnOutcome::NotLaunched { reason }) => {
                            Some(dudes_proto::AutomaticHold::LaunchFailed {
                                reason: launch_failed_reason(reason),
                            })
                        }
                        Some(TurnOutcome::LaunchRefused { reason }) => {
                            Some(dudes_proto::AutomaticHold::LaunchRefused {
                                reason: reason.clone(),
                            })
                        }
                        _ => None,
                    };
                    row.fast_failures = if qualifies {
                        Some((row.fast_failures.map_or(*turn, |(first, _)| first), *turn))
                    } else {
                        None
                    };
                    if !qualifies {
                        row.rapid_failure_beat = false;
                    }
                }
                let confirmed = self
                    .turns
                    .get(turn)
                    .is_some_and(|row| row.session_confirmed);
                if let (true, Some(session)) = (confirmed, session) {
                    let total = self.session_totals.entry(session.clone()).or_default();
                    match (session_usage, usage) {
                        (Some(recorded), _) => *total = *recorded,
                        // A record written before the running total was recorded carries none, so the sum
                        // of its charges stands in for one.
                        (None, Some(usage)) => *total = total.saturating_add(*usage),
                        (None, None) => {}
                    }
                }
                let invalidates_resume = *mode == dudes_types::Mode::Managed
                    && qualifying_unconfirmed_resume_end(outcome.as_ref())
                    && self.turns.get(turn).is_some_and(|turn_row| {
                        !turn_row.session_confirmed
                            && turn_row.session.as_ref().is_some_and(|announced| {
                                self.agents
                                    .get(agent)
                                    .and_then(|agent_row| agent_row.session.as_ref())
                                    == Some(announced)
                            })
                    });
                if let Some(row) = self.turns.get_mut(turn) {
                    row.end_lsn = Some(lsn);
                    row.status = if matches!(outcome, Some(dudes_proto::TurnOutcome::Killed { .. }))
                    {
                        TurnStatus::Killed
                    } else {
                        TurnStatus::Ended
                    };
                    if session.is_some() {
                        row.session.clone_from(session);
                    }
                }
                if let Some(row) = self.agents.get_mut(agent) {
                    // Last write by `lsn` wins, which is what makes an Observed superseder of an
                    // Inferred end work when an emitter of one ever exists.
                    row.last_turn_end = Some(*provenance);
                    if row.open_turn == Some(*turn) {
                        row.open_turn = None;
                        self.open_turn_owners.remove(agent);
                    }
                    if invalidates_resume
                        || matches!(
                            outcome,
                            Some(dudes_proto::TurnOutcome::LaunchRefused { .. })
                        )
                    {
                        row.session = None;
                        row.session_lsn = None;
                    }
                }
                if invalidates_resume {
                    self.unconfirmed_resumes.insert(*agent, *turn);
                    if self
                        .browser_issues
                        .get(turn)
                        .is_some_and(|issue| issue.goal.is_none())
                        && let Some(row) = self.agents.get_mut(agent)
                    {
                        row.context_lsn = lsn;
                    }
                }
            }
            // The effective pass is the recorded one AND the eligibility check, so a
            // passing result an older runner journaled for a sibling's status cannot finish a goal
            // after a restart.
            Record::PredicateResult {
                goal,
                turn,
                provenance,
                pass,
                evidence,
                ..
            } => {
                if let Some(row) = self.goals.get_mut(goal) {
                    row.predicate = Some(PredicateInput {
                        pass: *pass
                            && crate::predicate_evidence_eligible(&row.goal.predicate, evidence),
                        provenance: *provenance,
                        not_run: evidence.not_run(*pass),
                    });
                    row.predicate_lsn = Some(lsn);
                    if !row.cleared
                        && provenance.is_observed()
                        && let Some(turn) = turn
                        && row.last_turn == Some(*turn)
                    {
                        self.observed_predicates
                            .insert(*goal, ObservedPredicateResult { turn: *turn, lsn });
                    }
                }
            }
            Record::BreakerBeat {
                agent,
                rung_after,
                signals,
                ..
            } => {
                if let Some(row) = self.agents.get_mut(agent) {
                    row.rung = *rung_after;
                    if signals.contains(&dudes_types::BreakerSignal::RapidFailure) {
                        row.rapid_failure_beat = true;
                    }
                }
            }
            Record::HumanQuestion { agent, text } => {
                if let Some(row) = self.agents.get_mut(agent) {
                    row.open_question = Some(text.clone());
                    row.question_lsn = Some(lsn);
                    row.conversation_question = None;
                }
            }
            // An answer closes the open question and waits to be carried by the next spawned
            // turn. One that finds no open question is inert, and so is one appended while the
            // agent is attached: the question closes, and nothing is delivered, because the human
            // holds the keyboard.
            Record::HumanAnswer { agent, text, .. } => {
                if let Some(row) = self.agents.get_mut(agent)
                    && let Some(question) = row.open_question.take()
                {
                    row.question_lsn = None;
                    row.conversation_question = None;
                    if !row.attached {
                        row.pending_answers.push_back(PendingAnswer {
                            lsn,
                            question,
                            text: text.clone(),
                        });
                    }
                }
            }
            Record::Attached { agent, .. } => {
                if let Some(row) = self.agents.get_mut(agent) {
                    row.attached = true;
                }
            }
            Record::Detached { agent, .. } => {
                if let Some(row) = self.agents.get_mut(agent) {
                    row.attached = false;
                }
                self.attached_children.remove(agent);
            }
            Record::PtySpawned {
                agent,
                pty,
                pid,
                start_time,
                boot,
            } => {
                if let Some(attachment_lsn) = self.instructions.attachment_lsn(*agent)
                    && let Some((started_lsn, attempt)) =
                        self.instructions.current_for_attachment(attachment_lsn)
                    && attempt.owner
                        == (InstructionObservationOwner::AttachedProcess {
                            agent: *agent,
                            attachment_lsn,
                        })
                    && attempt.spawn_lsn == Some(lsn)
                    && attempt.boundary_settled.is_none()
                {
                    self.attached_native_spawns
                        .entry(started_lsn)
                        .or_insert_with(|| AttachedNativeSpawnReceipt {
                            owner: attempt.owner,
                            pty: *pty,
                            key: AdoptionKey {
                                pid: *pid,
                                start_time: *start_time,
                                boot: boot.clone(),
                            },
                            spawn_lsn: lsn,
                        });
                }
                if self.agents.get(agent).is_some_and(|row| row.attached) {
                    self.attached_children.insert(
                        *agent,
                        AdoptionKey {
                            pid: *pid,
                            start_time: *start_time,
                            boot: boot.clone(),
                        },
                    );
                }
            }
            // Append only. The cap is the producer's (see [`MAX_PENDING_STEERS`]), so there is
            // no eviction arm here and no branch on depth — which is what makes this fold a pure
            // function of the record sequence, identical live and on replay.
            Record::Steer { agent, text, .. } => {
                if let Some(row) = self.agents.get_mut(agent) {
                    row.steers.push_back((lsn, text.clone()));
                }
            }
            // Delivered, or dropped undelivered. Both remove the note **at that position** and
            // remove nothing when the queue does not hold it, so both are idempotent: replaying a
            // journal that already folded once reaches the same queue, and a removal that races a
            // second event cannot take a different note than the one it named.
            Record::SteerConsumed { agent, steer_lsn }
            | Record::SteerCleared { agent, steer_lsn } => {
                if let Some(row) = self.agents.get_mut(agent) {
                    row.steers.retain(|(at, _)| at != steer_lsn);
                }
            }
            Record::AgentHeld { agent, cause, .. } => {
                if let Some(row) = self.agents.get_mut(agent) {
                    row.automatic_hold = Some(cause.clone());
                    row.pending_launch_failure = None;
                    row.control_lsn = Some(lsn);
                }
            }
            Record::Halt { agent, .. } => {
                if let Some(row) = self.agents.get_mut(agent) {
                    row.halted = true;
                    row.control_lsn = Some(lsn);
                }
            }
            Record::Pause { agent, .. } => {
                if let Some(row) = self.agents.get_mut(agent) {
                    row.paused = true;
                    row.control_lsn = Some(lsn);
                }
            }
            Record::Resume { agent, .. } => {
                if let Some(row) = self.agents.get_mut(agent) {
                    row.automatic_hold = None;
                    row.fast_failures = None;
                    row.rapid_failure_beat = false;
                    row.pending_launch_failure = None;
                    row.halted = false;
                    row.paused = false;
                    row.control_lsn = Some(lsn);
                }
            }
            Record::HookTokenMinted { agent, token_hash } => {
                // Insert, never replace: a draw for an agent adds a member rather than superseding
                // what that agent already has, which is set membership.
                //
                // This map is not itself the valid set — it holds every hash ever minted, a
                // retired agent's included. The valid set is what `agent_for_token_hash` answers
                // over it, once retirement is applied there. Half the rule is folded here; the
                // other half is read, and neither is the whole.
                //
                // `or_insert` rather than `insert` decides the one case the machine cannot
                // produce — the same hash journaled twice for two different agents, which needs a
                // blake3 collision or a hand-written journal. First writer wins there, so a later
                // record cannot repoint a credential a child is already holding. Nothing tests
                // it because nothing can reach it; it is a choice made rather than left to the
                // last line of a corrupt log.
                self.minted.entry(token_hash.clone()).or_insert(*agent);
            }
            Record::LoopDecided {
                agent,
                input_digest,
                decision,
                ..
            } => {
                // The digest is the trace `dudes why` prints and also what the tick's sweep
                // compares an `Exhausted` agent's current input against.
                //
                // **Assignment, and never `or_insert`.** The `HookTokenMinted` arm two above is
                // set-membership under a first-writer-wins rule, and this arm is the opposite
                // shape: the field is the *most recent* decision's digest, so a `.entry().or_*`
                // here would freeze it on the first decision an agent ever took, every later
                // input change would compare equal, and the agent would stay `Exhausted` for
                // ever — the exact defect this record closes, restored by copying a neighbour.
                if matches!(decision, LoopDecision::Continue { .. })
                    && let Some(goal) = self.agents.get(agent).and_then(|row| row.goal)
                {
                    self.ordinary_continues.insert(lsn, (*agent, goal));
                }
                if let Some(row) = self.agents.get_mut(agent) {
                    row.last_input_digest = Some(input_digest.clone());
                    row.exhausted = match decision {
                        LoopDecision::Exhausted { reason } => Some(*reason),
                        _ => None,
                    };
                    if matches!(decision, LoopDecision::Done)
                        && let Some(goal) = row.goal
                    {
                        self.completed_goals.entry(goal).or_insert(lsn);
                    }
                }
            }
            // A refusal (the only kind there is) and a no-op on purpose, not for
            // want of a reader: every value on it is already a projection of other records. The
            // spend is the budget projection's, the ceilings are `agent.hire`'s and `goal.set`'s,
            // and the `Exhausted` status is moved by the `LoopDecided` written beside it — which
            // is the arm directly above, now that it has a reader of its own. Folding any of it
            // here would give one fact two owners.
            Record::BudgetDecision { .. } => {} // history, read from the journal
            Record::MemoryCondenseModelAdmitted { .. } => {} // history, read from the journal
            Record::FloorPeerRefused { .. } => {} // audit only
            Record::WorkspaceTrustSet { .. } => self.fold_workspace_trust(&entry.record, lsn),
            // The goal was installed above, through the one accessor every goal carrier shares.
            Record::GoalSet { .. } => {}
            // An edit changes words, not a card's status, so no board result it could hold moves.
            Record::TicketEdited { .. } => {}
            Record::InstructionObservationStarted { .. }
            | Record::InstructionObservationFinished { .. }
            | Record::InstructionAttemptBoundarySettled { .. }
            | Record::TicketInstructionReviewed { .. } => {}
            Record::TicketBaselineRecorded {
                card,
                goal,
                baseline,
            } => self.fold_ticket_baseline(lsn, *card, *goal, baseline),
            Record::TicketReviewStamped { card, goal, stamp } => {
                self.fold_ticket_stamp(lsn, *card, *goal, stamp);
            }
            Record::CardCreated { card, .. }
            | Record::CardTransition { card, .. }
            | Record::Claim { card, .. }
            | Record::TicketCreated { card, .. }
            | Record::TicketAssigned { card, .. }
            | Record::TicketStarted { card, .. }
            | Record::TicketRemoved { card, .. } => {
                for (goal, row) in &mut self.goals {
                    if !row.cleared
                        && !self.completed_goals.contains_key(goal)
                        && matches!(row.goal.predicate, Predicate::BoardState {
                            card: requested, ..
                        } if requested == *card)
                    {
                        row.predicate = None;
                    }
                }
            }
            Record::Mail { .. } | Record::MailState { .. } => {} // the mailbox's
            Record::MemoryAppend { .. } | Record::MemoryCondense { .. } => {} // the memory projection's
            Record::ConditionedEffectSuppressed { .. } | Record::MemoryAppendQuarantined { .. } => {
            } // conditioned-effect audit records
            // **A no-op is what makes replay safe here, not an omission.** A `pty.write` exists
            // only as the continuation of the call that made it: the bytes reach the host from the
            // router's connection thread, and the connection that owned the `PtyId` is gone by the
            // time anything replays. Folding a request into pending work would be the one way to
            // turn the "replay never replays bytes for an unmatched request" guarantee false —
            // a restarted daemon re-issuing a write nobody asked for twice.
            Record::PtyWriteRequested { .. }
            | Record::PtyWriteCompleted { .. }
            | Record::PtyCloseRequested { .. }
            | Record::PtyCloseCompleted { .. } => {}
            // The retry loop is in memory, and so is the once-per-cause guard in front of it: a
            // fold that re-derived "already deferred" from this record would have to survive a
            // restart, and the next run re-asks rather than inheriting an answer.
            Record::KillDeferred { .. } => {}
            Record::ClosingTime { .. } => {}
            Record::LiveActivityObserved { .. } => {}
            Record::Checkpoint { .. } => {} // the writer's own record
            Record::RecoveryRequired(_)
            | Record::RecoveryAuthorized(_)
            | Record::RecoveryInvalidated(_) => {}
        }
        Ok(())
    }

    /// The durable ordinary completion of the agent's current uncleared goal.
    #[must_use]
    pub fn completed_goal(&self, agent: AgentId) -> Option<(GoalId, Lsn)> {
        let goal = self.agents.get(&agent)?.goal?;
        if self.goals.get(&goal)?.cleared {
            return None;
        }
        self.completed_goal_lsn(goal).map(|lsn| (goal, lsn))
    }

    fn capture_attached_turn_end(&self, lsn: Lsn, entry: &Entry) -> Option<AttachedTurnEndReceipt> {
        let Record::TurnEnded {
            agent,
            turn,
            mode: dudes_types::Mode::Attached,
            provenance: Provenance::Observed,
            ..
        } = &entry.record
        else {
            return None;
        };
        if lsn == Lsn::ZERO || self.instructions.end_lsn(*turn).is_some() {
            return None;
        }
        let goal = self.agents.get(agent).and_then(|row| row.goal);
        let ticket = goal
            .and_then(|goal| self.instructions.recorded_association_for_goal(goal))
            .filter(|recorded| {
                self.instructions
                    .association_for_goal(recorded.goal, recorded.seq_at_start)
                    == Some(*recorded)
            });
        Some(AttachedTurnEndReceipt {
            owner: InstructionObservationOwner::AttachedTurn {
                agent: *agent,
                turn: *turn,
                attachment_lsn: None,
            },
            end_lsn: lsn,
            goal,
            ticket,
        })
    }

    fn retire_attached_turn_end(&mut self, lsn: Lsn, entry: &Entry) {
        let Record::InstructionObservationFinished {
            owner,
            baseline: InstructionBaselineRef::MissingTurnBoundary { turn, end_lsn },
            ..
        } = &entry.record
        else {
            return;
        };
        let matched = self.unmatched_attached_turn_ends.get(end_lsn).is_some_and(|captured| {
            captured.owner == *owner
                && matches!(captured.owner, InstructionObservationOwner::AttachedTurn { turn: actual, .. } if actual == *turn)
        });
        let folded = self.instructions.attempt(*end_lsn).is_some_and(|attempt| {
            attempt.owner == *owner
                && attempt
                    .finished
                    .as_ref()
                    .is_some_and(|finished| finished.lsn == lsn)
        });
        if matched && folded {
            self.unmatched_attached_turn_ends.remove(end_lsn);
        }
    }

    fn clear_goal(&mut self, agent: AgentId, goal: GoalId, lsn: Lsn) {
        self.observed_predicates.remove(&goal);
        if let Some(row) = self.goals.get_mut(&goal) {
            row.cleared = true;
        }
        if let Some(row) = self.agents.get_mut(&agent)
            && row.goal == Some(goal)
        {
            row.goal = None;
            row.context_lsn = lsn;
            row.exhausted = None;
        }
    }

    /// The first durable ordinary Done for this goal.
    #[must_use]
    pub fn completed_goal_lsn(&self, goal: GoalId) -> Option<Lsn> {
        self.completed_goals.get(&goal).copied()
    }

    pub(crate) fn observed_predicate_after(
        &self,
        goal: GoalId,
        turn: TurnId,
        end_lsn: Lsn,
    ) -> bool {
        self.observed_predicates
            .get(&goal)
            .is_some_and(|result| result.turn == turn && result.lsn > end_lsn)
    }

    /// A browser note by its original queue identity.
    #[must_use]
    pub fn browser_note(&self, note_lsn: Lsn) -> Option<&BrowserNoteRow> {
        self.browser_notes.get(&note_lsn)
    }

    /// Browser note identities in journal order, including durable dispositions.
    #[must_use]
    pub fn browser_notes(&self) -> &BTreeMap<Lsn, BrowserNoteRow> {
        &self.browser_notes
    }

    /// Oldest queued browser note with exactly this owner and destination.
    #[must_use]
    pub fn eligible_browser_note(
        &self,
        agent: AgentId,
        destination: &BrowserNoteDestination,
    ) -> Option<(Lsn, &BrowserNoteRow)> {
        self.browser_notes
            .iter()
            .filter(|(_, note)| {
                note.agent == agent
                    && note.state == BrowserNoteState::Queued
                    && &note.destination == destination
            })
            .min_by_key(|(lsn, note)| (note.answer_to.is_none(), **lsn))
            .map(|(lsn, note)| (*lsn, note))
    }

    /// All durable dispatch attempts and dispositions of a note-bearing turn.
    #[must_use]
    pub fn browser_issue(&self, turn: TurnId) -> Option<&BrowserIssueRow> {
        self.browser_issues.get(&turn)
    }

    /// Whether durable cancellation forbids every future dispatch of this turn.
    #[must_use]
    pub fn browser_issue_cancelled(&self, turn: TurnId) -> bool {
        self.browser_issues
            .get(&turn)
            .is_some_and(|issue| issue.cancellation.is_some())
    }

    /// Latest completed-note admission, separate from the ordinary goal digest.
    #[must_use]
    pub fn note_admission(&self, agent: AgentId) -> Option<&NoteDecisionRow> {
        self.note_decisions.get(&agent)
    }

    #[must_use]
    pub(crate) fn conversation_question_origin(
        &self,
        turn: TurnId,
    ) -> Option<dudes_proto::ConversationQuestionOrigin> {
        let issue = self.browser_issues.get(&turn)?;
        let executed = self.turns.get(&turn)?;
        if issue.goal.is_some()
            || executed.agent != issue.agent
            || executed.goal.is_some()
            || !executed.is_open_child()
        {
            return None;
        }
        let context_lsn = if issue.reissued {
            issue.spawn_lsn?
        } else {
            let BrowserNoteDestination::ManagedConversation { context_lsn } =
                self.browser_notes.get(&issue.note_lsn)?.destination
            else {
                return None;
            };
            context_lsn
        };
        Some(dudes_proto::ConversationQuestionOrigin { turn, context_lsn })
    }

    /// Exact finite turn whose vendor context requires explicit acknowledgement before new work.
    #[must_use]
    pub fn conversation_context_loss(&self, agent: AgentId) -> Option<TurnId> {
        let turn = self.unconfirmed_resumes.get(&agent)?;
        self.browser_issues
            .get(turn)
            .filter(|issue| issue.goal.is_none())
            .map(|_| *turn)
    }

    /// Latest finite admission digest, separate from the ordinary goal loop.
    #[must_use]
    pub fn conversation_input_digest(&self, agent: AgentId) -> Option<&str> {
        self.conversation_decisions
            .get(&agent)
            .map(|(_, _, _, _, digest)| digest.as_str())
    }

    /// An unissued message whose captured context is still current.
    #[must_use]
    pub fn conversation_message(
        &self,
        agent: AgentId,
        note_lsn: Lsn,
        context_lsn: Lsn,
    ) -> Option<&BrowserNoteRow> {
        let row = self.agents.get(&agent)?;
        let note = self.browser_notes.get(&note_lsn)?;
        (row.goal.is_none()
            && row.context_lsn == context_lsn
            && !row.attached
            && !row.retired
            && note.agent == agent
            && note.state == BrowserNoteState::Queued
            && note.destination == (BrowserNoteDestination::ManagedConversation { context_lsn }))
        .then_some(note)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one contract-qualified lifecycle join validates ownership before installing and charging the turn"
    )]
    fn fold_browser_issue(&mut self, lsn: Lsn, record: &Record) {
        let (agent, turn, goal, task, session, selection, decision, answers, note_lsn, completion) =
            match record {
                Record::NoteOnlyTurnIssued {
                    agent,
                    turn,
                    goal,
                    task,
                    session,
                    model_selection,
                    lsn_of_decision,
                    answers,
                    note_lsn,
                    completion_lsn,
                    ..
                }
                | Record::TaughtNoteOnlyTurnIssued {
                    agent,
                    turn,
                    goal,
                    task,
                    session,
                    model_selection,
                    lsn_of_decision,
                    answers,
                    note_lsn,
                    completion_lsn,
                    ..
                } => (
                    *agent,
                    *turn,
                    *goal,
                    task,
                    session,
                    model_selection,
                    *lsn_of_decision,
                    answers,
                    *note_lsn,
                    Some(*completion_lsn),
                ),
                Record::TurnIssuedWithBrowserNote {
                    agent,
                    turn,
                    goal,
                    task,
                    session,
                    model_selection,
                    lsn_of_decision,
                    answers,
                    note_lsn,
                    ..
                } => (
                    *agent,
                    *turn,
                    *goal,
                    task,
                    session,
                    model_selection,
                    *lsn_of_decision,
                    answers,
                    *note_lsn,
                    None,
                ),
                _ => return,
            };
        let contract = match record {
            Record::NoteOnlyTurnIssued { .. } => Some(NoteContract::Raw),
            Record::TaughtNoteOnlyTurnIssued {
                mode: dudes_types::Mode::Managed,
                ..
            } => Some(NoteContract::Taught),
            Record::TaughtNoteOnlyTurnIssued { .. } => return,
            _ => None,
        };
        if self.turns.contains_key(&turn) || self.browser_issues.contains_key(&turn) {
            return;
        }
        let Some(note) = self.browser_notes.get_mut(&note_lsn) else {
            return;
        };
        if note.agent != agent
            || note.state != BrowserNoteState::Queued
            || note.destination != (BrowserNoteDestination::ManagedGoal { goal })
            || self.agents.get(&agent).is_none_or(|row| {
                row.goal != Some(goal) || row.attached || row.retired || row.open_turn.is_some()
            })
            || self.goals.get(&goal).is_none_or(|row| row.cleared)
        {
            return;
        }
        let admitted = match completion {
            Some(done) => answers.is_empty() && contract.is_some_and(|kind| kind.task_matches(&note.text,task)) && self.completed_goals.get(&goal) == Some(&done)
                && self.note_decisions.get(&agent).is_some_and(|row| row.lsn == decision && row.note_lsn == note_lsn && row.goal == goal
                    && row.completion_lsn == done && Some(row.contract)==contract && matches!(&row.decision, NoteAdmission::Issue { task: issued } if issued == task)),
            None => !self.completed_goals.contains_key(&goal) && self.ordinary_continues.get(&decision) == Some(&(agent, goal)),
        };
        if !admitted {
            return;
        }
        note.state = BrowserNoteState::Reserved { turn };
        self.install_browser_turn(
            turn,
            BrowserIssueRow {
                agent,
                note_lsn,
                issue_lsn: lsn,
                goal: Some(goal),
                note_only: completion.is_some(),
                completion_lsn: completion,
                contract,
                dispatch_intents: Vec::new(),
                uncertain_dispatches: Vec::new(),
                spawn_lsn: None,
                reissued: false,
                end_lsn: None,
                cancellation: None,
            },
            task,
            session.as_ref(),
            selection,
            answers,
        );
    }

    fn install_browser_turn(
        &mut self,
        turn: TurnId,
        issue: BrowserIssueRow,
        task: &str,
        session: Option<&SessionId>,
        selection: &ModelSelection,
        answers: &[Lsn],
    ) {
        let agent = issue.agent;
        let goal = issue.goal;
        let issue_lsn = issue.issue_lsn;
        self.browser_issues.insert(turn, issue);
        self.install_managed_turn(
            turn,
            TurnRow {
                issue_lsn,
                agent,
                goal,
                status: TurnStatus::Issued,
                session: session.cloned(),
                announced: session.cloned(),
                session_confirmed: false,
                task: task.to_owned(),
                answers: answers.to_vec(),
                model_selection: Some(selection.clone()),
                pid: None,
                start_time: None,
                boot: None,
                spawn_lsn: None,
                end_lsn: None,
                confirmation_lsn: None,
                mcp_grant: None,
            },
        );
    }

    fn install_managed_turn(&mut self, turn: TurnId, issued: TurnRow) {
        let agent = issued.agent;
        let goal = issued.goal;
        self.turns.insert(turn, issued);
        if let Some(goal) = goal {
            self.observed_predicates.remove(&goal);
        }
        if let Some(row) = goal.and_then(|goal| self.goals.get_mut(&goal)) {
            row.turns_on_goal = row.turns_on_goal.saturating_add(1);
            row.last_turn = Some(turn);
        }
        if let Some(row) = self.agents.get_mut(&agent) {
            row.open_turn = Some(turn);
            self.open_turn_owners.insert(agent);
        }
    }

    pub(crate) fn mail_wake_state(
        &self,
        agent: AgentId,
    ) -> Option<&dudes_proto::mail_wake::MailWakeState> {
        self.mail_wakes.state(agent)
    }
    pub(crate) fn mail_wake_count(&self, agent: AgentId, day: dudes_types::UtcDay) -> u8 {
        self.mail_wakes.count(agent, day)
    }
    pub(crate) fn mail_wake_receipts(
        &self,
        agent: AgentId,
    ) -> Vec<dudes_proto::mail_wake::MailWakeMail> {
        self.mail_wakes.receipts(agent)
    }
    pub(crate) fn mail_wake_recipients(&self, record: &Record) -> Vec<AgentId> {
        self.mail_wakes.affected(record)
    }
    pub(crate) fn mail_wake_history_valid(&self) -> bool {
        self.mail_wakes.valid()
    }
    pub(crate) fn mail_wake_origin(&self, turn: TurnId) -> bool {
        self.mail_wakes.origin(turn)
    }
    pub(crate) fn mail_wake_entry_accepted(&self, lsn: Lsn) -> bool {
        self.mail_wakes.accepted(lsn)
    }

    pub(crate) fn held_board_goals(
        &self,
        changed_card: Option<CardId>,
    ) -> impl Iterator<Item = (AgentId, GoalId)> + '_ {
        self.agents.iter().filter_map(move |(agent, row)| {
            let goal = row.goal?;
            let held = self.goals.get(&goal)?;
            let Predicate::BoardState { card, .. } = held.goal.predicate else {
                return None;
            };
            (!row.retired
                && !held.cleared
                && !self.completed_goals.contains_key(&goal)
                && changed_card.is_none_or(|changed| card == changed))
            .then_some((*agent, goal))
        })
    }

    pub(crate) fn settled_status(&self, agent: AgentId) -> AgentStatus {
        let Some(row) = self.agents.get(&agent) else {
            return AgentStatus::Idle;
        };
        if row.attached {
            AgentStatus::Attached
        } else if row.open_question.is_some() {
            AgentStatus::Waiting
        } else if row.open_turn.is_some() {
            AgentStatus::Running
        } else if row
            .goal
            .is_some_and(|goal| self.completed_goals.contains_key(&goal))
        {
            AgentStatus::Done
        } else {
            AgentStatus::Idle
        }
    }

    /// Everything one decision for one agent depends on, or `None` when the agent cannot be
    /// decided for: it does not exist, it is retired, or its goal is absent, cleared or completed.
    ///
    /// An agent without a goal is not decided *at all* — no `LoopDecided`, no `Continue` on an
    /// empty task. Gate 7 always fires, so inventing a goal would issue a turn with nothing in
    /// it.
    #[must_use]
    pub fn loop_input(
        &self,
        agent: AgentId,
        spend: SpendView,
        work: WorkInput,
        floor_usd_per_day: Option<Usd>,
        per_turn_floor: dudes_proto::Spend,
    ) -> Option<LoopInput> {
        let row = self.agents.get(&agent)?;
        if row.retired {
            return None;
        }
        let goal_id = row.goal?;
        let goal = self.goals.get(&goal_id)?;
        if goal.cleared || self.completed_goals.contains_key(&goal_id) {
            return None;
        }
        Some(LoopInput {
            control: Control {
                halted: row.halted,
                paused: row.paused,
                attached: row.attached,
                automatic_hold: row
                    .automatic_hold
                    .as_ref()
                    .map(dudes_proto::AutomaticHold::hold_reason)
                    .or_else(|| {
                        (self.instructions.pending_for_goal(goal_id)
                            && !goal.predicate.is_some_and(|predicate| {
                                predicate.pass && predicate.provenance == Provenance::Observed
                            }))
                        .then_some(dudes_proto::HoldReason::SharedInstructionsReview)
                    }),
            },
            last_turn_end: row.last_turn_end,
            open_question: row.open_question.clone(),
            goal: GoalInput {
                text: goal.goal.text.clone(),
                hop_cap: goal.goal.hop_cap,
                turns_on_goal: goal.turns_on_goal,
                budget: goal.goal.budget,
                spent: spend.goal,
                unknown_ends: spend.goal_unknown_ends,
                predicate: goal.predicate,
                baseline: goal
                    .goal
                    .baseline
                    .as_ref()
                    .map(dudes_proto::CheckBaseline::kind),
            },
            agent: AgentInput {
                budget: row.budget,
                spent_today: spend.agent_today,
                unknown_ends_today: spend.agent_unknown_ends_today,
                rung: row.rung,
            },
            floor: FloorInput {
                usd_per_day: floor_usd_per_day,
                spent_today: spend.floor_today,
                unknown_ends_today: spend.floor_unknown_ends_today,
            },
            per_turn_floor,
            // Gate 7's card, mail and digest, computed by the caller from the board projection,
            // the mailbox and the memory projection; each is a crate this one does not depend on
            // and must not. Whatever was last pushed on `Input::Work`, or the default when
            // nothing has been: no card, no mail, no digest, which is the goal text alone.
            work,
            answers: row
                .pending_answers
                .iter()
                .map(|pending| AnswerInput {
                    lsn: pending.lsn,
                    question: pending.question.clone(),
                    text: pending.text.clone(),
                })
                .collect(),
        })
    }

    /// The review metadata recorded for a ticket goal, if any.
    #[must_use]
    pub fn ticket_review(&self, goal: GoalId) -> Option<&TicketReview> {
        self.ticket_reviews.get(&goal)
    }

    /// The card and round a ticket start installed `goal` for; `None` for any other goal.
    #[must_use]
    pub fn ticket_goal(&self, goal: GoalId) -> Option<(CardId, u32)> {
        self.ticket_goals.get(&goal).copied()
    }

    /// Whether any turn was ever issued for `goal`.
    #[must_use]
    pub fn goal_has_turn(&self, goal: GoalId) -> bool {
        self.goals
            .get(&goal)
            .is_some_and(|row| row.last_turn.is_some() || row.turns_on_goal > 0)
    }

    /// The first baseline for a goal wins, and only when it names the card, goal and claimant
    /// the journal installed. Anything else is ignored: the payload is display metadata and
    /// never an authority.
    fn fold_ticket_baseline(
        &mut self,
        lsn: Lsn,
        card: CardId,
        goal: GoalId,
        baseline: &dudes_proto::diff_stamp::BaselinePayload,
    ) {
        let Some(row) = self.goals.get(&goal) else {
            return;
        };
        if baseline.binding.card != card
            || baseline.binding.goal != goal
            || baseline.binding.claimant != row.agent
            || baseline.encode().is_err()
        {
            return;
        }
        self.ticket_reviews
            .entry(goal)
            .or_insert_with(|| TicketReview {
                baseline_lsn: lsn,
                baseline: *baseline,
                stamp: None,
            });
    }

    /// A stamp counts only against the recorded baseline it names, with the same binding.
    fn fold_ticket_stamp(
        &mut self,
        lsn: Lsn,
        card: CardId,
        goal: GoalId,
        stamp: &dudes_proto::diff_stamp::StampPayload,
    ) {
        let Some(review) = self.ticket_reviews.get_mut(&goal) else {
            return;
        };
        if stamp.binding.card == card
            && stamp.binding == review.baseline.binding
            && stamp.baseline_lsn == review.baseline_lsn
            && stamp.encode().is_ok()
        {
            review.stamp = Some((lsn, *stamp));
        }
    }

    /// Unspawned issues whose replay is still owed, excluding durable browser cancellation tombstones.
    pub fn issued_not_spawned(&self) -> impl Iterator<Item = (TurnId, &TurnRow)> {
        let issues = &self.browser_issues;
        self.turns
            .iter()
            .filter(move |(turn, row)| {
                row.is_issued_not_spawned()
                    && issues
                        .get(*turn)
                        .is_none_or(|issue| issue.cancellation.is_none())
            })
            .map(|(turn, row)| (*turn, row))
    }

    /// The latest qualifying unconfirmed resume for `agent`, when one remains unrecovered.
    #[must_use]
    pub(crate) fn unconfirmed_resume(&self, agent: AgentId) -> Option<TurnId> {
        self.unconfirmed_resumes.get(&agent).copied()
    }

    /// Turns with a recorded child and no recorded end: crash rule (3).
    pub fn open_children(&self) -> impl Iterator<Item = (TurnId, &TurnRow)> {
        self.turns
            .iter()
            .filter(|(_, row)| row.is_open_child())
            .map(|(turn, row)| (*turn, row))
    }

    /// Every recorded child whose end is not recorded, by owning agent and adoption key: a turn
    /// child until its `TurnEnded`, an attached child until its `Detached`. A retired agent's
    /// children are included. A turn child whose key is incomplete cannot be walked
    /// to, so it is not here.
    pub fn recorded_children(&self) -> impl Iterator<Item = (AgentId, AdoptionKey)> + '_ {
        let turns = self
            .open_children()
            .filter_map(|(_, row)| row.adoption_key().map(|key| (row.agent, key)));
        let attached = self
            .attached_children()
            .map(|(agent, key)| (agent, key.clone()));
        turns.chain(attached)
    }

    /// Each attached agent's recorded PTY child: the key of its `PtySpawned`, until its `Detached`
    /// clears it. A retired agent's is included until then.
    pub fn attached_children(&self) -> impl Iterator<Item = (AgentId, &AdoptionKey)> {
        self.attached_children
            .iter()
            .map(|(agent, key)| (*agent, key))
    }

    /// Agents with an `Attached` and no `Detached`; without confirmed old-PTY absence they hold.
    pub fn attached_agents(&self) -> impl Iterator<Item = AgentId> {
        self.agents
            .iter()
            .filter(|(_, row)| row.attached && !row.retired)
            .map(|(agent, _)| *agent)
    }

    /// The goal an agent pursues, when it has one that has not been cleared.
    #[must_use]
    pub fn goal_of(&self, agent: AgentId) -> Option<&GoalRow> {
        let goal = self.agents.get(&agent)?.goal?;
        self.goals.get(&goal).filter(|row| !row.cleared)
    }

    /// The agent an agent-token hash was minted for, when some `HookTokenMinted` carries that hash
    /// and that agent is not retired, or is retired and still has a recorded child.
    ///
    /// **The projection an implementation of `dudes_rpc::MintedTokens` reads**, which is why it
    /// takes the hash and not the token: the plaintext is never journaled, so hashing belongs to
    /// the implementation that holds a presented token, and this crate answers about the fold.
    /// What a presented token then resolves to is decided by `classify`, which this method neither
    /// implements nor restates: it answers only which of that table's rows a presented token
    /// falls in — row 2, the retired-agent row or row 3, as `MintedTokens` puts it — and nothing about
    /// the columns, which are peer provenance and are resolved elsewhere.
    ///
    /// `None` is the answer for a hash nothing minted **and** for the hash of a retired agent with
    /// no recorded child, because those are one row and a caller that could tell them apart would
    /// have an oracle for which agents exist.
    ///
    /// **A hash whose agent this fold has no row for is also `None`**, which is the direction
    /// invariant 8 requires: the fold cannot assert "not retired" about an agent it has never
    /// seen hired, and an identity it cannot prove is refused. In a whole journal the hire always
    /// precedes the mint, so this is the answer to a truncated one rather than to anything the
    /// machine writes.
    #[must_use]
    pub fn agent_for_token_hash(&self, token_hash: &str) -> Option<MintedFor> {
        let agent = *self.minted.get(token_hash)?;
        self.minted_for(agent, || {
            self.recorded_children().any(|(owner, _)| owner == agent)
        })
    }

    /// `has_recorded_child` is asked only for a retired agent, so a fold with no retired minting
    /// never walks the children.
    fn minted_for(
        &self,
        agent: AgentId,
        has_recorded_child: impl FnOnce() -> bool,
    ) -> Option<MintedFor> {
        let row = self.agents.get(&agent)?;
        if !row.retired {
            return Some(MintedFor::Live(agent));
        }
        has_recorded_child().then_some(MintedFor::RetiredWithChild(agent))
    }

    /// [`World::agent_for_token_hash`] over its whole domain: every journaled hash this fold
    /// answers an agent for.
    ///
    /// **This adds no rule.** A hash is in the answer exactly when
    /// [`World::agent_for_token_hash`] returns `Some` for it, which is what the two share an
    /// implementation for: set membership over every minting for an agent this fold has a
    /// non-retired row for, or a retired row with a recorded child, never the newest,
    /// and absent for an agent it has never seen hired.
    ///
    /// **Owned, so it can leave the loop thread.** `World` is folded by the single writer and a
    /// caller on another thread cannot hold a borrow of it; a snapshot can be published and read
    /// without one. What that costs — how stale a reader's copy may be, and which direction the
    /// staleness falls — is the publisher's to state, not this method's: it answers about the
    /// fold as it stands when it is called.
    #[must_use]
    pub fn token_hash_owners(&self) -> BTreeMap<String, MintedFor> {
        let with_children = OnceCell::new();
        self.minted
            .iter()
            .filter_map(|(hash, agent)| {
                self.minted_for(*agent, || {
                    with_children
                        .get_or_init(|| {
                            self.recorded_children()
                                .map(|(owner, _)| owner)
                                .collect::<BTreeSet<AgentId>>()
                        })
                        .contains(agent)
                })
                .map(|owner| (hash.clone(), owner))
            })
            .collect()
    }

    /// `session`'s running total as the journal last recorded it: the last confirmed turn's
    /// `session_usage`, or, for records written before the running total was recorded, the sum of their usage.
    #[must_use]
    pub fn session_total(&self, session: &SessionId) -> Usage {
        self.session_totals
            .get(session)
            .copied()
            .unwrap_or_default()
    }

    /// Nonretired recorded folder users, including idle, paused and halted agents.
    pub fn active_folder_user_ids(&self) -> impl ExactSizeIterator<Item = &AgentId> {
        self.active_folder_users.iter()
    }

    /// One unmatched first observed Attached end, retaining its original goal and ticket context.
    #[must_use]
    pub fn unmatched_attached_turn_end(&self, end_lsn: Lsn) -> Option<&AttachedTurnEndReceipt> {
        self.unmatched_attached_turn_ends.get(&end_lsn)
    }

    /// Current unmatched count without visiting historical turns or finished observations.
    #[must_use]
    pub fn unmatched_attached_turn_end_count(&self) -> usize {
        self.unmatched_attached_turn_ends.len()
    }

    /// Inspect at most `limit` current ends, capped at 512, and explicitly report omitted rows.
    ///
    /// All unmatched identities remain stored until their exact finishing observation is folded.
    /// Truncation cannot establish complete coverage or admit work requiring a complete census.
    pub fn unmatched_attached_turn_ends(
        &self,
        limit: usize,
    ) -> (
        impl ExactSizeIterator<Item = (&Lsn, &AttachedTurnEndReceipt)>,
        AttachedTurnEndQueryStatus,
    ) {
        let count = self.unmatched_attached_turn_ends.len();
        let inspected = limit
            .min(crate::host::MAX_OWNED_TARGET_QUERY_ROWS)
            .min(count);
        let status = if inspected == count {
            AttachedTurnEndQueryStatus::Complete
        } else {
            AttachedTurnEndQueryStatus::Truncated {
                omitted: count - inspected,
            }
        };
        (
            self.unmatched_attached_turn_ends.iter().take(inspected),
            status,
        )
    }

    /// Current issued or spawned turn owners, including retired recorded agents.
    pub fn open_turn_owner_ids(&self) -> impl ExactSizeIterator<Item = &AgentId> {
        self.open_turn_owners.iter()
    }

    /// Current native spawn metadata with authoritative instruction-attempt starts.
    ///
    /// Ended or reviewed attempts remain until exact validated boundary settlement. This index
    /// does not establish coverage of legacy spawns or attempts without a recorded start.
    pub fn managed_native_spawn_receipts(
        &self,
    ) -> impl ExactSizeIterator<Item = (&Lsn, &ManagedNativeSpawnReceipt)> {
        self.managed_native_spawns.iter()
    }

    /// One unsettled managed spawn bound to its exact starting record.
    #[must_use]
    pub fn managed_native_spawn_receipt(
        &self,
        started_lsn: Lsn,
    ) -> Option<&ManagedNativeSpawnReceipt> {
        self.managed_native_spawns.get(&started_lsn)
    }

    /// Current attached native metadata with authoritative attachment and attempt identities.
    ///
    /// Closing, detaching or finishing an observation retains these receipts until exact validated
    /// boundary settlement. Legacy spawns without a recorded attempt are outside this index.
    pub fn attached_native_spawn_receipts(
        &self,
    ) -> impl ExactSizeIterator<Item = (&Lsn, &AttachedNativeSpawnReceipt)> {
        self.attached_native_spawns.iter()
    }

    /// One unsettled attached spawn bound to its exact starting record.
    #[must_use]
    pub fn attached_native_spawn_receipt(
        &self,
        started_lsn: Lsn,
    ) -> Option<&AttachedNativeSpawnReceipt> {
        self.attached_native_spawns.get(&started_lsn)
    }
}

/// Which of the classification table's token rows a journaled agent-token hash reaches, and for which
/// agent. It mirrors `dudes_rpc::TokenOwner` because this crate does not depend on `dudes-rpc`,
/// and `dudes-types` holds only types that cross a process boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MintedFor {
    /// Row 2: the agent is not retired.
    Live(AgentId),
    /// The retired-agent row: the agent is retired and still has a recorded child.
    RetiredWithChild(AgentId),
}

fn launch_failed_reason(reason: &str) -> String {
    if reason.len() <= 128
        && reason
            .strip_prefix("managed process could not launch (")
            .and_then(|part| part.strip_suffix(')'))
            .is_some_and(|kind| {
                !kind.is_empty() && kind.bytes().all(|byte| byte.is_ascii_alphabetic())
            })
    {
        reason.to_owned()
    } else {
        "managed process could not launch (unknown error)".to_owned()
    }
}

#[cfg(test)]
mod current_census_tests {
    use dudes_proto::instruction::{
        InstructionBaselineRef, InstructionBoundaryAbsenceProof, InstructionComparison,
        InstructionCoverageGap, InstructionGapKind, InstructionIssueAssociation,
        InstructionNativeBoundaryReceipt, InstructionPathContext, InstructionSettlement,
        InstructionSnapshot, InstructionSnapshotSource, InstructionUnknownReason,
        NativeInstructionPath,
    };
    use dudes_proto::record::RecordKind;
    use dudes_types::{Mode, Timestamp};

    use super::*;

    fn fold(world: &mut World, lsn: u64, record: Record) {
        world.apply(
            Lsn(lsn),
            &Entry {
                at: Timestamp::UNIX_EPOCH,
                record,
            },
        );
    }

    fn hired(agent: AgentId) -> Record {
        Record::AgentHired {
            id: agent,
            name: "worker".into(),
            character: None,
            accent: None,
            role: "builder".into(),
            department: "engineering".into(),
            provider: Provider::Claude,
            cwd: "/work/repo".into(),
            workspace: None,
            budget: AgentBudget {
                usd_per_day: None,
                tokens_per_day: None,
            },
            sandbox: None,
            isolation: None,
            max_turns: None,
            max_budget: None,
            trusted: true,
            worktree: None,
            mcp_servers: Vec::new(),
            browser_onboarding: None,
        }
    }

    fn turn_record(kind: RecordKind, turn_id: TurnId) -> Record {
        match kind {
            RecordKind::TurnIssued => Record::TurnIssued {
                agent: agent(),
                turn: turn_id,
                goal: GoalId::from_parts(1, 1),
                task: "run the tests".into(),
                session: None,
                mode: Mode::Managed,
                model_selection: None,
                lsn_of_decision: Lsn(2),
                answers: Vec::new(),
            },
            RecordKind::TurnSpawned => Record::TurnSpawned {
                agent: agent(),
                turn: turn_id,
                pid: 4242,
                start_time: 1_787_394_600,
                boot: Some("f81d4fae-7dec-11d0-a765-00a0c91e6bf6".into()),
                session: None,
                reissued: true,
                mcp_grant: None,
            },
            RecordKind::TurnEnded => Record::TurnEnded {
                agent: agent(),
                turn: turn_id,
                mode: Mode::Managed,
                provenance: Provenance::Observed,
                elapsed_ms: None,
                exit: dudes_proto::record::ExitStatus {
                    code: Some(0),
                    signal: None,
                },
                usage: None,
                session_usage: None,
                model_usage: BTreeMap::new(),
                last_message: None,
                final_reply: None,
                session: None,
                outcome: Some(TurnOutcome::Completed),
            },
            _ => panic!("the fixture needs a managed turn carrier"),
        }
    }

    fn agent() -> AgentId {
        AgentId::from_parts(1, 1)
    }

    fn owner(turn: TurnId, issue: InstructionIssueAssociation) -> InstructionObservationOwner {
        InstructionObservationOwner::Managed {
            agent: agent(),
            turn,
            issue,
        }
    }

    fn started(turn: TurnId, issue: InstructionIssueAssociation) -> Record {
        Record::InstructionObservationStarted {
            owner: owner(turn, issue),
            ticket: None,
            snapshot: InstructionSnapshot {
                sampled_at: Timestamp::UNIX_EPOCH,
                source: InstructionSnapshotSource::Fresh,
                pid_namespace: None,
                boot: None,
                execution_folder: Some(NativeInstructionPath::Utf8 {
                    value: "/work/repo".into(),
                }),
                participants: Vec::new(),
                inputs: Vec::new(),
                inventoried: Vec::new(),
                gaps: vec![InstructionCoverageGap {
                    origin: dudes_proto::InstructionGapOrigin::Observed,
                    reason: InstructionGapKind::DeclarationMissing,
                    context: InstructionPathContext::Unresolved,
                    agent: Some(agent()),
                    path: None,
                }],
                usage: dudes_proto::instruction::InstructionSampleUsage::default(),
            },
        }
    }

    fn finished(turn: TurnId, issue: InstructionIssueAssociation, started_lsn: Lsn) -> Record {
        Record::InstructionObservationFinished {
            owner: owner(turn, issue),
            ticket: None,
            baseline: InstructionBaselineRef::Started { lsn: started_lsn },
            settlement: InstructionSettlement::Unknown {
                reason: InstructionUnknownReason::UnrecordedLaunchBoundary,
                end: None,
                sampled_at: None,
                recovery: true,
                comparison: InstructionComparison {
                    gaps: vec![InstructionCoverageGap {
                        origin: dudes_proto::InstructionGapOrigin::Observed,
                        reason: InstructionGapKind::UnrecordedLaunchBoundary,
                        context: InstructionPathContext::SharedProject,
                        agent: Some(agent()),
                        path: None,
                    }],
                    ..InstructionComparison::default()
                },
            },
        }
    }

    fn settled(
        turn: TurnId,
        issue: InstructionIssueAssociation,
        started_lsn: Lsn,
        spawn_lsn: Lsn,
    ) -> Record {
        Record::InstructionAttemptBoundarySettled {
            owner: owner(turn, issue),
            started_lsn,
            proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                receipt: InstructionNativeBoundaryReceipt::Spawned { lsn: spawn_lsn },
            },
        }
    }

    fn seeded() -> World {
        let mut world = World::default();
        fold(&mut world, 1, hired(agent()));
        fold(
            &mut world,
            2,
            Record::GoalSet {
                agent: agent(),
                goal: Goal {
                    id: GoalId::from_parts(1, 1),
                    text: "complete the work".into(),
                    predicate: Predicate::Command {
                        argv: vec!["true".into()],
                        timeout_s: 1,
                    },
                    completion_check: None,
                    baseline: None,
                    budget: dudes_proto::budget::GoalBudget {
                        usd: None,
                        tokens: None,
                    },
                    hop_cap: 1000,
                },
                browser_onboarding: None,
                browser_goal: None,
                goal_replacement: None,
            },
        );
        world
    }

    fn predicate_result(turn: Option<TurnId>, provenance: Provenance) -> Record {
        Record::PredicateResult {
            goal: GoalId::from_parts(1, 1),
            turn,
            provenance,
            pass: false,
            evidence: dudes_proto::Evidence::Command {
                argv: vec!["true".into()],
                program: Some("true".into()),
                exit: Some(1),
                output_tail: String::new(),
            },
        }
    }

    #[test]
    fn predicate_receipts_require_an_exact_current_observed_result_after_the_end() {
        let mut world = seeded();
        let goal = GoalId::from_parts(1, 1);
        let turn = TurnId::from_parts(1, 1);
        fold(&mut world, 3, turn_record(RecordKind::TurnIssued, turn));
        fold(&mut world, 4, turn_record(RecordKind::TurnSpawned, turn));
        fold(&mut world, 5, turn_record(RecordKind::TurnEnded, turn));
        fold(
            &mut world,
            6,
            predicate_result(Some(turn), Provenance::Inferred),
        );
        assert!(!world.observed_predicate_after(goal, turn, Lsn(5)));
        fold(&mut world, 7, predicate_result(None, Provenance::Observed));
        assert!(!world.observed_predicate_after(goal, turn, Lsn(5)));
        fold(
            &mut world,
            8,
            predicate_result(Some(turn), Provenance::Observed),
        );
        assert!(world.observed_predicate_after(goal, turn, Lsn(5)));
        assert!(!world.observed_predicate_after(goal, turn, Lsn(8)));
        assert!(!world.observed_predicate_after(GoalId::from_parts(1, 2), turn, Lsn(5)));
        fold(
            &mut world,
            9,
            Record::GoalCleared {
                agent: agent(),
                goal,
            },
        );
        assert!(!world.observed_predicate_after(goal, turn, Lsn(5)));
        fold(
            &mut world,
            10,
            predicate_result(Some(turn), Provenance::Observed),
        );
        assert!(!world.observed_predicate_after(goal, turn, Lsn(5)));
    }

    #[test]
    fn a_late_older_result_cannot_replace_the_current_predicate_receipt() {
        let mut world = seeded();
        let goal = GoalId::from_parts(1, 1);
        let older = TurnId::from_parts(1, 1);
        fold(&mut world, 3, turn_record(RecordKind::TurnIssued, older));
        fold(&mut world, 4, turn_record(RecordKind::TurnSpawned, older));
        fold(&mut world, 5, turn_record(RecordKind::TurnEnded, older));
        fold(
            &mut world,
            6,
            predicate_result(Some(older), Provenance::Observed),
        );
        assert!(world.observed_predicate_after(goal, older, Lsn(5)));
        let current = TurnId::from_parts(1, 2);
        fold(&mut world, 7, turn_record(RecordKind::TurnIssued, current));
        fold(&mut world, 8, turn_record(RecordKind::TurnSpawned, current));
        fold(&mut world, 9, turn_record(RecordKind::TurnEnded, current));
        assert!(!world.observed_predicate_after(goal, older, Lsn(5)));
        fold(
            &mut world,
            10,
            predicate_result(Some(older), Provenance::Observed),
        );
        assert!(!world.observed_predicate_after(goal, current, Lsn(9)));
        fold(
            &mut world,
            11,
            predicate_result(Some(current), Provenance::Observed),
        );
        fold(
            &mut world,
            12,
            predicate_result(Some(older), Provenance::Observed),
        );
        assert!(world.observed_predicate_after(goal, current, Lsn(9)));
        assert!(!world.observed_predicate_after(goal, older, Lsn(9)));
    }

    #[test]
    fn active_folder_users_include_idle_paused_and_halted_but_exclude_retired() {
        let mut world = World::default();
        let idle = AgentId::from_parts(1, 1);
        let paused = AgentId::from_parts(1, 2);
        let halted = AgentId::from_parts(1, 3);
        for (lsn, agent) in [(1, idle), (2, paused), (3, halted)] {
            fold(&mut world, lsn, hired(agent));
        }
        fold(
            &mut world,
            4,
            Record::Pause {
                agent: paused,
                browser: None,
            },
        );
        fold(
            &mut world,
            5,
            Record::Halt {
                agent: halted,
                text: None,
            },
        );
        assert_eq!(
            world.active_folder_user_ids().copied().collect::<Vec<_>>(),
            [idle, paused, halted]
        );
        fold(
            &mut world,
            6,
            Record::AgentRetired {
                id: paused,
                reason: "finished".into(),
                browser_retire: None,
            },
        );
        assert_eq!(
            world.active_folder_user_ids().copied().collect::<Vec<_>>(),
            [idle, halted]
        );
        assert_eq!(world.agents.len(), 3);
        assert!(world.agents[&paused].retired);
    }

    fn attached_owner(attachment: u64) -> InstructionObservationOwner {
        InstructionObservationOwner::AttachedProcess {
            agent: agent(),
            attachment_lsn: Lsn(attachment),
        }
    }

    fn attached() -> Record {
        Record::Attached {
            agent: agent(),
            by: dudes_proto::record::Actor::System,
            hook_port: None,
            provenance: None,
        }
    }

    fn attached_start(attachment: u64) -> Record {
        let mut record = started(
            TurnId::from_parts(1, 1),
            InstructionIssueAssociation::Prospective,
        );
        if let Record::InstructionObservationStarted { owner, .. } = &mut record {
            *owner = attached_owner(attachment);
        }
        record
    }

    fn pty_spawn(pty: u64, pid: u32) -> Record {
        Record::PtySpawned {
            agent: agent(),
            pty: dudes_proto::rpc::pty::PtyId(pty),
            pid,
            start_time: u64::from(pid) + 100,
            boot: Some("f81d4fae-7dec-11d0-a765-00a0c91e6bf6".into()),
        }
    }

    fn attached_finish(attachment: u64, start: u64) -> Record {
        let mut record = finished(
            TurnId::from_parts(1, 1),
            InstructionIssueAssociation::Prospective,
            Lsn(start),
        );
        if let Record::InstructionObservationFinished { owner, .. } = &mut record {
            *owner = attached_owner(attachment);
        }
        record
    }

    fn attached_settled(attachment: u64, start: u64, spawn: u64) -> Record {
        Record::InstructionAttemptBoundarySettled {
            owner: attached_owner(attachment),
            started_lsn: Lsn(start),
            proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                receipt: InstructionNativeBoundaryReceipt::Spawned { lsn: Lsn(spawn) },
            },
        }
    }

    fn pty_close_request(pty: u64) -> Record {
        Record::PtyCloseRequested {
            pty: dudes_proto::rpc::pty::PtyId(pty),
            actor: dudes_proto::record::Actor::System,
            provenance: None,
            reason: "finished".into(),
        }
    }

    #[test]
    fn retired_current_turn_owners_remain_until_the_exact_turn_ends() {
        let mut world = seeded();
        let first = TurnId::from_parts(1, 1);
        let second = TurnId::from_parts(1, 2);
        fold(&mut world, 3, turn_record(RecordKind::TurnIssued, first));
        fold(
            &mut world,
            4,
            Record::AgentRetired {
                id: agent(),
                reason: "finished".into(),
                browser_retire: None,
            },
        );
        assert_eq!(
            world.open_turn_owner_ids().copied().collect::<Vec<_>>(),
            [agent()]
        );
        assert_eq!(world.active_folder_user_ids().len(), 0);
        fold(&mut world, 5, turn_record(RecordKind::TurnIssued, second));
        fold(&mut world, 6, turn_record(RecordKind::TurnEnded, first));
        assert_eq!(world.agents[&agent()].open_turn, Some(second));
        assert_eq!(
            world.open_turn_owner_ids().copied().collect::<Vec<_>>(),
            [agent()]
        );
        fold(&mut world, 7, turn_record(RecordKind::TurnEnded, second));
        assert_eq!(world.open_turn_owner_ids().len(), 0);
    }

    #[test]
    fn attached_native_receipts_preserve_the_first_spawn_and_reject_duplicate_proofs() {
        let mut world = seeded();
        fold(&mut world, 3, attached());
        fold(&mut world, 4, attached_start(3));
        fold(&mut world, 5, pty_spawn(1, 4242));
        fold(&mut world, 6, pty_spawn(2, 4243));
        let receipt = world.attached_native_spawn_receipt(Lsn(4)).unwrap();
        assert_eq!(receipt.owner, attached_owner(3));
        assert_eq!(receipt.pty, dudes_proto::rpc::pty::PtyId(1));
        assert_eq!(receipt.key.pid, 4242);
        assert_eq!(receipt.key.start_time, 4342);
        assert_eq!(
            receipt.key.boot.as_deref(),
            Some("f81d4fae-7dec-11d0-a765-00a0c91e6bf6")
        );
        assert_eq!(receipt.spawn_lsn, Lsn(5));
        fold(&mut world, 7, attached_settled(3, 4, 6));
        assert_eq!(
            world
                .attached_native_spawn_receipt(Lsn(4))
                .unwrap()
                .spawn_lsn,
            Lsn(5)
        );
        assert!(
            world
                .instructions
                .attempt(Lsn(4))
                .unwrap()
                .boundary_settled
                .is_none()
        );
        fold(&mut world, 8, attached_settled(7, 4, 5));
        assert_eq!(world.attached_native_spawn_receipts().len(), 1);
        fold(&mut world, 9, attached_settled(3, 4, 5));
        assert_eq!(world.attached_native_spawn_receipts().len(), 0);
        assert!(
            world
                .instructions
                .attempt(Lsn(4))
                .unwrap()
                .boundary_settled
                .is_some()
        );
    }

    #[test]
    fn delayed_pty_close_and_finish_retain_independent_attachment_receipts() {
        let mut world = seeded();
        fold(&mut world, 3, attached());
        fold(&mut world, 4, attached_start(3));
        fold(&mut world, 5, pty_spawn(1, 4242));
        fold(&mut world, 6, pty_close_request(1));
        fold(&mut world, 7, attached());
        fold(&mut world, 8, attached_start(7));
        fold(&mut world, 9, pty_spawn(2, 4243));
        fold(
            &mut world,
            10,
            Record::PtyCloseCompleted {
                request_lsn: Lsn(6),
                outcome: dudes_proto::record::PtyCloseOutcome::Closed,
            },
        );
        assert_eq!(world.instructions.attachment_end_lsn(Lsn(3)), Some(Lsn(10)));
        assert_eq!(world.instructions.attachment_end_lsn(Lsn(7)), None);
        fold(&mut world, 11, attached_finish(3, 4));
        assert_eq!(
            world.instructions.current_for_attachment(Lsn(7)).unwrap().0,
            Lsn(8)
        );
        fold(
            &mut world,
            12,
            Record::Detached {
                agent: agent(),
                by: dudes_proto::record::Actor::System,
                provenance: None,
            },
        );
        assert_eq!(world.attached_native_spawn_receipts().len(), 2);
        fold(&mut world, 13, attached_settled(3, 4, 5));
        assert!(world.attached_native_spawn_receipt(Lsn(4)).is_none());
        assert_eq!(
            world.attached_native_spawn_receipt(Lsn(8)).unwrap().owner,
            attached_owner(7)
        );
        fold(&mut world, 14, pty_close_request(2));
        fold(
            &mut world,
            15,
            Record::PtyCloseCompleted {
                request_lsn: Lsn(14),
                outcome: dudes_proto::record::PtyCloseOutcome::Closed,
            },
        );
        fold(&mut world, 16, attached_finish(7, 8));
        assert_eq!(world.attached_native_spawn_receipts().len(), 1);
        fold(&mut world, 17, attached_settled(7, 8, 9));
        assert_eq!(world.attached_native_spawn_receipts().len(), 0);
    }

    #[test]
    fn attached_prior_boot_settlement_can_retire_its_exact_receipt() {
        let mut world = seeded();
        fold(&mut world, 3, attached());
        let previous = dudes_proto::instruction::InstructionBootIdentity::LinuxKernelBootId {
            value: "f81d4fae-7dec-11d0-a765-00a0c91e6bf6".into(),
        };
        let mut start = attached_start(3);
        if let Record::InstructionObservationStarted { snapshot, .. } = &mut start {
            snapshot.boot = Some(previous.clone());
        }
        fold(&mut world, 4, start);
        fold(&mut world, 5, pty_spawn(1, 4242));
        assert_eq!(world.attached_native_spawn_receipts().len(), 1);
        fold(
            &mut world,
            6,
            Record::InstructionAttemptBoundarySettled {
                owner: attached_owner(3),
                started_lsn: Lsn(4),
                proof: InstructionBoundaryAbsenceProof::PriorBootAbsent {
                    previous,
                    current: dudes_proto::instruction::InstructionBootIdentity::LinuxKernelBootId {
                        value: "another-boot".into(),
                    },
                },
            },
        );
        assert_eq!(world.attached_native_spawn_receipts().len(), 0);
        assert!(
            world
                .instructions
                .attempt(Lsn(4))
                .unwrap()
                .boundary_settled
                .is_some()
        );
    }

    #[test]
    fn ended_attempts_keep_exact_spawn_metadata_and_later_attempts_do_not_replace_it() {
        let mut world = seeded();
        let turn = TurnId::from_parts(1, 1);
        let initial = InstructionIssueAssociation::Prospective;
        fold(&mut world, 3, started(turn, initial));
        fold(&mut world, 4, turn_record(RecordKind::TurnIssued, turn));
        fold(&mut world, 5, turn_record(RecordKind::TurnSpawned, turn));
        fold(&mut world, 6, turn_record(RecordKind::TurnEnded, turn));
        let first = world.managed_native_spawn_receipt(Lsn(3)).unwrap();
        assert_eq!(first.owner, owner(turn, initial));
        assert_eq!(first.spawn_lsn, Lsn(5));
        assert_eq!(first.key.pid, 4242);
        assert_eq!(first.key.start_time, 1_787_394_600);
        assert_eq!(
            first.key.boot.as_deref(),
            Some("f81d4fae-7dec-11d0-a765-00a0c91e6bf6")
        );
        fold(&mut world, 7, finished(turn, initial, Lsn(3)));
        let retry = InstructionIssueAssociation::Issued { issue_lsn: Lsn(4) };
        fold(&mut world, 8, started(turn, retry));
        assert!(world.instructions.attempt(Lsn(8)).is_none());
        assert_eq!(world.last_applied_lsn, Some(Lsn(7)));
        assert_eq!(world.managed_native_spawn_receipts().len(), 1);
        let later_turn = TurnId::from_parts(1, 2);
        fold(&mut world, 9, started(later_turn, initial));
        fold(
            &mut world,
            10,
            turn_record(RecordKind::TurnIssued, later_turn),
        );
        let mut spawn = turn_record(RecordKind::TurnSpawned, later_turn);
        let Record::TurnSpawned {
            pid,
            start_time,
            boot,
            ..
        } = &mut spawn
        else {
            unreachable!()
        };
        *pid = 4243;
        *start_time = 7;
        *boot = None;
        fold(&mut world, 11, spawn);
        assert_eq!(world.managed_native_spawn_receipts().len(), 2);
        let first = world.managed_native_spawn_receipt(Lsn(3)).unwrap();
        assert_eq!(first.spawn_lsn, Lsn(5));
        assert_eq!(first.key.pid, 4242);
        let second = world.managed_native_spawn_receipt(Lsn(9)).unwrap();
        assert_eq!(second.owner, owner(later_turn, initial));
        assert_eq!(second.spawn_lsn, Lsn(11));
        assert_eq!(second.key.pid, 4243);
        assert_eq!(second.key.start_time, 7);
        assert_eq!(second.key.boot, None);
        fold(&mut world, 12, settled(turn, initial, Lsn(3), Lsn(11)));
        assert_eq!(world.managed_native_spawn_receipts().len(), 2);
        assert!(
            world
                .instructions
                .attempt(Lsn(3))
                .unwrap()
                .boundary_settled
                .is_none()
        );
        fold(&mut world, 13, settled(turn, initial, Lsn(3), Lsn(5)));
        assert!(world.managed_native_spawn_receipt(Lsn(3)).is_none());
        assert_eq!(
            world
                .managed_native_spawn_receipt(Lsn(9))
                .unwrap()
                .spawn_lsn,
            Lsn(11)
        );
        fold(
            &mut world,
            14,
            settled(later_turn, initial, Lsn(9), Lsn(11)),
        );
        assert_eq!(world.managed_native_spawn_receipts().len(), 0);
        assert_eq!(world.instructions.attempts().count(), 2);
    }

    #[test]
    fn a_duplicate_spawn_cannot_retire_the_first_attempt_with_its_later_receipt() {
        let mut world = seeded();
        let turn = TurnId::from_parts(1, 1);
        let issue = InstructionIssueAssociation::Prospective;
        fold(&mut world, 3, started(turn, issue));
        fold(&mut world, 4, turn_record(RecordKind::TurnIssued, turn));
        fold(&mut world, 5, turn_record(RecordKind::TurnSpawned, turn));
        let mut duplicate = turn_record(RecordKind::TurnSpawned, turn);
        let Record::TurnSpawned {
            pid,
            start_time,
            boot,
            ..
        } = &mut duplicate
        else {
            unreachable!()
        };
        *pid = 4243;
        *start_time = 7;
        *boot = None;
        fold(&mut world, 6, duplicate);
        let first = world.managed_native_spawn_receipt(Lsn(3)).unwrap();
        assert_eq!(first.spawn_lsn, Lsn(5));
        assert_eq!(first.key.pid, 4242);
        assert_eq!(first.key.start_time, 1_787_394_600);
        assert_eq!(
            first.key.boot.as_deref(),
            Some("f81d4fae-7dec-11d0-a765-00a0c91e6bf6")
        );
        assert_eq!(world.turns[&turn].pid, Some(4243));
        fold(&mut world, 7, settled(turn, issue, Lsn(3), Lsn(6)));
        assert_eq!(
            world
                .managed_native_spawn_receipt(Lsn(3))
                .unwrap()
                .spawn_lsn,
            Lsn(5)
        );
        assert!(
            world
                .instructions
                .attempt(Lsn(3))
                .unwrap()
                .boundary_settled
                .is_none()
        );
        fold(&mut world, 8, settled(turn, issue, Lsn(3), Lsn(5)));
        assert!(world.managed_native_spawn_receipt(Lsn(3)).is_none());
        assert!(
            world
                .instructions
                .attempt(Lsn(3))
                .unwrap()
                .boundary_settled
                .is_some()
        );
    }

    #[test]
    fn a_launch_failure_receipt_cannot_retire_an_indexed_spawn() {
        let mut world = seeded();
        let turn = TurnId::from_parts(1, 1);
        let issue = InstructionIssueAssociation::Prospective;
        fold(&mut world, 3, started(turn, issue));
        fold(&mut world, 4, turn_record(RecordKind::TurnIssued, turn));
        fold(&mut world, 5, turn_record(RecordKind::TurnSpawned, turn));
        let mut ended = turn_record(RecordKind::TurnEnded, turn);
        let Record::TurnEnded { outcome, .. } = &mut ended else {
            unreachable!()
        };
        *outcome = Some(TurnOutcome::NotLaunched {
            reason: "managed process could not launch (Other)".into(),
        });
        fold(&mut world, 6, ended);
        fold(
            &mut world,
            7,
            Record::InstructionAttemptBoundarySettled {
                owner: owner(turn, issue),
                started_lsn: Lsn(3),
                proof: InstructionBoundaryAbsenceProof::NativeOwnedBoundaryAbsent {
                    receipt: InstructionNativeBoundaryReceipt::LaunchFailureEnded { lsn: Lsn(6) },
                },
            },
        );
        assert_eq!(
            world
                .managed_native_spawn_receipt(Lsn(3))
                .unwrap()
                .spawn_lsn,
            Lsn(5)
        );
        assert!(
            world
                .instructions
                .attempt(Lsn(3))
                .unwrap()
                .boundary_settled
                .is_none()
        );
    }

    #[test]
    fn validated_prior_boot_absence_still_retires_the_exact_indexed_attempt() {
        use dudes_proto::instruction::InstructionBootIdentity;
        let mut world = seeded();
        let turn = TurnId::from_parts(1, 1);
        let issue = InstructionIssueAssociation::Prospective;
        let previous = InstructionBootIdentity::LinuxKernelBootId {
            value: "f81d4fae-7dec-11d0-a765-00a0c91e6bf6".into(),
        };
        let mut baseline = started(turn, issue);
        let Record::InstructionObservationStarted { snapshot, .. } = &mut baseline else {
            unreachable!()
        };
        snapshot.boot = Some(previous.clone());
        fold(&mut world, 3, baseline);
        fold(&mut world, 4, turn_record(RecordKind::TurnIssued, turn));
        fold(&mut world, 5, turn_record(RecordKind::TurnSpawned, turn));
        assert_eq!(world.managed_native_spawn_receipts().len(), 1);
        fold(
            &mut world,
            6,
            Record::InstructionAttemptBoundarySettled {
                owner: owner(turn, issue),
                started_lsn: Lsn(3),
                proof: InstructionBoundaryAbsenceProof::PriorBootAbsent {
                    previous,
                    current: InstructionBootIdentity::LinuxKernelBootId {
                        value: "another-boot".into(),
                    },
                },
            },
        );
        assert!(world.managed_native_spawn_receipt(Lsn(3)).is_none());
        assert!(
            world
                .instructions
                .attempt(Lsn(3))
                .unwrap()
                .boundary_settled
                .is_some()
        );
    }

    #[test]
    fn many_completed_settled_attempts_do_not_fill_the_current_census() {
        let mut world = seeded();
        for ordinal in 0..520_u64 {
            let turn = TurnId::from_parts(2, u128::from(ordinal) + 1);
            let lsn = 3 + ordinal * 6;
            let issue = InstructionIssueAssociation::Prospective;
            fold(&mut world, lsn, started(turn, issue));
            fold(
                &mut world,
                lsn + 1,
                turn_record(RecordKind::TurnIssued, turn),
            );
            fold(
                &mut world,
                lsn + 2,
                turn_record(RecordKind::TurnSpawned, turn),
            );
            fold(
                &mut world,
                lsn + 3,
                turn_record(RecordKind::TurnEnded, turn),
            );
            fold(&mut world, lsn + 4, finished(turn, issue, Lsn(lsn)));
            fold(
                &mut world,
                lsn + 5,
                settled(turn, issue, Lsn(lsn), Lsn(lsn + 2)),
            );
            assert_eq!(world.managed_native_spawn_receipts().len(), 0);
        }
        let turn = TurnId::from_parts(2, 521);
        fold(
            &mut world,
            3123,
            started(turn, InstructionIssueAssociation::Prospective),
        );
        fold(&mut world, 3124, turn_record(RecordKind::TurnIssued, turn));
        fold(&mut world, 3125, turn_record(RecordKind::TurnSpawned, turn));
        assert_eq!(
            world
                .managed_native_spawn_receipts()
                .take(2)
                .map(|(lsn, receipt)| (*lsn, receipt.spawn_lsn))
                .collect::<Vec<_>>(),
            [(Lsn(3123), Lsn(3125))]
        );
        assert_eq!(world.turns.len(), 521);
        assert_eq!(world.instructions.attempts().count(), 521);
        assert_eq!(world.active_folder_user_ids().len(), 1);
    }

    #[test]
    fn a_legacy_spawn_without_a_start_does_not_invent_attempt_metadata() {
        let mut world = seeded();
        let turn = TurnId::from_parts(1, 1);
        fold(&mut world, 3, turn_record(RecordKind::TurnIssued, turn));
        fold(&mut world, 4, turn_record(RecordKind::TurnSpawned, turn));
        assert_eq!(world.managed_native_spawn_receipts().len(), 0);
        assert!(world.instructions.current_for_turn(turn).is_none());
        assert_eq!(world.turns[&turn].spawn_lsn, Some(Lsn(4)));
        assert_eq!(world.turns[&turn].adoption_key().unwrap().pid, 4242);
        assert!(world.turns[&turn].is_open_child());
    }
}
